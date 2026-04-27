//! Ed25519 keypair lifecycle.
//!
//! [`Keypair`] wraps `ed25519_dalek::SigningKey` so callers do not
//! depend on the dalek surface directly — the day we want to swap
//! in a different curve or HSM-backed signer, this is the seam. A
//! [`PublicKeyRef`] is the read-only counterpart: callers that
//! verify but never sign hold one of these.

use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use ed25519_dalek::{Signature, SigningKey, Verifier as Ed25519Verifier, VerifyingKey};
use rand::RngCore;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Ed25519 public key (32 bytes), derived from a [`Keypair`] or
/// imported from external sources (DID document, manifest, trust
/// store).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKeyRef(pub [u8; 32]);

impl PublicKeyRef {
    /// Decode a base64-encoded public key.
    pub fn from_base64(s: &str) -> Result<Self> {
        let bytes = B64.decode(s.as_bytes())?;
        Self::from_slice(&bytes)
    }

    /// Wrap a 32-byte slice. Errors if the slice is the wrong length.
    pub fn from_slice(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 32 {
            return Err(Error::InvalidKeyLength {
                expected: 32,
                actual: bytes.len(),
            });
        }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(bytes);
        Ok(Self(buf))
    }

    /// Encode as standard base64.
    pub fn to_base64(&self) -> String {
        B64.encode(self.0)
    }

    /// Verify an Ed25519 signature over `msg`.
    pub fn verify(&self, msg: &[u8], signature: &[u8]) -> Result<()> {
        let vk = VerifyingKey::from_bytes(&self.0)
            .map_err(|e| Error::Signature(format!("verifying key: {e}")))?;
        if signature.len() != 64 {
            return Err(Error::Signature(format!(
                "signature length must be 64 bytes, got {}",
                signature.len()
            )));
        }
        let mut sig_bytes = [0u8; 64];
        sig_bytes.copy_from_slice(signature);
        let sig = Signature::from_bytes(&sig_bytes);
        vk.verify(msg, &sig)
            .map_err(|e| Error::Signature(e.to_string()))
    }

    /// Borrow the raw 32-byte public key.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl Serialize for PublicKeyRef {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        self.to_base64().serialize(ser)
    }
}

impl<'de> Deserialize<'de> for PublicKeyRef {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(de)?;
        Self::from_base64(&s).map_err(serde::de::Error::custom)
    }
}

/// Ed25519 keypair. Holds the secret material — keep this out of
/// logs and never serialize without explicit user intent.
pub struct Keypair {
    signing: SigningKey,
}

impl Keypair {
    /// Generate a fresh keypair from the OS RNG.
    pub fn generate() -> Self {
        let mut seed = [0u8; 32];
        rand::rng().fill_bytes(&mut seed);
        Self {
            signing: SigningKey::from_bytes(&seed),
        }
    }

    /// Load a keypair from an existing 32-byte seed. Errors if the
    /// slice is the wrong length.
    pub fn from_seed(seed: &[u8]) -> Result<Self> {
        if seed.len() != 32 {
            return Err(Error::InvalidKeyLength {
                expected: 32,
                actual: seed.len(),
            });
        }
        let mut buf = [0u8; 32];
        buf.copy_from_slice(seed);
        Ok(Self {
            signing: SigningKey::from_bytes(&buf),
        })
    }

    /// Read the public-key half.
    pub fn public(&self) -> PublicKeyRef {
        PublicKeyRef(self.signing.verifying_key().to_bytes())
    }

    /// Sign `msg`; the returned signature is exactly 64 bytes.
    pub fn sign(&self, msg: &[u8]) -> [u8; 64] {
        use ed25519_dalek::Signer;
        self.signing.sign(msg).to_bytes()
    }

    /// Borrow the 32-byte seed. Caller must keep this confidential.
    pub fn seed_bytes(&self) -> [u8; 32] {
        self.signing.to_bytes()
    }
}

impl std::fmt::Debug for Keypair {
    /// Debug-print the public side only — the seed is intentionally
    /// withheld so accidentally including a `Keypair` in a tracing
    /// span does not leak the secret.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Keypair")
            .field("public", &self.public().to_base64())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keypair_signs_and_verifies() {
        let kp = Keypair::generate();
        let sig = kp.sign(b"hello");
        kp.public().verify(b"hello", &sig).unwrap();
    }

    #[test]
    fn verify_rejects_tampered_message() {
        let kp = Keypair::generate();
        let sig = kp.sign(b"hello");
        let err = kp.public().verify(b"world", &sig).unwrap_err();
        assert!(matches!(err, Error::Signature(_)));
    }

    #[test]
    fn public_key_roundtrips_through_base64() {
        let kp = Keypair::generate();
        let pk = kp.public();
        let s = pk.to_base64();
        let pk2 = PublicKeyRef::from_base64(&s).unwrap();
        assert_eq!(pk, pk2);
    }

    #[test]
    fn invalid_key_length_is_loud() {
        assert!(matches!(
            PublicKeyRef::from_slice(&[0u8; 31]),
            Err(Error::InvalidKeyLength { expected: 32, actual: 31 })
        ));
    }

    #[test]
    fn seed_round_trip_preserves_keypair() {
        let kp = Keypair::generate();
        let seed = kp.seed_bytes();
        let kp2 = Keypair::from_seed(&seed).unwrap();
        assert_eq!(kp.public(), kp2.public());
    }
}
