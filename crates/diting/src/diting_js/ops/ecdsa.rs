// ---------------------------------------------------------------------------
// WebCrypto ECDSA (P-256 / P-384) and ECDH. The key material travels between
// the JS shim and these ops as a flat payload of four u16-BE length-prefixed
// sections in this order: [pkcs8 DER (private), SEC1 uncompressed point
// (public), scalar bytes (private), SPKI DER (public)]. Absent sections carry
// length 0. Signatures use the WebCrypto P1363 raw form (r||s, no DER
// wrapper) — that is what browsers emit and accept.
// ---------------------------------------------------------------------------

use super::*;

pub(crate) fn ecdsa_flat(parts: [&[u8]; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + parts.iter().map(|p| p.len()).sum::<usize>());
    for p in parts {
        out.extend_from_slice(&(p.len() as u16).to_be_bytes());
        out.extend_from_slice(p);
    }
    out
}

pub(crate) fn ecdsa_digest(hash: &str, data: &[u8]) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    use sha1::Digest as _;
    Ok(match hash {
        "SHA-1" => sha1::Sha1::digest(data).to_vec(),
        "SHA-256" => sha2::Sha256::digest(data).to_vec(),
        "SHA-384" => sha2::Sha384::digest(data).to_vec(),
        "SHA-512" => sha2::Sha512::digest(data).to_vec(),
        _ => return Err(crypto_err("unsupported ECDSA hash")),
    })
}

// Expand the body once per supported curve; each arm binds `ec` to the curve
// crate so the body text is uniform.
macro_rules! ecdsa_curves {
    ($curve:expr, { $($body:tt)* }) => {
        match $curve {
            "P-256" => {
                use p256 as ec;
                $($body)*
            }
            "P-384" => {
                use p384 as ec;
                $($body)*
            }
            _ => return Err(crypto_err(format!("unsupported named curve {}", $curve))),
        }
    };
}

macro_rules! ecdsa_key_flat {
    ($sk:expr) => {{
        let sk = $sk;
        let pkcs8 = sk
            .to_pkcs8_der()
            .map_err(crypto_err)?
            .as_bytes()
            .to_vec();
        let vk = sk.verifying_key();
        let sec1 = vk.to_encoded_point(false).as_bytes().to_vec();
        let spki = vk
            .to_public_key_der()
            .map_err(crypto_err)?
            .as_bytes()
            .to_vec();
        let scalar = sk.to_bytes().to_vec();
        ecdsa_flat([&pkcs8[..], &sec1[..], &scalar[..], &spki[..]])
    }};
}

/// Generate an ECDSA keypair. Returns the flat key payload (all four sections
/// populated — the public and private CryptoKey views in the JS shim share it).
#[op2]
#[buffer]
pub(crate) fn op_subtle_ecdsa_generate(
    #[string] curve: &str,
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    ecdsa_curves!(curve, {
        use ec::ecdsa::SigningKey;
        use ec::elliptic_curve::pkcs8::{EncodePrivateKey, EncodePublicKey};
        let mut rng = rand_core::OsRng;
        let sk = SigningKey::random(&mut rng);
        Ok(ecdsa_key_flat!(sk))
    })
}

/// Import a private ECDSA key from either PKCS#8 DER (`is_scalar` false) or a
/// raw big-endian scalar (`is_scalar` true, 32/48 bytes). Returns the full
/// flat payload (public parts derived from the private key).
#[op2]
#[buffer]
pub(crate) fn op_subtle_ecdsa_import_private(
    #[string] curve: &str,
    is_scalar: bool,
    #[buffer] material: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    ecdsa_curves!(curve, {
        use ec::ecdsa::SigningKey;
        use ec::elliptic_curve::pkcs8::{DecodePrivateKey, EncodePrivateKey, EncodePublicKey};
        let sk = if is_scalar {
            SigningKey::from_slice(material).map_err(crypto_err)?
        } else {
            SigningKey::from_pkcs8_der(material).map_err(crypto_err)?
        };
        Ok(ecdsa_key_flat!(sk))
    })
}

/// Import a public ECDSA key from either SPKI DER or a raw uncompressed SEC1
/// point. Returns the flat payload with only the public sections set.
#[op2]
#[buffer]
pub(crate) fn op_subtle_ecdsa_import_public(
    #[string] curve: &str,
    #[buffer] material: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    ecdsa_curves!(curve, {
        use ec::ecdsa::VerifyingKey;
        use ec::elliptic_curve::pkcs8::{DecodePublicKey, EncodePublicKey};
        let vk = if material.first() == Some(&0x30) {
            VerifyingKey::from_public_key_der(material).map_err(crypto_err)?
        } else {
            VerifyingKey::from_sec1_bytes(material).map_err(crypto_err)?
        };
        let sec1 = vk.to_encoded_point(false).as_bytes().to_vec();
        let spki = vk
            .to_public_key_der()
            .map_err(crypto_err)?
            .as_bytes()
            .to_vec();
        Ok(ecdsa_flat([&[] as &[u8], &sec1[..], &[] as &[u8], &spki[..]]))
    })
}

/// ECDSA sign. `pkcs8` is the private key's PKCS#8 DER; output is the P1363
/// raw signature (64 bytes for P-256, 96 for P-384).
#[op2]
#[buffer]
pub(crate) fn op_subtle_ecdsa_sign(
    #[string] curve: &str,
    #[string] hash: &str,
    #[buffer] pkcs8: &[u8],
    #[buffer] data: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    let prehash = ecdsa_digest(hash, data)?;
    ecdsa_curves!(curve, {
        use ec::ecdsa::{Signature, SigningKey};
        use ec::elliptic_curve::pkcs8::DecodePrivateKey;
        use ::ecdsa::signature::hazmat::PrehashSigner;
        let sk = SigningKey::from_pkcs8_der(pkcs8).map_err(crypto_err)?;
        let sig: Signature = sk.sign_prehash(&prehash).map_err(crypto_err)?;
        Ok(sig.to_bytes().to_vec())
    })
}

/// ECDSA verify. `sec1` is the public key's uncompressed SEC1 point,
/// `signature` a P1363 raw signature. Malformed keys or signatures return
/// `false` (the WebCrypto behavior), not an error.
#[op2(fast)]
pub(crate) fn op_subtle_ecdsa_verify(
    #[string] curve: &str,
    #[string] hash: &str,
    #[buffer] sec1: &[u8],
    #[buffer] signature: &[u8],
    #[buffer] data: &[u8],
) -> Result<bool, deno_error::JsErrorBox> {
    let prehash = ecdsa_digest(hash, data)?;
    ecdsa_curves!(curve, {
        use ec::ecdsa::{Signature, VerifyingKey};
        use ::ecdsa::signature::hazmat::PrehashVerifier;
        let vk = match VerifyingKey::from_sec1_bytes(sec1) {
            Ok(vk) => vk,
            Err(_) => return Ok(false),
        };
        let sig = match Signature::from_slice(signature) {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };
        Ok(vk.verify_prehash(&prehash, &sig).is_ok())
    })
}

/// ECDH shared secret (raw x-coordinate, 32/48 bytes). `scalar` is the
/// private key's raw big-endian scalar, `peer_sec1` the peer public key's
/// uncompressed SEC1 point — the two halves the JS shim already carries in
/// every EC key's material.
#[op2]
#[buffer]
pub(crate) fn op_subtle_ecdh_derive_bits(
    #[string] curve: &str,
    #[buffer] scalar: &[u8],
    #[buffer] peer_sec1: &[u8],
) -> Result<Vec<u8>, deno_error::JsErrorBox> {
    ecdsa_curves!(curve, {
        use ec::elliptic_curve::{ecdh, NonZeroScalar};
        let d = NonZeroScalar::from(&ec::SecretKey::from_slice(scalar).map_err(crypto_err)?);
        let peer = ec::PublicKey::from_sec1_bytes(peer_sec1).map_err(crypto_err)?;
        let shared = ecdh::diffie_hellman(d, peer.as_affine());
        Ok(shared.raw_secret_bytes().as_slice().to_vec())
    })
}
