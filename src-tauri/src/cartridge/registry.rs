use cabinet_cartridge::manifest::{Capabilities, Manifest, SettingType};
use cabinet_cartridge::{Publisher, Version};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const UNSIGNED: &str = "unsigned";
pub const ROOM_RETENTION_MS: u64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CartridgeRef {
    pub publisher: String,
    pub id: String,
}

impl CartridgeRef {
    pub fn new(publisher: Option<&Publisher>, id: impl Into<String>) -> Self {
        Self {
            publisher: publisher
                .map(Publisher::fingerprint)
                .unwrap_or_else(|| UNSIGNED.to_string()),
            id: id.into(),
        }
    }
}

impl std::fmt::Display for CartridgeRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} by {}", self.id, self.publisher)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Held {
    pub version: Version,
    pub digest: String,
    pub fetched_at: u64,
    #[serde(default)]
    pub last_used_by_room_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflict {
    pub version: Version,
    pub digest: String,
    pub held_digest: String,
    pub seen_at: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub cartridge: CartridgeRef,
    #[serde(default)]
    pub publisher_key: Option<Publisher>,
    #[serde(default)]
    pub versions: Vec<Held>,
    #[serde(default)]
    pub seen: BTreeMap<String, String>,
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub pinned: Option<String>,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub approved_capabilities: Option<Capabilities>,
    #[serde(default)]
    pub settings: Map<String, Value>,
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
    #[serde(default)]
    pub trusted_ca: Option<super::catrust::TrustedCa>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admission {
    New,
    AlreadyHeld,
    Conflict { held_digest: String },
}

impl Entry {
    pub fn new(cartridge: CartridgeRef, publisher_key: Option<Publisher>) -> Self {
        Self {
            cartridge,
            publisher_key,
            versions: Vec::new(),
            seen: BTreeMap::new(),
            active: None,
            pinned: None,
            private: false,
            approved_capabilities: None,
            settings: Map::new(),
            conflicts: Vec::new(),
            trusted_ca: None,
        }
    }

    pub fn held(&self, digest: &str) -> Option<&Held> {
        self.versions.iter().find(|held| held.digest == digest)
    }

    pub fn newest(&self) -> Option<&Held> {
        self.versions
            .iter()
            .max_by(|a, b| a.version.cmp(&b.version))
    }

    pub fn active(&self) -> Option<&Held> {
        self.active.as_deref().and_then(|digest| self.held(digest))
    }

    pub fn admit(&mut self, version: &Version, digest: &str, now: u64) -> Admission {
        match self.seen.get(&version.to_string()) {
            Some(seen) if seen == digest => {
                if self.held(digest).is_some() {
                    Admission::AlreadyHeld
                } else {
                    Admission::New
                }
            }
            Some(seen) => {
                let held_digest = seen.clone();
                self.conflicts.push(Conflict {
                    version: version.clone(),
                    digest: digest.to_string(),
                    held_digest: held_digest.clone(),
                    seen_at: now,
                });
                Admission::Conflict { held_digest }
            }
            None => Admission::New,
        }
    }

    pub fn hold(&mut self, version: Version, digest: &str, now: u64) {
        self.seen.insert(version.to_string(), digest.to_string());
        if self.held(digest).is_none() {
            self.versions.push(Held {
                version,
                digest: digest.to_string(),
                fetched_at: now,
                last_used_by_room_at: None,
            });
        }
    }

    pub fn settle_active(&mut self) {
        let pinned = self
            .pinned
            .as_deref()
            .filter(|digest| self.held(digest).is_some());
        self.active = pinned
            .map(str::to_string)
            .or_else(|| self.newest().map(|held| held.digest.clone()));
    }

    pub fn mark_used_by_room(&mut self, digest: &str, now: u64) {
        if let Some(held) = self.versions.iter_mut().find(|held| held.digest == digest) {
            held.last_used_by_room_at = Some(now);
        }
    }

    pub fn retained(&self, now: u64, in_use: &BTreeSet<String>) -> BTreeSet<String> {
        let mut keep = BTreeSet::new();
        keep.extend(self.active.clone());
        keep.extend(self.pinned.clone());
        if let Some(active) = self.active() {
            if let Some(previous) = self
                .versions
                .iter()
                .filter(|held| held.version < active.version)
                .max_by(|a, b| a.version.cmp(&b.version))
            {
                keep.insert(previous.digest.clone());
            }
        }
        for held in &self.versions {
            if in_use.contains(&held.digest)
                || held
                    .last_used_by_room_at
                    .is_some_and(|used| now.saturating_sub(used) <= ROOM_RETENTION_MS)
            {
                keep.insert(held.digest.clone());
            }
        }
        keep
    }

    pub fn prune(&mut self, now: u64, in_use: &BTreeSet<String>) -> Vec<String> {
        let keep = self.retained(now, in_use);
        let (kept, dropped): (Vec<_>, Vec<_>) = self
            .versions
            .drain(..)
            .partition(|held| keep.contains(&held.digest));
        self.versions = kept;
        dropped.into_iter().map(|held| held.digest).collect()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub publishers: Vec<super::trust::TrustedPublisher>,
}

impl Registry {
    pub fn load(path: &Path) -> crate::error::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(serde_json::from_str(&text)?),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err.into()),
        }
    }

    pub fn save(&self, path: &Path) -> crate::error::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let partial = path.with_extension("json.partial");
        std::fs::write(&partial, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&partial, path)?;
        Ok(())
    }

    pub fn entry(&self, cartridge: &CartridgeRef) -> Option<&Entry> {
        self.entries
            .iter()
            .find(|entry| &entry.cartridge == cartridge)
    }

    pub fn entry_mut(&mut self, cartridge: &CartridgeRef) -> Option<&mut Entry> {
        self.entries
            .iter_mut()
            .find(|entry| &entry.cartridge == cartridge)
    }

    pub fn entry_or_insert(
        &mut self,
        cartridge: &CartridgeRef,
        publisher_key: Option<&Publisher>,
    ) -> &mut Entry {
        if let Some(index) = self
            .entries
            .iter()
            .position(|entry| &entry.cartridge == cartridge)
        {
            return &mut self.entries[index];
        }
        self.entries
            .push(Entry::new(cartridge.clone(), publisher_key.cloned()));
        self.entries.last_mut().expect("just pushed")
    }

    pub fn referenced_digests(&self) -> BTreeSet<String> {
        self.entries
            .iter()
            .flat_map(|entry| entry.versions.iter().map(|held| held.digest.clone()))
            .collect()
    }
}

pub fn check_settings(manifest: &Manifest, settings: &Map<String, Value>) -> Result<(), String> {
    let mut problems = Vec::new();
    for (key, value) in settings {
        let Some(setting) = manifest.setting(key) else {
            problems.push(format!("\"{key}\" is not a setting of this cartridge"));
            continue;
        };
        let ok = match setting.kind {
            SettingType::Bool => value.is_boolean(),
            SettingType::Choice => value
                .as_str()
                .is_some_and(|choice| setting.options.iter().any(|option| option == choice)),
            SettingType::Dir | SettingType::File | SettingType::String => value.is_string(),
        };
        if !ok {
            problems.push(format!("{} has an invalid value", setting.label));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

pub fn missing_required(manifest: &Manifest, settings: &Map<String, Value>) -> Vec<String> {
    manifest
        .settings
        .iter()
        .filter(|setting| setting.required)
        .filter(|setting| match settings.get(&setting.key) {
            None | Some(Value::Null) => true,
            Some(Value::String(text)) => text.trim().is_empty(),
            Some(_) => false,
        })
        .map(|setting| setting.label.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v(value: &str) -> Version {
        value.parse().unwrap()
    }

    fn entry() -> Entry {
        Entry::new(CartridgeRef::new(None, "test"), None)
    }

    #[test]
    fn unsigned_cartridges_share_the_unsigned_publisher() {
        let cartridge = CartridgeRef::new(None, "test");
        assert_eq!(cartridge.publisher, UNSIGNED);
        assert_eq!(cartridge.to_string(), "test by unsigned");
    }

    #[test]
    fn one_version_is_one_build() {
        let mut entry = entry();
        assert_eq!(entry.admit(&v("1.0.0"), "aaa", 1), Admission::New);
        entry.hold(v("1.0.0"), "aaa", 1);
        assert_eq!(entry.admit(&v("1.0.0"), "aaa", 2), Admission::AlreadyHeld);
        assert_eq!(
            entry.admit(&v("1.0.0"), "bbb", 3),
            Admission::Conflict {
                held_digest: "aaa".to_string()
            }
        );
        assert_eq!(entry.conflicts.len(), 1);
        assert_eq!(entry.conflicts[0].digest, "bbb");
        entry.versions.clear();
        assert_eq!(entry.admit(&v("1.0.0"), "aaa", 4), Admission::New);
        assert!(matches!(
            entry.admit(&v("1.0.0"), "ccc", 5),
            Admission::Conflict { .. }
        ));
    }

    #[test]
    fn active_follows_the_newest_unless_pinned() {
        let mut entry = entry();
        entry.hold(v("1.0.0"), "one", 1);
        entry.hold(v("1.2.0"), "two", 2);
        entry.hold(v("1.1.0"), "older", 3);
        entry.settle_active();
        assert_eq!(entry.active.as_deref(), Some("two"));
        entry.pinned = Some("one".to_string());
        entry.settle_active();
        assert_eq!(entry.active.as_deref(), Some("one"));
        entry.pinned = Some("gone".to_string());
        entry.settle_active();
        assert_eq!(entry.active.as_deref(), Some("two"));
    }

    #[test]
    fn retention_keeps_active_previous_pinned_recent_and_in_use() {
        let now = 100 * ROOM_RETENTION_MS;
        let mut entry = entry();
        for (version, digest) in [
            ("1.0.0", "v100"),
            ("1.1.0", "v110"),
            ("1.2.0", "v120"),
            ("1.3.0", "v130"),
            ("1.4.0", "v140"),
            ("2.0.0", "v200"),
        ] {
            entry.hold(v(version), digest, 1);
        }
        entry.settle_active();
        entry.mark_used_by_room("v110", now - ROOM_RETENTION_MS + 1);
        entry.mark_used_by_room("v120", now - ROOM_RETENTION_MS - 1);
        entry.pinned = Some("v100".to_string());
        entry.active = Some("v200".to_string());
        let in_use = BTreeSet::from(["v130".to_string()]);
        let dropped = entry.prune(now, &in_use);
        assert_eq!(dropped, vec!["v120".to_string()]);
        let kept: BTreeSet<_> = entry.versions.iter().map(|h| h.digest.as_str()).collect();
        assert_eq!(
            kept,
            BTreeSet::from(["v100", "v110", "v130", "v140", "v200"])
        );
        assert!(entry.seen.contains_key("1.2.0"));
    }

    #[test]
    fn the_registry_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("cabinet-registry-{}", std::process::id()));
        let path = dir.join("registry.json");
        assert_eq!(Registry::load(&path).unwrap(), Registry::default());
        let mut registry = Registry::default();
        let cartridge = CartridgeRef::new(None, "test");
        let entry = registry.entry_or_insert(&cartridge, None);
        entry.hold(v("0.1.0"), "abc", 7);
        entry
            .settings
            .insert("scratch_dir".to_string(), json!("/tmp/x"));
        entry.settle_active();
        registry.save(&path).unwrap();
        assert_eq!(Registry::load(&path).unwrap(), registry);
        assert_eq!(registry.entry_or_insert(&cartridge, None).versions.len(), 1);
        assert_eq!(registry.entries.len(), 1);
        assert_eq!(
            registry.referenced_digests(),
            BTreeSet::from(["abc".to_string()])
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn checks_settings_against_the_manifest() {
        let manifest = Manifest::parse(
            r#"
format = 1
id = "s"
version = "1.0.0"
title = "S"
min_cabinet = "0.1.0"
platforms = ["linux-x86_64"]
[programs.p.run]
linux-x86_64 = "files/p"
[lifecycle.session]
program = "p"
[[settings]]
key = "dir"
label = "Folder"
type = "dir"
required = true
[[settings]]
key = "fast"
label = "Fast"
type = "bool"
[[settings]]
key = "mode"
label = "Mode"
type = "choice"
options = ["a", "b"]
"#,
            None,
        )
        .unwrap();
        let good = Map::from_iter([
            ("dir".to_string(), json!("/games")),
            ("fast".to_string(), json!(true)),
            ("mode".to_string(), json!("b")),
        ]);
        assert!(check_settings(&manifest, &good).is_ok());
        assert!(missing_required(&manifest, &good).is_empty());
        let bad = Map::from_iter([
            ("dir".to_string(), json!(3)),
            ("mode".to_string(), json!("c")),
            ("nope".to_string(), json!("x")),
        ]);
        let err = check_settings(&manifest, &bad).unwrap_err();
        assert!(err.contains("Folder has an invalid value"), "{err}");
        assert!(err.contains("Mode has an invalid value"), "{err}");
        assert!(err.contains("\"nope\" is not a setting"), "{err}");
        let empty = Map::from_iter([("dir".to_string(), json!("  "))]);
        assert_eq!(missing_required(&manifest, &empty), vec!["Folder"]);
        assert_eq!(missing_required(&manifest, &Map::new()), vec!["Folder"]);
    }
}
