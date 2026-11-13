use std::io::Write;
use std::path::{Path, PathBuf};

pub trait Anchors: Send {
    fn path(&self, id: &str) -> Result<String, String>;
    fn place(&self, path: &str, pem: &str) -> Result<(), String>;
    fn remove(&self, path: &str, pem: &str) -> Result<(), String>;
}

pub const PREFIX: &str = "the-cabinet-";

pub struct Layout {
    pub dir: &'static str,
    pub refresh: &'static [&'static str],
}

pub const LAYOUTS: &[Layout] = &[
    Layout {
        dir: "/usr/local/share/ca-certificates",
        refresh: &["/usr/sbin/update-ca-certificates"],
    },
    Layout {
        dir: "/etc/ca-certificates/trust-source/anchors",
        refresh: &["/usr/bin/update-ca-trust", "extract"],
    },
    Layout {
        dir: "/etc/pki/ca-trust/source/anchors",
        refresh: &["/usr/bin/update-ca-trust", "extract"],
    },
];

pub struct DirAnchors {
    pub dir: PathBuf,
    pub refresh: Vec<String>,
}

impl DirAnchors {
    pub fn system() -> Option<Self> {
        LAYOUTS
            .iter()
            .find(|layout| Path::new(layout.dir).is_dir() && Path::new(layout.refresh[0]).exists())
            .map(|layout| Self {
                dir: layout.dir.into(),
                refresh: layout.refresh.iter().map(|word| word.to_string()).collect(),
            })
    }

    fn refresh(&self) -> Result<(), String> {
        let Some((program, args)) = self.refresh.split_first() else {
            return Ok(());
        };
        let output = std::process::Command::new(program)
            .args(args)
            .output()
            .map_err(|err| format!("cannot run {program}: {err}"))?;
        if output.status.success() {
            Ok(())
        } else {
            Err(format!(
                "{} failed: {}",
                self.refresh.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    fn own(&self, path: &str) -> Result<PathBuf, String> {
        let path = PathBuf::from(path);
        let named = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(PREFIX) && name.ends_with(".crt"));
        if path.parent() == Some(self.dir.as_path()) && named {
            Ok(path)
        } else {
            Err(format!(
                "{} is not a certificate The Cabinet placed",
                path.display()
            ))
        }
    }
}

impl Anchors for DirAnchors {
    fn path(&self, id: &str) -> Result<String, String> {
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(format!("\"{id}\" is not a journal id"));
        }
        self.dir
            .join(format!("{PREFIX}{id}.crt"))
            .to_str()
            .map(str::to_string)
            .ok_or_else(|| "the anchors folder is not text".to_string())
    }

    fn place(&self, path: &str, pem: &str) -> Result<(), String> {
        let path = self.own(path)?;
        let written = (|| -> std::io::Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o644);
            }
            let mut file = options.open(&path)?;
            file.write_all(pem.as_bytes())?;
            file.sync_all()
        })();
        written.map_err(|err| format!("cannot write {}: {err}", path.display()))?;
        if let Err(err) = self.refresh() {
            let _ = std::fs::remove_file(&path);
            let _ = self.refresh();
            return Err(err);
        }
        Ok(())
    }

    fn remove(&self, path: &str, pem: &str) -> Result<(), String> {
        let path = self.own(path)?;
        match std::fs::read_to_string(&path) {
            Ok(held) if held == pem => std::fs::remove_file(&path)
                .map_err(|err| format!("cannot remove {}: {err}", path.display()))?,
            Ok(_) => {
                return Err(format!(
                    "{} changed since The Cabinet placed it, so it is left alone",
                    path.display()
                ))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
        }
        self.refresh()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::tests::Scratch;

    pub(crate) fn anchors(scratch: &Scratch) -> DirAnchors {
        let dir = scratch.0.join("anchors");
        std::fs::create_dir_all(&dir).unwrap();
        DirAnchors {
            dir,
            refresh: Vec::new(),
        }
    }

    #[test]
    fn places_and_removes_only_its_own_certificates() {
        let scratch = Scratch::new("anchors");
        let anchors = anchors(&scratch);
        let path = anchors.path("7").unwrap();
        assert!(path.ends_with("/anchors/the-cabinet-7.crt"));
        anchors.place(&path, "PEM").unwrap();
        assert!(anchors.place(&path, "PEM").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "PEM");
        anchors.remove(&path, "PEM").unwrap();
        assert!(!Path::new(&path).exists());
        anchors.remove(&path, "PEM").unwrap();

        anchors.place(&path, "PEM").unwrap();
        std::fs::write(&path, "EDITED").unwrap();
        assert!(anchors.remove(&path, "PEM").is_err());
        assert!(Path::new(&path).exists());

        let foreign = anchors.dir.join("someone-else.crt");
        std::fs::write(&foreign, "PEM").unwrap();
        assert!(anchors.remove(foreign.to_str().unwrap(), "PEM").is_err());
        assert!(foreign.exists());
        for bad in ["", "1/../2", "x"] {
            assert!(anchors.path(bad).is_err(), "{bad}");
        }
        assert!(anchors
            .place(scratch.0.join("the-cabinet-1.crt").to_str().unwrap(), "PEM")
            .is_err());
    }

    #[test]
    fn a_failed_refresh_leaves_no_certificate() {
        let scratch = Scratch::new("anchors-refresh");
        let mut anchors = anchors(&scratch);
        anchors.refresh = vec!["/nonexistent/refresh".to_string()];
        let path = anchors.path("1").unwrap();
        assert!(anchors.place(&path, "PEM").is_err());
        assert!(!Path::new(&path).exists());
    }
}
