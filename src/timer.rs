//! Timers relativos de 32 bits sobre la cápsula de alarma de Tock (driver `0x0`).
//!
//! Se activa con la feature `relative-timer` y reemplaza a [`embassy_time`]: un
//! `Timer` es un par `(reference, dt)` en ticks crudos del contador, igual que los
//! argumentos de `command 6`. No hay reloj de 64 bits, no se cuentan vueltas y no
//! hace falta una alarma de mantenimiento: sin timers activos no se arma nada.
//!
//! Todo se compara con aritmética modular, como en la cápsula. Por eso `dt` se
//! limita a [`MAX_DT`]: un timer se reconoce como vencido mientras su atraso sea
//! menor que `2^32 - dt`. Un atraso mayor no se detecta y el timer espera una
//! vuelta más, pero el error no se acumula en ningún reloj.
//!
//! Los plazos activos viven en una tabla estática de [`MAX_TIMERS`] entradas.

use core::cell::RefCell;
use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, Ordering};
use core::task::{Context, Poll, Waker};

#[cfg(feature = "stats")]
use core::sync::atomic::Ordering::Relaxed;

use critical_section::Mutex;
use libtock_platform::subscribe::OneId;
use libtock_platform::{ErrorCode, Syscalls, Upcall, share};

use crate::Never;
use crate::runtime::TockSyscalls as S;
use crate::source::DetectSteal;

#[cfg(feature = "stats")]
pub use crate::stats;

const DRIVER_NUM: u32 = 0x0;
const SUBSCRIBE_ALARM: u32 = 0;

mod command {
    pub const FREQUENCY: u32 = 1;
    pub const TIME: u32 = 2;
    pub const SET_ABSOLUTE: u32 = 6;
}

/// `dt` máximo de un timer: media vuelta del contador.
pub const MAX_DT: u32 = 1 << 31;

/// Timers que pueden esperar a la vez. Superarlo provoca un panic.
pub const MAX_TIMERS: usize = 16;

static HZ: AtomicU32 = AtomicU32::new(0);

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

/// Valor actual del contador de 32 bits (`command 2`).
pub fn now() -> u32 {
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

fn ticks(value: u32, per_second: u64) -> u32 {
    let ticks = (value as u64 * frequency() as u64).div_ceil(per_second);
    u32::try_from(ticks).expect("la duración no cabe en 32 bits de ticks")
}

#[derive(Clone, Copy)]
struct Deadline {
    reference: u32,
    dt: u32,
}

impl Deadline {
    /// Mismo criterio que la cápsula: vencido si `now` salió de `[reference, reference + dt)`.
    fn expired(self, now: u32) -> bool {
        now.wrapping_sub(self.reference) >= self.dt
    }

    /// Ticks que faltan. Solo tiene sentido si no ha vencido.
    fn remaining(self, now: u32) -> u32 {
        self.dt - now.wrapping_sub(self.reference)
    }

    /// Ticks de atraso. Solo tiene sentido si ya venció.
    fn overdue(self, now: u32) -> u32 {
        now.wrapping_sub(self.reference) - self.dt
    }
}

enum Slot {
    Free,
    Waiting { deadline: Deadline, waker: Waker },
    Fired,
}

struct State {
    slots: [Slot; MAX_TIMERS],
    /// Alarma armada en el kernel cuya upcall todavía no se atendió.
    armed: Option<Deadline>,
}

static STATE: Mutex<RefCell<State>> = Mutex::new(RefCell::new(State {
    slots: [const { Slot::Free }; MAX_TIMERS],
    armed: None,
}));

impl State {
    /// Guarda un plazo nuevo y re-arma solo si la alarma armada no lo cubre.
    ///
    /// Si la alarma ya venció, su upcall está en cola y al atenderla se re-arma;
    /// re-armar ahora encolaría una upcall más por el mismo vencimiento.
    fn insert(&mut self, now: u32, deadline: Deadline, waker: &Waker) -> usize {
        let index = self
            .slots
            .iter()
            .position(|slot| matches!(slot, Slot::Free))
            .expect("más timers activos que MAX_TIMERS");
        self.slots[index] = Slot::Waiting {
            deadline,
            waker: waker.clone(),
        };
        let covered = self.armed.is_some_and(|armed| {
            if armed.expired(now) {
                // Si lleva más de 1 s vencida, la upcall se dio por perdida.
                armed.overdue(now) < frequency()
            } else {
                armed.remaining(now) <= deadline.remaining(now)
            }
        });
        if covered {
            #[cfg(feature = "stats")]
            stats::SKIPPED.fetch_add(1, Relaxed);
        } else {
            self.service(now);
        }
        index
    }

    /// Despierta los timers vencidos y arma la alarma para el más próximo.
    fn service(&mut self, now: u32) {
        let mut next: Option<Deadline> = None;
        for slot in &mut self.slots {
            let Slot::Waiting { deadline, waker } = slot else {
                continue;
            };
            let deadline = *deadline;
            if deadline.expired(now) {
                waker.wake_by_ref();
                *slot = Slot::Fired;
            } else if next.is_none_or(|n| deadline.remaining(now) < n.remaining(now)) {
                next = Some(deadline);
            }
        }
        self.armed = next;
        if let Some(deadline) = next {
            // Con `reference` explícita no importa cuánto pasó desde que se leyó `now`:
            // si el plazo venció entremedio, la cápsula encola la upcall en esta syscall.
            #[cfg(feature = "stats")]
            stats::SET.fetch_add(1, Relaxed);
            S::command(
                DRIVER_NUM,
                command::SET_ABSOLUTE,
                deadline.reference,
                deadline.dt,
            )
            .to_result::<u32, ErrorCode>()
            .expect("no se pudo armar la alarma");
        }
    }
}

enum Stage {
    New,
    Waiting(usize),
    Done,
}

/// Espera hasta que pasen `dt` ticks desde `reference`.
///
/// Igual que `embassy_time::Timer`, cede al menos una vez aunque el plazo ya haya
/// vencido, para que una tarea atrasada no acapare el executor. Si se descarta
/// antes de terminar, libera su entrada.
pub struct Timer {
    deadline: Deadline,
    stage: Stage,
}

impl Timer {
    /// Timer de `dt` ticks contados desde ahora.
    pub fn after(dt: u32) -> Self {
        Self::new(now(), dt)
    }

    /// Timer de `dt` ticks contados desde `reference`. Sirve para tareas periódicas:
    /// sumar `dt` a `reference` en cada ciclo evita acumular deriva.
    ///
    /// Provoca un panic si `dt` supera [`MAX_DT`].
    pub fn new(reference: u32, dt: u32) -> Self {
        assert!(dt <= MAX_DT, "dt supera MAX_DT");
        Self {
            deadline: Deadline { reference, dt },
            stage: Stage::New,
        }
    }
}

impl Future for Timer {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        critical_section::with(|cs| {
            let mut state = STATE.borrow_ref_mut(cs);
            match this.stage {
                Stage::New => {
                    let now = now();
                    if this.deadline.expired(now) {
                        this.stage = Stage::Done;
                        cx.waker().wake_by_ref();
                    } else {
                        let index = state.insert(now, this.deadline, cx.waker());
                        this.stage = Stage::Waiting(index);
                    }
                    Poll::Pending
                }
                Stage::Waiting(index) => match &mut state.slots[index] {
                    Slot::Waiting { waker, .. } => {
                        if !waker.will_wake(cx.waker()) {
                            *waker = cx.waker().clone();
                        }
                        Poll::Pending
                    }
                    _ => {
                        state.slots[index] = Slot::Free;
                        this.stage = Stage::Done;
                        Poll::Ready(())
                    }
                },
                Stage::Done => Poll::Ready(()),
            }
        })
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        // La alarma puede quedar armada para este plazo. Su upcall no despierta a
        // nadie y al atenderla se re-arma para el siguiente.
        if let Stage::Waiting(index) = self.stage {
            critical_section::with(|cs| {
                STATE.borrow_ref_mut(cs).slots[index] = Slot::Free;
            });
        }
    }
}

struct AlarmUpcall;

static UPCALL: AlarmUpcall = AlarmUpcall;

impl Upcall<OneId<DRIVER_NUM, SUBSCRIBE_ALARM>> for AlarmUpcall {
    fn upcall(&self, _now: u32, _expiration: u32, _: u32) {
        #[cfg(feature = "stats")]
        stats::UPCALLS.fetch_add(1, Relaxed);
        critical_section::with(|cs| {
            let mut state = STATE.borrow_ref_mut(cs);
            state.armed = None;
            state.service(now());
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
