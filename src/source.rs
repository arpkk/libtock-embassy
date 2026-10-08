//! Estructura común de los drivers que multiplexan varias fuentes sobre un único
//! número de subscribe: botones y GPIO.
//!
//! Cada driver declara un [`SourceTable`] estático. La tabla reúne los slots, el
//! registro de fuentes tomadas y el receptor de la upcall, y abre la suscripción
//! permanente del driver con [`SourceTable::init`]. Lo que cada driver conserva
//! es lo que depende de su cápsula: los commands, el armado y el filtrado de
//! eventos.

use core::cell::Cell;
use core::sync::atomic::{AtomicU32, Ordering};
use core::task::Waker;

use critical_section::Mutex;
use libtock_platform::subscribe::OneId;
use libtock_platform::{ErrorCode, Syscalls, Upcall, share};

use crate::Never;
use crate::runtime::TockSyscalls as S;

/// Punto de encuentro entre la upcall y la espera de una fuente.
///
/// Se usa `Cell` y no `RefCell` porque la upcall puede ejecutarse durante un
/// `yield` dentro del `poll` de otra tarea.
pub(crate) struct Slot<T> {
    waker: Mutex<Cell<Option<Waker>>>,
    /// Último evento recibido y todavía no consumido.
    last: Mutex<Cell<Option<T>>>,
}

impl<T> Slot<T> {
    const fn new() -> Self {
        Self {
            waker: Mutex::new(Cell::new(None)),
            last: Mutex::new(Cell::new(None)),
        }
    }

    /// Prepara la espera en su primer `poll`: limpia el slot, registra el waker y
    /// llama a `enable`, en ese orden. En los `poll` siguientes solo registra el
    /// waker. Devuelve `true` en el primero.
    pub(crate) fn arm(&self, armed: &mut bool, waker: &Waker, enable: impl FnOnce()) -> bool {
        if *armed {
            self.register(waker);
            return false;
        }
        self.clear_last();
        self.register(waker);
        enable();
        *armed = true;
        true
    }

    /// Consume el evento pendiente, si lo hay.
    pub(crate) fn take(&self) -> Option<T> {
        critical_section::with(|cs| self.last.borrow(cs).replace(None))
    }

    pub(crate) fn clear_waker(&self) {
        critical_section::with(|cs| self.waker.borrow(cs).set(None))
    }

    /// Registra el waker, sin clonar si ya es el mismo.
    fn register(&self, w: &Waker) {
        critical_section::with(|cs| {
            let cell = self.waker.borrow(cs);
            cell.set(match cell.replace(None) {
                Some(old) if old.will_wake(w) => Some(old),
                _ => Some(w.clone()),
            });
        })
    }

    fn wake(&self) {
        critical_section::with(|cs| {
            let cell = self.waker.borrow(cs);
            if let Some(w) = cell.replace(None) {
                w.wake_by_ref();
                cell.set(Some(w));
            }
        })
    }

    fn put(&self, event: T) {
        critical_section::with(|cs| self.last.borrow(cs).set(Some(event)))
    }

    fn clear_last(&self) {
        critical_section::with(|cs| self.last.borrow(cs).set(None))
    }
}

/// Tabla de fuentes de un driver: un slot por fuente, el registro de las ya
/// tomadas y el receptor único de la upcall.
///
/// `D` y `SUB` atan la tabla a su par `(driver, subscribe)` en el tipo, de modo
/// que solo puede suscribirse a la upcall de su propio driver.
pub(crate) struct SourceTable<T, const N: usize, const D: u32, const SUB: u32> {
    slots: [Slot<T>; N],
    /// Bitmask de las fuentes ya entregadas. Limita `N` a 32.
    taken: AtomicU32,
}

impl<T, const N: usize, const D: u32, const SUB: u32> SourceTable<T, N, D, SUB> {
    pub(crate) const fn new() -> Self {
        const { assert!(N <= 32, "el bitmask de fuentes tomadas es de 32 bits") };
        Self {
            slots: [const { Slot::new() }; N],
            taken: AtomicU32::new(0),
        }
    }

    /// Slot de una fuente ya tomada. El índice lo valida [`Self::claim`].
    pub(crate) fn slot(&self, index: u32) -> &Slot<T> {
        &self.slots[index as usize]
    }

    /// Reserva la fuente `index`. Devuelve `false` si está fuera de la tabla o si
    /// ya tiene dueño.
    pub(crate) fn claim(&self, index: u32) -> bool {
        if index as usize >= N {
            return false;
        }
        let bit = 1u32 << index;
        self.taken.fetch_or(bit, Ordering::AcqRel) & bit == 0
    }

    pub(crate) fn release(&self, index: u32) {
        self.taken.fetch_and(!(1u32 << index), Ordering::AcqRel);
    }

    /// Registra la upcall del driver de forma permanente y ejecuta `f`.
    ///
    /// `count` es la cantidad de fuentes que reporta la placa. Como `f` devuelve
    /// [`Never`], el `share::scope` no termina y la suscripción permanece válida
    /// durante toda la vida del proceso.
    pub(crate) fn init(&'static self, count: u32, f: impl FnOnce() -> Never) -> !
    where
        T: From<u32>,
    {
        assert!(
            count as usize <= N,
            "driver {D:#x}: la placa reporta {count} fuentes y la tabla admite {N}"
        );
        match share::scope(|subscribe| {
            S::subscribe::<_, _, DetectSteal, D, SUB>(subscribe, self).unwrap_or_else(
                |e: ErrorCode| panic!("subscribe ({D:#x}, {SUB}) falló: {e:?}"),
            );
            f()
        }) {}
    }
}

impl<T: From<u32>, const N: usize, const D: u32, const SUB: u32> Upcall<OneId<D, SUB>>
    for SourceTable<T, N, D, SUB>
{
    /// Encamina el evento al slot de su fuente por índice.
    fn upcall(&self, index: u32, value: u32, _: u32) {
        // Un índice fuera de la tabla se ignora en lugar de provocar un panic
        // dentro de una upcall. En GPIO pueden llegar eventos de fuentes que el
        // proceso no tomó; escribir su slot es inocuo porque la siguiente espera
        // lo limpia.
        if let Some(slot) = self.slots.get(index as usize) {
            slot.put(T::from(value));
            slot.wake();
        }
    }
}

/// Provoca un panic si otro componente ya había registrado la upcall, en lugar
/// de dejar esperas que nunca despiertan.
pub(crate) struct DetectSteal;

impl libtock_platform::subscribe::Config for DetectSteal {
    fn returned_nonnull_upcall(driver_num: u32, subscribe_num: u32) {
        panic!("upcall ({driver_num:#x}, {subscribe_num}) already owned")
    }
}
