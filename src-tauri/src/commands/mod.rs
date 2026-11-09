use crate::constants;
use crate::net::{self, Network, PeerHealth};
use crate::probe;
use crate::roms::{self, RomIndex};
use std::time::Duration;
use tauri::AppHandle;

pub(crate) mod cabinet;
pub(crate) mod config;
pub(crate) mod launch;
pub(crate) mod lobby;

pub(crate) use config::load_config;

#[tauri::command(async)]
pub fn list_peers(app: AppHandle) -> crate::error::CommandResult<Network> {
    let cfg = load_config(&app)?;
    Ok(net::provider(&cfg)?.status()?)
}

#[tauri::command(async)]
pub fn peer_health(app: AppHandle, ip: String) -> crate::error::CommandResult<PeerHealth> {
    let cfg = load_config(&app)?;
    Ok(net::provider(&cfg)?.health(&ip))
}

#[tauri::command(async)]
pub fn peers_health(
    app: AppHandle,
    ips: Vec<String>,
) -> crate::error::CommandResult<Vec<PeerHealth>> {
    let cfg = load_config(&app)?;
    Ok(net::provider(&cfg)?.health_many(&ips))
}

#[tauri::command(async)]
pub fn list_roms(app: AppHandle) -> crate::error::CommandResult<RomIndex> {
    let cfg = load_config(&app)?;
    Ok(roms::index(&cfg))
}

#[tauri::command(async)]
pub fn probe_port(ip: String, port: u16, timeout_ms: Option<u64>) -> probe::PortProbe {
    let timeout = Duration::from_millis(
        timeout_ms
            .unwrap_or(constants::PROBE_DEFAULT_TIMEOUT_MS)
            .clamp(
                constants::PROBE_TIMEOUT_MIN_MS,
                constants::PROBE_TIMEOUT_MAX_MS,
            ),
    );
    probe::probe(&ip, port, timeout)
}
