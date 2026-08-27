//! Bucle principal y `__pender`: el otro lado del contrato con Embassy.
//!
//! Embassy le pide a la plataforma (a) un bucle que pollee y ceda el procesador
//! cuando todo duerme y (b) una función `__pender` que el executor llama cuando
//! una tarea queda lista. En bare metal eso se resuelve con `SEV`/`WFE`; acá el
//! interrupt lo reemplaza la **upcall**, y `WFE` lo reemplaza `yield-wait`.
//!
//! Por eso `embassy-executor` va sin ninguna feature `platform-*`: esas features
//! definen su propio `__pender` y chocarían con el de acá.
#![allow(unsafe_code)] // `export_name` para `__pender` y `raw::Executor::poll`.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::{Spawner, raw};
use libtock_platform::Syscalls;
use static_cell::StaticCell;

use crate::runtime::TockSyscalls as S;

static PENDING: AtomicBool = AtomicBool::new(false);

#[unsafe(export_name = "__pender")]
fn __pender(_context: *mut ()) {
    PENDING.store(true, Ordering::Release);
}

/// Executor de Embassy sobre las syscalls de Tock.
pub struct Executor {
    inner: raw::Executor,
    _not_send: PhantomData<*mut ()>,
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor {
    pub fn new() -> Self {
        Self {
            inner: raw::Executor::new(core::ptr::null_mut()),
            _not_send: PhantomData,
        }
    }

    /// Corre el executor para siempre.
    ///
    /// El bucle es `poll` + ceder. Cuál de los dos `yield` usar lo decide la
    /// bandera que dejó `__pender`:
    ///
    /// - bandera puesta: alguna tarea despertó a otra durante el `poll`, así que
    ///   hay que volver a pollear. Igual pasamos por `yield-no-wait` para no
    ///   dejar upcalls pendientes sin atender, pero sin dormirnos.
    /// - bandera limpia: nadie quedó listo, y lo único que puede cambiar eso es
    ///   una upcall. `yield-wait` duerme en el kernel hasta que llegue.
    pub fn run(&'static mut self, init: impl FnOnce(Spawner)) -> ! {
        init(self.inner.spawner());

        loop {
            // Limpiar ANTES de pollear: durante el poll el único que puede poner
            // la bandera es una tarea despertando a otra.
            PENDING.store(false, Ordering::Release);
            unsafe { self.inner.poll() };
            if PENDING.load(Ordering::Acquire) {
                let _ = S::yield_no_wait();
            } else {
                S::yield_wait();
            }
        }
    }
}

/// Arma todo el runtime y no vuelve nunca: driver de tiempo suscrito, executor
/// en memoria estática y bucle principal corriendo.
///
/// Es el punto de entrada normal de una app:
///
/// ```ignore
/// fn main() {
///     libtock_embassy::run(|spawner| {
///         spawner.spawn(blink()).unwrap();
///     })
/// }
/// ```
pub fn run(init: impl FnOnce(Spawner)) -> ! {
    static EXECUTOR: StaticCell<Executor> = StaticCell::new();
    crate::time_driver::init(|| EXECUTOR.init(Executor::new()).run(init))
}
