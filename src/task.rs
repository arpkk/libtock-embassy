use libtock_platform::{Syscalls, YieldNoWaitReturn};

pub async fn yield_wait() {
    #[cfg(feature = "relative-timer")]
    crate::timer::flush();
    crate::runtime::TockSyscalls::yield_wait();
}

pub async fn yield_no_wait() -> YieldNoWaitReturn {
    #[cfg(feature = "relative-timer")]
    crate::timer::flush();
    crate::runtime::TockSyscalls::yield_no_wait()
}
