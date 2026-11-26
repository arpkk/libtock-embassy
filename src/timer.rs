//! Timers relativos de 32 bits sobre la cápsula de alarma de Tock (driver `0x0`).
//!
//! Se activa con la feature `relative-timer` y reemplaza a `embassy_time`. Un
//! `Timer` se describe igual que los argumentos de `command 6`: `dt` ticks
//! contados desde `reference`, en ticks crudos del contador. No hay reloj de 64
//! bits ni alarma de mantenimiento: sin timers activos no se arma nada.
//!
//! Cada alarma cubre a lo sumo [`MAX_DT`] ticks. Un timer más largo, hasta
//! [`MAX_TICKS`], se recorre en tramos de ese largo, con una alarma y una upcall
//! por tramo.
//!
//! El objetivo es gastar la menor cantidad de syscalls posible:
//!
//! - La alarma se decide una sola vez por pasada del executor, en [`flush`],
//!   justo antes del `yield`. Muchos timers que cambian en una pasada cuestan a lo
//!   sumo un `command 6`.
//! - `Timer::after` no lee el contador al crearse. Su `dt` se empieza a contar en
//!   el `flush` de esa pasada, con una sola lectura para todos los timers creados
//!   en ella. Puede esperar un poco más que `dt`, nunca menos.
//! - `Timer::new` no lee el contador: se ubica en la cola respecto de la última
//!   hora conocida.
//! - La upcall usa la hora que trae (`arg0`) en vez de leer el contador.
//! - Un `poll` repetido no hace syscalls.

mod queue;

use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, Ordering};
use core::task::{Context, Poll};

#[cfg(feature = "stats")]
use core::sync::atomic::Ordering::Relaxed;

use critical_section::Mutex;
use libtock_platform::subscribe::OneId;
use libtock_platform::{ErrorCode, Syscalls, Upcall, share};

use crate::Never;
use crate::runtime::TockSyscalls as S;
use crate::source::DetectSteal;
use queue::{Action, Armed, Link, Node, Queue};

#[cfg(feature = "stats")]
pub use crate::stats;

const DRIVER_NUM: u32 = 0x0;
const SUBSCRIBE_ALARM: u32 = 0;

mod command {
    pub const FREQUENCY: u32 = 1;
    pub const TIME: u32 = 2;
    pub const STOP: u32 = 3;
    pub const SET_ABSOLUTE: u32 = 6;
}

/// Plazo más largo que se arma en una sola alarma: media vuelta del contador.
/// Deja otra media vuelta para reconocer un plazo vencido aunque se atienda
/// tarde. Los timers más largos se recorren en tramos de este largo.
pub const MAX_DT: u32 = queue::SEGMENT; //256 segundos

/// Plazo máximo de un timer: `2^32` tramos de [`MAX_DT`], unos 35.000 años a
/// 8.388.608 Hz.
pub const MAX_TICKS: u64 = (MAX_DT as u64) << 32;

static HZ: AtomicU32 = AtomicU32::new(0);

static QUEUE: Mutex<RefCell<Queue>> = Mutex::new(RefCell::new(Queue::new()));

/// Frecuencia del contador en Hz (`command 1`). Se lee una vez y queda guardada.
pub fn frequency() -> u32 {
    let hz = HZ.load(Ordering::Relaxed);
    if hz != 0 {
        return hz;
    }
    let hz = S::command(DRIVER_NUM, command::FREQUENCY, 0, 0)
        .to_result::<u32, ErrorCode>()
        .expect("alarm driver not present");
    HZ.store(hz, Ordering::Relaxed);
    hz
}

/// Valor actual del contador de 32 bits (`command 2`). La lectura también le
/// sirve a la cola como hora conocida.
pub fn now() -> u32 {
    let now = read();
    critical_section::with(|cs| QUEUE.borrow_ref_mut(cs).advance(now));
    now
}

fn read() -> u32 {
    #[cfg(feature = "stats")]
    stats::TIME.fetch_add(1, Relaxed);
    S::command(DRIVER_NUM, command::TIME, 0, 0)
        .to_result::<u32, ErrorCode>()
        .expect("alarm driver not present")
}

/// Convierte milisegundos a ticks, redondeando hacia arriba para no despertar antes.
pub fn ticks_from_millis(ms: u32) -> u32 {
    ticks(ms, 1_000)
}

/// Convierte microsegundos a ticks, redondeando hacia arriba para no despertar antes.
pub fn ticks_from_micros(us: u32) -> u32 {
    ticks(us, 1_000_000)
}

/// Convierte segundos a ticks, para timers más largos que [`MAX_DT`].
pub fn ticks_from_secs(secs: u32) -> u64 {
    secs as u64 * frequency() as u64
}

fn ticks(value: u32, per_second: u64) -> u32 {
    let ticks = (value as u64 * frequency() as u64).div_ceil(per_second);
    u32::try_from(ticks).expect("la duración no cabe en 32 bits de ticks")
}

/// Arma la alarma del kernel según lo que cambió en la cola durante la pasada.
///
/// El executor la llama antes de cada `yield`. Es la única que lee el contador
/// para los `Timer::after` nuevos y la única que arma la alarma.
pub(crate) fn flush() { // lee la hora una vez para los timers nuevos y decide la alarma
    critical_section::with(|cs| {
        let mut queue = QUEUE.borrow_ref_mut(cs);
        if queue.has_pending() {
            queue.flush_pending(read()); //read hace cmd2 
        }
        match queue.decide(frequency()) {
            Action::Nothing => {}
            Action::Skip => {
                #[cfg(feature = "stats")]
                stats::SKIPPED.fetch_add(1, Relaxed);
            }
            Action::Arm { reference, dt } => {
                #[cfg(feature = "stats")]
                stats::SET.fetch_add(1, Relaxed);
                // Con `reference` explícita no importa cuánto pasó desde la última
                // hora conocida: si el plazo ya venció, la cápsula encola la upcall
                // en esta misma syscall.
                let expiration = S::command(DRIVER_NUM, command::SET_ABSOLUTE, reference, dt) //cmd 6
                    .to_result::<u32, ErrorCode>()
                    .expect("no se pudo armar la alarma");
                queue.set_armed(Some(Armed {
                    reference,
                    dt,
                    expiration,
                }));
            }
            Action::Stop => {
                #[cfg(feature = "stats")]
                stats::SET.fetch_add(1, Relaxed);
                // Falla con `ALREADY` si venció entretanto; su upcall llega igual
                // y la cola la reconoce como vieja.
                let _ = S::command(DRIVER_NUM, command::STOP, 0, 0).to_result::<(), ErrorCode>();
                queue.set_armed(None);
            }
        }
    })
}

/// Espera hasta que pasen `dt` ticks desde `reference`.
///
/// Un `dt` mayor que [`MAX_DT`] se recorre en tramos. El tamaño del `Timer` no
/// depende de `dt`.
///
/// Igual que `embassy_time::Timer`, cede al menos una vez aunque el plazo ya haya
/// vencido, para que una tarea atrasada no acapare el executor. Si se descarta
/// antes de terminar, sale de la cola.
pub struct Timer {
    reference: Option<u32>, //momento desde que se cuenta
    dt: u32, //cuántos ticks esperar desde reference (primer tramo)
    node: Node, //nodo de la cola
}

impl Timer {
    /// Timer de `dt` ticks contados desde el final de la pasada actual del executor.
    pub fn after(dt: u32) -> Self {
        Self::with(None, dt as u64)
    }

    /// Como [`Timer::after`], para plazos que no caben en 32 bits.
    ///
    /// Provoca un panic si `dt` supera [`MAX_TICKS`].
    pub fn after_long(dt: u64) -> Self {
        Self::with(None, dt)
    }

    /// Timer de `dt` ticks contados desde `reference`. Sirve para tareas periódicas:
    /// sumar `dt` a `reference` en cada ciclo evita acumular deriva.
    ///
    /// `reference` tiene que venir de [`now`] o ser un plazo anterior. Una
    /// referencia en el futuro se ve como un plazo vencido, igual que en la cápsula.
    pub fn new(reference: u32, dt: u32) -> Self {
        Self::with(Some(reference), dt as u64)
    }

    /// Como [`Timer::new`], para plazos que no caben en 32 bits.
    ///
    /// Provoca un panic si `dt` supera [`MAX_TICKS`].
    pub fn new_long(reference: u32, dt: u64) -> Self {
        Self::with(Some(reference), dt)
    }

    /// Parte `dt` en un primer tramo de 1 a [`MAX_DT`] ticks, seguido de tramos
    /// completos de [`MAX_DT`].
    fn with(reference: Option<u32>, dt: u64) -> Self {
        assert!(dt <= MAX_TICKS, "dt supera MAX_TICKS");
        let segments = dt.saturating_sub(1) / MAX_DT as u64;
        let node = Node::new();
        node.set_segments(segments as u32);
        Self {
            reference,
            dt: (dt - segments * MAX_DT as u64) as u32,
            node,
        }
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // Basta una referencia compartida: el nodo solo tiene `Cell`s.
        let this = self.into_ref().get_ref();
        critical_section::with(|cs| {
            let mut queue = QUEUE.borrow_ref_mut(cs);
            match this.node.link() {
                Link::Idle => {
                    this.node.register(cx.waker()); //clona waker a node.waker
                    let waiting = match this.reference {
                        _ if this.dt == 0 => false,
                        None => {
                            queue.push_pending(&this.node, this.dt); //copia dt a node.delta y se marca pendiente
                            true
                        }
                        Some(reference) => queue.insert(&this.node, reference, this.dt),
                    };
                    if !waiting {
                        this.node.set_fired();
                        cx.waker().wake_by_ref();
                    }
                    Poll::Pending
                }
                Link::Pending | Link::Linked => {
                    this.node.register(cx.waker());
                    Poll::Pending
                }
                Link::Fired => Poll::Ready(()),
            }
        })
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        if matches!(self.node.link(), Link::Pending | Link::Linked) {
            critical_section::with(|cs| QUEUE.borrow_ref_mut(cs).remove(&self.node));
        }
    }
}

struct AlarmUpcall;

static UPCALL: AlarmUpcall = AlarmUpcall;

impl Upcall<OneId<DRIVER_NUM, SUBSCRIBE_ALARM>> for AlarmUpcall {
    /// `arg0` es la hora en que la cápsula atendió el vencimiento y `arg1` el
    /// vencimiento, el mismo valor que devolvió el `command 6`. La alarma no se
    /// re-arma acá: lo hace `flush` al final de la pasada.
    fn upcall(&self, now: u32, expiration: u32, _: u32) {
        #[cfg(feature = "stats")]
        stats::UPCALLS.fetch_add(1, Relaxed);
        critical_section::with(|cs| {
            let mut queue = QUEUE.borrow_ref_mut(cs);
            let now = if queue.is_current(now) { now } else { read() };
            queue.on_upcall(now, expiration);
        })
    }
}

/// Registra la upcall de alarma y corre `f`. El `share::scope` no termina porque
/// `f` devuelve [`Never`], así que la suscripción dura toda la vida del proceso.
pub fn init(f: impl FnOnce() -> Never) -> ! {
    frequency();
    match share::scope(|subscribe| {
        S::subscribe::<_, _, DetectSteal, DRIVER_NUM, SUBSCRIBE_ALARM>(subscribe, &UPCALL)
            .expect("could not subscribe to the alarm upcall");
        f()
    }) {}
}
