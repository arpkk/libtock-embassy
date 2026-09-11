#![no_main]
#![no_std]

use core::fmt::Write;

use embassy_time::{Duration, Timer};
use libtock_embassy as libtock;
use libtock::console::Console;
use libtock::leds::Leds;
use libtock::runtime::{set_main, stack_size};
use libtock::time_driver::trace::{self, Record};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x2000}

const TICK_SHIFT: u32 = 23;

/// Ticks a milisegundos, por división entera (trunca). Solo para las columnas
/// de conveniencia: las `[ticks]` van crudas. No llamarla con `at_in`, que puede
/// ser `u64::MAX` y desbordaría la multiplicación.
fn to_ms(ticks: u64) -> u64 {
    (ticks * 1000) >> TICK_SHIFT
}

const LED: u32 = 0;

#[embassy_executor::task]
async fn blink() {
    // Apaga todos una vez al arrancar: si quedaron prendidos de otra app, el
    // kernel no los toca al instalar la nueva.
    for led in 0..Leds::count().unwrap_or(1) {
        let _ = Leds::off(led);
    }

    let mut on = false;
    loop {
        let _ = if on { Leds::on(LED) } else { Leds::off(LED) };
        on = !on;
        Timer::after(Duration::from_secs(10)).await;
    }
}

#[embassy_executor::task]
async fn logger() {
    let mut writer = Console::writer();

    // Preámbulo: deja el archivo autodescrito, sin depender de esta conversación.
    let _ = writeln!(writer, "# libtock-embassy :: traza de time_driver::set_alarm");
    let _ = writeln!(
        writer,
        "# TICK_HZ=8388608 (2^23) | vuelta=4294967296 ticks (512 s) | MAX_DT=1073741824 ticks (128 s)"
    );
    let _ = writeln!(
        writer,
        "# ticks -> ms: ticks*1000>>23 (division entera, trunca). Las columnas [ticks] son crudas."
    );
    let _ = writeln!(
        writer,
        "# at_in[ticks]=18446744073709551615 (u64::MAX) => cola vacia: la alarma es un keepalive"
    );
    let _ = writeln!(
        writer,
        "seq,high[vueltas],last[ticks],last[hex],now[ticks],now[ms],at_in[ticks],at_out[ticks],dt[ticks],dt[ms],clamp[bool],armed[bool]"
    );

    let mut buf = [Record::default(); 16];
    let mut seq: u32 = 0;

    loop {
        // Despierta cuando el driver graba algo, no por reloj. Así el drenador
        // no arma timers propios y deja de aparecer en su propia traza.
        trace::wait().await;

        let (n, lost) = trace::drain(&mut buf);
        if lost > 0 {
            // El búfer se llenó entre drenajes. Queda como comentario en el CSV
            // para que el hueco no pase inadvertido.
            let _ = writeln!(writer, "# perdidos {lost}");
        }

        for rec in &buf[..n] {
            let now = rec.now();

            let _ = write!(
                writer,
                "{:>4},{:>4},{:>11},0x{:08X},{:>14},{:>9},{:>20},{:>14},",
                seq,
                rec.high,
                rec.last,
                rec.last,
                now,
                to_ms(now),
                rec.at_in,
                rec.at_out
            );

            // `dt` no existe si el plazo ya había vencido: ahí no se armó nada.
            match rec.dt() {
                Some(dt) => {
                    let _ = write!(writer, "{:>10},{:>7},", dt, to_ms(dt as u64));
                }
                None => {
                    let _ = write!(writer, "{:>10},{:>7},", "-", "-");
                }
            }

            let _ = writeln!(
                writer,
                "{},{}",
                if rec.clamped() { 1 } else { 0 },
                if rec.armed { 1 } else { 0 }
            );

            seq = seq.wrapping_add(1);
        }
    }
}

fn main() {
    libtock::run(|spawner| {
        spawner.spawn(blink().unwrap());
        spawner.spawn(logger().unwrap());
    })
}
