use crate::manifest::FORMAT;
use crate::version::Version;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("cartridge.toml: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("cartridge format {0} is not supported (this Cabinet reads format {FORMAT})")]
    UnsupportedFormat(i64),
    #[error("this cartridge needs The Cabinet {needed} or newer (this is {current})")]
    NeedsNewerCabinet { needed: Version, current: Version },
    #[error("invalid cartridge.toml:\n- {}", .0.join("\n- "))]
    Invalid(Vec<String>),
    #[error("invalid cartridge: {0}")]
    Archive(String),
    #[error("signature: {0}")]
    Signature(String),
    #[error("cartridge archive: {0}")]
    Zip(#[from] zip::result::ZipError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}
