//! The operating-system boundary. Only Windows exists so far.

#[cfg(windows)]
mod duplication;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::duplication::Duplicator;
#[cfg(windows)]
pub use self::windows::*;

#[cfg(not(windows))]
compile_error!("bruma currently supports Windows only; Linux and macOS are on the roadmap.");
