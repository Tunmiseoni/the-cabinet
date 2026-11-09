use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Platform {
    MacosAarch64,
    MacosX86_64,
    LinuxX86_64,
    WindowsX86_64,
}

impl Platform {
    pub const ALL: [Platform; 4] = [
        Platform::MacosAarch64,
        Platform::MacosX86_64,
        Platform::LinuxX86_64,
        Platform::WindowsX86_64,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Platform::MacosAarch64 => "macos-aarch64",
            Platform::MacosX86_64 => "macos-x86_64",
            Platform::LinuxX86_64 => "linux-x86_64",
            Platform::WindowsX86_64 => "windows-x86_64",
        }
    }

    pub fn current() -> Option<Platform> {
        Self::from_os_arch(std::env::consts::OS, std::env::consts::ARCH)
    }

    pub fn from_os_arch(os: &str, arch: &str) -> Option<Platform> {
        Self::ALL
            .into_iter()
            .find(|platform| platform.as_str() == format!("{os}-{arch}"))
    }

    pub fn is_windows(self) -> bool {
        matches!(self, Platform::WindowsX86_64)
    }
}

impl fmt::Display for Platform {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Platform {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|platform| platform.as_str() == value)
            .ok_or_else(|| {
                let known: Vec<_> = Self::ALL.iter().map(|p| p.as_str()).collect();
                format!("unknown platform \"{value}\" (known: {})", known.join(", "))
            })
    }
}

impl TryFrom<String> for Platform {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Platform> for String {
    fn from(value: Platform) -> Self {
        value.as_str().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_platform() {
        for platform in Platform::ALL {
            assert_eq!(platform.as_str().parse::<Platform>(), Ok(platform));
        }
        assert!("macos-arm64".parse::<Platform>().is_err());
    }

    #[test]
    fn maps_os_and_arch() {
        assert_eq!(
            Platform::from_os_arch("macos", "aarch64"),
            Some(Platform::MacosAarch64)
        );
        assert_eq!(
            Platform::from_os_arch("windows", "x86_64"),
            Some(Platform::WindowsX86_64)
        );
        assert_eq!(Platform::from_os_arch("linux", "aarch64"), None);
    }
}
