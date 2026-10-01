//! GPIO, botones y tiempo conviviendo en un solo proceso.
//!
//! Cuatro tareas concurrentes:
//!
//! - pin 0: cuenta flancos de bajada y alterna LED1 en cada uno;
//! - pin 1: LED2 copia el nivel del pin en cada flanco (encendido mientras está a GND);
//! - botón 0: LED3 encendido mientras esté apretado;
//! - un heartbeat con `Timer::after` por consola.
//!
//! En la nRF52840-DK los índices 0 y 1 son P1.01 y P1.02 del header. Están con
//! pull-up, así que para generar flancos basta tocarlos con un cable a GND. Los
//! botones de la placa no sirven para esto: no están en la tabla que exporta la
//! cápsula de GPIO. Con un cable a mano hay rebote, así que un solo toque puede
//! contar varios flancos; es esperable.

#![no_main]
#![no_std]

use core::fmt::Write;

use embassy_time::{Duration, Timer};
use libtock_embassy as libtock;
use libtock::buttons::{self, Button};
use libtock::console::Console;
use libtock::gpio::{self, Level, Pin, Pull};
use libtock::leds::Leds;
use libtock::runtime::{set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

/// Alterna el LED en cada flanco de bajada y lleva la cuenta.
#[embassy_executor::task]
async fn count_falling(mut pin: Pin, led: u32) {
    let mut writer = Console::writer();
    let mut count: u32 = 0;
    loop {
        pin.wait_for_falling_edge().await;
        count = count.wrapping_add(1);
        let _ = Leds::toggle(led);
        let _ = writeln!(writer, "pin {}: bajada #{count}", pin.index());
    }
}

/// El LED copia el nivel muestreado en cada flanco. El pin tiene pull-up, así que
/// nivel bajo significa "tocado a GND" y enciende el LED.
#[embassy_executor::task]
async fn mirror(mut pin: Pin, led: u32) {
    let mut writer = Console::writer();
    loop {
        let level = pin.wait_for_any_edge().await;
        let _ = match level {
            Level::Low => Leds::on(led),
            Level::High => Leds::off(led),
        };
        let _ = writeln!(writer, "pin {}: {level:?}", pin.index());
    }
}

/// LED encendido mientras el botón esté apretado.
#[embassy_executor::task]
async fn hold(mut button: Button, led: u32) {
    loop {
        button.wait_for_press().await;
        let _ = Leds::on(led);
        button.wait_for_release().await;
        let _ = Leds::off(led);
    }
}

/// Corre en paralelo para dejar claro que esperar flancos no congela el tiempo.
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
    // Tres capas de suscripción permanente. El orden entre `gpio` y `buttons` da
    // igual; `run` va al fondo porque abre la de alarma y es el que nunca vuelve.
    gpio::init(|| {
        buttons::init(|| {
            libtock::run(|spawner| {
                let mut writer = Console::writer();

                match Pin::take(0, Pull::Up) {
                    Some(pin) => spawner.spawn(count_falling(pin, 0).unwrap()),
                    None => {
                        let _ = writeln!(writer, "pin 0 no disponible");
                    }
                }
                match Pin::take(1, Pull::Up) {
                    Some(pin) => spawner.spawn(mirror(pin, 1).unwrap()),
                    None => {
                        let _ = writeln!(writer, "pin 1 no disponible");
                    }
                }
                match Button::take(0) {
                    Some(button) => spawner.spawn(hold(button, 2).unwrap()),
                    None => {
                        let _ = writeln!(writer, "boton 0 no disponible");
                    }
                }
                spawner.spawn(heartbeat().unwrap());
            })
        })
    })
}
