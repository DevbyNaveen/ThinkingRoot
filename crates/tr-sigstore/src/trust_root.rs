//! Trust-root model + X.509 cert-chain validation for Sigstore-style
//! bundles.
//!
//! A [`TrustedRoot`] is the set of root CAs whose intermediates are
//! authorised to issue ephemeral signing certs for v3 packs. The
//! Sigstore Public Good Instance ships its trust root via TUF; the
//! parsed JSON expands into a list of root CA certificates plus per-CA
//! validity windows. For private-Sigstore deployments and tests, this
//! module also accepts caller-supplied root CA certs directly (DER or
//! PEM).
//!
//! Vendoring the live Sigstore-public-good trust-root JSON is a
//! separate concern from this code: a follow-up commit will drop a
//! frozen snapshot under `crates/tr-sigstore/src/trusted_roots/` and
//! expose `TrustedRoot::sigstore_public_good()`. The chain-validation
//! algorithm is identical regardless of where the roots come from, so
//! this module is the right place to lock the validation rule before
//! the trust-root content question is settled.

use std::time::SystemTime;

use ::der::Decode as _;
use x509_cert::Certificate as X509Cert;

use crate::Error;

/// Root-CA bundle a verifier trusts to issue ephemeral signing certs.
///
/// Construct via [`TrustedRoot::from_root_pems`] or
/// [`TrustedRoot::from_root_ders`]. `verify_cert_chain` walks a bundle's
/// `x509CertificateChain` toward one of the roots in this set.
///
/// `TrustedRoot` is intentionally append-only at the public API level:
/// the Sigstore project has rotated CAs over time, and a verifier that
/// supports older bundles must keep historical roots in its trust set.
/// Add new roots by parsing their PEM bytes; remove a root only when
/// you're sure no extant bundle still chains to it.
#[derive(Debug, Clone)]
pub struct TrustedRoot {
    /// Root CAs whose direct or transitive children (intermediates)
    /// may sign end-entity certs for v3 packs.
    fulcio_roots: Vec<TrustedCertificate>,
}

/// One trusted root certificate, parsed once at trust-root construction
/// so chain verification doesn't re-parse on every bundle.
#[derive(Debug, Clone)]
pub struct TrustedCertificate {
    /// DER-encoded X.509 certificate. Re-emitted by `to_der()` for
    /// round-trip and by [`Self::der_bytes`] for callers that want the
    /// raw bytes.
    der: Vec<u8>,
    /// Pre-parsed cert. Held alongside `der` so chain-verification
    /// hot path doesn't pay decode cost per bundle.
    parsed: X509Cert,
}

impl TrustedRoot {
    /// Construct a trust root from concatenated PEM-encoded root CA
    /// certs. Multiple `-----BEGIN CERTIFICATE-----` blocks in a
    /// single string are accepted; entries that don't parse as DER
    /// X.509 surface as [`Error::CertParse`].
    pub fn from_root_pems(pem: &str) -> Result<Self, Error> {
        let mut roots = Vec::new();
        for entry in iter_pem_blocks(pem) {
            let der = entry?;
            let parsed = X509Cert::from_der(&der)
                .map_err(|e| Error::CertParse(format!("trust root DER decode: {e}")))?;
            roots.push(TrustedCertificate { der, parsed });
        }
        if roots.is_empty() {
            return Err(Error::CertParse(
                "no PEM CERTIFICATE blocks found in trust root".into(),
            ));
        }
        Ok(Self {
            fulcio_roots: roots,
        })
    }

    /// Construct a trust root from a slice of raw DER-encoded root CA
    /// certs. Each entry is parsed once; downstream chain verification
    /// reuses the cached parse.
    pub fn from_root_ders(ders: &[&[u8]]) -> Result<Self, Error> {
        let mut roots = Vec::with_capacity(ders.len());
        for (i, der) in ders.iter().enumerate() {
            let parsed = X509Cert::from_der(der)
                .map_err(|e| Error::CertParse(format!("trust root #{i} DER decode: {e}")))?;
            roots.push(TrustedCertificate {
                der: der.to_vec(),
                parsed,
            });
        }
        if roots.is_empty() {
            return Err(Error::CertParse(
                "from_root_ders: empty input slice".into(),
            ));
        }
        Ok(Self {
            fulcio_roots: roots,
        })
    }

    /// Number of root CAs in this trust root. Useful for diagnostics.
    pub fn root_count(&self) -> usize {
        self.fulcio_roots.len()
    }

    /// Iterate over the parsed root certs. Used by `verify_cert_chain`
    /// to find the issuer of the topmost intermediate.
    pub(crate) fn roots(&self) -> impl Iterator<Item = &TrustedCertificate> {
        self.fulcio_roots.iter()
    }
}

impl TrustedCertificate {
    /// Raw DER bytes of this root cert.
    pub fn der_bytes(&self) -> &[u8] {
        &self.der
    }

    /// Parsed cert, for callers that want to inspect subject / SPKI
    /// without re-parsing.
    pub fn parsed(&self) -> &X509Cert {
        &self.parsed
    }
}

/// Walk a base64-decoded list of cert PEM blocks and surface DER bytes
/// per block. Skips non-CERTIFICATE blocks (some PEM bundles include
/// metadata blocks like CERTIFICATE REQUEST that aren't cert-of-trust).
fn iter_pem_blocks(input: &str) -> impl Iterator<Item = Result<Vec<u8>, Error>> + '_ {
    PemBlockIter {
        rest: input,
        done: false,
    }
}

struct PemBlockIter<'a> {
    rest: &'a str,
    done: bool,
}

impl<'a> Iterator for PemBlockIter<'a> {
    type Item = Result<Vec<u8>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        // Find the next BEGIN block. We accept arbitrary types but
        // only emit DER for "CERTIFICATE" blocks; metadata-only blocks
        // get silently skipped.
        let begin_marker = "-----BEGIN ";
        let end_marker = "-----END ";

        loop {
            let begin_idx = match self.rest.find(begin_marker) {
                Some(i) => i,
                None => {
                    self.done = true;
                    return None;
                }
            };
            let header_start = begin_idx + begin_marker.len();
            let header_end = match self.rest[header_start..].find("-----") {
                Some(o) => header_start + o,
                None => {
                    self.done = true;
                    return Some(Err(Error::CertParse(
                        "malformed PEM: BEGIN block missing closing dashes".into(),
                    )));
                }
            };
            let block_type = &self.rest[header_start..header_end];

            // Move past the header (including the trailing 5 dashes).
            let body_start = header_end + 5;
            let footer_idx = match self.rest[body_start..].find(end_marker) {
                Some(o) => body_start + o,
                None => {
                    self.done = true;
                    return Some(Err(Error::CertParse(
                        "malformed PEM: missing END marker".into(),
                    )));
                }
            };
            let body = &self.rest[body_start..footer_idx];

            // Advance `rest` past this whole block (including the END
            // marker and the type tag after it).
            let after_footer_search = &self.rest[footer_idx..];
            let after_footer = match after_footer_search.find("-----\n") {
                Some(o) => footer_idx + o + "-----\n".len(),
                None => match after_footer_search.find("-----") {
                    Some(o) => footer_idx + o + "-----".len(),
                    None => after_footer_search.len(),
                },
            };
            self.rest = &self.rest[after_footer..];

            if block_type != "CERTIFICATE" {
                // Non-certificate block (e.g., a comment or RSA PRIVATE
                // KEY); ignore and look for the next one.
                continue;
            }

            // Decode base64 body (with whitespace stripped).
            let cleaned: String = body
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            use base64::Engine as _;
            let der = match base64::engine::general_purpose::STANDARD.decode(cleaned) {
                Ok(d) => d,
                Err(e) => {
                    return Some(Err(Error::CertParse(format!(
                        "PEM body base64 decode: {e}"
                    ))));
                }
            };
            return Some(Ok(der));
        }
    }
}

// ─────────────────────────────────────────────────────────────────
// Chain validation
// ─────────────────────────────────────────────────────────────────

/// Validate that the given cert chain links cleanly to one of the
/// roots in `trust_root`, with all certs valid at `signed_at`.
///
/// The chain is `[leaf, intermediate1, intermediate2, ...]` — the
/// Sigstore Bundle convention. Each cert C[i] must be signed by either:
///
/// 1. C[i+1]'s public key, **or**
/// 2. (only for the topmost intermediate or a directly-rooted leaf) a
///    root CA in `trust_root`.
///
/// Validity windows are checked: each cert's `tbs_certificate.validity`
/// must include `signed_at`. (The standard Sigstore-public-good Fulcio
/// leaf has a validity window of about 10 minutes; the time check is
/// the load-bearing distinction between "the cert was valid when the
/// pack was signed" and "the cert has since expired but the pack is
/// still trustworthy".)
///
/// On success, returns the index in `trust_root` of the root CA that
/// terminated the chain — useful for callers that want to log which
/// CA witnessed the signing.
///
/// **Not yet checked** (deliberate scope cap for this commit; tracked
/// as follow-ups):
/// - Sigstore-specific OID extensions (issuer claim, GitHub Actions
///   workflow ref, etc.) — Task #53b.
/// - CRL or OCSP checks — Sigstore packs use Rekor inclusion proofs
///   for tlog-based revocation; CRL is out of scope for v3.0.
/// - SCT (Certificate Transparency) — Fulcio-issued certs carry an
///   SCT extension; Sigstore verification typically validates it but
///   it isn't strictly required for chain-of-trust. Future commit.
pub fn verify_cert_chain(
    chain: &crate::X509CertificateChain,
    trust_root: &TrustedRoot,
    signed_at: SystemTime,
) -> Result<usize, Error> {
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;

    if chain.certificates.is_empty() {
        return Err(Error::EmptyCertChain);
    }

    // Decode + parse every chain cert once.
    let mut chain_certs: Vec<X509Cert> = Vec::with_capacity(chain.certificates.len());
    for (i, c) in chain.certificates.iter().enumerate() {
        let der = b64
            .decode(&c.raw_bytes)
            .map_err(Error::Base64)?;
        let parsed = X509Cert::from_der(&der)
            .map_err(|e| Error::CertParse(format!("chain[{i}] DER decode: {e}")))?;
        chain_certs.push(parsed);
    }

    // 1. All chain certs must be valid at signed_at.
    for (i, cert) in chain_certs.iter().enumerate() {
        check_cert_validity(cert, signed_at)
            .map_err(|e| Error::CertValidity(format!("chain[{i}]: {e}")))?;
    }

    // 2. Walk the chain bottom-up. Each cert is signed by the next one
    //    (or, for the topmost, by a trust-root CA).
    let mut root_index: Option<usize> = None;
    for i in 0..chain_certs.len() {
        let cert = &chain_certs[i];
        if i + 1 < chain_certs.len() {
            // Issuer is the next cert in the chain.
            let issuer_cert = &chain_certs[i + 1];
            verify_cert_signature_by_cert(cert, issuer_cert)?;
        } else {
            // Topmost chain cert: must be signed by a root CA.
            let mut found = None;
            for (idx, root) in trust_root.roots().enumerate() {
                check_cert_validity(&root.parsed, signed_at)
                    .map_err(|e| Error::CertValidity(format!("trust_root[{idx}]: {e}")))?;
                if verify_cert_signature_by_cert(cert, &root.parsed).is_ok() {
                    found = Some(idx);
                    break;
                }
            }
            root_index = Some(
                found.ok_or_else(|| Error::ChainDoesNotReachTrustRoot(format!(
                    "topmost chain cert (index {i}) is not signed by any of the {} trusted root(s)",
                    trust_root.root_count()
                )))?,
            );
        }
    }
    Ok(root_index.expect("chain non-empty -> root_index set in loop"))
}

/// Verify `child` was signed by `issuer`'s public key. Both the child's
/// `signatureAlgorithm` and the issuer's SPKI must declare ECDSA P-256
/// (ecdsa-with-SHA256); Sigstore's CA chain is exclusively that
/// algorithm. Other algorithms surface as
/// [`Error::UnsupportedKeyAlgorithm`] so callers see a clean
/// "unsupported" verdict rather than a generic signature failure.
fn verify_cert_signature_by_cert(
    child: &X509Cert,
    issuer: &X509Cert,
) -> Result<(), Error> {
    use signature::Verifier as _;
    use x509_cert::der::Encode as _;

    // Sigstore's CA chain uses ecdsa-with-SHA256 exclusively. OID is
    // 1.2.840.10045.4.3.2.
    let ecdsa_with_sha256 =
        ::der::asn1::ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2");
    if child.signature_algorithm.oid != ecdsa_with_sha256 {
        return Err(Error::UnsupportedKeyAlgorithm(format!(
            "issuer signature algorithm {:?} unsupported (expected ecdsa-with-SHA256)",
            child.signature_algorithm.oid
        )));
    }

    // Re-encode the TBS portion to DER — that's the bytes the issuer
    // signed. x509-cert returns this losslessly.
    let tbs_der = child
        .tbs_certificate
        .to_der()
        .map_err(|e| Error::CertParse(format!("re-encode TBS for verification: {e}")))?;

    // Parse the issuer signature as DER ECDSA. CertSignBitstring is
    // always byte-aligned so `as_bytes()` returns the bytes directly.
    let sig_bytes = child
        .signature
        .as_bytes()
        .ok_or_else(|| Error::CertParse("issuer signature not byte-aligned".into()))?;
    // Cert issuer signatures are conventionally DER-encoded. Fall back
    // to raw 64-byte form for the rare implementation that emits raw
    // P1363 — same tolerance the DSSE-side parser applies.
    let signature = match p256::ecdsa::Signature::from_der(sig_bytes) {
        Ok(s) => s,
        Err(_) if sig_bytes.len() == 64 => p256::ecdsa::Signature::from_slice(sig_bytes)
            .map_err(|_| Error::EcdsaSignatureFormat)?,
        Err(_) => return Err(Error::EcdsaSignatureFormat),
    };

    // Recover issuer's P-256 verifying key from SPKI.
    let issuer_pk = recover_p256_verifying_key(&issuer.tbs_certificate.subject_public_key_info)?;

    issuer_pk
        .verify(&tbs_der, &signature)
        .map_err(|_| Error::SignatureMismatch)
}

/// Recover a `p256::ecdsa::VerifyingKey` from a parsed SPKI. Surfaces a
/// clean unsupported-algorithm error for non-P-256 keys (RSA, P-384,
/// Ed25519-in-cert, etc.) so callers can map to a distinct verdict.
fn recover_p256_verifying_key(
    spki: &spki::SubjectPublicKeyInfoOwned,
) -> Result<p256::ecdsa::VerifyingKey, Error> {
    use ::der::Encode as _;
    use p256::pkcs8::DecodePublicKey as _;
    let spki_der = spki
        .to_der()
        .map_err(|e| Error::CertParse(format!("re-encode issuer SPKI: {e}")))?;
    p256::ecdsa::VerifyingKey::from_public_key_der(&spki_der)
        .map_err(|e| Error::UnsupportedKeyAlgorithm(format!("issuer P-256 SPKI: {e}")))
}

/// Check that `signed_at` falls within `cert.tbs_certificate.validity`.
/// Sigstore-public-good's leaf certs have ~10-minute validity windows;
/// a 5-minute clock skew is the standard tolerance.
fn check_cert_validity(cert: &X509Cert, signed_at: SystemTime) -> Result<(), String> {
    let validity = &cert.tbs_certificate.validity;
    let not_before = validity.not_before.to_system_time();
    let not_after = validity.not_after.to_system_time();

    const SKEW: std::time::Duration = std::time::Duration::from_secs(5 * 60);

    if signed_at + SKEW < not_before {
        return Err(format!(
            "signed_at is before cert's notBefore ({not_before:?})"
        ));
    }
    if not_after + SKEW < signed_at {
        return Err(format!(
            "signed_at is after cert's notAfter ({not_after:?})"
        ));
    }
    Ok(())
}

/// Test-only X.509 cert construction helpers — shared between this
/// module's unit tests and the lib-level integration tests in
/// `lib.rs::tests` that exercise full bundle-with-trust-root
/// verification.
#[cfg(test)]
pub(crate) mod test_helpers {
    use ::der::Encode as _;
    use ::der::asn1::BitString;
    pub use p256::ecdsa::{
        DerSignature as P256DerSignature, SigningKey as P256SigningKey,
    };
    use signature::Signer as _;
    use spki::{EncodePublicKey as _, SubjectPublicKeyInfoOwned};
    use std::str::FromStr;
    pub use x509_cert::Certificate as X509Cert;
    use x509_cert::TbsCertificate;
    use x509_cert::Version;
    pub use x509_cert::name::Name;
    use x509_cert::serial_number::SerialNumber;
    use x509_cert::spki::AlgorithmIdentifierOwned;
    use x509_cert::time::Validity;

    pub fn p256_key(seed: u8) -> P256SigningKey {
        let mut bytes = [0u8; 32];
        bytes[31] = seed.max(1);
        P256SigningKey::from_slice(&bytes).unwrap()
    }

    fn spki_from(vk: &p256::ecdsa::VerifyingKey) -> SubjectPublicKeyInfoOwned {
        let pk = p256::PublicKey::from(vk);
        let der = pk.to_public_key_der().unwrap();
        SubjectPublicKeyInfoOwned::try_from(der.as_bytes()).unwrap()
    }

    /// Hand-roll a syntactically valid X.509 cert that's signed by
    /// `issuer_signer` (with whatever name the caller specifies as
    /// `issuer_name`). For a self-signed root, pass the same key as
    /// both subject and issuer.
    ///
    /// Bypasses x509-cert's `Builder` trait because the trait bound
    /// `VerifyingKey<NistP256>: EncodePublicKey` doesn't materialize
    /// in our exact ecdsa+pkcs8 feature config; rolling it ourselves
    /// is shorter than untangling the unification quirk and gives
    /// these tests precise control over the cert's TBS bytes (which
    /// matters for the chain-validation tests below).
    pub fn build_signed_cert(
        subject_name: &Name,
        subject_spki: SubjectPublicKeyInfoOwned,
        issuer_name: &Name,
        issuer_signer: &P256SigningKey,
        serial: u32,
        validity_seconds: u64,
    ) -> X509Cert {
        // ecdsa-with-SHA256: 1.2.840.10045.4.3.2.
        let ecdsa_with_sha256 =
            ::der::asn1::ObjectIdentifier::new_unwrap("1.2.840.10045.4.3.2");
        let sig_alg = AlgorithmIdentifierOwned {
            oid: ecdsa_with_sha256,
            parameters: None,
        };

        let tbs = TbsCertificate {
            version: Version::V3,
            serial_number: SerialNumber::from(serial),
            signature: sig_alg.clone(),
            issuer: issuer_name.clone(),
            validity: Validity::from_now(std::time::Duration::from_secs(
                validity_seconds,
            ))
            .unwrap(),
            subject: subject_name.clone(),
            subject_public_key_info: subject_spki,
            issuer_unique_id: None,
            subject_unique_id: None,
            extensions: None,
        };

        // Encode TBS to DER and sign it. Use the DER-form ECDSA
        // signature variant (matches what Sigstore + cosign emit when
        // they sign cert TBS bytes).
        let tbs_der = tbs.to_der().expect("encode TBS");
        let signature: P256DerSignature = issuer_signer.sign(&tbs_der);
        let sig_bytes = signature.to_bytes();

        X509Cert {
            tbs_certificate: tbs,
            signature_algorithm: sig_alg,
            signature: BitString::from_bytes(sig_bytes.as_ref())
                .expect("sig bitstring"),
        }
    }

    pub fn build_self_signed_root(
        signer: &P256SigningKey,
        cn: &str,
    ) -> (X509Cert, Name) {
        let name = Name::from_str(&format!("CN={cn}")).unwrap();
        let spki = spki_from(signer.verifying_key());
        let cert = build_signed_cert(&name, spki, &name, signer, 1, 86_400);
        (cert, name)
    }

    pub fn build_intermediate(
        issuer_signer: &P256SigningKey,
        issuer_name: &Name,
        int_signer: &P256SigningKey,
        cn: &str,
    ) -> (X509Cert, Name) {
        let int_name = Name::from_str(&format!("CN={cn}")).unwrap();
        let int_spki = spki_from(int_signer.verifying_key());
        let cert = build_signed_cert(
            &int_name,
            int_spki,
            issuer_name,
            issuer_signer,
            2,
            86_400,
        );
        (cert, int_name)
    }

    pub fn build_leaf(
        issuer_signer: &P256SigningKey,
        issuer_name: &Name,
        leaf_signer: &P256SigningKey,
        cn: &str,
    ) -> X509Cert {
        let subject = Name::from_str(&format!("CN={cn}")).unwrap();
        let leaf_spki = spki_from(leaf_signer.verifying_key());
        build_signed_cert(
            &subject,
            leaf_spki,
            issuer_name,
            issuer_signer,
            3,
            86_400,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::*;
    use super::{Error, TrustedRoot, verify_cert_chain};
    use std::time::SystemTime;
    use x509_cert::der::Encode as _;

    fn cert_to_chain_entry(cert: &X509Cert) -> crate::X509Certificate {
        use base64::Engine as _;
        let der = cert.to_der().unwrap();
        crate::X509Certificate {
            raw_bytes: base64::engine::general_purpose::STANDARD.encode(&der),
        }
    }

    #[test]
    fn synthetic_chain_root_intermediate_leaf_validates() {
        let root_key = p256_key(0x10);
        let int_key = p256_key(0x20);
        let leaf_key = p256_key(0x30);

        let (root_cert, root_name) =
            build_self_signed_root(&root_key, "Test Sigstore Root");
        let (int_cert, int_name) = build_intermediate(
            &root_key,
            &root_name,
            &int_key,
            "Test Fulcio Intermediate",
        );
        let leaf_cert =
            build_leaf(&int_key, &int_name, &leaf_key, "Test Subject");

        let chain = crate::X509CertificateChain {
            certificates: vec![
                cert_to_chain_entry(&leaf_cert),
                cert_to_chain_entry(&int_cert),
            ],
        };

        let trust_root =
            TrustedRoot::from_root_ders(&[&root_cert.to_der().unwrap()]).unwrap();

        let idx = verify_cert_chain(&chain, &trust_root, SystemTime::now()).unwrap();
        assert_eq!(idx, 0, "expected chain to terminate at the only root");
    }

    #[test]
    fn chain_with_unrelated_root_in_trust_root_fails() {
        // Build a real chain rooted at root_a, but supply a trust root
        // that only knows root_b. Chain validation must fail.
        let root_a = p256_key(0x40);
        let root_b = p256_key(0x41);
        let int_key = p256_key(0x42);
        let leaf_key = p256_key(0x43);

        let (_root_a_cert, root_a_name) =
            build_self_signed_root(&root_a, "Real Root");
        let (int_cert, int_name) =
            build_intermediate(&root_a, &root_a_name, &int_key, "Intermediate");
        let leaf_cert = build_leaf(&int_key, &int_name, &leaf_key, "Leaf");

        let chain = crate::X509CertificateChain {
            certificates: vec![
                cert_to_chain_entry(&leaf_cert),
                cert_to_chain_entry(&int_cert),
            ],
        };

        // Trust root only knows the unrelated root_b.
        let (root_b_cert, _) =
            build_self_signed_root(&root_b, "Unrelated Root");
        let trust_root =
            TrustedRoot::from_root_ders(&[&root_b_cert.to_der().unwrap()]).unwrap();

        let err = verify_cert_chain(&chain, &trust_root, SystemTime::now()).unwrap_err();
        assert!(
            matches!(err, Error::ChainDoesNotReachTrustRoot(_)),
            "expected ChainDoesNotReachTrustRoot, got {err:?}"
        );
    }

    #[test]
    fn from_root_pems_parses_concatenated_certs() {
        // Build two roots, encode each as PEM-CERTIFICATE, concatenate.
        let root_a = p256_key(0x50);
        let root_b = p256_key(0x51);
        let (cert_a, _) = build_self_signed_root(&root_a, "Root A");
        let (cert_b, _) = build_self_signed_root(&root_b, "Root B");

        use base64::Engine as _;
        let to_pem = |cert: &X509Cert| -> String {
            let der = cert.to_der().unwrap();
            let b64 = base64::engine::general_purpose::STANDARD.encode(&der);
            // Wrap at 64 chars per RFC 7468.
            let wrapped: String = b64
                .as_bytes()
                .chunks(64)
                .map(std::str::from_utf8)
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap()
                .join("\n");
            format!("-----BEGIN CERTIFICATE-----\n{wrapped}\n-----END CERTIFICATE-----\n")
        };

        let pem = format!("{}{}", to_pem(&cert_a), to_pem(&cert_b));
        let trust_root = TrustedRoot::from_root_pems(&pem).unwrap();
        assert_eq!(trust_root.root_count(), 2);
    }

    #[test]
    fn from_root_pems_rejects_empty_input() {
        let err = TrustedRoot::from_root_pems("").unwrap_err();
        assert!(matches!(err, Error::CertParse(_)));
    }

    #[test]
    fn from_root_pems_skips_non_certificate_blocks() {
        // PEM stream with one CERTIFICATE block and one PRIVATE-KEY
        // block — the parser must keep the cert and silently skip the
        // key block (so a real-world Fulcio deploy shipping its CA
        // bundle alongside other PEM artifacts works without surgery).
        let root = p256_key(0x60);
        let (cert, _) = build_self_signed_root(&root, "Solo Root");

        use base64::Engine as _;
        let der = cert.to_der().unwrap();
        let body = base64::engine::general_purpose::STANDARD.encode(&der);
        let wrapped: String = body
            .as_bytes()
            .chunks(64)
            .map(std::str::from_utf8)
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap()
            .join("\n");
        let pem = format!(
            "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----\n\
             -----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
            base64::engine::general_purpose::STANDARD.encode(b"not a real key"),
            wrapped,
        );
        let trust_root = TrustedRoot::from_root_pems(&pem).unwrap();
        assert_eq!(trust_root.root_count(), 1);
    }
}
