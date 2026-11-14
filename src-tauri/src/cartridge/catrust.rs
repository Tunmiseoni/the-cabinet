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

#[cfg(target_os = "linux")]
pub struct HelperTrust(pub super::privileged::SocketHelper);

#[cfg(target_os = "linux")]
impl HelperTrust {
    fn owner(sha256: &str) -> String {
        format!(
            "ca/{}",
            sha256.get(..16).unwrap_or(sha256).to_ascii_lowercase()
        )
    }
}

#[cfg(target_os = "linux")]
impl TrustStore for HelperTrust {
    fn trust(&self, ca: &Ca) -> Result<(), String> {
        let change = cabinet_helper::protocol::Change::TrustCa {
            certificate: ca::pem(&ca.der),
            names: ca.permitted.clone(),
            anchor: String::new(),
        };
        self.0
            .connect()?
            .apply(
                &Self::owner(&ca.sha256),
                cabinet_helper::protocol::Scope::Install,
                &change,
            )
            .map(|_| ())
    }

    fn untrust(&self, sha256: &str, _der: &[u8]) -> Result<(), String> {
        self.0.connect()?.forget(&Self::owner(sha256)).map(|_| ())
    }

    fn holds(&self, sha256: &str) -> Result<bool, String> {
        let owner = Self::owner(sha256);
        Ok(self
            .0
            .connect()?
            .status()?
            .iter()
            .any(|entry| entry.owner == owner))
    }
}

#[cfg(windows)]
pub struct UserStore(pub String);

#[cfg(windows)]
pub(crate) struct OpenStore(pub(crate) windows_sys::Win32::Security::Cryptography::HCERTSTORE);

#[cfg(windows)]
impl Drop for OpenStore {
    fn drop(&mut self) {
        unsafe { windows_sys::Win32::Security::Cryptography::CertCloseStore(self.0, 0) };
    }
}

#[cfg(windows)]
impl UserStore {
    pub const ROOT: &'static str = "Root";

    pub fn root() -> Self {
        Self(Self::ROOT.to_string())
    }

    pub(crate) fn open(&self) -> Result<OpenStore, String> {
        use windows_sys::Win32::Security::Cryptography::{
            CertOpenStore, CERT_STORE_PROV_SYSTEM_W, CERT_SYSTEM_STORE_CURRENT_USER,
        };
        let name = cabinet_helper::win::wide(&self.0);
        let handle = unsafe {
            CertOpenStore(
                CERT_STORE_PROV_SYSTEM_W,
                0,
                0,
                CERT_SYSTEM_STORE_CURRENT_USER,
                name.as_ptr().cast(),
            )
        };
        if handle.is_null() {
            return Err(cabinet_helper::win::last_error(&format!(
                "cannot open your \"{}\" certificate store",
                self.0
            )));
        }
        Ok(OpenStore(handle))
    }

    fn find(
        store: &OpenStore,
        sha256: &str,
    ) -> Option<*mut windows_sys::Win32::Security::Cryptography::CERT_CONTEXT> {
        use sha2::{Digest, Sha256};
        use windows_sys::Win32::Security::Cryptography::{
            CertDuplicateCertificateContext, CertEnumCertificatesInStore,
            CertFreeCertificateContext,
        };
        let mut previous = std::ptr::null();
        loop {
            let context = unsafe { CertEnumCertificatesInStore(store.0, previous) };
            if context.is_null() {
                return None;
            }
            let der = unsafe {
                std::slice::from_raw_parts(
                    (*context).pbCertEncoded,
                    (*context).cbCertEncoded as usize,
                )
            };
            if hex(&Sha256::digest(der)).eq_ignore_ascii_case(sha256) {
                let found = unsafe { CertDuplicateCertificateContext(context) };
                unsafe { CertFreeCertificateContext(context) };
                return Some(found);
            }
            previous = context;
        }
    }
}

#[cfg(windows)]
fn declined(code: u32) -> bool {
    code == windows_sys::Win32::Foundation::ERROR_CANCELLED || code == 0x8007_04C7
}

#[cfg(windows)]
impl TrustStore for UserStore {
    fn trust(&self, ca: &Ca) -> Result<(), String> {
        use windows_sys::Win32::Security::Cryptography::{
            CertAddEncodedCertificateToStore, CERT_STORE_ADD_NEW, CRYPT_E_EXISTS, X509_ASN_ENCODING,
        };
        let store = self.open()?;
        let added = unsafe {
            CertAddEncodedCertificateToStore(
                store.0,
                X509_ASN_ENCODING,
                ca.der.as_ptr(),
                ca.der.len() as u32,
                CERT_STORE_ADD_NEW,
                std::ptr::null_mut(),
            )
        };
        if added == 0 {
            let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            if declined(code) {
                return Err(
                    "Windows did not trust the certificate: the prompt was declined".to_string(),
                );
            }
            if code != CRYPT_E_EXISTS as u32 {
                return Err(format!(
                    "Windows did not trust the certificate: {}",
                    std::io::Error::from_raw_os_error(code as i32)
                ));
            }
        }
        if self.holds(&ca.sha256)? {
            Ok(())
        } else {
            Err("Windows did not keep the certificate".to_string())
        }
    }

    fn untrust(&self, sha256: &str, _der: &[u8]) -> Result<(), String> {
        use windows_sys::Win32::Security::Cryptography::CertDeleteCertificateFromStore;
        let store = self.open()?;
        let Some(found) = Self::find(&store, sha256) else {
            return Ok(());
        };
        if unsafe { CertDeleteCertificateFromStore(found) } == 0 {
            let code = unsafe { windows_sys::Win32::Foundation::GetLastError() };
            if declined(code) {
                return Err(
                    "Windows kept the certificate trusted: the prompt was declined".to_string(),
                );
            }
            return Err(format!(
                "cannot remove the certificate: {}",
                std::io::Error::from_raw_os_error(code as i32)
            ));
        }
        Ok(())
    }

    fn holds(&self, sha256: &str) -> Result<bool, String> {
        let store = self.open()?;
        Ok(match Self::find(&store, sha256) {
            Some(found) => {
                unsafe {
                    windows_sys::Win32::Security::Cryptography::CertFreeCertificateContext(found)
                };
                true
            }
            None => false,
        })
    }
}

pub fn system() -> Option<SharedTrustStore> {
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(Keychain))
    }
    #[cfg(target_os = "linux")]
    {
        Some(Arc::new(HelperTrust(super::privileged::system_socket())))
    }
    #[cfg(windows)]
    {
        Some(Arc::new(UserStore::root()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
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
