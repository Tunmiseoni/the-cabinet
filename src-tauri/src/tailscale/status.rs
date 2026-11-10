use super::cli_command;
use crate::net::{Network, Node};
use crate::sync::MutexExt;
use serde::{Deserialize, Deserializer};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Deserialize)]
struct RawStatus {
    #[serde(rename = "BackendState", default, deserialize_with = "nullable")]
    backend_state: String,
    #[serde(rename = "Self", default, deserialize_with = "nullable")]
    self_node: Option<RawPeer>,
    #[serde(rename = "Peer", default, deserialize_with = "nullable")]
    peers: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Deserialize)]
struct RawPeer {
    #[serde(rename = "ID", default, deserialize_with = "nullable")]
    id: String,
    #[serde(rename = "HostName", default, deserialize_with = "nullable")]
    hostname: String,
    #[serde(rename = "DNSName", default, deserialize_with = "nullable")]
    dns_name: String,
    #[serde(rename = "OS", default, deserialize_with = "nullable")]
    os: String,
    #[serde(rename = "TailscaleIPs", default, deserialize_with = "nullable")]
    ips: Vec<String>,
    #[serde(rename = "Online", default, deserialize_with = "nullable")]
    online: bool,
    #[serde(rename = "Active", default, deserialize_with = "nullable")]
    active: bool,
    #[serde(rename = "CurAddr", default, deserialize_with = "nullable")]
    cur_addr: String,
    #[serde(rename = "Relay", default, deserialize_with = "nullable")]
    relay: String,
    #[serde(rename = "RxBytes", default, deserialize_with = "nullable")]
    rx_bytes: u64,
    #[serde(rename = "TxBytes", default, deserialize_with = "nullable")]
    tx_bytes: u64,
    #[serde(rename = "LastSeen", default, deserialize_with = "nullable")]
    last_seen: String,
}

impl RawPeer {
    fn into_peer(self, is_self: bool, fallback_id: String) -> Node {
        let node_id = if self.id.is_empty() {
            fallback_id
        } else {
            self.id
        };
        let ip = self
            .ips
            .iter()
            .find(|ip| !ip.contains(':'))
            .cloned()
            .or_else(|| self.ips.first().cloned())
            .unwrap_or_default();
        Node {
            node_id,
            hostname: self.hostname,
            dns_name: self.dns_name,
            os: self.os,
            ip,
            ips: self.ips,
            online: self.online,
            active: self.active,
            cur_addr: self.cur_addr,
            relay: self.relay,
            rx_bytes: self.rx_bytes,
            tx_bytes: self.tx_bytes,
            last_seen: self.last_seen,
            is_self,
        }
    }
}

fn run(binary: &PathBuf, args: &[&str]) -> crate::error::Result<String> {
    let output = cli_command(binary)
        .args(args)
        .output()
        .map_err(|err| format!("failed to run tailscale: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    if stdout.trim().is_empty() && !output.status.success() {
        let message = if stderr.trim().is_empty() {
            "tailscale command failed".to_string()
        } else {
            stderr.trim().to_string()
        };
        return Err(message.into());
    }
    Ok(stdout)
}

fn status_cache() -> &'static Mutex<Option<(PathBuf, Instant, Network)>> {
    static CACHE: OnceLock<Mutex<Option<(PathBuf, Instant, Network)>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

pub fn status(binary: &PathBuf) -> crate::error::Result<Network> {
    if let Some((cached_binary, at, tailnet)) = status_cache().lock_or_recover().as_ref() {
        if cached_binary == binary && at.elapsed() < crate::constants::TAILSCALE_STATUS_CACHE_TTL {
            return Ok(tailnet.clone());
        }
    }

    let raw = run(binary, &["status", "--json"])?;
    let tailnet = parse_status(&raw)?;
    *status_cache().lock_or_recover() = Some((binary.clone(), Instant::now(), tailnet.clone()));
    Ok(tailnet)
}

pub fn parse_status(raw: &str) -> crate::error::Result<Network> {
    let parsed: RawStatus =
        serde_json::from_str(raw).map_err(|err| format!("invalid status JSON: {err}"))?;

    let self_peer = parsed
        .self_node
        .map(|node| node.into_peer(true, String::new()));

    let mut peers: Vec<Node> = parsed
        .peers
        .map(|map| {
            map.into_iter()
                .filter_map(|(id, value)| {
                    serde_json::from_value::<RawPeer>(value)
                        .ok()
                        .map(|node| node.into_peer(false, id))
                })
                .collect()
        })
        .unwrap_or_default();

    peers.sort_by(|a, b| {
        b.online
            .cmp(&a.online)
            .then_with(|| a.hostname.to_lowercase().cmp(&b.hostname.to_lowercase()))
    });

    Ok(Network {
        backend_state: parsed.backend_state,
        self_peer,
        peers,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tailscale::resolve_binary;

    const STATUS_FIXTURE: &str = r#"{
      "Version": "1.102.4",
      "BackendState": "Running",
      "Self": {
        "ID": "nSELF",
        "HostName": "mac-host",
        "DNSName": "mac-host.example-tailnet.ts.net.",
        "OS": "macOS",
        "TailscaleIPs": ["100.64.0.1", "fd7a:115c:a1e0::1"],
        "Online": true,
        "Active": false,
        "CurAddr": "",
        "Relay": "",
        "RxBytes": 100,
        "TxBytes": 200,
        "LastSeen": "0001-01-01T00:00:00Z"
      },
      "Peer": {
        "nodeA": {
          "ID": "nWIN",
          "HostName": "windows-host",
          "DNSName": "windows-host.example-tailnet.ts.net.",
          "OS": "windows",
          "TailscaleIPs": ["100.64.0.3", "fd7a:115c:a1e0::3"],
          "Online": false,
          "Active": false,
          "CurAddr": "",
          "Relay": "lhr",
          "RxBytes": 0,
          "TxBytes": 0,
          "LastSeen": "2026-09-18T21:51:35.1Z"
        },
        "nodeB": {
          "HostName": "cachyos-host",
          "DNSName": "cachyos-host.example-tailnet.ts.net.",
          "OS": "linux",
          "TailscaleIPs": ["100.64.0.2", "fd7a:115c:a1e0::2"],
          "Online": true,
          "Active": true,
          "CurAddr": "192.0.2.101:41641",
          "Relay": "lhr",
          "RxBytes": 5000,
          "TxBytes": 6000,
          "LastSeen": "2026-09-18T22:00:00Z"
        }
      }
    }"#;

    #[test]
    fn parses_status_and_orders_online_first() {
        let tailnet = parse_status(STATUS_FIXTURE).expect("should parse");
        assert_eq!(tailnet.backend_state, "Running");
        let self_peer = tailnet.self_peer.expect("self present");
        assert!(self_peer.is_self);
        assert_eq!(self_peer.ip, "100.64.0.1");
        assert_eq!(self_peer.node_id, "nSELF");
        assert_eq!(tailnet.peers.len(), 2);
        assert_eq!(tailnet.peers[0].hostname, "cachyos-host");
        assert!(tailnet.peers[0].online);
        assert_eq!(tailnet.peers[0].node_id, "nodeB");
        assert_eq!(tailnet.peers[1].hostname, "windows-host");
        assert_eq!(tailnet.peers[1].node_id, "nWIN");
        assert_eq!(tailnet.peers[0].ip, "100.64.0.2");
    }

    #[test]
    fn tolerates_stopped_backend() {
        let tailnet = parse_status(r#"{"BackendState":"Stopped"}"#).expect("should parse");
        assert_eq!(tailnet.backend_state, "Stopped");
        assert!(tailnet.self_peer.is_none());
        assert!(tailnet.peers.is_empty());
    }

    #[test]
    fn tolerates_null_lists_and_fields_when_stopped() {
        let tailnet = parse_status(
            r#"{"BackendState":"Stopped","Self":{"ID":"nSELF","HostName":"mac-host","TailscaleIPs":null,"Relay":null},"Peer":{"nodekey:b":{"ID":"nodeB","HostName":"cachyos-host","TailscaleIPs":["100.64.0.2"],"CurAddr":null,"Addrs":null}}}"#,
        )
        .expect("should parse");
        assert_eq!(tailnet.backend_state, "Stopped");
        let self_peer = tailnet.self_peer.expect("self present");
        assert_eq!(self_peer.node_id, "nSELF");
        assert_eq!(self_peer.ip, "");
        assert_eq!(tailnet.peers.len(), 1);
        assert_eq!(tailnet.peers[0].ip, "100.64.0.2");
    }

    #[test]
    #[ignore = "requires a running local tailscale"]
    fn live_status_smoke() {
        let cfg = Config::default();
        let binary = resolve_binary(&cfg).expect("tailscale binary should resolve");
        let tailnet = status(&binary).expect("status should run");
        assert!(tailnet.self_peer.is_some(), "self node should be present");
        println!(
            "backend={} self={:?} peers={}",
            tailnet.backend_state,
            tailnet.self_peer.as_ref().map(|p| &p.ip),
            tailnet.peers.len()
        );
    }
}
