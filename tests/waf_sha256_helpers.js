// Compact SHA-256 + the three wrapper helpers the byte-WAF challenge page
// (juejin.cn class) loads from its out-sha256.js. Same contracts:
//   s256(prefixBytes, str) -> hex digest of prefixBytes ++ utf8(str)
//   b64tohex(b64) -> hex
//   b64tou8a(b64) -> Uint8Array
// The real out-sha256.js is Chromium's; this is a faithful compact reimpl
// for offline tests of the challenge flow (onload -> PoW -> cookie -> reload).

var WAF_K = [
  0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1,
  0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3,
  0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
  0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
  0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147,
  0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
  0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
  0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
  0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
  0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208,
  0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

function wafSha256Bytes(bytes) {
  var H = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f,
           0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
  var msg = Array.prototype.slice.call(bytes);
  var bitLen = msg.length * 8;
  msg.push(0x80);
  while (msg.length % 64 !== 56) msg.push(0);
  for (var i = 7; i >= 0; i--) msg.push(Math.floor(bitLen / Math.pow(2, i * 8)) & 0xff);

  function rotr(w, r) { return ((w << (32 - r)) | (w >>> r)) >>> 0; }

  for (var off = 0; off < msg.length; off += 64) {
    var W = new Array(64);
    for (var t = 0; t < 16; t++) {
      W[t] = ((msg[off + t * 4] << 24) | (msg[off + t * 4 + 1] << 16) |
              (msg[off + t * 4 + 2] << 8) | msg[off + t * 4 + 3]) >>> 0;
    }
    for (var t = 16; t < 64; t++) {
      var s0 = rotr(W[t - 15], 7) ^ rotr(W[t - 15], 18) ^ (W[t - 15] >>> 3);
      var s1 = rotr(W[t - 2], 17) ^ rotr(W[t - 2], 19) ^ (W[t - 2] >>> 10);
      W[t] = (W[t - 16] + s0 + W[t - 7] + s1) >>> 0;
    }
    var A = H[0], B = H[1], C = H[2], D = H[3],
        E = H[4], F = H[5], G = H[6], Hh = H[7];
    for (var t = 0; t < 64; t++) {
      var S1 = rotr(E, 6) ^ rotr(E, 11) ^ rotr(E, 25);
      var ch = (E & F) ^ (~E & G);
      var t1 = (Hh + S1 + ch + WAF_K[t] + W[t]) >>> 0;
      var S0 = rotr(A, 2) ^ rotr(A, 13) ^ rotr(A, 22);
      var maj = (A & B) ^ (A & C) ^ (B & C);
      var t2 = (S0 + maj) >>> 0;
      Hh = G; G = F; F = E;
      E = (D + t1) >>> 0;
      D = C; C = B; B = A;
      A = (t1 + t2) >>> 0;
    }
    H[0] = (H[0] + A) >>> 0; H[1] = (H[1] + B) >>> 0;
    H[2] = (H[2] + C) >>> 0; H[3] = (H[3] + D) >>> 0;
    H[4] = (H[4] + E) >>> 0; H[5] = (H[5] + F) >>> 0;
    H[6] = (H[6] + G) >>> 0; H[7] = (H[7] + Hh) >>> 0;
  }

  var out = [];
  for (var i = 0; i < 8; i++) {
    for (var j = 24; j >= 0; j -= 8) out.push((H[i] >>> j) & 0xff);
  }
  return out;
}

function s256(s1, s2) {
  var enc = new TextEncoder();
  var bytes = Array.prototype.slice.call(s1).concat(Array.from(enc.encode(s2)));
  return wafSha256Bytes(bytes)
    .map(function (v) {
      return ((v >>> 4).toString(16)) + ((v & 0xf).toString(16));
    })
    .join('');
}

function b64tohex(b) {
  return Array.prototype.map.call(atob(b), function (c) {
    return c.charCodeAt(0).toString(16).padStart(2, '0');
  }).join('');
}

function b64tou8a(b) {
  return Uint8Array.from(atob(b), function (c) { return c.charCodeAt(0); });
}
