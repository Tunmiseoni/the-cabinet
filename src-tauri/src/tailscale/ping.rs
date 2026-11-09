use super::cli_command;
use crate::net::{PathKind, PeerHealth};
use std::path::PathBuf;

pub fn ping(binary: &PathBuf, ip: &str) -> PeerHealth {
    let output = cli_command(binary)
        .args(["ping", "--c", "1", "--timeout", "2s", ip])
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            let combined = format!("{stdout}{stderr}");
            parse_ping(ip, &combined)
        }
        Err(err) => PeerHealth {
            ip: ip.to_string(),
            ok: false,
            path: PathKind::Unknown,
            rtt_ms: None,
            relay_code: None,
            raw: format!("failed to run tailscale ping: {err}"),
        },
    }
}

pub fn ping_many(binary: &PathBuf, ips: &[String]) -> Vec<PeerHealth> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = ips
            .iter()
            .map(|ip| scope.spawn(|| ping(binary, ip)))
            .collect();
        handles
            .into_iter()
            .zip(ips)
            .map(|(handle, ip)| {
                handle.join().unwrap_or_else(|_| PeerHealth {
                    ip: ip.clone(),
                    ok: false,
                    path: PathKind::Unknown,
                    rtt_ms: None,
                    relay_code: None,
                    raw: "ping worker panicked".to_string(),
                })
            })
            .collect()
    })
}

pub fn parse_ping(ip: &str, raw: &str) -> PeerHealth {
    let trimmed = raw.trim().to_string();

    if trimmed.contains("is local Tailscale IP") {
        return PeerHealth {
            ip: ip.to_string(),
            ok: true,
            path: PathKind::Local,
            rtt_ms: Some(0.0),
            relay_code: None,
            raw: trimmed,
        };
    }

    let lower = trimmed.to_lowercase();
    let timed_out = lower.contains("timed out") || lower.contains("no reply");
    if timed_out {
        return PeerHealth {
            ip: ip.to_string(),
            ok: false,
            path: PathKind::Unknown,
            rtt_ms: None,
            relay_code: None,
            raw: trimmed,
        };
    }

    let rtt_ms = extract_rtt_ms(&trimmed);
    let relay_code = extract_relay_code(&trimmed);

    let path = if relay_code.is_some() {
        PathKind::Relay
    } else if trimmed.contains(" via ") {
        PathKind::Direct
    } else {
        PathKind::Unknown
    };

    let ok = rtt_ms.is_some() && path != PathKind::Unknown;

    PeerHealth {
        ip: ip.to_string(),
        ok,
        path,
        rtt_ms,
        relay_code,
        raw: trimmed,
    }
}

fn extract_rtt_ms(raw: &str) -> Option<f64> {
    let marker = " in ";
    let tail = raw.get(raw.rfind(marker)? + marker.len()..)?;
    let number: String = tail
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    number.parse::<f64>().ok()
}

fn extract_relay_code(raw: &str) -> Option<String> {
    let start = raw.find("DERP(")? + "DERP(".len();
    let rest = raw.get(start..)?;
    let end = rest.find(')')?;
    Some(rest.get(..end)?.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tailscale::{resolve_binary, status};

    #[test]
    fn parses_direct_ping() {
        let health = parse_ping(
            "100.64.0.2",
            "pong from cachyos-host (100.64.0.2) via 192.0.2.101:41641 in 12ms",
        );
        assert!(health.ok);
        assert_eq!(health.path, PathKind::Direct);
        assert_eq!(health.rtt_ms, Some(12.0));
        assert_eq!(health.relay_code, None);
    }

    #[test]
    fn parses_derp_ping() {
        let health = parse_ping(
            "100.64.0.2",
            "pong from cachyos-host (100.64.0.2) via DERP(lhr) in 329ms",
        );
        assert!(health.ok);
        assert_eq!(health.path, PathKind::Relay);
        assert_eq!(health.rtt_ms, Some(329.0));
        assert_eq!(health.relay_code.as_deref(), Some("lhr"));
    }

    #[test]
    fn parses_local_ping() {
        let health = parse_ping("100.64.0.1", "100.64.0.1 is local Tailscale IP");
        assert!(health.ok);
        assert_eq!(health.path, PathKind::Local);
    }

    #[test]
    fn parses_timeout() {
        let health = parse_ping(
            "100.64.0.2",
            "ping \"100.64.0.2\" timed out\n2026/09/18 23:48:45 no reply",
        );
        assert!(!health.ok);
        assert_eq!(health.path, PathKind::Unknown);
        assert_eq!(health.rtt_ms, None);
    }

    #[test]
    fn parses_fractional_rtt() {
        let health = parse_ping(
            "100.64.0.2",
            "pong from cachyos-host (100.64.0.2) via [fd7a::1]:41641 in 4.5ms",
        );
        assert_eq!(health.rtt_ms, Some(4.5));
        assert_eq!(health.path, PathKind::Direct);
    }

    #[cfg(unix)]
    #[test]
    fn ping_many_preserves_order() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("cabinet-ping-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let script = dir.join("faketailscale");
        std::fs::write(
            &script,
            "#!/bin/sh\necho \"pong from fake ($6) via 1.2.3.4:41641 in 7ms\"\n",
        )
        .expect("write fake binary");
        let mut perms = std::fs::metadata(&script).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod");

        let ips = vec!["100.0.0.1".to_string(), "100.0.0.2".to_string()];
        let results = ping_many(&script, &ips);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].ip, "100.0.0.1");
        assert_eq!(results[1].ip, "100.0.0.2");
        assert!(results.iter().all(|h| h.ok && h.path == PathKind::Direct));
        assert_eq!(results[0].rtt_ms, Some(7.0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    #[ignore = "requires a running local tailscale"]
    fn live_ping_smoke() {
        let cfg = Config::default();
        let binary = resolve_binary(&cfg).expect("tailscale binary should resolve");
        let tailnet = status(&binary).expect("status should run");
        let self_ip = tailnet.self_peer.expect("self node").ip;
        let results = ping_many(&binary, std::slice::from_ref(&self_ip));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, PathKind::Local);
        println!("ping self -> {:?}", results[0]);
    }
}
