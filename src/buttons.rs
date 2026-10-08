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
//! La estructura común con GPIO está en el módulo `source`.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use libtock_platform::{ErrorCode, Syscalls};

use crate::Never;
use crate::runtime::TockSyscalls as S;
use crate::source::SourceTable;

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

/// Capacidad de la tabla. [`init`] verifica que la placa no la supere.
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

static BUTTONS: SourceTable<ButtonState, MAX_BUTTONS, DRIVER_NUM, SUBSCRIBE_BUTTON> =
    SourceTable::new();

/// Estado actual de un botón (`command 3`).
fn read_state(index: u32) -> Result<ButtonState, ErrorCode> {
    let state: u32 = S::command(DRIVER_NUM, command::READ, index, 0).to_result()?;
    Ok(state.into())
}

/// Registra la upcall de botones de forma permanente y ejecuta `f`.
///
/// [`crate::run`] va al fondo del anidamiento:
///
/// ```ignore
/// fn main() {
///     buttons::init(|| libtock_embassy::run(|spawner| { ... }))
/// }
/// ```
pub fn init(f: impl FnOnce() -> Never) -> ! {
    let count = Button::count().expect("button driver not present");
    BUTTONS.init(count, f)
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
        // El `Button` se construye solo si la reserva tuvo éxito: su `Drop` libera
        // el bit, y uno construido de más liberaría el de otro dueño.
        if index < Self::count().ok()? && BUTTONS.claim(index) {
            Some(Button { index })
        } else {
            None
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
        BUTTONS.release(self.index);
    }
}

/// Espera sobre un botón.
///
/// Con `want` es una espera de nivel: el primer `poll` lee el estado y termina
/// si ya es el buscado. Sin `want`, de flanco.
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
        let index = this.index;
        let slot = BUTTONS.slot(index);

        let first = slot.arm(&mut this.armed, cx.waker(), || {
            let _ = S::command(DRIVER_NUM, command::ENABLE_INTERRUPTS, index, 0)
                .to_result::<(), ErrorCode>();
        });

        // Nivel: se lee después de habilitar, así una transición posterior a la
        // lectura igual genera upcall. Si la lectura falla, se espera el evento.
        if first
            && let (Some(want), Ok(state)) = (this.want, read_state(index))
            && state == want
        {
            return Poll::Ready(state);
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
        BUTTONS.slot(self.index).clear_waker();
        if self.armed {
            let _ = S::command(DRIVER_NUM, command::DISABLE_INTERRUPTS, self.index, 0)
                .to_result::<(), ErrorCode>();
        }
        // No se hace `unsubscribe`: la suscripción es compartida por todos los
        // botones.
    }
}
