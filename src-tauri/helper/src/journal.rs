use crate::protocol::{Change, Entry, Scope};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const FILE: &str = "journal.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct Stored {
    next: u64,
    entries: Vec<Entry>,
}

pub struct Journal {
    path: PathBuf,
    stored: Stored,
}

fn private(options: &mut std::fs::OpenOptions) -> &mut std::fs::OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
}

impl Journal {
    pub fn open(dir: &Path) -> Result<Self, String> {
        let path = dir.join(FILE);
        let stored = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str(&text) {
                Ok(stored) => stored,
                Err(err) => {
                    let aside = path.with_extension("corrupt");
                    log::error!(
                        "the journal is unreadable ({err}); keeping it as {}",
                        aside.display()
                    );
                    std::fs::rename(&path, &aside)
                        .map_err(|err| format!("cannot set the journal aside: {err}"))?;
                    Stored::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Stored::default(),
            Err(err) => return Err(format!("cannot read {}: {err}", path.display())),
        };
        Ok(Self { path, stored })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.stored.entries
    }

    pub fn entry(&self, id: &str) -> Option<&Entry> {
        self.stored.entries.iter().find(|entry| entry.id == id)
    }

    fn save(&self) -> Result<(), String> {
        let temp = self.path.with_extension("tmp");
        let _ = std::fs::remove_file(&temp);
        let written = (|| {
            let mut file =
                private(std::fs::OpenOptions::new().write(true).create_new(true)).open(&temp)?;
            file.write_all(&serde_json::to_vec_pretty(&self.stored)?)?;
            file.sync_all()?;
            std::fs::rename(&temp, &self.path)
        })();
        written.map_err(|err: std::io::Error| {
            let _ = std::fs::remove_file(&temp);
            format!("cannot write {}: {err}", self.path.display())
        })
    }

    pub fn add(
        &mut self,
        owner: &str,
        scope: Scope,
        change: Change,
        now: u64,
    ) -> Result<Entry, String> {
        self.stored.next += 1;
        let entry = Entry {
            id: self.stored.next.to_string(),
            owner: owner.to_string(),
            scope,
            change,
            applied_at: now,
        };
        self.stored.entries.push(entry.clone());
        if let Err(err) = self.save() {
            self.stored.entries.pop();
            return Err(err);
        }
        Ok(entry)
    }

    pub fn remove(&mut self, id: &str) -> Result<(), String> {
        let Some(index) = self.stored.entries.iter().position(|entry| entry.id == id) else {
            return Ok(());
        };
        let entry = self.stored.entries.remove(index);
        if let Err(err) = self.save() {
            self.stored.entries.insert(index, entry);
            return Err(err);
        }
        Ok(())
    }
}
