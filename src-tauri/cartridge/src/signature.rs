use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};

pub const SIGNATURE_PATH: &str = "cartridge.sig";
pub const PUBLISHER_KEY_PATH: &str = "publisher.pub";
pub const FINGERPRINT_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Publisher {
    pub key: String,
}

impl Publisher {
    pub fn parse(text: &str) -> Result<Publisher> {
        let key = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .find(|line| !line.starts_with("untrusted comment:"))
            .ok_or_else(|| Error::Signature("the publisher key is empty".to_string()))?;
        minisign_verify::PublicKey::from_base64(key)
            .map_err(|err| Error::Signature(format!("invalid publisher key: {err}")))?;
        Ok(Publisher {
            key: key.to_string(),
        })
    }

    pub fn fingerprint(&self) -> String {
        crate::digest::sha256_bytes(self.key.as_bytes())[..FINGERPRINT_LEN].to_string()
    }

    pub fn file_text(&self) -> String {
        format!("untrusted comment: cartridge publisher key\n{}\n", self.key)
    }

    pub fn verify(&self, digest: &str, signature: &str) -> Result<()> {
        let key = minisign_verify::PublicKey::from_base64(&self.key)
            .map_err(|err| Error::Signature(format!("invalid publisher key: {err}")))?;
        let signature = minisign_verify::Signature::decode(signature)
            .map_err(|err| Error::Signature(format!("invalid cartridge.sig: {err}")))?;
        key.verify(digest.as_bytes(), &signature, false)
            .map_err(|err| {
                Error::Signature(format!(
                    "cartridge.sig does not match the content for publisher {}: {err}",
                    self.fingerprint()
                ))
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedDigest {
    pub signature: String,
    pub publisher: Publisher,
}

pub trait Signer {
    fn sign(&self, digest: &str) -> Result<SignedDigest>;
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=
trusted comment: timestamp:1556193335\tfile:test
y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==";

    #[test]
    fn verifies_a_prehashed_minisign_signature() {
        let publisher =
            Publisher::parse(&format!("untrusted comment: minisign public key\n{KEY}\n")).unwrap();
        assert_eq!(publisher.key, KEY);
        assert_eq!(Publisher::parse(KEY).unwrap(), publisher);
        assert_eq!(Publisher::parse(&publisher.file_text()).unwrap(), publisher);
        assert_eq!(publisher.fingerprint().len(), FINGERPRINT_LEN);
        publisher.verify("test", SIGNATURE).unwrap();
        let err = publisher.verify("Test", SIGNATURE).unwrap_err().to_string();
        assert!(err.contains("does not match"), "{err}");
        assert!(publisher.verify("test", "garbage").is_err());
    }

    #[test]
    fn rejects_malformed_keys() {
        assert!(Publisher::parse("").is_err());
        assert!(Publisher::parse("untrusted comment: x\n").is_err());
        assert!(Publisher::parse("not base64 at all").is_err());
    }
}
