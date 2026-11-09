use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MANIFEST: &str = r#"
format      = 1
id          = "hello"
version     = "0.2.0"
title       = "Hello"
min_cabinet = "0.1.0"
platforms   = ["linux-x86_64"]

[programs.main.run]
linux-x86_64 = "files/hello"

[lifecycle.session]
program = "main"
"#;

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cabinet-pack-{tag}-{}", std::process::id()));
        cabinet_cartridge::archive::remove_tree(&dir).unwrap();
        std::fs::create_dir_all(dir.join("src/files")).unwrap();
        std::fs::write(dir.join("src/cartridge.toml"), MANIFEST).unwrap();
        std::fs::write(dir.join("src/files/hello"), b"#!/bin/sh\necho hi\n").unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = cabinet_cartridge::archive::remove_tree(&self.0);
    }
}

fn pack_tool(cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_cabinet-pack"))
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap()
}

fn line<'a>(stdout: &'a str, key: &str) -> &'a str {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key} ")))
        .unwrap_or_else(|| panic!("no {key} line in {stdout}"))
}

#[test]
fn packs_to_the_default_name_and_inspects() {
    let scratch = Scratch::new("default");
    let packed = pack_tool(&scratch.0, &["pack", "src"]);
    assert!(packed.status.success(), "{packed:?}");
    let stdout = String::from_utf8(packed.stdout).unwrap();
    assert_eq!(line(&stdout, "packed"), "hello-0.2.0.cart");
    assert_eq!(line(&stdout, "version"), "0.2.0");
    assert!(stdout.lines().any(|line| line == "unsigned"));
    let digest = line(&stdout, "digest").to_string();
    assert_eq!(digest.len(), 64);

    let inspected = pack_tool(&scratch.0, &["inspect", "hello-0.2.0.cart"]);
    assert!(inspected.status.success(), "{inspected:?}");
    let stdout = String::from_utf8(inspected.stdout).unwrap();
    assert_eq!(line(&stdout, "id"), "hello");
    assert_eq!(line(&stdout, "digest"), digest);
    assert!(stdout.lines().any(|line| line.ends_with("  files/hello")));
}

#[test]
fn packs_to_an_explicit_path() {
    let scratch = Scratch::new("out");
    let out = scratch.0.join("custom.cart");
    let packed = pack_tool(&scratch.0, &["pack", "src", "--out", out.to_str().unwrap()]);
    assert!(packed.status.success(), "{packed:?}");
    assert!(out.is_file());
}

#[test]
fn reports_errors_and_usage() {
    let scratch = Scratch::new("errors");
    std::fs::write(scratch.0.join("src/stray.txt"), b"x").unwrap();
    let failed = pack_tool(&scratch.0, &["pack", "src"]);
    assert_eq!(failed.status.code(), Some(1));
    let stderr = String::from_utf8(failed.stderr).unwrap();
    assert!(stderr.contains("remove: stray.txt"), "{stderr}");

    let usage = pack_tool(&scratch.0, &["frobnicate"]);
    assert_eq!(usage.status.code(), Some(2));
    assert!(String::from_utf8(usage.stderr).unwrap().contains("usage:"));
}
