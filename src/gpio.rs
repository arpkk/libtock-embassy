//! Espera asincrónica de flancos en pines GPIO (cápsula `0x4`). Solo entrada.
//!
//! Cada pin se obtiene una sola vez con [`Pin::take`], que lo configura como
//! entrada (`command 5`). Cada espera habilita la interrupción con el flanco pedido
//! (`command 7`), y soltar el [`Pin`] la deshabilita y desactiva el pin
//! (`command 8`). La suscripción a la upcall es única y permanente, y se abre con
//! [`init`].
//!
//! Limitaciones relevantes:
//!
//! - La cápsula no aísla procesos. Las upcalls se difunden a todas las aplicaciones
//!   que usaron el driver, y la configuración de cada pin es compartida.
//! - El nivel que entrega la upcall es una muestra tomada al atender la
//!   interrupción, no la dirección del flanco.
//! - Hay un solo slot por pin, por lo que dos flancos sin una espera activa se
//!   reducen al último.
//! - La frontera temporal de una espera es laxa.
//!
//! El análisis completo está en `docs/documentacion.md`, secciones 2.3 y 3.

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin as Pinned;
use core::sync::atomic::{AtomicU32, Ordering};
use core::task::{Context, Poll, Waker};

use critical_section::Mutex;
use libtock_platform::subscribe::OneId;
use libtock_platform::{ErrorCode, Syscalls, Upcall, share};

use crate::Never;
use crate::runtime::TockSyscalls as S;

/// Driver number de la cápsula de GPIO.
pub const DRIVER_NUM: u32 = 0x4;
/// Subscribe number de la upcall de GPIO, común a todos los pines.
pub const SUBSCRIBE_GPIO: u32 = 0;

mod command {
    pub const ENABLE_INPUT: u32 = 5;
    pub const READ: u32 = 6;
    pub const ENABLE_INTERRUPT: u32 = 7;
    pub const DISABLE_INTERRUPT: u32 = 8;
    pub const COUNT: u32 = 10;
}

/// Segundo argumento de `command 7`.
mod edge {
    pub const EITHER: u32 = 0;
    pub const RISING: u32 = 1;
    pub const FALLING: u32 = 2;
}

/// Capacidad de la tabla de slots. [`init`] verifica que la placa no la supere.
/// El límite superior es 32, el ancho de [`TAKEN`].
const MAX_PINS: usize = 16;

/// Resistencia interna de entrada. Usa los mismos nombres que `embassy-nrf`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Pull {
    None,
    Up,
    Down,
}

impl Pull {
    /// Segundo argumento de `command 5`.
    fn arg(self) -> u32 {
        match self {
            Pull::None => 0,
            Pull::Up => 1,
            Pull::Down => 2,
        }
    }
}

/// Nivel lógico de un pin. Usa los mismos nombres que `embassy-nrf`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Level {
    Low,
    High,
}

impl From<u32> for Level {
    fn from(value: u32) -> Self {
        match value {
            0 => Level::Low,
            _ => Level::High,
        }
    }
}

/// Punto de encuentro entre la upcall y la espera de un pin.
///
/// Se usa `Cell` y no `RefCell` porque la upcall puede ejecutarse durante un
/// `yield` dentro del `poll` de otra tarea.
struct Slot {
    waker: Mutex<Cell<Option<Waker>>>,
    last: Mutex<Cell<Option<Level>>>,
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

    fn put(&self, level: Level) {
        critical_section::with(|cs| self.last.borrow(cs).set(Some(level)))
    }

    /// Consume el evento pendiente, si lo hay.
    fn take(&self) -> Option<Level> {
        critical_section::with(|cs| self.last.borrow(cs).replace(None))
    }

    fn clear_last(&self) {
        critical_section::with(|cs| self.last.borrow(cs).set(None))
    }
}

static SLOTS: [Slot; MAX_PINS] = [const { Slot::new() }; MAX_PINS];

/// Bitmask de los índices ya entregados por [`Pin::take`].
static TAKEN: AtomicU32 = AtomicU32::new(0);

/// Receptor único de la upcall de GPIO.
struct Listener;

static LISTENER: Listener = Listener;

impl Upcall<OneId<DRIVER_NUM, SUBSCRIBE_GPIO>> for Listener {
    fn upcall(&self, index: u32, level: u32, _: u32) {
        // Pueden llegar flancos de pines que este proceso no tomó. Escribir su slot
        // es inocuo porque la siguiente espera lo limpia.
        if let Some(slot) = SLOTS.get(index as usize) {
            slot.put(Level::from(level));
            slot.wake();
        }
    }
}

/// Provoca un panic si otro componente ya había registrado la upcall de GPIO, en
/// lugar de dejar esperas que nunca despiertan.
pub struct DetectSteal;

impl libtock_platform::subscribe::Config for DetectSteal {
    fn returned_nonnull_upcall(_driver_num: u32, _subscribe_num: u32) {
        panic!("gpio upcall already owned")
    }
}

/// Registra la upcall de GPIO de forma permanente y ejecuta `f`.
///
/// Como `f` devuelve [`Never`], el `share::scope` no termina y la suscripción
/// permanece válida durante toda la vida del proceso. [`crate::run`] va al fondo
/// del anidamiento:
///
/// ```ignore
/// fn main() {
///     gpio::init(|| buttons::init(|| libtock_embassy::run(|spawner| { ... })))
/// }
/// ```
pub fn init(f: impl FnOnce() -> Never) -> ! {
    let count: u32 = S::command(DRIVER_NUM, command::COUNT, 0, 0)
        .to_result::<u32, ErrorCode>()
        .expect("gpio driver not present");
    assert!(
        count as usize <= MAX_PINS,
        "la placa reporta más pines que MAX_PINS"
    );

    match share::scope(|subscribe| {
        S::subscribe::<_, _, DetectSteal, DRIVER_NUM, SUBSCRIBE_GPIO>(subscribe, &LISTENER)
            .expect("could not subscribe to the gpio upcall");
        f()
    }) {}
}

/// Acceso exclusivo a un pin configurado como entrada.
///
/// Las esperas toman `&mut self`, lo que garantiza una sola espera por pin dentro
/// del proceso.
pub struct Pin {
    index: u32,
}

impl Pin {
    /// Cantidad de posiciones en la tabla de pines de la placa, incluidas las vacías.
    pub fn count() -> Result<u32, ErrorCode> {
        S::command(DRIVER_NUM, command::COUNT, 0, 0).to_result()
    }

    /// Toma el pin `index` y lo configura como entrada con la resistencia `pull`.
    ///
    /// Devuelve `None` si el índice no corresponde a un pin de la placa o si ya fue
    /// tomado.
    pub fn take(index: u32, pull: Pull) -> Option<Pin> {
        if index as usize >= MAX_PINS {
            return None;
        }
        // El bit se reserva antes de configurar el hardware para no reconfigurar un
        // pin que ya tiene dueño.
        let bit = 1u32 << index;
        if TAKEN.fetch_or(bit, Ordering::AcqRel) & bit != 0 {
            return None;
        }
        // `command 5` configura el pin y a la vez verifica que exista.
        match S::command(DRIVER_NUM, command::ENABLE_INPUT, index, pull.arg())
            .to_result::<(), ErrorCode>()
        {
            Ok(()) => Some(Pin { index }),
            Err(_) => {
                TAKEN.fetch_and(!bit, Ordering::AcqRel);
                None
            }
        }
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    /// Lee el nivel actual, sin esperar.
    pub fn read(&self) -> Result<Level, ErrorCode> {
        let level: u32 = S::command(DRIVER_NUM, command::READ, self.index, 0).to_result()?;
        Ok(level.into())
    }

    /// Espera un flanco de subida.
    pub async fn wait_for_rising_edge(&mut self) {
        PinWait::new(self.index, edge::RISING).await;
    }

    /// Espera un flanco de bajada.
    pub async fn wait_for_falling_edge(&mut self) {
        PinWait::new(self.index, edge::FALLING).await;
    }

    /// Espera un flanco cualquiera y devuelve el nivel muestreado por la cápsula,
    /// que puede no coincidir con la dirección del flanco.
    pub async fn wait_for_any_edge(&mut self) -> Level {
        PinWait::new(self.index, edge::EITHER).await
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        let _ = S::command(DRIVER_NUM, command::DISABLE_INTERRUPT, self.index, 0)
            .to_result::<(), ErrorCode>();
        TAKEN.fetch_and(!(1u32 << self.index), Ordering::AcqRel);
        // No se hace `unsubscribe`: la suscripción es compartida por todos los
        // pines. Ver `docs/documentacion.md`, sección 1.4.
    }
}

/// Espera sobre un pin. No filtra eventos porque el flanco lo selecciona el
/// hardware.
struct PinWait {
    index: u32,
    edge: u32,
    /// Indica si ya se emitió el `command 7` de esta espera.
    armed: bool,
}

impl PinWait {
    fn new(index: u32, edge: u32) -> Self {
        Self {
            index,
            edge,
            armed: false,
        }
    }
}

impl Future for PinWait {
    type Output = Level;

    fn poll(self: Pinned<&mut Self>, cx: &mut Context<'_>) -> Poll<Level> {
        let this = self.get_mut();
        let slot = &SLOTS[this.index as usize];

        if !this.armed {
            // Orden: limpiar, registrar, habilitar, consultar. Ver
            // `docs/documentacion.md`, sección 3.
            slot.clear_last();
            slot.register(cx.waker());
            let _ = S::command(DRIVER_NUM, command::ENABLE_INTERRUPT, this.index, this.edge)
                .to_result::<(), ErrorCode>();
            this.armed = true;
        } else {
            slot.register(cx.waker());
        }

        match slot.take() {
            Some(level) => Poll::Ready(level),
            None => Poll::Pending,
        }
    }
}

impl Drop for PinWait {
    fn drop(&mut self) {
        // La interrupción permanece habilitada hasta que se suelta el `Pin`, porque
        // deshabilitarla aquí exigiría reconfigurar el pin en la siguiente espera.
        SLOTS[self.index as usize].clear_waker();
    }
}
