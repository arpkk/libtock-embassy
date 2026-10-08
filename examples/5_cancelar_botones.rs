// Botón 1 enciende led 1, botón 2 enciende led 2 y cada 3ms se intenta tomar el botón 2 y soltarlo, para provocar cancelaciones de upcalls
#![no_std]

use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Timer};
use libtock_embassy as libtock;
use libtock::buttons::{self, Button, ButtonState};
use libtock::console::Console;
use libtock::leds::Leds;
use libtock::runtime::{set_main, stack_size};

extern crate libtock_debug_panic;

set_main! {main}
stack_size! {0x1000}

static CANCELS: AtomicU32 = AtomicU32::new(0);
static CHURN: AtomicU32 = AtomicU32::new(0);
static CHURN_FAIL: AtomicU32 = AtomicU32::new(0);

fn slice() -> Timer {
    Timer::after(Duration::from_millis(1))
}

async fn press(b: &mut Button) {
    while let Either::Second(()) = select(b.wait_for_press(), slice()).await {
        CANCELS.fetch_add(1, Relaxed);
    }
}

async fn release(b: &mut Button) {
    while let Either::Second(()) = select(b.wait_for_release(), slice()).await {
        CANCELS.fetch_add(1, Relaxed);
    }
}

// espera el cambio del boton pero cancelando y rearmando la espera cada 1ms
async fn change(b: &mut Button) -> ButtonState {
    loop {
        match select(b.wait_for_change(), slice()).await {
            Either::First(s) => return s,
            Either::Second(()) => {
                CANCELS.fetch_add(1, Relaxed);
            }
        }
    }
}

//boton 1 enciende led 1 e imprime cada vez que se presiona y suelta
#[embassy_executor::task]
async fn level(mut b: Button) {
    let mut w = Console::writer();
    let mut n: u32 = 0;
    loop {
        press(&mut b).await;
        let _ = Leds::on(0);
        release(&mut b).await;
        let _ = Leds::off(0);
        n += 1;
        let _ = writeln!(w, "boton {}: ciclo {n}", b.index());
    }
}

// boton 2 enciende led 2 pero espera a que cambie el estado del botón
#[embassy_executor::task]
async fn edge(mut b: Button) {
    let mut w = Console::writer();
    let mut prev = b.read().ok();
    let mut lost: u32 = 0;
    loop {
        let s = change(&mut b).await;
        if prev == Some(s) {
            lost += 1;
        }
        prev = Some(s);
        let _ = Leds::toggle(1);
        let _ = writeln!(w, "boton {}: {s:?} repetidos={lost}", b.index());
    }
}

// cada 3ms intenta tomar el botón 2 y soltarlo
#[embassy_executor::task]
async fn churn() {
    loop {
        if Button::take(0).is_some() || Button::take(1).is_some() || Button::take(99).is_some() {
            CHURN_FAIL.fetch_add(1, Relaxed);
        }
        match Button::take(2) {
            Some(b) => drop(b),
            None => {
                CHURN_FAIL.fetch_add(1, Relaxed);
            }
        }
        CHURN.fetch_add(1, Relaxed);
        Timer::after(Duration::from_millis(3)).await;
    }
}

//cada 5s imprime la cantidad de cancelaciones y churn
#[embassy_executor::task]
async fn report() {
    let mut w = Console::writer();
    loop {
        Timer::after(Duration::from_secs(5)).await;
        let _ = writeln!(
            w,
            "cancels={} churn={} churn_fail={}",
            CANCELS.swap(0, Relaxed),
            CHURN.swap(0, Relaxed),
            CHURN_FAIL.load(Relaxed),
        );
    }
}

fn main() {
    buttons::init(|| {
        libtock::run(|spawner| {
            spawner.spawn(level(Button::take(0).unwrap()).unwrap());
            spawner.spawn(edge(Button::take(1).unwrap()).unwrap());
            spawner.spawn(churn().unwrap());
            spawner.spawn(report().unwrap());
        })
    })
}

/*
Resultados:
tock$ process 5_cancelar_botones
𝐀𝐩𝐩: 5_cancelar_botones   -   [Yielded]
 Events Queued: 0   Syscall Count: 9815   Dropped Upcall Count: 53
 Restart Count: 0
 Last Syscall: Yield { yield_type: Wait }
 Completion Code: None


 ╔═══════════╤══════════════════════════════════════════╗
 ║  Address  │ Region Name    Used | Allocated (bytes)  ║
 ╚0x20014000═╪══════════════════════════════════════════╝
             │ Grant Ptrs      120
             │ Upcalls         320
             │ Process         772
  0x20013B44 ┼───────────────────────────────────────────
             │ ▼ Grant         116
  0x20013AD0 ┼───────────────────────────────────────────
             │ Unused
  0x200111E8 ┼───────────────────────────────────────────
             │ ▲ Heap            0 |  10472               S
  0x200111E8 ┼─────────────────────────────────────────── R
             │ Data            488 |    488               A
  0x20011000 ┼─────────────────────────────────────────── M
             │ ▼ Stack         448 |   4096          
  0x20010E40 ┼───────────────────────────────────────────
             │ Unused
  0x20010000 ┴───────────────────────────────────────────
             .....
  0x00048000 ┬─────────────────────────────────────────── F
             │ App Flash     32640                        L
  0x00040080 ┼─────────────────────────────────────────── A
             │ Protected       128                        S
  0x00040000 ┴─────────────────────────────────────────── H

No se imprimio ningun report
*/