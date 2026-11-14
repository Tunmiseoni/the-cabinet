use super::{home, place, require_root};
use crate::engine::Engine;
use crate::firewall::Programs;
use crate::protocol::LABEL;
use crate::server::{Config, Server};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

pub const SOCKET: &str = "/run/com.the-cabinet.helper.sock";
pub const STATE_DIR: &str = "/var/lib/com.the-cabinet.helper";
pub const HOSTS: &str = "/etc/hosts";
pub const TOOLS_DIR: &str = "/usr/local/libexec/the-cabinet";
pub const INSTALLED: &str = "/usr/local/libexec/the-cabinet/com.the-cabinet.helper";
pub const UNIT: &str = "/etc/systemd/system/com.the-cabinet.helper.service";
pub const APP_DATA: &str = ".local/share/com.the-cabinet.app";
const SYSTEMCTL: &[&str] = &["/usr/bin/systemctl", "/bin/systemctl"];
const STARTUP: Duration = Duration::from_secs(10);

pub fn unit(uid: u32) -> String {
    format!(
        "[Unit]
Description=The Cabinet's privileged helper

[Service]
ExecStart={INSTALLED} daemon --uid {uid}
Restart=always
RestartSec=1
UMask=0077

[Install]
WantedBy=multi-user.target
"
    )
}

fn service() -> String {
    format!("{LABEL}.service")
}

pub fn programs(uid: u32) -> Option<Programs> {
    let Some(home) = home(uid) else {
        log::warn!("uid {uid} has no home folder; program changes are off");
        return None;
    };
    Some(Programs {
        firewall: None,
        port_firewall: Some(Box::new(crate::firewall::Ufw)),
        rule_firewall: None,
        low_ports: Some(Box::new(crate::lowports::FileCapability)),
        root: home.join(APP_DATA),
        uid: Some(uid),
    })
}

fn anchors() -> Option<Box<dyn crate::anchors::Anchors>> {
    let found = crate::anchors::DirAnchors::system();
    if found.is_none() {
        log::warn!("no system certificate folder found; certificate authorities are off");
    }
    found.map(|anchors| Box::new(anchors) as Box<dyn crate::anchors::Anchors>)
}

fn installed_uid() -> Option<u32> {
    std::fs::read_to_string(UNIT)
        .ok()?
        .split(" daemon --uid ")
        .nth(1)?
        .lines()
        .next()?
        .trim()
        .parse()
        .ok()
}

fn systemctl(args: &[&str]) -> Result<(), String> {
    let program = SYSTEMCTL
        .iter()
        .find(|path| Path::new(path).exists())
        .ok_or("this system has no systemctl, so the helper cannot run as a service")?;
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|err| format!("cannot run systemctl: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

pub fn daemon(uid: u32) -> Result<(), String> {
    require_root()?;
    unsafe { libc::umask(0o077) };
    let server = Server::bind(Config {
        endpoint: SOCKET.into(),
        state_dir: STATE_DIR.into(),
        hosts: HOSTS.into(),
        user: Some(uid),
        flush: false,
        programs: programs(uid),
        anchors: anchors(),
    })?;
    log::info!("listening on {SOCKET} for uid {uid}");
    server.run();
    Ok(())
}

pub fn install(uid: u32) -> Result<(), String> {
    require_root()?;
    let source = std::env::current_exe().map_err(|err| format!("cannot find myself: {err}"))?;
    let binary =
        std::fs::read(&source).map_err(|err| format!("cannot read {}: {err}", source.display()))?;
    std::fs::create_dir_all(TOOLS_DIR)
        .map_err(|err| format!("cannot create {TOOLS_DIR}: {err}"))?;
    let _ = systemctl(&["stop", &service()]);
    let _ = std::fs::remove_file(SOCKET);
    place(Path::new(INSTALLED), &binary, 0o755)?;
    place(Path::new(UNIT), unit(uid).as_bytes(), 0o644)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", &service()])?;
    let deadline = Instant::now() + STARTUP;
    while !Path::new(SOCKET).exists() {
        if Instant::now() > deadline {
            return Err(format!(
                "the helper service did not start; see `journalctl -u {}`",
                service()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    require_root()?;
    let uid = installed_uid();
    let _ = systemctl(&["disable", "--now", &service()]);
    let mut problems = Vec::new();
    if Path::new(STATE_DIR).exists() {
        match Engine::open(Path::new(STATE_DIR), Path::new(HOSTS), false).map(|engine| {
            engine
                .with_programs(uid.and_then(programs))
                .with_anchors(anchors())
        }) {
            Ok(mut engine) => {
                let (_, failures) = engine.undo_where(|_| true);
                problems.extend(failures);
                if let Err(err) = engine.drop_orphans() {
                    problems.push(err);
                }
            }
            Err(err) => problems.push(err),
        }
    }
    if !problems.is_empty() {
        return Err(format!(
            "some changes could not be undone, so the journal in {STATE_DIR} is kept: {}",
            problems.join("; ")
        ));
    }
    for path in [UNIT, SOCKET, INSTALLED] {
        if let Err(err) = std::fs::remove_file(path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                problems.push(format!("cannot remove {path}: {err}"));
            }
        }
    }
    let _ = std::fs::remove_dir(TOOLS_DIR);
    if let Err(err) = std::fs::remove_dir_all(STATE_DIR) {
        if err.kind() != std::io::ErrorKind::NotFound {
            problems.push(format!("cannot remove {STATE_DIR}: {err}"));
        }
    }
    let _ = systemctl(&["daemon-reload"]);
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unit_starts_the_installed_helper_for_one_user() {
        let unit = unit(1000);
        assert!(unit.contains(
            "ExecStart=/usr/local/libexec/the-cabinet/com.the-cabinet.helper daemon --uid 1000\n"
        ));
        assert!(unit.contains("Restart=always\n"));
        assert_eq!(service(), "com.the-cabinet.helper.service");
    }
}
