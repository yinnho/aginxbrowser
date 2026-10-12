(function () {
  if (typeof globalThis !== "undefined" && typeof globalThis.abxEnsureAuthCtx === "function") {
    return;
  }
  // Shared X write-path helpers: txid ctx + web session bind (x-reply / x-follow).
  // Installs abxEnsureAuthCtx + abxB64keep / abxDerEncode / abxAuthHeaders / abxTxidGen.

const b64keep = (bytes) => {
  const A = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';
  let out = '';
  for (let i = 0; i < bytes.length; i += 3) {
    const b0 = bytes[i], b1 = bytes[i + 1], b2 = bytes[i + 2];
    out += A[b0 >> 2];
    out += A[((b0 & 3) << 4) | (b1 !== undefined ? b1 >> 4 : 0)];
    out += b1 !== undefined ? A[((b1 & 15) << 2) | (b2 !== undefined ? b2 >> 6 : 0)] : '=';
    out += b2 !== undefined ? A[b2 & 63] : '=';
  }
  return out;
};
const derEncode = (r, s) => {
  const enc = (big) => {
    const bytes = P256.bytesBE(big, 32);
    let t = 0; while (t < bytes.length - 1 && bytes[t] === 0) t++;
    const z = bytes.slice(t);
    if (z[0] & 128) return [2, z.length + 1, 0, ...z];
    return [2, z.length, ...z];
  };
  const rE = enc(r), sE = enc(s);
  return new Uint8Array([48, rE.length + sE.length, ...rE, ...sE]);
};
function solve(value, minVal, maxVal, rounding) {
  const result = (value * (maxVal - minVal)) / 255 + minVal;
  return rounding ? Math.floor(result) : Math.round(result * 100) / 100;
}
function isOdd(num) { return num % 2 ? -1.0 : 0.0; }
class Cubic {
  constructor(curves) { this.curves = curves; }
  calculate(a, b, m) { return (3.0 * a * (1 - m) * (1 - m) * m + 3.0 * b * (1 - m) * m * m + m * m * m); }
  getValue(time) {
    let startGradient = 0, endGradient = 0, start = 0.0, mid = 0.0, end = 1.0;
    if (time <= 0.0) {
      if (this.curves[0] > 0.0) startGradient = this.curves[1] / this.curves[0];
      else if (this.curves[1] === 0.0 && this.curves[2] > 0.0) startGradient = this.curves[3] / this.curves[2];
      return startGradient * time;
    }
    if (time >= 1.0) {
      if (this.curves[2] < 1.0) endGradient = (this.curves[3] - 1.0) / (this.curves[2] - 1.0);
      else if (this.curves[2] === 1.0 && this.curves[0] < 1.0) endGradient = (this.curves[1] - 1.0) / (this.curves[0] - 1.0);
      return 1.0 + endGradient * (time - 1.0);
    }
    while (start < end) {
      mid = (start + end) / 2;
      const xEst = this.calculate(this.curves[0], this.curves[2], mid);
      if (Math.abs(time - xEst) < 0.00001) return this.calculate(this.curves[1], this.curves[3], mid);
      if (xEst < time) start = mid; else end = mid;
    }
    return this.calculate(this.curves[1], this.curves[3], mid);
  }
}
function interpolate(fromList, toList, f) {
  const o = [];
  for (let i = 0; i < fromList.length; i++) {
    const a = fromList[i], b = toList[i];
    o.push(typeof a === 'number' && typeof b === 'number' ? a * (1 - f) + b * f : 0);
  }
  return o;
}
function convertRotationToMatrix(rotation) {
  const rad = (rotation * Math.PI) / 180;
  return [Math.cos(rad), -Math.sin(rad), Math.sin(rad), Math.cos(rad)];
}
function floatToHex(x) {
  const result = [];
  let quotient = Math.floor(x);
  let fraction = x - quotient;
  while (quotient > 0) {
    quotient = Math.floor(x / 16);
    const remainder = Math.floor(x - quotient * 16);
    if (remainder > 9) result.unshift(String.fromCharCode(remainder + 55));
    else result.unshift(remainder.toString());
    x = quotient;
  }
  if (fraction === 0) return result.join('');
  result.push('.');
  let guard = 0;
  while (fraction > 0 && guard++ < 60) {
    fraction *= 16;
    const integer = Math.floor(fraction);
    fraction -= integer;
    if (integer > 9) result.push(String.fromCharCode(integer + 55));
    else result.push(integer.toString());
  }
  return result.join('');
}
function animate(frames, targetTime) {
  const fromColor = frames.slice(0, 3).concat(1).map(Number);
  const toColor = frames.slice(3, 6).concat(1).map(Number);
  const toRotation = [solve(frames[6], 60.0, 360.0, true)];
  const curves = frames.slice(7).map((item, counter) => solve(item, isOdd(counter), 1.0, false));
  const cubic = new Cubic(curves);
  const val = cubic.getValue(targetTime);
  const color = interpolate(fromColor, toColor, val).map(v => v > 0 ? v : 0);
  const rotation = interpolate([0.0], toRotation, val);
  const matrix = convertRotationToMatrix(rotation[0]);
  const strArr = color.slice(0, -1).map(v => Math.round(v).toString(16));
  for (const value of matrix) {
    let rounded = Math.round(value * 100) / 100;
    if (rounded < 0) rounded = -rounded;
    const hexValue = floatToHex(rounded);
    strArr.push(hexValue.startsWith('.') ? ('0' + hexValue).toLowerCase() : hexValue || '0');
  }
  strArr.push('0', '0');
  return strArr.join('').replace(/[.-]/g, '');
}
function animKeyFromCtx(ctx) {
  const keyBytes = Array.from(atob(ctx.key), c => c.charCodeAt(0));
  const frameIdx = keyBytes[5] % 4;
  const arr = ctx.frames[frameIdx].substring(9).split('C').map(item => {
    const cleaned = item.replace(/[^\d]+/g, ' ').trim();
    const parts = cleaned === '' ? [] : cleaned.split(/\s+/);
    return parts.map(s => parseInt(s, 10));
  });
  const rowIndex = keyBytes[41] % 16;
  let frameTime = [22, 19, 46].reduce((n1, n2) => n1 * (keyBytes[n2] % 16), 1);
  frameTime = Math.round(frameTime / 10) * 10;
  return animate(arr[rowIndex], frameTime / 4096);
}
function keyBytesOf(ctx) { return Array.from(atob(ctx.key), c => c.charCodeAt(0)); }
async function txidGen(method, path, ctx, animationKey) {
  const keyBytes = keyBytesOf(ctx);
  const timeNow = Math.floor((Date.now() - 1682924400 * 1000) / 1000);
  const timeNowBytes = [timeNow & 0xff, (timeNow >> 8) & 0xff, (timeNow >> 16) & 0xff, (timeNow >> 24) & 0xff];
  const data = method + '!' + path + '!' + timeNow + 'obfiowerehiring' + animationKey;
  const buf = await crypto.subtle.digest('SHA-256', new TextEncoder().encode(data));
  const hashBytes = Array.from(new Uint8Array(buf));
  const randomNum = Math.floor(Math.random() * 256);
  const bytesArr = [...keyBytes, ...timeNowBytes, ...hashBytes.slice(0, 16), 3];
  const u8 = new Uint8Array([randomNum, ...bytesArr.map(b => b ^ randomNum)]);
  return b64keep(u8).replace(/=/g, '');
}
const authHeaders = () => {
  const bearer = localStorage.getItem('abx_bearer');
  const ct0 = (document.cookie.match(/(?:^|;\s*)ct0=([^;]+)/) || [])[1];
  if (!bearer || !ct0) throw new Error('missing bearer/ct0 - session not logged in');
  return {
    'content-type': 'application/json',
    'authorization': 'Bearer ' + bearer,
    'x-twitter-auth-type': 'OAuth2Session',
    'x-twitter-active-user': 'yes',
    'x-csrf-token': ct0,
    'x-twitter-client-language': 'en'
  };
};
  async function abxEnsureAuthCtx() {
  const out = {};
  try {
    // ---- ctx: cached (72h) -> DOM -> /home fallback
    let ctx = null;
    try { ctx = JSON.parse(localStorage.getItem('abx_txid_ctx') || 'null'); } catch {}
    if (!(ctx && ctx.key && Array.isArray(ctx.frames) && ctx.frames.length === 4 && (Date.now() - (ctx.at || 0)) < 72 * 3600e3)) {
      let meta = document.querySelector("meta[name='twitter-site-verification']");
      let key = meta ? meta.getAttribute('content') : null;
      const domHtml = document.documentElement.outerHTML;
      let svgs = [...domHtml.matchAll(/<svg[^>]*id="loading-x-anim-(\d)"[^>]*>([\s\S]*?)<\/svg>/g)];
      if (!key || svgs.length !== 4) {
        const t = await (await fetch('/home', {credentials: 'include'})).text();
        const mk = t.match(/<meta[^>]*name="twitter-site-verification"[^>]*content="([^"]+)"/);
        if (!key && mk) key = mk[1];
        svgs = [...t.matchAll(/<svg[^>]*id="loading-x-anim-(\d)"[^>]*>([\s\S]*?)<\/svg>/g)];
        out.ctx_src = 'home';
      } else out.ctx_src = 'dom';
      if (key && svgs.length === 4) {
        ctx = {key, frames: svgs.map(m => [...m[2].matchAll(/ d="([^"]*)"/g)].map(x => x[1])[1]), at: Date.now()};
        out.ctx_new = true;
      }
    } else out.ctx_src = 'cached';
    if (!ctx || !ctx.key || !ctx.frames || ctx.frames.some(f => !f)) {
      return JSON.stringify({ok: false, step: 'ctx', ...out, err: 'no txid ctx (needs x.com page or reachable /home)'});
    }
    if (!ctx.animationKey) {
      ctx.animationKey = animKeyFromCtx(ctx);
      out.animKey_len = ctx.animationKey.length;
    }
    localStorage.setItem('abx_txid_ctx', JSON.stringify(ctx));
    // ---- binding: fresh (5min margin) -> establish
    let bind = null;
    try { bind = JSON.parse(localStorage.getItem('abx_bind') || 'null'); } catch {}
    if (bind && bind.tokenhash && Number(bind.expiresAt) > Date.now() + 300e3 && bind.d && bind.Qx && bind.Qy) {
      return JSON.stringify({ok: true, ...out, bind: 'cached', expires_in_min: Math.round((bind.expiresAt - Date.now()) / 60000)});
    }
    if (typeof P256 === 'undefined') return JSON.stringify({ok: false, ...out, err: 'P256 missing - run p256 step first'});
    const chResp = await fetch('/i/session_binding/challenge', {cache: 'no-store', credentials: 'same-origin', headers: {accept: 'application/json'}});
    if (!chResp.ok) return JSON.stringify({ok: false, ...out, step: 'challenge', status: chResp.status});
    const challenge = (await chResp.json()).challenge;
    if (!challenge) return JSON.stringify({ok: false, ...out, step: 'challenge', err: 'no challenge field'});
    const kp = P256.genKeyPair();
    const publicKeyHash = b64keep(P256.spki(kp.Q));
    const bindPath = '/i/api/graphql/0qFmnKTY3JwBZnqDdQrtHw/CreateWebSessionBind';
    const headers = authHeaders();
    try { headers['x-client-transaction-id'] = await txidGen('POST', bindPath, ctx, ctx.animationKey); } catch (e) { out.txid_skip = String(e).slice(0, 80); }
    const bindResp = await fetch(bindPath, {
      method: 'POST', credentials: 'include', headers,
      body: JSON.stringify({variables: JSON.stringify({publicKeyHash, sessionTokenhash: challenge}), queryId: '0qFmnKTY3JwBZnqDdQrtHw'})
    });
    const bindText = await bindResp.text();
    let bindJson = null; try { bindJson = JSON.parse(bindText); } catch {}
    const node = bindJson && bindJson.data && bindJson.data.create_web_session_bind;
    if (!bindResp.ok || !node || node.success !== true) {
      return JSON.stringify({ok: false, ...out, step: 'bind', status: bindResp.status, body: bindText.slice(0, 200)});
    }
    localStorage.setItem('abx_bind', JSON.stringify({
      d: kp.d.toString(16), Qx: kp.Q.x.toString(16), Qy: kp.Q.y.toString(16),
      tokenhash: challenge, expiresAt: node.expires_at, mintedAt: Date.now()
    }));
    return JSON.stringify({ok: true, ...out, bind: 'established', expires_at: node.expires_at});
  } catch (e) {
    return JSON.stringify({ok: false, ...out, err: e.name + ': ' + e.message});
  }
  }

  if (typeof globalThis !== "undefined") {
    globalThis.abxB64keep = b64keep;
    globalThis.abxDerEncode = derEncode;
    globalThis.abxAuthHeaders = authHeaders;
    globalThis.abxTxidGen = txidGen;
    globalThis.abxEnsureAuthCtx = abxEnsureAuthCtx;
  }
})();
