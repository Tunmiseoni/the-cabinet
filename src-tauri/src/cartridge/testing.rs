use cabinet_cartridge::archive;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

pub(crate) struct Scratch(PathBuf);

impl Scratch {
    pub(crate) fn new(tag: &str) -> Self {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "cabinet-cart-{tag}-{}-{}",
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
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = archive::remove_tree(&self.0);
    }
}

pub(crate) const PROGRAM_UNIX: &str = "files/main";
pub(crate) const PROGRAM_WINDOWS: &str = "files/main.cmd";

pub(crate) const ALL_PLATFORMS: &[&str] = &[
    "macos-aarch64",
    "macos-x86_64",
    "linux-x86_64",
    "windows-x86_64",
];

pub(crate) fn manifest(id: &str, version: &str, platforms: &[&str], extra: &str) -> String {
    let listed: Vec<String> = platforms.iter().map(|p| format!("\"{p}\"")).collect();
    let run: String = platforms
        .iter()
        .map(|p| {
            let program = if p.starts_with("windows") {
                PROGRAM_WINDOWS
            } else {
                PROGRAM_UNIX
            };
            format!("{p} = \"{program}\"\n")
        })
        .collect();
    format!(
        r#"format = 1
id = "{id}"
version = "{version}"
title = "{id} cartridge"
min_cabinet = "0.1.0"
platforms = [{}]

[programs.main.run]
{run}
[lifecycle.session]
program = "main"
{extra}
"#,
        listed.join(", ")
    )
}

pub(crate) struct CartSpec<'a> {
    pub id: &'a str,
    pub version: &'a str,
    pub platforms: &'a [&'a str],
    pub extra: &'a str,
    pub unix: &'a str,
    pub windows: &'a str,
    pub files: &'a [(&'a str, &'a [u8])],
}

impl Default for CartSpec<'_> {
    fn default() -> Self {
        Self {
            id: "test",
            version: "0.1.0",
            platforms: ALL_PLATFORMS,
            extra: "",
            unix: "#!/bin/sh\nexit 0\n",
            windows: "@exit /b 0\r\n",
            files: &[],
        }
    }
}

pub(crate) fn build_cart(scratch: &Scratch, spec: &CartSpec<'_>) -> PathBuf {
    let source = scratch.path(&format!("src-{}-{}", spec.id, spec.version));
    let _ = archive::remove_tree(&source);
    std::fs::create_dir_all(source.join("files")).unwrap();
    std::fs::write(
        source.join("cartridge.toml"),
        manifest(spec.id, spec.version, spec.platforms, spec.extra),
    )
    .unwrap();
    std::fs::write(source.join(PROGRAM_UNIX), spec.unix).unwrap();
    std::fs::write(source.join(PROGRAM_WINDOWS), spec.windows).unwrap();
    for (path, content) in spec.files {
        let path = source.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    let out = scratch.path(&format!("{}-{}.cart", spec.id, spec.version));
    archive::pack(&source, &out, None).unwrap();
    archive::remove_tree(&source).unwrap();
    out
}

pub(crate) fn cart(scratch: &Scratch, id: &str, version: &str) -> PathBuf {
    build_cart(
        scratch,
        &CartSpec {
            id,
            version,
            ..CartSpec::default()
        },
    )
}
