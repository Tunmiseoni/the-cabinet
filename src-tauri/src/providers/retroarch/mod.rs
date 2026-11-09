#[allow(dead_code)]
pub(crate) mod command;
mod core;
mod hotkeys;
mod parity;
mod spec;
#[cfg(test)]
mod test_support;

pub(crate) use core::{download_managed_core, frozen_core_sha256};
pub use hotkeys::Binding;
pub use parity::ParityStatus;

use super::{Capabilities, MatchRequest, Provider, Role};
use crate::config::{Config, RetroArchInput};
use crate::constants;
use crate::contracts::{InstallInfo, LaunchSpec, PeerOverride};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InputMap {
    pub bindings: Vec<Binding>,
    pub defaults: RetroArchInput,
}

pub struct RetroArchProvider {
    program: PathBuf,
    core: PathBuf,
    port: u16,
    command_port: u16,
    nickname: Option<String>,
    handle: Option<String>,
    network: Option<crate::net::SharedProvider>,
    overrides_dir: PathBuf,
    peer_override: PeerOverride,
    mute_spectators: bool,
    max_ping_ms: u32,
    isolated_config: bool,
    autoconfig_dir: Option<PathBuf>,
    host_config: Option<PathBuf>,
    core_dir: Option<PathBuf>,
    input: RetroArchInput,
    input_enabled: bool,
    socd: String,
}

impl RetroArchProvider {
    pub fn new(cfg: &Config, app_config_dir: &Path, app_data_dir: &Path, dev: bool) -> Self {
        let program = cfg
            .retroarch_path
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(core::DEFAULT_PROGRAM));
        let managed_core = core::managed_core_candidates(app_data_dir);
        let home = crate::env::home_dir();
        let candidates = core::core_candidates(&program, home.as_deref());
        let core = core::resolve_core(cfg.retroarch_core.as_deref(), &candidates, &managed_core);
        let core = core::adopt_managed_core(core, app_data_dir);
        let nickname = cfg
            .retroarch_nickname
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        let handle = cfg
            .handle
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(str::to_string);
        let network = crate::net::provider(cfg).ok();
        let autoconfig_dir = core::resolve_autoconfig_dir(&program, home.as_deref());
        let host_config = core::resolve_host_config(&program, home.as_deref());
        let core_dir =
            core::resolve_retroarch_core_dir(host_config.as_deref(), &program, home.as_deref());
        Self {
            program,
            core,
            port: if cfg.retroarch_port == 0 {
                constants::RETROARCH_DEFAULT_PORT
            } else {
                cfg.retroarch_port
            },
            command_port: if cfg.retroarch_command_port == 0 {
                constants::RETROARCH_DEFAULT_COMMAND_PORT
            } else {
                cfg.retroarch_command_port
            },
            nickname,
            handle,
            network,
            overrides_dir: app_config_dir.join("retroarch"),
            peer_override: if dev {
                PeerOverride::loopback()
            } else {
                PeerOverride::default()
            },
            mute_spectators: cfg.retroarch_mute_spectators,
            max_ping_ms: cfg.retroarch_max_ping_ms,
            isolated_config: cfg.retroarch_isolated_config,
            autoconfig_dir,
            host_config,
            core_dir,
            input: cfg.retroarch_input.clone(),
            input_enabled: cfg.retroarch_input_enabled,
            socd: cfg.retroarch_socd.clone(),
        }
    }

    fn peer(&self, fallback: &str) -> String {
        self.peer_override.resolve_or(fallback)
    }

    pub(crate) fn command_port_for(&self, role: Role) -> u16 {
        let offset = match role {
            Role::P1 => 0,
            Role::P2 => 1,
            Role::Spectator => 2,
        };
        self.command_port.saturating_add(offset)
    }

    fn tailnet_hostname(&self) -> Option<String> {
        let network = self.network.as_ref()?;
        let hostname = network.self_node().ok()??.hostname;
        let hostname = hostname.trim();
        (!hostname.is_empty()).then(|| hostname.to_string())
    }

    fn resolve_nickname(&self) -> String {
        let explicit = self
            .nickname
            .as_deref()
            .or(self.handle.as_deref())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let nickname = explicit
            .or_else(|| self.tailnet_hostname())
            .or_else(crate::env::os_hostname)
            .unwrap_or_else(|| "player".to_string());
        spec::sanitize_value(&nickname)
    }

    fn write_overrides(
        &self,
        role: Role,
        seat: Option<u8>,
        nickname: &str,
        start_as_spectator: bool,
        rom_path: &Path,
    ) -> crate::error::Result<PathBuf> {
        spec::write_overrides(self, role, seat, nickname, start_as_spectator, rom_path)
    }

    /// Make the core basename visible to RetroArch's own core scan, so its netplay capability
    /// resolves instead of reading as an unsupported core. See `core::ensure_core_visible`.
    pub(crate) fn ensure_core_visible(&self) -> crate::error::Result<()> {
        let Some(core_dir) = self.core_dir.as_deref() else {
            return Ok(());
        };
        core::ensure_core_visible(&self.core, core_dir).map(|_| ())
    }

    pub(crate) fn hotkey_map(&self) -> InputMap {
        let host_cfg = if self.isolated_config {
            None
        } else {
            self.host_config.as_deref()
        };
        let bindings = if self.input_enabled {
            hotkeys::effective_bindings_with_collisions(host_cfg, &self.input)
        } else {
            Vec::new()
        };
        InputMap {
            bindings,
            defaults: RetroArchInput::default(),
        }
    }
}

impl Provider for RetroArchProvider {
    fn detect(&self) -> crate::error::Result<InstallInfo> {
        let program_ok = self.program.is_file() || core::is_on_path(&self.program);
        let core_ok = self.core.is_file();
        let installed = program_ok && core_ok;
        let detail = if installed {
            format!("{} · {}", self.program.display(), self.core.display())
        } else if !program_ok {
            format!(
                "RetroArch binary not found at {} — install RetroArch",
                self.program.display()
            )
        } else {
            format!(
                "core not found at {} — Settings → RetroArch → Download frozen core",
                self.core.display()
            )
        };
        Ok(InstallInfo {
            id: "retroarch".to_string(),
            label: "RetroArch (netplay)".to_string(),
            installed,
            detail,
        })
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            spectate: true,
            dev_pair: true,
        }
    }

    fn port(&self, _role: Role) -> Option<u16> {
        Some(self.port)
    }

    fn command_port(&self, role: Role) -> Option<u16> {
        Some(self.command_port_for(role))
    }

    fn requires_rom_file(&self) -> bool {
        true
    }

    fn spec(&self, request: &MatchRequest) -> crate::error::Result<LaunchSpec> {
        if !request.rom_path.is_file() {
            return Err(format!("ROM not found: {}", request.rom_path.display()).into());
        }
        let nickname = self.resolve_nickname();
        let seat = request.seat()?;
        let overrides = self.write_overrides(
            request.role,
            seat,
            &nickname,
            request.start_as_spectator,
            request.rom_path,
        )?;
        let base_config = spec::write_base_config(self)?;
        let peer = self.peer(request.peer_ip);
        let args = spec::launch_args(&spec::Args {
            core: &self.core,
            rom_path: request.rom_path,
            overrides: &overrides,
            base_config: base_config.as_deref(),
            role: request.role,
            peer: &peer,
            port: self.port,
            nickname: &nickname,
        });

        Ok(LaunchSpec {
            program: self.program.clone(),
            args,
            cwd: request
                .rom_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from(".")),
            envs: Vec::new(),
        })
    }

    fn parity(&self, rom_path: &Path) -> crate::error::Result<Option<ParityStatus>> {
        parity::status(self, rom_path)
    }

    fn input_map(&self) -> Option<InputMap> {
        Some(self.hotkey_map())
    }

    fn ensure_core_visible(&self) -> crate::error::Result<()> {
        RetroArchProvider::ensure_core_visible(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::retroarch::test_support::{provider, request, Scratch};
    use crate::sync::MutexExt;
    use std::sync::{Arc, Mutex};

    fn spawn_capturing(
        spec: &crate::contracts::LaunchSpec,
    ) -> (std::process::Child, Arc<Mutex<String>>) {
        use std::process::Stdio;
        let mut child = crate::process::command(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .envs(spec.envs.iter().cloned())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn retroarch");
        let log = Arc::new(Mutex::new(String::new()));
        let mut streams: Vec<Box<dyn std::io::Read + Send>> = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            streams.push(Box::new(stdout));
        }
        if let Some(stderr) = child.stderr.take() {
            streams.push(Box::new(stderr));
        }
        for stream in streams {
            let log = Arc::clone(&log);
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in std::io::BufReader::new(stream)
                    .lines()
                    .map_while(Result::ok)
                {
                    let mut log = log.lock_or_recover();
                    log.push_str(&line);
                    log.push('\n');
                }
            });
        }
        (child, log)
    }

    fn wait_for(log: &Arc<Mutex<String>>, needle: &str, timeout: std::time::Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if log.lock_or_recover().contains(needle) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    #[test]
    fn capabilities_match_the_degrade_paths() {
        let scratch = Scratch::new("caps");
        let caps = provider(&scratch).capabilities();
        assert!(caps.spectate);
        assert!(caps.dev_pair);
    }

    #[test]
    fn nickname_prefers_the_explicit_value_and_sanitizes_it() {
        let scratch = Scratch::new("nick-explicit");
        let mut provider = provider(&scratch);
        provider.nickname = Some("ex\"plicit\n".to_string());
        provider.handle = Some("handle".to_string());
        assert_eq!(provider.resolve_nickname(), "explicit");
    }

    #[test]
    fn nickname_falls_back_to_the_handle() {
        let scratch = Scratch::new("nick-handle");
        let mut provider = provider(&scratch);
        provider.nickname = None;
        provider.handle = Some("handle".to_string());
        assert_eq!(provider.resolve_nickname(), "handle");
    }

    #[test]
    fn nickname_is_never_empty() {
        let scratch = Scratch::new("nick-empty");
        let mut provider = provider(&scratch);
        provider.nickname = Some("   ".to_string());
        provider.handle = Some(String::new());
        assert!(!provider.resolve_nickname().is_empty());
    }

    #[test]
    #[ignore = "launches three real RetroArch netplay instances (host/client/spectator) over loopback"]
    fn live_loopback_roles_smoke() {
        use std::time::Duration;

        let core = PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Library/Application Support/RetroArch/cores/fbneo_libretro.dylib");
        let Some(rom) = std::env::var_os("CABINET_TEST_ROM").map(PathBuf::from) else {
            eprintln!("skipping: set CABINET_TEST_ROM to a local ROM zip");
            return;
        };
        if !core.is_file() || !rom.is_file() {
            eprintln!("skipping: RetroArch core or ROM not present");
            return;
        }

        let scratch = Scratch::new("live");
        let cfg = Config {
            retroarch_core: Some(core.to_string_lossy().to_string()),
            ..Config::default()
        };
        let provider = RetroArchProvider::new(&cfg, &scratch.dir, &scratch.dir, true);

        let mut children = Vec::new();
        for role in [Role::P1, Role::P2, Role::Spectator] {
            let spec = provider.spec(&request(role, &rom, "127.0.0.1")).unwrap();
            let child = crate::process::command(&spec.program)
                .args(&spec.args)
                .current_dir(&spec.cwd)
                .envs(spec.envs.iter().cloned())
                .spawn()
                .expect("spawn retroarch");
            children.push((role, child));
            std::thread::sleep(Duration::from_secs(2));
        }
        std::thread::sleep(Duration::from_secs(8));

        let mut all_alive = true;
        for (role, child) in children.iter_mut() {
            let alive = child.try_wait().unwrap().is_none();
            eprintln!("retroarch {role:?} alive={alive}");
            all_alive &= alive;
        }
        for (_, child) in children.iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        assert!(all_alive, "all three roles should stay running");
    }

    #[test]
    #[ignore = "launches a host-as-spectator and two loopback clients (opens RetroArch windows)"]
    fn live_host_as_spectator_seats_clients_by_seat() {
        use std::time::Duration;

        let core = PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Library/Application Support/RetroArch/cores/fbneo_libretro.dylib");
        let Some(rom) = std::env::var_os("CABINET_TEST_ROM").map(PathBuf::from) else {
            eprintln!("skipping: set CABINET_TEST_ROM to a local ROM zip");
            return;
        };
        if !core.is_file() || !rom.is_file() {
            eprintln!("skipping: RetroArch core or ROM not present");
            return;
        }

        let scratch = Scratch::new("live-seats");
        let cfg = Config {
            retroarch_core: Some(core.to_string_lossy().to_string()),
            ..Config::default()
        };
        let provider = RetroArchProvider::new(&cfg, &scratch.dir, &scratch.dir, true);

        let mut host_request = request(Role::P1, &rom, "127.0.0.1");
        host_request.start_as_spectator = true;
        let host_spec = provider.spec(&host_request).unwrap();
        let (mut host, host_log) = spawn_capturing(&host_spec);
        std::thread::sleep(Duration::from_secs(4));

        let mut first_request = request(Role::P2, &rom, "127.0.0.1");
        first_request.player_slot = Some(1);
        let first_spec = provider.spec(&first_request).unwrap();
        let (mut first, first_log) = spawn_capturing(&first_spec);
        let seat_one = wait_for(
            &first_log,
            "You have joined as player 1",
            Duration::from_secs(25),
        );

        let mut second_request = request(Role::P2, &rom, "127.0.0.1");
        second_request.player_slot = Some(2);
        let second_spec = provider.spec(&second_request).unwrap();
        let (mut second, second_log) = spawn_capturing(&second_spec);
        let seat_two = wait_for(
            &second_log,
            "You have joined as player 2",
            Duration::from_secs(25),
        );

        for child in [&mut host, &mut first, &mut second] {
            let _ = child.kill();
            let _ = child.wait();
        }

        if !seat_one {
            eprintln!(
                "seat-1 client never bound player 1:\n{}",
                first_log.lock_or_recover()
            );
        }
        if !seat_two {
            eprintln!(
                "seat-2 client never bound player 2:\n{}",
                second_log.lock_or_recover()
            );
        }
        eprintln!("host log:\n{}", host_log.lock_or_recover());
        assert!(
            seat_one,
            "the client seated on player 1 did not join as player 1"
        );
        assert!(
            seat_two,
            "the client seated on player 2 did not join as player 2"
        );
    }

    #[test]
    #[ignore = "launches a real RetroArch host and drives its loopback command socket"]
    fn live_command_socket_smoke() {
        use std::time::Duration;

        let core = PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join("Library/Application Support/RetroArch/cores/fbneo_libretro.dylib");
        let Some(rom) = std::env::var_os("CABINET_TEST_ROM").map(PathBuf::from) else {
            eprintln!("skipping: set CABINET_TEST_ROM to a local ROM zip");
            return;
        };
        if !core.is_file() || !rom.is_file() {
            eprintln!("skipping: RetroArch core or ROM not present");
            return;
        }

        let scratch = Scratch::new("live-command");
        let cfg = Config {
            retroarch_core: Some(core.to_string_lossy().to_string()),
            ..Config::default()
        };
        let provider = RetroArchProvider::new(&cfg, &scratch.dir, &scratch.dir, false);
        let port = provider.command_port_for(Role::P1);
        let spec = provider
            .spec(&request(Role::P1, &rom, "127.0.0.1"))
            .unwrap();
        let mut child = crate::process::command(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.cwd)
            .envs(spec.envs.iter().cloned())
            .spawn()
            .expect("spawn retroarch");

        let mut status = None;
        for _ in 0..40 {
            std::thread::sleep(Duration::from_millis(500));
            if let Ok(answer) = command::instance_status(port) {
                status = Some(answer);
                break;
            }
        }

        let read = command::read_core_ram_bytes(port, 0x010D28, 1);
        let snapshot = crate::lobby::results::read_snapshot(port, crate::lobby::results::SFIII3NR1);

        let _ = child.kill();
        let _ = child.wait();

        assert!(
            status.is_some(),
            "command socket on {port} never answered GET_STATUS"
        );
        let read = read.expect("read the round counter over READ_CORE_RAM");
        assert_eq!(read.address, 0x010D28);
        assert_eq!(read.bytes.len(), 1);
        assert!(snapshot.is_ok(), "read a health snapshot: {snapshot:?}");
    }
}
