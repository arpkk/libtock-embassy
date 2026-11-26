// Prueba de presencia de usuario de OpenSK: espera un toque hasta 30 s, con parpadeo y keepalives.
// BUTTON2 de la DK simula la solicitud del navegador y BUTTON1 es el toque.
#![no_main]
#![no_std]

use core::fmt::Write;

use embassy_futures::select::select3;
use embassy_time::{Duration, Instant, Ticker, TimeoutError, Timer, with_timeout};
use libtock_embassy as libtock;
use libtock::buttons::{self, Button};
use libtock::console::Console;
use libtock::leds::Leds;
use libtock::runtime::{set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

const TOUCH_TIMEOUT: Duration = Duration::from_millis(30_000);
const KEEPALIVE_DELAY: Duration = Duration::from_millis(100);
const DEBOUNCE: Duration = Duration::from_millis(20);

async fn blink() -> ! {
    let mut t = Ticker::every(Duration::from_millis(500));
    loop {
        let _ = Leds::toggle(0);
        t.next().await;
    }
}

async fn keepalive() -> ! {
    let mut t = Ticker::every(KEEPALIVE_DELAY);
    loop {
        let _ = Console::write(b"KEEPALIVE UP_NEEDED\n");
        t.next().await;
    }
}

// espera a que el botón quede suelto, sin la pausa un rebote al soltar se lee como presión
async fn released(b: &mut Button) {
    b.wait_for_release().await;
    Timer::after(DEBOUNCE).await;
}

async fn check_user_presence(touch: &mut Button) -> Result<(), TimeoutError> {
    let result = with_timeout(TOUCH_TIMEOUT, async {
        let touched = async {
            released(touch).await;
            touch.wait_for_press().await;
        };
        let _ = select3(touched, blink(), keepalive()).await;
    })
    .await;
    let _ = Leds::off(0);
    result
}

#[embassy_executor::task]
async fn requests(mut request: Button, mut touch: Button) {
    let mut w = Console::writer();
    let mut n: u32 = 0;
    loop {
        request.wait_for_press().await;
        n += 1;
        let _ = writeln!(w, "solicitud {n}: esperando toque");

        let start = Instant::now();
        let result = check_user_presence(&mut touch).await;
        let ms = start.elapsed().as_millis();
        match result {
            Ok(()) => {
                let _ = writeln!(w, "solicitud {n}: toque a los {ms} ms");
            }
            Err(TimeoutError) => {
                let _ = writeln!(w, "solicitud {n}: sin toque, se rinde a los {ms} ms");
            }
        }

        // sin esto mantener el botón presionado haria otra solicitud
        released(&mut request).await;
    }
}

#[embassy_executor::task]
async fn heartbeat() {
    let mut w = Console::writer();
    let mut t = Ticker::every(Duration::from_secs(1));
    let mut n: u32 = 0;
    loop {
        t.next().await;
        n += 1;
        let _ = writeln!(w, "heartbeat {n}");
    }
}

fn main() {
    buttons::init(|| {
        libtock::run(|spawner| {
            let touch = Button::take(0).unwrap();
            let request = Button::take(1).unwrap();
            spawner.spawn(requests(request, touch).unwrap());
            spawner.spawn(heartbeat().unwrap());
        })
    })
}