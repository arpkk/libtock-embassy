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
//! La estructura común con botones está en el módulo `source`.

use core::future::Future;
use core::pin::Pin as Pinned;
use core::task::{Context, Poll};

use libtock_platform::{ErrorCode, Syscalls};

use crate::Never;
use crate::runtime::TockSyscalls as S;
use crate::source::SourceTable;

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

/// Capacidad de la tabla. [`init`] verifica que la placa no la supere, y
/// [`SourceTable`] no admite más de 32.
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

static PINS: SourceTable<Level, MAX_PINS, DRIVER_NUM, SUBSCRIBE_GPIO> = SourceTable::new();

/// Registra la upcall de GPIO de forma permanente y ejecuta `f`.
///
/// [`crate::run`] va al fondo del anidamiento:
///
/// ```ignore
/// fn main() {
///     gpio::init(|| buttons::init(|| libtock_embassy::run(|spawner| { ... })))
/// }
/// ```
pub fn init(f: impl FnOnce() -> Never) -> ! {
    let count = Pin::count().expect("gpio driver not present");
    PINS.init(count, f)
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
        // El bit se reserva antes de configurar el hardware para no reconfigurar un
        // pin que ya tiene dueño.
        if !PINS.claim(index) {
            return None;
        }
        // `command 5` configura el pin y a la vez verifica que exista.
        match S::command(DRIVER_NUM, command::ENABLE_INPUT, index, pull.arg())
            .to_result::<(), ErrorCode>()
        {
            Ok(()) => Some(Pin { index }),
            Err(_) => {
                PINS.release(index);
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
        PINS.release(self.index);
        // No se hace `unsubscribe`: la suscripción es compartida por todos los
        // pines.
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
        let (index, edge) = (this.index, this.edge);
        let slot = PINS.slot(index);

        slot.arm(&mut this.armed, cx.waker(), || {
            let _ = S::command(DRIVER_NUM, command::ENABLE_INTERRUPT, index, edge)
                .to_result::<(), ErrorCode>();
        });

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
        PINS.slot(self.index).clear_waker();
    }
}
