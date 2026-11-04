//! Lazo entre P1.15 y P1.14 un cable une la salida con la entrada.
//!
//! Una tarea alterna P1.15 cada 100 ms y otra espera los flancos en P1.14 y
//! alterna el LED 1 con cada uno. Cada 10 flancos se imprime cuántos se enviaron
//! y cuántos llegaron.

#![no_main]
#![no_std]

use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

use embassy_time::{Duration, Timer};
use libtock_embassy as libtock;
use libtock::console::Console;
use libtock::gpio::{self, Pin, Pull};
use libtock::leds::Leds;
use libtock::platform::{ErrorCode, Syscalls};
use libtock::runtime::{TockSyscalls, set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

const IN_PIN: u32 = 12;
const OUT_PIN: u32 = 13;

mod out_cmd {
    pub const ENABLE: u32 = 1;
    pub const SET: u32 = 2;
    pub const TOGGLE: u32 = 4;
}

static SENT: AtomicU32 = AtomicU32::new(0);
static RECEIVED: AtomicU32 = AtomicU32::new(0);

fn out(cmd: u32) {
    let _ = TockSyscalls::command(gpio::DRIVER_NUM, cmd, OUT_PIN, 0).to_result::<(), ErrorCode>();
}

/// Alterna P1.15 cada 100 ms.
#[embassy_executor::task]
async fn toggle() {
    // Arranca en alto, igual que el pull-up de la entrada.
    out(out_cmd::SET);
    out(out_cmd::ENABLE);
    loop {
        Timer::after(Duration::from_millis(100)).await;
        out(out_cmd::TOGGLE);
        SENT.fetch_add(1, Relaxed);
    }
}

/// Espera los flancos en P1.14.
#[embassy_executor::task]
async fn listen(mut pin: Pin) {
    let mut w = Console::writer();
    loop {
        let level = pin.wait_for_any_edge().await;
        let _ = Leds::toggle(0);
        let received = RECEIVED.fetch_add(1, Relaxed) + 1;
        if received % 10 == 0 {
            let _ = writeln!(
                w,
                "enviados={} recibidos={received} nivel={level:?}",
                SENT.load(Relaxed)
            );
        }
    }
}

fn main() {
    gpio::init(|| {
        libtock::run(|spawner| {
            let pin = Pin::take(IN_PIN, Pull::Up).expect("P1.14 no disponible");
            spawner.spawn(listen(pin).unwrap());
            spawner.spawn(toggle().unwrap());
        })
    })
}
