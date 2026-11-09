use super::core::DEFAULT_PROGRAM;
use super::RetroArchProvider;
use crate::providers::{MatchRequest, Role};
use std::path::{Path, PathBuf};

pub(super) struct Scratch {
    pub dir: PathBuf,
}

impl Scratch {
    pub(super) fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "cabinet-retroarch-{}-{}-{tag}",
            std::process::id(),
            unique()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self { dir }
    }

    pub(super) fn rom(&self) -> PathBuf {
        let path = self.dir.join("sfiii3nr1.zip");
        std::fs::write(&path, b"rom-bytes").expect("write rom");
        path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

fn unique() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0)
}

pub(super) fn provider(scratch: &Scratch) -> RetroArchProvider {
    RetroArchProvider {
        program: PathBuf::from(DEFAULT_PROGRAM),
        core: PathBuf::from("/app/cores/fbneo_libretro.dylib"),
        port: 55435,
        command_port: 55355,
        nickname: Some("spike".to_string()),
        handle: None,
        network: None,
        overrides_dir: scratch.dir.join("cfg"),
        peer_override: Default::default(),
        mute_spectators: true,
        max_ping_ms: 0,
        isolated_config: false,
        autoconfig_dir: None,
        host_config: None,
        core_dir: None,
        input: crate::config::RetroArchInput::default(),
        input_enabled: true,
        socd: "neutral".to_string(),
    }
}

pub(super) fn request<'a>(role: Role, rom: &'a Path, peer: &'a str) -> MatchRequest<'a> {
    MatchRequest {
        role,
        player_slot: None,
        rom_path: rom,
        peer_ip: peer,
        start_as_spectator: false,
    }
}
