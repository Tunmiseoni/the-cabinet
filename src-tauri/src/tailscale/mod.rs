mod ping;
mod status;

pub use ping::{ping, ping_many};
pub use status::status;

use crate::config::Config;
use crate::process;
use std::path::PathBuf;

pub fn resolve_binary(cfg: &Config) -> crate::error::Result<PathBuf> {
    if let Some(custom) = cfg.tailscale_path.as_deref() {
        let path = PathBuf::from(custom);
        if path.exists() {
            return Ok(path);
        }
        return Err(format!("configured tailscale path not found: {custom}").into());
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from(
            "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
        ));
    }
    if cfg!(target_os = "windows") {
        if let Ok(program_files) = std::env::var("ProgramFiles") {
            candidates.push(
                PathBuf::from(program_files)
                    .join("Tailscale")
                    .join("tailscale.exe"),
            );
        }
        if let Ok(program_files) = std::env::var("ProgramFiles(x86)") {
            candidates.push(
                PathBuf::from(program_files)
                    .join("Tailscale")
                    .join("tailscale.exe"),
            );
        }
    }

    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.clone());
        }
    }

    if let Some(found) = find_on_path() {
        return Ok(found);
    }

    for candidate in candidates {
        if candidate.exists() {
            return Ok(candidate);
        }
    }

    Err("tailscale binary not found (set an override in settings)".into())
}

fn find_on_path() -> Option<PathBuf> {
    let binary = if cfg!(target_os = "windows") {
        "tailscale.exe"
    } else {
        "tailscale"
    };
    crate::env::on_path(binary)
}

pub(crate) fn cli_command(binary: &PathBuf) -> std::process::Command {
    #[cfg_attr(not(target_os = "macos"), allow(unused_mut))]
    let mut command = process::command(binary);
    #[cfg(target_os = "macos")]
    command.env("TAILSCALE_BE_CLI", "1");
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn forces_cli_mode_on_macos() {
        let command = cli_command(&PathBuf::from("/fake/tailscale"));
        let be_cli = command
            .get_envs()
            .find(|(key, _)| *key == "TAILSCALE_BE_CLI")
            .and_then(|(_, value)| value);
        assert_eq!(be_cli, Some(std::ffi::OsStr::new("1")));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn leaves_cli_mode_unset_off_macos() {
        let command = cli_command(&PathBuf::from("/fake/tailscale"));
        assert!(command.get_envs().all(|(key, _)| key != "TAILSCALE_BE_CLI"));
    }
}
