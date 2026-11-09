use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Identifier {
    Numeric(u64),
    Alpha(String),
}

impl Ord for Identifier {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Identifier::Numeric(a), Identifier::Numeric(b)) => a.cmp(b),
            (Identifier::Numeric(_), Identifier::Alpha(_)) => Ordering::Less,
            (Identifier::Alpha(_), Identifier::Numeric(_)) => Ordering::Greater,
            (Identifier::Alpha(a), Identifier::Alpha(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Identifier {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pre: Vec<Identifier>,
}

impl Version {
    pub fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
            pre: Vec::new(),
        }
    }

    pub fn is_prerelease(&self) -> bool {
        !self.pre.is_empty()
    }
}

fn numeric(part: &str, what: &str) -> Result<u64, String> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{what} \"{part}\" is not a number"));
    }
    if part.len() > 1 && part.starts_with('0') {
        return Err(format!("{what} \"{part}\" has a leading zero"));
    }
    part.parse()
        .map_err(|_| format!("{what} \"{part}\" is too large"))
}

impl FromStr for Version {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let invalid = |reason: String| format!("\"{value}\" is not a semver version: {reason}");
        if value.contains('+') {
            return Err(invalid(
                "build metadata (+...) is not allowed; one version is one build".to_string(),
            ));
        }
        let (core, pre) = match value.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (value, None),
        };
        let parts: Vec<&str> = core.split('.').collect();
        let [major, minor, patch] = parts[..] else {
            return Err(invalid("expected MAJOR.MINOR.PATCH".to_string()));
        };
        let mut version = Version::new(
            numeric(major, "major").map_err(invalid)?,
            numeric(minor, "minor").map_err(invalid)?,
            numeric(patch, "patch").map_err(invalid)?,
        );
        if let Some(pre) = pre {
            for identifier in pre.split('.') {
                if identifier.is_empty()
                    || !identifier
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                {
                    return Err(invalid(format!(
                        "pre-release identifier \"{identifier}\" is invalid"
                    )));
                }
                let parsed = if identifier.bytes().all(|b| b.is_ascii_digit()) {
                    Identifier::Numeric(numeric(identifier, "pre-release").map_err(invalid)?)
                } else {
                    Identifier::Alpha(identifier.to_string())
                };
                version.pre.push(parsed);
            }
        }
        Ok(version)
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => self.pre.cmp(&other.pre),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (index, identifier) in self.pre.iter().enumerate() {
            f.write_str(if index == 0 { "-" } else { "." })?;
            match identifier {
                Identifier::Numeric(value) => write!(f, "{value}")?,
                Identifier::Alpha(value) => f.write_str(value)?,
            }
        }
        Ok(())
    }
}

impl TryFrom<String> for Version {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Version> for String {
    fn from(value: Version) -> Self {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(value: &str) -> Version {
        value.parse().unwrap()
    }

    #[test]
    fn parses_and_displays() {
        for value in ["0.1.0", "2.0.0", "1.2.3-rc.1", "1.0.0-alpha-2.x"] {
            assert_eq!(v(value).to_string(), value);
        }
    }

    #[test]
    fn rejects_malformed_versions() {
        for value in [
            "1.0",
            "1.0.0.0",
            "01.0.0",
            "1.0.x",
            "",
            "1.0.0-",
            "1.0.0-a..b",
            "1.0.0-01",
            "1.0.0+build",
        ] {
            assert!(value.parse::<Version>().is_err(), "{value} should fail");
        }
    }

    #[test]
    fn orders_by_semver_precedence() {
        let ordered = [
            "1.0.0-alpha",
            "1.0.0-alpha.1",
            "1.0.0-alpha.beta",
            "1.0.0-beta",
            "1.0.0-beta.2",
            "1.0.0-beta.11",
            "1.0.0-rc.1",
            "1.0.0",
            "1.0.1",
            "1.1.0",
            "2.0.0",
        ];
        for pair in ordered.windows(2) {
            assert!(v(pair[0]) < v(pair[1]), "{} < {}", pair[0], pair[1]);
        }
        assert!(v("0.1.15") < v("2.0.0"));
        assert!(v("0.10.0") > v("0.9.9"));
    }
}
