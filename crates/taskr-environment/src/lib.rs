//! Portable environment companion protocol, with an optional native catalog host.
pub mod protocol;
pub use protocol::{EnvironmentCompanion, PreparedEnvironment};
#[cfg(feature = "native")]
mod native;
#[cfg(feature = "native")]
pub use native::*;
