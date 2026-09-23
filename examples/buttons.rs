//! Botones y tiempo conviviendo en un solo proceso.
//!
//! Tres tareas concurrentes: dos esperando botones distintos, cada una con su LED,
//! y una tercera con `Timer::after`. Ninguna bloquea a las otras — es la
//! demostración de que el driver de tiempo y el de botones comparten el mismo
//! executor sin estorbarse.
//!

#![no_main]
#![no_std]

use core::fmt::Write;

use embassy_time::{Duration, Timer};
use libtock_embassy as libtock;
use libtock::buttons::{self, Button};
use libtock::console::Console;
use libtock::leds::Leds;
use libtock::runtime::{set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

/// Mantiene el LED encendido mientras el botón esté apretado.
#[embassy_executor::task(pool_size = 2)]
async fn watch(mut button: Button, led: u32) {
    let mut writer = Console::writer();
    let index = button.index();
    let _ = writeln!(writer, "boton {index} -> led {led}");

    loop {
        button.wait_for_press().await;
        let _ = Leds::on(led);
        let _ = writeln!(writer, "boton {index} presionado");

        button.wait_for_release().await;
        let _ = Leds::off(led);
        let _ = writeln!(writer, "boton {index} soltado");
    }
}

/// Corre en paralelo para dejar claro que esperar botones no congela el tiempo.
#[embassy_executor::task]
async fn heartbeat() {
    let mut writer = Console::writer();
    let mut tick: u32 = 0;
    loop {
        Timer::after(Duration::from_millis(1000)).await;
        tick = tick.wrapping_add(1);
        let _ = writeln!(writer, "tick {tick}");
    }
}

fn main() {
    // `buttons::init` va por fuera porque `run` ya abre el `share::scope` de la
    // alarma por dentro. Los dos quedan anidados y ninguno se cierra nunca.
    buttons::init(|| {
        libtock::run(|spawner| {
            for (index, led) in [(0u32, 0u32), (1, 1)] {
                match Button::take(index) {
                    Some(button) => spawner.spawn(watch(button, led).unwrap()),
                    None => {
                        let mut writer = Console::writer();
                        let _ = writeln!(writer, "boton {index} no disponible");
                    }
                }
            }
            spawner.spawn(heartbeat().unwrap());
        })
    })
}
