//! Driver de alarma bloqueante
//!
//! `sleep_for` es `async` solo de firma. Adentro hace `share::scope` +
//! `subscribe` + `loop { yield_wait() }`, o sea nunca devuelve `Pending` ni cede
//! al executor: mientras una tarea duerme acá, las demás no avanzan. Se conserva
//! como referencia para los benchmarks contra [`crate::time_driver`].
//!
//! **No mezclar con el driver de tiempo.** Los dos se suscriben al par
//! `(driver 0x0, subscribe 0)`. Si una tarea llama a `sleep_for` mientras corre
//! [`crate::run`], le roba la upcall al driver de tiempo y al salir del
//! `share::scope` hace `unsubscribe`, dejando a `embassy_time::Timer` mudo para
//! siempre. Bajo `crate::run` esto se detecta y hace panic
//! ([`crate::time_driver::DetectSteal`]). Para dormir usar `Timer::after`.

use core::cell::Cell;

use libtock_platform::share;
use libtock_platform::{DefaultConfig, ErrorCode, Syscalls};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Hz(pub u32);

pub trait Convert {
    fn to_ticks(self, freq: Hz) -> Ticks;
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Ticks(pub u32);

impl Convert for Ticks {
    fn to_ticks(self, _freq: Hz) -> Ticks {
        self
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Milliseconds(pub u32);

impl Convert for Milliseconds {
    fn to_ticks(self, freq: Hz) -> Ticks {
        fn div_ceil(a: u32, other: u32) -> u32 {
            let d = a / other;
            let m = a % other;
            if m == 0 { d } else { d + 1 }
        }
        Ticks(div_ceil(self.0.saturating_mul(freq.0), 1000))
    }
}

pub struct Alarm;

impl Alarm {
    const DRIVER_NUM: u32 = 0x0;
    const SUBSCRIBE_CALLBACK: u32 = 0;
    const COMMAND_EXISTS: u32 = 0;
    const COMMAND_FREQUENCY: u32 = 1;
    const COMMAND_TIME: u32 = 2;
    const COMMAND_SET_RELATIVE: u32 = 5;

    #[inline(always)]
    pub fn exists() -> Result<(), ErrorCode> {
        crate::runtime::TockSyscalls::command(Self::DRIVER_NUM, Self::COMMAND_EXISTS, 0, 0)
            .to_result()
    }

    pub fn get_frequency() -> Result<Hz, ErrorCode> {
        crate::runtime::TockSyscalls::command(Self::DRIVER_NUM, Self::COMMAND_FREQUENCY, 0, 0)
            .to_result()
            .map(Hz)
    }

    pub fn get_ticks() -> Result<u32, ErrorCode> {
        crate::runtime::TockSyscalls::command(Self::DRIVER_NUM, Self::COMMAND_TIME, 0, 0)
            .to_result()
    }

    pub async fn sleep_for<T: Convert>(time: T) -> Result<(), ErrorCode> {
        let freq = Self::get_frequency()?;
        let ticks = time.to_ticks(freq);
        let called: Cell<Option<(u32, u32)>> = Cell::new(None);

        share::scope(|subscribe| {
            crate::runtime::TockSyscalls::subscribe::<
                _,
                _,
                DefaultConfig,
                { Self::DRIVER_NUM },
                { Self::SUBSCRIBE_CALLBACK },
            >(subscribe, &called)?;

            crate::runtime::TockSyscalls::command(
                Self::DRIVER_NUM,
                Self::COMMAND_SET_RELATIVE,
                ticks.0,
                0,
            )
            .to_result()
            .map(|_when: u32| ())?;

            loop {
                crate::runtime::TockSyscalls::yield_wait();
                if called.get().is_some() {
                    return Ok(());
                }
            }
        })
    }
}
