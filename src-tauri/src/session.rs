use crate::config::{self, Config};
use crate::constants;
use crate::contracts::LaunchSpec;
use crate::logging;
use crate::net::PeerHealth;
use crate::netplay::{NetplayConnection, NetplayInfo, NetplayTracker};
use crate::probe;
use crate::providers::Role;
use crate::supervisor::{self, Capture, Supervised};
use crate::sync::MutexExt;
use crate::time;
use serde::Serialize;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

pub const MATCH_EVENT: &str = "match-state-changed";

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstanceState {
    pub role: Role,
    pub role_label: String,
    pub port: Option<u16>,
    pub command_port: Option<u16>,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub message: Option<String>,
    pub netplay: Option<NetplayInfo>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MatchState {
    pub status: String,
    pub rom: Option<String>,
    pub peer_ip: Option<String>,
    pub dev: bool,
    pub started_at_ms: Option<u64>,
    pub instances: Vec<InstanceState>,
    pub peer_health: Option<PeerHealth>,
    pub message: Option<String>,
}

impl Default for MatchState {
    fn default() -> Self {
        Self {
            status: "idle".to_string(),
            rom: None,
            peer_ip: None,
            dev: false,
            started_at_ms: None,
            instances: Vec::new(),
            peer_health: None,
            message: None,
        }
    }
}

pub struct Plan {
    pub spec: LaunchSpec,
    pub role: Role,
    pub rom: String,
    pub peer_ip: String,
    pub port: Option<u16>,
    pub command_port: Option<u16>,
}

struct Running {
    child: Supervised,
    role: Role,
}

#[derive(Default)]
struct SessionInner {
    generation: u64,
    running: Vec<Running>,
    state: MatchState,
}

#[derive(Default)]
pub struct Session {
    inner: Mutex<SessionInner>,
}

fn emit(app: &AppHandle, state: &MatchState) {
    app.emit(MATCH_EVENT, state).ok();
}

fn netplay_observer(app: &AppHandle, generation: u64, role: Role) -> logging::LineObserver {
    let app = app.clone();
    let tracker = Arc::new(Mutex::new(NetplayTracker::new()));
    Arc::new(move |line: &str| {
        let snapshot = {
            let mut tracker = tracker.lock_or_recover();
            if tracker.feed(role, line) {
                Some(tracker.info().clone())
            } else {
                None
            }
        };
        let Some(netplay) = snapshot else {
            return;
        };
        // The host's beacon advertises the live seats and the waiting queue from what its netplay
        // log has actually seen, so a joiner can tell whether a seat is free and spectators can
        // see their place in line.
        if role == Role::P1 {
            if let Some(lobby) = app.try_state::<crate::lobby::Lobby>() {
                lobby.observe_netplay(&netplay);
            }
        }
        let session = app.state::<Session>();
        let state = {
            let mut inner = session.inner.lock_or_recover();
            if inner.generation != generation {
                return;
            }
            let Some(slot) = inner
                .state
                .instances
                .iter_mut()
                .find(|slot| slot.role == role)
            else {
                return;
            };
            slot.netplay = Some(netplay);
            inner.state.clone()
        };
        emit(&app, &state);
    })
}

fn spawn_child(
    app: &AppHandle,
    generation: u64,
    plan: &Plan,
    session_dir: Option<&Path>,
    capture_netplay: bool,
) -> crate::error::Result<Supervised> {
    let capture =
        session_dir.and_then(
            |dir| match logging::open_emulator_log(dir, plan.role.key()) {
                Ok(sink) => Some(Capture {
                    label: format!("{}[{}]", plan.role.key(), plan.rom),
                    sink,
                    observer: capture_netplay.then(|| netplay_observer(app, generation, plan.role)),
                }),
                Err(err) => {
                    log::warn!("cannot capture emulator output: {err}");
                    None
                }
            },
        );
    supervisor::spawn(&plan.spec, capture)
}

fn clear_running(inner: &mut SessionInner) {
    for running in inner.running.drain(..) {
        running.child.stop_blocking(Duration::ZERO);
    }
}

#[derive(Debug, Clone, Default)]
pub struct LaunchOptions {
    pub dev: bool,
    pub wait_for_host: bool,
    pub peer_display: String,
    pub capture_netplay: bool,
}

pub fn launch_many(
    app: &AppHandle,
    plans: &[Plan],
    options: LaunchOptions,
) -> crate::error::Result<MatchState> {
    if plans.is_empty() {
        return Err("nothing to launch".into());
    }

    let session = app.state::<Session>();
    let mut inner = session.inner.lock_or_recover();
    clear_running(&mut inner);
    inner.generation += 1;
    let generation = inner.generation;

    let session_dir = match logging::create_session_dir(app) {
        Ok(dir) => {
            log::info!("session log dir: {}", dir.display());
            match logging::prune_sessions(app) {
                Ok(removed) if removed > 0 => {
                    log::info!("pruned {removed} old session dir(s)")
                }
                Ok(_) => {}
                Err(err) => log::warn!("cannot prune old session dirs: {err}"),
            }
            Some(dir)
        }
        Err(err) => {
            log::warn!("{err}");
            None
        }
    };

    let mut running: Vec<Running> = Vec::new();
    let mut instances = Vec::new();

    for plan in plans {
        if options.wait_for_host && matches!(plan.role, Role::P2 | Role::Spectator) {
            if let Some(port) = plan.port {
                log::info!(
                    "waiting for host {}:{} before starting {}",
                    plan.peer_ip,
                    port,
                    plan.role.label()
                );
                let ready = probe::wait_for_port(&plan.peer_ip, port, constants::HOST_WAIT_TIMEOUT);
                log::info!("host {}:{} reachable={ready}", plan.peer_ip, port);
                if !ready {
                    log::warn!(
                        "host {}:{} did not accept connections; {} may show \"Failed to initialize netplay\"",
                        plan.peer_ip,
                        port,
                        plan.role.label()
                    );
                }
            }
        }
        log::info!(
            "launching {} program={} cwd={} args={:?}",
            plan.role.label(),
            plan.spec.program.display(),
            plan.spec.cwd.display(),
            plan.spec.args
        );
        let child = match spawn_child(
            app,
            generation,
            plan,
            session_dir.as_deref(),
            options.capture_netplay,
        ) {
            Ok(child) => child,
            Err(err) => {
                log::error!("{err}");
                for started in running.drain(..) {
                    started.child.stop_blocking(Duration::ZERO);
                }
                inner.state = MatchState::default();
                return Err(err);
            }
        };
        let pid = child.pid();
        log::info!("spawned {} pid={}", plan.role.label(), pid);
        running.push(Running {
            child,
            role: plan.role,
        });
        instances.push(InstanceState {
            role: plan.role,
            role_label: plan.role.label().to_string(),
            port: plan.port,
            command_port: plan.command_port,
            pid: Some(pid),
            exit_code: None,
            message: None,
            netplay: options.capture_netplay.then(NetplayInfo::connecting),
        });
    }

    let state = MatchState {
        status: "running".to_string(),
        rom: plans.first().map(|plan| plan.rom.clone()),
        peer_ip: Some(options.peer_display),
        dev: options.dev,
        started_at_ms: Some(time::now_ms()),
        instances,
        peer_health: None,
        message: None,
    };

    inner.running = running;
    inner.state = state.clone();
    drop(inner);

    emit(app, &state);
    spawn_monitor(app.clone(), generation);
    if !options.dev && !plans[0].peer_ip.trim().is_empty() {
        spawn_health(app.clone(), generation, plans[0].peer_ip.clone());
    }
    Ok(state)
}

pub fn launch(
    app: &AppHandle,
    plan: &Plan,
    options: LaunchOptions,
) -> crate::error::Result<MatchState> {
    launch_many(app, std::slice::from_ref(plan), options)
}

fn spawn_health(app: AppHandle, generation: u64, ip: String) {
    std::thread::spawn(move || {
        let network = {
            let cfg = app
                .path()
                .app_config_dir()
                .map(|dir| Config::load(&config::config_path(dir)))
                .unwrap_or_default();
            crate::net::provider(&cfg).ok()
        };
        let Some(network) = network else {
            return;
        };

        while running(&app, generation) {
            let health = network.health(&ip);
            if !running(&app, generation) {
                break;
            }
            let state = {
                let session = app.state::<Session>();
                let mut inner = session.inner.lock_or_recover();
                if inner.generation != generation {
                    break;
                }
                inner.state.peer_health = Some(health);
                inner.state.clone()
            };
            emit(&app, &state);
            std::thread::sleep(constants::HEALTH_INTERVAL);
        }
    });
}

fn running(app: &AppHandle, generation: u64) -> bool {
    let session = app.state::<Session>();
    let inner = session.inner.lock_or_recover();
    inner.generation == generation && !inner.running.is_empty()
}

/// A hosted room exists only while its host instance runs. End it when the match ends.
fn stop_lobby(app: &AppHandle) {
    if let Some(lobby) = app.try_state::<crate::lobby::Lobby>() {
        lobby.stop();
    }
}

fn spawn_monitor(app: AppHandle, generation: u64) {
    std::thread::spawn(move || loop {
        std::thread::sleep(constants::MONITOR_INTERVAL);

        let session = app.state::<Session>();
        let mut inner = session.inner.lock_or_recover();
        if inner.generation != generation || inner.running.is_empty() {
            break;
        }

        let running = std::mem::take(&mut inner.running);
        let mut still = Vec::new();
        let mut changed = false;

        for entry in running {
            match entry.child.try_exit() {
                Some(exit) => {
                    changed = true;
                    log::info!("{} exited with code {:?}", entry.role.label(), exit.code);
                    finish_slot(&mut inner.state, entry.role, exit.code);
                }
                None => still.push(entry),
            }
        }

        inner.running = still;
        let done = inner.running.is_empty();
        if done && inner.state.status == "running" {
            inner.state.status = "finished".to_string();
            inner.state.message = Some("all instances exited".to_string());
            log::info!("match finished: all instances exited");
            changed = true;
        }

        let state = inner.state.clone();
        drop(inner);

        if changed {
            emit(&app, &state);
        }

        if done {
            stop_lobby(&app);
            break;
        }
    });
}

fn mark_netplay_stopped(slot: &mut InstanceState) {
    if let Some(netplay) = slot.netplay.as_mut() {
        if netplay.connection != NetplayConnection::Failed {
            netplay.connection = NetplayConnection::Disconnected;
        }
    }
}

fn finish_slot(state: &mut MatchState, role: Role, code: Option<i32>) {
    if let Some(slot) = state
        .instances
        .iter_mut()
        .find(|slot| slot.role == role && slot.pid.is_some())
    {
        slot.pid = None;
        slot.exit_code = code;
        slot.message = Some(match code {
            Some(0) => "exited normally".to_string(),
            Some(code) => format!("exited with code {code}"),
            None => "terminated".to_string(),
        });
        mark_netplay_stopped(slot);
    }
}

pub fn stop(app: &AppHandle) -> crate::error::Result<MatchState> {
    let session = app.state::<Session>();
    let mut inner = session.inner.lock_or_recover();
    inner.generation += 1;
    let had_running = !inner.running.is_empty();
    clear_running(&mut inner);

    if had_running {
        log::info!("stopping match");
        inner.state.status = "finished".to_string();
        inner.state.message = Some("stopped by user".to_string());
        for slot in inner.state.instances.iter_mut() {
            if slot.pid.is_some() {
                slot.pid = None;
                slot.message = Some("stopped by user".to_string());
                mark_netplay_stopped(slot);
            }
        }
    }

    let state = inner.state.clone();
    drop(inner);

    if had_running {
        emit(app, &state);
        stop_lobby(app);
    }
    Ok(state)
}

pub fn status(app: &AppHandle) -> MatchState {
    app.state::<Session>().inner.lock_or_recover().state.clone()
}
