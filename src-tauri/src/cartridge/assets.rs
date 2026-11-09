use super::context::Context;
use cabinet_cartridge::digest;
use serde::Serialize;
use std::io::Write;
use std::path::Path;

pub const MAX_ASSET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub trait Fetch: Sync {
    fn fetch(&self, url: &str, out: &mut dyn Write) -> crate::error::Result<()>;
}

pub struct HttpFetch;

impl Fetch for HttpFetch {
    fn fetch(&self, url: &str, out: &mut dyn Write) -> crate::error::Result<()> {
        let mut response = ureq::get(url)
            .call()
            .map_err(|err| format!("download of {url} failed: {err}"))?;
        let mut reader = response
            .body_mut()
            .with_config()
            .limit(MAX_ASSET_BYTES)
            .reader();
        std::io::copy(&mut reader, out)
            .map_err(|err| format!("download of {url} failed: {err}"))?;
        Ok(())
    }
}

fn matches(path: &Path, sha256: &str) -> bool {
    path.is_file() && digest::sha256_file(path).is_ok_and(|actual| actual == sha256)
}

pub fn ensure_assets(context: &Context<'_>, fetch: &dyn Fetch) -> crate::error::Result<()> {
    for asset in &context.manifest.assets {
        let Some(download) = asset
            .download
            .as_ref()
            .and_then(|downloads| downloads.get(&context.platform))
        else {
            continue;
        };
        let target = context.asset_path(&asset.id)?;
        if matches(&target, &download.sha256) {
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let partial = target.with_extension("partial");
        let result = (|| -> crate::error::Result<()> {
            let mut file = std::fs::File::create(&partial)?;
            fetch.fetch(&download.url, &mut file)?;
            file.sync_all()?;
            drop(file);
            let actual = digest::sha256_file(&partial)?;
            if actual != download.sha256 {
                return Err(format!(
                    "asset \"{}\" does not match its sha256 (expected {}, got {actual})",
                    asset.id, download.sha256
                )
                .into());
            }
            std::fs::rename(&partial, &target)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&partial);
        }
        result?;
        log::info!("asset {} ready at {}", asset.id, target.display());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckFailure {
    pub label: String,
    pub message: String,
}

pub fn run_checks(context: &Context<'_>) -> Vec<CheckFailure> {
    context
        .manifest
        .checks
        .iter()
        .filter_map(|check| {
            let fail = |message: String| {
                Some(CheckFailure {
                    label: check.label.clone(),
                    message,
                })
            };
            let path = match context.expand_path(&check.path) {
                Ok(path) => path,
                Err(err) => return fail(err),
            };
            match digest::sha256_file(&path) {
                Err(_) => fail(format!("{} was not found", path.display())),
                Ok(actual) if actual != check.sha256 => fail(
                    check
                        .message
                        .clone()
                        .unwrap_or_else(|| format!("{} differs from the group's", path.display())),
                ),
                Ok(_) => None,
            }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use cabinet_cartridge::{Manifest, Platform};
    use serde_json::{json, Map};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    pub(crate) struct FakeFetch {
        pub files: BTreeMap<String, Vec<u8>>,
        pub fetched: Mutex<Vec<String>>,
    }

    impl Fetch for FakeFetch {
        fn fetch(&self, url: &str, out: &mut dyn Write) -> crate::error::Result<()> {
            self.fetched.lock().unwrap().push(url.to_string());
            let body = self
                .files
                .get(url)
                .ok_or_else(|| format!("404 for {url}"))?;
            out.write_all(body)?;
            Ok(())
        }
    }

    fn manifest(sha: &str, check_sha: &str) -> Manifest {
        let extra = format!(
            r#"
[[assets]]
id = "core"
[assets.download.linux-x86_64]
url = "https://example.com/core"
sha256 = "{sha}"

[[settings]]
key = "dir"
label = "Folder"
type = "dir"

[[checks]]
label = "Game"
path = "${{setting:dir}}/game.bin"
sha256 = "{check_sha}"
message = "Your game.bin differs."

[[checks]]
label = "Core"
path = "${{asset:core}}"
sha256 = "{sha}"
"#
        );
        let text =
            crate::cartridge::testing::manifest("assets", "1.0.0", &["linux-x86_64"], &extra);
        Manifest::parse(&text, None).unwrap()
    }

    #[test]
    fn downloads_verifies_and_reuses_assets() {
        let scratch = crate::cartridge::testing::Scratch::new("assets");
        let body = b"core bytes".to_vec();
        let manifest = manifest(&digest::sha256_bytes(&body), &digest::sha256_bytes(b"game"));
        let settings = Map::from_iter([(
            "dir".to_string(),
            json!(scratch.path("games").to_string_lossy()),
        )]);
        let context = Context {
            manifest: &manifest,
            cartridge_dir: scratch.path("store"),
            data_dir: scratch.path("data"),
            platform: Platform::LinuxX86_64,
            settings: &settings,
        };
        let fetch = FakeFetch {
            files: BTreeMap::from([("https://example.com/core".to_string(), body.clone())]),
            ..FakeFetch::default()
        };
        ensure_assets(&context, &fetch).unwrap();
        assert_eq!(
            std::fs::read(scratch.path("data/assets/core")).unwrap(),
            body
        );
        ensure_assets(&context, &fetch).unwrap();
        assert_eq!(fetch.fetched.lock().unwrap().len(), 1);

        let failures = run_checks(&context);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].label, "Game");
        assert!(failures[0].message.contains("was not found"));
        std::fs::create_dir_all(scratch.path("games")).unwrap();
        std::fs::write(scratch.path("games/game.bin"), b"other").unwrap();
        assert_eq!(run_checks(&context)[0].message, "Your game.bin differs.");
        std::fs::write(scratch.path("games/game.bin"), b"game").unwrap();
        assert!(run_checks(&context).is_empty());
    }

    #[test]
    fn a_mismatched_download_fails_and_leaves_nothing() {
        let scratch = crate::cartridge::testing::Scratch::new("badasset");
        let manifest = manifest(&digest::sha256_bytes(b"expected"), &"0".repeat(64));
        let settings = Map::new();
        let context = Context {
            manifest: &manifest,
            cartridge_dir: scratch.path("store"),
            data_dir: scratch.path("data"),
            platform: Platform::LinuxX86_64,
            settings: &settings,
        };
        let fetch = FakeFetch {
            files: BTreeMap::from([("https://example.com/core".to_string(), b"tampered".to_vec())]),
            ..FakeFetch::default()
        };
        let err = ensure_assets(&context, &fetch).unwrap_err().to_string();
        assert!(err.contains("does not match its sha256"), "{err}");
        assert!(!scratch.path("data/assets/core").exists());
        assert!(!scratch.path("data/assets/core.partial").exists());
        let failures = run_checks(&context);
        assert!(failures.iter().any(|f| f.message.contains("is not set")));
    }
}
