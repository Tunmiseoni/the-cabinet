use super::*;
use crate::cartridge::testing::{build_cart, cart, CartSpec, Scratch};
use serde_json::json;

const NOW: u64 = 1_000_000;

fn cabinet() -> Version {
    Version::new(0, 1, 15)
}

fn platform() -> Platform {
    Platform::LinuxX86_64
}

fn install(store: &mut Store, cart: &Path) -> crate::error::Result<Installed> {
    let staged = store.stage(cart, &cabinet(), platform(), NOW)?;
    store.register(&staged, NOW, false)
}

#[test]
fn installs_into_the_content_addressed_store() {
    let scratch = Scratch::new("install");
    let file = cart(&scratch, "test", "0.1.0");
    let mut store = Store::open(scratch.path("app")).unwrap();
    let installed = install(&mut store, &file).unwrap();
    assert!(installed.new);
    assert_eq!(installed.cartridge, CartridgeRef::new(None, "test"));
    assert_eq!(installed.version, Version::new(0, 1, 0));
    let unpacked = store.unpacked(&installed.digest);
    assert!(unpacked.join("cartridge.toml").is_file());
    assert!(unpacked.join("files/main").is_file());
    assert!(store.archive(&installed.digest).is_file());
    assert_eq!(store.manifest(&installed.digest).unwrap().id, "test");
    assert_eq!(
        store.active_digest(&installed.cartridge).unwrap(),
        installed.digest
    );
    assert_eq!(
        store.data_dir(&installed.cartridge),
        scratch.path("app/cartridges/unsigned/test")
    );

    let reopened = Store::open(scratch.path("app")).unwrap();
    assert_eq!(reopened.registry(), store.registry());

    let again = install(&mut store, &file).unwrap();
    assert!(!again.new);
    assert_eq!(again.digest, installed.digest);
    assert_eq!(store.entry(&installed.cartridge).unwrap().versions.len(), 1);
}

#[test]
fn refuses_other_platforms_and_newer_formats() {
    let scratch = Scratch::new("platform");
    let file = build_cart(
        &scratch,
        &CartSpec {
            id: "nomac",
            platforms: &["linux-x86_64", "windows-x86_64"],
            ..CartSpec::default()
        },
    );
    let mut store = Store::open(scratch.path("app")).unwrap();
    let err = store
        .stage(&file, &cabinet(), Platform::MacosAarch64, NOW)
        .err()
        .unwrap();
    assert!(err.to_string().contains("not available"), "{err}");
    assert!(store.registry().entries.is_empty());
    let old_cabinet = Version::new(0, 0, 1);
    let err = store
        .stage(&file, &old_cabinet, platform(), NOW)
        .err()
        .unwrap();
    assert!(err.to_string().contains("needs The Cabinet 0.1.0"), "{err}");
}

#[test]
fn refuses_a_second_build_of_a_seen_version() {
    let scratch = Scratch::new("conflict");
    let mut store = Store::open(scratch.path("app")).unwrap();
    let first = install(&mut store, &cart(&scratch, "test", "1.0.0")).unwrap();
    let rebuilt = build_cart(
        &scratch,
        &CartSpec {
            version: "1.0.0",
            unix: "#!/bin/sh\nexit 1\n",
            ..CartSpec::default()
        },
    );
    let err = install(&mut store, &rebuilt).err().unwrap().to_string();
    assert!(err.contains("different build of that version"), "{err}");
    let entry = store.entry(&first.cartridge).unwrap();
    assert_eq!(entry.conflicts.len(), 1);
    assert_eq!(entry.versions.len(), 1);
    assert_eq!(
        Store::open(scratch.path("app"))
            .unwrap()
            .entry(&first.cartridge)
            .unwrap()
            .conflicts
            .len(),
        1
    );
}

#[test]
fn a_newer_version_becomes_active_unless_busy_or_pinned() {
    let scratch = Scratch::new("active");
    let mut store = Store::open(scratch.path("app")).unwrap();
    let one = install(&mut store, &cart(&scratch, "test", "1.0.0")).unwrap();
    let staged = store
        .stage(
            &cart(&scratch, "test", "1.1.0"),
            &cabinet(),
            platform(),
            NOW,
        )
        .unwrap();
    let two = store.register(&staged, NOW, true).unwrap();
    assert_eq!(store.active_digest(&one.cartridge).unwrap(), one.digest);
    store.settle().unwrap();
    assert_eq!(store.active_digest(&one.cartridge).unwrap(), two.digest);

    store
        .update(&one.cartridge, |entry| {
            entry.pinned = Some(one.digest.clone());
            Ok(())
        })
        .unwrap();
    install(&mut store, &cart(&scratch, "test", "1.2.0")).unwrap();
    assert_eq!(store.active_digest(&one.cartridge).unwrap(), one.digest);

    let older = install(&mut store, &cart(&scratch, "test", "0.9.0")).unwrap();
    assert!(older.new);
    assert_eq!(store.active_digest(&one.cartridge).unwrap(), one.digest);
}

#[test]
fn prune_removes_unretained_builds_from_disk() {
    let scratch = Scratch::new("prune");
    let mut store = Store::open(scratch.path("app")).unwrap();
    let digests: Vec<String> = ["1.0.0", "1.1.0", "1.2.0", "1.3.0"]
        .iter()
        .map(|version| {
            install(&mut store, &cart(&scratch, "test", version))
                .unwrap()
                .digest
        })
        .collect();
    let in_use = BTreeSet::from([digests[0].clone()]);
    let removed = store.prune(NOW, &in_use).unwrap();
    assert_eq!(removed, vec![digests[1].clone()]);
    assert!(!store.unpacked(&digests[1]).exists());
    assert!(!store.archive(&digests[1]).exists());
    for kept in [&digests[0], &digests[2], &digests[3]] {
        assert!(store.unpacked(kept).exists());
    }
}

#[test]
fn settings_are_checked_against_the_active_manifest() {
    let scratch = Scratch::new("settings");
    let file = build_cart(
        &scratch,
        &CartSpec {
            extra:
                "[[settings]]\nkey = \"dir\"\nlabel = \"Folder\"\ntype = \"dir\"\nrequired = true\n",
            ..CartSpec::default()
        },
    );
    let mut store = Store::open(scratch.path("app")).unwrap();
    let installed = install(&mut store, &file).unwrap();
    store
        .set_settings(
            &installed.cartridge,
            Map::from_iter([("dir".to_string(), json!("/games"))]),
        )
        .unwrap();
    assert_eq!(
        store.entry(&installed.cartridge).unwrap().settings["dir"],
        json!("/games")
    );
    assert!(store
        .set_settings(
            &installed.cartridge,
            Map::from_iter([("dir".to_string(), json!(false))]),
        )
        .is_err());
    assert_eq!(
        store.entry(&installed.cartridge).unwrap().settings["dir"],
        json!("/games")
    );
}

#[test]
fn removing_an_entry_deletes_its_data_and_unshared_builds() {
    let scratch = Scratch::new("remove");
    let mut store = Store::open(scratch.path("app")).unwrap();
    let installed = install(&mut store, &cart(&scratch, "test", "1.0.0")).unwrap();
    let data = store.data_dir(&installed.cartridge);
    std::fs::create_dir_all(data.join("assets")).unwrap();
    std::fs::write(data.join("assets/x"), b"x").unwrap();
    store.remove_entry(&installed.cartridge).unwrap();
    assert!(!data.exists());
    assert!(!store.unpacked(&installed.digest).exists());
    assert!(store.registry().entries.is_empty());
    assert!(store.remove_entry(&installed.cartridge).is_err());
}

#[test]
fn discard_cleans_up_a_failed_install() {
    let scratch = Scratch::new("discard");
    let mut store = Store::open(scratch.path("app")).unwrap();
    let staged = store
        .stage(
            &cart(&scratch, "test", "1.0.0"),
            &cabinet(),
            platform(),
            NOW,
        )
        .unwrap();
    let digest = staged.inspected.digest.clone();
    assert!(store.unpacked(&digest).exists());
    store.discard(&staged);
    assert!(!store.unpacked(&digest).exists());
    assert!(!store.archive(&digest).exists());
    assert!(store.registry().entries.is_empty());
}
