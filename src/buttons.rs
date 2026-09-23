//! Espera asincrónica de eventos de botón sobre la cápsula de botones de Tock
//! (driver num `0x3`).
//!
//! Dos tareas pueden esperar botones distintos sin bloquearse entre sí: cada una
//! suspende su propio future y el executor sigue corriendo el resto. Es el mismo
//! patrón de [`crate::time_driver`] pero con N fuentes de evento en vez de una.
//!
//! # El contrato con la cápsula
//!
//! - `command(3, 0)` devuelve la cantidad de botones de la placa.
//! - `command(3, 1, i)` habilita la interrupción del botón `i`, y `command(3, 2, i)`
//!   la deshabilita. El flanco **no se elige**: la cápsula fuerza `EitherEdge`, o
//!   sea siempre llegan los dos eventos, presionado y soltado.
//! - `command(3, 3, i)` lee el estado actual, sin interrupciones de por medio.
//! - La upcall trae `(índice, estado, _)`, con estado `1` = presionado.
//!
//! El filtrado por botón lo hace el kernel, no nosotros: `command 1` prende un bit
//! en el `subscribe_map` del grant de la app y `fired()` solo agenda la upcall para
//! las apps que tienen ese bit. Por eso `init` no habilita nada — habilitar es
//! responsabilidad de quien empieza a esperar.
//!
//! Ojo con la granularidad de ese bitmask: es **por app**, no por waiter. Un
//! `command 2` apaga el botón para todo el proceso. De ahí que [`Button`] se
//! entregue una sola vez por índice ([`Button::take`]) y que las esperas tomen
//! `&mut self`: dos waiters sobre el mismo botón serían un bug silencioso, donde
//! el `Drop` de uno deja mudo al otro. Acá el tipo lo vuelve imposible.
//!
//! # Se pierden eventos, a propósito
//!
//! Hay **un solo slot de evento por botón**. Si se aprieta y se suelta rápido
//! mientras nadie está en el `await`, el segundo evento pisa al primero y solo
//! sobrevive uno. Es inherente a un slot único y es la decisión tomada: la
//! alternativa sería una cola por botón, que en el caso normal (una tarea esperando
//! su botón) no compra nada y cuesta RAM estática en todos los casos. Si alguna vez
//! hace falta no perder ninguna transición, el lugar para cambiarlo es [`Slot`].
//!
//! # Por qué el orden del primer poll importa
//!
//! El primer `poll` hace, en este orden: limpiar `last`, registrar el waker,
//! habilitar la interrupción, y recién ahí mirar `last`. Limpiar **antes** de
//! habilitar es lo que define la frontera temporal de la espera: el kernel puede
//! haber encolado una upcall de una espera anterior que se canceló, y esa upcall se
//! entrega igual en el siguiente `yield`. Sin la limpieza, un `wait_for_change`
//! nuevo devolvería `Ready` de inmediato por un evento viejo. Con ella, todo lo que
//! aparezca en `last` después de habilitar es legítimo.

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
/// Subscribe number de la upcall de botones. Es una sola para todos los botones.
pub const SUBSCRIBE_BUTTON: u32 = 0;

mod command {
    pub const COUNT: u32 = 0;
    pub const ENABLE_INTERRUPTS: u32 = 1;
    pub const DISABLE_INTERRUPTS: u32 = 2;
    pub const READ: u32 = 3;
}

/// Cota de compilación para la tabla de slots. La cantidad real de botones se
/// sabe en runtime; [`init`] valida que la placa no reporte más que esto.
const MAX_BUTTONS: usize = 8;

/// Estado de un botón, ya normalizado por la cápsula (contempla la polaridad del
/// pin, así que "presionado" es lógico y no el nivel eléctrico).
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

/// Punto de encuentro entre la upcall y el future de un botón.
///
/// `Cell` y no `RefCell`: la upcall corre dentro de un `yield`, y ese `yield`
/// puede estar ocurriendo dentro del `poll()` de una tarea (por ejemplo, un
/// `Console::write`, que bloquea por dentro). Con `RefCell` el handler podría
/// encontrarse el préstamo tomado por ese `poll` y hacer panic en runtime. `Cell`
/// no tiene estado de préstamo, así que ese modo de falla no existe.
///
/// El `Mutex` de `critical-section` no es un lock con estado: es solo el token que
/// exige presentar una sección crítica para acceder, y es lo que vuelve el `Cell`
/// utilizable desde un `static`.
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

/// Receptor de la upcall. Es uno solo para todos los botones: la cápsula tiene un
/// único subscribe slot y despacha por índice.
struct Listener;

static LISTENER: Listener = Listener;

impl Upcall<OneId<DRIVER_NUM, SUBSCRIBE_BUTTON>> for Listener {
    fn upcall(&self, index: u32, state: u32, _: u32) {
        // Un índice fuera de rango solo puede venir de una placa que reporta más
        // botones de los que `init` aceptó, cosa que `init` ya descarta. Se ignora
        // en vez de indexar y hacer panic desde dentro de una upcall.
        if let Some(slot) = SLOTS.get(index as usize) {
            slot.put(ButtonState::from(state));
            slot.wake();
        }
    }
}

/// `subscribe::Config` que hace ruido si alguien más ya tenía registrada la upcall
/// de botones.
///
/// Mismo razonamiento que [`crate::time_driver::DetectSteal`]: el subscribe slot
/// `(0x3, 0)` es único, así que `libtock_buttons::Buttons::register_listener` o
/// cualquier otro usuario lo pisaría y, al cerrar su `share::scope`, haría
/// `unsubscribe`. El resultado serían esperas que nunca despiertan. Mejor un panic
/// ruidoso que un cuelgue silencioso.
pub struct DetectSteal;

impl libtock_platform::subscribe::Config for DetectSteal {
    fn returned_nonnull_upcall(_driver_num: u32, _subscribe_num: u32) {
        panic!("button upcall already owned")
    }
}

/// Registra la upcall de botones y corre `f` con el listener vivo.
///
/// Igual que [`crate::time_driver::init`], la suscripción se abre en un
/// `share::scope` que por tipo no puede terminar: `f` devuelve [`Never`], así que
/// `Subscribe::drop` no corre jamás y el kernel puede escribir en la upcall durante
/// toda la vida del proceso.
///
/// Compone anidado con el de tiempo. Como [`crate::run`] ya abre el de alarma por
/// dentro, este va por fuera:
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
/// Se obtiene una sola vez por índice con [`Button::take`]. Las esperas toman
/// `&mut self`, así que no puede haber dos waiters sobre el mismo botón (ver la
/// nota sobre el `subscribe_map` en el doc del módulo).
///
/// Lleva solo un `u32`, así que es `'static` y entra en un `#[embassy_executor::task]`.
pub struct Button {
    index: u32,
}

impl Button {
    /// Cantidad de botones de la placa.
    pub fn count() -> Result<u32, ErrorCode> {
        S::command(DRIVER_NUM, command::COUNT, 0, 0).to_result()
    }

    /// Toma el botón `index`, o `None` si no existe o si ya fue tomado.
    ///
    /// Devolverlo (drop) libera el índice para un `take` posterior.
    pub fn take(index: u32) -> Option<Button> {
        if index as usize >= MAX_BUTTONS || index >= Self::count().ok()? {
            return None;
        }
        let bit = 1u32 << index;
        // `fetch_or` devuelve el valor previo: si el bit ya estaba puesto, el botón
        // tiene dueño.
        if TAKEN.fetch_or(bit, Ordering::AcqRel) & bit != 0 {
            None
        } else {
            Some(Button { index })
        }
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    /// Lee el estado actual, sin esperar. No necesita la interrupción habilitada.
    pub fn read(&self) -> Result<ButtonState, ErrorCode> {
        let state: u32 = S::command(DRIVER_NUM, command::READ, self.index, 0).to_result()?;
        Ok(state.into())
    }

    /// Espera la próxima transición, sea cual sea, y devuelve el estado nuevo.
    pub async fn wait_for_change(&mut self) -> ButtonState {
        ButtonWait::new(self.index, None).await
    }

    /// Espera hasta que el botón se presione.
    pub async fn wait_for_press(&mut self) {
        ButtonWait::new(self.index, Some(ButtonState::Pressed)).await;
    }

    /// Espera hasta que el botón se suelte.
    pub async fn wait_for_release(&mut self) {
        ButtonWait::new(self.index, Some(ButtonState::Released)).await;
    }
}

impl Drop for Button {
    fn drop(&mut self) {
        TAKEN.fetch_and(!(1u32 << self.index), Ordering::AcqRel);
    }
}

/// Future de una espera sobre un botón.
///
/// `want` filtra: `None` acepta cualquier transición, `Some(s)` descarta las que no
/// son `s` y vuelve a `Pending`. El filtro vive **acá dentro** y no en un loop de
/// futures en [`Button::wait_for_press`] a propósito: un loop crearía un future
/// nuevo por cada evento descartado, y entre el `Drop` de uno y el `enable` del
/// siguiente el botón queda deshabilitado. Esa ventana perdería justamente el
/// evento que se está esperando.
struct ButtonWait {
    index: u32,
    want: Option<ButtonState>,
    /// Si ya corrió el primer `poll`, o sea si la interrupción está habilitada.
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
            // El orden es la parte delicada; está explicado en el doc del módulo.
            slot.clear_last();
            slot.register(cx.waker());
            // Habilitar acá y deshabilitar en `Drop` significa 2 syscalls por
            // espera. La alternativa es habilitar en `Button::take` y deshabilitar
            // en `Drop for Button`: cero syscalls por espera, a cambio de upcalls
            // que solo escriben `last` y se descartan mientras nadie espera. Para
            // medir esa variante, mover estas dos líneas y la de `Drop` — el resto
            // del módulo no cambia.
            let _ = S::command(DRIVER_NUM, command::ENABLE_INTERRUPTS, this.index, 0)
                .to_result::<(), ErrorCode>();
            this.armed = true;
        } else {
            // El waker puede cambiar entre polls si la tarea migró de contexto.
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
        // Deliberadamente NO se hace `unsubscribe` acá.
        //
        // El listener de `(0x3, 0)` es único y compartido por todos los botones: lo
        // registra `init` una vez para todo el proceso. Un `unsubscribe` desde el
        // Drop de *un* future dejaría sin eventos a todos los demás botones.
        //
        // Esto marca el límite de lo que el patrón actual de `libtock-rs` puede
        // expresar. `share::scope` ata la vida de la suscripción a un bloque léxico,
        // que es justo lo que una tarea async cancelable no tiene. Acá se esquiva
        // haciendo la suscripción permanente, pero la solución general —un future
        // cuyo `Drop` haga `unsubscribe` antes de liberar la memoria que el kernel
        // podría escribir— sigue pendiente, y es la contribución central de la
        // tesis. Ver sección 6 de la skill `tock-embassy-async`.
    }
}
