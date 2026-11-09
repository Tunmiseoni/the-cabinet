use super::{Network, NetworkProvider, PeerHealth};
use crate::config::Config;
use std::path::PathBuf;

pub struct TailscaleProvider {
    binary: PathBuf,
}

impl TailscaleProvider {
    pub fn new(binary: PathBuf) -> Self {
        Self { binary }
    }

    pub fn from_config(cfg: &Config) -> crate::error::Result<Self> {
        Ok(Self::new(crate::tailscale::resolve_binary(cfg)?))
    }
}

impl NetworkProvider for TailscaleProvider {
    fn status(&self) -> crate::error::Result<Network> {
        crate::tailscale::status(&self.binary)
    }

    fn health(&self, address: &str) -> PeerHealth {
        crate::tailscale::ping(&self.binary, address)
    }

    fn health_many(&self, addresses: &[String]) -> Vec<PeerHealth> {
        crate::tailscale::ping_many(&self.binary, addresses)
    }
}
