// Como 6_tormenta_lenta, pero todos los workers con el mismo período y desfasados entre sí
// Durante la medición no se escribe en la consola, salvo que CONSOLE_LOAD lo active
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
use libtock::stats;

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

const N: usize = 7;
const BASE_PERIOD_US: u64 = 4000;
const SCALE_PCT: u64 = 50;
const OFFSET_US: u64 = period_us() / N as u64;
const CONSOLE_LOAD: bool = false;
const LOAD_LINE: &str = "# ..............................................................";
const LOAD_MS: u64 = 40;
const REPORT_MS: u64 = 1000;
const DURATION_S: u64 = 60;
const SAMPLES: usize = (DURATION_S * 1000 / REPORT_MS) as usize;
const HEADER: &str =
    "t_ms,worker,period_us,offset_us,n,expected,late_max_us,late_avg_us,passes,cmd_time,cmd_set,upcalls,skipped";

static TOTAL: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
static LATE_MAX_US: [AtomicU32; N] = [const { AtomicU32::new(0) }; N];
static LATE_SUM_US: Mutex<RefCell<[u64; N]>> = Mutex::new(RefCell::new([0; N]));
static PASSES: AtomicU32 = AtomicU32::new(0);
static RESULTS: Mutex<RefCell<[Sample; SAMPLES]>> =
    Mutex::new(RefCell::new([Sample::ZERO; SAMPLES]));

const fn period_us() -> u64 {
    BASE_PERIOD_US * SCALE_PCT / 100
}

const fn offset_us(i: usize) -> u64 {
    OFFSET_US * i as u64
}

/// Lo medido de un worker en una ventana.
#[derive(Clone, Copy)]
struct Row {
    n: u32,
    expected: u32,
    late_max_us: u32,
    late_avg_us: u32,
}

/// Una ventana de REPORT_MS.
#[derive(Clone, Copy)]
struct Sample {
    t_ms: u32,
    passes: u32,
    driver: stats::Stats,
    rows: [Row; N],
}

impl Row {
    const ZERO: Self = Self { n: 0, expected: 0, late_max_us: 0, late_avg_us: 0 };
}

impl Sample {
    const ZERO: Self = Self {
        t_ms: 0,
        passes: 0,
        driver: stats::Stats { time: 0, set: 0, upcalls: 0, skipped: 0 },
        rows: [Row::ZERO; N],
    };
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
async fn worker(i: usize, t0: Instant) {
    let period = Duration::from_micros(period_us());
    let mut next = t0 + Duration::from_micros(offset_us(i));
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
        let _ = line.write_str(LOAD_LINE);
        line.send();
    }
}

/// Guarda una muestra por ventana sin escribir en la consola, y al final imprime el CSV.
#[embassy_executor::task]
async fn report() {
    let mut line = Line::new();
    let _ = write!(line, "# midiendo {DURATION_S} s");
    line.send();

    // Descarta lo acumulado mientras se imprimía la línea anterior.
    let mut seen = [0u32; N];
    for i in 0..N {
        seen[i] = TOTAL[i].load(Relaxed);
        LATE_MAX_US[i].store(0, Relaxed);
    }
    critical_section::with(|cs| LATE_SUM_US.replace(cs, [0; N]));
    PASSES.store(0, Relaxed);
    let _ = stats::take();

    let start = Instant::now();
    let mut prev = start;
    for k in 0..SAMPLES {
        Timer::at(start + Duration::from_millis((k as u64 + 1) * REPORT_MS)).await;
        let now = Instant::now();
        let window_us = now.duration_since(prev).as_micros();
        prev = now;
        let mut sample = Sample::ZERO;
        sample.t_ms = now.duration_since(start).as_millis() as u32;
        sample.passes = PASSES.swap(0, Relaxed);
        sample.driver = stats::take();
        let sums = critical_section::with(|cs| LATE_SUM_US.replace(cs, [0; N]));
        for i in 0..N {
            let total = TOTAL[i].load(Relaxed);
            let n = total - seen[i];
            seen[i] = total;
            sample.rows[i] = Row {
                n,
                expected: (window_us / period_us()) as u32,
                late_max_us: LATE_MAX_US[i].swap(0, Relaxed),
                late_avg_us: if n == 0 { 0 } else { (sums[i] / n as u64) as u32 },
            };
        }
        critical_section::with(|cs| RESULTS.borrow_ref_mut(cs)[k] = sample);
    }

    let _ = line.write_str(HEADER);
    line.send();
    for k in 0..SAMPLES {
        let s = critical_section::with(|cs| RESULTS.borrow_ref(cs)[k]);
        for (i, r) in s.rows.iter().enumerate() {
            let _ = write!(
                line,
                "{},{i},{},{},{},{},{},{},{},{},{},{},{}",
                s.t_ms,
                period_us(),
                offset_us(i),
                r.n,
                r.expected,
                r.late_max_us,
                r.late_avg_us,
                s.passes,
                s.driver.time,
                s.driver.set,
                s.driver.upcalls,
                s.driver.skipped,
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
        let t0 = Instant::now();
        for i in 0..N {
            spawner.spawn(worker(i, t0).unwrap());
        }
        spawner.spawn(spin().unwrap());
        if CONSOLE_LOAD {
            spawner.spawn(load().unwrap());
        }
        spawner.spawn(report().unwrap());
    })
}
