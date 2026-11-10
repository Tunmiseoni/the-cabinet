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

pub const PASSWORD_ENV: &str = "CABINET_PACK_KEY_PASSWORD";

fn signing(err: impl std::fmt::Display) -> Error {
    Error::Signature(err.to_string())
}

pub struct KeySigner {
    secret: minisign::SecretKey,
    public: minisign::PublicKey,
}

impl KeySigner {
    pub fn from_key_file(text: &str, password: Option<String>) -> Result<Self> {
        let boxed = minisign::SecretKeyBox::from_string(text)
            .map_err(|err| signing(format!("not a minisign secret key: {err}")))?;
        let secret = match password {
            Some(password) => boxed.into_secret_key(Some(password)),
            None => boxed.into_unencrypted_secret_key(),
        }
        .map_err(|err| {
            signing(format!(
                "cannot open the secret key (an encrypted key needs {PASSWORD_ENV}): {err}"
            ))
        })?;
        let public = minisign::PublicKey::from_secret_key(&secret).map_err(signing)?;
        Ok(Self { secret, public })
    }

    pub fn generate(password: Option<String>) -> Result<(String, String)> {
        let pair = match password {
            Some(password) => minisign::KeyPair::generate_encrypted_keypair(Some(password)),
            None => minisign::KeyPair::generate_unencrypted_keypair(),
        }
        .map_err(signing)?;
        let secret = pair
            .sk
            .to_box(Some("cartridge publisher secret key"))
            .map_err(signing)?
            .to_string();
        let public = Publisher {
            key: pair.pk.to_base64(),
        }
        .file_text();
        Ok((secret, public))
    }

    pub fn publisher(&self) -> Publisher {
        Publisher {
            key: self.public.to_base64(),
        }
    }
}

impl Signer for KeySigner {
    fn sign(&self, digest: &str) -> Result<SignedDigest> {
        let trusted = format!("cartridge digest {digest}");
        let signature = minisign::sign(
            Some(&self.public),
            &self.secret,
            digest.as_bytes(),
            Some(&trusted),
            Some("cartridge signature"),
        )
        .map_err(signing)?;
        Ok(SignedDigest {
            signature: signature.into_string(),
            publisher: self.publisher(),
        })
    }
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
    fn signs_and_verifies_a_digest() {
        let (secret, public) = KeySigner::generate(None).unwrap();
        let signer = KeySigner::from_key_file(&secret, None).unwrap();
        let publisher = Publisher::parse(&public).unwrap();
        assert_eq!(signer.publisher(), publisher);
        let signed = signer.sign("abc123").unwrap();
        assert_eq!(signed.publisher, publisher);
        publisher.verify("abc123", &signed.signature).unwrap();
        assert!(publisher.verify("abc124", &signed.signature).is_err());
        let (_, other) = KeySigner::generate(None).unwrap();
        assert!(Publisher::parse(&other)
            .unwrap()
            .verify("abc123", &signed.signature)
            .is_err());
        assert!(KeySigner::from_key_file("not a key", None).is_err());
    }

    #[test]
    fn an_encrypted_key_needs_its_password() {
        let (secret, _) = KeySigner::generate(Some("hunter2".to_string())).unwrap();
        let err = KeySigner::from_key_file(&secret, None)
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains(PASSWORD_ENV), "{err}");
        assert!(KeySigner::from_key_file(&secret, Some("wrong".to_string())).is_err());
        let signer = KeySigner::from_key_file(&secret, Some("hunter2".to_string())).unwrap();
        let signed = signer.sign("abc").unwrap();
        signer.publisher().verify("abc", &signed.signature).unwrap();
    }

    #[test]
    fn rejects_malformed_keys() {
        assert!(Publisher::parse("").is_err());
        assert!(Publisher::parse("untrusted comment: x\n").is_err());
        assert!(Publisher::parse("not base64 at all").is_err());
    }
}
