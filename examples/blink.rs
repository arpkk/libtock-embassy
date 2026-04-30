//! The blink app.

#![no_main]
#![no_std]

extern crate libtock_debug_panic;
extern crate cortex_m;

use core::sync::atomic::{AtomicBool, Ordering};
use core::fmt::Write;

use embassy_executor::{raw::Executor, Spawner};
use libtock_embassy as libtock;
use libtock::alarm::{Alarm, Milliseconds};
use libtock::console;
use libtock::console::Console;
use libtock::leds::Leds;
use libtock::platform::Syscalls;
use libtock::runtime::{set_main, stack_size};
use static_cell::StaticCell;

set_main! {main}
stack_size! {0x1000}

static EXECUTOR: StaticCell<Executor> = StaticCell::new();
static PENDED: AtomicBool = AtomicBool::new(false);

#[unsafe(export_name = "__pender")]
fn pender(_context: *mut ()) {
    PENDED.store(true, Ordering::Release);
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

    let active_leds = if leds_count == 0 { 1 } else { leds_count };
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
            } else {
                if let Err(e) = Leds::off(led_index) {
                    let _ = writeln!(writer, "led off({led_index}) err: {e:?}");
                }
            }
        }

        Alarm::sleep_for(Milliseconds(250)).await.unwrap();
        count = count.wrapping_add(1);
    }
}

#[embassy_executor::task]
async fn idle_tick_task() {
    let mut tick: u32 = 0;
    loop {
        Alarm::sleep_for(Milliseconds(1000)).await.unwrap();
        tick = tick.wrapping_add(1);
        let _ = tick;
        console::write_str_async("idle tick\n").await.unwrap();
    }
}

fn spawn_tasks(spawner: Spawner) {
    spawner.spawn(blink_task()).unwrap();
    // spawner.spawn(idle_tick_task()).unwrap();
}

fn main() {
    let executor = EXECUTOR.init(Executor::new(core::ptr::null_mut()));
    spawn_tasks(executor.spawner());

    loop {
        unsafe { executor.poll() };
        if !PENDED.swap(false, Ordering::AcqRel) {
            // If no embassy tasks are ready, sleep in kernel until an upcall arrives.
            libtock::runtime::TockSyscalls::yield_wait();
        }
    }
}
