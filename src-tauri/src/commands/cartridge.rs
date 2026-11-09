use crate::cartridge::assets::HttpFetch;
use crate::cartridge::install::{self, Installer};
use crate::cartridge::registry::{CartridgeRef, Entry};
use crate::cartridge::runtime::{HostOptions, JoinOptions, Me, Prepared, Session, SessionView};
use crate::cartridge::store::{Installed, Store};
use crate::commands::config::load_config;
use crate::constants;
use crate::error::CommandResult;
use crate::sync::MutexExt;
use cabinet_cartridge::manifest::{Capabilities, Setting};
use cabinet_cartridge::{archive, Platform, Version};
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, Manager};

pub const SESSION_EVENT: &str = "cartridge://session";

#[derive(Default)]
pub struct Cartridges {
    store: Mutex<Option<Store>>,
    session: Mutex<Option<Arc<Session>>>,
}

fn cabinet_version() -> Version {
    env!("CARGO_PKG_VERSION")
        .parse()
        .expect("the package version is semver")
}

fn platform() -> crate::error::Result<Platform> {
    Platform::current().ok_or_else(|| {
        format!(
            "cartridges are not available for {}-{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
        .into()
    })
}

fn with_store<T>(
    app: &AppHandle,
    work: impl FnOnce(&mut Store) -> crate::error::Result<T>,
) -> crate::error::Result<T> {
    let state = app.state::<Cartridges>();
    let mut guard = state.store.lock_or_recover();
    if guard.is_none() {
        let root = app
            .path()
            .app_data_dir()
            .map_err(|err| format!("cannot resolve the data dir: {err}"))?;
        *guard = Some(Store::open(root)?);
    }
    work(guard.as_mut().expect("store opened above"))
}

fn current_session(app: &AppHandle) -> Option<Arc<Session>> {
    app.state::<Cartridges>()
        .session
        .lock_or_recover()
        .clone()
        .filter(|session| !session.is_ended())
}

fn busy(app: &AppHandle) -> bool {
    current_session(app).is_some()
}

fn in_use(app: &AppHandle) -> BTreeSet<String> {
    current_session(app)
        .map(|session| BTreeSet::from([session.digest().to_string()]))
        .unwrap_or_default()
}

fn settle(app: &AppHandle) {
    if busy(app) {
        return;
    }
    let in_use = in_use(app);
    let settled = with_store(app, |store| {
        store.settle()?;
        store.prune(crate::time::now_ms(), &in_use)
    });
    if let Err(err) = settled {
        log::warn!("cannot settle the cartridge store: {err}");
    }
}

fn log_dir(app: &AppHandle) -> crate::error::Result<PathBuf> {
    let _ = crate::logging::prune_sessions(app);
    crate::logging::create_session_dir(app)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Inspection {
    pub id: String,
    pub version: Version,
    pub title: String,
    pub description: Option<String>,
    pub digest: String,
    pub publisher: String,
    pub platforms: Vec<Platform>,
    pub available: bool,
    pub capabilities: Capabilities,
    pub settings: Vec<Setting>,
    pub installed: bool,
}

#[tauri::command(async)]
pub fn cartridge_inspect(app: AppHandle, path: PathBuf) -> CommandResult<Inspection> {
    let inspected = archive::inspect(&path, Some(&cabinet_version()))
        .map_err(crate::error::CabinetError::from)?;
    let manifest = inspected.manifest;
    let cartridge = CartridgeRef::new(inspected.publisher.as_ref(), &manifest.id);
    let installed = with_store(&app, |store| {
        Ok(store
            .registry()
            .entry(&cartridge)
            .is_some_and(|entry| entry.held(&inspected.digest).is_some()))
    })?;
    Ok(Inspection {
        available: Platform::current().is_some_and(|platform| manifest.supports(platform)),
        id: manifest.id,
        version: manifest.version,
        title: manifest.title,
        description: manifest.description,
        digest: inspected.digest,
        publisher: cartridge.publisher,
        platforms: manifest.platforms,
        capabilities: manifest.capabilities,
        settings: manifest.settings,
        installed,
    })
}

#[tauri::command(async)]
pub fn cartridge_install(
    app: AppHandle,
    path: PathBuf,
    settings: Option<Map<String, Value>>,
) -> CommandResult<Installed> {
    let platform = platform()?;
    let log_dir = log_dir(&app)?;
    let busy = busy(&app);
    Ok(with_store(&app, |store| {
        Installer {
            store,
            cabinet: cabinet_version(),
            platform,
            fetch: &HttpFetch,
            log_dir,
            now: crate::time::now_ms(),
            busy,
        }
        .install(&path, settings)
    })?)
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listed {
    pub entry: Entry,
    pub title: Option<String>,
    pub settings_schema: Vec<Setting>,
}

#[tauri::command(async)]
pub fn cartridge_list(app: AppHandle) -> CommandResult<Vec<Listed>> {
    Ok(with_store(&app, |store| {
        Ok(store
            .registry()
            .entries
            .iter()
            .map(|entry| {
                let manifest = entry
                    .active
                    .as_deref()
                    .and_then(|digest| store.manifest(digest).ok());
                Listed {
                    entry: entry.clone(),
                    title: manifest.as_ref().map(|manifest| manifest.title.clone()),
                    settings_schema: manifest
                        .map(|manifest| manifest.settings)
                        .unwrap_or_default(),
                }
            })
            .collect())
    })?)
}

#[tauri::command(async)]
pub fn cartridge_set_settings(
    app: AppHandle,
    publisher: String,
    id: String,
    settings: Map<String, Value>,
) -> CommandResult<()> {
    let cartridge = CartridgeRef { publisher, id };
    Ok(with_store(&app, |store| {
        store.set_settings(&cartridge, settings)
    })?)
}

#[tauri::command(async)]
pub fn cartridge_uninstall(
    app: AppHandle,
    publisher: String,
    id: String,
) -> CommandResult<Vec<String>> {
    let cartridge = CartridgeRef { publisher, id };
    if current_session(&app).is_some_and(|session| session.cartridge() == &cartridge) {
        return Err("stop the session before uninstalling its cartridge".into());
    }
    let platform = platform()?;
    let log_dir = log_dir(&app)?;
    Ok(with_store(&app, |store| {
        install::uninstall(store, &cartridge, platform, &log_dir)
    })?)
}

struct Local {
    me: Me,
    address: String,
    bind: Vec<IpAddr>,
    provider: Option<crate::net::SharedProvider>,
}

fn local(app: &AppHandle, loopback: bool) -> crate::error::Result<Local> {
    let cfg = load_config(app)?;
    let provider = crate::net::provider(&cfg).ok();
    let node = provider
        .as_ref()
        .and_then(|provider| provider.self_node().ok().flatten());
    let identity = node.as_ref().and_then(crate::net::local_identity);
    let id = match (identity, loopback) {
        (Some(identity), _) => identity.member_id(),
        (None, true) => crate::net::Identity::new("local", crate::env::instance()).member_id(),
        (None, false) => {
            return Err("the tailnet is not available; hosting needs a tailnet address".into())
        }
    };
    let name =
        crate::config::netplay_nickname(&cfg, node.as_ref().map(|node| node.hostname.as_str()));
    let tailnet = node
        .as_ref()
        .and_then(|node| node.ip.parse::<Ipv4Addr>().ok());
    let (address, bind) = if loopback {
        (
            Ipv4Addr::LOCALHOST.to_string(),
            vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
        )
    } else {
        let tailnet =
            tailnet.ok_or("the tailnet is not available; hosting needs a tailnet address")?;
        (
            tailnet.to_string(),
            crate::lobby::beacon::bind_addresses(Some(tailnet)),
        )
    };
    Ok(Local {
        me: Me { id, name },
        address,
        bind,
        provider,
    })
}

fn observer(app: &AppHandle) -> crate::cartridge::runtime::Observer {
    let app = app.clone();
    Arc::new(move |view: &SessionView| {
        let _ = app.emit(SESSION_EVENT, view);
    })
}

fn install_session(app: &AppHandle, session: Session) -> SessionView {
    let view = session.view();
    with_store(app, |store| {
        let cartridge = session.cartridge().clone();
        let digest = session.digest().to_string();
        store.update(&cartridge, |entry| {
            entry.mark_used_by_room(&digest, crate::time::now_ms());
            Ok(())
        })
    })
    .unwrap_or_else(|err| log::warn!("cannot record the room's build: {err}"));
    *app.state::<Cartridges>().session.lock_or_recover() = Some(Arc::new(session));
    view
}

#[tauri::command(async)]
pub fn cartridge_host(
    app: AppHandle,
    publisher: String,
    id: String,
    loopback: Option<bool>,
) -> CommandResult<SessionView> {
    if busy(&app) {
        return Err("a session is already running".into());
    }
    settle(&app);
    let local = local(&app, loopback.unwrap_or(false))?;
    let cartridge = CartridgeRef { publisher, id };
    let prepared = with_store(&app, |store| {
        Prepared::from_store(store, &cartridge, None, platform()?)
    })?;
    let provider = local.provider.clone();
    let verify = Arc::new(move |member_id: &str, address: IpAddr| {
        let node_id = member_id.split('#').next().unwrap_or(member_id);
        provider.as_ref().is_some_and(|provider| {
            provider
                .address(node_id)
                .ok()
                .flatten()
                .is_some_and(|known| known == address.to_string())
        })
    });
    let session = Session::host(
        prepared,
        local.me,
        HostOptions {
            bind: local.bind,
            port: constants::LOBBY_BEACON_PORT,
            address: local.address,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            verify,
            log_dir: log_dir(&app)?,
        },
        observer(&app),
    )?;
    log::info!(
        "hosting {} on {:?}",
        session.cartridge(),
        session.addresses()
    );
    Ok(install_session(&app, session))
}

#[tauri::command(async)]
pub fn cartridge_join(
    app: AppHandle,
    host: String,
    port: Option<u16>,
    loopback: Option<bool>,
) -> CommandResult<SessionView> {
    if busy(&app) {
        return Err("a session is already running".into());
    }
    settle(&app);
    let local = local(&app, loopback.unwrap_or(false))?;
    let platform = platform()?;
    let prepare_app = app.clone();
    let session = Session::join(
        move |room| {
            let cartridge = CartridgeRef {
                publisher: room.publisher.clone(),
                id: room.id.clone(),
            };
            with_store(&prepare_app, |store| {
                Prepared::from_store(store, &cartridge, Some(&room.digest), platform)
            })
        },
        local.me,
        JoinOptions {
            host,
            port: port.unwrap_or(constants::LOBBY_BEACON_PORT),
            log_dir: log_dir(&app)?,
        },
        observer(&app),
    )?;
    Ok(install_session(&app, session))
}

fn session(app: &AppHandle) -> crate::error::Result<Arc<Session>> {
    current_session(app).ok_or_else(|| "no session is running".into())
}

#[tauri::command]
pub async fn cartridge_start(app: AppHandle) -> CommandResult<()> {
    let session = session(&app)?;
    Ok(session.start().await?)
}

#[tauri::command]
pub fn cartridge_action(app: AppHandle, id: String) -> CommandResult<()> {
    session(&app)?.action(&id);
    Ok(())
}

#[tauri::command]
pub async fn cartridge_stop(app: AppHandle) -> CommandResult<Option<SessionView>> {
    let session = app.state::<Cartridges>().session.lock_or_recover().clone();
    let Some(session) = session else {
        return Ok(None);
    };
    session.stop().await;
    let view = session.view();
    let settle_app = app.clone();
    crate::tasks::blocking(move || settle(&settle_app)).await;
    Ok(Some(view))
}

#[tauri::command]
pub fn cartridge_session(app: AppHandle) -> Option<SessionView> {
    app.state::<Cartridges>()
        .session
        .lock_or_recover()
        .as_ref()
        .map(|session| session.view())
}

pub fn stop_on_exit(app: &AppHandle) {
    let session = app.state::<Cartridges>().session.lock_or_recover().take();
    if let Some(session) = session {
        tauri::async_runtime::block_on(session.stop());
    }
}
