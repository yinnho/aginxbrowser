// WebCrypto Ed25519 (RFC 8032). Keys are the raw 32-byte seed / public key;
// the JS shim does the PKCS#8 / SPKI / JWK wrapping and draws seeds from
// `op_random_bytes`.

use super::*;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};

fn seed_of(seed: &[u8]) -> Result<SigningKey, deno_error::JsErrorBox> {
    let seed: [u8; 32] = seed.try_into().map_err(|_| crypto_err("Ed25519 seed must be 32 bytes"))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Public key (32 bytes) of a 32-byte seed.
#[diting_op]
#[buffer]
pub(crate) fn op_subtle_ed25519_public(#[buffer] seed: &[u8]) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    Ok(seed_of(seed)?.verifying_key().to_bytes().to_vec())
}

/// Whether `public` is a valid curve point encoding (raw/SPKI/JWK import).
#[diting_op]
pub(crate) fn op_subtle_ed25519_check(#[buffer] public: &[u8]) -> Result<bool, deno_error::JsErrorBox> {
    Ok(<[u8; 32]>::try_from(public).ok().is_some_and(|b| VerifyingKey::from_bytes(&b).is_ok()))
}

/// 64-byte signature of `data` under the seed.
#[diting_op]
#[buffer]
pub(crate) fn op_subtle_ed25519_sign(
    #[buffer] seed: &[u8],
    #[buffer] data: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    Ok(seed_of(seed)?.sign(data).to_bytes().to_vec())
}

/// Verify; malformed keys or signatures are `false`, not errors.
#[diting_op]
pub(crate) fn op_subtle_ed25519_verify(
    #[buffer] public: &[u8],
    #[buffer] signature: &[u8],
    #[buffer] data: &[u8],
) -> Result<bool, deno_error::JsErrorBox> {
    let Ok(pk) = <[u8; 32]>::try_from(public) else { return Ok(false) };
    let Ok(vk) = VerifyingKey::from_bytes(&pk) else { return Ok(false) };
    let Ok(sig) = ed25519_dalek::Signature::from_slice(signature) else { return Ok(false) };
    Ok(vk.verify(data, &sig).is_ok())
}
