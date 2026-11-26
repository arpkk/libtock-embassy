// `deny` y no `forbid`: el crate es seguro salvo por tres puntos que la
// integración con Embassy obliga y que quedan confinados a `executor` y
// `time_driver` con un `#[allow(unsafe_code)]` explícito arriba de cada archivo
// (`__pender`, `time_driver_impl!` y `raw::Executor::poll`).
#![deny(unsafe_code)]
#![no_std]

// `embassy-executor` y el driver de tiempo piden una implementación de
// `critical-section`. Se linkea la de `cortex-m` solo por su símbolo, así que
// hay que referenciar el crate a mano o el linker no la trae.
//
// Ojo con lo que significa acá: un proceso de Tock corre sin privilegios, y en
// ARMv7-M `cpsid i` es un NOP en modo no privilegiado. O sea la sección crítica
// termina siendo un no-op. Igual es correcta, porque un proceso de Tock es de un
// solo hilo y el kernel "invokes upcalls only in response to Yield system calls"
// (TRD104, sección 4.1): nada puede reentrar mientras no hagamos un yield, y
// ninguna sección crítica de este crate hace yield adentro.
//
// Lo que NO sirve es portar esto tal cual a RISC-V: ahí la implementación
// equivalente escribe `mstatus`, que en U-mode sí lanza un trap. Antes de
// `qemu_rv32_virt` hay que escribir una implementación propia de
// `critical-section` para userspace de Tock.
use cortex_m as _;

pub use libtock_platform as platform;
pub use libtock_runtime as runtime;

// El runtime async se reexporta tal cual: las apps escriben contra la API de
// Embassy de siempre (`Timer::after`, `#[embassy_executor::task]`), no contra
// una API paralela.
pub use embassy_executor;
pub use embassy_futures;
#[cfg(feature = "embassy-time")]
pub use embassy_time;

#[cfg(not(any(feature = "embassy-time", feature = "relative-timer")))]
compile_error!("hace falta la feature `embassy-time` (por defecto) o `relative-timer`");

/// Tipo vacío: `Never` en posición de retorno obliga a que una función no vuelva.
///
/// Es `!` estable, que todavía no se puede escribir en un bound `FnOnce() -> !`.
/// Lo usan las capas de `init` que abren un `share::scope` permanente: al exigir
/// que el closure no retorne, el tipo garantiza que el scope nunca se cierre y
/// que el `unsubscribe` de `Subscribe::drop` no corra jamás.
pub enum Never {}

pub mod buttons;
pub mod console;
pub mod executor;
pub mod gpio;
pub mod leds;
mod source;
#[cfg(feature = "stats")]
pub mod stats;
#[cfg(all(feature = "embassy-time", not(feature = "relative-timer")))]
pub mod time_driver;
#[cfg(feature = "relative-timer")]
pub mod timer;

pub use executor::run;
