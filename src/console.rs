use libtock_console as console;
use libtock_platform::ErrorCode;

pub type Console = console::Console<crate::runtime::TockSyscalls>;
pub use console::ConsoleWriter;

pub async fn write_async(bytes: &[u8]) -> Result<(), ErrorCode> {
    Console::write(bytes)
}

pub async fn write_str_async(s: &str) -> Result<(), ErrorCode> {
    write_async(s.as_bytes()).await
}
