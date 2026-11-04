// Tormenta de timers en dos procesos a la vez
#![no_main]
#![no_std]

use core::cell::RefCell;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

use critical_section::Mutex;
use embassy_futures::yield_now;
use embassy_time::{Duration, Instant, Timer};
use libtock_embassy as libtock;
use libtock::console::Console;
use libtock::platform::Syscalls;
use libtock::runtime::{TockSyscalls, set_main, stack_size};
use libtock::time_driver::stats;

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

const BASE_PERIODS_US: [u64; 8] = [2300, 2900, 3700, 4100, 5300, 7100, 11000, 130_000];
const N: usize = BASE_PERIODS_US.len();
const SCALE_PCT: u64 = 25;
const LOAD_LINE: &str = "# ..............................................................";
const LOAD_MS: u64 = 40;
const REPORT_MS: u64 = 1000;
const DURATION_S: u64 = 60;
const PROC: &str = match option_env!("PROC") {
    Some(p) => p,
    None => "a",
};

static TOTAL: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
static LATE_MAX_US: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
static LATE_SUM_US: Mutex<RefCell<[u64; N]>> = Mutex::new(RefCell::new([0; N]));
static PASSES: AtomicU32 = AtomicU32::new(0);

const fn period_us(i: usize) -> u64 {
    BASE_PERIODS_US[i] * SCALE_PCT / 100
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
    let period = Duration::from_micros(period_us(i));
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
async fn load() {
    let mut line = Line::new();
    loop {
        Timer::after(Duration::from_millis(LOAD_MS)).await;
        let _ = write!(line, "{LOAD_LINE} {PROC}");
        line.send();
    }
}

#[embassy_executor::task]
async fn report() {
    let mut line = Line::new();
    let _ = line.write_str(
        "proc,t_ms,worker,period_us,n,expected,late_max_us,late_avg_us,passes,cmd_time,cmd_set,upcalls,skipped",
    );
    line.send();
    let start = Instant::now();
    let mut prev = start;
    let mut seen = [0u32; N];
    let _ = stats::take();
    for k in 1..=DURATION_S * 1000 / REPORT_MS {
        Timer::at(start + Duration::from_millis(k * REPORT_MS)).await;
        let now = Instant::now();
        let window_us = now.duration_since(prev).as_micros();
        prev = now;
        let t_ms = now.duration_since(start).as_millis();
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
                "{PROC},{t_ms},{i},{},{n},{},{},{avg},{passes},{},{},{},{}",
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
    let _ = write!(line, "# fin {PROC}");
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
