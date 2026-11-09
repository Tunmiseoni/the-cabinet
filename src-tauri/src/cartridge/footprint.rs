use super::context::Context;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const SNAPSHOT_FILE: &str = "snapshot.json";
pub const BACKUPS_DIR: &str = "backups";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Touched {
    Path(PathBuf),
    Registry(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Record {
    Path {
        path: PathBuf,
        existed: bool,
        backup: Option<String>,
    },
    Registry {
        key: String,
        existed: bool,
        backup: Option<String>,
    },
}

impl Record {
    fn covers(&self, touched: &Touched) -> bool {
        match (self, touched) {
            (Record::Path { path, .. }, Touched::Path(other)) => path == other,
            (Record::Registry { key, .. }, Touched::Registry(other)) => {
                key.eq_ignore_ascii_case(other)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub records: Vec<Record>,
}

pub fn touched(context: &Context<'_>) -> Result<Vec<Touched>, String> {
    context
        .manifest
        .touches
        .iter()
        .filter(|touch| touch.applies_to(context.platform))
        .map(|touch| match (&touch.path, &touch.registry) {
            (Some(path), _) => context.expand_path(path).map(Touched::Path),
            (None, Some(key)) => Ok(Touched::Registry(key.clone())),
            (None, None) => Err("a touch declares nothing".to_string()),
        })
        .collect()
}

fn load(dir: &Path) -> crate::error::Result<Snapshot> {
    match std::fs::read_to_string(dir.join(SNAPSHOT_FILE)) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Snapshot::default()),
        Err(err) => Err(err.into()),
    }
}

fn save(dir: &Path, snapshot: &Snapshot) -> crate::error::Result<()> {
    std::fs::create_dir_all(dir)?;
    let partial = dir.join(format!("{SNAPSHOT_FILE}.partial"));
    std::fs::write(&partial, serde_json::to_vec_pretty(snapshot)?)?;
    std::fs::rename(&partial, dir.join(SNAPSHOT_FILE))?;
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for child in std::fs::read_dir(from)? {
            let child = child?;
            copy_tree(&child.path(), &to.join(child.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(from, to).map(|_| ())
    }
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
        Ok(metadata) if metadata.is_dir() => cabinet_cartridge::archive::remove_tree(path),
        Ok(_) => std::fs::remove_file(path),
    }
}

fn reg(args: &[&str]) -> bool {
    crate::process::command("reg")
        .args(args)
        .output()
        .is_ok_and(|output| output.status.success())
}

fn record(dir: &Path, index: usize, touched: &Touched) -> crate::error::Result<Record> {
    let backup_name = index.to_string();
    let backup = dir.join(BACKUPS_DIR).join(&backup_name);
    match touched {
        Touched::Path(path) => {
            let existed = std::fs::symlink_metadata(path).is_ok();
            if existed {
                copy_tree(path, &backup)
                    .map_err(|err| format!("cannot back up {}: {err}", path.display()))?;
            }
            Ok(Record::Path {
                path: path.clone(),
                existed,
                backup: existed.then_some(backup_name),
            })
        }
        Touched::Registry(key) => {
            std::fs::create_dir_all(dir.join(BACKUPS_DIR))?;
            let file = backup.with_extension("reg");
            let existed = reg(&["export", key, &file.to_string_lossy(), "/y"]);
            Ok(Record::Registry {
                key: key.clone(),
                existed,
                backup: existed.then(|| format!("{backup_name}.reg")),
            })
        }
    }
}

pub fn snapshot(dir: &Path, touched: &[Touched]) -> crate::error::Result<()> {
    let mut snapshot = load(dir)?;
    for item in touched {
        if snapshot.records.iter().any(|record| record.covers(item)) {
            continue;
        }
        let index = snapshot.records.len();
        snapshot.records.push(record(dir, index, item)?);
        save(dir, &snapshot)?;
    }
    save(dir, &snapshot)
}

pub fn restore(dir: &Path) -> crate::error::Result<Vec<String>> {
    let snapshot = load(dir)?;
    let mut problems = Vec::new();
    for record in snapshot.records.iter().rev() {
        match record {
            Record::Path {
                path,
                existed,
                backup,
            } => {
                if let Err(err) = remove_any(path) {
                    problems.push(format!("cannot remove {}: {err}", path.display()));
                    continue;
                }
                if let (true, Some(backup)) = (existed, backup) {
                    if let Err(err) = copy_tree(&dir.join(BACKUPS_DIR).join(backup), path) {
                        problems.push(format!("cannot restore {}: {err}", path.display()));
                    }
                }
            }
            Record::Registry {
                key,
                existed,
                backup,
            } => {
                reg(&["delete", key, "/f"]);
                if let (true, Some(backup)) = (existed, backup) {
                    let file = dir.join(BACKUPS_DIR).join(backup);
                    if !reg(&["import", &file.to_string_lossy()]) {
                        problems.push(format!("cannot restore the registry key {key}"));
                    }
                }
            }
        }
    }
    for problem in &problems {
        log::warn!("footprint restore: {problem}");
    }
    Ok(problems)
}

pub fn forget(dir: &Path) -> std::io::Result<()> {
    cabinet_cartridge::archive::remove_tree(dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cartridge::testing::Scratch;

    #[test]
    fn restores_changed_files_and_removes_created_ones() {
        let scratch = Scratch::new("footprint");
        let existing = scratch.path("game/config.ini");
        let created = scratch.path("game/new.txt");
        let tree = scratch.path("game/saves");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::write(&existing, b"original").unwrap();
        std::fs::write(tree.join("slot1"), b"save").unwrap();
        let dir = scratch.path("footprint");
        let touched = [
            Touched::Path(existing.clone()),
            Touched::Path(created.clone()),
            Touched::Path(tree.clone()),
        ];
        snapshot(&dir, &touched).unwrap();

        std::fs::write(&existing, b"changed").unwrap();
        std::fs::write(&created, b"made by setup").unwrap();
        std::fs::write(tree.join("slot2"), b"new").unwrap();
        std::fs::remove_file(tree.join("slot1")).unwrap();
        snapshot(&dir, &touched).unwrap();

        assert!(restore(&dir).unwrap().is_empty());
        assert_eq!(std::fs::read(&existing).unwrap(), b"original");
        assert!(!created.exists());
        assert_eq!(std::fs::read(tree.join("slot1")).unwrap(), b"save");
        assert!(!tree.join("slot2").exists());
        forget(&dir).unwrap();
        assert!(!dir.exists());
        assert!(restore(&dir).unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn restores_registry_keys() {
        let scratch = Scratch::new("footprint-reg");
        let existing = format!(r"HKCU\Software\CabinetFootprintTest{}", std::process::id());
        let created = format!("{existing}New");
        let query = |key: &str| {
            crate::process::command("reg")
                .args(["query", key, "/v", "x"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        };
        assert!(reg(&["add", &existing, "/v", "x", "/d", "original", "/f"]));
        reg(&["delete", &created, "/f"]);
        let dir = scratch.path("footprint");
        snapshot(
            &dir,
            &[
                Touched::Registry(existing.clone()),
                Touched::Registry(created.clone()),
            ],
        )
        .unwrap();
        assert!(reg(&["add", &existing, "/v", "x", "/d", "changed", "/f"]));
        assert!(reg(&["add", &created, "/v", "x", "/d", "new", "/f"]));
        assert!(restore(&dir).unwrap().is_empty());
        assert!(query(&existing).unwrap().contains("original"));
        assert!(query(&created).is_none());
        reg(&["delete", &existing, "/f"]);
    }

    #[test]
    fn a_later_snapshot_adds_only_new_touches() {
        let scratch = Scratch::new("footprint-merge");
        let first = scratch.path("a.txt");
        let second = scratch.path("b.txt");
        std::fs::write(&first, b"one").unwrap();
        let dir = scratch.path("footprint");
        snapshot(&dir, &[Touched::Path(first.clone())]).unwrap();
        std::fs::write(&first, b"changed").unwrap();
        snapshot(
            &dir,
            &[Touched::Path(first.clone()), Touched::Path(second.clone())],
        )
        .unwrap();
        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.records.len(), 2);
        std::fs::write(&second, b"two").unwrap();
        restore(&dir).unwrap();
        assert_eq!(std::fs::read(&first).unwrap(), b"one");
        assert!(!second.exists());
    }
}
