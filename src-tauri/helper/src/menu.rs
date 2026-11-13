use crate::protocol::{Change, Scope};
use std::net::Ipv4Addr;

pub const PROVEN_HOSTS: Option<&[&str]> = Some(&["cabinet-test.invalid"]);
pub const MAX_HOSTS_NAMES: usize = 32;
pub const MAX_PROGRAM_PATH: usize = 1024;
pub const MAX_PORTS: usize = 16;
pub const MAX_CERTIFICATE: usize = 16 * 1024;
const PEM_START: &str = "-----BEGIN CERTIFICATE-----";
const RESERVED: &[&str] = &["localhost", "broadcasthost"];

pub fn is_dns_name(value: &str) -> bool {
    value.len() <= 253
        && value.split('.').count() >= 2
        && value.split('.').all(|label| {
            let bytes = label.as_bytes();
            !bytes.is_empty()
                && bytes.len() <= 63
                && bytes
                    .iter()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
                && bytes[0] != b'-'
                && bytes[bytes.len() - 1] != b'-'
        })
        && !value
            .rsplit('.')
            .next()
            .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
}

pub fn is_room_address(target: Ipv4Addr) -> bool {
    let [a, b, ..] = target.octets();
    target == Ipv4Addr::LOCALHOST || (a == 100 && (64..128).contains(&b))
}

pub fn is_owner(owner: &str) -> bool {
    let part = |part: &str| {
        !part.is_empty()
            && part.len() <= 64
            && part
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"-_.".contains(&b))
    };
    match owner.split_once('/') {
        Some((publisher, id)) => part(publisher) && part(id),
        None => false,
    }
}

pub fn check(owner: &str, scope: Scope, change: &Change) -> Result<(), String> {
    if !is_owner(owner) {
        return Err("the owner is not a publisher/id pair".to_string());
    }
    match change {
        Change::Hosts { names, target } => {
            if scope != Scope::Session {
                return Err("hosts entries are session-scoped".to_string());
            }
            if names.is_empty() || names.len() > MAX_HOSTS_NAMES {
                return Err(format!(
                    "a hosts entry takes 1 to {MAX_HOSTS_NAMES} names, not {}",
                    names.len()
                ));
            }
            for (index, name) in names.iter().enumerate() {
                if !is_dns_name(name) || RESERVED.contains(&name.as_str()) {
                    return Err(format!("\"{name}\" is not a lowercase DNS name"));
                }
                if names[..index].contains(name) {
                    return Err(format!("\"{name}\" is listed twice"));
                }
                if PROVEN_HOSTS.is_some_and(|allowed| !allowed.contains(&name.as_str())) {
                    return Err(format!(
                        "\"{name}\" is not allowed yet: this helper only redirects its test name"
                    ));
                }
            }
            if !is_room_address(*target) {
                return Err(format!("{target} is not this machine or a tailnet address"));
            }
            Ok(())
        }
        Change::Firewall {
            program,
            ports,
            keep,
            opened,
        } => {
            if scope != Scope::Session {
                return Err("firewall allowances are session-scoped".to_string());
            }
            if *keep || !opened.is_empty() {
                return Err("only the helper decides what it keeps".to_string());
            }
            check_program(program)?;
            if ports.len() > MAX_PORTS {
                return Err(format!(
                    "a firewall allowance takes at most {MAX_PORTS} ports"
                ));
            }
            for (index, port) in ports.iter().enumerate() {
                if port.port == 0 {
                    return Err("port 0 cannot be allowed".to_string());
                }
                if ports[..index].contains(port) {
                    return Err(format!("port {} is listed twice", port.port));
                }
            }
            Ok(())
        }
        Change::LowPorts { program, keep } => {
            if scope != Scope::Session {
                return Err("low ports are session-scoped".to_string());
            }
            if *keep {
                return Err("only the helper decides what it keeps".to_string());
            }
            check_program(program)
        }
        Change::TrustCa {
            certificate,
            names,
            anchor,
        } => {
            if scope != Scope::Install {
                return Err("certificate authorities are install-scoped".to_string());
            }
            if !anchor.is_empty() {
                return Err("only the helper decides where a certificate goes".to_string());
            }
            if certificate.len() > MAX_CERTIFICATE
                || !certificate.trim_start().starts_with(PEM_START)
            {
                return Err("the certificate is not a single PEM certificate".to_string());
            }
            if names.is_empty() || names.len() > MAX_HOSTS_NAMES {
                return Err(format!(
                    "a certificate authority takes 1 to {MAX_HOSTS_NAMES} names, not {}",
                    names.len()
                ));
            }
            for name in names {
                if !is_dns_name(name) {
                    return Err(format!("\"{name}\" is not a lowercase DNS name"));
                }
                if PROVEN_HOSTS.is_some_and(|allowed| !allowed.contains(&name.as_str())) {
                    return Err(format!(
                        "\"{name}\" is not allowed yet: this helper only trusts authorities for its test name"
                    ));
                }
            }
            Ok(())
        }
    }
}

fn check_program(program: &str) -> Result<(), String> {
    if !program.starts_with('/') && !is_windows_absolute(program)
        || program.len() > MAX_PROGRAM_PATH
        || program.chars().any(char::is_control)
    {
        return Err("the program is not an absolute path".to_string());
    }
    Ok(())
}

fn is_windows_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    cfg!(windows)
        && bytes.len() > 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'\\'
}

pub fn check_certificate(certificate: &str, names: &[String], now: i64) -> Result<(), String> {
    cabinet_cartridge::ca::check(certificate.as_bytes(), names, now)
        .map(|_| ())
        .map_err(|err| format!("the certificate authority is refused: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Port, Proto};

    fn hosts(names: &[&str], target: &str) -> Change {
        Change::Hosts {
            names: names.iter().map(|name| name.to_string()).collect(),
            target: target.parse().unwrap(),
        }
    }

    const OWNER: &str = "0123456789abcdef/test";

    #[test]
    fn accepts_the_test_name_on_loopback_and_the_tailnet() {
        for target in ["127.0.0.1", "100.64.0.1", "100.64.0.3"] {
            check(
                OWNER,
                Scope::Session,
                &hosts(&["cabinet-test.invalid"], target),
            )
            .unwrap();
        }
    }

    #[test]
    fn refuses_targets_outside_the_room() {
        for target in ["203.0.113.7", "192.0.2.1", "127.0.0.2", "0.0.0.0"] {
            let err = check(
                OWNER,
                Scope::Session,
                &hosts(&["cabinet-test.invalid"], target),
            )
            .unwrap_err();
            assert!(err.contains("not this machine"), "{target}: {err}");
        }
    }

    #[test]
    fn the_tailnet_range_is_exactly_the_shared_address_space() {
        for second in 0..=255u8 {
            assert_eq!(
                is_room_address(Ipv4Addr::new(100, second, 0, 1)),
                (64..128).contains(&second),
                "100.{second}.x.x"
            );
        }
    }

    #[test]
    fn refuses_names_outside_the_proven_list() {
        let err = check(
            OWNER,
            Scope::Session,
            &hosts(&["api.example-game.net"], "127.0.0.1"),
        )
        .unwrap_err();
        assert!(err.contains("not allowed yet"), "{err}");
    }

    #[test]
    fn refuses_malformed_requests() {
        let bad_names: &[&[&str]] = &[
            &[],
            &["localhost"],
            &["Cabinet-Test.invalid"],
            &["cabinet-test.invalid\n203.0.113.7 evil.example"],
            &["cabinet-test.invalid # x"],
            &["cabinet-test.invalid", "cabinet-test.invalid"],
        ];
        for names in bad_names {
            assert!(
                check(OWNER, Scope::Session, &hosts(names, "127.0.0.1")).is_err(),
                "{names:?}"
            );
        }
        let change = hosts(&["cabinet-test.invalid"], "127.0.0.1");
        assert!(check(OWNER, Scope::Install, &change).is_err());
        for owner in ["", "test", "a/b/c", "A/test", "a/te st", "a/"] {
            assert!(check(owner, Scope::Session, &change).is_err(), "{owner}");
        }
    }

    #[test]
    fn firewall_requests_name_one_absolute_program_for_a_session() {
        let firewall = |program: &str, keep: bool| Change::Firewall {
            program: program.to_string(),
            ports: Vec::new(),
            keep,
            opened: Vec::new(),
        };
        check(OWNER, Scope::Session, &firewall("/a/b", false)).unwrap();
        assert!(check(OWNER, Scope::Install, &firewall("/a/b", false)).is_err());
        assert!(check(OWNER, Scope::Session, &firewall("/a/b", true)).is_err());
        let opened = Change::Firewall {
            program: "/a/b".to_string(),
            ports: Vec::new(),
            keep: false,
            opened: vec![Port {
                proto: Proto::Tcp,
                port: 987,
            }],
        };
        assert!(check(OWNER, Scope::Session, &opened).is_err());
        for program in ["", "a/b", "/a\nb", &format!("/{}", "a".repeat(2000))] {
            assert!(
                check(OWNER, Scope::Session, &firewall(program, false)).is_err(),
                "{program}"
            );
        }
    }

    fn port(port: u16) -> Port {
        Port {
            proto: Proto::Tcp,
            port,
        }
    }

    #[test]
    fn firewall_ports_are_bounded_and_distinct() {
        let firewall = |ports: Vec<Port>| Change::Firewall {
            program: "/a/b".to_string(),
            ports,
            keep: false,
            opened: Vec::new(),
        };
        check(
            OWNER,
            Scope::Session,
            &firewall(vec![port(987), port(55435)]),
        )
        .unwrap();
        for ports in [
            vec![port(0)],
            vec![port(987), port(987)],
            (1..=MAX_PORTS as u16 + 1).map(port).collect(),
        ] {
            assert!(
                check(OWNER, Scope::Session, &firewall(ports.clone())).is_err(),
                "{ports:?}"
            );
        }
    }

    #[test]
    fn low_ports_name_one_absolute_program_for_a_session() {
        let low = |program: &str| Change::LowPorts {
            program: program.to_string(),
            keep: false,
        };
        check(OWNER, Scope::Session, &low("/a/b")).unwrap();
        assert!(check(OWNER, Scope::Install, &low("/a/b")).is_err());
        assert!(check(OWNER, Scope::Session, &low("a/b")).is_err());
        let kept = Change::LowPorts {
            program: "/a/b".to_string(),
            keep: true,
        };
        assert!(check(OWNER, Scope::Session, &kept).is_err());
    }

    const GOOD: &str = include_str!("../../cartridge/src/fixtures/ca-good.pem");

    fn trust(names: &[&str], anchor: &str) -> Change {
        Change::TrustCa {
            certificate: GOOD.to_string(),
            names: names.iter().map(|name| name.to_string()).collect(),
            anchor: anchor.to_string(),
        }
    }

    #[test]
    fn certificate_authorities_are_install_scoped_and_limited_to_the_test_name() {
        check(OWNER, Scope::Install, &trust(&["cabinet-test.invalid"], "")).unwrap();
        assert!(check(OWNER, Scope::Session, &trust(&["cabinet-test.invalid"], "")).is_err());
        assert!(check(
            OWNER,
            Scope::Install,
            &trust(&["cabinet-test.invalid"], "/etc/x")
        )
        .is_err());
        assert!(check(OWNER, Scope::Install, &trust(&[], "")).is_err());
        let err = check(OWNER, Scope::Install, &trust(&["example-game.net"], "")).unwrap_err();
        assert!(err.contains("not allowed yet"), "{err}");
        let der = Change::TrustCa {
            certificate: "MIIB".to_string(),
            names: vec!["cabinet-test.invalid".to_string()],
            anchor: String::new(),
        };
        assert!(check(OWNER, Scope::Install, &der).is_err());
    }

    #[test]
    fn the_helper_checks_the_certificate_itself() {
        let now = 1_794_600_000;
        let names = vec!["cabinet-test.invalid".to_string()];
        check_certificate(GOOD, &names, now).unwrap();
        let err = check_certificate(GOOD, &["other.invalid".to_string()], now).unwrap_err();
        assert!(err.contains("refused"), "{err}");
    }
}
