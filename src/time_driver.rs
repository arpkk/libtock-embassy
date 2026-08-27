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
    fn set_alarm(&self, cs: CriticalSection, at: u64) -> bool {
        let now = self.extend(cs);
        // El clamp también cubre `at == u64::MAX` (cola vacía): queda un
        // keepalive pelado, que es lo que mantiene viva la cuenta de vueltas.
        let at = at.min(now + MAX_DT);
        if at <= now {
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
            let mut next = queue.next_expiration(self.extend(cs));
            while !self.set_alarm(cs, next) {
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
