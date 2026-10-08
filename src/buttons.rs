//! Espera asincrónica de eventos de botón (cápsula `0x3`).
//!
//! Cada botón se obtiene una sola vez con [`Button::take`]. Cada espera habilita la
//! interrupción del botón en su primer `poll` (`command 1`) y la deshabilita al
//! terminar (`command 2`). La cápsula entrega siempre ambos eventos, presionado y
//! soltado, y la espera descarta los que no corresponden. La suscripción a la
//! upcall es única y permanente, y se abre con [`init`].
//!
//! Limitaciones relevantes:
//!
//! - Hay un solo slot por botón, por lo que dos eventos sin una espera activa se
//!   reducen al último.
//! - La frontera temporal de una espera es laxa. Tras una espera cancelada, la
//!   siguiente puede recibir un evento anterior a su inicio.
//!
//! El análisis completo está en `docs/documentacion.md`, secciones 2.2 y 3.

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, Ordering};
use core::task::{Context, Poll, Waker};

use critical_section::Mutex;
use libtock_platform::subscribe::OneId;
use libtock_platform::{ErrorCode, Syscalls, Upcall, share};

use crate::Never;
use crate::runtime::TockSyscalls as S;

/// Driver number de la cápsula de botones.
pub const DRIVER_NUM: u32 = 0x3;
/// Subscribe number de la upcall de botones, común a todos los botones.
pub const SUBSCRIBE_BUTTON: u32 = 0;

mod command {
    pub const COUNT: u32 = 0;
    pub const ENABLE_INTERRUPTS: u32 = 1;
    pub const DISABLE_INTERRUPTS: u32 = 2;
    pub const READ: u32 = 3;
}

/// Capacidad de la tabla de slots. [`init`] verifica que la placa no la supere.
const MAX_BUTTONS: usize = 8;

/// Estado lógico de un botón, normalizado por la cápsula según la polaridad del pin.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ButtonState {
    Released,
    Pressed,
}

impl From<u32> for ButtonState {
    fn from(value: u32) -> Self {
        match value {
            0 => ButtonState::Released,
            _ => ButtonState::Pressed,
        }
    }
}

/// Punto de encuentro entre la upcall y la espera de un botón.
///
/// Se usa `Cell` y no `RefCell` porque la upcall puede ejecutarse durante un
/// `yield` dentro del `poll` de otra tarea.
struct Slot {
    waker: Mutex<Cell<Option<Waker>>>,
    last: Mutex<Cell<Option<ButtonState>>>,
}

impl Slot {
    const fn new() -> Self {
        Self {
            waker: Mutex::new(Cell::new(None)),
            last: Mutex::new(Cell::new(None)),
        }
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

    fn clear_waker(&self) {
        critical_section::with(|cs| self.waker.borrow(cs).set(None))
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

    fn put(&self, state: ButtonState) {
        critical_section::with(|cs| self.last.borrow(cs).set(Some(state)))
    }

    /// Consume el evento pendiente, si lo hay.
    fn take(&self) -> Option<ButtonState> {
        critical_section::with(|cs| self.last.borrow(cs).replace(None))
    }

    fn clear_last(&self) {
        critical_section::with(|cs| self.last.borrow(cs).set(None))
    }
}

static SLOTS: [Slot; MAX_BUTTONS] = [const { Slot::new() }; MAX_BUTTONS];

/// Bitmask de los índices ya entregados por [`Button::take`].
static TAKEN: AtomicU32 = AtomicU32::new(0);

/// Estado actual de un botón (`command 3`).
fn read_state(index: u32) -> Result<ButtonState, ErrorCode> {
    let state: u32 = S::command(DRIVER_NUM, command::READ, index, 0).to_result()?;
    Ok(state.into())
}

/// Receptor único de la upcall de botones.
struct Listener;

static LISTENER: Listener = Listener;

impl Upcall<OneId<DRIVER_NUM, SUBSCRIBE_BUTTON>> for Listener {
    fn upcall(&self, index: u32, state: u32, _: u32) {
        // Un índice fuera de la tabla se ignora en lugar de provocar un panic
        // dentro de una upcall.
        if let Some(slot) = SLOTS.get(index as usize) {
            slot.put(ButtonState::from(state));
            slot.wake();
        }
    }
}

/// Provoca un panic si otro componente ya había registrado la upcall de botones,
/// en lugar de dejar esperas que nunca despiertan.
pub struct DetectSteal;

impl libtock_platform::subscribe::Config for DetectSteal {
    fn returned_nonnull_upcall(_driver_num: u32, _subscribe_num: u32) {
        panic!("button upcall already owned")
    }
}

/// Registra la upcall de botones de forma permanente y ejecuta `f`.
///
/// Como `f` devuelve [`Never`], el `share::scope` no termina y la suscripción
/// permanece válida durante toda la vida del proceso. [`crate::run`] va al fondo
/// del anidamiento:
///
/// ```ignore
/// fn main() {
///     buttons::init(|| libtock_embassy::run(|spawner| { ... }))
/// }
/// ```
pub fn init(f: impl FnOnce() -> Never) -> ! {
    let count: u32 = S::command(DRIVER_NUM, command::COUNT, 0, 0)
        .to_result::<u32, ErrorCode>()
        .expect("button driver not present");
    assert!(
        count as usize <= MAX_BUTTONS,
        "la placa reporta más botones que MAX_BUTTONS"
    );

    match share::scope(|subscribe| {
        S::subscribe::<_, _, DetectSteal, DRIVER_NUM, SUBSCRIBE_BUTTON>(subscribe, &LISTENER)
            .expect("could not subscribe to the button upcall");
        f()
    }) {}
}

/// Acceso exclusivo a un botón.
///
/// Las esperas toman `&mut self`, lo que garantiza una sola espera por botón. Es
/// necesario porque deshabilitar un botón lo desactiva para todo el proceso.
pub struct Button {
    index: u32,
}

impl Button {
    /// Cantidad de botones de la placa.
    pub fn count() -> Result<u32, ErrorCode> {
        S::command(DRIVER_NUM, command::COUNT, 0, 0).to_result()
    }

    /// Toma el botón `index`. Devuelve `None` si no existe o si ya fue tomado.
    pub fn take(index: u32) -> Option<Button> {
        if index as usize >= MAX_BUTTONS || index >= Self::count().ok()? {
            return None;
        }
        let bit = 1u32 << index;
        if TAKEN.fetch_or(bit, Ordering::AcqRel) & bit != 0 {
            None
        } else {
            Some(Button { index })
        }
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    /// Lee el estado actual, sin esperar.
    pub fn read(&self) -> Result<ButtonState, ErrorCode> {
        read_state(self.index)
    }

    /// Espera la próxima transición y devuelve el estado nuevo. Semántica de
    /// flanco: aunque el botón no se mueva, nunca termina sin un evento nuevo.
    pub async fn wait_for_change(&mut self) -> ButtonState {
        ButtonWait::new(self.index, None).await
    }

    /// Espera hasta que el botón esté presionado. Semántica de nivel: si ya lo
    /// está, termina de inmediato.
    pub async fn wait_for_press(&mut self) {
        ButtonWait::new(self.index, Some(ButtonState::Pressed)).await;
    }

    /// Espera hasta que el botón esté suelto. Semántica de nivel: si ya lo está,
    /// termina de inmediato.
    pub async fn wait_for_release(&mut self) {
        ButtonWait::new(self.index, Some(ButtonState::Released)).await;
    }
}

impl Drop for Button {
    fn drop(&mut self) {
        TAKEN.fetch_and(!(1u32 << self.index), Ordering::AcqRel);
    }
}

/// Espera sobre un botón.
///
/// Con `want` es una espera de nivel: el primer `poll` lee el estado y termina
/// si ya es el buscado. Sin `want`, de flanco. Ver `docs/documentacion.md`,
/// sección 2.2.
///
/// `want` filtra el tipo de evento dentro del future. Filtrar con un ciclo de
/// futures deshabilitaría el botón entre un evento descartado y el siguiente.
struct ButtonWait {
    index: u32,
    want: Option<ButtonState>,
    /// Indica si la interrupción ya fue habilitada por esta espera.
    armed: bool,
}

impl ButtonWait {
    fn new(index: u32, want: Option<ButtonState>) -> Self {
        Self {
            index,
            want,
            armed: false,
        }
    }
}

impl Future for ButtonWait {
    type Output = ButtonState;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<ButtonState> {
        let this = self.get_mut();
        let slot = &SLOTS[this.index as usize];

        if !this.armed {
            // Orden: limpiar, registrar, habilitar, consultar. Ver
            // `docs/documentacion.md`, secciones 2.2 y 3.
            slot.clear_last();
            slot.register(cx.waker());
            let _ = S::command(DRIVER_NUM, command::ENABLE_INTERRUPTS, this.index, 0)
                .to_result::<(), ErrorCode>();
            this.armed = true;

            // Nivel: se lee después de habilitar, así una transición posterior a la
            // lectura igual genera upcall. Si la lectura falla, se espera el evento.
            if let (Some(want), Ok(state)) = (this.want, read_state(this.index))
                && state == want
            {
                return Poll::Ready(state);
            }
        } else {
            slot.register(cx.waker());
        }

        match slot.take() {
            Some(state) => match this.want {
                Some(want) if want != state => Poll::Pending,
                _ => Poll::Ready(state),
            },
            None => Poll::Pending,
        }
    }
}

impl Drop for ButtonWait {
    fn drop(&mut self) {
        let slot = &SLOTS[self.index as usize];
        slot.clear_waker();
        if self.armed {
            let _ = S::command(DRIVER_NUM, command::DISABLE_INTERRUPTS, self.index, 0)
                .to_result::<(), ErrorCode>();
        }
        // No se hace `unsubscribe`: la suscripción es compartida por todos los
        // botones. Ver `docs/documentacion.md`, sección 1.4.
    }
}
