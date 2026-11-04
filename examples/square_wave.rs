//! Onda cuadrada en P1.15 para ver con el analizador lógico.
//!
//! P1.15 es salida y una tarea la alterna con `Timer::at`, en etapas
//! cada vez más rápidas. Por cada etapa se imprime el período pedido y el peor
//! atraso de los cambios según el reloj de la placa, para compararlo con lo que
//! mide el analizador.
//!
//! El analizador va con su primer canal en P1.15 y su GND al GND de la placa.

#![no_main]
#![no_std]

use core::fmt::Write;

use embassy_time::{Duration, Instant, Timer};
use libtock_embassy as libtock;
use libtock::console::Console;
use libtock::gpio;
use libtock::platform::{ErrorCode, Syscalls};
use libtock::runtime::{TockSyscalls, set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

const OUT_PIN: u32 = 13;
const STAGES_MS: [u64; 6] = [100, 20, 10, 5, 2, 1];
const EDGES_PER_STAGE: u32 = 200;

mod out_cmd {
    pub const ENABLE: u32 = 1;
    pub const CLEAR: u32 = 3;
    pub const TOGGLE: u32 = 4;
}

fn out(cmd: u32) {
    let _ = TockSyscalls::command(gpio::DRIVER_NUM, cmd, OUT_PIN, 0).to_result::<(), ErrorCode>();
}

/// Alterna P1.15 con período fijo, en etapas cada vez más rápidas.
#[embassy_executor::task]
async fn generator() {
    let mut w = Console::writer();
    out(out_cmd::ENABLE);
    out(out_cmd::CLEAR);
    Timer::after(Duration::from_millis(500)).await;

    for period_ms in STAGES_MS {
        let _ = writeln!(w, "{period_ms} ms: empieza");
        let period = Duration::from_millis(period_ms);
        let mut next = Instant::now();
        let mut late_max = Duration::from_ticks(0);
        for _ in 0..EDGES_PER_STAGE {
            next += period;
            Timer::at(next).await;
            out(out_cmd::TOGGLE);
            late_max = late_max.max(Instant::now().saturating_duration_since(next));
        }
        let _ = writeln!(
            w,
            "{period_ms} ms: {EDGES_PER_STAGE} cambios, atraso max={}us",
            late_max.as_micros()
        );
        Timer::after(Duration::from_millis(500)).await;
    }
    out(out_cmd::CLEAR);
    let _ = writeln!(w, "fin");
}

fn main() {
    libtock::run(|spawner| {
        spawner.spawn(generator().unwrap());
    })
}

/*
tock$ 100 ms: empieza
100 ms: 200 cambios, atraso max=488us
20 ms: empieza
20 ms: 200 cambios, atraso max=487us
10 ms: empieza
10 ms: 200 cambios, atraso max=488us
5 ms: empieza
5 ms: 200 cambios, atraso max=487us
2 ms: empieza
2 ms: 200 cambios, atraso max=488us
1 ms: empieza
1 ms: 200 cambios, atraso max=488us
fin
*/