//! Implementación de `embassy_time_driver::Driver` sobre la cápsula de alarma
//! de Tock (driver num `0x0`).
//!
//! Es la pieza que le faltaba al prototipo: con esto `embassy_time::Timer`
//! funciona y varias tareas pueden dormir a la vez sobre la *única* alarma que
//! el kernel le da al proceso. La cola de plazos no la escribimos nosotros: la
//! reutilizamos de `embassy-time-queue-utils`, que guarda el nodo dentro de la
//! propia tarea de `embassy-executor` (sin heap).
//!
//! # El contrato con la cápsula
//!
//! - `command(0, 1)` devuelve la frecuencia y `command(0, 2)` el contador. La
//!   cápsula entrega los ticks *left-justified* a 32 bits (`into_u32_left_justified`),
//!   así que el contador que ve userspace siempre da la vuelta exactamente en
//!   `2^32`, sea cual sea el ancho del timer del chip, y la frecuencia viene
//!   escalada por el mismo factor.
//! - `command(0, 6, reference, dt)` (`SET_ABSOLUTE`) arma la alarma para
//!   `reference + dt`. Sobrescribe incondicionalmente la alarma anterior del
//!   proceso, que es justo lo que necesitamos: solo hay un plazo por app y el
//!   más próximo de la cola siempre gana.
//! - Un plazo ya vencido no se pierde: la cápsula considera vencida toda alarma
//!   cuyo `now` quede fuera de `[reference, reference + dt)` y dispara la upcall
//!   de inmediato.
//!
//! # Extensión a 64 bits
//!
//! `Driver::now` tiene que ser monótono y no desbordar nunca, pero el contador
//! de Tock es de 32 bits. Contamos las vueltas en software (`high`) detectando
//! `raw < last`. Eso obliga a leer el contador al menos una vez por vuelta, y
//! como no existe una upcall de overflow, el que garantiza esas lecturas es
//! `MAX_DT`: toda alarma se acota a `now + MAX_DT`, así que si la cola está
//! vacía igual queda armado un "keepalive" que nos despierta a tiempo.
//!
//! El keepalive se auto-sostiene, pero recién arranca con el primer
//! `schedule_wake`. Una app que nunca use un `Timer` y solo consulte
//! `Instant::now()` de a ratos puede perderse una vuelta. En cuanto hay un solo
//! timer en juego, la cadena no se corta más.
#![allow(unsafe_code)] // `time_driver_impl!` expande a `#[unsafe(no_mangle)]`.

use core::cell::{Cell, RefCell};
use core::task::Waker;

use critical_section::{CriticalSection, Mutex};
use embassy_time_driver::Driver;
use embassy_time_queue_utils::Queue;
use libtock_platform::subscribe::OneId;
use libtock_platform::{ErrorCode, Syscalls, Upcall, share};

use crate::runtime::TockSyscalls as S;

/// Driver number de la cápsula de alarma.
pub const DRIVER_NUM: u32 = 0x0;
/// Subscribe number de la upcall de alarma.
pub const SUBSCRIBE_ALARM: u32 = 0;

mod command {
    pub const FREQUENCY: u32 = 1;
    pub const TIME: u32 = 2;
    pub const SET_ABSOLUTE: u32 = 6;
}

/// Cota superior de `dt`. Hace dos trabajos a la vez:
///
/// - deja `dt` bastante por debajo de media vuelta del contador, así el test de
///   vencimiento con aritmética modular de la cápsula no es ambiguo;
/// - fuerza a que `now()` corra ~4 veces por vuelta, que es lo que mantiene
///   correcta la extensión a 64 bits (no hay upcall de overflow).
const MAX_DT: u64 = 1 << 30;

/// Driver de tiempo de Embassy respaldado por la cápsula de alarma de Tock.
pub struct TockDriver {
    /// Cuenta de vueltas del contador de 32 bits: la parte alta de `now()`.
    high: Mutex<Cell<u32>>,
    /// Último valor crudo leído. Sirve de detector de vuelta y de `reference`.
    last: Mutex<Cell<u32>>,
    /// Cola de plazos, reutilizada de `embassy-time-queue-utils`.
    queue: Mutex<RefCell<Queue>>,
}

embassy_time_driver::time_driver_impl!(static DRIVER: TockDriver = TockDriver {
    high: Mutex::new(Cell::new(0)),
    last: Mutex::new(Cell::new(0)),
    queue: Mutex::new(RefCell::new(Queue::new())),
});

impl Driver for TockDriver {
    fn now(&self) -> u64 {
        critical_section::with(|cs| self.extend(cs))
    }

    fn schedule_wake(&self, at: u64, waker: &Waker) {
        critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();
            if queue.schedule_wake(at, waker) {
                let mut next = queue.next_expiration(self.extend(cs));
                while !self.set_alarm(cs, next) {
                    next = queue.next_expiration(self.extend(cs));
                }
            }
        })
    }
}

impl TockDriver {
    /// Lee el contador de 32 bits y lo pliega en la extensión de 64.
    fn extend(&self, cs: CriticalSection) -> u64 {
        let last = self.last.borrow(cs);
        let high = self.high.borrow(cs);
        // Ante un error nos quedamos con el valor cacheado: `now()` no puede
        // fallar ni retroceder. Devolver 0 se vería como una vuelta y corrompería
        // `high` para siempre.
        let raw: u32 = S::command(DRIVER_NUM, command::TIME, 0, 0)
            .to_result()
            .unwrap_or_else(|_: ErrorCode| last.get());
        if raw < last.get() {
            high.set(high.get() + 1);
        }
        last.set(raw);
        ((high.get() as u64) << 32) | raw as u64
    }

    /// Arma la alarma del kernel. Devuelve `false` si `at` ya pasó, en cuyo caso
    /// quien llama tiene que volver a drenar la cola.
    ///
    /// Con la feature `trace-time` deja además un registro en [`trace`]. La
    /// grabación va acá afuera y no dentro de `arm` para que el camino caliente
    /// quede idéntico cuando la feature está apagada.
    fn set_alarm(&self, cs: CriticalSection, at: u64) -> bool {
        let armed = self.arm(cs, at);
        #[cfg(feature = "trace-time")]
        trace::record(cs, self.high.borrow(cs).get(), self.last.borrow(cs).get(), at, armed);
        armed
    }

    fn arm(&self, cs: CriticalSection, at: u64) -> bool {
        let now = self.extend(cs);
        // El clamp también cubre `at == u64::MAX` (cola vacía): queda un
        // keepalive pelado, que es lo que mantiene viva la cuenta de vueltas.
        let at = at.min(now + MAX_DT);
        if at <= now { //el plazo ya paso
            return false;
        }
        // `extend` acaba de refrescar `last`, así que `reference <= now`.
        let reference = self.last.borrow(cs).get();
        let dt = (at - now) as u32;
        S::command(DRIVER_NUM, command::SET_ABSOLUTE, reference, dt)
            .to_result::<u32, ErrorCode>()
            .is_ok()
    }

    /// La "ISR". En un HAL de Embassy esto sería el handler del interrupt; acá
    /// lo dispara la upcall de la cápsula, que corre dentro de un `yield`.
    fn on_alarm(&self) {
        critical_section::with(|cs| {
            let mut queue = self.queue.borrow(cs).borrow_mut();
            let mut next = queue.next_expiration(self.extend(cs)); //despierta a los vencidos y devuelve el próximo
            while !self.set_alarm(cs, next) { //impide que el proceso duerma para siempre
                next = queue.next_expiration(self.extend(cs));
            }
        })
    }
}

impl Upcall<OneId<DRIVER_NUM, SUBSCRIBE_ALARM>> for TockDriver {
    // arg0 es `now` y arg1 el vencimiento, ambos left-justified. Ignoramos los
    // dos: arg0 sería un `now()` gratis, y es sano usarlo porque la cápsula lo
    // lee al disparar (o sea, después de nuestro `SET_ABSOLUTE`, nunca puede
    // correr el detector de vueltas hacia atrás), pero ahorrarse una syscall no
    // paga tener que reconstruir ese argumento de orden cada vez que se toca
    // este archivo.
    fn upcall(&self, _now: u32, _expiration: u32, _: u32) {
        self.on_alarm()
    }
}

/// `subscribe::Config` que hace ruido si alguien más ya tenía registrada la
/// upcall de alarma.
///
/// Es la trampa para el conflicto real de este diseño: [`crate::alarm`] y
/// cualquier otro usuario de `Alarm::sleep_for` se suscriben al mismo par
/// `(0, 0)` y, al salir de su `share::scope`, hacen `unsubscribe` y dejan al
/// driver de tiempo mudo. Mejor un panic ruidoso que timers que no disparan.
pub struct DetectSteal;

impl libtock_platform::subscribe::Config for DetectSteal {
    fn returned_nonnull_upcall(_driver_num: u32, _subscribe_num: u32) {
        panic!("alarm upcall already owned")
    }
}

/// Tipo vacío: `Never` en posición de retorno obliga a que la función no vuelva.
/// Es `!` estable, que todavía no se puede escribir en un bound `FnOnce() -> !`.
pub enum Never {}

/// Registra la upcall de alarma y corre `f` con el driver de tiempo vivo.
///
/// La suscripción se abre en un `share::scope` que nunca termina, y el tipo lo
/// exige: `f` devuelve [`Never`], así que `Subscribe::drop` (y con él el
/// `unsubscribe`) no corre jamás y el kernel puede escribir en la upcall durante
/// toda la vida del proceso.
pub fn init(f: impl FnOnce() -> Never) -> ! {
    let hz: u32 = S::command(DRIVER_NUM, command::FREQUENCY, 0, 0)
        .to_result::<u32, ErrorCode>()
        .expect("alarm driver not present");
    assert_eq!(
        hz as u64,
        embassy_time_driver::TICK_HZ,
        "la feature tick-hz-* de embassy-time no calza con la placa"
    );

    match share::scope(|subscribe| {
        S::subscribe::<_, _, DetectSteal, DRIVER_NUM, SUBSCRIBE_ALARM>(subscribe, &DRIVER)
            .expect("could not subscribe to the alarm upcall");
        f()
    }) {}
}

/// Traza de las llamadas a `set_alarm`, para instrumentar el driver sin
/// perturbarlo.
///
/// El driver corre dentro de una sección crítica, así que no puede imprimir:
/// `Console::write` hace `yield_wait`, y eso dejaría entrar una upcall que
/// reentra `on_alarm` y revienta el `RefCell` de la cola. La salida es grabar en
/// un búfer estático (cero syscalls) y que alguien lo drene desde afuera.
///
/// Ver `examples/time_trace.rs`.
#[cfg(feature = "trace-time")]
pub mod trace {
    use super::{MAX_DT, Mutex, RefCell};
    use critical_section::CriticalSection;
    use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
    use embassy_sync::signal::Signal;

    /// Cuántos registros aguanta el búfer antes de empezar a pisar los viejos.
    pub const CAP: usize = 64;

    /// Una llamada a `set_alarm`.
    #[derive(Copy, Clone, Debug, Default)]
    pub struct Record {
        /// Cuenta de vueltas al momento de la llamada.
        pub high: u32,
        /// Último valor crudo del contador. Es el `reference` que fue al kernel.
        pub last: u32,
        /// `at` como llegó de la cola. `u64::MAX` = la cola quedó vacía.
        pub at_in: u64,
        /// `at` después del clamp a `now + MAX_DT`.
        pub at_out: u64,
        /// `false` = el plazo ya había vencido y no se armó nada.
        pub armed: bool,
    }

    impl Record {
        const EMPTY: Record = Record {
            high: 0,
            last: 0,
            at_in: 0,
            at_out: 0,
            armed: false,
        };

        /// `now()` reconstruido de `high`/`last`, sin volver a llamar al kernel.
        pub fn now(&self) -> u64 {
            ((self.high as u64) << 32) | self.last as u64
        }

        /// `dt` que se le mandó a la cápsula, en ticks. `None` si no se armó.
        pub fn dt(&self) -> Option<u32> {
            let now = self.now();
            (self.at_out > now).then(|| (self.at_out - now) as u32)
        }

        /// `true` si el clamp de `MAX_DT` recortó el plazo pedido.
        pub fn clamped(&self) -> bool {
            self.at_out < self.at_in
        }
    }

    struct Buffer {
        buf: [Record; CAP],
        /// Dónde se escribe el próximo registro.
        head: usize,
        len: usize,
        /// Registros pisados por no drenar a tiempo.
        lost: u32,
    }

    static TRACE: Mutex<RefCell<Buffer>> = Mutex::new(RefCell::new(Buffer {
        buf: [Record::EMPTY; CAP],
        head: 0,
        len: 0,
        lost: 0,
    }));

    /// Avisa que llegó al menos un registro nuevo.
    ///
    /// `Signal` retiene el estado, así que no hay despertares perdidos, y
    /// coalesce: varios registros entre dos drenajes son un solo despertar.
    static NEW: Signal<CriticalSectionRawMutex, ()> = Signal::new();

    /// Espera a que haya registros para drenar.
    ///
    /// Reemplaza al `Timer` periódico que tenía antes el drenador. La diferencia
    /// importa: un drenador con temporizador se auto-registra en la traza y
    /// ensucia justo lo que se quiere medir.
    pub async fn wait() {
        NEW.wait().await
    }

    /// Graba un registro. Solo escribe memoria: no hace syscalls y no cede el
    /// procesador, así que es seguro llamarlo desde la sección crítica.
    pub(super) fn record(cs: CriticalSection, high: u32, last: u32, at_in: u64, armed: bool) {
        {
            let mut t = TRACE.borrow(cs).borrow_mut();
            let now = ((high as u64) << 32) | last as u64;
            let head = t.head;
            t.buf[head] = Record {
                high,
                last,
                at_in,
                at_out: at_in.min(now + MAX_DT),
                armed,
            };
            t.head = (head + 1) % CAP;
            if t.len == CAP {
                t.lost = t.lost.saturating_add(1);
            } else {
                t.len += 1;
            }
        }
        // Fuera del `borrow_mut`: `signal` despierta al drenador, y aunque eso
        // solo lo encola (no lo ejecuta), no conviene tener el `RefCell` tomado
        // mientras se toca el run queue del executor.
        NEW.signal(());
    }

    /// Saca los registros acumulados, del más viejo al más nuevo.
    ///
    /// Devuelve cuántos copió en `out` y cuántos se perdieron por desborde desde
    /// el drenaje anterior. Si `out` es más chico que lo acumulado, lo que no
    /// quepa queda para la próxima.
    pub fn drain(out: &mut [Record]) -> (usize, u32) {
        critical_section::with(|cs| {
            let mut t = TRACE.borrow(cs).borrow_mut();
            let n = out.len().min(t.len);
            // El más viejo está `len` posiciones atrás de `head`.
            let start = (t.head + CAP - t.len) % CAP;
            for (i, slot) in out.iter_mut().enumerate().take(n) {
                *slot = t.buf[(start + i) % CAP];
            }
            t.len -= n;
            let lost = core::mem::take(&mut t.lost);
            (n, lost)
        })
    }
}
