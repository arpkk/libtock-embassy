// Igual que 6_tormenta_lenta, con timers relativos de 32 bits: mismo CSV para comparar
#![no_main]
#![no_std]

use core::cell::RefCell;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

use critical_section::Mutex;
use embassy_futures::yield_now;
use libtock_embassy as libtock;
use libtock::console::Console;
use libtock::platform::Syscalls;
use libtock::runtime::{TockSyscalls, set_main, stack_size};
use libtock::timer::{self, Timer, stats, ticks_from_micros, ticks_from_millis};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

const BASE_PERIODS_US: [u64; 8] = [2300, 2900, 3700, 4100, 5300, 7100, 11000, 130_000];
const N: usize = BASE_PERIODS_US.len();
const SCALE_PCT: u64 = 50;
const LOAD_LINE: &str = "# ..............................................................";
const LOAD_MS: u32 = 40;
const REPORT_MS: u32 = 1000;
const DURATION_S: u32 = 60;

static TOTAL: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
static LATE_MAX_US: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
static LATE_SUM_US: Mutex<RefCell<[u64; N]>> = Mutex::new(RefCell::new([0; N]));
static PASSES: AtomicU32 = AtomicU32::new(0);

const fn period_us(i: usize) -> u64 {
    BASE_PERIODS_US[i] * SCALE_PCT / 100
}

fn micros(ticks: u64) -> u64 {
    ticks * 1_000_000 / timer::frequency() as u64
}

struct Line {
    buf: [u8; 160],
    len: usize,
}

impl Line {
    const fn new() -> Self {
        Self { buf: [0; 160], len: 0 }
    }

    fn send(&mut self) {
        let _ = self.write_str("\n");
        let _ = Console::write(&self.buf[..self.len]);
        self.len = 0;
    }
}

impl Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        if end > self.buf.len() {
            return Err(fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

#[embassy_executor::task(pool_size = N)]
async fn worker(i: usize) {
    let period = ticks_from_micros(period_us(i) as u32);
    let mut reference = timer::now();
    loop {
        Timer::new(reference, period).await;
        reference = reference.wrapping_add(period);
        let late = micros(timer::now().wrapping_sub(reference) as u64);
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
async fn load() {
    let mut line = Line::new();
    loop {
        Timer::after(ticks_from_millis(LOAD_MS)).await;
        let _ = line.write_str(LOAD_LINE);
        line.send();
    }
}

#[embassy_executor::task]
async fn report() {
    let mut line = Line::new();
    let _ = line.write_str(
        "t_ms,worker,period_us,n,expected,late_max_us,late_avg_us,passes,cmd_time,cmd_set,upcalls,skipped",
    );
    line.send();
    let report = ticks_from_millis(REPORT_MS);
    let mut reference = timer::now();
    let mut prev = reference;
    let mut elapsed: u64 = 0;
    let mut seen = [0u32; N];
    let _ = stats::take();
    for _ in 0..DURATION_S * 1000 / REPORT_MS {
        Timer::new(reference, report).await;
        reference = reference.wrapping_add(report);
        let now = timer::now();
        let window = now.wrapping_sub(prev) as u64;
        let window_us = micros(window);
        prev = now;
        elapsed += window;
        let t_ms = micros(elapsed) / 1000;
        let passes = PASSES.swap(0, Relaxed);
        let d = stats::take();
        let sums = critical_section::with(|cs| LATE_SUM_US.replace(cs, [0; N]));
        for i in 0..N {
            let total = TOTAL[i].load(Relaxed);
            let n = total - seen[i];
            seen[i] = total;
            let avg = if n == 0 { 0 } else { sums[i] / n as u64 };
            let _ = write!(
                line,
                "{t_ms},{i},{},{n},{},{},{avg},{passes},{},{},{},{}",
                period_us(i),
                window_us / period_us(i),
                LATE_MAX_US[i].swap(0, Relaxed),
                d.time,
                d.set,
                d.upcalls,
                d.skipped,
            );
            line.send();
        }
    }
    let _ = line.write_str("# fin");
    line.send();
    TockSyscalls::exit_terminate(0);
}

fn main() {
    libtock::run(|spawner| {
        for i in 0..N {
            spawner.spawn(worker(i).unwrap());
        }
        spawner.spawn(spin().unwrap());
        spawner.spawn(load().unwrap());
        spawner.spawn(report().unwrap());
    })
}
