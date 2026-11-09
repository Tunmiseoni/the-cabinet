use sha2::{Digest as _, Sha256};
use std::io::Read;

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn sha256_reader(mut reader: impl Read) -> std::io::Result<(String, u64)> {
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((hex(&hasher.finalize()), total))
}

pub fn sha256_bytes(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

pub fn sha256_file(path: &std::path::Path) -> std::io::Result<String> {
    Ok(sha256_reader(std::fs::File::open(path)?)?.0)
}

pub fn content_digest<'a>(entries: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    let mut entries: Vec<_> = entries.into_iter().collect();
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut listing = String::new();
    for (path, sha256) in entries {
        listing.push_str(sha256);
        listing.push_str("  ");
        listing.push_str(path);
        listing.push('\n');
    }
    sha256_bytes(listing.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_known_values() {
        assert_eq!(
            sha256_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_reader(&b"abc"[..]).unwrap(),
            (
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string(),
                3
            )
        );
    }

    #[test]
    fn the_digest_ignores_entry_order() {
        let a = content_digest([("files/b", "22"), ("cartridge.toml", "11")]);
        let b = content_digest([("cartridge.toml", "11"), ("files/b", "22")]);
        assert_eq!(a, b);
        assert_eq!(a, sha256_bytes(b"11  cartridge.toml\n22  files/b\n"));
    }

    #[test]
    fn the_digest_covers_paths_and_contents() {
        let base = content_digest([("cartridge.toml", "11"), ("files/a", "22")]);
        assert_ne!(
            base,
            content_digest([("cartridge.toml", "11"), ("files/b", "22")])
        );
        assert_ne!(
            base,
            content_digest([("cartridge.toml", "11"), ("files/a", "23")])
        );
        assert_ne!(base, content_digest([("cartridge.toml", "11")]));
    }
}
