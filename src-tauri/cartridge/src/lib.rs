pub mod archive;
pub mod digest;
pub mod error;
pub mod manifest;
pub mod platform;
pub mod signature;
pub mod validate;
pub mod vars;
pub mod version;

pub use archive::{Inspected, Packed};
pub use error::{Error, Result};
pub use manifest::Manifest;
pub use platform::Platform;
pub use signature::Publisher;
pub use version::Version;

#[cfg(test)]
mod archive_tests;
#[cfg(test)]
mod tests;
