use super::registry::{self, Admission, CartridgeRef, Entry, Registry};
use cabinet_cartridge::archive::{self, Inspected};
use cabinet_cartridge::manifest::MANIFEST_PATH;
use cabinet_cartridge::{Manifest, Platform, Version};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub const STORE_DIR: &str = "store";
pub const DATA_DIR: &str = "cartridges";
pub const REGISTRY_FILE: &str = "registry.json";
pub const FOOTPRINT_DIR: &str = "footprint";

pub struct Staged {
    pub inspected: Inspected,
    pub cartridge: CartridgeRef,
    pub held: bool,
    unpacked_now: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Installed {
    pub cartridge: CartridgeRef,
    pub version: Version,
    pub digest: String,
    pub new: bool,
}

pub struct Store {
    root: PathBuf,
    registry: Registry,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> crate::error::Result<Self> {
        let root = root.into();
        let registry = Registry::load(&root.join(REGISTRY_FILE))?;
        Ok(Self { root, registry })
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn entry(&self, cartridge: &CartridgeRef) -> crate::error::Result<&Entry> {
        self.registry
            .entry(cartridge)
            .ok_or_else(|| format!("{cartridge} is not installed").into())
    }

    pub fn unpacked(&self, digest: &str) -> PathBuf {
        self.root.join(STORE_DIR).join(digest)
    }

    pub fn archive(&self, digest: &str) -> PathBuf {
        self.root
            .join(STORE_DIR)
            .join(format!("{digest}.{}", archive::EXTENSION))
    }

    pub fn data_dir(&self, cartridge: &CartridgeRef) -> PathBuf {
        self.root
            .join(DATA_DIR)
            .join(&cartridge.publisher)
            .join(&cartridge.id)
    }

    pub fn footprint_dir(&self, cartridge: &CartridgeRef) -> PathBuf {
        self.root
            .join(FOOTPRINT_DIR)
            .join(&cartridge.publisher)
            .join(&cartridge.id)
    }

    pub fn manifest(&self, digest: &str) -> crate::error::Result<Manifest> {
        let path = self.unpacked(digest).join(MANIFEST_PATH);
        let text = std::fs::read_to_string(&path)
            .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
        Ok(Manifest::parse(&text, None)?)
    }

    pub fn active_digest(&self, cartridge: &CartridgeRef) -> crate::error::Result<String> {
        self.entry(cartridge)?
            .active
            .clone()
            .ok_or_else(|| format!("{cartridge} has no active version").into())
    }

    fn save(&self) -> crate::error::Result<()> {
        self.registry.save(&self.root.join(REGISTRY_FILE))
    }

    pub fn stage(
        &mut self,
        cart: &Path,
        cabinet: &Version,
        platform: Platform,
        now: u64,
    ) -> crate::error::Result<Staged> {
        let inspected = archive::inspect(cart, Some(cabinet))?;
        let manifest = &inspected.manifest;
        if !manifest.supports(platform) {
            return Err(format!(
                "{} is not available for this machine ({platform})",
                manifest.title
            )
            .into());
        }
        let cartridge = CartridgeRef::new(inspected.publisher.as_ref(), &manifest.id);
        let admission = match self.registry.entry_mut(&cartridge) {
            Some(entry) => entry.admit(&manifest.version, &inspected.digest, now),
            None => Admission::New,
        };
        if let Admission::Conflict { held_digest } = admission {
            self.save()?;
            return Err(format!(
                "refusing {cartridge} {}: this publisher already released a different build of that version ({})",
                manifest.version,
                &held_digest[..12.min(held_digest.len())]
            )
            .into());
        }

        let dest = self.unpacked(&inspected.digest);
        let unpacked_now = !dest.exists();
        if unpacked_now {
            std::fs::create_dir_all(self.root.join(STORE_DIR))?;
            archive::unpack(cart, &dest, &inspected)?;
        }
        let kept = self.archive(&inspected.digest);
        if !kept.exists() {
            let partial = kept.with_extension("partial");
            let copied =
                std::fs::copy(cart, &partial).and_then(|_| std::fs::rename(&partial, &kept));
            if let Err(err) = copied {
                let _ = std::fs::remove_file(&partial);
                if unpacked_now {
                    let _ = archive::remove_tree(&dest);
                }
                return Err(format!("cannot keep the cartridge file: {err}").into());
            }
        }
        Ok(Staged {
            held: admission == Admission::AlreadyHeld,
            inspected,
            cartridge,
            unpacked_now,
        })
    }

    pub fn discard(&mut self, staged: &Staged) {
        if staged.unpacked_now && !self.is_referenced(&staged.inspected.digest) {
            self.remove_digest(&staged.inspected.digest);
        }
    }

    pub fn register(
        &mut self,
        staged: &Staged,
        now: u64,
        busy: bool,
    ) -> crate::error::Result<Installed> {
        let manifest = &staged.inspected.manifest;
        let digest = &staged.inspected.digest;
        let entry = self
            .registry
            .entry_or_insert(&staged.cartridge, staged.inspected.publisher.as_ref());
        let new = entry.held(digest).is_none();
        entry.hold(manifest.version.clone(), digest, now);
        if !busy || entry.active.is_none() {
            entry.settle_active();
        }
        self.save()?;
        Ok(Installed {
            cartridge: staged.cartridge.clone(),
            version: manifest.version.clone(),
            digest: digest.clone(),
            new,
        })
    }

    pub fn update(
        &mut self,
        cartridge: &CartridgeRef,
        change: impl FnOnce(&mut Entry) -> crate::error::Result<()>,
    ) -> crate::error::Result<()> {
        let entry = self
            .registry
            .entry_mut(cartridge)
            .ok_or_else(|| format!("{cartridge} is not installed"))?;
        change(entry)?;
        self.save()
    }

    pub fn set_settings(
        &mut self,
        cartridge: &CartridgeRef,
        settings: Map<String, Value>,
    ) -> crate::error::Result<()> {
        let manifest = self.manifest(&self.active_digest(cartridge)?)?;
        registry::check_settings(&manifest, &settings)?;
        self.update(cartridge, |entry| {
            entry.settings = settings;
            Ok(())
        })
    }

    pub fn settle(&mut self) -> crate::error::Result<()> {
        for entry in &mut self.registry.entries {
            entry.settle_active();
        }
        self.save()
    }

    pub fn prune(
        &mut self,
        now: u64,
        in_use: &BTreeSet<String>,
    ) -> crate::error::Result<Vec<String>> {
        let mut dropped = Vec::new();
        for entry in &mut self.registry.entries {
            dropped.extend(entry.prune(now, in_use));
        }
        self.save()?;
        let referenced = self.registry.referenced_digests();
        let removed: Vec<String> = dropped
            .into_iter()
            .filter(|digest| !referenced.contains(digest) && !in_use.contains(digest))
            .collect();
        for digest in &removed {
            self.remove_digest(digest);
        }
        Ok(removed)
    }

    pub fn remove_entry(&mut self, cartridge: &CartridgeRef) -> crate::error::Result<Entry> {
        let index = self
            .registry
            .entries
            .iter()
            .position(|entry| &entry.cartridge == cartridge)
            .ok_or_else(|| format!("{cartridge} is not installed"))?;
        let entry = self.registry.entries.remove(index);
        self.save()?;
        let data = self.data_dir(cartridge);
        archive::remove_tree(&data)
            .map_err(|err| format!("cannot remove {}: {err}", data.display()))?;
        let referenced = self.registry.referenced_digests();
        for held in &entry.versions {
            if !referenced.contains(&held.digest) {
                self.remove_digest(&held.digest);
            }
        }
        Ok(entry)
    }

    fn is_referenced(&self, digest: &str) -> bool {
        self.registry.referenced_digests().contains(digest)
    }

    fn remove_digest(&self, digest: &str) {
        let dir = self.unpacked(digest);
        if let Err(err) = archive::remove_tree(&dir) {
            log::warn!("cannot remove {}: {err}", dir.display());
        }
        let kept = self.archive(digest);
        if let Err(err) = std::fs::remove_file(&kept) {
            if err.kind() != std::io::ErrorKind::NotFound {
                log::warn!("cannot remove {}: {err}", kept.display());
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
