// crypto.subtle (SubtleCrypto / CryptoKey) WebCrypto shim.
// Split out of bootstrap.js wholesale (god-file ratchet, pure move): top level only
// declares bindings; every bootstrap helper it names (_OPS, DOMException,
// __diting_clone_hooks) is touched at call time. Basic crypto.getRandomValues /
// randomUUID stay in bootstrap — this file only installs .subtle onto that object.

if (!globalThis.crypto) globalThis.crypto = {};
if (!globalThis.crypto.subtle) {
  // Real WebCrypto for the algorithms sites actually use: HMAC, AES-GCM/CBC/CTR,
  // PBKDF2, HKDF, ECDSA P-256/P-384 sign/verify, and ECDH key agreement
  // (deriveBits/deriveKey), plus raw/JWK key handling. The crypto
  // itself runs in Rust ops (RustCrypto); this shim only marshals bytes and
  // normalizes algorithm parameters. Other public-key algorithms (RSA*)
  // throw NotSupportedError rather than returning fake data.
  const keyMaterial = new WeakMap();

  class CryptoKey {
    constructor() { throw new TypeError("Illegal constructor"); }
    get [Symbol.toStringTag]() { return "CryptoKey"; }
  }
  function makeKey(type, extractable, algorithm, usages, bytes) {
    const k = Object.create(CryptoKey.prototype);
    Object.defineProperty(k, "type", { value: type, enumerable: true });
    Object.defineProperty(k, "extractable", { value: !!extractable, enumerable: true });
    Object.defineProperty(k, "algorithm", { value: algorithm, enumerable: true });
    Object.defineProperty(k, "usages", { value: Object.freeze((usages || []).slice()), enumerable: true });
    keyMaterial.set(k, bytes);
    return k;
  }
  function keyBytes(key) {
    if (!(key instanceof CryptoKey) || !keyMaterial.has(key)) {
      throw new DOMException("Argument is not a valid CryptoKey", "InvalidAccessError");
    }
    return keyMaterial.get(key);
  }

  const toBytes = (data) => {
    if (data instanceof ArrayBuffer) return new Uint8Array(data);
    if (ArrayBuffer.isView(data)) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
    return new Uint8Array(data || []);
  };
  const bufferOf = (u8) => new Uint8Array(u8).buffer;

  // ECDSA (P-256/P-384). The Rust ops return/accept a flat payload of four
  // u16-BE length-prefixed sections in this order: [pkcs8, sec1 point,
  // scalar, spki]. A key's keyMaterial entry is {curve, pkcs8, sec1, scalar,
  // spki} with the private-only sections null on public keys and vice versa.
  const ECDSA_CURVES = { "P-256": 32, "P-384": 48 };
  function ecdsaUnflat(buf) {
    const parts = [];
    let off = 0;
    for (let i = 0; i < 4; i++) {
      const len = (buf[off] << 8) | buf[off + 1];
      off += 2;
      parts.push(buf.subarray(off, off + len));
      off += len;
    }
    return parts;
  }
  function ecMaterial(key) {
    if (!(key instanceof CryptoKey) || !keyMaterial.has(key) || !keyMaterial.get(key) || !keyMaterial.get(key).curve) {
      throw new DOMException("Argument is not a valid CryptoKey", "InvalidAccessError");
    }
    return keyMaterial.get(key);
  }
  function requireEcdsaUsages(usages, allowed) {
    for (const u of usages) {
      if (!allowed.includes(u)) {
        throw new DOMException("The key usages must contain '" + allowed[0] + "'", "SyntaxError");
      }
    }
  }
  // Usage sets per EC algorithm face: ECDH derives, ECDSA signs.
  function ecUsagesFor(name, type) {
    if (name === "ECDH") return type === "private" ? ["deriveKey", "deriveBits"] : [];
    return type === "private" ? ["sign"] : ["verify"];
  }

  const ALGO_CANON = {
    "AES-CTR": "AES-CTR", "AES-CBC": "AES-CBC", "AES-GCM": "AES-GCM", "AES-KW": "AES-KW",
    "HMAC": "HMAC", "PBKDF2": "PBKDF2", "HKDF": "HKDF",
    "RSASSA-PKCS1-V1_5": "RSASSA-PKCS1-v1_5", "RSA-PSS": "RSA-PSS", "RSA-OAEP": "RSA-OAEP",
    "ECDSA": "ECDSA", "ECDH": "ECDH",
  };
  function normalizeAlgo(algorithm) {
    const a = typeof algorithm === "string" ? { name: algorithm } : (algorithm || {});
    const upper = String(a.name || "").toUpperCase();
    const name = ALGO_CANON[upper] || upper;
    return Object.assign({}, a, { name });
  }
  // SubtleCrypto hashes for HMAC/PBKDF2/HKDF and digest (SHA-1/256/384/512).
  function normalizeHash(h) {
    const n = (typeof h === "string" ? h : (h && h.name) || "").toUpperCase().replace("_", "-");
    if (n === "SHA-1" || n === "SHA-256" || n === "SHA-384" || n === "SHA-512") return n;
    throw new DOMException("Unsupported hash algorithm: " + (h && (h.name || h)), "NotSupportedError");
  }
  const hashBlockSize = (hash) => (hash === "SHA-384" || hash === "SHA-512" ? 128 : 64);

  function b64urlToBytes(s) {
    s = String(s).replace(/-/g, "+").replace(/_/g, "/");
    while (s.length % 4) s += "=";
    const bin = atob(s);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
  }
  function bytesToB64url(bytes) {
    let bin = "";
    for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
    return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
  }

  // Run an op, converting a Rust-side failure (bad GCM tag, bad CBC padding)
  // into the OperationError the WebCrypto spec requires. DOMExceptions we raise
  // ourselves pass through unchanged.
  function runOp(fn) {
    try { return fn(); }
    catch (e) {
      if (e instanceof DOMException) throw e;
      // `|| e` (not `?? e`): an empty-string message must survive as empty —
      // Chrome's corrupt-unwrap/corrupt-OAEP OperationError carries no text,
      // and falling back to `String(e)` would surface "Error" instead.
      const msg = e && e.message !== undefined ? String(e.message) : String(e);
      throw new DOMException(msg, "OperationError");
    }
  }

  function keyAlgorithmFor(alg, bytes) {
    switch (alg.name) {
      case "HMAC":
        return { name: "HMAC", hash: { name: normalizeHash(alg.hash) }, length: bytes.length * 8 };
      case "AES-CTR": case "AES-CBC": case "AES-GCM": case "AES-KW":
        // Chrome 155 dropped 192-bit AES (BoringShell): raw import of a
        // 24-byte key is an OperationError; the jwk path reports a DataError
        // ("right length" wording) in the oct import branch above us.
        if (bytes.length === 24) {
          throw new DOMException("192-bit AES keys are not supported", "OperationError");
        }
        if (bytes.length !== 16 && bytes.length !== 32) {
          throw new DOMException("AES key data must be 128 or 256 bits", "DataError");
        }
        return { name: alg.name, length: bytes.length * 8 };
      case "PBKDF2": return { name: "PBKDF2" };
      case "HKDF": return { name: "HKDF" };
      default:
        throw new DOMException("Unsupported key algorithm: " + alg.name, "NotSupportedError");
    }
  }

  // ---- Chrome-155-aligned shared validation (issue #269) ----
  // The usage faces below are pinned from live Chrome 155, not from the spec:
  // an unrecognized usage string is a TypeError; empty usages are a
  // SyntaxError except on public keys; out-of-family usages are a different
  // SyntaxError.
  const KNOWN_USAGES = ["encrypt", "decrypt", "sign", "verify", "deriveKey", "deriveBits", "wrapKey", "unwrapKey"];
  function checkUsageStrings(method, usages) {
    for (const u of usages || []) {
      if (!KNOWN_USAGES.includes(u)) {
        throw new TypeError("Failed to execute '" + method + "' on 'SubtleCrypto': Invalid keyUsages argument");
      }
    }
  }
  function checkKeyUsages(usages, allowed, isPublic) {
    if (!usages || usages.length === 0) {
      if (!isPublic) {
        throw new DOMException("Usages cannot be empty when creating a key.", "SyntaxError");
      }
      return;
    }
    for (const u of usages) {
      if (!allowed.includes(u)) {
        throw new DOMException("Cannot create a key using the specified key usages.", "SyntaxError");
      }
    }
  }
  function jwkRequire(jwk, member) {
    if (jwk[member] === undefined || jwk[member] === null) {
      throw new DOMException("The required JWK member \"" + member + "\" was missing", "DataError");
    }
  }
  // Strict base64url: Chrome rejects any '=' padding and non-alphabet bytes.
  function jwkMemberBytes(jwk, member) {
    const s = jwk[member];
    if (typeof s !== "string" || !/^[A-Za-z0-9_-]+$/.test(s) || s.length % 4 === 1) {
      throw new DOMException("The JWK member \"" + member + "\" could not be base64url decoded or contained padding", "DataError");
    }
    return b64urlToBytes(s);
  }
  function jwkValidateCommon(jwk, extractable, usages, expectedAlg) {
    if (expectedAlg && jwk.alg !== undefined && jwk.alg !== expectedAlg) {
      throw new DOMException("The JWK \"alg\" member was inconsistent with that specified by the Web Crypto call", "DataError");
    }
    if (jwk.use !== undefined) {
      const want = (jwk.use === "sig" || jwk.use === "enc") ? jwk.use : null;
      for (const u of usages) {
        const cat = (u === "sign" || u === "verify") ? "sig" : "enc";
        if (cat !== want) {
          throw new DOMException("The JWK \"use\" member was inconsistent with that specified by the Web Crypto call. The JWK usage must be a superset of those requested", "DataError");
        }
      }
    }
    if (Array.isArray(jwk.key_ops)) {
      const seen = new Set();
      for (const op of jwk.key_ops) {
        if (seen.has(op)) {
          throw new DOMException("The \"key_ops\" member of the JWK dictionary contains duplicate usages.", "DataError");
        }
        seen.add(op);
      }
      for (const u of usages) {
        if (!jwk.key_ops.includes(u)) {
          throw new DOMException("The JWK \"key_ops\" member was inconsistent with that specified by the Web Crypto call. The JWK usage must be a superset of those requested", "DataError");
        }
      }
    }
    if (jwk.ext === false && extractable) {
      // Chrome's message carries this typo verbatim ("inconsistent what that").
      throw new DOMException("The \"ext\" member of the JWK dictionary is inconsistent what that specified by the Web Crypto call", "DataError");
    }
  }
  // Chrome requires a hash member on HMAC and RSA-OAEP imports/generation and
  // words the missing/unrecognized cases differently per param tag.
  function requireHashParam(alg, method, paramTag) {
    const h = alg.hash;
    if (h == null || (typeof h !== "string" && (typeof h !== "object" || h.name == null))) {
      throw new TypeError("Failed to execute '" + method + "' on 'SubtleCrypto': " + paramTag + ": hash: Missing or not an AlgorithmIdentifier");
    }
    const n = String(typeof h === "string" ? h : h.name).toUpperCase().replace("_", "-");
    if (n !== "SHA-1" && n !== "SHA-256" && n !== "SHA-384" && n !== "SHA-512") {
      throw new DOMException("Failed to execute '" + method + "' on 'SubtleCrypto': " + paramTag + ": hash: Algorithm: Unrecognized name", "NotSupportedError");
    }
    return n;
  }

  // RSA-OAEP key material: a ten-section flat payload from the Rust ops
  // [pkcs8, spki, n, e, d, p, q, dp, dq, qi]; public-only imports carry
  // empty private sections. JWK alg names per OAEP hash (SHA-1 has no suffix).
  const RSA_OAEP_JWK_ALG = { "SHA-1": "RSA-OAEP", "SHA-256": "RSA-OAEP-256", "SHA-384": "RSA-OAEP-384", "SHA-512": "RSA-OAEP-512" };
  const RSA_OAEP_PUB_USAGES = ["encrypt", "wrapKey"];
  const RSA_OAEP_PRIV_USAGES = ["decrypt", "unwrapKey"];
  // #273: RSASSA-PKCS1-v1_5 (JWT RS256) — usages split sign/verify, JWK
  // algs RS1/RS256/RS384/RS512, pinned from Chrome 155.
  const RSASSA_JWK_ALG = { "SHA-1": "RS1", "SHA-256": "RS256", "SHA-384": "RS384", "SHA-512": "RS512" };
  const RSASSA_PUB_USAGES = ["verify"];
  const RSASSA_PRIV_USAGES = ["sign"];
  function rsaFace(name) {
    return name === "RSASSA-PKCS1-v1_5"
      ? { pub: RSASSA_PUB_USAGES, priv: RSASSA_PRIV_USAGES, alg: RSASSA_JWK_ALG }
      : { pub: RSA_OAEP_PUB_USAGES, priv: RSA_OAEP_PRIV_USAGES, alg: RSA_OAEP_JWK_ALG };
  }
  function rsaUnflat(buf) {
    const parts = [];
    let off = 0;
    for (let i = 0; i < 10; i++) {
      const len = (buf[off] << 8) | buf[off + 1];
      off += 2;
      parts.push(len ? buf.subarray(off, off + len) : null);
      off += len;
    }
    return { pkcs8: parts[0], spki: parts[1], n: parts[2], e: parts[3], d: parts[4], p: parts[5], q: parts[6], dp: parts[7], dq: parts[8], qi: parts[9] };
  }
  function rsaMaterial(key) {
    if (!(key instanceof CryptoKey) || !keyMaterial.has(key) || !keyMaterial.get(key) || keyMaterial.get(key).spki == null) {
      throw new DOMException("Argument is not a valid CryptoKey", "InvalidAccessError");
    }
    return keyMaterial.get(key);
  }
  // RSA import (spki / pkcs8 / jwk), shared by the OAEP and RSASSA faces
  // (the key DER is algorithm-agnostic; only the usage sets and JWK alg
  // names differ). The jwk ladder follows Chrome's order: kty → usage set
  // (which depends on d's presence) → required members n,e,d,p,q,dp,dq,qi
  // → strict b64url → alg/use/key_ops/ext.
  function rsaImportKey(format, keyData, alg, extractable, usages) {
    checkUsageStrings("importKey", usages);
    const hash = requireHashParam(alg, "importKey", "RsaHashedImportParams");
    const face = rsaFace(alg.name);
    let m, type;
    if (format === "spki") {
      checkKeyUsages(usages, face.pub, true);
      m = rsaUnflat(runOp(() => _OPS.op_subtle_rsa_import_spki(toBytes(keyData))));
      type = "public";
    } else if (format === "pkcs8") {
      checkKeyUsages(usages, face.priv, false);
      m = rsaUnflat(runOp(() => _OPS.op_subtle_rsa_import_pkcs8(toBytes(keyData))));
      type = "private";
    } else if (format === "jwk") {
      const jwk = keyData;
      if (!jwk || jwk.kty !== "RSA") {
        throw new DOMException("The JWK \"kty\" member was not \"RSA\"", "DataError");
      }
      const isPriv = typeof jwk.d === "string";
      checkKeyUsages(usages, isPriv ? face.priv : face.pub, !isPriv);
      jwkRequire(jwk, "n");
      jwkRequire(jwk, "e");
      const n = jwkMemberBytes(jwk, "n");
      const e = jwkMemberBytes(jwk, "e");
      const expected = face.alg[hash];
      if (isPriv) {
        for (const mem of ["d", "p", "q", "dp", "dq", "qi"]) jwkRequire(jwk, mem);
        const d = jwkMemberBytes(jwk, "d");
        const p = jwkMemberBytes(jwk, "p");
        const q = jwkMemberBytes(jwk, "q");
        jwkValidateCommon(jwk, extractable, usages, expected);
        m = rsaUnflat(runOp(() => _OPS.op_subtle_rsa_import_private(n, e, d, p, q)));
        type = "private";
      } else {
        jwkValidateCommon(jwk, extractable, usages, expected);
        m = rsaUnflat(runOp(() => _OPS.op_subtle_rsa_import_public(n, e)));
        type = "public";
      }
    } else {
      throw new DOMException("Unsupported key format for RSA keys: " + format, "NotSupportedError");
    }
    const algorithm = {
      name: alg.name, modulusLength: m.n.length * 8,
      publicExponent: new Uint8Array(m.e), hash: { name: hash },
    };
    return makeKey(type, extractable, algorithm, usages, m);
  }
  // Chrome's RSA generateKey policy (pinned from 155, both faces): exponent
  // must be 3 or 65537, modulus a multiple of 8 in [256, 8192], and the
  // usages split across the halves — encrypt/wrapKey+decrypt/unwrapKey on
  // OAEP, verify/sign on RSASSA (the public key is always extractable).
  function rsaGenerateKey(alg, extractable, usages) {
    checkUsageStrings("generateKey", usages);
    const hash = requireHashParam(alg, "generateKey", "RsaHashedKeyGenParams");
    const face = rsaFace(alg.name);
    const combined = face.pub.concat(face.priv);
    checkKeyUsages(usages, combined, false);
    const bits = alg.modulusLength;
    if (!Number.isInteger(bits) || bits < 256 || bits > 8192 || bits % 8 !== 0) {
      throw new DOMException("The modulus length must be a multiple of 8 bits and >= 256 and <= 8192", "OperationError");
    }
    const eBytes = toBytes(alg.publicExponent);
    let ev = 0n;
    for (const b of eBytes) ev = (ev << 8n) | BigInt(b);
    if (ev !== 3n && ev !== 65537n) {
      throw new DOMException("The \"publicExponent\" must be either 3 or 65537", "OperationError");
    }
    const m = rsaUnflat(runOp(() => _OPS.op_subtle_rsa_generate(bits, eBytes)));
    const algorithm = {
      name: alg.name, modulusLength: bits,
      publicExponent: new Uint8Array(m.e), hash: { name: hash },
    };
    const pub = makeKey("public", true, algorithm, usages.filter((u) => face.pub.includes(u)), m);
    const priv = makeKey("private", extractable, algorithm, usages.filter((u) => face.priv.includes(u)), m);
    return { publicKey: pub, privateKey: priv };
  }

  const subtle = {
    async digest(algorithm, data) {
      const name = (typeof algorithm === "string" ? algorithm : algorithm && algorithm.name || "").toUpperCase().replace("_", "-");
      if (name !== "SHA-1" && name !== "SHA-256" && name !== "SHA-384" && name !== "SHA-512" &&
          name !== "SHA-512/224" && name !== "SHA-512/256") {
        throw new DOMException("Unrecognized algorithm name", "NotSupportedError");
      }
      return bufferOf(_OPS.op_subtle_digest(name, toBytes(data)));
    },

    async importKey(format, keyData, algorithm, extractable, keyUsages) {
      const alg = normalizeAlgo(algorithm);
      if (alg.name === "ECDSA" || alg.name === "ECDH") {
        const crv = String(alg.namedCurve || "");
        if (!(crv in ECDSA_CURVES)) {
          throw new DOMException("Unrecognized namedCurve '" + crv + "'", "NotSupportedError");
        }
        const usages = keyUsages || [];
        const coordLen = ECDSA_CURVES[crv];
        let type, m;
        if (format === "raw") {
          const point = toBytes(keyData);
          if (point.length !== coordLen * 2 + 1 || point[0] !== 4) {
            throw new DOMException("Key data is not an uncompressed point on the named curve", "DataError");
          }
          const parts = ecdsaUnflat(_OPS.op_subtle_ecdsa_import_public(crv, point));
          m = { curve: crv, pkcs8: null, sec1: parts[1], scalar: null, spki: parts[3] };
          type = "public";
          requireEcdsaUsages(usages, ecUsagesFor(alg.name, "public"));
        } else if (format === "spki") {
          const parts = ecdsaUnflat(_OPS.op_subtle_ecdsa_import_public(crv, toBytes(keyData)));
          m = { curve: crv, pkcs8: null, sec1: parts[1], scalar: null, spki: parts[3] };
          type = "public";
          requireEcdsaUsages(usages, ecUsagesFor(alg.name, "public"));
        } else if (format === "pkcs8") {
          const parts = ecdsaUnflat(_OPS.op_subtle_ecdsa_import_private(crv, false, toBytes(keyData)));
          m = { curve: crv, pkcs8: parts[0], sec1: parts[1], scalar: parts[2], spki: parts[3] };
          type = "private";
          requireEcdsaUsages(usages, ecUsagesFor(alg.name, "private"));
        } else if (format === "jwk") {
          const jwk = keyData;
          if (!jwk || jwk.kty !== "EC") {
            throw new DOMException("Not an 'EC' JWK key", "DataError");
          }
          if (jwk.crv && jwk.crv !== crv) {
            throw new DOMException("The JWK 'crv' member does not match the algorithm's namedCurve", "DataError");
          }
          if (typeof jwk.x !== "string" || typeof jwk.y !== "string") {
            throw new DOMException("The JWK 'x' and 'y' members are required for EC keys", "DataError");
          }
          const x = b64urlToBytes(jwk.x), y = b64urlToBytes(jwk.y);
          if (x.length !== coordLen || y.length !== coordLen) {
            throw new DOMException("The JWK 'x'/'y' coordinate length does not match the named curve", "DataError");
          }
          const point = new Uint8Array(coordLen * 2 + 1);
          point[0] = 4;
          point.set(x, 1);
          point.set(y, coordLen + 1);
          if (typeof jwk.d === "string") {
            const d = b64urlToBytes(jwk.d);
            if (d.length !== coordLen) {
              throw new DOMException("The JWK 'd' member length does not match the named curve", "DataError");
            }
            const parts = ecdsaUnflat(_OPS.op_subtle_ecdsa_import_private(crv, true, d));
            m = { curve: crv, pkcs8: parts[0], sec1: parts[1], scalar: parts[2], spki: parts[3] };
            type = "private";
            requireEcdsaUsages(usages, ecUsagesFor(alg.name, "private"));
          } else {
            const parts = ecdsaUnflat(_OPS.op_subtle_ecdsa_import_public(crv, point));
            m = { curve: crv, pkcs8: null, sec1: parts[1], scalar: null, spki: parts[3] };
            type = "public";
            requireEcdsaUsages(usages, ecUsagesFor(alg.name, "public"));
          }
        } else {
          throw new DOMException("Unsupported key format for EC keys: " + format, "NotSupportedError");
        }
        return makeKey(type, extractable, { name: alg.name, namedCurve: crv }, usages, m);
      }
      if (alg.name === "RSA-OAEP" || alg.name === "RSASSA-PKCS1-v1_5") {
        return rsaImportKey(format, keyData, alg, extractable, keyUsages || []);
      }
      checkUsageStrings("importKey", keyUsages || []);
      let bytes;
      if (format === "raw") {
        bytes = toBytes(keyData);
      } else if (format === "jwk") {
        if (!keyData || keyData.kty !== "oct") {
          throw new DOMException("The JWK \"kty\" member was not \"oct\"", "DataError");
        }
        jwkRequire(keyData, "k");
        bytes = jwkMemberBytes(keyData, "k");
        if (bytes.length === 24) {
          throw new DOMException("The JWK \"k\" member did not include the right length of key data for the given algorithm.", "DataError");
        }
      } else {
        throw new DOMException("Only 'raw' and symmetric 'jwk' key formats are supported", "NotSupportedError");
      }
      let expectedAlg = null;
      if (alg.name === "HMAC") {
        const hash = requireHashParam(alg, "importKey", "HmacImportParams");
        checkKeyUsages(keyUsages || [], ["sign", "verify"], false);
        // "SHA-1".slice(4) === "1" too — no special case needed.
        expectedAlg = "HS" + hash.slice(4);
      } else if (alg.name === "AES-CTR" || alg.name === "AES-CBC" || alg.name === "AES-GCM" || alg.name === "AES-KW") {
        checkKeyUsages(keyUsages || [],
          alg.name === "AES-KW" ? ["wrapKey", "unwrapKey"] : ["encrypt", "decrypt", "wrapKey", "unwrapKey"], false);
        expectedAlg = "A" + (bytes.length * 8) + alg.name.slice(4);
      } else if (alg.name === "PBKDF2" || alg.name === "HKDF") {
        checkKeyUsages(keyUsages || [], ["deriveKey", "deriveBits"], false);
      }
      if (format === "jwk") {
        jwkValidateCommon(keyData, extractable, keyUsages || [], expectedAlg);
      }
      return makeKey("secret", extractable, keyAlgorithmFor(alg, bytes), keyUsages, bytes);
    },

    async exportKey(format, key) {
      const bytes = keyBytes(key);
      if (!key.extractable) throw new DOMException("Key is not extractable", "InvalidAccessError");
      if (key.algorithm && (key.algorithm.name === "ECDSA" || key.algorithm.name === "ECDH")) {
        const m = ecMaterial(key);
        if (format === "raw" || format === "spki") {
          if (key.type !== "public") throw new DOMException("Key is not of type 'public'", "InvalidAccessError");
          return bufferOf(format === "raw" ? m.sec1 : m.spki);
        }
        if (format === "pkcs8") {
          if (key.type !== "private") throw new DOMException("Key is not of type 'private'", "InvalidAccessError");
          return bufferOf(m.pkcs8);
        }
        if (format === "jwk") {
          const coordLen = ECDSA_CURVES[m.curve];
          // Chrome emits EC JWK fields in alphabetical insertion order.
          const jwk = { crv: m.curve };
          if (key.type === "private") jwk.d = bytesToB64url(m.scalar);
          jwk.ext = key.extractable;
          jwk.key_ops = key.usages.slice();
          jwk.kty = "EC";
          jwk.x = bytesToB64url(m.sec1.subarray(1, 1 + coordLen));
          jwk.y = bytesToB64url(m.sec1.subarray(1 + coordLen));
          return jwk;
        }
        throw new DOMException("Unsupported key format for EC keys export: " + format, "NotSupportedError");
      }
      if (key.algorithm && (key.algorithm.name === "RSA-OAEP" || key.algorithm.name === "RSASSA-PKCS1-v1_5")) {
        const m = rsaMaterial(key);
        if (format === "spki") {
          if (key.type !== "public") throw new DOMException("The key is not of the expected type", "InvalidAccessError");
          return bufferOf(m.spki);
        }
        if (format === "pkcs8") {
          if (key.type !== "private") throw new DOMException("The key is not of the expected type", "InvalidAccessError");
          return bufferOf(m.pkcs8);
        }
        if (format === "jwk") {
          // Chrome emits RSA JWK fields in alphabetical insertion order; the
          // alg name is face-specific (RSA-OAEP-256 vs RS256).
          const jwk = {};
          const algName = key.algorithm.hash && rsaFace(key.algorithm.name).alg[key.algorithm.hash.name];
          if (algName) jwk.alg = algName;
          if (key.type === "private") {
            jwk.d = bytesToB64url(m.d);
            jwk.dp = bytesToB64url(m.dp);
            jwk.dq = bytesToB64url(m.dq);
          }
          jwk.e = bytesToB64url(m.e);
          jwk.ext = key.extractable;
          jwk.key_ops = key.usages.slice();
          jwk.kty = "RSA";
          jwk.n = bytesToB64url(m.n);
          if (key.type === "private") {
            jwk.p = bytesToB64url(m.p);
            jwk.q = bytesToB64url(m.q);
            jwk.qi = bytesToB64url(m.qi);
          }
          return jwk;
        }
        if (format === "raw") {
          throw new DOMException("Unsupported export key format for algorithm", "NotSupportedError");
        }
        throw new DOMException("Unsupported key format for RSA keys export: " + format, "NotSupportedError");
      }
      if (format === "raw") return bufferOf(bytes);
      if (format === "jwk") {
        // Chrome emits oct JWK fields in alphabetical insertion order.
        const jwk = {};
        if (key.algorithm.name && key.algorithm.name.indexOf("AES-") === 0) {
          jwk.alg = "A" + (bytes.length * 8) + key.algorithm.name.slice(4);
        } else if (key.algorithm.name === "HMAC") {
          jwk.alg = "HS" + key.algorithm.hash.name.slice(4);
        }
        jwk.ext = key.extractable;
        jwk.k = bytesToB64url(bytes);
        jwk.key_ops = key.usages.slice();
        jwk.kty = "oct";
        return jwk;
      }
      throw new DOMException("Only 'raw' and 'jwk' export is supported", "NotSupportedError");
    },

    async generateKey(algorithm, extractable, keyUsages) {
      const alg = normalizeAlgo(algorithm);
      if (alg.name === "RSA-OAEP" || alg.name === "RSASSA-PKCS1-v1_5") {
        return rsaGenerateKey(alg, extractable, keyUsages || []);
      }
      if (alg.name === "HMAC") {
        checkUsageStrings("generateKey", keyUsages || []);
        const hash = requireHashParam(alg, "generateKey", "HmacKeyGenParams");
        checkKeyUsages(keyUsages || [], ["sign", "verify"], false);
        const len = alg.length ? Math.ceil(alg.length / 8) : hashBlockSize(hash);
        const bytes = _OPS.op_random_bytes(len);
        return makeKey("secret", extractable, { name: "HMAC", hash: { name: hash }, length: len * 8 }, keyUsages, bytes);
      }
      if (alg.name === "AES-CTR" || alg.name === "AES-CBC" || alg.name === "AES-GCM" || alg.name === "AES-KW") {
        checkUsageStrings("generateKey", keyUsages || []);
        if (alg.length === 192) {
          throw new DOMException("192-bit AES keys are not supported", "OperationError");
        }
        if (alg.length !== 128 && alg.length !== 256) {
          throw new DOMException("AES key length must be 128 or 256 bits", "OperationError");
        }
        checkKeyUsages(keyUsages || [],
          alg.name === "AES-KW" ? ["wrapKey", "unwrapKey"] : ["encrypt", "decrypt", "wrapKey", "unwrapKey"], false);
        const bytes = _OPS.op_random_bytes(alg.length / 8);
        return makeKey("secret", extractable, { name: alg.name, length: alg.length }, keyUsages, bytes);
      }
      if (alg.name === "ECDSA" || alg.name === "ECDH") {
        const crv = String(alg.namedCurve || "");
        if (!(crv in ECDSA_CURVES)) {
          throw new DOMException("Unrecognized namedCurve '" + crv + "'", "NotSupportedError");
        }
        const usages = keyUsages || [];
        if (alg.name === "ECDSA") {
          if (usages.some((u) => u !== "sign" && u !== "verify") || (!usages.includes("sign") && !usages.includes("verify"))) {
            throw new DOMException("The key usages must contain 'sign' or 'verify'", "SyntaxError");
          }
        } else if (usages.some((u) => u !== "deriveKey" && u !== "deriveBits") || usages.length === 0) {
          throw new DOMException("The key usages must contain 'deriveKey' or 'deriveBits'", "SyntaxError");
        }
        const parts = ecdsaUnflat(_OPS.op_subtle_ecdsa_generate(crv));
        const m = { curve: crv, pkcs8: parts[0], sec1: parts[1], scalar: parts[2], spki: parts[3] };
        const privUsages = usages.filter((u) => ecUsagesFor(alg.name, "private").includes(u));
        const pub = makeKey("public", true, { name: alg.name, namedCurve: crv }, ecUsagesFor(alg.name, "public"), m);
        const priv = makeKey("private", extractable, { name: alg.name, namedCurve: crv }, privUsages, m);
        return { publicKey: pub, privateKey: priv };
      }
      throw new DOMException("generateKey does not support " + alg.name, "NotSupportedError");
    },

    async sign(algorithm, key, data) {
      const alg = normalizeAlgo(algorithm);
      if (alg.name === "ECDSA") {
        const m = ecMaterial(key);
        if (key.type !== "private") throw new DOMException("Key is not of type 'private'", "InvalidAccessError");
        if (!key.usages.includes("sign")) throw new DOMException("This CryptoKey cannot be used for signing", "InvalidAccessError");
        const hash = normalizeHash(alg.hash || (key.algorithm.hash && key.algorithm.hash.name));
        return bufferOf(runOp(() => _OPS.op_subtle_ecdsa_sign(m.curve, hash, m.pkcs8, toBytes(data))));
      }
      if (alg.name === "RSASSA-PKCS1-v1_5") {
        const m = rsaMaterial(key);
        // Chrome pins one message for every failure shape here — public key,
        // missing usage — unlike the ECDSA wording above.
        if (key.type !== "private" || !key.usages.includes("sign")) {
          throw new DOMException("key.usages does not permit this operation", "InvalidAccessError");
        }
        // The hash rides the key; a hash param on sign() is ignored
        // (Chrome-pinned: same signature with and without).
        const hash = key.algorithm.hash && key.algorithm.hash.name;
        return bufferOf(runOp(() => _OPS.op_subtle_rsa_pkcs1v15_sign(hash, m.n, m.e, m.d, m.p, m.q, toBytes(data))));
      }
      const bytes = keyBytes(key);
      if (alg.name === "HMAC") {
        const hash = key.algorithm && key.algorithm.hash ? key.algorithm.hash.name : normalizeHash(alg.hash);
        return bufferOf(runOp(() => _OPS.op_subtle_hmac(hash, bytes, toBytes(data))));
      }
      throw new DOMException("sign does not support " + alg.name, "NotSupportedError");
    },

    async verify(algorithm, key, signature, data) {
      const alg = normalizeAlgo(algorithm);
      if (alg.name === "ECDSA") {
        const m = ecMaterial(key);
        if (key.type !== "public") throw new DOMException("Key is not of type 'public'", "InvalidAccessError");
        if (!key.usages.includes("verify")) throw new DOMException("This CryptoKey cannot be used for verifying", "InvalidAccessError");
        const hash = normalizeHash(alg.hash || (key.algorithm.hash && key.algorithm.hash.name));
        return runOp(() => _OPS.op_subtle_ecdsa_verify(m.curve, hash, m.sec1, toBytes(signature), toBytes(data)));
      }
      if (alg.name === "RSASSA-PKCS1-v1_5") {
        const m = rsaMaterial(key);
        if (key.type !== "public" || !key.usages.includes("verify")) {
          throw new DOMException("key.usages does not permit this operation", "InvalidAccessError");
        }
        const hash = key.algorithm.hash && key.algorithm.hash.name;
        return runOp(() => _OPS.op_subtle_rsa_pkcs1v15_verify(hash, m.n, m.e, toBytes(signature), toBytes(data)));
      }
      const bytes = keyBytes(key);
      if (alg.name === "HMAC") {
        const hash = key.algorithm && key.algorithm.hash ? key.algorithm.hash.name : normalizeHash(alg.hash);
        const mac = runOp(() => _OPS.op_subtle_hmac(hash, bytes, toBytes(data)));
        const sig = toBytes(signature);
        if (sig.length !== mac.length) return false;
        let diff = 0;
        for (let i = 0; i < mac.length; i++) diff |= mac[i] ^ sig[i];
        return diff === 0;
      }
      throw new DOMException("verify does not support " + alg.name, "NotSupportedError");
    },

    async encrypt(algorithm, key, data) { return subtleCipher(true, algorithm, key, data, false); },
    async decrypt(algorithm, key, data) { return subtleCipher(false, algorithm, key, data, false); },

    async deriveBits(algorithm, baseKey, length) {
      const alg = normalizeAlgo(algorithm);
      if (alg.name === "ECDH") {
        const m = ecMaterial(baseKey);
        if (baseKey.type !== "private") throw new DOMException("Key is not of type 'private'", "InvalidAccessError");
        if (!baseKey.usages.includes("deriveBits") && !baseKey.usages.includes("deriveKey")) {
          throw new DOMException("This CryptoKey cannot be used for derivation", "InvalidAccessError");
        }
        const pub = alg.public;
        const pm = ecMaterial(pub);
        if (pub.type !== "public" || pm.curve !== m.curve) {
          throw new DOMException("The ECDH 'public' key is of the wrong type or curve", "InvalidAccessError");
        }
        const secret = runOp(() => _OPS.op_subtle_ecdh_derive_bits(m.curve, m.scalar, pm.sec1));
        if (length == null) return bufferOf(secret);
        if (length <= 0 || length % 8 !== 0 || length / 8 > secret.length) {
          throw new DOMException("Invalid ECDH deriveBits length", "OperationError");
        }
        return bufferOf(secret.subarray(0, length / 8));
      }
      const bytes = keyBytes(baseKey);
      const lenBytes = Math.ceil((length || 0) / 8);
      if (alg.name === "PBKDF2") {
        const hash = normalizeHash(alg.hash);
        const salt = toBytes(alg.salt);
        const iterations = alg.iterations >>> 0;
        return bufferOf(runOp(() => _OPS.op_subtle_pbkdf2(hash, bytes, salt, iterations, lenBytes)));
      }
      if (alg.name === "HKDF") {
        const hash = normalizeHash(alg.hash);
        const salt = alg.salt != null ? toBytes(alg.salt) : new Uint8Array(0);
        const info = alg.info != null ? toBytes(alg.info) : new Uint8Array(0);
        return bufferOf(runOp(() => _OPS.op_subtle_hkdf(hash, bytes, salt, info, lenBytes)));
      }
      throw new DOMException("deriveBits does not support " + alg.name, "NotSupportedError");
    },

    async deriveKey(algorithm, baseKey, derivedKeyAlgorithm, extractable, keyUsages) {
      const dAlg = normalizeAlgo(derivedKeyAlgorithm);
      let bits;
      if (dAlg.name === "HMAC") {
        bits = dAlg.length || hashBlockSize(normalizeHash(dAlg.hash)) * 8;
      } else if (dAlg.name === "AES-CTR" || dAlg.name === "AES-CBC" || dAlg.name === "AES-GCM" || dAlg.name === "AES-KW") {
        bits = dAlg.length;
        if (bits !== 128 && bits !== 192 && bits !== 256) {
          throw new DOMException("AES key length must be 128, 192, or 256 bits", "OperationError");
        }
      } else {
        throw new DOMException("deriveKey does not support deriving " + dAlg.name, "NotSupportedError");
      }
      const derivedBits = await this.deriveBits(algorithm, baseKey, bits);
      return this.importKey("raw", derivedBits, derivedKeyAlgorithm, extractable, keyUsages);
    },

    async wrapKey(format, key, wrappingKey, wrapAlgorithm) {
      const exported = await this.exportKey(format, key);
      const bytes = format === "jwk"
        ? new TextEncoder().encode(JSON.stringify(exported))
        : new Uint8Array(exported);
      return subtleCipher(true, wrapAlgorithm, wrappingKey, bytes, true);
    },

    async unwrapKey(format, wrappedKey, unwrappingKey, unwrapAlgorithm, unwrappedKeyAlgorithm, extractable, keyUsages) {
      checkUsageStrings("unwrapKey", keyUsages || []);
      const decrypted = subtleCipher(false, unwrapAlgorithm, unwrappingKey, toBytes(wrappedKey), true);
      const keyData = format === "jwk"
        ? JSON.parse(new TextDecoder().decode(new Uint8Array(decrypted)))
        : decrypted;
      return this.importKey(format, keyData, unwrappedKeyAlgorithm, extractable, keyUsages);
    },
  };

  // encrypt/decrypt core, shared with wrapKey/unwrapKey (`viaWrap` opens the
  // AES-KW path — Chrome exposes AES-KW only through wrapping, and rejects a
  // direct encrypt/decrypt call with NotSupportedError).
  function subtleCipher(encrypt, algorithm, key, data, viaWrap) {
    const alg = normalizeAlgo(algorithm);
    const input = toBytes(data);
    if (alg.name === "RSA-OAEP") {
      const m = rsaMaterial(key);
      const needed = encrypt ? ["encrypt", "wrapKey"] : ["decrypt", "unwrapKey"];
      if (!key.usages.some((u) => needed.includes(u))) {
        throw new DOMException("key.usages does not permit this operation", "InvalidAccessError");
      }
      if ((encrypt && key.type !== "public") || (!encrypt && key.type !== "private")) {
        throw new DOMException("The key is not of the expected type", "InvalidAccessError");
      }
      const label = alg.label != null ? toBytes(alg.label) : new Uint8Array(0);
      const hash = (key.algorithm && key.algorithm.hash && key.algorithm.hash.name) || "SHA-256";
      const empty = new Uint8Array(0);
      return bufferOf(runOp(() => _OPS.op_subtle_rsa_oaep(
        encrypt, hash, label, m.n, m.e,
        encrypt ? empty : m.d, encrypt ? empty : m.p, encrypt ? empty : m.q, input)));
    }
    if (alg.name === "AES-KW") {
      if (!viaWrap) {
        throw new DOMException(
          "Failed to execute '" + (encrypt ? "encrypt" : "decrypt") + "' on 'SubtleCrypto': AES-KW: Unsupported operation: " + (encrypt ? "encrypt" : "decrypt"),
          "NotSupportedError");
      }
      const bytes = keyBytes(key);
      const needed = encrypt ? ["encrypt", "wrapKey"] : ["decrypt", "unwrapKey"];
      if (!key.usages.some((u) => needed.includes(u))) {
        throw new DOMException("key.usages does not permit this operation", "InvalidAccessError");
      }
      if (encrypt && input.length % 8 !== 0) {
        throw new DOMException("The AES-KW input data length is invalid: not a multiple of 8 bytes", "DataError");
      }
      return bufferOf(runOp(() => _OPS.op_subtle_aes_kw(encrypt, bytes, input)));
    }
    const bytes = keyBytes(key);
    const needed = encrypt ? ["encrypt", "wrapKey"] : ["decrypt", "unwrapKey"];
    if (!key.usages.some((u) => needed.includes(u))) {
      throw new DOMException("key.usages does not permit this operation", "InvalidAccessError");
    }
    if (alg.name === "AES-GCM") {
      const iv = toBytes(alg.iv);
      const aad = alg.additionalData != null ? toBytes(alg.additionalData) : new Uint8Array(0);
      const tagLength = alg.tagLength == null ? 128 : alg.tagLength;
      if (tagLength !== 128) {
        throw new DOMException("Only a 128-bit AES-GCM tag length is supported", "NotSupportedError");
      }
      return bufferOf(runOp(() => _OPS.op_subtle_aes_gcm(encrypt, bytes, iv, aad, input)));
    }
    if (alg.name === "AES-CBC") {
      const iv = toBytes(alg.iv);
      return bufferOf(runOp(() => _OPS.op_subtle_aes_cbc(encrypt, bytes, iv, input)));
    }
    if (alg.name === "AES-CTR") {
      const counter = toBytes(alg.counter);
      const length = alg.length >>> 0;
      return bufferOf(runOp(() => _OPS.op_subtle_aes_ctr(bytes, counter, length, input)));
    }
    throw new DOMException((encrypt ? "encrypt" : "decrypt") + " does not support " + alg.name, "NotSupportedError");
  }

  globalThis.CryptoKey = CryptoKey;
  globalThis.SubtleCrypto = function SubtleCrypto() { throw new TypeError("Illegal constructor"); };
  Object.setPrototypeOf(subtle, globalThis.SubtleCrypto.prototype);
  globalThis.crypto.subtle = subtle;
  globalThis.__diting_subtle_hooks = { makeKey, keyBytes, toBytes, bufferOf, b64urlToBytes, bytesToB64url, runOp };

  // A CryptoKey cloned via structuredClone or postMessage is a different
  // object, so the keyMaterial WeakMap lookup misses and crypto.subtle throws
  // "Argument is not a valid CryptoKey". Rebuild the (cloned) key through
  // makeKey so it re-enters the WeakMap and stays usable. `seen` is the clone
  // memo _structuredClone hands every hook; populate it so one key reached
  // twice in a graph clones to one shared object (upstream 8698afc).
  globalThis.__diting_clone_hooks = globalThis.__diting_clone_hooks || {};
  globalThis.__diting_clone_hooks["CryptoKey"] = function (src, seen) {
    if (seen && seen.has(src)) return seen.get(src);
    const copy = makeKey(src.type, src.extractable, src.algorithm, src.usages, keyBytes(src));
    if (seen) seen.set(src, copy);
    return copy;
  };
}
