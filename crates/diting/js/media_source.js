// canPlayType + MSE (MediaSource / SourceBuffer) shims.
// Split out of bootstrap.js wholesale (god-file ratchet, pure move): top level only
// declares bindings; every bootstrap helper it names is touched at call time.

// Chrome 145 (macOS, proprietary codecs) answers, measured as a 1068-string
// matrix against a real Chrome: a container knows a set of codec families, a
// type is "probably" when every listed codec is in its set, "maybe" for a
// known container with no (or an empty) codec list, "" otherwise. Feature
// probes hash these answers, so the whole table has to be right, not just
// the common rows.
const _CODEC_FAMILY = (c) => {
  c = c.trim().toLowerCase();
  if (/^avc[13]\.[0-9a-f]{6}$/.test(c)) return 'avc';
  if (/^mp4a\.40\.(2|5)$/.test(c)) return 'aac';
  if (/^mp4a\.(69|6b)$/.test(c)) return 'mp3a';
  if (/^(hev1|hvc1)\./.test(c)) return 'hevc';
  if (/^av01\./.test(c)) return 'av1';
  if (/^vp09\./.test(c)) return 'vp09';
  if (c === 'vp9') return 'vp9';
  if (c === 'vp8' || c === 'vp8.0') return 'vp8';
  if (c === 'vorbis' || c === 'opus' || c === 'flac' || c === 'mp3') return c;
  return '';
};
const _CPT_ALL = ['avc', 'aac', 'mp3a', 'hevc', 'av1', 'vp09', 'opus', 'flac', 'mp3'];
const _CPT_MIME = {
  'video/mp4': _CPT_ALL,
  'video/webm': ['av1', 'vp8', 'vp9', 'vp09', 'vorbis', 'opus'],
  'video/ogg': ['vp8', 'vorbis', 'opus', 'flac'],
  'video/x-matroska': ['avc', 'aac', 'mp3a', 'hevc', 'av1', 'vp8', 'vp09', 'vorbis', 'opus', 'flac', 'mp3'],
  'video/3gpp': ['avc', 'aac'],
  'video/x-m4v': ['avc', 'aac'],
  'audio/mp4': ['aac', 'mp3a', 'opus', 'flac', 'mp3'],
  'audio/mpeg': ['mp3a', 'mp3'],
  'audio/webm': ['vorbis', 'opus'],
  'audio/ogg': ['vorbis', 'opus', 'flac'],
  'audio/x-m4a': ['aac'],
  'audio/mpegurl': ['avc', 'aac', 'mp3a', 'mp3'],
  'audio/x-mpegurl': ['avc', 'aac', 'mp3a', 'mp3'],
  'audio/wav': [], 'audio/x-wav': [],
};
const _CPT_BARE_PROBABLY = new Set(['audio/mpeg', 'audio/mp3', 'audio/flac', 'audio/aac']);
function _parseMediaType(type) {
  const parts = String(type).split(';');
  const mime = parts[0].trim().toLowerCase();
  let codecs = null;
  for (let i = 1; i < parts.length; i++) {
    if (/^\s*codecs\s*=/i.test(parts[i]) && !/^\s*codecs\s*=/.test(parts[i])) return { mime, codecs: null, bad: true };
    const m = /^\s*codecs\s*=\s*(?:"([^"]*)"|(.*?))\s*$/.exec(parts[i]);
    if (m) { codecs = (m[1] !== undefined ? m[1] : m[2]).split(',').map((x) => x.trim()).filter(Boolean); break; }
  }
  // A "codecs" parameter that is present but empty still counts as "none given".
  return { mime, codecs };
}
function _cptAnswer(type) {
  const { mime, codecs, bad } = _parseMediaType(type);
  if (bad) return '';
  const known = Object.prototype.hasOwnProperty.call(_CPT_MIME, mime);
  if (!codecs || !codecs.length) {
    if (_CPT_BARE_PROBABLY.has(mime)) return 'probably';
    return known ? 'maybe' : '';
  }
  if (mime === 'audio/mp3') return '';
  if (!known) return '';
  const ok = _CPT_MIME[mime];
  return codecs.every((c) => ok.indexOf(_CODEC_FAMILY(c)) >= 0 && _CODEC_FAMILY(c) !== '') ? 'probably' : '';
}
// Chrome's home is HTMLMediaElement.prototype; fall back to Element only when
// the media interfaces are not defined yet at this point of the bundle.
(globalThis.HTMLMediaElement ? globalThis.HTMLMediaElement.prototype : Element.prototype).canPlayType =
  function canPlayType(type) { return _cptAnswer(type); };
_markNative((globalThis.HTMLMediaElement ? globalThis.HTMLMediaElement.prototype : Element.prototype).canPlayType);

// ---- MSE (MediaSource) surface ----------------------------------------
//
// DASH/HLS players gate their whole mount on
// `window.MediaSource && MediaSource.isTypeSupported(...)` — bilibili's
// nano player leaves the player docker EMPTY without it (its /video/ pages
// are dash-only, no durl fallback). This stub satisfies the capability
// probes and runs the attach handshake (createObjectURL → video.src =
// blob:… → async 'sourceopen'); SourceBuffers accept appended segments and
// drop the bytes. Playback itself is not simulated — the goal is player
// DOM/UI mounting for automation; pulling the real stream stays the
// sniffer's job (session_network filter=media).
function _mediaRanges(pairs) {
  return {
    length: pairs.length,
    start(i) {
      i = i >>> 0;
      if (i >= pairs.length) throw new DOMException("Failed to execute 'start' on 'TimeRanges': The index provided (" + i + ") is greater than or equal to the maximum bound (" + pairs.length + ").", "IndexSizeError");
      return pairs[i][0];
    },
    end(i) {
      i = i >>> 0;
      if (i >= pairs.length) throw new DOMException("Failed to execute 'end' on 'TimeRanges': The index provided (" + i + ") is greater than or equal to the maximum bound (" + pairs.length + ").", "IndexSizeError");
      return pairs[i][1];
    },
  };
}
const _EMPTY_RANGES = _mediaRanges([]);

// Minimal ISO-BMFF (fMP4) walker — enough to derive real buffered ranges
// from the bytes a page appends, the way Chrome's demuxer does. Appends feed
// us init segments (moov: per-track timescale + trex default durations) then
// media segments (moof: tfdt base time + trun sample durations). WebM is not
// parsed — its appends just leave buffered untouched.
function _mp4BoxIter(u8, start, end, fn) {
  let p = start;
  while (p + 8 <= end) {
    let size = (u8[p] << 24) | (u8[p + 1] << 16) | (u8[p + 2] << 8) | u8[p + 3];
    const type = String.fromCharCode(u8[p + 4], u8[p + 5], u8[p + 6], u8[p + 7]);
    let body = p + 8;
    if (size === 1) {
      if (p + 16 > end) break;
      size = 0;
      for (let k = 8; k < 16; k++) size = size * 256 + u8[p + k];
      body = p + 16;
    } else if (size === 0) {
      size = end - p;
    }
    if (size < 8 || p + size > end) break;
    if (fn(type, body, p + size) === false) return;
    p += size;
  }
}
function _mp4Scan(u8, knownTracks) {
  const out = { initTracks: {}, segments: [] };
  if (knownTracks) for (const id of Object.keys(knownTracks)) out.initTracks[id] = knownTracks[id];
  // bilibili's moov puts mvex BEFORE trak, so a trex can arrive before its
  // track exists — park the defaults and apply at trak registration.
  const pendingTrex = {};
  _mp4BoxIter(u8, 0, u8.length, (type, body, boxEnd) => {
    if (type === 'moov') {
      _mp4BoxIter(u8, body, boxEnd, (t2, b2, e2) => {
        if (t2 === 'trak') {
          let id = 0, timescale = 0;
          _mp4BoxIter(u8, b2, e2, (t3, b3, e3) => {
            if (t3 === 'tkhd' && b3 + 16 <= e3) {
              const v = u8[b3];
              id = (u8[b3 + (v ? 20 : 12)] << 24) | (u8[b3 + (v ? 21 : 13)] << 16) | (u8[b3 + (v ? 22 : 14)] << 8) | u8[b3 + (v ? 23 : 15)];
            } else if (t3 === 'mdia') {
              _mp4BoxIter(u8, b3, e3, (t4, b4, e4) => {
                if (t4 === 'mdhd') {
                  const v = u8[b4];
                  // v0: timescale u32 @12; v1: u32 @20 (creation/mod are u64)
                  if (b4 + (v ? 24 : 16) <= e4) {
                    timescale = (u8[b4 + (v ? 20 : 12)] << 24) | (u8[b4 + (v ? 21 : 13)] << 16) | (u8[b4 + (v ? 22 : 14)] << 8) | u8[b4 + (v ? 23 : 15)];
                  }
                }
              });
            }
          });
          if (id && timescale) out.initTracks[id] = { timescale, defaultDur: pendingTrex[id] || 0 };
        } else if (t2 === 'mvex') {
          _mp4BoxIter(u8, b2, e2, (t3, b3, e3) => {
            if (t3 === 'trex' && b3 + 16 <= e3) {
              // ver/flags, track_ID @4, description_index @8, default_duration @12
              const tid = (u8[b3 + 4] << 24) | (u8[b3 + 5] << 16) | (u8[b3 + 6] << 8) | u8[b3 + 7];
              const dur = (u8[b3 + 12] << 24) | (u8[b3 + 13] << 16) | (u8[b3 + 14] << 8) | u8[b3 + 15];
              if (out.initTracks[tid]) out.initTracks[tid].defaultDur = dur;
              else pendingTrex[tid] = dur;
            }
          });
        }
      });
    } else if (type === 'moof') {
      _mp4BoxIter(u8, body, boxEnd, (t2, b2, e2) => {
        if (t2 !== 'traf') return;
        let trackId = 0, tfdt = null, tfhdDur = 0, trun = null;
        _mp4BoxIter(u8, b2, e2, (t3, b3, e3) => {
          if (t3 === 'tfhd' && b3 + 8 <= e3) {
            let q = b3 + 4;
            // FullBox flags are the 3 bytes AFTER the version byte.
            const flags = (u8[b3 + 1] << 16) | (u8[b3 + 2] << 8) | u8[b3 + 3];
            if (flags & 0x01) q += 8;
            if (flags & 0x02) q += 4;
            if (q + 4 <= e3) trackId = (u8[q] << 24) | (u8[q + 1] << 16) | (u8[q + 2] << 8) | u8[q + 3];
            // 0x08 default_sample_duration sits right after track_ID
            if ((flags & 0x08) && q + 8 <= e3) {
              tfhdDur = (u8[q + 4] << 24) | (u8[q + 5] << 16) | (u8[q + 6] << 8) | u8[q + 7];
            }
          } else if (t3 === 'tfdt' && b3 + 8 <= e3) {
            const v = u8[b3];
            tfdt = 0;
            const n = v ? 8 : 4;
            for (let k = 4; k < 4 + n; k++) tfdt = tfdt * 256 + u8[b3 + k];
          } else if (t3 === 'trun' && b3 + 8 <= e3) {
            const v = u8[b3];
            const flags = (u8[b3 + 1] << 16) | (u8[b3 + 2] << 8) | u8[b3 + 3];
            const count = (u8[b3 + 4] << 24) | (u8[b3 + 5] << 16) | (u8[b3 + 6] << 8) | u8[b3 + 7];
            let q = b3 + 8;
            if (flags & 0x01) q += 4;
            if (flags & 0x04) q += 4;
            let total = 0;
            const perSample = (flags & 0x100) !== 0;
            for (let s = 0; s < count && q + 4 <= e3; s++) {
              if (perSample) {
                total += (u8[q] << 24) | (u8[q + 1] << 16) | (u8[q + 2] << 8) | u8[q + 3];
                q += 4;
              }
              if (flags & 0x200) q += 4;
              if (flags & 0x400) q += 4;
              if (flags & 0x800) q += (v ? 8 : 4);
            }
            trun = { count, perSample, total };
          }
        });
        const track = trackId ? (out.initTracks[trackId] || null) : null;
        // Prefer remembered init config; a segment before its init is a no-op.
        if (!track || tfdt == null) return;
        const def = tfhdDur || track.defaultDur;
        let durUnits = 0;
        if (trun && trun.perSample) durUnits = trun.total;
        else if (trun && def) durUnits = def * trun.count;
        if (durUnits > 0) {
          out.segments.push({ trackId, start: tfdt / track.timescale, end: (tfdt + durUnits) / track.timescale });
        }
      });
    }
  });
  return out;
}

class _SourceBufferList extends Array {
  item(i) { return this[i] || null; }
}

// MSE objects are EventTargets but NOT Nodes — Chrome says
// `new MediaSource() instanceof Node` is false. The global EventTarget in
// this realm is the DOM Node stand-in (see the binding block near
// Document/Element), so extending it would both route dispatch through the
// tree-flavored path (which silently no-ops on nid-less objects — the
// 'sourceopen' handshake never fired) and make instanceof Node true. A
// self-contained registry keeps the surface right; same shape
// XMLHttpRequestEventTarget uses. The on<type> property dispatch is
// load-bearing here: onsourceopen/onsourceended and SourceBuffer's
// onupdate/onupdateend/onerror/onabort are standard IDL attributes.
class _MseEventTarget {
  addEventListener(type, handler) {
    __listenerGate(handler);
    const L = __lmap(this);
    (L[type] || (L[type] = [])).push(handler);
  }
  removeEventListener(type, handler) {
    const L = __evtStore.get(this);
    if (L && L[type]) {
      L[type] = L[type].filter(h => h !== handler);
    }
  }
  dispatchEvent(event) {
    if (!event || !event.type) return false;
    const ev = (typeof event === 'object') ? event : { type: event };
    ev.target = ev.target || this;
    ev.currentTarget = ev.currentTarget || this;
    const L = __evtStore.get(this);
    const handlers = (L && L[ev.type]) || [];
    for (const h of handlers) { try { _invokeListener(h, this, ev); } catch (e) { globalThis.__diting_reportUncaughtError(e); } }
    const prop = 'on' + ev.type;
    if (typeof this[prop] === 'function') {
      try { this[prop](ev); } catch (e) {}
    }
    return true;
  }
}
_markNative(_MseEventTarget);
_markNative(_MseEventTarget.prototype.addEventListener);
_markNative(_MseEventTarget.prototype.removeEventListener);
_markNative(_MseEventTarget.prototype.dispatchEvent);

globalThis.SourceBuffer = class SourceBuffer extends _MseEventTarget {
  constructor(ms, mime) {
    super();
    this._ms = ms;
    this.mimeType = mime;
    this.mode = 'segments';
    this.updating = false;
    this.timestampOffset = 0;
    this.appendWindowStart = 0;
    this.appendWindowEnd = Infinity;
    this._ranges = [];
    this._tracks = {};
    __hideOwn(this);
  }
  get buffered() {
    // Chrome hands out a fresh TimeRanges object per access.
    return _mediaRanges(this._ranges.map(r => [r[0], r[1]]));
  }
  appendBuffer(data) {
    if (this._ms.readyState !== 'open' || this.updating) {
      throw new DOMException("Failed to execute 'appendBuffer' on 'SourceBuffer': this SourceBuffer cannot be appended to right now.", 'InvalidStateError');
    }
    if (data == null) {
      throw new TypeError("Failed to execute 'appendBuffer' on 'SourceBuffer': 1 argument required, but only 0 present.");
    }
    const bytes = data.buffer ? new Uint8Array(data.buffer, data.byteOffset || 0, data.byteLength) : new Uint8Array(data);
    this.updating = true;
    // Chrome completes appends off-thread; a microtask keeps the
    // update/updateend contract without starving the player's loop.
    queueMicrotask(() => {
      this.updating = false;
      this._applyAppend(bytes);
      this.dispatchEvent(new Event('update'));
      this.dispatchEvent(new Event('updateend'));
      this._ms._afterAppend();
    });
  }
  // Derive real buffered ranges from the appended bytes (moov seeds track
  // config; moof frames carry tfdt base + durations). Unparseable bytes are
  // a silent no-op — same as Chrome ignoring a partial box.
  _applyAppend(bytes) {
    if (!bytes || bytes.length < 8) return;
    let scan;
    try { scan = _mp4Scan(bytes, this._tracks); } catch (_) { return; }
    for (const id of Object.keys(scan.initTracks)) {
      this._tracks[id] = scan.initTracks[id];
    }
    for (const seg of scan.segments) {
      const merged = this._ranges.filter(r => !(r[1] < seg.start - 0.001 || r[0] > seg.end + 0.001));
      const start = Math.min(seg.start, ...merged.map(r => r[0]), seg.start);
      const end = Math.max(seg.end, ...merged.map(r => r[1]), seg.end);
      this._ranges = this._ranges.filter(r => merged.indexOf(r) === -1);
      this._ranges.push([start, end]);
      this._ranges.sort((a, b) => a[0] - b[0]);
    }
  }
  remove(start, end) {
    if (this._ms.readyState !== 'open' || this.updating) {
      throw new DOMException("Failed to execute 'remove' on 'SourceBuffer'.", 'InvalidStateError');
    }
    this.updating = true;
    const s = Number(start) || 0, e = Number(end) || 0;
    queueMicrotask(() => {
      this.updating = false;
      this._ranges = this._ranges.filter(r => r[1] <= s + 0.001 || r[0] >= e - 0.001);
      this.dispatchEvent(new Event('update'));
      this.dispatchEvent(new Event('updateend'));
    });
  }
  changeType(mime) { this.mimeType = String(mime); }
  abort() {
    if (this.updating) {
      this.updating = false;
      this.dispatchEvent(new Event('abort'));
      this.dispatchEvent(new Event('updateend'));
    }
  }
};

globalThis.MediaSource = class MediaSource extends _MseEventTarget {
  constructor() {
    super();
    this._explicitDuration = undefined;
    this._mediaElement = null;
    this.sourceBuffers = new _SourceBufferList();
    this.activeSourceBuffers = new _SourceBufferList();
    __hideOwn(this);
  }
  get readyState() { return this._readyState || 'closed'; }
  set readyState(v) { __def(this, '_readyState', v); }
  // Spec: with no explicit duration set, duration is the highest end
  // timestamp across source buffers.
  get duration() {
    if (this._explicitDuration !== undefined) return this._explicitDuration;
    let max = 0;
    for (const sb of this.sourceBuffers) {
      if (sb._ranges.length) max = Math.max(max, sb._ranges[sb._ranges.length - 1][1]);
    }
    return max > 0 ? max : NaN;
  }
  set duration(v) {
    const d = Number(v);
    if (this.readyState !== 'open') {
      throw new DOMException("Failed to set the 'duration' property on 'MediaSource': The MediaSource's readyState is not 'open'.", 'InvalidStateError');
    }
    this._explicitDuration = d;
  }
  static isTypeSupported(type) {
    if (arguments.length === 0) {
      throw new TypeError("Failed to execute 'isTypeSupported' on 'MediaSource': 1 argument required, but only 0 present.");
    }
    // Same codec table as canPlayType, but MSE wants an explicit codec list
    // for mp4/webm (Chrome: bare "video/mp4" is false) and accepts audio/mpeg.
    const { mime, codecs } = _parseMediaType(type);
    if (mime === 'audio/mpeg') return !codecs || !codecs.length || _cptAnswer(type) === 'probably';
    if (!/^(?:audio|video)\/(?:mp4|webm)$/.test(mime)) return false;
    if (!codecs || !codecs.length) return false;
    return _cptAnswer(type) === 'probably';
  }
  addSourceBuffer(mime) {
    if (this.readyState !== 'open') {
      throw new DOMException("Failed to execute 'addSourceBuffer' on 'MediaSource': The MediaSource object is not attached", 'InvalidStateError');
    }
    const sb = new globalThis.SourceBuffer(this, String(mime));
    this.sourceBuffers.push(sb);
    return sb;
  }
  removeSourceBuffer(sb) {
    const i = this.sourceBuffers.indexOf(sb);
    if (i === -1) {
      throw new DOMException("Failed to execute 'removeSourceBuffer' on 'MediaSource': The SourceBuffer provided is not contained in this MediaSource's sourceBuffers attribute.", 'NotFoundError');
    }
    this.sourceBuffers.splice(i, 1);
  }
  endOfStream() {
    if (this.readyState === 'closed') {
      throw new DOMException("Failed to execute 'endOfStream' on 'MediaSource': The MediaSource object is not attached", 'InvalidStateError');
    }
    if (this.readyState === 'open') {
      this.readyState = 'ended';
      this.dispatchEvent(new Event('sourceended'));
    }
  }
  setLiveSeekableRange() {}
  clearLiveSeekableRange() {}
  // Attach handshake: a media element picked up our blob: URL. Chrome
  // flips readyState synchronously and runs the resource selection
  // algorithm on a task — 'loadstart' on the element, then 'sourceopen'
  // here; players build their SourceBuffers inside that handler.
  _attachToElement(el) {
    if (this.readyState !== 'closed') return;
    this._mediaElement = el;
    el._mediaSource = this;
    el._mediaNetworkState = 2; // NETWORK_LOADING
    this.readyState = 'open';
    setTimeout(() => {
      el.dispatchEvent(new Event('loadstart'));
      this.dispatchEvent(new Event('sourceopen'));
    }, 0);
  }
  // Union of every SourceBuffer's ranges — what the attached element can
  // read. Coalesces touching/overlapping ranges (0.001s tolerance, same
  // as append-time merging).
  _unionBuffered() {
    const all = [];
    for (const sb of this.sourceBuffers) {
      for (const r of sb._ranges) all.push(r);
    }
    if (!all.length) return [];
    all.sort((a, b) => a[0] - b[0]);
    const merged = [[all[0][0], all[0][1]]];
    for (let i = 1; i < all.length; i++) {
      const last = merged[merged.length - 1];
      if (all[i][0] <= last[1] + 0.001) {
        if (all[i][1] > last[1]) last[1] = all[i][1];
      } else {
        merged.push([all[i][0], all[i][1]]);
      }
    }
    return merged;
  }
  // Readiness ladder: every completed append re-derives the element's
  // readyState from the union of buffered data, firing the same ladder
  // Chrome's demuxer does as fMP4 bytes land — loadedmetadata (HAVE_METADATA),
  // loadeddata (2), canplay (3), canplaythrough (4) — plus 'progress' and
  // 'durationchange' when the derived duration moves. This is the mount
  // signal bilibili's nano player parks on: player.connect() resolves a
  // deferred from onCanplay, and no video ever mounts without it.
  _afterAppend() {
    const el = this._mediaElement;
    if (!el) return;
    const merged = this._unionBuffered();
    if (!merged.length) return;
    const ct = el.currentTime || 0;
    // How far playable data extends from the playhead, if the playhead
    // sits inside buffered data at all.
    let aheadEnd = null;
    for (const r of merged) {
      if (ct >= r[0] - 0.001 && ct < r[1]) { aheadEnd = r[1]; break; }
      if (r[0] > ct) break;
    }
    let ready = 1; // HAVE_METADATA — some data parsed
    if (aheadEnd !== null) {
      ready = 2; // HAVE_CURRENT_DATA
      if (aheadEnd > ct + 0.05) ready = 3; // HAVE_FUTURE_DATA
      const enough = Number.isFinite(this.duration) ? Math.min(this.duration, ct + 30) : ct + 30;
      if (aheadEnd >= enough - 0.001) ready = 4; // HAVE_ENOUGH_DATA
    }
    const prev = el._mediaReadyState || 0;
    el._mediaReadyState = ready;
    el._mediaNetworkState = ready >= 4 ? 1 : 2; // NETWORK_IDLE : NETWORK_LOADING
    el.dispatchEvent(new Event('progress'));
    const dur = el.duration;
    if (!Number.isNaN(dur) && dur !== el._lastReportedDuration) {
      el._lastReportedDuration = dur;
      el.dispatchEvent(new Event('durationchange'));
    }
    if (prev < 1) el.dispatchEvent(new Event('loadedmetadata'));
    if (prev < 2 && ready >= 2) el.dispatchEvent(new Event('loadeddata'));
    if (prev < 3 && ready >= 3) el.dispatchEvent(new Event('canplay'));
    if (prev < 4 && ready >= 4) el.dispatchEvent(new Event('canplaythrough'));
  }
};
_markNative(globalThis.MediaSource);
_markNative(globalThis.MediaSource.isTypeSupported);
