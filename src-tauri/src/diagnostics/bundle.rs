use super::redact::redact;
use crate::logging;
use crate::providers;
use crate::session;
use std::path::{Path, PathBuf};
use tauri::AppHandle;

pub(crate) fn diagnostics_text(app: &AppHandle) -> String {
    use std::fmt::Write as _;

    let cfg = crate::commands::load_config(app).unwrap_or_default();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "The Cabinet diagnostics — {}",
        crate::time::utc_stamp()
    );
    let _ = writeln!(
        out,
        "NOTE: the raw app-log tail at the end is not redacted; review this bundle before sharing it publicly."
    );
    let _ = writeln!(out, "version: {}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(
        out,
        "platform: {} {}",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    match logging::app_log_dir(app) {
        Ok(dir) => {
            let _ = writeln!(out, "log dir: {}", dir.display());
        }
        Err(err) => {
            let _ = writeln!(out, "log dir error: {err}");
        }
    }

    let _ = writeln!(out, "\n--- config ---");
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&cfg).unwrap_or_else(|_| "<unavailable>".to_string())
    );

    let _ = writeln!(out, "\n--- provider ---");
    match providers::resolve_provider(app, &cfg, false) {
        Ok(provider) => {
            match provider.detect() {
                Ok(install) => {
                    let _ = writeln!(
                        out,
                        "install: {} installed={} detail={}",
                        install.label, install.installed, install.detail
                    );
                }
                Err(err) => {
                    let _ = writeln!(out, "install error: {err}");
                }
            }
            let caps = provider.capabilities();
            let _ = writeln!(
                out,
                "capabilities: spectate={} devPair={}",
                caps.spectate, caps.dev_pair
            );
        }
        Err(err) => {
            let _ = writeln!(out, "provider error: {err}");
        }
    }

    let _ = writeln!(out, "\n--- tailnet ---");
    match crate::net::provider(&cfg) {
        Ok(network) => match network.status() {
            Ok(tailnet) => {
                let online = tailnet.peers.iter().filter(|peer| peer.online).count();
                let _ = writeln!(out, "backendState: {}", tailnet.backend_state);
                let _ = writeln!(out, "peers: {} ({online} online)", tailnet.peers.len());
            }
            Err(err) => {
                let _ = writeln!(out, "status error: {err}");
            }
        },
        Err(err) => {
            let _ = writeln!(out, "tailscale binary error: {err}");
        }
    }

    let _ = writeln!(out, "\n--- last match ---");
    let _ = writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&session::status(app))
            .unwrap_or_else(|_| "<unavailable>".to_string())
    );

    let _ = writeln!(out, "\n--- sessions ---");
    match logging::sessions_dir(app) {
        Ok(sessions) => match std::fs::read_dir(&sessions) {
            Ok(entries) => {
                let mut dirs: Vec<PathBuf> = entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.is_dir())
                    .collect();
                dirs.sort();
                for dir in dirs
                    .iter()
                    .rev()
                    .take(crate::constants::DIAGNOSTICS_SESSION_LIST_LIMIT)
                {
                    let _ = writeln!(out, "{}", dir.display());
                    if let Ok(files) = std::fs::read_dir(dir) {
                        for file in files.flatten() {
                            let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                            let _ = writeln!(out, "  {} ({size} bytes)", file.path().display());
                        }
                    }
                }
            }
            Err(err) => {
                let _ = writeln!(out, "cannot list sessions: {err}");
            }
        },
        Err(err) => {
            let _ = writeln!(out, "{err}");
        }
    }

    if let Ok(sessions) = logging::sessions_dir(app) {
        if let Some(latest) = latest_session_dir(&sessions) {
            let logs = emulator_logs(&latest);
            if !logs.is_empty() {
                let _ = writeln!(
                    out,
                    "\n--- latest session emulator logs ({}) ---",
                    latest.display()
                );
                for log in logs {
                    let name = log
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("emulator");
                    let _ = writeln!(
                        out,
                        "\n### {name} (last {} lines)",
                        crate::constants::DIAGNOSTICS_LOG_TAIL_LINES
                    );
                    let _ = writeln!(
                        out,
                        "{}",
                        tail_text(&log, crate::constants::DIAGNOSTICS_LOG_TAIL_LINES)
                    );
                }
            }
        }
    }

    let tail = match logging::app_log_path(app) {
        Ok(path) => tail_text(&path, crate::constants::DIAGNOSTICS_LOG_TAIL_LINES),
        Err(err) => err.to_string(),
    };
    format!(
        "{}\n--- recent app log (last {} lines) ---\n{tail}",
        redact(&out, &cfg),
        crate::constants::DIAGNOSTICS_LOG_TAIL_LINES
    )
}

fn latest_session_dir(sessions: &Path) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(sessions)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs.pop()
}

fn emulator_logs(dir: &Path) -> Vec<PathBuf> {
    let mut logs: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("emulator-"))
                .unwrap_or(false)
        })
        .collect();
    logs.sort();
    logs
}

fn tail_text(path: &Path, max_lines: usize) -> String {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return format!("(no log at {})", path.display());
    };
    let lines: Vec<&str> = raw.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_text_returns_the_last_lines() {
        let path = std::env::temp_dir().join(format!("cabinet-tail-{}.txt", std::process::id()));
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        assert_eq!(tail_text(&path, 2), "c\nd");
        assert_eq!(tail_text(&path, 10), "a\nb\nc\nd");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn tail_text_reports_a_missing_file() {
        let path = std::env::temp_dir().join("cabinet-definitely-missing.txt");
        assert!(tail_text(&path, 5).starts_with("(no log at"));
    }

    #[test]
    fn latest_session_dir_picks_the_newest() {
        let root = std::env::temp_dir().join(format!("cabinet-latest-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        for name in ["20260101-000000", "20260103-000000", "20260102-000000"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("stray.txt"), b"x").unwrap();

        assert_eq!(
            latest_session_dir(&root),
            Some(root.join("20260103-000000"))
        );

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn emulator_logs_filters_and_sorts() {
        let root = std::env::temp_dir().join(format!("cabinet-logs-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).unwrap();
        for name in ["emulator-p2.log", "emulator-p1.log", "notes.txt"] {
            std::fs::write(root.join(name), b"x").unwrap();
        }

        let logs = emulator_logs(&root);
        assert_eq!(logs.len(), 2);
        assert!(logs[0].ends_with("emulator-p1.log"));
        assert!(logs[1].ends_with("emulator-p2.log"));

        std::fs::remove_dir_all(&root).ok();
    }
}
