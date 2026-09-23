#![no_main]
#![no_std]

use embassy_time::{Duration, Timer};
use libtock_embassy as libtock;
use libtock::leds::Leds;
use libtock::runtime::{set_main, stack_size};

extern crate libtock_debug_panic; //obtiene un panic handler de libtock

set_main! {main}
stack_size! {0x1000}

const BASE_MS: u64 = 1000;

#[embassy_executor::task(pool_size = 4)] //4 tareas
async fn blink(led: u32, tiempo: u64) {
    let mut on = false;
    loop {
        Timer::after(Duration::from_millis(tiempo)).await;
        on = !on;
        if on { 
            Leds::on(led) 
        } 
        else { 
            Leds::off(led) 
        };
    }
}

fn main() {
    libtock::run(|spawner| {
        let leds = Leds::count().unwrap_or(1).min(4); //4 leds
        for led in 0..leds {
            let tiempo = BASE_MS << led;
            spawner.spawn(blink(led, tiempo).unwrap());
        }
    })
}
