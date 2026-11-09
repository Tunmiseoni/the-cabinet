mod commands;
mod config;
mod constants;
mod contracts;
mod diagnostics;
mod env;
pub mod error;
mod lobby;
mod logging;
mod net;
mod netplay;
mod probe;
mod process;
mod providers;
mod roms;
mod session;
mod supervisor;
mod sync;
mod tailscale;
mod time;
mod windowing;

const LEGACY_IDENTIFIER: &str = "com.cabinet.app";
const MIGRATED_FILES: [&str; 1] = ["config.json"];

fn migrate_legacy_config_dir(current: &std::path::Path) {
    let Some(legacy) = current
        .parent()
        .map(|parent| parent.join(LEGACY_IDENTIFIER))
    else {
        return;
    };
    if !legacy.is_dir() {
        return;
    }
    for name in MIGRATED_FILES {
        let src = legacy.join(name);
        let dst = current.join(name);
        if src.is_file() && !dst.exists() {
            if let Some(parent) = dst.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(err) = std::fs::copy(&src, &dst) {
                log::warn!("failed to migrate {src:?}: {err}");
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn apply_linux_webkit_workarounds() {
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
}

fn verbose_logging_requested(app: &tauri::AppHandle) -> bool {
    if crate::commands::load_config(app)
        .map(|config| config.verbose_logging)
        .unwrap_or(false)
    {
        return true;
    }
    std::env::var("RUST_LOG").is_ok_and(|value| {
        let value = value.to_ascii_lowercase();
        value.contains("debug") || value.contains("trace")
    })
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "linux")]
    apply_linux_webkit_workarounds();

    let log_plugin = tauri_plugin_log::Builder::new()
        .level(log::LevelFilter::Debug)
        .timezone_strategy(tauri_plugin_log::TimezoneStrategy::UseUtc)
        .target(tauri_plugin_log::Target::new(
            tauri_plugin_log::TargetKind::Stdout,
        ))
        .target(tauri_plugin_log::Target::new(
            tauri_plugin_log::TargetKind::LogDir {
                file_name: Some(logging::APP_LOG_FILE.to_string()),
            },
        ))
        .max_file_size(logging::MAX_LOG_BYTES)
        .rotation_strategy(tauri_plugin_log::RotationStrategy::KeepSome(
            logging::KEEP_LOG_FILES,
        ))
        .build();

    tauri::Builder::default()
        .plugin(log_plugin)
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(session::Session::default())
        .manage(windowing::Host::new())
        .manage(lobby::Lobby::default())
        .setup(|app| {
            use tauri::Manager;
            let dir = app.path().app_config_dir()?;
            migrate_legacy_config_dir(&dir);

            let config_path = config::config_path(dir.clone());
            let mut cfg = config::Config::load(&config_path);
            if cfg.reset_developer_mode() {
                log::info!("developer mode reset for this session");
                if let Err(err) = cfg.save(&config_path) {
                    log::warn!("cannot persist developer-mode reset: {err}");
                }
            }

            // Identify the instance in the title bar. Several instances can run on one machine
            // (lobby host + joiners, or the dev pair) and are otherwise indistinguishable, so the
            // netplay nickname is the one stable label a window can carry regardless of the role
            // it later takes. Role/host is still chosen at runtime.
            if let Some(window) = app.get_webview_window("main") {
                let nickname = config::netplay_nickname(&cfg, None);
                if let Err(err) = window.set_title(&format!("The Cabinet — {nickname}")) {
                    log::warn!("cannot set the window title: {err}");
                }
            }

            let verbose = verbose_logging_requested(app.handle());
            log::set_max_level(if verbose {
                log::LevelFilter::Debug
            } else {
                log::LevelFilter::Info
            });
            log::info!(
                "the-cabinet {} starting ({} {}) verbose={}",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH,
                verbose
            );
            match logging::app_log_dir(app.handle()) {
                Ok(dir) => log::info!("log dir: {}", dir.display()),
                Err(err) => log::warn!("{err}"),
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::config::get_config,
            commands::config::set_config,
            commands::list_peers,
            commands::peer_health,
            commands::peers_health,
            commands::list_roms,
            commands::launch::launcher_info,
            commands::launch::parity_status,
            commands::launch::launch_match,
            commands::launch::launch_dev_pair,
            commands::launch::download_retroarch_core,
            commands::launch::retroarch_hotkey_map,
            commands::launch::stop_match,
            commands::launch::match_status,
            commands::lobby::lobby_start,
            commands::lobby::lobby_stop,
            commands::lobby::lobby_status,
            commands::lobby::lobby_room,
            commands::lobby::lobby_query,
            commands::lobby::lobby_discover,
            commands::lobby::lobby_join,
            commands::cabinet::cabinet_status,
            commands::cabinet::cabinet_place,
            commands::cabinet::cabinet_release,
            commands::cabinet::cabinet_request_permission,
            commands::probe_port,
            diagnostics::log_dir,
            diagnostics::open_logs_dir,
            diagnostics::collect_diagnostics,
            diagnostics::log_frontend,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
        let root =
            std::env::temp_dir().join(format!("the-cabinet-migrate-{tag}-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        let legacy = root.join(LEGACY_IDENTIFIER);
        let current = root.join("com.the-cabinet.app");
        std::fs::create_dir_all(&legacy).unwrap();
        (root, legacy, current)
    }

    #[test]
    fn migrates_missing_files_from_legacy_dir() {
        let (root, legacy, current) = scratch("copy");
        std::fs::write(legacy.join("config.json"), "{}").unwrap();

        migrate_legacy_config_dir(&current);

        assert_eq!(
            std::fs::read_to_string(current.join("config.json")).unwrap(),
            "{}"
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn does_not_overwrite_existing_files() {
        let (root, legacy, current) = scratch("keep");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(legacy.join("config.json"), "old").unwrap();
        std::fs::write(current.join("config.json"), "new").unwrap();

        migrate_legacy_config_dir(&current);

        assert_eq!(
            std::fs::read_to_string(current.join("config.json")).unwrap(),
            "new"
        );
        std::fs::remove_dir_all(&root).ok();
    }
}
