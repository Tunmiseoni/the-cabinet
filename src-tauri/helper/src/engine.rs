use crate::anchors::Anchors;
use crate::firewall::{AppFirewall, PortFirewall, Programs};
use crate::hosts;
use crate::journal::Journal;
use crate::lowports::PortGrant;
use crate::menu;
use crate::protocol::{Change, Entry, Scope};
use std::path::{Path, PathBuf};

pub struct Engine {
    journal: Journal,
    hosts: PathBuf,
    flush: bool,
    programs: Option<Programs>,
    anchors: Option<Box<dyn Anchors>>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

impl Engine {
    pub fn open(state_dir: &Path, hosts: &Path, flush: bool) -> Result<Self, String> {
        Ok(Self {
            journal: Journal::open(state_dir)?,
            hosts: hosts.to_path_buf(),
            flush,
            programs: None,
            anchors: None,
        })
    }

    pub fn with_programs(mut self, programs: Option<Programs>) -> Self {
        self.programs = programs;
        self
    }

    pub fn with_anchors(mut self, anchors: Option<Box<dyn Anchors>>) -> Self {
        self.anchors = anchors;
        self
    }

    fn anchors(&self) -> Result<&dyn Anchors, String> {
        self.anchors.as_deref().ok_or_else(|| {
            "this helper cannot trust certificate authorities on this system".to_string()
        })
    }

    fn firewall(&self) -> Result<(&Programs, &dyn AppFirewall), String> {
        let refused = || "this helper cannot change the firewall".to_string();
        let programs = self.programs.as_ref().ok_or_else(refused)?;
        let firewall = programs.firewall.as_deref().ok_or_else(refused)?;
        Ok((programs, firewall))
    }

    fn port_firewall(&self) -> Option<(&Programs, &dyn PortFirewall)> {
        let programs = self.programs.as_ref()?;
        Some((programs, programs.port_firewall.as_deref()?))
    }

    fn low_ports(&self) -> Result<(&Programs, &dyn PortGrant), String> {
        let refused = || "this helper cannot grant low ports on this system".to_string();
        let programs = self.programs.as_ref().ok_or_else(refused)?;
        let grant = programs.low_ports.as_deref().ok_or_else(refused)?;
        Ok((programs, grant))
    }

    fn held(&self, kept: impl Fn(&Change) -> Option<bool>) -> Option<bool> {
        self.journal
            .entries()
            .iter()
            .find_map(|entry| kept(&entry.change))
    }

    fn shared(&self, id: &str, same: impl Fn(&Change) -> bool) -> bool {
        self.journal
            .entries()
            .iter()
            .any(|other| other.id != id && same(&other.change))
    }

    pub fn entries(&self) -> &[Entry] {
        self.journal.entries()
    }

    fn rewrite_hosts(&self, edit: impl FnOnce(&str) -> String) -> Result<(), String> {
        let text = hosts::read(&self.hosts)?;
        let edited = edit(&text);
        if edited != text {
            hosts::write(&self.hosts, &edited)?;
            if self.flush {
                hosts::flush_cache();
            }
        }
        Ok(())
    }

    pub fn apply(&mut self, owner: &str, scope: Scope, change: Change) -> Result<Entry, String> {
        menu::check(owner, scope, &change)?;
        let mut act = false;
        let change = match change {
            Change::Firewall { program, ports, .. } if self.port_firewall().is_some() => {
                let (programs, firewall) = self.port_firewall().expect("checked above");
                let program = programs.resolve(&program)?;
                let mut opened = Vec::new();
                if firewall.enabled()? {
                    for port in &ports {
                        let held = self.journal.entries().iter().any(|entry| {
                            matches!(&entry.change, Change::Firewall { ports: held, .. } if held.contains(port))
                        });
                        if !held && !firewall.is_open(port)? {
                            opened.push(port.clone());
                        }
                    }
                }
                Change::Firewall {
                    program,
                    ports,
                    keep: opened.is_empty(),
                    opened,
                }
            }
            Change::Firewall { program, ports, .. } => {
                let (programs, firewall) = self.firewall()?;
                let program = programs.resolve(&program)?;
                let held = self.held(|change| match change {
                    Change::Firewall {
                        program: held,
                        keep,
                        ..
                    } if *held == program => Some(*keep),
                    _ => None,
                });
                let keep = match held {
                    Some(keep) => keep,
                    None => {
                        let keep = !firewall.enabled()? || firewall.listed(&program)?;
                        act = !keep;
                        keep
                    }
                };
                Change::Firewall {
                    program,
                    ports,
                    keep,
                    opened: Vec::new(),
                }
            }
            Change::LowPorts { program, .. } => {
                let (programs, grant) = self.low_ports()?;
                let program = programs.resolve(&program)?;
                let held = self.held(|change| match change {
                    Change::LowPorts {
                        program: held,
                        keep,
                    } if *held == program => Some(*keep),
                    _ => None,
                });
                let keep = match held {
                    Some(keep) => keep,
                    None => {
                        let keep = grant.granted(&program)?;
                        act = !keep;
                        keep
                    }
                };
                Change::LowPorts { program, keep }
            }
            Change::TrustCa {
                certificate, names, ..
            } => {
                let ca = menu::check_certificate(&certificate, &names, (now_ms() / 1000) as i64)?;
                self.anchors()?;
                Change::TrustCa {
                    certificate: cabinet_cartridge::ca::pem(&ca.der),
                    names,
                    anchor: String::new(),
                }
            }
            other => other,
        };
        match &change {
            Change::Hosts { names, target } => {
                for entry in self.journal.entries() {
                    let Change::Hosts {
                        names: held,
                        target: held_target,
                    } = &entry.change
                    else {
                        continue;
                    };
                    if held_target != target {
                        if let Some(name) = names.iter().find(|name| held.contains(name)) {
                            return Err(format!(
                                "\"{name}\" is already redirected to {held_target} by another session"
                            ));
                        }
                    }
                }
                let text = hosts::read(&self.hosts)?;
                let wanted = target.to_string();
                for (name, address) in hosts::outside(&text) {
                    if address != wanted && names.contains(&name) {
                        return Err(format!(
                            "\"{name}\" already has an entry in {} that The Cabinet did not write ({address}); remove it first",
                            self.hosts.display()
                        ));
                    }
                }
            }
            Change::Firewall { .. } | Change::LowPorts { .. } | Change::TrustCa { .. } => {}
        }
        let mut entry = self.journal.add(owner, scope, change, now_ms())?;
        if matches!(entry.change, Change::TrustCa { .. }) {
            let path = self.anchors().and_then(|anchors| anchors.path(&entry.id));
            let recorded = path.and_then(|path| {
                if let Change::TrustCa { anchor, .. } = &mut entry.change {
                    *anchor = path;
                }
                self.journal.replace(entry.clone())
            });
            if let Err(err) = recorded {
                if let Err(journal) = self.journal.remove(&entry.id) {
                    log::error!("cannot drop the journal entry for a failed change: {journal}");
                }
                return Err(err);
            }
        }
        let applied = match &entry.change {
            Change::Hosts { names, target } => {
                self.rewrite_hosts(|text| hosts::add(text, &entry.id, names, *target))
            }
            Change::Firewall { opened, .. } if !opened.is_empty() => self.open_ports(opened),
            Change::Firewall { program, .. } if act => self
                .firewall()
                .and_then(|(_, firewall)| firewall.allow(program)),
            Change::LowPorts { program, .. } if act => {
                self.low_ports().and_then(|(_, grant)| grant.grant(program))
            }
            Change::TrustCa {
                certificate,
                anchor,
                ..
            } => self
                .anchors()
                .and_then(|anchors| anchors.place(anchor, certificate)),
            Change::Firewall { .. } | Change::LowPorts { .. } => Ok(()),
        };
        if let Err(err) = applied {
            if let Err(journal) = self.journal.remove(&entry.id) {
                log::error!("cannot drop the journal entry for a failed change: {journal}");
            }
            return Err(err);
        }
        log::info!(
            "applied {} for {}: {:?}",
            entry.id,
            entry.owner,
            entry.change
        );
        Ok(entry)
    }

    fn open_ports(&self, ports: &[crate::protocol::Port]) -> Result<(), String> {
        let (_, firewall) = self
            .port_firewall()
            .ok_or_else(|| "this helper cannot change the firewall".to_string())?;
        for (index, port) in ports.iter().enumerate() {
            if let Err(err) = firewall.open(port) {
                for opened in &ports[..index] {
                    if let Err(close) = firewall.close(opened) {
                        log::error!("cannot close port {} again: {close}", opened.port);
                    }
                }
                return Err(err);
            }
        }
        Ok(())
    }

    fn close_ports(&mut self, id: &str, opened: &[crate::protocol::Port]) -> Result<(), String> {
        let mut closing = Vec::new();
        for port in opened {
            let heir = self
                .journal
                .entries()
                .iter()
                .find(|other| {
                    other.id != id
                        && matches!(&other.change, Change::Firewall { ports, .. } if ports.contains(port))
                })
                .cloned();
            match heir {
                Some(mut heir) => {
                    if let Change::Firewall { opened, keep, .. } = &mut heir.change {
                        if !opened.contains(port) {
                            opened.push(port.clone());
                        }
                        *keep = false;
                    }
                    self.journal.replace(heir)?;
                }
                None => closing.push(port.clone()),
            }
        }
        let (_, firewall) = self
            .port_firewall()
            .ok_or_else(|| "this helper cannot change the firewall".to_string())?;
        for port in &closing {
            firewall.close(port)?;
        }
        Ok(())
    }

    pub fn undo(&mut self, id: &str) -> Result<(), String> {
        let Some(entry) = self.journal.entry(id).cloned() else {
            return Ok(());
        };
        match &entry.change {
            Change::Hosts { .. } => self.rewrite_hosts(|text| hosts::remove(text, &entry.id))?,
            Change::Firewall { opened, .. } if !opened.is_empty() => {
                self.close_ports(&entry.id, opened)?
            }
            Change::Firewall { program, keep, .. } => {
                let shared = self.shared(&entry.id, |other| {
                    matches!(other, Change::Firewall { program: held, .. } if held == program)
                });
                if !keep && !shared {
                    self.firewall()?.1.remove(program)?;
                }
            }
            Change::LowPorts { program, keep } => {
                let shared = self.shared(&entry.id, |other| {
                    matches!(other, Change::LowPorts { program: held, .. } if held == program)
                });
                if !keep && !shared {
                    self.low_ports()?.1.revoke(program)?;
                }
            }
            Change::TrustCa {
                certificate,
                anchor,
                ..
            } => {
                if !anchor.is_empty() {
                    self.anchors()?.remove(anchor, certificate)?;
                }
            }
        }
        self.journal.remove(id)?;
        log::info!("undid {} for {}: {:?}", entry.id, entry.owner, entry.change);
        Ok(())
    }

    pub fn undo_where(&mut self, matches: impl Fn(&Entry) -> bool) -> (usize, Vec<String>) {
        let ids: Vec<String> = self
            .journal
            .entries()
            .iter()
            .filter(|entry| matches(entry))
            .map(|entry| entry.id.clone())
            .collect();
        let mut undone = 0;
        let mut failures = Vec::new();
        for id in ids {
            match self.undo(&id) {
                Ok(()) => undone += 1,
                Err(err) => {
                    log::error!("cannot undo {id}: {err}");
                    failures.push(err);
                }
            }
        }
        (undone, failures)
    }

    pub fn drop_orphans(&mut self) -> Result<usize, String> {
        let text = hosts::read(&self.hosts)?;
        let orphans: Vec<String> = hosts::ids(&text)
            .into_iter()
            .filter(|id| self.journal.entry(id).is_none())
            .collect();
        if !orphans.is_empty() {
            log::warn!("removing hosts blocks the journal does not know: {orphans:?}");
            self.rewrite_hosts(|text| {
                orphans
                    .iter()
                    .fold(text.to_string(), |text, id| hosts::remove(&text, id))
            })?;
        }
        Ok(orphans.len())
    }

    pub fn sweep(&mut self, live: impl Fn(&Entry) -> bool) -> (usize, Vec<String>) {
        let (mut undone, mut failures) =
            self.undo_where(|entry| entry.scope == Scope::Session && !live(entry));
        match self.drop_orphans() {
            Ok(orphans) => undone += orphans,
            Err(err) => failures.push(err),
        }
        (undone, failures)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicU32, Ordering};

    pub(crate) const OWNER: &str = "0123456789abcdef/test";
    pub(crate) const NAME: &str = "cabinet-test.invalid";
    pub(crate) const BASE: &str = "127.0.0.1\tlocalhost\n# >>> other-tool\n127.0.0.1 api.example-game.net\n# <<< other-tool\n";

    pub(crate) struct Scratch(pub PathBuf);

    #[cfg(unix)]
    pub(crate) fn program(scratch: &Scratch) -> String {
        let files = scratch.0.join("store").join("cd".repeat(32)).join("files");
        std::fs::create_dir_all(&files).unwrap();
        let program = files.join("echo-cart");
        std::fs::write(&program, "x").unwrap();
        program
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string()
    }

    #[cfg(unix)]
    fn firewall(program: &str) -> Change {
        Change::Firewall {
            program: program.to_string(),
            ports: Vec::new(),
            keep: false,
            opened: Vec::new(),
        }
    }

    #[cfg(unix)]
    fn with_firewall(scratch: &Scratch, fake: &crate::firewall::tests::FakeFirewall) -> Engine {
        scratch.engine().with_programs(Some(Programs {
            firewall: Some(Box::new(fake.clone())),
            port_firewall: None,
            low_ports: None,
            root: scratch.0.clone(),
            uid: None,
        }))
    }

    #[cfg(unix)]
    #[test]
    fn a_program_is_allowed_for_its_sessions_and_removed_with_the_last_one() {
        let scratch = Scratch::new("fw");
        let fake = crate::firewall::tests::FakeFirewall::default();
        let program = program(&scratch);
        let mut engine = with_firewall(&scratch, &fake);
        let first = engine
            .apply(OWNER, Scope::Session, firewall(&program))
            .unwrap();
        let second = engine
            .apply(OWNER, Scope::Session, firewall(&program))
            .unwrap();
        assert_eq!(*fake.apps.lock().unwrap(), vec![program.clone()]);
        engine.undo(&first.id).unwrap();
        assert_eq!(fake.apps.lock().unwrap().len(), 1);
        engine.undo(&second.id).unwrap();
        assert!(fake.apps.lock().unwrap().is_empty());
        assert!(engine.entries().is_empty());

        engine
            .apply(OWNER, Scope::Session, firewall(&program))
            .unwrap();
        let mut restarted = with_firewall(&scratch, &fake);
        let (undone, failures) = restarted.sweep(|_| false);
        assert_eq!((undone, failures.len()), (1, 0));
        assert!(fake.apps.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn an_allowance_the_cabinet_did_not_add_is_never_removed() {
        let scratch = Scratch::new("fw-keep");
        let fake = crate::firewall::tests::FakeFirewall::default();
        let program = program(&scratch);
        fake.apps.lock().unwrap().push(program.clone());
        let mut engine = with_firewall(&scratch, &fake);
        let entry = engine
            .apply(OWNER, Scope::Session, firewall(&program))
            .unwrap();
        assert_eq!(
            entry.change,
            Change::Firewall {
                program: program.clone(),
                ports: Vec::new(),
                keep: true,
                opened: Vec::new(),
            }
        );
        engine.undo(&entry.id).unwrap();
        assert_eq!(*fake.apps.lock().unwrap(), vec![program.clone()]);

        fake.apps.lock().unwrap().clear();
        *fake.off.lock().unwrap() = true;
        let entry = engine
            .apply(OWNER, Scope::Session, firewall(&program))
            .unwrap();
        assert!(fake.apps.lock().unwrap().is_empty());
        engine.undo(&entry.id).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn ports_are_opened_for_their_sessions_and_closed_with_the_last_one() {
        use crate::protocol::{Port, Proto};
        let scratch = Scratch::new("ports");
        let fake = crate::firewall::tests::FakePorts::default();
        let program = program(&scratch);
        let engine = |scratch: &Scratch| {
            scratch.engine().with_programs(Some(Programs {
                firewall: None,
                port_firewall: Some(Box::new(fake.clone())),
                low_ports: None,
                root: scratch.0.clone(),
                uid: None,
            }))
        };
        let tcp = |port| Port {
            proto: Proto::Tcp,
            port,
        };
        let allow = |ports: Vec<Port>| Change::Firewall {
            program: program.clone(),
            ports,
            keep: false,
            opened: Vec::new(),
        };
        let mut helper = engine(&scratch);
        let first = helper
            .apply(OWNER, Scope::Session, allow(vec![tcp(987), tcp(55435)]))
            .unwrap();
        let second = helper
            .apply(OWNER, Scope::Session, allow(vec![tcp(987)]))
            .unwrap();
        assert_eq!(*fake.open.lock().unwrap(), vec![tcp(987), tcp(55435)]);
        helper.undo(&first.id).unwrap();
        assert_eq!(*fake.open.lock().unwrap(), vec![tcp(987)]);
        helper.undo(&second.id).unwrap();
        assert!(fake.open.lock().unwrap().is_empty());
        assert!(helper.entries().is_empty());

        fake.open.lock().unwrap().push(tcp(22));
        let kept = helper
            .apply(OWNER, Scope::Session, allow(vec![tcp(22), tcp(987)]))
            .unwrap();
        assert_eq!(*fake.open.lock().unwrap(), vec![tcp(22), tcp(987)]);
        let mut restarted = engine(&scratch);
        let (undone, failures) = restarted.sweep(|_| false);
        assert_eq!((undone, failures.len()), (1, 0));
        assert_eq!(*fake.open.lock().unwrap(), vec![tcp(22)]);
        let _ = kept;

        *fake.off.lock().unwrap() = true;
        let off = restarted
            .apply(OWNER, Scope::Session, allow(vec![tcp(987)]))
            .unwrap();
        assert!(
            matches!(&off.change, Change::Firewall { opened, keep: true, .. } if opened.is_empty())
        );
        restarted.undo(&off.id).unwrap();
        assert_eq!(*fake.open.lock().unwrap(), vec![tcp(22)]);
        assert!(restarted
            .apply(OWNER, Scope::Session, allow(vec![tcp(987)]))
            .map(|entry| entry.change)
            .is_ok());
        assert!(engine(&scratch)
            .apply(
                OWNER,
                Scope::Session,
                Change::Firewall {
                    program: "/bin/sh".to_string(),
                    ports: vec![tcp(987)],
                    keep: false,
                    opened: Vec::new(),
                }
            )
            .is_err());
    }

    #[test]
    fn a_certificate_authority_is_placed_for_its_install_and_removed_with_it() {
        let scratch = Scratch::new("trust");
        let anchors = crate::anchors::tests::anchors(&scratch);
        let dir = anchors.dir.clone();
        let mut engine = scratch.engine().with_anchors(Some(Box::new(anchors)));
        let trust = |names: &[&str]| Change::TrustCa {
            certificate: include_str!("../../cartridge/src/fixtures/ca-good.pem").to_string(),
            names: names.iter().map(|name| name.to_string()).collect(),
            anchor: String::new(),
        };
        let entry = engine.apply(OWNER, Scope::Install, trust(&[NAME])).unwrap();
        let Change::TrustCa {
            certificate,
            anchor,
            ..
        } = &entry.change
        else {
            panic!("{entry:?}");
        };
        assert_eq!(engine.entries()[0], entry);
        assert_eq!(std::fs::read_to_string(anchor).unwrap(), *certificate);
        assert!(anchor.starts_with(dir.to_str().unwrap()));
        let (undone, failures) = engine.sweep(|_| false);
        assert_eq!((undone, failures.len()), (0, 0));
        assert!(std::path::Path::new(anchor).exists());
        let (undone, failures) = engine.undo_where(|held| held.owner == OWNER);
        assert_eq!((undone, failures.len()), (1, 0));
        assert!(!std::path::Path::new(anchor).exists());
        assert!(engine.entries().is_empty());

        assert!(engine
            .apply(OWNER, Scope::Install, trust(&["other.invalid"]))
            .is_err());
        let mut plain = scratch.engine();
        assert!(plain.apply(OWNER, Scope::Install, trust(&[NAME])).is_err());
        assert!(plain.entries().is_empty());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn low_ports_are_granted_for_their_sessions_and_kept_when_already_there() {
        let scratch = Scratch::new("low");
        let grant = crate::lowports::tests::FakeGrant::default();
        let program = program(&scratch);
        let mut engine = scratch.engine().with_programs(Some(Programs {
            firewall: None,
            port_firewall: None,
            low_ports: Some(Box::new(grant.clone())),
            root: scratch.0.clone(),
            uid: None,
        }));
        let low = |program: &str| Change::LowPorts {
            program: program.to_string(),
            keep: false,
        };
        let first = engine.apply(OWNER, Scope::Session, low(&program)).unwrap();
        let second = engine.apply(OWNER, Scope::Session, low(&program)).unwrap();
        assert_eq!(*grant.programs.lock().unwrap(), vec![program.clone()]);
        engine.undo(&first.id).unwrap();
        assert_eq!(grant.programs.lock().unwrap().len(), 1);
        engine.undo(&second.id).unwrap();
        assert!(grant.programs.lock().unwrap().is_empty());

        engine.apply(OWNER, Scope::Session, low(&program)).unwrap();
        let mut engine = scratch.engine().with_programs(Some(Programs {
            firewall: None,
            port_firewall: None,
            low_ports: Some(Box::new(grant.clone())),
            root: scratch.0.clone(),
            uid: None,
        }));
        let (undone, failures) = engine.sweep(|_| false);
        assert_eq!((undone, failures.len()), (1, 0));
        assert!(grant.programs.lock().unwrap().is_empty());

        grant.programs.lock().unwrap().push(program.clone());
        let kept = engine.apply(OWNER, Scope::Session, low(&program)).unwrap();
        assert_eq!(
            kept.change,
            Change::LowPorts {
                program: program.clone(),
                keep: true,
            }
        );
        engine.undo(&kept.id).unwrap();
        assert_eq!(*grant.programs.lock().unwrap(), vec![program.clone()]);

        assert!(engine.apply(OWNER, Scope::Session, low("/bin/sh")).is_err());
        let mut plain = scratch.engine();
        assert!(plain.apply(OWNER, Scope::Session, low(&program)).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn firewall_requests_are_refused_outside_the_store_or_without_a_firewall() {
        let scratch = Scratch::new("fw-refuse");
        let fake = crate::firewall::tests::FakeFirewall::default();
        let program = program(&scratch);
        let mut plain = scratch.engine();
        assert!(plain
            .apply(OWNER, Scope::Session, firewall(&program))
            .is_err());
        let mut engine = with_firewall(&scratch, &fake);
        assert!(engine
            .apply(OWNER, Scope::Session, firewall("/bin/sh"))
            .is_err());
        assert!(engine.entries().is_empty());
        assert!(fake.apps.lock().unwrap().is_empty());
    }

    impl Scratch {
        pub(crate) fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "cabinet-helper-{tag}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("hosts"), BASE).unwrap();
            Self(dir)
        }

        pub(crate) fn hosts(&self) -> PathBuf {
            self.0.join("hosts")
        }

        pub(crate) fn hosts_text(&self) -> String {
            std::fs::read_to_string(self.hosts()).unwrap()
        }

        pub(crate) fn engine(&self) -> Engine {
            Engine::open(&self.0, &self.hosts(), false).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn hosts(target: Ipv4Addr) -> Change {
        Change::Hosts {
            names: vec![NAME.to_string()],
            target,
        }
    }

    #[test]
    fn a_hosts_entry_appears_and_is_undone() {
        let scratch = Scratch::new("apply");
        let mut engine = scratch.engine();
        let entry = engine
            .apply(OWNER, Scope::Session, hosts(Ipv4Addr::new(100, 64, 0, 2)))
            .unwrap();
        assert!(scratch
            .hosts_text()
            .contains("100.64.0.2 cabinet-test.invalid\n"));
        assert_eq!(engine.entries().len(), 1);
        engine.undo(&entry.id).unwrap();
        assert_eq!(scratch.hosts_text(), BASE);
        assert!(engine.entries().is_empty());
        engine.undo(&entry.id).unwrap();
    }

    #[test]
    fn the_journal_survives_a_crash_and_the_sweep_undoes_it() {
        let scratch = Scratch::new("crash");
        scratch
            .engine()
            .apply(OWNER, Scope::Session, hosts(Ipv4Addr::LOCALHOST))
            .unwrap();
        assert!(scratch.hosts_text().contains(NAME));
        let mut restarted = scratch.engine();
        assert_eq!(restarted.entries().len(), 1);
        let (undone, failures) = restarted.sweep(|_| false);
        assert_eq!((undone, failures.len()), (1, 0));
        assert_eq!(scratch.hosts_text(), BASE);
        assert!(scratch.engine().entries().is_empty());
    }

    #[test]
    fn the_sweep_keeps_live_entries_and_drops_orphan_blocks() {
        let scratch = Scratch::new("sweep");
        let mut engine = scratch.engine();
        let live = engine
            .apply(OWNER, Scope::Session, hosts(Ipv4Addr::LOCALHOST))
            .unwrap();
        let orphaned = format!(
            "{}# >>> the-cabinet 99\n127.0.0.1 lost.invalid\n# <<< the-cabinet 99\n",
            scratch.hosts_text()
        );
        std::fs::write(scratch.hosts(), orphaned).unwrap();
        let (undone, failures) = engine.sweep(|entry| entry.id == live.id);
        assert_eq!((undone, failures.len()), (1, 0));
        let text = scratch.hosts_text();
        assert!(text.contains(NAME) && !text.contains("lost.invalid"));
        assert!(text.contains("api.example-game.net"));
    }

    #[test]
    fn refuses_a_name_someone_else_already_maps() {
        let scratch = Scratch::new("conflict");
        let mut engine = scratch.engine();
        engine
            .apply(OWNER, Scope::Session, hosts(Ipv4Addr::LOCALHOST))
            .unwrap();
        engine
            .apply(OWNER, Scope::Session, hosts(Ipv4Addr::LOCALHOST))
            .unwrap();
        let err = engine
            .apply(OWNER, Scope::Session, hosts(Ipv4Addr::new(100, 64, 0, 2)))
            .unwrap_err();
        assert!(err.contains("another session"), "{err}");

        let scratch = Scratch::new("foreign");
        std::fs::write(scratch.hosts(), format!("{BASE}203.0.113.7 {NAME}\n")).unwrap();
        let before = scratch.hosts_text();
        let err = scratch
            .engine()
            .apply(OWNER, Scope::Session, hosts(Ipv4Addr::LOCALHOST))
            .unwrap_err();
        assert!(err.contains("did not write"), "{err}");
        assert_eq!(scratch.hosts_text(), before);
        assert!(scratch.engine().entries().is_empty());
    }

    #[test]
    fn a_failed_write_leaves_no_journal_entry() {
        let scratch = Scratch::new("failed");
        let mut engine = Engine::open(&scratch.0, &scratch.0.join("missing"), false).unwrap();
        assert!(engine
            .apply(OWNER, Scope::Session, hosts(Ipv4Addr::LOCALHOST))
            .is_err());
        assert!(engine.entries().is_empty());
    }

    #[test]
    fn an_unreadable_journal_is_set_aside() {
        let scratch = Scratch::new("corrupt");
        std::fs::write(scratch.0.join(crate::journal::FILE), "not json").unwrap();
        assert!(scratch.engine().entries().is_empty());
        assert!(scratch.0.join("journal.corrupt").exists());
    }
}
