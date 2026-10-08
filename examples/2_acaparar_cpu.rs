// Una tarea ocupa la CPU
#![no_main]
#![no_std]

use core::fmt::Write;

use embassy_time::{Duration, Instant, TICK_HZ, Timer};
use libtock_embassy as libtock;
use libtock::alarm::Alarm;
use libtock::console::Console;
use libtock::runtime::{set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

// Tarea que imprime un tick cada segundo
#[embassy_executor::task]
async fn heartbeat() {
    let mut w = Console::writer();
    let mut n: u32 = 0;
    loop {
        Timer::after(Duration::from_secs(1)).await;
        n += 1;
        let _ = writeln!(w, "tick {n} now={}s", Instant::now().as_secs());
    }
}

fn busy(secs: u64) -> u64 {
    let target = secs * TICK_HZ;
    let mut last = Alarm::get_ticks().unwrap();
    let mut real: u64 = 0;
    while real < target {
        let raw = Alarm::get_ticks().unwrap();
        real += raw.wrapping_sub(last) as u64;
        last = raw;
    }
    real
}

// Tarea que ocupa la CPU durante un tiempo determinado
#[embassy_executor::task]
async fn hog() {
    let mut w = Console::writer();
    for secs in [5u64, 200, 540] {
        Timer::after(Duration::from_secs(3)).await;
        let _ = writeln!(w, "hog {secs}s empieza");
        let before = Instant::now();
        let real = busy(secs);
        let seen = Instant::now().saturating_duration_since(before);
        let _ = writeln!(
            w,
            "hog {secs}s: real={}ms now()={}ms",
            real * 1000 / TICK_HZ,
            seen.as_millis(),
        );
    }
}

fn main() {
    libtock::run(|spawner| {
        spawner.spawn(heartbeat().unwrap());
        spawner.spawn(hog().unwrap());
    })
}

/*
Resultados:
tock$ tick 1 now=1s
tick 2 now=2s
hog 5s empieza
hog 5s: real=5000ms now()=5000ms
tick 3 now=8s
tick 4 now=9s
tick 5 now=10s
hog 200s empieza
hog 200s: real=200000ms now()=200000ms
tick 6 now=211s
tick 7 now=212s
tick 8 now=213s
hog 540s empieza
hog 540s: real=540000ms now()=28000ms
tick 9 now=242s
tick 10 now=243s
tick 11 now=244s
tick 12 now=245s
tick 13 now=246s

El reloj perdio una vuelta entera, el driver debe leer el contador al menos una vez cada 512s.
*/