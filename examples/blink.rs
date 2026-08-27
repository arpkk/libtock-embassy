//! Dos tareas concurrentes sobre un solo proceso de Tock.
//!
//! Es la demo de lo que el driver de tiempo habilita: `blink` parpadea cada
//! 250 ms y `heartbeat` escribe cada 1000 ms, y los dos plazos conviven en la
//! única alarma que el kernel le da al proceso. Comparar con
//! `examples/blink_blocking.rs`, donde la segunda tarea directamente no avanza.

#![no_main]
#![no_std]

use core::fmt::Write;

use embassy_time::{Duration, Timer};
use libtock_embassy as libtock;
use libtock::console::Console;
use libtock::leds::Leds;
use libtock::runtime::{set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

/// Índice del LED que se reserva `heartbeat`. `blink` usa todos los demás, así
/// que la concurrencia se ve en la placa aunque no haya consola.
fn heartbeat_led() -> u32 {
    Leds::count().unwrap_or(1).saturating_sub(1)
}

#[embassy_executor::task]
async fn blink() {
    let mut writer = Console::writer();
    let leds = heartbeat_led().max(1);
    let _ = writeln!(writer, "blink: {leds} leds");

    let mut count: u32 = 0;
    loop {
        for led in 0..leds {
            let _ = if count & (1 << led) > 0 {
                Leds::on(led)
            } else {
                Leds::off(led)
            };
        }
        count = count.wrapping_add(1);
        Timer::after(Duration::from_millis(250)).await;
    }
}

#[embassy_executor::task]
async fn heartbeat() {
    let mut writer = Console::writer();
    let led = heartbeat_led();
    let mut tick: u32 = 0;
    loop {
        Timer::after(Duration::from_millis(1000)).await;
        tick = tick.wrapping_add(1);
        // El LED es la única señal observable en placas cuyo kernel no expone
        // consola: alterna cada 1000 ms, en paralelo al parpadeo de `blink`.
        let _ = if tick % 2 == 0 {
            Leds::on(led)
        } else {
            Leds::off(led)
        };
        // Ojo: `Console::write` es síncrono y adentro hace `yield_wait`, así que
        // esta línea puede correr la upcall de alarma desde dentro del `poll`.
        // Es sano (equivale a un interrupt en bare metal) pero bloquea a las
        // otras tareas hasta que la consola termina. Portar la consola a async
        // es parte del backlog.
        let _ = writeln!(writer, "heartbeat {tick}");
    }
}

fn main() {
    libtock::run(|spawner| {
        // En embassy-executor 0.10 el `Result` lo devuelve el token (el pool de
        // la tarea puede estar lleno), no el `spawn`.
        spawner.spawn(blink().unwrap());
        spawner.spawn(heartbeat().unwrap());
    })
}
