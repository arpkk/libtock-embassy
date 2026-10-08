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

#[cfg(debug_assertions)]
extern crate libtock_debug_panic;
// #[cfg(not(debug_assertions))]
// extern crate libtock_small_panic;

pub use libtock_platform as platform;
pub use libtock_runtime as runtime;

// El runtime async se reexporta tal cual: las apps escriben contra la API de
// Embassy de siempre (`Timer::after`, `#[embassy_executor::task]`), no contra
// una API paralela.
pub use embassy_executor;
pub use embassy_futures;
pub use embassy_time;

/// Tipo vacío: `Never` en posición de retorno obliga a que una función no vuelva.
///
/// Es `!` estable, que todavía no se puede escribir en un bound `FnOnce() -> !`.
/// Lo usan las capas de `init` que abren un `share::scope` permanente: al exigir
/// que el closure no retorne, el tipo garantiza que el scope nunca se cierre y
/// que el `unsubscribe` de `Subscribe::drop` no corra jamás.
pub enum Never {}

pub mod alarm;
pub mod buttons;
pub mod console;
pub mod executor;
pub mod gpio;
pub mod leds;
mod source;
pub mod task;
pub mod time_driver;

pub use executor::run;

// pub mod adc {
//     use libtock_adc as adc;
//     pub type Adc = adc::Adc<super::runtime::TockSyscalls>;
//     pub use adc::ADCListener;
// }
// pub mod display {
//     use libtock_screen as screen;
//     pub type Screen = screen::Screen<super::runtime::TockSyscalls>;
// }
// pub mod air_quality {
//     use libtock_air_quality as air_quality;
//     pub type AirQuality = air_quality::AirQuality<super::runtime::TockSyscalls>;
//     pub use air_quality::AirQualityListener;
// }

// pub mod ambient_light {
//     use libtock_ambient_light as ambient_light;
//     pub type AmbientLight = ambient_light::AmbientLight<super::runtime::TockSyscalls>;
//     pub use ambient_light::IntensityListener;
// }
// pub mod buzzer {
//     use libtock_buzzer as buzzer;
//     pub type Buzzer = buzzer::Buzzer<super::runtime::TockSyscalls>;
//     pub use buzzer::Note;
// }
// pub mod i2c_master {
//     use libtock_i2c_master as i2c_master;
//     pub type I2CMaster = i2c_master::I2CMaster<super::runtime::TockSyscalls>;
// }
// pub mod i2c_master_slave {
//     use libtock_i2c_master_slave as i2c_master_slave;
//     pub type I2CMasterSlave = i2c_master_slave::I2CMasterSlave<super::runtime::TockSyscalls>;
// }
// pub mod ieee802154 {
//     use libtock_ieee802154 as ieee802154;
//     pub type Ieee802154 = ieee802154::Ieee802154<super::runtime::TockSyscalls>;
//     pub use ieee802154::{Frame, RxOperator, RxRingBuffer};
//     pub type RxSingleBufferOperator<'buf, const N: usize> =
//         ieee802154::RxSingleBufferOperator<'buf, N, super::runtime::TockSyscalls>;
// }
// pub mod low_level_debug {
//     use libtock_low_level_debug as lldb;
//     pub type LowLevelDebug = lldb::LowLevelDebug<super::runtime::TockSyscalls>;
//     pub use lldb::AlertCode;
// }
// pub mod ninedof {
//     use libtock_ninedof as ninedof;
//     pub type NineDof = ninedof::NineDof<super::runtime::TockSyscalls>;
//     pub use ninedof::NineDofListener;
// }
// pub mod proximity {
//     use libtock_proximity as proximity;
//     pub type Proximity = proximity::Proximity<super::runtime::TockSyscalls>;
// }
// pub mod rng {
//     use libtock_rng as rng;
//     pub type Rng = rng::Rng<super::runtime::TockSyscalls>;
//     pub use rng::RngListener;
// }
// pub mod sound_pressure {
//     use libtock_sound_pressure as sound_pressure;
//     pub type SoundPressure = sound_pressure::SoundPressure<super::runtime::TockSyscalls>;
// }
// #[cfg(feature = "rust_embedded")]
// pub mod spi_controller;
// #[cfg(not(feature = "rust_embedded"))]
// pub mod spi_controller {
//     use libtock_spi_controller as spi_controller;
//     pub type SpiController = spi_controller::SpiController<super::runtime::TockSyscalls>;
// }
// pub mod temperature {
//     use libtock_temperature as temperature;
//     pub type Temperature = temperature::Temperature<super::runtime::TockSyscalls>;
//     pub use temperature::TemperatureListener;
// }
// pub mod key_value {
//     use libtock_key_value as key_value;
//     pub type KeyValue = key_value::KeyValue<super::runtime::TockSyscalls>;
// }
// 