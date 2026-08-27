//! Línea base pre-driver-de-tiempo: el mismo blink pero durmiendo con
//! `alarm::Alarm::sleep_for`, que es `async` de firma y bloqueante por dentro.
//!
//! Sirve para dos cosas: comparar tamaño de binario y RAM contra
//! `examples/blink.rs`, y ver el problema en vivo. `idle_tick_task` se spawnea
//! igual que en `blink.rs` y con el mismo LED reservado, pero nunca corre: el
//! `loop { yield_wait() }` de `sleep_for` no le devuelve el control al executor,
//! así que ese LED se queda congelado. En `blink.rs` alterna cada 1000 ms.
//!
//! No llama a `libtock_embassy::run`, así que el driver de tiempo nunca se
//! suscribe y no hay conflicto por la upcall `(0, 0)`.

#![no_main]
#![no_std]

use core::fmt::Write;

use libtock_embassy as libtock;
use libtock::alarm::{Alarm, Milliseconds};
use libtock::console::{self, Console};
use libtock::executor::Executor;
use libtock::leds::Leds;
use libtock::runtime::{set_main, stack_size};
use static_cell::StaticCell;

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

static EXECUTOR: StaticCell<Executor> = StaticCell::new();

/// Mismo LED reservado que en `examples/blink.rs`, para comparar de a pares.
fn idle_led() -> u32 {
    Leds::count().unwrap_or(1).saturating_sub(1)
}

#[embassy_executor::task]
async fn blink_task() {
    let mut writer = Console::writer();
    let _ = writeln!(writer, "blink task start");

    let leds_count = match Leds::count() {
        Ok(count) => {
            let _ = writeln!(writer, "led count = {count}");
            count
        }
        Err(_) => {
            let _ = writeln!(writer, "led count failed, trying LED0");
            1
        }
    };

    // Deja el último LED para `idle_tick_task`, igual que en `blink.rs`.
    let active_leds = idle_led().max(1);
    let _ = leds_count;
    let mut count: u32 = 0;
    loop {
        if count % 4 == 0 {
            let _ = writeln!(writer, "blink tick {count}");
        }
        for led_index in 0..active_leds {
            if count & (1 << led_index) > 0 {
                if let Err(e) = Leds::on(led_index) {
                    let _ = writeln!(writer, "led on({led_index}) err: {e:?}");
                }
            } else if let Err(e) = Leds::off(led_index) {
                let _ = writeln!(writer, "led off({led_index}) err: {e:?}");
            }
        }

        Alarm::sleep_for(Milliseconds(250)).await.unwrap();
        count = count.wrapping_add(1);
    }
}

#[embassy_executor::task]
async fn idle_tick_task() {
    let led = idle_led();
    let mut tick: u32 = 0;
    loop {
        Alarm::sleep_for(Milliseconds(1000)).await.unwrap();
        tick = tick.wrapping_add(1);
        let _ = if tick % 2 == 0 {
            Leds::on(led)
        } else {
            Leds::off(led)
        };
        let _ = console::write_str_async("idle tick\n").await;
    }
}

fn main() {
    EXECUTOR.init(Executor::new()).run(|spawner| {
        spawner.spawn(blink_task().unwrap());
        // Se spawnea, pero no avanza nunca: `blink_task` se queda dentro del
        // primer `poll` y no le devuelve el control al executor.
        spawner.spawn(idle_tick_task().unwrap());
    })
}
