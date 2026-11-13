use sha2::{Digest, Sha256};
use x509_parser::extensions::{GeneralName, GeneralSubtree};
use x509_parser::prelude::{FromDer, X509Certificate, X509Version};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ca {
    pub der: Vec<u8>,
    pub sha256: String,
    pub subject: String,
    pub not_after: i64,
    pub permitted: Vec<String>,
}

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn pem(der: &[u8]) -> String {
    let mut encoded = String::new();
    for chunk in der.chunks(3) {
        let bytes = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let joined = (u32::from(bytes[0]) << 16) | (u32::from(bytes[1]) << 8) | u32::from(bytes[2]);
        for index in 0..4 {
            if index <= chunk.len() {
                encoded.push(BASE64[(joined >> (18 - 6 * index) & 63) as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    let mut text = String::from("-----BEGIN CERTIFICATE-----\n");
    for line in encoded.as_bytes().chunks(64) {
        text.push_str(std::str::from_utf8(line).expect("base64 is ASCII"));
        text.push('\n');
    }
    text.push_str("-----END CERTIFICATE-----\n");
    text
}

fn der(bytes: &[u8]) -> Result<Vec<u8>, String> {
    if !bytes.starts_with(b"-----") {
        return Ok(bytes.to_vec());
    }
    let mut blocks = x509_parser::pem::Pem::iter_from_buffer(bytes);
    let first = blocks
        .next()
        .ok_or("it holds no certificate")?
        .map_err(|err| format!("it is not readable PEM: {err}"))?;
    if first.label != "CERTIFICATE" {
        return Err(format!("it holds a {}, not a certificate", first.label));
    }
    if blocks.next().is_some() {
        return Err("it holds more than one PEM block".to_string());
    }
    Ok(first.contents)
}

fn excludes_every_address(excluded: &[GeneralSubtree<'_>]) -> bool {
    [8, 32].iter().all(|length| {
        excluded.iter().any(|subtree| {
            matches!(&subtree.base, GeneralName::IPAddress(range)
                if range.len() == *length && range.iter().all(|byte| *byte == 0))
        })
    })
}

pub fn check(bytes: &[u8], permitted_names: &[String], now: i64) -> Result<Ca, String> {
    let der = der(bytes)?;
    let (rest, cert) = X509Certificate::from_der(&der)
        .map_err(|err| format!("it is not a readable certificate: {err}"))?;
    if !rest.is_empty() {
        return Err("it has data after the certificate".to_string());
    }
    if cert.version() != X509Version::V3 {
        return Err("it is not a version 3 certificate".to_string());
    }
    if cert.subject().as_raw() != cert.issuer().as_raw() {
        return Err("it is not a root: its issuer is someone else".to_string());
    }
    let unreadable = |what: &str| format!("its {what} cannot be read");
    let basic = cert
        .basic_constraints()
        .map_err(|_| unreadable("basic constraints"))?
        .ok_or("it has no basic constraints")?;
    if !basic.critical || !basic.value.ca {
        return Err("it is not marked as a certificate authority".to_string());
    }
    if let Some(usage) = cert.key_usage().map_err(|_| unreadable("key usage"))? {
        if !usage.value.key_cert_sign() {
            return Err("its key usage does not allow signing certificates".to_string());
        }
    }
    let constraints = cert
        .name_constraints()
        .map_err(|_| unreadable("name constraints"))?
        .ok_or("it has no name constraints, so it could vouch for any site")?;
    if !constraints.critical {
        return Err("its name constraints are not marked critical".to_string());
    }
    let mut permitted = Vec::new();
    for subtree in constraints.value.permitted_subtrees.iter().flatten() {
        match &subtree.base {
            GeneralName::DNSName(name) => permitted.push(name.to_string()),
            other => {
                return Err(format!(
                    "it permits something other than DNS names: {other}"
                ))
            }
        }
    }
    let mut wanted: Vec<String> = permitted_names.to_vec();
    permitted.sort();
    wanted.sort();
    if permitted.is_empty() || permitted != wanted {
        return Err(format!(
            "its name constraints permit [{}], but the cartridge declares [{}]",
            permitted.join(", "),
            wanted.join(", ")
        ));
    }
    let excluded = constraints
        .value
        .excluded_subtrees
        .as_deref()
        .unwrap_or(&[]);
    if !excludes_every_address(excluded) {
        return Err("its name constraints do not exclude every IPv4 and IPv6 address".to_string());
    }
    let validity = cert.validity();
    let (not_before, not_after) = (
        validity.not_before.timestamp(),
        validity.not_after.timestamp(),
    );
    if now < not_before || now > not_after {
        return Err("it is not valid today (not yet valid, or expired)".to_string());
    }
    Ok(Ca {
        sha256: Sha256::digest(&der)
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect(),
        subject: cert.subject().to_string(),
        not_after,
        permitted,
        der,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_600_000;
    const NAME: &str = "cabinet-test.invalid";

    fn names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn refused(fixture: &[u8], permitted: &[&str]) -> String {
        check(fixture, &names(permitted), NOW).unwrap_err()
    }

    #[test]
    fn accepts_a_root_limited_to_the_declared_names() {
        let pem = check(include_bytes!("fixtures/ca-good.pem"), &names(&[NAME]), NOW).unwrap();
        let der = check(include_bytes!("fixtures/ca-good.der"), &names(&[NAME]), NOW).unwrap();
        assert_eq!(pem, der);
        assert_eq!(pem.permitted, names(&[NAME]));
        assert_eq!(pem.subject, "CN=Cabinet fixture good");
        assert_eq!(pem.sha256.len(), 64);
        check(
            include_bytes!("fixtures/ca-two.pem"),
            &names(&["other-test.invalid", NAME]),
            NOW,
        )
        .unwrap();
    }

    #[test]
    fn the_declared_names_must_equal_the_constraints() {
        let good = include_bytes!("fixtures/ca-good.pem");
        for permitted in [
            &["other-test.invalid"][..],
            &[NAME, "other-test.invalid"],
            &["invalid"],
            &[],
        ] {
            let err = refused(good, permitted);
            assert!(err.contains("but the cartridge declares"), "{err}");
        }
        let err = refused(include_bytes!("fixtures/ca-two.pem"), &[NAME]);
        assert!(err.contains("but the cartridge declares"), "{err}");
    }

    #[test]
    fn refuses_roots_that_are_not_tightly_constrained() {
        let cases: [(&[u8], &str); 6] = [
            (
                include_bytes!("fixtures/ca-unconstrained.pem"),
                "no name constraints",
            ),
            (
                include_bytes!("fixtures/ca-not-critical.pem"),
                "not marked critical",
            ),
            (include_bytes!("fixtures/ca-no-ip.pem"), "do not exclude"),
            (include_bytes!("fixtures/ca-v4-only.pem"), "do not exclude"),
            (
                include_bytes!("fixtures/ca-permits-ip.pem"),
                "other than DNS names",
            ),
            (
                include_bytes!("fixtures/ca-leaf.pem"),
                "not marked as a certificate authority",
            ),
        ];
        for (fixture, expected) in cases {
            let err = refused(fixture, &[NAME]);
            assert!(err.contains(expected), "{expected}: {err}");
        }
    }

    #[test]
    fn refuses_a_root_outside_its_validity() {
        let short = include_bytes!("fixtures/ca-short.pem");
        check(short, &names(&[NAME]), NOW + 100_000).unwrap_err();
        check(short, &names(&[NAME]), 1_794_583_000).unwrap_err();
        check(short, &names(&[NAME]), 1_794_584_000).unwrap();
    }

    #[test]
    fn refuses_files_that_are_not_one_certificate() {
        let good = include_bytes!("fixtures/ca-good.pem");
        let twice = [&good[..], &good[..]].concat();
        assert!(refused(&twice, &[NAME]).contains("more than one"));
        assert!(refused(b"hello", &[NAME]).contains("not a readable certificate"));
        assert!(refused(
            b"-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n",
            &[NAME]
        )
        .contains("not a certificate"));
        let mut trailing = include_bytes!("fixtures/ca-good.der").to_vec();
        trailing.push(0);
        assert!(refused(&trailing, &[NAME]).contains("after the certificate"));
    }

    #[test]
    fn writes_a_certificate_back_as_pem() {
        let ca = check(include_bytes!("fixtures/ca-good.der"), &names(&[NAME]), NOW).unwrap();
        let text = super::pem(&ca.der);
        assert_eq!(
            text.replace('\n', ""),
            String::from_utf8_lossy(include_bytes!("fixtures/ca-good.pem")).replace('\n', "")
        );
        assert_eq!(check(text.as_bytes(), &names(&[NAME]), NOW).unwrap(), ca);
        assert!(text.lines().all(|line| line.len() <= 64));
    }
}
