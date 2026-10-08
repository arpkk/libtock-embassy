// Alarm::sleep_for dentro de run()
#![no_main]
#![no_std]

use core::fmt::Write;

use embassy_time::{Duration, Timer};
use libtock_embassy as libtock;
use libtock::alarm::{Alarm, Milliseconds};
use libtock::console::Console;
use libtock::runtime::{set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

// Despierta cada 500ms e imprime, si deja de imprimir el driver murió
#[embassy_executor::task]
async fn heartbeat() {
    let mut w = Console::writer();
    let mut n: u32 = 0;
    loop {
        Timer::after(Duration::from_millis(500)).await;
        n += 1;
        let _ = writeln!(w, "tick {n}");
    }
}

//Provoca la falla del driver de alarmas, porque el kernel no puede atender la interrupción a tiempo
#[embassy_executor::task]
async fn intruder() {
    let mut w = Console::writer();
    Timer::after(Duration::from_secs(3)).await;
    let _ = writeln!(w, "sleep_for empieza");
    let r = Alarm::sleep_for(Milliseconds(200)).await;
    let _ = writeln!(w, "sleep_for volvio: {r:?}");
    Timer::after(Duration::from_secs(1)).await;
    let _ = writeln!(w, "intruder: timer despues de sleep_for ok");
}

fn main() {
    libtock::run(|spawner| {
        spawner.spawn(heartbeat().unwrap());
        spawner.spawn(intruder().unwrap());
    })
}

/*
Resultados:
tock$ tick 1
tick 2
tick 3
tick 4
tick 5
sleep_for empieza
sleep_for volvio: Ok(())
tick 6

el driver muere pero en silencio
*/