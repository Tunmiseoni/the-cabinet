use crate::protocol::{Change, Scope};
use std::net::Ipv4Addr;

pub const PROVEN_HOSTS: Option<&[&str]> = Some(&["cabinet-test.invalid"]);
pub const MAX_HOSTS_NAMES: usize = 32;
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
