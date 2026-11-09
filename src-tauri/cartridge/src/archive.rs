use crate::digest;
use crate::error::{Error, Result};
use crate::manifest::{Manifest, MANIFEST_PATH};
use crate::signature::{Publisher, Signer, PUBLISHER_KEY_PATH, SIGNATURE_PATH};
use crate::version::Version;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

pub const EXTENSION: &str = "cart";
pub const FILES_DIR: &str = "files";
pub const MAX_ENTRIES: usize = 10_000;
pub const MAX_TOTAL_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_TEXT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone)]
pub struct Inspected {
    pub manifest: Manifest,
    pub digest: String,
    pub entries: Vec<Entry>,
    pub publisher: Option<Publisher>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Archive(message.into())
}

pub fn is_safe_entry(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('/')
        && !name.contains('\\')
        && !name.contains(':')
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

fn is_envelope(name: &str) -> bool {
    name == SIGNATURE_PATH || name == PUBLISHER_KEY_PATH
}

fn is_content(name: &str) -> bool {
    name == MANIFEST_PATH || name.starts_with("files/")
}

fn read_text(reader: impl Read, name: &str) -> Result<String> {
    let mut text = String::new();
    reader
        .take(MAX_TEXT_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|err| invalid(format!("{name} is not readable text: {err}")))?;
    if text.len() as u64 > MAX_TEXT_BYTES {
        return Err(invalid(format!(
            "{name} is larger than {MAX_TEXT_BYTES} bytes"
        )));
    }
    Ok(text)
}

fn check_bundled(manifest: &Manifest, present: &BTreeSet<String>) -> Result<()> {
    let missing: Vec<String> = manifest
        .bundled_paths()
        .into_iter()
        .filter(|path| !present.contains(*path))
        .map(|path| format!("{path} is referenced by cartridge.toml but not bundled"))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(Error::Invalid(missing))
    }
}

fn verify_envelope(
    digest: &str,
    signature: Option<String>,
    key: Option<String>,
) -> Result<Option<Publisher>> {
    match (signature, key) {
        (None, None) => Ok(None),
        (Some(signature), Some(key)) => {
            let publisher = Publisher::parse(&key)?;
            publisher.verify(digest, &signature)?;
            Ok(Some(publisher))
        }
        (Some(_), None) => Err(Error::Signature(format!(
            "{SIGNATURE_PATH} is present without {PUBLISHER_KEY_PATH}"
        ))),
        (None, Some(_)) => Err(Error::Signature(format!(
            "{PUBLISHER_KEY_PATH} is present without {SIGNATURE_PATH}"
        ))),
    }
}

pub fn inspect(path: &Path, cabinet: Option<&Version>) -> Result<Inspected> {
    let file = File::open(path)
        .map_err(|err| invalid(format!("cannot open {}: {err}", path.display())))?;
    let mut zip = zip::ZipArchive::new(file).map_err(|err| {
        invalid(format!(
            "{} is not a cartridge archive: {err}",
            path.display()
        ))
    })?;
    if zip.len() > MAX_ENTRIES {
        return Err(invalid(format!(
            "the archive has more than {MAX_ENTRIES} entries"
        )));
    }
    let mut seen = BTreeSet::new();
    let mut present = BTreeSet::new();
    let mut entries = Vec::new();
    let mut manifest_text = None;
    let mut signature = None;
    let mut key = None;
    let mut total = 0u64;
    for index in 0..zip.len() {
        let entry = zip.by_index(index)?;
        let name = entry.name()?.to_string();
        let trimmed = name.trim_end_matches('/');
        if !is_safe_entry(trimmed) {
            return Err(invalid(format!("unsafe entry name {name:?}")));
        }
        if !seen.insert(trimmed.to_ascii_lowercase()) {
            return Err(invalid(format!(
                "entry {trimmed} appears twice (names compare case-insensitively)"
            )));
        }
        if entry.is_symlink() {
            return Err(invalid(format!("entry {trimmed} is a symlink")));
        }
        if entry.is_dir() {
            if trimmed != FILES_DIR && !trimmed.starts_with("files/") {
                return Err(invalid(format!("unexpected directory {trimmed}")));
            }
            continue;
        }
        if is_envelope(trimmed) {
            let text = read_text(entry, trimmed)?;
            if trimmed == SIGNATURE_PATH {
                signature = Some(text);
            } else {
                key = Some(text);
            }
            continue;
        }
        if !is_content(trimmed) {
            return Err(invalid(format!(
                "unexpected entry {trimmed} (only {MANIFEST_PATH}, {SIGNATURE_PATH}, {PUBLISHER_KEY_PATH} and files/ belong in a cartridge)"
            )));
        }
        let (sha256, size) = if trimmed == MANIFEST_PATH {
            let text = read_text(entry, trimmed)?;
            let hashed = (digest::sha256_bytes(text.as_bytes()), text.len() as u64);
            manifest_text = Some(text);
            hashed
        } else {
            let limit = MAX_TOTAL_BYTES - total;
            let (sha256, size) = digest::sha256_reader(entry.take(limit + 1))?;
            if size > limit {
                return Err(invalid(format!(
                    "the archive unpacks to more than {MAX_TOTAL_BYTES} bytes"
                )));
            }
            (sha256, size)
        };
        total += size;
        present.insert(trimmed.to_string());
        entries.push(Entry {
            path: trimmed.to_string(),
            sha256,
            size,
        });
    }
    let manifest_text =
        manifest_text.ok_or_else(|| invalid(format!("{MANIFEST_PATH} is missing")))?;
    let manifest = Manifest::parse(&manifest_text, cabinet)?;
    check_bundled(&manifest, &present)?;
    let digest =
        digest::content_digest(entries.iter().map(|e| (e.path.as_str(), e.sha256.as_str())));
    let publisher = verify_envelope(&digest, signature, key)?;
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Inspected {
        manifest,
        digest,
        entries,
        publisher,
    })
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

fn set_read_only(path: &Path, executable: bool, dir: bool) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        set_mode(path, if executable || dir { 0o555 } else { 0o444 })
    }
    #[cfg(not(unix))]
    {
        let _ = executable;
        if dir {
            return Ok(());
        }
        let mut permissions = fs::metadata(path)?.permissions();
        permissions.set_readonly(true);
        fs::set_permissions(path, permissions)
    }
}

fn make_writable(path: &Path) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        set_mode(path, if metadata.is_dir() { 0o755 } else { 0o644 })?;
    }
    #[cfg(not(unix))]
    {
        let mut permissions = metadata.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        fs::set_permissions(path, permissions)?;
    }
    if metadata.is_dir() {
        for child in fs::read_dir(path)? {
            make_writable(&child?.path())?;
        }
    }
    Ok(())
}

pub fn remove_tree(path: &Path) -> std::io::Result<()> {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(err),
        Ok(_) => {}
    }
    make_writable(path)?;
    fs::remove_dir_all(path)
}

fn mark_tree_read_only(
    root: &Path,
    dir: &Path,
    executables: &BTreeSet<&str>,
) -> std::io::Result<()> {
    for child in fs::read_dir(dir)? {
        let child = child?.path();
        if child.is_dir() {
            mark_tree_read_only(root, &child, executables)?;
        } else {
            let relative = child
                .strip_prefix(root)
                .unwrap_or(&child)
                .to_string_lossy()
                .replace('\\', "/");
            set_read_only(&child, executables.contains(relative.as_str()), false)?;
        }
    }
    set_read_only(dir, false, true)
}

pub fn unpack(path: &Path, dest: &Path, inspected: &Inspected) -> Result<()> {
    if dest.exists() {
        return Err(invalid(format!("{} already exists", dest.display())));
    }
    let partial = dest.with_extension("partial");
    remove_tree(&partial)?;
    let result = unpack_into(path, &partial, inspected).and_then(|()| {
        fs::rename(&partial, dest).map_err(|err| {
            invalid(format!(
                "cannot move the cartridge into {}: {err}",
                dest.display()
            ))
        })
    });
    if result.is_err() {
        let _ = remove_tree(&partial);
    }
    result
}

fn unpack_into(path: &Path, partial: &Path, inspected: &Inspected) -> Result<()> {
    let expected: BTreeMap<&str, &Entry> = inspected
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry))
        .collect();
    let mut zip = zip::ZipArchive::new(File::open(path)?)?;
    fs::create_dir_all(partial)?;
    let mut written = BTreeSet::new();
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let name = entry.name()?.trim_end_matches('/').to_string();
        if entry.is_dir() || is_envelope(&name) {
            continue;
        }
        let Some(wanted) = expected.get(name.as_str()) else {
            return Err(invalid(format!(
                "{name} changed since the cartridge was inspected"
            )));
        };
        let target = partial.join(&name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut out = File::create(&target)?;
        let mut hashing = HashingWriter::new(&mut out);
        std::io::copy(&mut (&mut entry).take(wanted.size + 1), &mut hashing)?;
        let (sha256, size) = hashing.finish();
        out.sync_all()?;
        if sha256 != wanted.sha256 || size != wanted.size {
            return Err(invalid(format!(
                "{name} changed since the cartridge was inspected"
            )));
        }
        written.insert(name);
    }
    if written.len() != expected.len() {
        return Err(invalid("the archive changed since it was inspected"));
    }
    mark_tree_read_only(partial, partial, &inspected.manifest.program_paths())?;
    Ok(())
}

struct HashingWriter<'a, W: Write> {
    inner: &'a mut W,
    hasher: sha2::Sha256,
    size: u64,
}

impl<'a, W: Write> HashingWriter<'a, W> {
    fn new(inner: &'a mut W) -> Self {
        use sha2::Digest as _;
        Self {
            inner,
            hasher: sha2::Sha256::new(),
            size: 0,
        }
    }

    fn finish(self) -> (String, u64) {
        use sha2::Digest as _;
        (digest::hex(&self.hasher.finalize()), self.size)
    }
}

impl<W: Write> Write for HashingWriter<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use sha2::Digest as _;
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug, Clone)]
pub struct Packed {
    pub manifest: Manifest,
    pub digest: String,
    pub publisher: Option<Publisher>,
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> Result<()> {
    let mut children: Vec<_> = fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    children.sort_by_key(|child| child.file_name());
    for child in children {
        let path = child.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| invalid(format!("{} is outside {}", path.display(), root.display())))?
            .to_string_lossy()
            .replace('\\', "/");
        let kind = child.file_type()?;
        if kind.is_symlink() {
            return Err(invalid(format!(
                "{relative} is a symlink; cartridges hold plain files only"
            )));
        }
        if kind.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            out.push((relative, path));
        }
    }
    Ok(())
}

pub fn pack(source: &Path, out: &Path, signer: Option<&dyn Signer>) -> Result<Packed> {
    let manifest_path = source.join(MANIFEST_PATH);
    let manifest_text = fs::read_to_string(&manifest_path)
        .map_err(|err| invalid(format!("cannot read {}: {err}", manifest_path.display())))?;
    let manifest = Manifest::parse(&manifest_text, None)?;

    let mut files = Vec::new();
    collect_files(source, source, &mut files)?;
    let stray: Vec<_> = files
        .iter()
        .map(|(relative, _)| relative.as_str())
        .filter(|relative| !is_content(relative))
        .collect();
    if !stray.is_empty() {
        return Err(invalid(format!(
            "only {MANIFEST_PATH} and files/ are packed; remove: {}",
            stray.join(", ")
        )));
    }
    for (relative, _) in &files {
        if !is_safe_entry(relative) {
            return Err(invalid(format!("unsafe file name {relative:?}")));
        }
    }
    let present: BTreeSet<String> = files.iter().map(|(relative, _)| relative.clone()).collect();
    check_bundled(&manifest, &present)?;

    let mut hashed = Vec::new();
    for (relative, path) in &files {
        hashed.push((relative.clone(), digest::sha256_file(path)?));
    }
    let digest = digest::content_digest(hashed.iter().map(|(p, h)| (p.as_str(), h.as_str())));
    let signed = signer.map(|signer| signer.sign(&digest)).transpose()?;
    if let Some(signed) = &signed {
        signed.publisher.verify(&digest, &signed.signature)?;
    }

    let executables = manifest.program_paths();
    let partial = out.with_extension("partial");
    let result = write_zip(&partial, &files, &executables, signed.as_ref());
    let result = result.and_then(|()| fs::rename(&partial, out).map_err(Error::from));
    if result.is_err() {
        let _ = fs::remove_file(&partial);
    }
    result?;
    Ok(Packed {
        manifest,
        digest,
        publisher: signed.map(|signed| signed.publisher),
    })
}

fn write_zip(
    out: &Path,
    files: &[(String, PathBuf)],
    executables: &BTreeSet<&str>,
    signed: Option<&crate::signature::SignedDigest>,
) -> Result<()> {
    let mut zip = zip::ZipWriter::new(File::create(out)?);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .last_modified_time(zip::DateTime::default());
    for (relative, path) in files {
        let mode = if executables.contains(relative.as_str()) {
            0o755
        } else {
            0o644
        };
        zip.start_file(relative.as_str(), options.unix_permissions(mode))?;
        std::io::copy(&mut File::open(path)?, &mut zip)?;
    }
    if let Some(signed) = signed {
        zip.start_file(SIGNATURE_PATH, options.unix_permissions(0o644))?;
        zip.write_all(signed.signature.as_bytes())?;
        zip.start_file(PUBLISHER_KEY_PATH, options.unix_permissions(0o644))?;
        zip.write_all(signed.publisher.file_text().as_bytes())?;
    }
    zip.finish()?.sync_all()?;
    Ok(())
}
