use super::registry::CartridgeRef;
use super::trust::{self, Grant};
use cabinet_cartridge::manifest::Capabilities;
use cabinet_helper::protocol::Change;
use std::net::{IpAddr, Ipv4Addr, ToSocketAddrs};
use std::sync::Arc;

pub trait Lease: Send {
    fn apply(&mut self, owner: &str, change: &Change) -> Result<(), String>;
    fn release(&mut self) -> Result<(), String>;
}

pub trait Helper: Send + Sync {
    fn lease(&self) -> Result<Box<dyn Lease>, String>;
    fn forget(&self, owner: &str) -> Result<usize, String>;
    fn sweep(&self) -> Result<usize, String>;
}

pub type SharedHelper = Arc<dyn Helper>;

pub fn owner(cartridge: &CartridgeRef) -> String {
    format!("{}/{}", cartridge.publisher, cartridge.id)
}

pub fn service_ip(address: &str) -> Result<Ipv4Addr, String> {
    if let Ok(ip) = address.parse::<Ipv4Addr>() {
        return Ok(ip);
    }
    (address, 0)
        .to_socket_addrs()
        .ok()
        .into_iter()
        .flatten()
        .find_map(|resolved| match resolved.ip() {
            IpAddr::V4(ip) => Some(ip),
            IpAddr::V6(_) => None,
        })
        .ok_or_else(|| format!("cannot redirect names to \"{address}\": it has no IPv4 address"))
}

pub fn session_changes(capabilities: &Capabilities, service: Ipv4Addr) -> Vec<Change> {
    capabilities
        .hosts
        .iter()
        .map(|hosts| Change::Hosts {
            names: hosts.names.clone(),
            target: service,
        })
        .collect()
}

fn unsupported(capabilities: &Capabilities) -> Vec<&'static str> {
    let mut items = Vec::new();
    if !capabilities.low_ports.is_empty() {
        items.push("low_ports");
    }
    if capabilities.trust_ca.is_some() {
        items.push("trust_ca");
    }
    if capabilities.firewall.is_some() {
        items.push("firewall");
    }
    items
}

pub struct Privileges {
    helper: Option<SharedHelper>,
    owner: String,
    title: String,
    capabilities: Capabilities,
    grant: Grant,
    lease: Option<Box<dyn Lease>>,
}

impl Privileges {
    pub fn new(
        helper: Option<SharedHelper>,
        cartridge: &CartridgeRef,
        title: &str,
        capabilities: &Capabilities,
        grant: Grant,
    ) -> Self {
        Self {
            helper,
            owner: owner(cartridge),
            title: title.to_string(),
            capabilities: capabilities.clone(),
            grant,
            lease: None,
        }
    }

    fn helper(&self) -> Result<Option<&SharedHelper>, String> {
        if self.capabilities.is_empty() {
            return Ok(None);
        }
        let Some(helper) = &self.helper else {
            return Ok(None);
        };
        if self.grant != Grant::Granted {
            return Err(format!(
                "{} needs changes to this machine that are not approved ({}). Trust its publisher and approve them first.",
                self.title,
                trust::describe(&self.capabilities).join("; ")
            ));
        }
        let unsupported = unsupported(&self.capabilities);
        if !unsupported.is_empty() {
            return Err(format!(
                "{} needs changes this version of The Cabinet cannot make yet: {}",
                self.title,
                unsupported.join(", ")
            ));
        }
        Ok(Some(helper))
    }

    pub fn check(&self) -> Result<(), String> {
        if let Some(helper) = self.helper()? {
            helper.lease()?;
        }
        Ok(())
    }

    pub fn apply(&mut self, service: &str) -> Result<(), String> {
        if self.lease.is_some() {
            return Ok(());
        }
        let Some(helper) = self.helper()? else {
            if !self.capabilities.is_empty() {
                log::warn!(
                    "{} declares changes to this machine, which The Cabinet cannot make on this system yet; continuing without them",
                    self.title
                );
            }
            return Ok(());
        };
        let service = service_ip(service)?;
        let mut lease = helper.lease()?;
        for change in session_changes(&self.capabilities, service) {
            lease.apply(&self.owner, &change)?;
            log::info!("{}: applied {change:?}", self.title);
        }
        self.lease = Some(lease);
        Ok(())
    }

    pub fn release(&mut self) {
        if let Some(mut lease) = self.lease.take() {
            match lease.release() {
                Ok(()) => log::info!("{}: undid its session changes", self.title),
                Err(err) => log::warn!("{}: cannot undo its session changes: {err}", self.title),
            }
        }
    }
}

#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub struct SocketHelper {
    pub socket: std::path::PathBuf,
    pub installed: Option<std::path::PathBuf>,
}

#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl SocketHelper {
    pub fn absent(&self) -> bool {
        self.installed.as_ref().is_some_and(|path| !path.exists())
    }

    pub fn connect(&self) -> Result<cabinet_helper::client::Client, String> {
        let mut client = cabinet_helper::client::Client::connect(&self.socket).map_err(|err| {
            if self.absent() {
                "The Cabinet's helper is not installed. Install it first; it asks for an administrator password once.".to_string()
            } else {
                format!("The Cabinet's helper is not answering: {err}")
            }
        })?;
        let version = client.hello()?;
        if version != cabinet_helper::protocol::VERSION {
            return Err(format!(
                "The Cabinet's helper is out of date (version {version}, this app needs {}). Install it again.",
                cabinet_helper::protocol::VERSION
            ));
        }
        Ok(client)
    }
}

#[cfg(unix)]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
struct SocketLease(cabinet_helper::client::Client);

#[cfg(unix)]
impl Lease for SocketLease {
    fn apply(&mut self, owner: &str, change: &Change) -> Result<(), String> {
        self.0
            .apply(owner, cabinet_helper::protocol::Scope::Session, change)
            .map(|_| ())
    }

    fn release(&mut self) -> Result<(), String> {
        self.0.release().map(|_| ())
    }
}

#[cfg(unix)]
impl Helper for SocketHelper {
    fn lease(&self) -> Result<Box<dyn Lease>, String> {
        Ok(Box::new(SocketLease(self.connect()?)))
    }

    fn forget(&self, owner: &str) -> Result<usize, String> {
        if self.absent() {
            return Ok(0);
        }
        self.connect()?.forget(owner)
    }

    fn sweep(&self) -> Result<usize, String> {
        if self.absent() {
            return Ok(0);
        }
        self.connect()?.sweep()
    }
}

#[cfg(target_os = "macos")]
pub fn system_socket() -> SocketHelper {
    SocketHelper {
        socket: cabinet_helper::system::SOCKET.into(),
        installed: Some(cabinet_helper::system::INSTALLED.into()),
    }
}

pub fn system() -> Option<SharedHelper> {
    #[cfg(target_os = "macos")]
    {
        Some(Arc::new(system_socket()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sync::MutexExt;
    use cabinet_cartridge::manifest::{Hosts, HostsTarget, Scope};
    use std::sync::Mutex;

    #[derive(Default)]
    pub(crate) struct FakeHelper {
        pub applied: Arc<Mutex<Vec<(String, Change)>>>,
        pub down: bool,
    }

    struct FakeLease(Arc<Mutex<Vec<(String, Change)>>>, Vec<Change>);

    impl Lease for FakeLease {
        fn apply(&mut self, owner: &str, change: &Change) -> Result<(), String> {
            self.0
                .lock_or_recover()
                .push((owner.to_string(), change.clone()));
            self.1.push(change.clone());
            Ok(())
        }

        fn release(&mut self) -> Result<(), String> {
            let mine = std::mem::take(&mut self.1);
            self.0
                .lock_or_recover()
                .retain(|(_, change)| !mine.contains(change));
            Ok(())
        }
    }

    impl Drop for FakeLease {
        fn drop(&mut self) {
            let _ = self.release();
        }
    }

    impl Helper for FakeHelper {
        fn lease(&self) -> Result<Box<dyn Lease>, String> {
            if self.down {
                return Err("the helper is not installed".to_string());
            }
            Ok(Box::new(FakeLease(Arc::clone(&self.applied), Vec::new())))
        }

        fn forget(&self, owner: &str) -> Result<usize, String> {
            let mut applied = self.applied.lock_or_recover();
            let before = applied.len();
            applied.retain(|(held, _)| held != owner);
            Ok(before - applied.len())
        }

        fn sweep(&self) -> Result<usize, String> {
            Ok(0)
        }
    }

    fn hosts() -> Capabilities {
        Capabilities {
            hosts: Some(Hosts {
                names: vec!["cabinet-test.invalid".to_string()],
                target: HostsTarget::Service,
                scope: Scope::Session,
            }),
            ..Capabilities::default()
        }
    }

    fn privileges(
        helper: Option<SharedHelper>,
        capabilities: &Capabilities,
        grant: Grant,
    ) -> Privileges {
        let cartridge = CartridgeRef {
            publisher: "0123456789abcdef".to_string(),
            id: "test".to_string(),
        };
        Privileges::new(helper, &cartridge, "Test cartridge", capabilities, grant)
    }

    #[test]
    fn granted_hosts_are_applied_for_the_session_and_undone_after() {
        let helper = Arc::new(FakeHelper::default());
        let applied = Arc::clone(&helper.applied);
        let mut privileges = privileges(Some(helper), &hosts(), Grant::Granted);
        privileges.check().unwrap();
        assert!(applied.lock_or_recover().is_empty());
        privileges.apply("100.64.0.2").unwrap();
        privileges.apply("100.64.0.2").unwrap();
        assert_eq!(
            *applied.lock_or_recover(),
            vec![(
                "0123456789abcdef/test".to_string(),
                Change::Hosts {
                    names: vec!["cabinet-test.invalid".to_string()],
                    target: Ipv4Addr::new(100, 64, 0, 2),
                }
            )]
        );
        privileges.release();
        assert!(applied.lock_or_recover().is_empty());
    }

    #[test]
    fn withheld_capabilities_block_the_session_and_reach_no_helper() {
        let helper = Arc::new(FakeHelper::default());
        let applied = Arc::clone(&helper.applied);
        let mut privileges = privileges(Some(helper), &hosts(), Grant::Withheld);
        let err = privileges.check().unwrap_err();
        assert!(err.contains("not approved"), "{err}");
        assert!(err.contains("cabinet-test.invalid"), "{err}");
        assert!(privileges.apply("127.0.0.1").is_err());
        assert!(applied.lock_or_recover().is_empty());
    }

    #[test]
    fn a_missing_helper_blocks_only_cartridges_that_need_it() {
        let down = || -> Option<SharedHelper> {
            Some(Arc::new(FakeHelper {
                down: true,
                ..FakeHelper::default()
            }))
        };
        let err = privileges(down(), &hosts(), Grant::Granted)
            .check()
            .unwrap_err();
        assert!(err.contains("not installed"), "{err}");
        let mut plain = privileges(down(), &Capabilities::default(), Grant::None);
        plain.check().unwrap();
        plain.apply("127.0.0.1").unwrap();
    }

    #[test]
    fn capabilities_the_helper_cannot_apply_yet_block_the_session() {
        let capabilities = Capabilities {
            low_ports: vec![443],
            ..hosts()
        };
        let helper: SharedHelper = Arc::new(FakeHelper::default());
        let err = privileges(Some(helper), &capabilities, Grant::Granted)
            .check()
            .unwrap_err();
        assert!(err.contains("low_ports"), "{err}");
    }

    #[test]
    fn a_system_without_a_helper_runs_without_the_changes() {
        let mut privileges = privileges(None, &hosts(), Grant::Withheld);
        privileges.check().unwrap();
        privileges.apply("127.0.0.1").unwrap();
    }

    #[test]
    fn the_service_address_must_be_ipv4() {
        assert_eq!(
            service_ip("100.64.0.1").unwrap(),
            Ipv4Addr::new(100, 64, 0, 1)
        );
        assert_eq!(service_ip("localhost").unwrap(), Ipv4Addr::LOCALHOST);
        assert!(service_ip("::1").is_err());
    }
}
