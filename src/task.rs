use libtock_platform::{Syscalls, YieldNoWaitReturn};

pub async fn yield_wait() {
    crate::runtime::TockSyscalls::yield_wait();
}

pub async fn yield_no_wait() -> YieldNoWaitReturn {
    crate::runtime::TockSyscalls::yield_no_wait()
}
