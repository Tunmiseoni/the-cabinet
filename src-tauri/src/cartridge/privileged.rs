use super::catrust::CaState;
use super::registry::CartridgeRef;
use super::trust::{self, Grant};
use cabinet_cartridge::manifest::{Capabilities, Scope as ManifestScope, Store as CaStore};
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

pub fn session_changes(
    capabilities: &Capabilities,
    service: &str,
    programs: &[String],
    service_program: Option<&str>,
) -> Result<Vec<Change>, String> {
    let mut changes = Vec::new();
    if let Some(program) = service_program.filter(|_| low_ports_through_helper(capabilities)) {
        changes.push(Change::LowPorts {
            program: program.to_string(),
            keep: false,
        });
    }
    if let Some(hosts) = &capabilities.hosts {
        changes.push(Change::Hosts {
            names: hosts.names.clone(),
            target: service_ip(service)?,
        });
    }
    if let Some(firewall) = &capabilities.firewall {
        changes.extend(programs.iter().map(|program| Change::Firewall {
            program: program.clone(),
            ports: firewall.inbound.clone(),
            keep: false,
            opened: Vec::new(),
        }));
    }
    Ok(changes)
}

fn low_ports_through_helper(capabilities: &Capabilities) -> bool {
    cfg!(target_os = "linux") && !capabilities.low_ports.is_empty()
}

fn settled(capabilities: &Capabilities) -> Vec<&'static str> {
    let mut items = Vec::new();
    if cfg!(target_os = "macos") {
        if !capabilities.low_ports.is_empty() {
            items.push("low_ports");
        }
        if capabilities
            .trust_ca
            .as_ref()
            .is_some_and(|ca| ca.stores == [CaStore::System])
        {
            items.push("trust_ca");
        }
        if capabilities
            .firewall
            .as_ref()
            .is_some_and(|firewall| firewall.scope == ManifestScope::Session)
        {
            items.push("firewall");
        }
    }
    items
}

fn unsupported(capabilities: &Capabilities) -> Vec<&'static str> {
    let mut items = Vec::new();
    let settled = settled(capabilities);
    if !capabilities.low_ports.is_empty()
        && !settled.contains(&"low_ports")
        && !low_ports_through_helper(capabilities)
    {
        items.push("low_ports");
    }
    if let Some(ca) = &capabilities.trust_ca {
        if !settled.contains(&"trust_ca") {
            items.push(if ca.stores.contains(&CaStore::Bottle) {
                "trust_ca (bottle store)"
            } else {
                "trust_ca"
            });
        }
    }
    let firewall_on_linux = cfg!(target_os = "linux")
        && capabilities
            .firewall
            .as_ref()
            .is_some_and(|firewall| firewall.scope == ManifestScope::Session);
    if capabilities.firewall.is_some() && !settled.contains(&"firewall") && !firewall_on_linux {
        items.push("firewall");
    }
    items
}

enum Route<'a> {
    Nothing,
    Skipped,
    Helper(&'a SharedHelper),
}

pub struct Privileges {
    helper: Option<SharedHelper>,
    owner: String,
    title: String,
    capabilities: Capabilities,
    grant: Grant,
    ca: CaState,
    programs: Vec<String>,
    service_program: Option<String>,
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
            ca: CaState::NotNeeded,
            programs: Vec::new(),
            service_program: None,
            lease: None,
        }
    }

    pub fn with_programs(mut self, programs: Vec<String>) -> Self {
        self.programs = programs;
        self
    }

    pub fn with_service_program(mut self, program: Option<String>) -> Self {
        self.service_program = program;
        self
    }

    fn through_helper(&self) -> bool {
        self.capabilities.hosts.is_some()
            || self.capabilities.firewall.is_some()
            || (self.service_program.is_some() && low_ports_through_helper(&self.capabilities))
    }

    pub fn with_ca(mut self, ca: CaState) -> Self {
        self.ca = ca;
        self
    }

    fn route(&self) -> Result<Route<'_>, String> {
        if self.capabilities.is_empty() {
            return Ok(Route::Nothing);
        }
        let Some(helper) = &self.helper else {
            return Ok(Route::Skipped);
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
        if let CaState::Missing(reason) = &self.ca {
            return Err(reason.clone());
        }
        if self.through_helper() {
            Ok(Route::Helper(helper))
        } else {
            Ok(Route::Nothing)
        }
    }

    pub fn check(&self) -> Result<(), String> {
        if let Route::Helper(helper) = self.route()? {
            helper.lease()?;
        }
        Ok(())
    }

    pub fn apply(&mut self, service: &str) -> Result<(), String> {
        if self.lease.is_some() {
            return Ok(());
        }
        let helper = match self.route()? {
            Route::Helper(helper) => helper,
            Route::Skipped => {
                log::warn!(
                    "{} declares changes to this machine, which The Cabinet cannot make on this system yet; continuing without them",
                    self.title
                );
                return Ok(());
            }
            Route::Nothing => {
                for item in settled(&self.capabilities) {
                    log::info!("{}: {item} needs no session change here", self.title);
                }
                return Ok(());
            }
        };
        let changes = session_changes(
            &self.capabilities,
            service,
            &self.programs,
            self.service_program.as_deref(),
        )?;
        let mut lease = helper.lease()?;
        for change in changes {
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
pub struct SocketHelper {
    pub socket: std::path::PathBuf,
    pub installed: Option<std::path::PathBuf>,
}

#[cfg(unix)]
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

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn system_socket() -> SocketHelper {
    SocketHelper {
        socket: cabinet_helper::system::SOCKET.into(),
        installed: Some(cabinet_helper::system::INSTALLED.into()),
    }
}

pub fn system() -> Option<SharedHelper> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        Some(Arc::new(system_socket()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
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
            firewall: Some(cabinet_cartridge::manifest::Firewall {
                scope: Scope::Install,
                inbound: vec![cabinet_cartridge::manifest::Port {
                    proto: cabinet_cartridge::manifest::Proto::Tcp,
                    port: 55440,
                }],
            }),
            ..hosts()
        };
        let helper: SharedHelper = Arc::new(FakeHelper::default());
        let err = privileges(Some(helper), &capabilities, Grant::Granted)
            .check()
            .unwrap_err();
        assert!(err.contains("firewall"), "{err}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn low_ports_need_no_helper_on_macos_but_still_need_approval() {
        let capabilities = Capabilities {
            low_ports: vec![443],
            ..Capabilities::default()
        };
        let down = || -> Option<SharedHelper> {
            Some(Arc::new(FakeHelper {
                down: true,
                ..FakeHelper::default()
            }))
        };
        let mut granted = privileges(down(), &capabilities, Grant::Granted);
        granted.check().unwrap();
        granted.apply("127.0.0.1").unwrap();
        let err = privileges(down(), &capabilities, Grant::Withheld)
            .check()
            .unwrap_err();
        assert!(err.contains("not approved"), "{err}");

        let helper = Arc::new(FakeHelper::default());
        let applied = Arc::clone(&helper.applied);
        let both = Capabilities {
            low_ports: vec![443],
            ..hosts()
        };
        let mut privileges = privileges(Some(helper), &both, Grant::Granted);
        privileges.apply("127.0.0.1").unwrap();
        assert_eq!(applied.lock_or_recover().len(), 1);
        assert!(matches!(
            applied.lock_or_recover()[0].1,
            Change::Hosts { .. }
        ));
    }

    #[cfg(target_os = "macos")]
    fn trusts_ca(stores: &[CaStore]) -> Capabilities {
        Capabilities {
            trust_ca: Some(cabinet_cartridge::manifest::TrustCa {
                asset: "ca".to_string(),
                permitted_names: vec!["cabinet-test.invalid".to_string()],
                scope: Scope::Install,
                stores: stores.to_vec(),
                bottle_setting: None,
            }),
            ..Capabilities::default()
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_session_waits_for_its_certificate_authority_and_the_bottle_store_is_not_built() {
        let helper = || -> Option<SharedHelper> { Some(Arc::new(FakeHelper::default())) };
        let system = trusts_ca(&[CaStore::System]);
        privileges(helper(), &system, Grant::Granted)
            .with_ca(CaState::Ready)
            .check()
            .unwrap();
        let err = privileges(helper(), &system, Grant::Granted)
            .with_ca(CaState::Missing("not trusted yet".to_string()))
            .check()
            .unwrap_err();
        assert_eq!(err, "not trusted yet");
        let err = privileges(
            helper(),
            &trusts_ca(&[CaStore::System, CaStore::Bottle]),
            Grant::Granted,
        )
        .check()
        .unwrap_err();
        assert!(err.contains("bottle store"), "{err}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_granted_firewall_allows_the_cartridge_programs_for_the_session() {
        let capabilities = Capabilities {
            firewall: Some(cabinet_cartridge::manifest::Firewall {
                scope: Scope::Session,
                inbound: vec![cabinet_cartridge::manifest::Port {
                    proto: cabinet_cartridge::manifest::Proto::Tcp,
                    port: 55440,
                }],
            }),
            ..Capabilities::default()
        };
        let helper = Arc::new(FakeHelper::default());
        let applied = Arc::clone(&helper.applied);
        let mut privileges =
            privileges(Some(helper), &capabilities, Grant::Granted).with_programs(vec![
                "/store/service".to_string(),
                "/store/game".to_string(),
            ]);
        privileges.apply("").unwrap();
        let inbound = capabilities.firewall.unwrap().inbound;
        let programs: Vec<Change> = applied
            .lock_or_recover()
            .iter()
            .map(|(_, change)| change.clone())
            .collect();
        assert_eq!(
            programs,
            vec![
                Change::Firewall {
                    program: "/store/service".to_string(),
                    ports: inbound.clone(),
                    keep: false,
                    opened: Vec::new(),
                },
                Change::Firewall {
                    program: "/store/game".to_string(),
                    ports: inbound,
                    keep: false,
                    opened: Vec::new(),
                },
            ]
        );
        privileges.release();
        assert!(applied.lock_or_recover().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn low_ports_go_to_the_helper_for_the_hosts_service_program_on_linux() {
        let capabilities = Capabilities {
            low_ports: vec![443],
            ..Capabilities::default()
        };
        let helper = Arc::new(FakeHelper::default());
        let applied = Arc::clone(&helper.applied);
        let mut member = privileges(Some(helper.clone()), &capabilities, Grant::Granted);
        member.apply("127.0.0.1").unwrap();
        assert!(applied.lock_or_recover().is_empty());
        let mut host = privileges(Some(helper), &capabilities, Grant::Granted)
            .with_programs(vec![
                "/store/service".to_string(),
                "/store/game".to_string(),
            ])
            .with_service_program(Some("/store/service".to_string()));
        host.apply("127.0.0.1").unwrap();
        let changes: Vec<Change> = applied
            .lock_or_recover()
            .iter()
            .map(|(_, change)| change.clone())
            .collect();
        assert_eq!(
            changes,
            vec![Change::LowPorts {
                program: "/store/service".to_string(),
                keep: false,
            }]
        );
        host.release();
        assert!(applied.lock_or_recover().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn low_ports_are_not_built_off_macos() {
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
