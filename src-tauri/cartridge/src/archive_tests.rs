use crate::archive::{self, inspect, pack, unpack};
use crate::tests::TEST_MANIFEST;
use crate::{digest, Error};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use zip::write::SimpleFileOptions;

pub(crate) struct Scratch(pub PathBuf);

impl Scratch {
    pub(crate) fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cabinet-cartridge-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        archive::remove_tree(&dir).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub(crate) fn path(&self, relative: &str) -> PathBuf {
        self.0.join(relative)
    }

    pub(crate) fn write(&self, relative: &str, content: &[u8]) {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = archive::remove_tree(&self.0);
    }
}

pub(crate) fn source_folder(scratch: &Scratch) -> PathBuf {
    scratch.write("src/cartridge.toml", TEST_MANIFEST.as_bytes());
    for platform in ["macos-aarch64", "linux-x86_64"] {
        scratch.write(
            &format!("src/files/{platform}/echo-cart"),
            platform.as_bytes(),
        );
    }
    scratch.write("src/files/windows-x86_64/echo-cart.exe", b"windows");
    scratch.write("src/files/readme.txt", b"notes");
    scratch.path("src")
}

fn raw_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    for (name, content) in entries {
        zip.start_file(*name, SimpleFileOptions::default()).unwrap();
        zip.write_all(content).unwrap();
    }
    zip.finish().unwrap();
}

fn valid_entries() -> Vec<(&'static str, &'static [u8])> {
    vec![
        ("cartridge.toml", TEST_MANIFEST.as_bytes()),
        ("files/macos-aarch64/echo-cart", b"a"),
        ("files/linux-x86_64/echo-cart", b"b"),
        ("files/windows-x86_64/echo-cart.exe", b"c"),
    ]
}

fn inspect_err(entries: &[(&str, &[u8])]) -> String {
    let scratch = Scratch::new("raw");
    let cart = scratch.path("raw.cart");
    raw_zip(&cart, entries);
    inspect(&cart, None).unwrap_err().to_string()
}

#[test]
fn packs_inspects_and_unpacks() {
    let scratch = Scratch::new("roundtrip");
    let source = source_folder(&scratch);
    let cart = scratch.path("test-0.1.0.cart");
    let packed = pack(&source, &cart, None).unwrap();
    assert!(packed.publisher.is_none());
    assert!(!scratch.path("test-0.1.0.partial").exists());

    let inspected = inspect(&cart, None).unwrap();
    assert_eq!(inspected.digest, packed.digest);
    assert_eq!(inspected.manifest.id, "test");
    assert!(inspected.publisher.is_none());
    let paths: Vec<_> = inspected.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(
        paths,
        vec![
            "cartridge.toml",
            "files/linux-x86_64/echo-cart",
            "files/macos-aarch64/echo-cart",
            "files/readme.txt",
            "files/windows-x86_64/echo-cart.exe",
        ]
    );
    let expected = digest::content_digest(
        inspected
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.sha256.as_str())),
    );
    assert_eq!(inspected.digest, expected);

    let dest = scratch.path("store/unpacked");
    std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
    unpack(&cart, &dest, &inspected).unwrap();
    assert_eq!(
        std::fs::read(dest.join("files/linux-x86_64/echo-cart")).unwrap(),
        b"linux-x86_64"
    );
    assert_eq!(
        std::fs::read_to_string(dest.join("cartridge.toml")).unwrap(),
        TEST_MANIFEST
    );
    assert!(std::fs::metadata(dest.join("files/readme.txt"))
        .unwrap()
        .permissions()
        .readonly());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &str| {
            std::fs::metadata(dest.join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode("files/linux-x86_64/echo-cart"), 0o555);
        assert_eq!(mode("files/readme.txt"), 0o444);
        assert_eq!(mode("files"), 0o555);
        assert!(std::fs::write(dest.join("files/new.txt"), b"x").is_err());
    }
    assert!(unpack(&cart, &dest, &inspected).is_err());
    archive::remove_tree(&dest).unwrap();
    assert!(!dest.exists());
}

#[test]
fn the_digest_depends_on_content_not_on_packing() {
    let scratch = Scratch::new("stable");
    let source = source_folder(&scratch);
    let first = pack(&source, &scratch.path("a.cart"), None).unwrap();
    let second = pack(&source, &scratch.path("b.cart"), None).unwrap();
    assert_eq!(first.digest, second.digest);
    assert_eq!(
        std::fs::read(scratch.path("a.cart")).unwrap(),
        std::fs::read(scratch.path("b.cart")).unwrap()
    );
    scratch.write("src/files/readme.txt", b"changed");
    let third = pack(&source, &scratch.path("c.cart"), None).unwrap();
    assert_ne!(first.digest, third.digest);
}

#[test]
fn unpack_refuses_an_archive_changed_after_inspection() {
    let scratch = Scratch::new("tamper");
    let cart = scratch.path("x.cart");
    raw_zip(&cart, &valid_entries());
    let inspected = inspect(&cart, None).unwrap();
    let mut tampered = valid_entries();
    tampered[1].1 = b"evil";
    raw_zip(&cart, &tampered);
    let dest = scratch.path("dest");
    let err = unpack(&cart, &dest, &inspected).unwrap_err().to_string();
    assert!(
        err.contains("changed since the cartridge was inspected"),
        "{err}"
    );
    assert!(!dest.exists());
    assert!(!scratch.path("dest.partial").exists());
}

#[test]
fn pack_refuses_stray_and_missing_files() {
    let scratch = Scratch::new("stray");
    let source = source_folder(&scratch);
    scratch.write("src/notes.md", b"x");
    let err = pack(&source, &scratch.path("x.cart"), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("remove: notes.md"), "{err}");
    std::fs::remove_file(scratch.path("src/notes.md")).unwrap();
    std::fs::remove_file(scratch.path("src/files/linux-x86_64/echo-cart")).unwrap();
    let err = pack(&source, &scratch.path("x.cart"), None)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("files/linux-x86_64/echo-cart is referenced"),
        "{err}"
    );
    assert!(!scratch.path("x.cart").exists());
}

#[cfg(unix)]
#[test]
fn pack_refuses_symlinks() {
    let scratch = Scratch::new("symlink");
    let source = source_folder(&scratch);
    std::os::unix::fs::symlink("/etc/hosts", scratch.path("src/files/hosts")).unwrap();
    let err = pack(&source, &scratch.path("x.cart"), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("symlink"), "{err}");
}

#[test]
fn inspect_refuses_unsafe_or_unexpected_entries() {
    for (name, needle) in [
        ("../evil", "unsafe entry name"),
        ("files/../../evil", "unsafe entry name"),
        ("/abs", "unsafe entry name"),
        ("files\\win", "unsafe entry name"),
        ("C:/x", "unsafe entry name"),
        ("notes.md", "unexpected entry notes.md"),
    ] {
        let mut entries = valid_entries();
        entries.push((name, b"x"));
        let err = inspect_err(&entries);
        assert!(err.contains(needle), "{name}: {err}");
    }
    let mut twice = valid_entries();
    twice.push(("FILES/macos-aarch64/echo-cart", b"x"));
    assert!(inspect_err(&twice).contains("appears twice"));
}

#[test]
fn inspect_needs_the_manifest_and_bundled_files() {
    let entries = valid_entries();
    assert!(inspect_err(&entries[1..]).contains("cartridge.toml is missing"));
    let err = inspect_err(&entries[..3]);
    assert!(
        err.contains("files/windows-x86_64/echo-cart.exe is referenced"),
        "{err}"
    );
    let scratch = Scratch::new("notzip");
    scratch.write("x.cart", b"plain text");
    assert!(inspect(&scratch.path("x.cart"), None).is_err());
}

#[test]
fn inspect_refuses_half_an_envelope() {
    let mut entries = valid_entries();
    entries.push(("cartridge.sig", b"untrusted comment: x\nRUQ\n"));
    let err = inspect_err(&entries);
    assert!(err.contains("without publisher.pub"), "{err}");
    let mut entries = valid_entries();
    entries.push(("publisher.pub", b"untrusted comment: x\nRWQ\n"));
    assert!(inspect_err(&entries).contains("without cartridge.sig"));
}

#[test]
fn inspect_reports_a_manifest_error() {
    let mut entries = valid_entries();
    let broken = TEST_MANIFEST.replace("format      = 1", "format = 7");
    entries[0].1 = broken.as_bytes();
    let scratch = Scratch::new("format");
    let cart = scratch.path("x.cart");
    raw_zip(&cart, &entries);
    assert!(matches!(
        inspect(&cart, None),
        Err(Error::UnsupportedFormat(7))
    ));
}
