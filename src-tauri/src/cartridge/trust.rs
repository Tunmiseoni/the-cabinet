use super::registry::{CartridgeRef, Entry, Registry};
use cabinet_cartridge::manifest::{Capabilities, Proto, Scope, Store as CaStore};
use cabinet_cartridge::Publisher;
use serde::{Deserialize, Serialize};

pub const MAX_NAME_LEN: usize = 40;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedPublisher {
    pub key: Publisher,
    pub name: String,
    pub trusted_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Standing {
    Unsigned,
    Unknown,
    Trusted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublisherView {
    pub standing: Standing,
    pub fingerprint: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    #[serde(default)]
    pub trust_publisher: Option<String>,
    #[serde(default)]
    pub capabilities: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Grant {
    None,
    Granted,
    Withheld,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outlook {
    None,
    Approved,
    NeedsApproval,
    NeedsTrust,
    Unsigned,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub trust: Option<(Publisher, String)>,
    pub approved: Option<Capabilities>,
    pub grant: Grant,
}

pub fn clean_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("the publisher needs a name".to_string());
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(format!(
            "the publisher name is longer than {MAX_NAME_LEN} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("the publisher name has control characters".to_string());
    }
    Ok(name.to_string())
}

fn scope(scope: Scope) -> &'static str {
    match scope {
        Scope::Session => "while a session runs",
        Scope::Install => "while it is installed",
    }
}

pub fn describe(capabilities: &Capabilities) -> Vec<String> {
    let mut lines = Vec::new();
    if !capabilities.low_ports.is_empty() {
        let ports: Vec<String> = capabilities.low_ports.iter().map(u16::to_string).collect();
        lines.push(format!(
            "let its service listen on port {}",
            ports.join(", ")
        ));
    }
    if let Some(hosts) = &capabilities.hosts {
        lines.push(format!(
            "point {} at the room's service {}",
            hosts.names.join(", "),
            scope(hosts.scope)
        ));
    }
    if let Some(ca) = &capabilities.trust_ca {
        let stores: Vec<&str> = ca
            .stores
            .iter()
            .map(|store| match store {
                CaStore::System => "system store",
                CaStore::Bottle => "bottle",
            })
            .collect();
        lines.push(format!(
            "trust its certificate authority for {} ({}) {}",
            ca.permitted_names.join(", "),
            stores.join(", "),
            scope(ca.scope)
        ));
    }
    if let Some(firewall) = &capabilities.firewall {
        let ports: Vec<String> = firewall
            .inbound
            .iter()
            .map(|port| {
                let proto = match port.proto {
                    Proto::Tcp => "tcp",
                    Proto::Udp => "udp",
                };
                format!("{proto} {}", port.port)
            })
            .collect();
        lines.push(format!(
            "open the firewall for inbound {} {}",
            ports.join(", "),
            scope(firewall.scope)
        ));
    }
    lines
}

impl Registry {
    pub fn trusted(&self, fingerprint: &str) -> Option<&TrustedPublisher> {
        self.publishers
            .iter()
            .find(|publisher| publisher.key.fingerprint() == fingerprint)
    }

    pub fn is_trusted(&self, publisher: Option<&Publisher>) -> bool {
        publisher.is_some_and(|key| self.publishers.iter().any(|trusted| &trusted.key == key))
    }

    pub fn publisher_view(&self, publisher: Option<&Publisher>) -> PublisherView {
        let Some(key) = publisher else {
            return PublisherView {
                standing: Standing::Unsigned,
                fingerprint: None,
                name: None,
            };
        };
        let trusted = self.publishers.iter().find(|trusted| &trusted.key == key);
        PublisherView {
            standing: if trusted.is_some() {
                Standing::Trusted
            } else {
                Standing::Unknown
            },
            fingerprint: Some(key.fingerprint()),
            name: trusted.map(|trusted| trusted.name.clone()),
        }
    }

    pub fn known_key(&self, fingerprint: &str) -> Option<Publisher> {
        self.trusted(fingerprint)
            .map(|trusted| trusted.key.clone())
            .or_else(|| {
                self.entries
                    .iter()
                    .filter_map(|entry| entry.publisher_key.as_ref())
                    .find(|key| key.fingerprint() == fingerprint)
                    .cloned()
            })
    }

    pub fn check_key(
        &self,
        cartridge: &CartridgeRef,
        publisher: Option<&Publisher>,
    ) -> Result<(), String> {
        let Some(key) = publisher else {
            return Ok(());
        };
        let known = self
            .trusted(&cartridge.publisher)
            .map(|trusted| &trusted.key)
            .into_iter()
            .chain(
                self.entry(cartridge)
                    .and_then(|entry| entry.publisher_key.as_ref()),
            );
        for other in known {
            if other != key {
                return Err(format!(
                    "refusing {cartridge}: its publisher key is not the one already known as {}",
                    cartridge.publisher
                ));
            }
        }
        Ok(())
    }

    pub fn trust(&mut self, key: &Publisher, name: &str, now: u64) -> Result<(), String> {
        let name = clean_name(name)?;
        let fingerprint = key.fingerprint();
        match self
            .publishers
            .iter_mut()
            .find(|publisher| publisher.key.fingerprint() == fingerprint)
        {
            Some(known) if &known.key != key => Err(format!(
                "another publisher key is already trusted as {fingerprint}"
            )),
            Some(known) => {
                known.name = name;
                Ok(())
            }
            None => {
                self.publishers.push(TrustedPublisher {
                    key: key.clone(),
                    name,
                    trusted_at: now,
                });
                Ok(())
            }
        }
    }

    pub fn forget_publisher(&mut self, fingerprint: &str) -> bool {
        let before = self.publishers.len();
        self.publishers
            .retain(|publisher| publisher.key.fingerprint() != fingerprint);
        if self.publishers.len() == before {
            return false;
        }
        for entry in &mut self.entries {
            if entry.cartridge.publisher == fingerprint {
                entry.approved_capabilities = None;
            }
        }
        true
    }

    pub fn grant(&self, entry: &Entry, declared: &Capabilities) -> Grant {
        if declared.is_empty() {
            Grant::None
        } else if self.is_trusted(entry.publisher_key.as_ref())
            && entry.approved_capabilities.as_ref() == Some(declared)
        {
            Grant::Granted
        } else {
            Grant::Withheld
        }
    }

    pub fn outlook(
        &self,
        cartridge: &CartridgeRef,
        publisher: Option<&Publisher>,
        declared: &Capabilities,
    ) -> Outlook {
        if declared.is_empty() {
            Outlook::None
        } else if publisher.is_none() {
            Outlook::Unsigned
        } else if !self.is_trusted(publisher) {
            Outlook::NeedsTrust
        } else if self.approved(cartridge) == Some(declared) {
            Outlook::Approved
        } else {
            Outlook::NeedsApproval
        }
    }

    fn approved(&self, cartridge: &CartridgeRef) -> Option<&Capabilities> {
        self.entry(cartridge)
            .and_then(|entry| entry.approved_capabilities.as_ref())
    }

    pub fn decide(
        &self,
        cartridge: &CartridgeRef,
        title: &str,
        publisher: Option<&Publisher>,
        declared: &Capabilities,
        approval: &Approval,
    ) -> Result<Decision, String> {
        self.check_key(cartridge, publisher)?;
        let trust = match (&approval.trust_publisher, publisher) {
            (Some(_), None) => {
                return Err(format!(
                    "{title} is unsigned, so it has no publisher to trust"
                ))
            }
            (Some(name), Some(key)) => Some((key.clone(), clean_name(name)?)),
            (None, _) => None,
        };
        let trusted = trust.is_some() || self.is_trusted(publisher);
        let grant = if declared.is_empty() {
            Grant::None
        } else if !trusted {
            Grant::Withheld
        } else if approval.capabilities || self.approved(cartridge) == Some(declared) {
            Grant::Granted
        } else {
            return Err(format!(
                "{title} asks for privileged capabilities ({}); approve them to install it",
                describe(declared).join("; ")
            ));
        };
        Ok(Decision {
            trust,
            approved: (grant == Grant::Granted).then(|| declared.clone()),
            grant,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cabinet_cartridge::manifest::{Hosts, HostsTarget};
    use cabinet_cartridge::signature::KeySigner;

    fn publisher() -> Publisher {
        let (_, public) = KeySigner::generate(None).unwrap();
        Publisher::parse(&public).unwrap()
    }

    fn hosts(name: &str) -> Capabilities {
        Capabilities {
            hosts: Some(Hosts {
                names: vec![name.to_string()],
                target: HostsTarget::Service,
                scope: Scope::Session,
            }),
            ..Capabilities::default()
        }
    }

    fn approve(name: Option<&str>, capabilities: bool) -> Approval {
        Approval {
            trust_publisher: name.map(str::to_string),
            capabilities,
        }
    }

    #[test]
    fn a_publisher_is_trusted_under_a_clean_name() {
        let mut registry = Registry::default();
        let key = publisher();
        assert_eq!(registry.publisher_view(None).standing, Standing::Unsigned);
        let view = registry.publisher_view(Some(&key));
        assert_eq!(view.standing, Standing::Unknown);
        assert_eq!(view.fingerprint, Some(key.fingerprint()));
        assert!(registry.trust(&key, "  ", 1).is_err());
        assert!(registry
            .trust(&key, &"x".repeat(MAX_NAME_LEN + 1), 1)
            .is_err());
        assert!(registry.trust(&key, "a\nb", 1).is_err());
        registry.trust(&key, "  Player One ", 1).unwrap();
        let view = registry.publisher_view(Some(&key));
        assert_eq!(view.standing, Standing::Trusted);
        assert_eq!(view.name.as_deref(), Some("Player One"));
        registry.trust(&key, "Renamed", 2).unwrap();
        assert_eq!(registry.publishers.len(), 1);
        assert_eq!(registry.publishers[0].name, "Renamed");
        assert_eq!(registry.publishers[0].trusted_at, 1);
        assert!(!registry.is_trusted(Some(&publisher())));
        assert!(!registry.is_trusted(None));
    }

    #[test]
    fn capabilities_need_a_trusted_publisher_and_an_approval() {
        let mut registry = Registry::default();
        let key = publisher();
        let cartridge = CartridgeRef::new(Some(&key), "test");
        let unsigned = CartridgeRef::new(None, "test");
        let declared = hosts("cabinet-test.invalid");
        let none = Capabilities::default();

        let decision = registry
            .decide(&unsigned, "Test", None, &declared, &approve(None, true))
            .unwrap();
        assert_eq!(decision.grant, Grant::Withheld);
        assert_eq!(decision.approved, None);
        let err = registry
            .decide(&unsigned, "Test", None, &none, &approve(Some("Me"), false))
            .unwrap_err();
        assert!(err.contains("no publisher to trust"), "{err}");

        let decision = registry
            .decide(
                &cartridge,
                "Test",
                Some(&key),
                &declared,
                &approve(None, true),
            )
            .unwrap();
        assert_eq!(decision.grant, Grant::Withheld);
        assert_eq!(decision.trust, None);
        assert_eq!(
            registry.outlook(&cartridge, Some(&key), &declared),
            Outlook::NeedsTrust
        );
        assert_eq!(
            registry.outlook(&unsigned, None, &declared),
            Outlook::Unsigned
        );
        assert_eq!(
            registry.outlook(&cartridge, Some(&key), &none),
            Outlook::None
        );

        let err = registry
            .decide(
                &cartridge,
                "Test",
                Some(&key),
                &declared,
                &approve(Some("Friend"), false),
            )
            .unwrap_err();
        assert!(err.contains("approve them"), "{err}");
        assert!(err.contains("cabinet-test.invalid"), "{err}");

        let decision = registry
            .decide(
                &cartridge,
                "Test",
                Some(&key),
                &declared,
                &approve(Some("Friend"), true),
            )
            .unwrap();
        assert_eq!(decision.grant, Grant::Granted);
        assert_eq!(decision.trust, Some((key.clone(), "Friend".to_string())));
        assert_eq!(decision.approved.as_ref(), Some(&declared));

        registry.trust(&key, "Friend", 1).unwrap();
        let entry = registry.entry_or_insert(&cartridge, Some(&key));
        entry.approved_capabilities = Some(declared.clone());
        assert_eq!(
            registry.outlook(&cartridge, Some(&key), &declared),
            Outlook::Approved
        );
        let same = registry
            .decide(
                &cartridge,
                "Test",
                Some(&key),
                &declared,
                &Approval::default(),
            )
            .unwrap();
        assert_eq!(same.grant, Grant::Granted);
        let changed = hosts("other.invalid");
        assert_eq!(
            registry.outlook(&cartridge, Some(&key), &changed),
            Outlook::NeedsApproval
        );
        assert!(registry
            .decide(
                &cartridge,
                "Test",
                Some(&key),
                &changed,
                &Approval::default()
            )
            .is_err());
        let dropped = registry
            .decide(&cartridge, "Test", Some(&key), &none, &Approval::default())
            .unwrap();
        assert_eq!(dropped.grant, Grant::None);
        assert_eq!(dropped.approved, None);
    }

    #[test]
    fn forgetting_a_publisher_withdraws_its_approvals() {
        let mut registry = Registry::default();
        let key = publisher();
        let other = publisher();
        let cartridge = CartridgeRef::new(Some(&key), "test");
        let elsewhere = CartridgeRef::new(Some(&other), "test");
        let declared = hosts("cabinet-test.invalid");
        registry.trust(&key, "Friend", 1).unwrap();
        registry.trust(&other, "Other", 1).unwrap();
        for (cartridge, key) in [(&cartridge, &key), (&elsewhere, &other)] {
            registry
                .entry_or_insert(cartridge, Some(key))
                .approved_capabilities = Some(declared.clone());
        }
        let entry = registry.entry(&cartridge).unwrap();
        assert_eq!(registry.grant(entry, &declared), Grant::Granted);
        assert_eq!(registry.grant(entry, &hosts("b.invalid")), Grant::Withheld);
        assert_eq!(registry.grant(entry, &Capabilities::default()), Grant::None);

        assert!(registry.forget_publisher(&key.fingerprint()));
        assert!(!registry.forget_publisher(&key.fingerprint()));
        let entry = registry.entry(&cartridge).unwrap();
        assert_eq!(entry.approved_capabilities, None);
        assert_eq!(registry.grant(entry, &declared), Grant::Withheld);
        assert_eq!(registry.known_key(&key.fingerprint()).as_ref(), Some(&key));
        let kept = registry.entry(&elsewhere).unwrap();
        assert_eq!(registry.grant(kept, &declared), Grant::Granted);
    }

    #[test]
    fn a_different_key_under_a_known_fingerprint_is_refused() {
        let mut registry = Registry::default();
        let key = publisher();
        let imposter = publisher();
        let cartridge = CartridgeRef::new(Some(&key), "test");
        registry.entry_or_insert(&cartridge, Some(&key));
        assert!(registry.check_key(&cartridge, Some(&key)).is_ok());
        let err = registry.check_key(&cartridge, Some(&imposter)).unwrap_err();
        assert!(err.contains("not the one already known"), "{err}");
        registry.entries.clear();
        registry.trust(&key, "Friend", 1).unwrap();
        assert!(registry.check_key(&cartridge, Some(&imposter)).is_err());
        assert!(registry
            .decide(
                &cartridge,
                "Test",
                Some(&imposter),
                &Capabilities::default(),
                &Approval::default()
            )
            .is_err());
    }

    #[test]
    fn describes_every_capability() {
        let capabilities: Capabilities = serde_json::from_value(serde_json::json!({
            "low_ports": [80, 443],
            "hosts": {
                "names": ["api.example-game.net"],
                "target": "service",
                "scope": "session"
            },
            "trust_ca": {
                "asset": "ca",
                "permitted_names": ["example-game.net"],
                "scope": "install",
                "stores": ["system", "bottle"],
                "bottle_setting": "bottle"
            },
            "firewall": {
                "scope": "session",
                "inbound": [{ "proto": "tcp", "port": 55435 }]
            }
        }))
        .unwrap();
        assert_eq!(
            describe(&capabilities),
            vec![
                "let its service listen on port 80, 443",
                "point api.example-game.net at the room's service while a session runs",
                "trust its certificate authority for example-game.net (system store, bottle) while it is installed",
                "open the firewall for inbound tcp 55435 while a session runs",
            ]
        );
        assert!(describe(&Capabilities::default()).is_empty());
    }
}
