# Sigstore-public-good trust root

These three files are the production-trusted material for verifying
Sigstore-public-good signed bundles offline. They are vendored as
`include_bytes!()` constants by `tr_sigstore::trust_root::sigstore_public_good`.

## Source

All files were fetched verbatim from
<https://github.com/sigstore/root-signing/tree/main/targets> — the
TUF-managed authoritative source for the Sigstore-public-good
deployment. The `sigstore/root-signing` repo is itself signed by
Sigstore's offline root keys per [The Update Framework](https://theupdateframework.io/).

## Files

### `fulcio_v1.crt.pem`

The Fulcio v1 root CA certificate. Fulcio v1 issues all ECDSA P-256
ephemeral certs the public-good keyless flow produces.

| Field           | Value                                                                                                     |
|-----------------|-----------------------------------------------------------------------------------------------------------|
| Subject         | `CN=sigstore, O=sigstore.dev`                                                                             |
| Issuer (self)   | `CN=sigstore, O=sigstore.dev`                                                                             |
| Validity        | `notBefore=Oct 7 13:56:59 2021 GMT`, `notAfter=Oct 5 13:56:58 2031 GMT`                                  |
| Curve           | NIST P-384 (secp384r1)                                                                                    |
| SHA-256 (DER)   | `3B:A7:B6:CC:4E:95:46:9D:4D:33:4B:49:CB:25:7A:D8:53:70:76:FA:84:B0:CA:87:FF:4E:CF:E6:A5:46:80:C1`           |

The fingerprint is verified by `tests::fulcio_v1_fingerprint_matches_published_value`
inside the trust-root module — if Fulcio rotates the root, the test
fails loudly so the vendored bytes can be re-pulled from
`sigstore/root-signing` deliberately rather than silently.

### `fulcio_intermediate_v1.crt.pem`

The Fulcio v1 intermediate CA. All ephemeral leaves chain through
this intermediate to the root above. Vendoring the intermediate in
addition to the root is belt-and-suspenders: Fulcio always returns
the full chain in its v1 endpoint response, but mounting the
intermediate locally protects against a future server bug that
omitted the intermediate from the response.

| Field           | Value                                                                                                     |
|-----------------|-----------------------------------------------------------------------------------------------------------|
| Subject         | `CN=sigstore-intermediate, O=sigstore.dev`                                                                |
| Issuer          | `CN=sigstore, O=sigstore.dev` (= the v1 root above)                                                       |
| Validity        | `notBefore=Apr 13 20:06:15 2022 GMT`, `notAfter=Oct 5 13:56:58 2031 GMT`                                |
| Curve           | NIST P-384 (secp384r1)                                                                                    |

### `rekor.pub`

The Rekor transparency log's public key. Sigstore-public-good's Rekor
signs SignedEntryTimestamps (SETs) and tree-head checkpoints with this
key. Verifying SETs offline closes the loop on Rekor witness
authenticity without contacting the log.

| Field             | Value                                                                                              |
|-------------------|----------------------------------------------------------------------------------------------------|
| Curve             | NIST P-256 (secp256r1)                                                                             |
| logID             | `c0d23d6ad406973f9559f3ba2d1ca01f84147d8ffc5b8445c224f98b9591801d`                                  |

The logID is `SHA-256(DER(SubjectPublicKeyInfo))` — the same value
Rekor surfaces in `LogEntry.logID` and that the Sigstore-public-good
deployment publishes. `tests::rekor_log_id_matches_published_value`
asserts the match.

## Refresh procedure

When Sigstore rotates the root or rolls a new intermediate:

1. Pull the new file(s) from the `sigstore/root-signing` repo,
   verifying the TUF metadata signatures.
2. Replace the file(s) in this directory.
3. Update the SHA-256 fingerprint in the test that pins the value.
4. Verify all `cargo test -p tr-sigstore` cases still pass against
   bundles signed under the new chain.
5. Commit with a message stating the rotation date and pre/post
   fingerprints.
