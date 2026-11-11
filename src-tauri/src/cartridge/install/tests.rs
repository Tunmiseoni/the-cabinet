use super::*;
use crate::cartridge::assets::tests::FakeFetch;
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
            now: 1,
            busy: false,
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
