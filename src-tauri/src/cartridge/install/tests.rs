use super::*;
use crate::cartridge::assets::tests::FakeFetch;
use crate::cartridge::catrust::tests::FakeTrustStore;
use crate::cartridge::catrust::CaState;
use crate::cartridge::testing::{build_cart, CartSpec, Scratch};
use crate::cartridge::trust::{Grant, Standing};
use cabinet_cartridge::digest;
use cabinet_cartridge::signature::KeySigner;
use serde_json::json;
use std::collections::BTreeMap;

const EXTRA: &str = r#"
[lifecycle.setup]
program = "main"
args = ["setup", "hello", "${setting:scratch_dir}"]

[lifecycle.uninstall]
program = "main"
args = ["uninstall", "bye", "${setting:scratch_dir}"]

[[settings]]
key = "scratch_dir"
label = "Scratch folder"
type = "dir"
required = true

[[touches]]
path = "${setting:scratch_dir}/touch-test.txt"
"#;

const UNIX: &str = r#"#!/bin/sh
case "$1" in
  setup) [ -f "$3/fail" ] && exit 3; printf '%s\n' "$2" > "$3/touch-test.txt" ;;
  uninstall) printf '%s\n' "$2" > "$3/uninstalled.txt" ;;
esac
exit 0
"#;

const WINDOWS: &str = "@echo off\r\n\
if \"%~1\"==\"setup\" goto setup\r\n\
if \"%~1\"==\"uninstall\" goto uninstall\r\n\
exit /b 0\r\n\
:setup\r\n\
if exist \"%~3\\fail\" goto fail\r\n\
>\"%~3\\touch-test.txt\" echo %~2\r\n\
exit /b 0\r\n\
:fail\r\n\
exit /b 3\r\n\
:uninstall\r\n\
>\"%~3\\uninstalled.txt\" echo %~2\r\n\
exit /b 0\r\n";

struct Fixture {
    scratch: Scratch,
    store: Store,
    fetch: FakeFetch,
    approval: Approval,
    now: u64,
    trust: Option<std::sync::Arc<FakeTrustStore>>,
}

impl Fixture {
    fn new(tag: &str) -> Self {
        let scratch = Scratch::new(tag);
        std::fs::create_dir_all(scratch.path("scratch")).unwrap();
        let store = Store::open(scratch.path("app")).unwrap();
        Self {
            scratch,
            store,
            fetch: FakeFetch::default(),
            approval: Approval::default(),
            now: 1,
            trust: None,
        }
    }

    fn cart(&self, version: &str, extra: &str) -> PathBuf {
        build_cart(
            &self.scratch,
            &CartSpec {
                version,
                extra,
                unix: UNIX,
                windows: WINDOWS,
                ..CartSpec::default()
            },
        )
    }

    fn settings(&self) -> Map<String, Value> {
        Map::from_iter([(
            "scratch_dir".to_string(),
            json!(self.scratch.path("scratch").to_string_lossy()),
        )])
    }

    fn install(
        &mut self,
        cart: &Path,
        settings: Option<Map<String, Value>>,
    ) -> crate::error::Result<Installed> {
        Installer {
            store: &mut self.store,
            cabinet: Version::new(0, 1, 15),
            platform: Platform::current().unwrap(),
            fetch: &self.fetch,
            log_dir: self.scratch.path("logs"),
            now: self.now,
            busy: false,
            trust: self.trust.as_deref().map(|trust| trust as &dyn TrustStore),
        }
        .install(cart, settings, &self.approval)
    }

    fn touch_file(&self) -> PathBuf {
        self.scratch.path("scratch/touch-test.txt")
    }
}

fn read_trimmed(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap().trim().to_string()
}

#[test]
fn setup_runs_after_the_footprint_snapshot_and_uninstall_restores_it() {
    let mut fixture = Fixture::new("install-roundtrip");
    std::fs::write(fixture.touch_file(), b"before\n").unwrap();
    let cart = fixture.cart("0.1.0", EXTRA);
    let settings = fixture.settings();
    let installed = fixture.install(&cart, Some(settings.clone())).unwrap();
    assert_eq!(read_trimmed(&fixture.touch_file()), "hello");
    let entry = fixture.store.entry(&installed.cartridge).unwrap();
    assert_eq!(entry.settings, settings);
    assert!(fixture.store.data_dir(&installed.cartridge).is_dir());
    assert!(fixture
        .scratch
        .path("logs/cartridge-test-setup.log")
        .is_file());

    let problems = uninstall(
        &mut fixture.store,
        &installed.cartridge,
        Platform::current().unwrap(),
        &fixture.scratch.path("logs"),
        None,
        None,
    )
    .unwrap();
    assert!(problems.is_empty(), "{problems:?}");
    assert_eq!(read_trimmed(&fixture.touch_file()), "before");
    assert_eq!(
        read_trimmed(&fixture.scratch.path("scratch/uninstalled.txt")),
        "bye"
    );
    assert!(!fixture.store.data_dir(&installed.cartridge).exists());
    assert!(!fixture.store.footprint_dir(&installed.cartridge).exists());
    assert!(!fixture.store.unpacked(&installed.digest).exists());
    assert!(fixture.store.registry().entries.is_empty());
}

#[test]
fn an_update_keeps_the_settings_and_the_original_snapshot() {
    let mut fixture = Fixture::new("install-update");
    let settings = fixture.settings();
    let first = fixture.cart("0.1.0", EXTRA);
    let installed = fixture.install(&first, Some(settings)).unwrap();
    std::fs::write(fixture.touch_file(), b"edited by the user\n").unwrap();
    let second = fixture.cart("0.2.0", EXTRA);
    let updated = fixture.install(&second, None).unwrap();
    assert_eq!(updated.version, Version::new(0, 2, 0));
    assert_eq!(
        fixture.store.active_digest(&installed.cartridge).unwrap(),
        updated.digest
    );
    assert_eq!(read_trimmed(&fixture.touch_file()), "hello");
    uninstall(
        &mut fixture.store,
        &installed.cartridge,
        Platform::current().unwrap(),
        &fixture.scratch.path("logs"),
        None,
        None,
    )
    .unwrap();
    assert!(!fixture.touch_file().exists());
}

#[test]
fn a_failed_first_setup_rolls_everything_back() {
    let mut fixture = Fixture::new("install-fail");
    std::fs::write(fixture.scratch.path("scratch/fail"), b"").unwrap();
    std::fs::write(fixture.touch_file(), b"before\n").unwrap();
    let cart = fixture.cart("0.1.0", EXTRA);
    let settings = fixture.settings();
    let err = fixture
        .install(&cart, Some(settings))
        .unwrap_err()
        .to_string();
    assert!(err.contains("failed (exit 3)"), "{err}");
    let cartridge = CartridgeRef::new(None, "test");
    assert!(fixture.store.registry().entries.is_empty());
    assert!(!fixture.store.data_dir(&cartridge).exists());
    assert!(!fixture.store.footprint_dir(&cartridge).exists());
    assert_eq!(read_trimmed(&fixture.touch_file()), "before");
    let inspected = cabinet_cartridge::archive::inspect(&cart, None).unwrap();
    assert!(!fixture.store.unpacked(&inspected.digest).exists());
}

#[test]
fn install_needs_the_required_settings_first() {
    let mut fixture = Fixture::new("install-settings");
    let cart = fixture.cart("0.1.0", EXTRA);
    let err = fixture.install(&cart, None).unwrap_err().to_string();
    assert!(
        err.contains("needs these settings first: Scratch folder"),
        "{err}"
    );
    let bad = Map::from_iter([("scratch_dir".to_string(), json!(7))]);
    assert!(fixture.install(&cart, Some(bad)).is_err());
    assert!(fixture.store.registry().entries.is_empty());
    assert!(!fixture.touch_file().exists());
}

#[test]
fn install_downloads_the_assets_for_this_platform() {
    let mut fixture = Fixture::new("install-assets");
    let body = b"core".to_vec();
    let sha = digest::sha256_bytes(&body);
    let downloads: String = crate::cartridge::testing::ALL_PLATFORMS
        .iter()
        .map(|platform| {
            format!(
                "[assets.download.{platform}]\nurl = \"https://example.com/{platform}/core\"\nsha256 = \"{sha}\"\n"
            )
        })
        .collect();
    let extra = format!("[[assets]]\nid = \"core\"\n{downloads}");
    let url = format!("https://example.com/{}/core", Platform::current().unwrap());
    fixture.fetch = FakeFetch {
        files: BTreeMap::from([(url.clone(), body.clone())]),
        ..FakeFetch::default()
    };
    let cart = fixture.cart("0.1.0", &extra);
    let installed = fixture.install(&cart, None).unwrap();
    let asset = fixture
        .store
        .data_dir(&installed.cartridge)
        .join("assets/core");
    assert_eq!(std::fs::read(asset).unwrap(), body);
    assert_eq!(*fixture.fetch.fetched.lock().unwrap(), vec![url]);
}

const PRIVILEGED: &str = r#"
[lifecycle.service]
program = "main"

[capabilities.hosts]
names = ["cabinet-test.invalid"]
target = "service"
scope = "session"
"#;

const PRIVILEGED_MORE: &str = r#"
[lifecycle.service]
program = "main"

[capabilities.hosts]
names = ["cabinet-test.invalid", "more.cabinet-test.invalid"]
target = "service"
scope = "session"
"#;

fn signer() -> KeySigner {
    let (secret, _) = KeySigner::generate(None).unwrap();
    KeySigner::from_key_file(&secret, None).unwrap()
}

fn signed_cart(fixture: &Fixture, signer: &KeySigner, version: &str, extra: &str) -> PathBuf {
    build_cart(
        &fixture.scratch,
        &CartSpec {
            version,
            extra,
            signer: Some(signer),
            ..CartSpec::default()
        },
    )
}

fn trusting(name: &str) -> Approval {
    Approval {
        trust_publisher: Some(name.to_string()),
        capabilities: true,
    }
}

#[test]
fn an_unsigned_cartridge_installs_without_its_capabilities() {
    let mut fixture = Fixture::new("trust-unsigned");
    let cart = fixture.cart("0.1.0", PRIVILEGED);
    fixture.approval = Approval {
        trust_publisher: None,
        capabilities: true,
    };
    let installed = fixture.install(&cart, None).unwrap();
    assert_eq!(installed.capabilities, Grant::Withheld);
    let entry = fixture.store.entry(&installed.cartridge).unwrap();
    assert_eq!(entry.approved_capabilities, None);
    assert_eq!(fixture.store.grant(entry), Grant::Withheld);
    let err = fixture
        .store
        .approve_capabilities(&installed.cartridge)
        .unwrap_err()
        .to_string();
    assert!(err.contains("until its publisher is trusted"), "{err}");

    fixture.approval = trusting("Nobody");
    let second = fixture.cart("0.2.0", PRIVILEGED);
    let err = fixture.install(&second, None).unwrap_err().to_string();
    assert!(err.contains("no publisher to trust"), "{err}");
    assert!(fixture.store.registry().publishers.is_empty());
    let inspected = cabinet_cartridge::archive::inspect(&second, None).unwrap();
    assert!(!fixture.store.unpacked(&inspected.digest).exists());
}

#[test]
fn a_signed_cartridge_gets_its_capabilities_once_the_publisher_is_trusted() {
    let mut fixture = Fixture::new("trust-signed");
    let signer = signer();
    let key = signer.publisher();
    let cart = signed_cart(&fixture, &signer, "0.1.0", PRIVILEGED);

    let installed = fixture.install(&cart, None).unwrap();
    assert_eq!(installed.capabilities, Grant::Withheld);
    assert_eq!(
        fixture.store.registry().publisher_view(Some(&key)).standing,
        Standing::Unknown
    );

    fixture
        .store
        .trust_publisher(&key.fingerprint(), "Friend", 5)
        .unwrap();
    let entry = fixture.store.entry(&installed.cartridge).unwrap();
    assert_eq!(fixture.store.grant(entry), Grant::Withheld);
    fixture
        .store
        .approve_capabilities(&installed.cartridge)
        .unwrap();
    let entry = fixture.store.entry(&installed.cartridge).unwrap();
    assert_eq!(fixture.store.grant(entry), Grant::Granted);

    let reopened = Store::open(fixture.scratch.path("app")).unwrap();
    let view = reopened.registry().publisher_view(Some(&key));
    assert_eq!(view.standing, Standing::Trusted);
    assert_eq!(view.name.as_deref(), Some("Friend"));

    fixture.store.forget_publisher(&key.fingerprint()).unwrap();
    let entry = fixture.store.entry(&installed.cartridge).unwrap();
    assert_eq!(fixture.store.grant(entry), Grant::Withheld);
    assert!(fixture.store.forget_publisher(&key.fingerprint()).is_err());
    assert!(fixture
        .store
        .trust_publisher("0000000000000000", "X", 5)
        .is_err());
}

#[test]
fn trusting_at_install_grants_and_an_update_asks_only_when_capabilities_change() {
    let mut fixture = Fixture::new("trust-update");
    let signer = signer();
    let key = signer.publisher();
    let first = signed_cart(&fixture, &signer, "0.1.0", PRIVILEGED);

    fixture.approval = Approval {
        trust_publisher: Some("Friend".to_string()),
        capabilities: false,
    };
    let err = fixture.install(&first, None).unwrap_err().to_string();
    assert!(err.contains("approve them to install it"), "{err}");
    assert!(fixture.store.registry().publishers.is_empty());
    assert!(fixture.store.registry().entries.is_empty());

    fixture.approval = trusting("Friend");
    let installed = fixture.install(&first, None).unwrap();
    assert_eq!(installed.capabilities, Grant::Granted);
    assert!(fixture.store.registry().is_trusted(Some(&key)));

    fixture.approval = Approval::default();
    let same = signed_cart(&fixture, &signer, "0.2.0", PRIVILEGED);
    assert_eq!(
        fixture.install(&same, None).unwrap().capabilities,
        Grant::Granted
    );

    let more = signed_cart(&fixture, &signer, "0.3.0", PRIVILEGED_MORE);
    let err = fixture.install(&more, None).unwrap_err().to_string();
    assert!(err.contains("more.cabinet-test.invalid"), "{err}");
    let entry = fixture.store.entry(&installed.cartridge).unwrap();
    assert_eq!(entry.versions.len(), 2);
    assert_eq!(fixture.store.grant(entry), Grant::Granted);

    fixture.approval = Approval {
        trust_publisher: None,
        capabilities: true,
    };
    assert_eq!(
        fixture.install(&more, None).unwrap().capabilities,
        Grant::Granted
    );

    fixture.approval = Approval::default();
    let plain = signed_cart(&fixture, &signer, "0.4.0", "");
    assert_eq!(
        fixture.install(&plain, None).unwrap().capabilities,
        Grant::None
    );
    let entry = fixture.store.entry(&installed.cartridge).unwrap();
    assert_eq!(entry.approved_capabilities, None);
}

#[test]
fn another_publisher_with_the_same_id_is_a_separate_cartridge() {
    let mut fixture = Fixture::new("trust-two");
    let friend = signer();
    let stranger = signer();
    fixture.approval = trusting("Friend");
    let trusted = fixture
        .install(&signed_cart(&fixture, &friend, "0.1.0", PRIVILEGED), None)
        .unwrap();
    fixture.approval = Approval::default();
    let unknown = fixture
        .install(&signed_cart(&fixture, &stranger, "0.2.0", PRIVILEGED), None)
        .unwrap();
    assert_ne!(trusted.cartridge, unknown.cartridge);
    assert_eq!(trusted.capabilities, Grant::Granted);
    assert_eq!(unknown.capabilities, Grant::Withheld);
    assert_eq!(fixture.store.registry().entries.len(), 2);
    assert_eq!(fixture.store.registry().publishers.len(), 1);
}

const CA_NOW: u64 = 1_794_600_000_000;
const CA_GOOD: &[u8] = include_bytes!("../../../cartridge/src/fixtures/ca-good.pem");
const CA_TWO: &[u8] = include_bytes!("../../../cartridge/src/fixtures/ca-two.pem");
const CA_LOOSE: &[u8] = include_bytes!("../../../cartridge/src/fixtures/ca-unconstrained.pem");

fn trusts_ca(names: &str) -> String {
    format!(
        r#"
[[assets]]
id = "ca"
file = "files/ca.pem"

[capabilities.trust_ca]
asset = "ca"
permitted_names = [{names}]
scope = "install"
stores = ["system"]
"#
    )
}

fn ca_cart(
    fixture: &Fixture,
    signer: &KeySigner,
    version: &str,
    names: &str,
    ca: &[u8],
) -> PathBuf {
    build_cart(
        &fixture.scratch,
        &CartSpec {
            version,
            extra: &trusts_ca(names),
            files: &[("files/ca.pem", ca)],
            signer: Some(signer),
            ..CartSpec::default()
        },
    )
}

fn ca_fixture(tag: &str) -> (Fixture, std::sync::Arc<FakeTrustStore>) {
    let mut fixture = Fixture::new(tag);
    let trust = std::sync::Arc::new(FakeTrustStore::default());
    fixture.now = CA_NOW;
    fixture.trust = Some(std::sync::Arc::clone(&trust));
    (fixture, trust)
}

fn ca_state(fixture: &Fixture, installed: &Installed) -> CaState {
    let entry = fixture.store.entry(&installed.cartridge).unwrap();
    catrust::state(
        entry,
        &fixture.store.manifest(&installed.digest).unwrap(),
        &fixture.store.unpacked(&installed.digest),
        true,
        CA_NOW,
    )
}

const ONE_NAME: &str = "\"cabinet-test.invalid\"";
const TWO_NAMES: &str = "\"cabinet-test.invalid\", \"other-test.invalid\"";

#[test]
fn a_granted_certificate_authority_is_trusted_at_install_and_removed_at_uninstall() {
    let (mut fixture, trust) = ca_fixture("ca-install");
    let signer = signer();
    let cart = ca_cart(&fixture, &signer, "0.1.0", ONE_NAME, CA_GOOD);
    fixture.approval = trusting("Friend");
    let installed = fixture.install(&cart, None).unwrap();
    assert_eq!(installed.capabilities, Grant::Granted);
    assert_eq!(installed.ca_problem, None);
    let recorded = fixture
        .store
        .entry(&installed.cartridge)
        .unwrap()
        .trusted_ca
        .clone()
        .unwrap();
    assert_eq!(recorded.subject, "CN=Cabinet fixture good");
    assert_eq!(*trust.held.lock().unwrap(), vec![recorded.sha256.clone()]);
    assert_eq!(ca_state(&fixture, &installed), CaState::Ready);

    *trust.refuse.lock().unwrap() = true;
    let err = uninstall(
        &mut fixture.store,
        &installed.cartridge,
        Platform::current().unwrap(),
        &fixture.scratch.path("logs"),
        None,
        Some(&*trust),
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("stays installed"), "{err}");
    assert!(fixture.store.entry(&installed.cartridge).is_ok());
    assert_eq!(trust.held.lock().unwrap().len(), 1);

    *trust.refuse.lock().unwrap() = false;
    uninstall(
        &mut fixture.store,
        &installed.cartridge,
        Platform::current().unwrap(),
        &fixture.scratch.path("logs"),
        None,
        Some(&*trust),
    )
    .unwrap();
    assert!(trust.held.lock().unwrap().is_empty());
}

#[test]
fn a_certificate_authority_that_does_not_match_its_declaration_fails_the_install() {
    let (mut fixture, trust) = ca_fixture("ca-mismatch");
    let signer = signer();
    fixture.approval = trusting("Friend");
    for (version, names, ca) in [
        ("0.1.0", ONE_NAME, CA_LOOSE),
        ("0.1.1", ONE_NAME, CA_TWO),
        ("0.1.2", TWO_NAMES, CA_GOOD),
    ] {
        let cart = ca_cart(&fixture, &signer, version, names, ca);
        let err = fixture.install(&cart, None).unwrap_err().to_string();
        assert!(err.contains("certificate authority is refused"), "{err}");
    }
    assert!(fixture.store.registry().entries.is_empty());
    assert!(trust.held.lock().unwrap().is_empty());
}

#[test]
fn a_withheld_or_refused_certificate_authority_is_not_trusted_and_blocks_sessions() {
    let (mut fixture, trust) = ca_fixture("ca-withheld");
    let signer = signer();
    let cart = ca_cart(&fixture, &signer, "0.1.0", ONE_NAME, CA_GOOD);
    let installed = fixture.install(&cart, None).unwrap();
    assert_eq!(installed.capabilities, Grant::Withheld);
    assert!(trust.held.lock().unwrap().is_empty());
    assert!(matches!(
        ca_state(&fixture, &installed),
        CaState::Missing(_)
    ));

    *trust.refuse.lock().unwrap() = true;
    fixture.approval = trusting("Friend");
    let installed = fixture.install(&cart, None).unwrap();
    assert_eq!(installed.capabilities, Grant::Granted);
    assert!(installed.ca_problem.as_ref().unwrap().contains("cancelled"));
    assert!(fixture
        .store
        .entry(&installed.cartridge)
        .unwrap()
        .trusted_ca
        .is_none());
    assert!(matches!(
        ca_state(&fixture, &installed),
        CaState::Missing(_)
    ));

    *trust.refuse.lock().unwrap() = false;
    catrust::reconcile(
        &mut fixture.store,
        &installed.cartridge,
        &*trust,
        true,
        CA_NOW,
    )
    .unwrap();
    assert_eq!(trust.held.lock().unwrap().len(), 1);
    assert_eq!(ca_state(&fixture, &installed), CaState::Ready);
}

#[test]
fn forgetting_the_publisher_or_changing_the_certificate_authority_updates_the_trust() {
    let (mut fixture, trust) = ca_fixture("ca-change");
    let signer = signer();
    fixture.approval = trusting("Friend");
    let first = fixture
        .install(
            &ca_cart(&fixture, &signer, "0.1.0", ONE_NAME, CA_GOOD),
            None,
        )
        .unwrap();
    let old = trust.held.lock().unwrap().clone();
    let second = fixture
        .install(
            &ca_cart(&fixture, &signer, "0.2.0", TWO_NAMES, CA_TWO),
            None,
        )
        .unwrap();
    assert_eq!(second.ca_problem, None);
    let new = trust.held.lock().unwrap().clone();
    assert_eq!(new.len(), 1);
    assert_ne!(new, old);
    assert!(matches!(ca_state(&fixture, &first), CaState::Missing(_)));
    assert_eq!(ca_state(&fixture, &second), CaState::Ready);

    fixture
        .store
        .forget_publisher(&signer.publisher().fingerprint())
        .unwrap();
    let problems = catrust::reconcile_all(&mut fixture.store, &*trust, false, CA_NOW);
    assert!(problems.is_empty(), "{problems:?}");
    assert!(trust.held.lock().unwrap().is_empty());
    assert!(fixture
        .store
        .entry(&second.cartridge)
        .unwrap()
        .trusted_ca
        .is_none());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn openssl(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("/usr/bin/openssl")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "openssl {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn throwaway_ca(dir: &Path) -> Vec<u8> {
    std::fs::write(
        dir.join("ca.cnf"),
        "[req]\ndistinguished_name=dn\nx509_extensions=v3\nprompt=no\n[dn]\nCN=Cabinet throwaway test CA\n[v3]\nbasicConstraints=critical,CA:TRUE,pathlen:0\nkeyUsage=critical,keyCertSign,cRLSign\nsubjectKeyIdentifier=hash\nnameConstraints=critical,permitted;DNS:cabinet-test.invalid,excluded;IP:0.0.0.0/0.0.0.0,excluded;IP:0:0:0:0:0:0:0:0/0:0:0:0:0:0:0:0\n",
    )
    .unwrap();
    openssl(
        dir,
        &[
            "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca.key", "-out", "ca.pem",
            "-days", "2", "-config", "ca.cnf",
        ],
    );
    for (leaf, name) in [
        ("inside", "cabinet-test.invalid"),
        ("outside", "other-test.invalid"),
    ] {
        std::fs::write(
            dir.join(format!("{leaf}.ext")),
            format!("basicConstraints=CA:FALSE\nkeyUsage=critical,digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:{name}\n"),
        )
        .unwrap();
        let (key, csr, pem, ext) = (
            format!("{leaf}.key"),
            format!("{leaf}.csr"),
            format!("{leaf}.pem"),
            format!("{leaf}.ext"),
        );
        let subject = format!("/CN={name}");
        openssl(
            dir,
            &[
                "req", "-newkey", "rsa:2048", "-nodes", "-keyout", &key, "-out", &csr, "-subj",
                &subject,
            ],
        );
        openssl(
            dir,
            &[
                "x509",
                "-req",
                "-in",
                &csr,
                "-CA",
                "ca.pem",
                "-CAkey",
                "ca.key",
                "-CAcreateserial",
                "-out",
                &pem,
                "-days",
                "1",
                "-extfile",
                &ext,
            ],
        );
        std::fs::remove_file(dir.join(key)).unwrap();
    }
    std::fs::remove_file(dir.join("ca.key")).unwrap();
    std::fs::read(dir.join("ca.pem")).unwrap()
}

#[cfg(target_os = "macos")]
fn verifies(dir: &Path, leaf: &str, name: &str) -> bool {
    std::process::Command::new("/usr/bin/security")
        .args(["verify-cert", "-L", "-p", "ssl", "-n", name, "-c"])
        .arg(dir.join(format!("{leaf}.pem")))
        .output()
        .unwrap()
        .status
        .success()
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "trusts a throwaway certificate authority in the login keychain; macOS asks for a password twice"]
fn the_login_keychain_trusts_a_cartridge_certificate_authority_while_it_is_installed() {
    let keychain = catrust::Keychain;
    let mut fixture = Fixture::new("ca-live");
    fixture.now = crate::time::now_ms();
    let dir = fixture.scratch.path("ca");
    std::fs::create_dir_all(&dir).unwrap();
    let ca = throwaway_ca(&dir);
    assert!(!verifies(&dir, "inside", "cabinet-test.invalid"));

    let signer = signer();
    let cart = ca_cart(&fixture, &signer, "0.1.0", ONE_NAME, &ca);
    let installed = Installer {
        store: &mut fixture.store,
        cabinet: Version::new(0, 1, 15),
        platform: Platform::current().unwrap(),
        fetch: &fixture.fetch,
        log_dir: fixture.scratch.path("logs"),
        now: fixture.now,
        busy: false,
        trust: Some(&keychain),
    }
    .install(&cart, None, &trusting("Friend"))
    .unwrap();
    assert_eq!(installed.ca_problem, None);
    let recorded = fixture
        .store
        .entry(&installed.cartridge)
        .unwrap()
        .trusted_ca
        .clone()
        .unwrap();
    let outcome = std::panic::catch_unwind(|| {
        assert!(keychain.holds(&recorded.sha256).unwrap());
        assert!(verifies(&dir, "inside", "cabinet-test.invalid"));
        assert!(!verifies(&dir, "outside", "other-test.invalid"));
        assert!(!verifies(&dir, "inside", "other-test.invalid"));
    });

    uninstall(
        &mut fixture.store,
        &installed.cartridge,
        Platform::current().unwrap(),
        &fixture.scratch.path("logs"),
        None,
        Some(&keychain),
    )
    .unwrap();
    assert!(!keychain.holds(&recorded.sha256).unwrap());
    assert!(!verifies(&dir, "inside", "cabinet-test.invalid"));
    outcome.unwrap();
}

#[cfg(target_os = "linux")]
fn verifies(dir: &Path, leaf: &str, name: &str) -> bool {
    std::process::Command::new("/usr/bin/openssl")
        .args(["verify", "-purpose", "sslserver", "-verify_hostname", name])
        .arg(dir.join(format!("{leaf}.pem")))
        .output()
        .unwrap()
        .status
        .success()
}

#[cfg(target_os = "linux")]
#[test]
#[ignore = "needs The Cabinet's helper installed on this machine; it trusts a throwaway certificate authority system-wide"]
fn the_installed_helper_trusts_a_cartridge_certificate_authority_while_it_is_installed() {
    let trust = catrust::HelperTrust(crate::cartridge::privileged::system_socket());
    let anchors = || {
        crate::cartridge::privileged::system_socket()
            .connect()
            .unwrap()
            .status()
            .unwrap()
            .into_iter()
            .filter_map(|entry| match entry.change {
                cabinet_helper::protocol::Change::TrustCa { anchor, .. } => Some(anchor),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(anchors().is_empty());
    let mut fixture = Fixture::new("ca-live");
    fixture.now = crate::time::now_ms();
    let dir = fixture.scratch.path("ca");
    std::fs::create_dir_all(&dir).unwrap();
    let ca = throwaway_ca(&dir);
    assert!(!verifies(&dir, "inside", "cabinet-test.invalid"));

    let signer = signer();
    let cart = ca_cart(&fixture, &signer, "0.1.0", ONE_NAME, &ca);
    let installed = Installer {
        store: &mut fixture.store,
        cabinet: Version::new(0, 1, 15),
        platform: Platform::current().unwrap(),
        fetch: &fixture.fetch,
        log_dir: fixture.scratch.path("logs"),
        now: fixture.now,
        busy: false,
        trust: Some(&trust),
    }
    .install(&cart, None, &trusting("Friend"))
    .unwrap();
    assert_eq!(installed.ca_problem, None);
    let recorded = fixture
        .store
        .entry(&installed.cartridge)
        .unwrap()
        .trusted_ca
        .clone()
        .unwrap();
    let placed = anchors();
    let outcome = std::panic::catch_unwind(|| {
        assert!(trust.holds(&recorded.sha256).unwrap());
        assert_eq!(placed.len(), 1, "{placed:?}");
        assert!(Path::new(&placed[0]).exists(), "{placed:?}");
        assert!(verifies(&dir, "inside", "cabinet-test.invalid"));
        assert!(!verifies(&dir, "outside", "other-test.invalid"));
        assert!(!verifies(&dir, "inside", "other-test.invalid"));
    });

    uninstall(
        &mut fixture.store,
        &installed.cartridge,
        Platform::current().unwrap(),
        &fixture.scratch.path("logs"),
        None,
        Some(&trust),
    )
    .unwrap();
    assert!(!trust.holds(&recorded.sha256).unwrap());
    assert!(anchors().is_empty());
    assert!(placed.iter().all(|path| !Path::new(path).exists()));
    assert!(!verifies(&dir, "inside", "cabinet-test.invalid"));
    outcome.unwrap();
}
