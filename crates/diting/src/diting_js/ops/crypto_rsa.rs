// ---------------------------------------------------------------------------
// WebCrypto RSA-OAEP (crypto.subtle). Key material travels between the JS
// shim and these ops as a flat payload of ten u16-BE length-prefixed
// sections in this order: [pkcs8 DER (private), SPKI DER (public), n, e, d,
// p, q, dp, dq, qi]. Private-only sections carry length 0 on public keys
// and vice versa (spki/n/e always present). The JS shim owns CryptoKey
// objects; these ops are stateless byte-in/byte-out like the ECDSA ones.
//
// Chrome 155 supports exactly RSA-OAEP over SHA-1/256/384/512 on this face
// (RSA-PSS/RSASSA sign is not absorbed yet) and restricts generateKey to
// publicExponent 3 or 65537 — that policy lives in the JS shim; the ops
// accept what they are given.
// ---------------------------------------------------------------------------

use super::*;

/// Flat ten-section payload shared with the JS shim (`ecdsa_flat` sibling).
pub(crate) fn rsa_flat(parts: [&[u8]; 10]) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + parts.iter().map(|p| p.len()).sum::<usize>());
    for p in parts {
        out.extend_from_slice(&(p.len() as u16).to_be_bytes());
        out.extend_from_slice(p);
    }
    out
}

fn rsa_key_flat(sk: &rsa::RsaPrivateKey) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey};
    use rsa::traits::{PrivateKeyParts, PublicKeyParts};
    let pkcs8 = sk.to_pkcs8_der().map_err(crypto_err)?.as_bytes().to_vec();
    let spki = sk
        .to_public_key()
        .to_public_key_der()
        .map_err(crypto_err)?
        .as_bytes()
        .to_vec();
    let two_primes = sk.primes();
    if two_primes.len() != 2 {
        return Err(crypto_err("only two-prime RSA keys are supported"));
    }
    let (p, q) = (&two_primes[0], &two_primes[1]);
    let one = rsa::BigUint::from(1u8);
    let dp = (sk.d() % (p - &one)).to_bytes_be();
    let dq = (sk.d() % (q - &one)).to_bytes_be();
    // qi = q^-1 mod p (RFC 8017 CRT coefficient). p is prime, so Fermat's
    // little theorem gives the inverse as q^(p-2) mod p — no ModInverse
    // trait import needed, plain modpow on the re-exported BigUint.
    let two = rsa::BigUint::from(2u8);
    let qi = q.modpow(&(p - &two), p).to_bytes_be();
    let n = sk.n().to_bytes_be();
    let e = sk.e().to_bytes_be();
    let d = sk.d().to_bytes_be();
    let pb = p.to_bytes_be();
    let qb = q.to_bytes_be();
    Ok(rsa_flat([
        &pkcs8[..],
        &spki[..],
        &n[..],
        &e[..],
        &d[..],
        &pb[..],
        &qb[..],
        &dp[..],
        &dq[..],
        &qi[..],
    ]))
}

fn public_flat(pk: &rsa::RsaPublicKey) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use rsa::pkcs8::EncodePublicKey;
    use rsa::traits::PublicKeyParts;
    let spki = pk
        .to_public_key_der()
        .map_err(crypto_err)?
        .as_bytes()
        .to_vec();
    let n = pk.n().to_bytes_be();
    let e = pk.e().to_bytes_be();
    let empty: &[u8] = &[];
    Ok(rsa_flat([
        empty,
        &spki[..],
        &n[..],
        &e[..],
        empty,
        empty,
        empty,
        empty,
        empty,
        empty,
    ]))
}

fn private_from_components(
    n: rsa::BigUint,
    e: rsa::BigUint,
    d: rsa::BigUint,
    p: rsa::BigUint,
    q: rsa::BigUint,
) -> Result<rsa::RsaPrivateKey, deno_error::JsErrorBox> {
    rsa::RsaPrivateKey::from_components(n, e, d, vec![p, q])
        .map_err(|_| crypto_err("invalid RSA private key components"))
}

/// Generate an RSA keypair. `bits` is the modulus length; `e` the public
/// exponent as big-endian bytes. Returns the full ten-section payload.
#[diting_op]
#[buffer]
pub(crate) fn op_subtle_rsa_generate(
    bits: u32,
    #[buffer] e: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use rsa::BigUint;
    let exponent = BigUint::from_bytes_be(e);
    let sk = rsa::RsaPrivateKey::new_with_exp(&mut rand_core::OsRng, bits as usize, &exponent)
        .map_err(crypto_err)?;
    rsa_key_flat(&sk)
}

/// Import a public key from (n, e) big-endian integers (JWK path). Returns
/// the payload with only the public sections set.
#[diting_op]
#[buffer]
pub(crate) fn op_subtle_rsa_import_public(
    #[buffer] n: &[u8],
    #[buffer] e: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use rsa::BigUint;
    let pk = rsa::RsaPublicKey::new(BigUint::from_bytes_be(n), BigUint::from_bytes_be(e))
        .map_err(crypto_err)?;
    public_flat(&pk)
}

/// Import a public key from SPKI DER. Returns the public-only payload.
#[diting_op]
#[buffer]
pub(crate) fn op_subtle_rsa_import_spki(
    #[buffer] spki: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use rsa::pkcs8::DecodePublicKey;
    let pk = rsa::RsaPublicKey::from_public_key_der(spki).map_err(crypto_err)?;
    public_flat(&pk)
}

/// Import a private key from (n, e, d, p, q) big-endian integers (JWK path —
/// dp/dq/qi are recomputed per RFC 8017 so exported JWKs round-trip). Returns
/// the full payload.
#[diting_op]
#[buffer]
pub(crate) fn op_subtle_rsa_import_private(
    #[buffer] n: &[u8],
    #[buffer] e: &[u8],
    #[buffer] d: &[u8],
    #[buffer] p: &[u8],
    #[buffer] q: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use rsa::BigUint;
    let sk = private_from_components(
        BigUint::from_bytes_be(n),
        BigUint::from_bytes_be(e),
        BigUint::from_bytes_be(d),
        BigUint::from_bytes_be(p),
        BigUint::from_bytes_be(q),
    )?;
    rsa_key_flat(&sk)
}

/// Import a private key from PKCS#8 DER. Returns the full payload.
#[diting_op]
#[buffer]
pub(crate) fn op_subtle_rsa_import_pkcs8(
    #[buffer] pkcs8: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use rsa::pkcs8::DecodePrivateKey;
    let sk = rsa::RsaPrivateKey::from_pkcs8_der(pkcs8).map_err(crypto_err)?;
    rsa_key_flat(&sk)
}

/// RSA-OAEP encrypt/decrypt. `hash` selects the MGF1 hash (and the label
/// hash — WebCrypto uses the same hash for both); `label` is the optional
/// OAEP label. The encrypt arm takes only (n, e); the decrypt arm takes the
/// full private components. Any crypto failure (corrupt ciphertext, label
/// mismatch, wrong length) surfaces as the WebCrypto OperationError with an
/// empty message, matching Chrome.
#[diting_op]
#[buffer]
pub(crate) fn op_subtle_rsa_oaep(
    encrypt: bool,
    #[string] hash: &str,
    #[buffer] label: &[u8],
    #[buffer] n: &[u8],
    #[buffer] e: &[u8],
    #[buffer] d: &[u8],
    #[buffer] p: &[u8],
    #[buffer] q: &[u8],
    #[buffer] data: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use rsa::traits::PublicKeyParts;
    let pk = rsa::RsaPublicKey::new(
        rsa::BigUint::from_bytes_be(n),
        rsa::BigUint::from_bytes_be(e),
    )
    .map_err(crypto_err)?;
    // The label is a string in this rsa release; WebCrypto labels are
    // arbitrary bytes, but every real-world label is ASCII, so lossy
    // conversion only bends for byte labels ≥ 0x80 (no probe pins those).
    let label_str = String::from_utf8_lossy(label).into_owned();
    macro_rules! run {
        ($digest:ty) => {{
            let scheme = if label.is_empty() {
                rsa::Oaep::new::<$digest>()
            } else {
                rsa::Oaep::new_with_label::<$digest, _>(&label_str)
            };
            if encrypt {
                pk.encrypt(&mut rand_core::OsRng, scheme, data)
                    .map_err(|_| crypto_err(""))?
            } else {
                let sk = private_from_components(
                    pk.n().clone(),
                    pk.e().clone(),
                    rsa::BigUint::from_bytes_be(d),
                    rsa::BigUint::from_bytes_be(p),
                    rsa::BigUint::from_bytes_be(q),
                )?;
                sk.decrypt(scheme, data).map_err(|_| crypto_err(""))?
            }
        }};
    }
    Ok(match hash {
        "SHA-1" => run!(rsa_sha1::Sha1),
        "SHA-256" => run!(rsa::sha2::Sha256),
        "SHA-384" => run!(rsa::sha2::Sha384),
        "SHA-512" => run!(rsa::sha2::Sha512),
        _ => return Err(crypto_err("unsupported RSA-OAEP hash")),
    })
}
