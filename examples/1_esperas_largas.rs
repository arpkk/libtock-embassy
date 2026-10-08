//2 esperas: tick 0 y now(), 8 esperas: 0 ticks, 1 tick, 1ms, 3s, MAX_DT, 130s, 300s y 600s 
#![no_main]
#![no_std]

use core::fmt::Write;

use embassy_time::{Duration, Instant, Timer};
use libtock_embassy as libtock;
use libtock::console::Console;
use libtock::runtime::{set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

#[embassy_executor::task]
async fn past() {
    let mut w = Console::writer();
    let t0 = Instant::now();
    Timer::at(Instant::from_ticks(0)).await;
    let _ = writeln!(w, "at(0): {}us", t0.elapsed().as_micros());
    let t0 = Instant::now();
    Timer::at(t0).await;
    let _ = writeln!(w, "at(now): {}us", t0.elapsed().as_micros());
}

/*
pedido: Duración que se le pasa a Timer::after
medido: Tiempo real que pasa segun Instant::now() entre el momento de llamar a Timer::after y el momento en que se despierta la tarea
retraso: medido - pedido
*/
#[embassy_executor::task]
async fn cases() {
    let mut w = Console::writer();
    let cases: [(&str, Duration); 8] = [
        ("0 ticks", Duration::from_ticks(0)),
        ("1 tick", Duration::from_ticks(1)),
        ("1 ms", Duration::from_millis(1)),
        ("3 s", Duration::from_secs(3)),
        ("2^30 ticks", Duration::from_ticks(1 << 30)),
        ("130 s", Duration::from_secs(130)),
        ("300 s", Duration::from_secs(300)),
        ("600 s", Duration::from_secs(600)),
    ];
    for (label, d) in cases {
        let start = Instant::now();
        let _ = writeln!(w, "[{}s] {label}: empieza", start.as_secs());
        Timer::after(d).await;
        let el = start.elapsed();
        let tag = if el < d { "ADELANTADO" } else { "ok" };
        let _ = writeln!(
            w,
            "[{}s] {label}: pedido={}us medido={}us retraso={}us {tag}",
            Instant::now().as_secs(),
            d.as_micros(),
            el.as_micros(),
            el.as_micros().saturating_sub(d.as_micros()),
        );
    }
    let _ = writeln!(w, "fin");
}

fn main() {
    libtock::run(|spawner| {
        spawner.spawn(past().unwrap());
        spawner.spawn(cases().unwrap());
    })
}

/* Resultados:
[0s] 0 ticks: empieza
at(0): 457us
[0s] 0 ticks: pedido=0us medido=17364us retraso=17364us ok
[0s] 1 tick: empieza
[0s] 1 tick: pedido=0us medido=4425us retraso=4425us ok
[0s] 1 ms: empieza
at(now): 30853us
[0s] 1 ms: pedido=1000us medido=7263us retraso=6263us ok
[0s] 3 s: empieza
tock$ [3s] 3 s: pedido=3000000us medido=3004272us retraso=4272us ok
[3s] 2^30 ticks: empieza
[131s] 2^30 ticks: pedido=128000000us medido=128004882us retraso=4882us ok
[131s] 130 s: empieza
[261s] 130 s: pedido=130000000us medido=130004638us retraso=4638us ok
[261s] 300 s: empieza
[561s] 300 s: pedido=300000000us medido=300004638us retraso=4638us ok
[561s] 600 s: empieza
[1161s] 600 s: pedido=600000000us medido=600004638us retraso=4638us ok
fin
 */