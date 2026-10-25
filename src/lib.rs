//! Linux account profiles. Secret bytes never implement Debug or Serialize.
pub mod provider;
pub mod store;

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub struct Error(pub &'static str);
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Error {}
pub trait Safe<T> {
    fn safe(self, message: &'static str) -> Result<T>;
}
impl<T, E> Safe<T> for std::result::Result<T, E> {
    fn safe(self, message: &'static str) -> Result<T> {
        self.map_err(|_| Error(message))
    }
}
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub mod usage;

pub mod runner;

pub mod terminal;

pub mod display;
pub mod identity;

pub mod rename;
