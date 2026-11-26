//! Onda cuadrada en P1.15 para medir la precisión de los timers con el analizador lógico.
//!
//! Una sola tarea alterna P1.15 con `Timer::at`, en etapas cada vez más rápidas
//! separadas por pausas de 500 ms. Los períodos son múltiplos enteros del tick del
//! RTC (32 768 Hz), así que una variación en el ancho de los pulsos viene del
//! software y no del redondeo del reloj. Durante la medición no se usa la consola:
//! el atraso de cada etapa se guarda en RAM y la tabla se imprime al final.
//!
//! El analizador va con su primer canal en P1.15 y su GND al GND de la placa.

#![no_main]
#![no_std]

use core::fmt::Write;

use embassy_time::{Duration, Instant, TICK_HZ, Timer};
use libtock_embassy as libtock;
use libtock::console::Console;
use libtock::gpio;
use libtock::platform::{ErrorCode, Syscalls};
use libtock::runtime::{TockSyscalls, set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

const OUT_PIN: u32 = 13;
const RTC_HZ: u64 = 32_768;
const TICKS_PER_RTC: u64 = TICK_HZ / RTC_HZ;
const _: () = assert!(TICK_HZ.is_multiple_of(RTC_HZ));
/// Períodos en ticks del RTC: ~100, 20, 10, 5, 2 y 1 ms.
const STAGES_RTC: [u64; 6] = [3277, 655, 328, 164, 66, 33];
const N: usize = STAGES_RTC.len();
const EDGES_PER_STAGE: u64 = 200;
const START_MS: u64 = 2000;
const PAUSE_MS: u64 = 500;

mod out_cmd {
    pub const ENABLE: u32 = 1;
    pub const CLEAR: u32 = 3;
    pub const TOGGLE: u32 = 4;
}

/// Atraso acumulado de una etapa, en ticks de `embassy_time`.
#[derive(Clone, Copy)]
struct Stage {
    late_max: u64,
    late_sum: u64,
}

fn out(cmd: u32) {
    let _ = TockSyscalls::command(gpio::DRIVER_NUM, cmd, OUT_PIN, 0).to_result::<(), ErrorCode>();
}

fn to_us(ticks: u64) -> u64 {
    ticks * 1_000_000 / TICK_HZ
}

/// Alterna P1.15 en cada etapa y al final imprime la tabla de atrasos.
#[embassy_executor::task]
async fn generator() {
    out(out_cmd::ENABLE);
    out(out_cmd::CLEAR);
    Timer::after(Duration::from_millis(START_MS)).await;

    let mut stages = [Stage { late_max: 0, late_sum: 0 }; N];
    for (stage, rtc) in stages.iter_mut().zip(STAGES_RTC) {
        let period = Duration::from_ticks(rtc * TICKS_PER_RTC);
        let mut next = Instant::now();
        for _ in 0..EDGES_PER_STAGE {
            next += period;
            Timer::at(next).await;
            out(out_cmd::TOGGLE);
            let late = Instant::now().saturating_duration_since(next).as_ticks();
            stage.late_max = stage.late_max.max(late);
            stage.late_sum += late;
        }
        Timer::after(Duration::from_millis(PAUSE_MS)).await;
    }
    out(out_cmd::CLEAR);

    let mut w = Console::writer();
    let _ = writeln!(w, "period_rtc,period_us,edges,late_max_us,late_avg_us");
    for (stage, rtc) in stages.iter().zip(STAGES_RTC) {
        let _ = writeln!(
            w,
            "{rtc},{},{EDGES_PER_STAGE},{},{}",
            rtc * 1_000_000 / RTC_HZ,
            to_us(stage.late_max),
            to_us(stage.late_sum / EDGES_PER_STAGE),
        );
    }
    let _ = writeln!(w, "# fin");
}

fn main() {
    libtock::run(|spawner| {
        spawner.spawn(generator().unwrap());
    })
}