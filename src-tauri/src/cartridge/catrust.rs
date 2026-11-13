use super::context;
use super::registry::{CartridgeRef, Entry};
use super::store::Store;
use super::trust::Grant;
use cabinet_cartridge::ca::{self, Ca};
use cabinet_cartridge::manifest::{Manifest, Store as CaStore};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

pub trait TrustStore: Send + Sync {
    fn trust(&self, ca: &Ca) -> Result<(), String>;
    fn untrust(&self, sha256: &str, der: &[u8]) -> Result<(), String>;
    fn holds(&self, sha256: &str) -> Result<bool, String>;
}

pub type SharedTrustStore = Arc<dyn TrustStore>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedCa {
    pub sha256: String,
    pub subject: String,
    pub der: String,
    pub trusted_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaState {
    NotNeeded,
    Ready,
    Missing(String),
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len() / 2)
        .filter_map(|at| u8::from_str_radix(text.get(at * 2..at * 2 + 2)?, 16).ok())
        .collect()
}

fn wants_system(manifest: &Manifest) -> bool {
    manifest
        .capabilities
        .trust_ca
        .as_ref()
        .is_some_and(|ca| ca.stores.contains(&CaStore::System))
}

pub fn declared(manifest: &Manifest, cartridge_dir: &Path, now: u64) -> Result<Option<Ca>, String> {
    let Some(declared) = &manifest.capabilities.trust_ca else {
        return Ok(None);
    };
    let refused = |reason: String| {
        format!(
            "{}'s certificate authority is refused: {reason}",
            manifest.title
        )
    };
    let file = manifest
        .asset(&declared.asset)
        .and_then(|asset| asset.file.as_ref())
        .ok_or_else(|| refused(format!("asset \"{}\" is not bundled", declared.asset)))?;
    let bytes = std::fs::read(context::bundled(cartridge_dir, file))
        .map_err(|err| refused(format!("cannot read {file}: {err}")))?;
    ca::check(&bytes, &declared.permitted_names, (now / 1000) as i64)
        .map(Some)
        .map_err(refused)
}

pub fn state(
    entry: &Entry,
    manifest: &Manifest,
    cartridge_dir: &Path,
    supported: bool,
    now: u64,
) -> CaState {
    if !supported || !wants_system(manifest) {
        return CaState::NotNeeded;
    }
    match declared(manifest, cartridge_dir, now) {
        Err(err) => CaState::Missing(err),
        Ok(None) => CaState::NotNeeded,
        Ok(Some(ca)) => match &entry.trusted_ca {
            Some(trusted) if trusted.sha256 == ca.sha256 => CaState::Ready,
            _ => CaState::Missing(format!(
                "{}'s certificate authority is not trusted on this machine yet. Approve it first.",
                manifest.title
            )),
        },
    }
}

fn wanted(store: &Store, cartridge: &CartridgeRef, now: u64) -> Option<Ca> {
    let entry = store.entry(cartridge).ok()?;
    let digest = entry.active.as_deref()?;
    let manifest = store.manifest(digest).ok()?;
    if !wants_system(&manifest) || store.grant(entry) != Grant::Granted {
        return None;
    }
    match declared(&manifest, &store.unpacked(digest), now) {
        Ok(ca) => ca,
        Err(err) => {
            log::warn!("{err}");
            None
        }
    }
}

fn shared(store: &Store, cartridge: &CartridgeRef, sha256: &str) -> bool {
    store.registry().entries.iter().any(|entry| {
        &entry.cartridge != cartridge
            && entry
                .trusted_ca
                .as_ref()
                .is_some_and(|trusted| trusted.sha256 == sha256)
    })
}

fn record(
    store: &mut Store,
    cartridge: &CartridgeRef,
    trusted: Option<TrustedCa>,
) -> crate::error::Result<()> {
    store.update(cartridge, |entry| {
        entry.trusted_ca = trusted;
        Ok(())
    })
}

pub fn release(
    store: &mut Store,
    cartridge: &CartridgeRef,
    trust: &dyn TrustStore,
) -> crate::error::Result<()> {
    let Some(old) = store.entry(cartridge)?.trusted_ca.clone() else {
        return Ok(());
    };
    if !shared(store, cartridge, &old.sha256) {
        trust.untrust(&old.sha256, &unhex(&old.der))?;
    }
    record(store, cartridge, None)?;
    log::info!("{cartridge}: no longer trusts {}", old.subject);
    Ok(())
}

pub fn reconcile(
    store: &mut Store,
    cartridge: &CartridgeRef,
    trust: &dyn TrustStore,
    add: bool,
    now: u64,
) -> crate::error::Result<()> {
    let current = store.entry(cartridge)?.trusted_ca.clone();
    let wanted = wanted(store, cartridge, now);
    if let (Some(current), Some(wanted)) = (&current, &wanted) {
        if current.sha256 == wanted.sha256 {
            if add && !trust.holds(&wanted.sha256)? {
                trust.trust(wanted)?;
            }
            return Ok(());
        }
    }
    release(store, cartridge, trust)?;
    let Some(ca) = wanted.filter(|_| add) else {
        return Ok(());
    };
    record(
        store,
        cartridge,
        Some(TrustedCa {
            sha256: ca.sha256.clone(),
            subject: ca.subject.clone(),
            der: hex(&ca.der),
            trusted_at: now,
        }),
    )?;
    let present = shared(store, cartridge, &ca.sha256) && trust.holds(&ca.sha256)?;
    if !present {
        if let Err(err) = trust.trust(&ca) {
            record(store, cartridge, None)?;
            return Err(err.into());
        }
    }
    log::info!("{cartridge}: trusts {} ({})", ca.subject, ca.sha256);
    Ok(())
}

pub fn reconcile_all(
    store: &mut Store,
    trust: &dyn TrustStore,
    add: bool,
    now: u64,
) -> Vec<String> {
    let cartridges: Vec<CartridgeRef> = store
        .registry()
        .entries
        .iter()
        .map(|entry| entry.cartridge.clone())
        .collect();
    cartridges
        .iter()
        .filter_map(|cartridge| {
            reconcile(store, cartridge, trust, add, now)
                .err()
                .map(|err| format!("{cartridge}: {err}"))
        })
        .collect()
}

#[cfg(target_os = "macos")]
pub struct Keychain;

#[cfg(target_os = "macos")]
impl Keychain {
    const SECURITY: &'static str = "/usr/bin/security";

    fn security(args: &[&std::ffi::OsStr]) -> Result<String, String> {
        let output = crate::process::command(Self::SECURITY)
            .args(args)
            .output()
            .map_err(|err| format!("cannot run {}: {err}", Self::SECURITY))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
        }
    }

    fn login() -> Result<std::path::PathBuf, String> {
        let listed = Self::security(&["login-keychain".as_ref()])?;
        listed
            .split('"')
            .nth(1)
            .map(std::path::PathBuf::from)
            .ok_or_else(|| "cannot find the login keychain".to_string())
    }

    fn with_file<T>(
        sha256: &str,
        der: &[u8],
        work: impl FnOnce(&Path) -> Result<T, String>,
    ) -> Result<T, String> {
        let file = std::env::temp_dir().join(format!(
            "the-cabinet-ca-{}-{}.cer",
            std::process::id(),
            &sha256[..16.min(sha256.len())]
        ));
        std::fs::write(&file, der)
            .map_err(|err| format!("cannot write {}: {err}", file.display()))?;
        let result = work(&file);
        let _ = std::fs::remove_file(&file);
        result
    }

    fn delete(sha256: &str, keychain: &Path) -> Result<(), String> {
        Self::security(&[
            "delete-certificate".as_ref(),
            "-Z".as_ref(),
            sha256.as_ref(),
            keychain.as_os_str(),
        ])
        .map(|_| ())
    }
}

#[cfg(target_os = "macos")]
impl TrustStore for Keychain {
    fn trust(&self, ca: &Ca) -> Result<(), String> {
        let keychain = Self::login()?;
        let added = Self::with_file(&ca.sha256, &ca.der, |file| {
            Self::security(&[
                "add-trusted-cert".as_ref(),
                "-r".as_ref(),
                "trustRoot".as_ref(),
                "-p".as_ref(),
                "ssl".as_ref(),
                "-k".as_ref(),
                keychain.as_os_str(),
                file.as_os_str(),
            ])
        });
        if let Err(err) = added {
            if self.holds(&ca.sha256).unwrap_or(false) {
                if let Err(left) = Self::delete(&ca.sha256, &keychain) {
                    log::warn!("cannot remove the untrusted certificate again: {left}");
                }
            }
            return Err(format!("macOS did not trust the certificate: {err}"));
        }
        Ok(())
    }

    fn untrust(&self, sha256: &str, der: &[u8]) -> Result<(), String> {
        if !self.holds(sha256)? {
            return Ok(());
        }
        let keychain = Self::login()?;
        let removed = Self::with_file(sha256, der, |file| {
            Self::security(&["remove-trusted-cert".as_ref(), file.as_os_str()])
        });
        if let Err(err) = removed {
            if err.to_lowercase().contains("cancel") {
                return Err(format!("macOS kept the certificate trusted: {err}"));
            }
            log::warn!("removing the trust setting reported: {err}");
        }
        Self::delete(sha256, &keychain)
            .map_err(|err| format!("cannot delete the certificate from the keychain: {err}"))
    }

    fn holds(&self, sha256: &str) -> Result<bool, String> {
        let keychain = Self::login()?;
        let listed = Self::security(&[
            "find-certificate".as_ref(),
            "-a".as_ref(),
            "-Z".as_ref(),
            keychain.as_os_str(),
        ])?;
        Ok(listed
            .lines()
            .any(|line| line.starts_with("SHA-256 hash:") && line.contains(sha256)))
    }
}

pub fn system() -> Option<SharedTrustStore> {
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(Keychain))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sync::MutexExt;
    use std::sync::Mutex;

    #[derive(Default)]
    pub(crate) struct FakeTrustStore {
        pub held: Mutex<Vec<String>>,
        pub refuse: Mutex<bool>,
    }

    impl TrustStore for FakeTrustStore {
        fn trust(&self, ca: &Ca) -> Result<(), String> {
            if *self.refuse.lock_or_recover() {
                return Err("the user cancelled".to_string());
            }
            self.held.lock_or_recover().push(ca.sha256.clone());
            Ok(())
        }

        fn untrust(&self, sha256: &str, _: &[u8]) -> Result<(), String> {
            if *self.refuse.lock_or_recover() {
                return Err("the user cancelled".to_string());
            }
            self.held.lock_or_recover().retain(|held| held != sha256);
            Ok(())
        }

        fn holds(&self, sha256: &str) -> Result<bool, String> {
            Ok(self
                .held
                .lock_or_recover()
                .iter()
                .any(|held| held == sha256))
        }
    }

    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 127, 128, 255];
        assert_eq!(hex(&bytes), "00017f80ff");
        assert_eq!(unhex(&hex(&bytes)), bytes);
    }
}
