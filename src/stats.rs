//! Contadores del driver de tiempo para mediciones. Solo existen con la feature `stats`.

use core::sync::atomic::AtomicU32;
use core::sync::atomic::Ordering::Relaxed;

pub(crate) static TIME: AtomicU32 = AtomicU32::new(0);
pub(crate) static SET: AtomicU32 = AtomicU32::new(0);
pub(crate) static UPCALLS: AtomicU32 = AtomicU32::new(0);
pub(crate) static SKIPPED: AtomicU32 = AtomicU32::new(0);

/// Cuentas acumuladas desde la última llamada a [`take`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Lecturas del contador (`command 2`).
    pub time: u32,
    /// Alarmas armadas (`command 6`).
    pub set: u32,
    /// Upcalls de alarma atendidas.
    pub upcalls: u32,
    /// Re-armes evitados porque la alarma armada ya cubría el plazo.
    pub skipped: u32,
}

/// Devuelve las cuentas y las deja en cero.
pub fn take() -> Stats {
    Stats {
        time: TIME.swap(0, Relaxed),
        set: SET.swap(0, Relaxed),
        upcalls: UPCALLS.swap(0, Relaxed),
        skipped: SKIPPED.swap(0, Relaxed),
    }
}
