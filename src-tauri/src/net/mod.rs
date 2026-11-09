mod tailscale;

pub use tailscale::TailscaleProvider;

use crate::config::Config;
use serde::Serialize;
use std::sync::Arc;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Node {
    pub node_id: String,
    pub hostname: String,
    pub dns_name: String,
    pub os: String,
    pub ip: String,
    pub ips: Vec<String>,
    pub online: bool,
    pub active: bool,
    pub cur_addr: String,
    pub relay: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub last_seen: String,
    pub is_self: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Network {
    pub backend_state: String,
    pub self_peer: Option<Node>,
    pub peers: Vec<Node>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PathKind {
    Direct,
    Relay,
    Local,
    Unknown,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerHealth {
    pub ip: String,
    pub ok: bool,
    pub path: PathKind,
    pub rtt_ms: Option<f64>,
    pub relay_code: Option<String>,
    pub raw: String,
}

pub trait NetworkProvider: Send + Sync {
    fn status(&self) -> crate::error::Result<Network>;

    fn health(&self, address: &str) -> PeerHealth;

    fn health_many(&self, addresses: &[String]) -> Vec<PeerHealth> {
        addresses
            .iter()
            .map(|address| self.health(address))
            .collect()
    }

    fn self_node(&self) -> crate::error::Result<Option<Node>> {
        Ok(self.status()?.self_peer)
    }

    fn peers(&self) -> crate::error::Result<Vec<Node>> {
        Ok(self.status()?.peers)
    }

    #[allow(dead_code)]
    fn address(&self, node_id: &str) -> crate::error::Result<Option<String>> {
        let network = self.status()?;
        Ok(network
            .self_peer
            .into_iter()
            .chain(network.peers)
            .find(|node| node.node_id == node_id)
            .map(|node| node.ip)
            .filter(|ip| !ip.trim().is_empty()))
    }

    fn self_address(&self) -> Option<String> {
        self.self_node()
            .ok()
            .flatten()
            .map(|node| node.ip)
            .filter(|ip| !ip.trim().is_empty())
    }
}

pub type SharedProvider = Arc<dyn NetworkProvider>;

pub fn provider(cfg: &Config) -> crate::error::Result<SharedProvider> {
    Ok(Arc::new(TailscaleProvider::from_config(cfg)?))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    pub node_id: String,
    pub instance: Option<String>,
}

impl Identity {
    pub fn new(node_id: impl Into<String>, instance: Option<String>) -> Self {
        Self {
            node_id: node_id.into(),
            instance: instance.filter(|value| !value.trim().is_empty()),
        }
    }

    pub fn member_id(&self) -> String {
        match &self.instance {
            Some(instance) => format!("{}#{instance}", self.node_id),
            None => self.node_id.clone(),
        }
    }
}

pub fn local_identity(node: &Node) -> Option<Identity> {
    let node_id = node.node_id.trim();
    (!node_id.is_empty()).then(|| Identity::new(node_id, crate::env::instance()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, ip: &str, is_self: bool) -> Node {
        Node {
            node_id: id.to_string(),
            hostname: id.to_string(),
            dns_name: String::new(),
            os: String::new(),
            ip: ip.to_string(),
            ips: vec![ip.to_string()],
            online: true,
            active: false,
            cur_addr: String::new(),
            relay: String::new(),
            rx_bytes: 0,
            tx_bytes: 0,
            last_seen: String::new(),
            is_self,
        }
    }

    struct Fixed(Network);

    impl NetworkProvider for Fixed {
        fn status(&self) -> crate::error::Result<Network> {
            Ok(self.0.clone())
        }

        fn health(&self, address: &str) -> PeerHealth {
            PeerHealth {
                ip: address.to_string(),
                ok: true,
                path: PathKind::Direct,
                rtt_ms: Some(1.0),
                relay_code: None,
                raw: String::new(),
            }
        }
    }

    fn fixed() -> Fixed {
        Fixed(Network {
            backend_state: "Running".to_string(),
            self_peer: Some(node("nSELF", "100.64.0.1", true)),
            peers: vec![node("nLINUX", "100.64.0.2", false), node("nWIN", "", false)],
        })
    }

    #[test]
    fn resolves_addresses_by_node_id() {
        let provider = fixed();
        assert_eq!(
            provider.address("nSELF").unwrap().as_deref(),
            Some("100.64.0.1")
        );
        assert_eq!(
            provider.address("nLINUX").unwrap().as_deref(),
            Some("100.64.0.2")
        );
        assert_eq!(provider.address("nWIN").unwrap(), None);
        assert_eq!(provider.address("missing").unwrap(), None);
        assert_eq!(provider.self_address().as_deref(), Some("100.64.0.1"));
    }

    #[test]
    fn exposes_self_and_peers() {
        let provider = fixed();
        assert_eq!(provider.self_node().unwrap().unwrap().node_id, "nSELF");
        assert_eq!(provider.peers().unwrap().len(), 2);
        let health = provider.health_many(&["100.64.0.2".to_string()]);
        assert_eq!(health.len(), 1);
        assert!(health[0].ok);
    }

    #[test]
    fn member_id_carries_the_instance_suffix() {
        assert_eq!(Identity::new("nSELF", None).member_id(), "nSELF");
        assert_eq!(
            Identity::new("nSELF", Some("2".to_string())).member_id(),
            "nSELF#2"
        );
        assert_eq!(
            Identity::new("nSELF", Some("  ".to_string())).member_id(),
            "nSELF"
        );
    }

    #[test]
    fn local_identity_needs_a_node_id() {
        assert!(local_identity(&node("  ", "100.64.0.1", true)).is_none());
        assert_eq!(
            local_identity(&node("nSELF", "100.64.0.1", true))
                .unwrap()
                .node_id,
            "nSELF"
        );
    }
}
