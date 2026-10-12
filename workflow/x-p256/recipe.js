(function(){
  if (typeof globalThis !== 'undefined' && globalThis.P256) return;
// P-256 in-page (BigInt, Jacobian scalar mul) — engine crypto.subtle lacks ECDSA
const P256 = (() => {
  const p  = 0xffffffff00000001000000000000000000000000ffffffffffffffffffffffffn;
  const a  = p - 3n;
  const b  = 0x5ac635d8aa3a93e7b3ebbd55769886bc651d06b0cc53b0f63bce3c3e27d2604bn;
  const Gx = 0x6b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296n;
  const Gy = 0x4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5n;
  const n  = 0xffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc632551n;
  const mod = (x, m = p) => ((x % m) + m) % m;
  const inv = (x, m = p) => { let g = m, y = 0n, r = x % m, q, t, s = 1n; while (r > 0n) { q = g / r; t = g - q * r; g = r; r = t; t = y - q * s; y = s; s = t; } return y < 0n ? y + m : y; };
  const inf = () => ({x: 1n, y: 1n, z: 0n});
  // full dbl (correct short-weierstrass jacobian doubling)
  const dbl = (P0) => {
    if (P0.z === 0n || P0.y === 0n) return inf();
    const Y1 = P0.y, Z1 = P0.z;
    const S = mod(4n * P0.x * Y1 * Y1);
    const Z1sq = mod(Z1 * Z1);
    const Z1cu = mod(Z1sq * Z1);
    const M = mod(3n * P0.x * P0.x + a * Z1sq * Z1sq);
    const X3 = mod(M * M - 2n * S);
    const Y3 = mod(M * (S - X3) - 8n * Y1 * Y1 * mod(Y1 * Y1));
    const Z3 = mod(2n * Y1 * Z1);
    return {x: X3, y: Y3, z: Z3};
  };
  const add = (P0, Q) => {
    if (P0.z === 0n) return Q;
    if (Q.z === 0n) return P0;
    const Z1Z1 = mod(P0.z * P0.z), Z2Z2 = mod(Q.z * Q.z);
    const U1 = mod(P0.x * Z2Z2), U2 = mod(Q.x * Z1Z1);
    const S1 = mod(P0.y * Z2Z2 * Q.z), S2 = mod(Q.y * Z1Z1 * P0.z);
    if (U1 === U2) { if (S1 !== S2) return inf(); return dbl(P0); }
    const H = mod(U2 - U1), I = mod(4n * H * H), J = mod(H * I);
    const r = mod(2n * (S2 - S1)), V = mod(U1 * I);
    const X3 = mod(r * r - J - 2n * V);
    const Y3 = mod(r * (V - X3) - 2n * S1 * J);
    const Z3 = mod(2n * P0.z * Q.z * H);
    return {x: X3, y: Y3, z: Z3};
  };
  const mul = (k, Pt) => {
    let R = inf(), A = Pt, kk = mod(k, n) === 0n ? 0n : ((k % n) + n) % n;
    while (kk > 0n) { if (kk & 1n) R = add(R, A); A = dbl(A); kk >>= 1n; }
    return R;
  };
  const toAffine = (Pt) => {
    if (Pt.z === 0n) return null;
    const zi = inv(Pt.z), zi2 = mod(zi * zi);
    return {x: mod(Pt.x * zi2), y: mod(Pt.y * zi2 * zi)};
  };
  const G = {x: Gx, y: Gy, z: 1n};
  const hex32 = (v) => v.toString(16).padStart(64, '0');
  const bytesBE = (v, len) => { const h = v.toString(16).padStart(len * 2, '0'); const out = new Uint8Array(len); for (let i = 0; i < len; i++) out[i] = parseInt(h.substr(i * 2, 2), 16); return out; };
  const randBelow = (m) => {
    const bl = (m.toString(16).length + 1) >> 1;
    const buf = new Uint8Array(bl);
    for (;;) {
      crypto.getRandomValues(buf);
      let v = 0n; for (const byte of buf) v = (v << 8n) | BigInt(byte);
      if (v > 0n && v < m) return v;
    }
  };
  async function sha256bytes(msg) { return new Uint8Array(await crypto.subtle.digest('SHA-256', new TextEncoder().encode(msg))); }
  const genKeyPair = () => {
    const d = randBelow(n);
    const Q = toAffine(mul(d, G));
    return {d, Q};
  };
  const spki = (Q) => {
    const pre = [0x30,0x59,0x30,0x13,0x06,0x07,0x2a,0x86,0x48,0xce,0x3d,0x02,0x01,0x06,0x08,0x2a,0x86,0x48,0xce,0x3d,0x03,0x01,0x07,0x03,0x42,0x00,0x04];
    return new Uint8Array([...pre, ...bytesBE(Q.x, 32), ...bytesBE(Q.y, 32)]);
  };
  async function sign(priv, msg) {
    const h = await sha256bytes(msg);
    let e = 0n; for (const byte of h) e = (e << 8n) | BigInt(byte);
    e %= n;
    for (;;) {
      const k = randBelow(n);
      const R = toAffine(mul(k, G));
      const r = mod(R.x, n);
      if (r === 0n) continue;
      const s = mod(inv(k, n) * (e + r * priv.d), n);
      if (s === 0n) continue;
      return {r, s};
    }
  }
  async function verify(pub, msg, r, s) {
    if (r <= 0n || r >= n || s <= 0n || s >= n) return false;
    const h = await sha256bytes(msg);
    let e = 0n; for (const byte of h) e = (e << 8n) | BigInt(byte);
    e %= n;
    const w = inv(s, n);
    const u1 = mod(e * w, n), u2 = mod(r * w, n);
    const Pt = toAffine(add(mul(u1, G), mul(u2, {x: pub.x, y: pub.y, z: 1n})));
    if (!Pt) return false;
    return mod(Pt.x, n) === r;
  }
  return {p, n, G, genKeyPair, spki, sign, verify, hex32, bytesBE};
})();

if (typeof globalThis !== "undefined") globalThis.P256 = P256;
})();
