// Misma estructura que 3_tormenta_timers con menos carga; las filas CSV cada ROW_MS reemplazan a chatter como carga de consola
#![no_main]
#![no_std]

use core::cell::RefCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

use critical_section::Mutex;
use embassy_futures::yield_now;
use embassy_time::{Duration, Instant, Timer};
use libtock_embassy as libtock;
use libtock::console::Console;
use libtock::runtime::{set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

const PERIODS_US: [u64; 8] = [
    2300, 2900, 3700, 4100, 5300, 7100, 11000, 130_000,
];
const N: usize = PERIODS_US.len();
const ROW_MS: u64 = 40;

static TOTAL: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
static LATE_MAX_US: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
static LATE_SUM_US: Mutex<RefCell<[u64; N]>> = Mutex::new(RefCell::new([0; N]));
static PASSES: AtomicU32 = AtomicU32::new(0);

#[embassy_executor::task(pool_size = N)]
async fn worker(i: usize) {
    let period = Duration::from_micros(PERIODS_US[i]);
    let mut next = Instant::now();
    loop {
        next += period;
        Timer::at(next).await;
        let late = Instant::now().saturating_duration_since(next).as_micros();
        LATE_MAX_US[i].fetch_max(late.min(u32::MAX as u64) as u32, Relaxed);
        critical_section::with(|cs| LATE_SUM_US.borrow_ref_mut(cs)[i] += late);
        TOTAL[i].fetch_add(1, Relaxed);
    }
}

#[embassy_executor::task]
async fn spin() {
    loop {
        PASSES.fetch_add(1, Relaxed);
        yield_now().await;
    }
}

#[embassy_executor::task]
async fn report() {
    let mut w = Console::writer();
    let _ = writeln!(
        w,
        "t_ms,worker,period_us,n_total,expected_total,late_max_us,late_avg_us,passes_total"
    );
    let start = Instant::now();
    let mut seen = [0u32; N];
    let mut passes: u64 = 0;
    let mut k: usize = 0;
    loop {
        Timer::after(Duration::from_millis(ROW_MS)).await;
        let i = k % N;
        k += 1;
        let t_us = start.elapsed().as_micros();
        passes += PASSES.swap(0, Relaxed) as u64;
        let total = TOTAL[i].load(Relaxed);
        let n = total - seen[i];
        seen[i] = total;
        let sum = critical_section::with(|cs| core::mem::take(&mut LATE_SUM_US.borrow_ref_mut(cs)[i]));
        let avg = if n == 0 { 0 } else { sum / n as u64 };
        let _ = writeln!(
            w,
            "{},{i},{},{total},{},{},{avg},{passes}",
            t_us / 1000,
            PERIODS_US[i],
            t_us / PERIODS_US[i],
            LATE_MAX_US[i].swap(0, Relaxed),
        );
    }
}

fn main() {
    libtock::run(|spawner| {
        for i in 0..N {
            spawner.spawn(worker(i).unwrap());
        }
        spawner.spawn(spin().unwrap());
        spawner.spawn(report().unwrap());
    })
}
