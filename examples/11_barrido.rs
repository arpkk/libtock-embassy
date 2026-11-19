// Experimento de atraso de timers con time_driver: barrido de N workers del mismo período, desfasados
// de forma pareja, con y sin una tarea que nunca duerme. Cada configuración se mide CONFIG_S s tras
// WARMUP_MS de calentamiento. Todo el barrido se repite REPEATS veces. Los resultados se guardan en RAM
// y al final se imprimen dos tablas CSV, resumen e histograma, que se identifican por (config, rep).
// pXX_us es el límite superior de la cubeta del histograma donde cae ese percentil.
#![no_main]
#![no_std]

use core::cell::RefCell;
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use critical_section::Mutex;
use embassy_executor::Spawner;
use embassy_futures::yield_now;
use embassy_time::{Duration, Instant, Timer};
use libtock::console::Console;
use libtock::platform::Syscalls;
use libtock::runtime::{TockSyscalls, set_main, stack_size};
use libtock::stats;
use libtock_embassy as libtock;

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

const NS: [usize; 5] = [1, 2, 4, 8, 16];
const PERIODS_US: [u64; 2] = [2000, 8000];
const SPINS: [bool; 2] = [false, true];
const N_MAX: usize = 16;
const WARMUP_MS: u64 = 500;
const CONFIG_S: u64 = 10;
const REPEATS: usize = 3;
const CONFIGS: usize = NS.len() * PERIODS_US.len() * SPINS.len();
const RUNS: usize = CONFIGS * REPEATS;

// Histograma de atrasos: FINE cubetas de FINE_US hasta 1024 µs, y después 7 que se duplican.
const FINE: usize = 32;
const FINE_US: u32 = 32;
const BUCKETS: usize = FINE + 7;

static HIST: [AtomicU32; BUCKETS] = [const { AtomicU32::new(0) }; BUCKETS];
static LATE_SUM_US: Mutex<RefCell<u64>> = Mutex::new(RefCell::new(0));
static LATE_MAX_US: AtomicU32 = AtomicU32::new(0);
static WAKES: AtomicU32 = AtomicU32::new(0);
static DONE: AtomicU32 = AtomicU32::new(0);
static PASSES: AtomicU32 = AtomicU32::new(0);
static SPIN_STOP: AtomicBool = AtomicBool::new(false);
static SPIN_DONE: AtomicBool = AtomicBool::new(true);
static RESULTS: Mutex<RefCell<[Sample; RUNS]>> = Mutex::new(RefCell::new([Sample::ZERO; RUNS]));

/// Resultado de una configuración en una repetición.
#[derive(Clone, Copy)]
struct Sample {
    config: u32,
    rep: u32,
    n: u32,
    period_us: u32,
    spin: bool,
    wakes: u32,
    expected: u32,
    late_avg_us: u32,
    late_max_us: u32,
    passes: u32,
    driver: stats::Stats,
    hist: [u32; BUCKETS],
}

impl Sample {
    const ZERO: Self = Self {
        config: 0,
        rep: 0,
        n: 0,
        period_us: 0,
        spin: false,
        wakes: 0,
        expected: 0,
        late_avg_us: 0,
        late_max_us: 0,
        passes: 0,
        driver: stats::Stats {
            time: 0,
            set: 0,
            upcalls: 0,
            skipped: 0,
        },
        hist: [0; BUCKETS],
    };
}

/// Plazos de una configuración: los workers registran los despertares con plazo en
/// `[from, end)`.
#[derive(Clone, Copy)]
struct Run {
    t0: Instant,
    period: Duration,
    from: Instant,
    end: Instant,
}

/// Plazos `first + k·period` que caen en `[from, end)`: los que un worker registra.
fn deadlines(first: u64, period: u64, from: u64, end: u64) -> u32 {
    let before = |t: u64| {
        if t <= first {
            0
        } else {
            (t - first).div_ceil(period)
        }
    };
    (before(end) - before(from)) as u32
}

fn bucket(late_us: u32) -> usize {
    let fine_end = FINE as u32 * FINE_US;
    if late_us < fine_end {
        (late_us / FINE_US) as usize
    } else {
        let doublings = 31 - (late_us / fine_end).leading_zeros();
        FINE + (doublings as usize).min(BUCKETS - FINE - 1)
    }
}

/// Límite superior, exclusivo, de la cubeta `b` en µs. La última no tiene límite.
fn bucket_hi(b: usize) -> Option<u32> {
    if b < FINE {
        Some((b as u32 + 1) * FINE_US)
    } else if b + 1 < BUCKETS {
        Some((FINE as u32 * FINE_US) << (b - FINE + 1))
    } else {
        None
    }
}

fn percentile(s: &Sample, pct: u64) -> u32 {
    let target = (s.wakes as u64 * pct).div_ceil(100).max(1);
    let mut acc = 0u64;
    for (b, &count) in s.hist.iter().enumerate() {
        acc += count as u64;
        if acc >= target {
            return bucket_hi(b).map_or(s.late_max_us, |hi| hi.min(s.late_max_us));
        }
    }
    s.late_max_us
}

fn record(late_us: u64) {
    let late = late_us.min(u32::MAX as u64) as u32;
    HIST[bucket(late)].fetch_add(1, Relaxed);
    LATE_MAX_US.fetch_max(late, Relaxed);
    critical_section::with(|cs| *LATE_SUM_US.borrow_ref_mut(cs) += late_us);
    WAKES.fetch_add(1, Relaxed);
}

fn reset_latency() {
    for h in &HIST {
        h.store(0, Relaxed);
    }
    critical_section::with(|cs| *LATE_SUM_US.borrow_ref_mut(cs) = 0);
    LATE_MAX_US.store(0, Relaxed);
    WAKES.store(0, Relaxed);
}

struct Line {
    buf: [u8; 160],
    len: usize,
}

impl Line {
    const fn new() -> Self {
        Self {
            buf: [0; 160],
            len: 0,
        }
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

#[embassy_executor::task(pool_size = N_MAX)]
async fn worker(run: Run, offset: Duration) {
    let mut next = run.t0 + offset;
    loop {
        next += run.period;
        if next >= run.end {
            break;
        }
        Timer::at(next).await;
        if next >= run.from {
            record(Instant::now().saturating_duration_since(next).as_micros());
        }
    }
    DONE.fetch_add(1, Relaxed);
}

#[embassy_executor::task]
async fn spin() {
    while !SPIN_STOP.load(Relaxed) {
        PASSES.fetch_add(1, Relaxed);
        yield_now().await;
    }
    SPIN_DONE.store(true, Relaxed);
}

/// Corre las configuraciones una tras otra, sin escribir en la consola mientras miden, y al
/// final imprime el resumen y el histograma.
#[embassy_executor::task]
async fn control(spawner: Spawner) {
    let mut line = Line::new();
    let _ = write!(
        line,
        "# midiendo {CONFIGS} configuraciones x {REPEATS} repeticiones de {CONFIG_S} s"
    );
    line.send();

    let mut slot = 0;
    for rep in 1..=REPEATS as u32 {
        let mut config = 0;
        for spin_on in SPINS {
            for period_us in PERIODS_US {
                for n in NS {
                    reset_latency();
                    DONE.store(0, Relaxed);
                    let t0 = Instant::now();
                    let from = t0 + Duration::from_millis(WARMUP_MS);
                    let run = Run {
                        t0,
                        period: Duration::from_micros(period_us),
                        from,
                        end: from + Duration::from_secs(CONFIG_S),
                    };
                    let mut expected = 0;
                    for i in 0..n {
                        let offset = Duration::from_micros(period_us * i as u64 / n as u64);
                        expected += deadlines(
                            (t0 + offset + run.period).as_ticks(),
                            run.period.as_ticks(),
                            run.from.as_ticks(),
                            run.end.as_ticks(),
                        );
                        spawner.spawn(worker(run, offset).unwrap());
                    }
                    if spin_on {
                        SPIN_STOP.store(false, Relaxed);
                        SPIN_DONE.store(false, Relaxed);
                        spawner.spawn(spin().unwrap());
                    }

                    Timer::at(run.from).await;
                    PASSES.store(0, Relaxed);
                    let _ = stats::take();
                    Timer::at(run.end).await;
                    let passes = PASSES.load(Relaxed);
                    let driver = stats::take();

                    SPIN_STOP.store(true, Relaxed);
                    while DONE.load(Relaxed) < n as u32 || !SPIN_DONE.load(Relaxed) {
                        Timer::after(Duration::from_millis(1)).await;
                    }

                    let wakes = WAKES.load(Relaxed);
                    let sum = critical_section::with(|cs| *LATE_SUM_US.borrow_ref(cs));
                    let mut sample = Sample {
                        config,
                        rep,
                        n: n as u32,
                        period_us: period_us as u32,
                        spin: spin_on,
                        wakes,
                        expected,
                        late_avg_us: if wakes == 0 {
                            0
                        } else {
                            (sum / wakes as u64) as u32
                        },
                        late_max_us: LATE_MAX_US.load(Relaxed),
                        passes,
                        driver,
                        hist: [0; BUCKETS],
                    };
                    for (b, h) in HIST.iter().enumerate() {
                        sample.hist[b] = h.load(Relaxed);
                    }
                    critical_section::with(|cs| RESULTS.borrow_ref_mut(cs)[slot] = sample);
                    slot += 1;
                    config += 1;
                }
            }
        }
    }

    let _ = line.write_str("# resumen");
    line.send();
    let _ = line.write_str(
        "config,rep,n,period_us,spin,events_per_s,wakes,expected,late_avg_us,p50_us,p90_us,p99_us,late_max_us,passes,cmd_time,cmd_set,upcalls,skipped",
    );
    line.send();
    for k in 0..RUNS {
        let s = critical_section::with(|cs| RESULTS.borrow_ref(cs)[k]);
        let _ = write!(
            line,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            s.config,
            s.rep,
            s.n,
            s.period_us,
            s.spin as u8,
            s.n as u64 * 1_000_000 / s.period_us as u64,
            s.wakes,
            s.expected,
            s.late_avg_us,
            percentile(&s, 50),
            percentile(&s, 90),
            percentile(&s, 99),
            s.late_max_us,
            s.passes,
            s.driver.time,
            s.driver.set,
            s.driver.upcalls,
            s.driver.skipped,
        );
        line.send();
    }

    let _ = line.write_str("# histograma");
    line.send();
    let _ = line.write_str("config,rep,n,period_us,spin,bucket_hi_us,count");
    line.send();
    for k in 0..RUNS {
        let s = critical_section::with(|cs| RESULTS.borrow_ref(cs)[k]);
        for (b, &count) in s.hist.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let _ = write!(
                line,
                "{},{},{},{},{},",
                s.config, s.rep, s.n, s.period_us, s.spin as u8
            );
            let _ = match bucket_hi(b) {
                Some(hi) => write!(line, "{hi},{count}"),
                None => write!(line, "inf,{count}"),
            };
            line.send();
        }
    }
    let _ = line.write_str("# fin");
    line.send();
    TockSyscalls::exit_terminate(0);
}

fn main() {
    libtock::run(|spawner| {
        spawner.spawn(control(spawner).unwrap());
    })
}
