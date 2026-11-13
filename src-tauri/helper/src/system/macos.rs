use super::{home, place, require_root};
use crate::engine::Engine;
use crate::firewall::{Programs, SocketFilter};
use crate::protocol::LABEL;
use crate::server::{Config, Server};
use std::path::Path;
use std::process::Command;

pub const SOCKET: &str = "/var/run/com.the-cabinet.helper.sock";
pub const STATE_DIR: &str = "/Library/Application Support/com.the-cabinet.helper";
pub const HOSTS: &str = "/etc/hosts";
pub const TOOLS_DIR: &str = "/Library/PrivilegedHelperTools";
pub const INSTALLED: &str = "/Library/PrivilegedHelperTools/com.the-cabinet.helper";
pub const PLIST: &str = "/Library/LaunchDaemons/com.the-cabinet.helper.plist";
pub const LOG: &str = "/Library/Logs/com.the-cabinet.helper.log";
pub const APP_DATA: &str = "Library/Application Support/com.the-cabinet.app";
const LAUNCHCTL: &str = "/bin/launchctl";

pub fn plist(uid: u32) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{INSTALLED}</string>
		<string>daemon</string>
		<string>--uid</string>
		<string>{uid}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Background</string>
	<key>StandardErrorPath</key>
	<string>{LOG}</string>
</dict>
</plist>
"#
    )
}

pub fn programs(uid: u32) -> Option<Programs> {
    let Some(home) = home(uid) else {
        log::warn!("uid {uid} has no home folder; firewall changes are off");
        return None;
    };
    Some(Programs {
        firewall: Some(Box::new(SocketFilter)),
        port_firewall: None,
        low_ports: None,
        root: home.join(APP_DATA),
        uid: Some(uid),
    })
}

fn installed_uid() -> Option<u32> {
    let plist = std::fs::read_to_string(PLIST).ok()?;
    let after = plist.split("<string>--uid</string>").nth(1)?;
    after
        .split("<string>")
        .nth(1)?
        .split("</string>")
        .next()?
        .parse()
        .ok()
}

fn launchctl(args: &[&str]) -> Result<(), String> {
    let output = Command::new(LAUNCHCTL)
        .args(args)
        .output()
        .map_err(|err| format!("cannot run launchctl: {err}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "launchctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn service() -> String {
    format!("system/{LABEL}")
}

pub fn daemon(uid: u32) -> Result<(), String> {
    require_root()?;
    unsafe { libc::umask(0o077) };
    let server = Server::bind(Config {
        endpoint: SOCKET.into(),
        state_dir: STATE_DIR.into(),
        hosts: HOSTS.into(),
        user: Some(uid),
        flush: true,
        programs: programs(uid),
        anchors: None,
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
    let _ = launchctl(&["bootout", &service()]);
    place(Path::new(INSTALLED), &binary, 0o755)?;
    place(Path::new(PLIST), plist(uid).as_bytes(), 0o644)?;
    let mut attempt = 0;
    loop {
        match launchctl(&["bootstrap", "system", PLIST]) {
            Ok(()) => return Ok(()),
            Err(err) if attempt == 4 => return Err(err),
            Err(_) => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
    }
}

pub fn uninstall() -> Result<(), String> {
    require_root()?;
    let uid = installed_uid();
    let _ = launchctl(&["bootout", &service()]);
    let mut problems = Vec::new();
    if Path::new(STATE_DIR).exists() {
        match Engine::open(Path::new(STATE_DIR), Path::new(HOSTS), true)
            .map(|engine| engine.with_programs(uid.and_then(programs)))
        {
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
    for path in [PLIST, SOCKET, LOG, INSTALLED] {
        if let Err(err) = std::fs::remove_file(path) {
            if err.kind() != std::io::ErrorKind::NotFound {
                problems.push(format!("cannot remove {path}: {err}"));
            }
        }
    }
    if let Err(err) = std::fs::remove_dir_all(STATE_DIR) {
        if err.kind() != std::io::ErrorKind::NotFound {
            problems.push(format!("cannot remove {STATE_DIR}: {err}"));
        }
    }
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
    fn the_plist_starts_the_installed_helper_for_one_user() {
        let plist = plist(501);
        assert!(plist.contains("<string>com.the-cabinet.helper</string>"));
        let after = plist.split("<string>--uid</string>").nth(1).unwrap();
        assert!(after.trim_start().starts_with("<string>501</string>"));
        assert!(plist.contains(
            "<string>/Library/PrivilegedHelperTools/com.the-cabinet.helper</string>\n\t\t<string>daemon</string>\n\t\t<string>--uid</string>\n\t\t<string>501</string>"
        ));
    }
}
