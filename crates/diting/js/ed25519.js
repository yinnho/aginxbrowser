// crypto.subtle Ed25519 (WebCrypto Secure Curves). Wraps the subtle methods
// crypto_subtle.js installed: Ed25519 calls are served here, everything else
// falls through to the original. Rust ops do the curve math; the fixed PKCS#8
// / SPKI DER prefixes and the OKP JWK shape are handled below.
if (globalThis.__diting_subtle_hooks && globalThis.crypto && globalThis.crypto.subtle) {
  const { makeKey, keyBytes, toBytes, bufferOf, b64urlToBytes, bytesToB64url, runOp } = globalThis.__diting_subtle_hooks;
  delete globalThis.__diting_subtle_hooks;
  const subtle = globalThis.crypto.subtle;
  const orig = {};
  for (const m of ["generateKey", "importKey", "exportKey", "sign", "verify"]) orig[m] = subtle[m].bind(subtle);

  const ALG = { name: "Ed25519" };
  const PKCS8 = Uint8Array.from([0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04, 0x20]);
  const SPKI = Uint8Array.from([0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00]);
  const isEd = (a) => String(typeof a === "string" ? a : (a && a.name) || "").toLowerCase() === "ed25519";
  const keyIsEd = (k) => !!(k && k.algorithm && k.algorithm.name === "Ed25519");
  const err = (msg, name) => new DOMException(msg, name);
  const cat = (a, b) => { const o = new Uint8Array(a.length + b.length); o.set(a); o.set(b, a.length); return o; };
  const startsWith = (b, p) => b.length === p.length + 32 && p.every((v, i) => b[i] === v);
  const same = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);

  // usages must be a non-empty subset of the key half's allowed set.
  function checkUsages(usages, allowed, needOne) {
    usages = usages || [];
    if (usages.some((u) => !allowed.includes(u)) || (needOne && usages.length === 0)) {
      throw err("Unsupported key usage for an Ed25519 key", "SyntaxError");
    }
    return usages;
  }
  const pubKey = (pub, ext, usages) => makeKey("public", ext, ALG, usages, { seed: null, pub });
  const privKey = (seed, pub, ext, usages) => makeKey("private", ext, ALG, usages, { seed, pub });
  const publicOf = (seed) => new Uint8Array(runOp(() => _OPS.op_subtle_ed25519_public(seed)));
  const validPoint = (pub) => {
    if (!_OPS.op_subtle_ed25519_check(pub)) throw err("Invalid Ed25519 public key", "DataError");
  };

  subtle.generateKey = async function generateKey(algorithm, extractable, keyUsages) {
    if (!isEd(algorithm)) return orig.generateKey(algorithm, extractable, keyUsages);
    const usages = checkUsages(keyUsages, ["sign", "verify"], true);
    const seed = new Uint8Array(runOp(() => _OPS.op_random_bytes(32)));
    const pub = publicOf(seed);
    return {
      publicKey: pubKey(pub, true, usages.filter((u) => u === "verify")),
      privateKey: privKey(seed, pub, extractable, usages.filter((u) => u === "sign")),
    };
  };

  subtle.importKey = async function importKey(format, keyData, algorithm, extractable, keyUsages) {
    if (!isEd(algorithm)) return orig.importKey(format, keyData, algorithm, extractable, keyUsages);
    if (format === "raw") {
      const pub = new Uint8Array(toBytes(keyData));
      if (pub.length !== 32) throw err("Ed25519 public key must be 32 bytes", "DataError");
      checkUsages(keyUsages, ["verify"], false);
      validPoint(pub);
      return pubKey(pub, extractable, keyUsages);
    }
    if (format === "spki") {
      const b = new Uint8Array(toBytes(keyData));
      if (!startsWith(b, SPKI)) throw err("Invalid Ed25519 SPKI", "DataError");
      checkUsages(keyUsages, ["verify"], false);
      const pub = b.slice(SPKI.length);
      validPoint(pub);
      return pubKey(pub, extractable, keyUsages);
    }
    if (format === "pkcs8") {
      const b = new Uint8Array(toBytes(keyData));
      if (!startsWith(b, PKCS8)) throw err("Invalid Ed25519 PKCS#8", "DataError");
      const usages = checkUsages(keyUsages, ["sign"], true);
      const seed = b.slice(PKCS8.length);
      return privKey(seed, publicOf(seed), extractable, usages);
    }
    if (format === "jwk") {
      const jwk = keyData;
      if (!jwk || jwk.kty !== "OKP" || jwk.crv !== "Ed25519" || typeof jwk.x !== "string") {
        throw err("Invalid Ed25519 JWK", "DataError");
      }
      if (jwk.alg !== undefined && jwk.alg !== "Ed25519" && jwk.alg !== "EdDSA") throw err("JWK 'alg' does not match Ed25519", "DataError");
      if (jwk.ext === false && extractable) throw err("JWK 'ext' is false but the key is requested extractable", "DataError");
      if (Array.isArray(jwk.key_ops) && (keyUsages || []).some((u) => !jwk.key_ops.includes(u))) {
        throw err("JWK 'key_ops' does not permit the requested usages", "DataError");
      }
      if (jwk.use !== undefined && jwk.use !== "sig") throw err("JWK 'use' is not 'sig'", "DataError");
      const pub = b64urlToBytes(jwk.x);
      if (pub.length !== 32) throw err("Ed25519 JWK 'x' must be 32 bytes", "DataError");
      if (typeof jwk.d === "string") {
        const usages = checkUsages(keyUsages, ["sign"], true);
        const seed = b64urlToBytes(jwk.d);
        if (seed.length !== 32) throw err("Ed25519 JWK 'd' must be 32 bytes", "DataError");
        const derived = publicOf(seed);
        if (!same(derived, pub)) throw err("Ed25519 JWK 'x' does not match 'd'", "DataError");
        return privKey(seed, derived, extractable, usages);
      }
      checkUsages(keyUsages, ["verify"], false);
      validPoint(pub);
      return pubKey(pub, extractable, keyUsages);
    }
    throw err("Unsupported key format for Ed25519 keys: " + format, "NotSupportedError");
  };

  subtle.exportKey = async function exportKey(format, key) {
    if (!keyIsEd(key)) return orig.exportKey(format, key);
    const m = keyBytes(key);
    if (!key.extractable) throw err("Key is not extractable", "InvalidAccessError");
    if (format === "raw" || format === "spki") {
      if (key.type !== "public") throw err("Key is not of type 'public'", "InvalidAccessError");
      return bufferOf(format === "raw" ? m.pub : cat(SPKI, m.pub));
    }
    if (format === "pkcs8") {
      if (key.type !== "private") throw err("Key is not of type 'private'", "InvalidAccessError");
      return bufferOf(cat(PKCS8, m.seed));
    }
    if (format === "jwk") {
      // Chrome emits OKP JWK members in alphabetical order.
      const jwk = { crv: "Ed25519" };
      if (key.type === "private") jwk.d = bytesToB64url(m.seed);
      jwk.ext = key.extractable;
      jwk.key_ops = key.usages.slice();
      jwk.kty = "OKP";
      jwk.x = bytesToB64url(m.pub);
      return jwk;
    }
    throw err("Unsupported key format for Ed25519 keys export: " + format, "NotSupportedError");
  };

  subtle.sign = async function sign(algorithm, key, data) {
    if (!isEd(algorithm) && !keyIsEd(key)) return orig.sign(algorithm, key, data);
    if (!isEd(algorithm) || !keyIsEd(key)) throw err("Key algorithm does not match the requested algorithm", "InvalidAccessError");
    const m = keyBytes(key);
    if (key.type !== "private") throw err("Key is not of type 'private'", "InvalidAccessError");
    if (!key.usages.includes("sign")) throw err("This CryptoKey cannot be used for signing", "InvalidAccessError");
    return bufferOf(runOp(() => _OPS.op_subtle_ed25519_sign(m.seed, toBytes(data))));
  };

  subtle.verify = async function verify(algorithm, key, signature, data) {
    if (!isEd(algorithm) && !keyIsEd(key)) return orig.verify(algorithm, key, signature, data);
    if (!isEd(algorithm) || !keyIsEd(key)) throw err("Key algorithm does not match the requested algorithm", "InvalidAccessError");
    const m = keyBytes(key);
    if (key.type !== "public") throw err("Key is not of type 'public'", "InvalidAccessError");
    if (!key.usages.includes("verify")) throw err("This CryptoKey cannot be used for verifying", "InvalidAccessError");
    return runOp(() => _OPS.op_subtle_ed25519_verify(m.pub, toBytes(signature), toBytes(data)));
  };
}
