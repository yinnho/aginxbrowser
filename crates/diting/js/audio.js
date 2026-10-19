// Web Audio / speechSynthesis faces.
// Split out of bootstrap.js wholesale (god-file ratchet, pure move): top level only
// declares bindings; every bootstrap helper it names is touched at call time.

function _audioParam(value, min, max) {
  const FLT_MAX = 3.4028234663852886e38;
  return {
    value, defaultValue: value,
    minValue: min === undefined ? -FLT_MAX : min,
    maxValue: max === undefined ? FLT_MAX : max,
    setValueAtTime(v) { this.value = v; return this; },
    linearRampToValueAtTime() { return this; },
    exponentialRampToValueAtTime() { return this; },
    setTargetAtTime() { return this; },
    setValueCurveAtTime() { return this; },
    cancelScheduledValues() { return this; },
    cancelAndHoldAtTime() { return this; },
  };
}
const _audioNodeBase = {
  connect(dest) {
    try {
      const ctx = this.context;
      if (ctx) {
        const b = _ctxBag(ctx);
        b.__graph = true;
        b.__nodes = b.__nodes || [];
        b.__nodes.push(this);
        if (dest === ctx.destination || (dest && dest === _ctxBag(ctx).destination)) {
          b.__toDest = true;
        }
      }
    } catch (e) {}
    return dest || this;
  },
  disconnect(){},
  addEventListener(){}, removeEventListener(){},
  channelCount: 2, channelCountMode: 'max', channelInterpretation: 'speakers', numberOfInputs: 1, numberOfOutputs: 1
};
// Every AudioNode exposes `.context` back to its AudioContext — fingerprint
// probes read `analyser.context.sampleRate`.
function _audioNode(ctx, extra) { return Object.assign({ context: ctx }, _audioNodeBase, extra); }
// Real AudioBuffer: detectors check `"copyFromChannel" in AudioBuffer.prototype`
// (ReferenceError if the global is missing) and verify copyFromChannel copies
// what getChannelData exposes — one backing Float32Array per channel.
// (300015 P4) Members in Chrome's exact order, state in a WeakMap bag so
// instances enumerate with ZERO own properties like Chrome's.
const _abBag = new WeakMap();
globalThis.AudioBuffer = class AudioBuffer {
  constructor(options) {
    const o = options || {};
    const nc = Math.max(1, o.numberOfChannels || 1);
    const len = Math.max(0, (o.length | 0) || 0);
    const chs = [];
    for (let i = 0; i < nc; i++) {
      chs.push(o.channelData && o.channelData[i]
        ? Float32Array.from(o.channelData[i])
        : new Float32Array(len));
    }
    _abBag.set(this, { ch: chs, len: len, sr: o.sampleRate || 44100 });
  }
  get length() { return _abBag.get(this).len; }
  get duration() { const b = _abBag.get(this); return b.len / b.sr; }
  get sampleRate() { return _abBag.get(this).sr; }
  get numberOfChannels() { return _abBag.get(this).ch.length; }
  copyFromChannel(dest, channel, bufferOffset) {
    const src = this.getChannelData(channel);
    const off = bufferOffset || 0;
    const n = Math.min(dest.length, src.length - off);
    for (let i = 0; i < n; i++) dest[i] = src[off + i];
  }
  copyToChannel(src, channel, bufferOffset) {
    const dst = this.getChannelData(channel);
    const off = bufferOffset || 0;
    const n = Math.min(src.length, dst.length - off);
    for (let i = 0; i < n; i++) dst[off + i] = src[i];
  }
  getChannelData(c) { const b = _abBag.get(this); return b.ch[c] || (b.ch[c] = new Float32Array(b.len)); }
};
Object.defineProperty(AudioBuffer.prototype, Symbol.toStringTag, { value: 'AudioBuffer', configurable: true });
_markNative(AudioBuffer);
_markNativeProto(AudioBuffer.prototype);
// The context classes are (re)defined below — sweep their prototypes only
// after the definitions exist, otherwise snapshot creation crashes on
// `undefined.prototype`.
//
// (300015 P4) Chrome 155 audio surface (tmp-agent/chrome-audio-truth.json):
// every attribute is a prototype accessor in Chrome's own order, instances
// carry ZERO own properties (state/sampleRate/destination live in a WeakMap
// bag), the event methods are INHERITED from EventTarget (chain:
// EventTarget → BaseAudioContext → AudioContext/OfflineAudioContext),
// resume/suspend live on AudioContext + OfflineAudioContext — NOT Base —
// and contexts are born 'suspended' (autoplay policy), with OAC flipping to
// 'closed' when startRendering settles.
const _ctxBagMap = new WeakMap();
function _ctxBag(ctx) {
  let b = _ctxBagMap.get(ctx);
  if (!b) { b = {}; _ctxBagMap.set(ctx, b); }
  return b;
}
function _ctxSetState(ctx, s) {
  const b = _ctxBag(ctx);
  if (b.state === s) return;
  b.state = s;
  Promise.resolve().then(() => {
    const ev = { type: 'statechange', target: ctx };
    try { if (typeof b.onstatechange === 'function') b.onstatechange(ev); } catch (e) {}
    try { ctx.dispatchEvent(ev); } catch (e) {}
  });
}
globalThis.BaseAudioContext = class BaseAudioContext extends EventTarget {
  constructor() {
    super();
    const b = _ctxBag(this);
    b.sampleRate = _fp('audioSampleRate');
    b.currentTime = 0;
    b.state = 'suspended';
    b.onstatechange = null;
    b.destination = _audioNode(this, {maxChannelCount:2,numberOfInputs:1,numberOfOutputs:0,channelCount:2});
    b.listener = {positionX:_audioParam(0),positionY:_audioParam(0),positionZ:_audioParam(0),forwardX:_audioParam(0),forwardY:_audioParam(0),forwardZ:_audioParam(-1),upX:_audioParam(0),upY:_audioParam(1),upZ:_audioParam(0),setPosition(){},setOrientation(){}};
  }
  get destination() { return _ctxBag(this).destination; }
  get sampleRate() { return _ctxBag(this).sampleRate; }
  get currentTime() { return _ctxBag(this).currentTime; }
  get listener() { return _ctxBag(this).listener; }
  get state() { return _ctxBag(this).state; }
  get onstatechange() { return _ctxBag(this).onstatechange; }
  set onstatechange(v) { _ctxBag(this).onstatechange = v; }
  createAnalyser() {
    return _audioNode(this, {fftSize:2048,frequencyBinCount:1024,minDecibels:-100,maxDecibels:-30,smoothingTimeConstant:0.8,
      getByteFrequencyData(a){for(let i=0;i<a.length;i++)a[i]=Math.floor(_fpRand(600+i)*10);},
      getFloatFrequencyData(a){for(let i=0;i<a.length;i++)a[i]=-100+_fpRand(700+i)*5;},
      getByteTimeDomainData(a){for(let i=0;i<a.length;i++)a[i]=128;},
      getFloatTimeDomainData(a){for(let i=0;i<a.length;i++)a[i]=0;}
    });
  }
  createBiquadFilter() { return _audioNode(this, {type:'lowpass',frequency:_audioParam(350,0,24000),Q:_audioParam(1),detune:_audioParam(0),gain:_audioParam(0,-40,40),
    getFrequencyResponse(){}}); }
  createBuffer(ch,len,rate) { return new AudioBuffer({numberOfChannels:ch,length:len,sampleRate:rate||this.sampleRate}); }
  createBufferSource() { return _audioNode(this, {buffer:null,playbackRate:_audioParam(1),detune:_audioParam(0),loop:false,loopStart:0,loopEnd:0,start(){},stop(){},onended:null}); }
  createChannelMerger(n) { return _audioNode(this, {numberOfInputs:n||6,numberOfOutputs:1}); }
  createChannelSplitter(n) { return _audioNode(this, {numberOfInputs:1,numberOfOutputs:n||6}); }
  createConstantSource() { return _audioNode(this, {offset:_audioParam(0),start(){},stop(){},onended:null}); }
  createConvolver() { return _audioNode(this, {buffer:null,normalize:true}); }
  createDelay() { return _audioNode(this, {delayTime:_audioParam(0,0,1)}); }
  createDynamicsCompressor() { return _audioNode(this, {threshold:_audioParam(_fp('compThreshold'),-100,0),knee:_audioParam(_fp('compKnee'),0,40),ratio:_audioParam(_fp('compRatio'),1,20),attack:_audioParam(0.003,0,1),release:_audioParam(0.25,0,1),reduction:0}); }
  createGain() { return _audioNode(this, {gain:_audioParam(1)}); }
  createIIRFilter(feedforward, feedback) { return _audioNode(this, {feedforward:feedforward||null,feedback:feedback||null,getFrequencyResponse(){}}); }
  createOscillator() { return _audioNode(this, {type:'sine',frequency:_audioParam(440),detune:_audioParam(0),start(){},stop(){},onended:null}); }
  createPanner() { return _audioNode(this, {panningModel:'equalpower',distanceModel:'inverse',refDistance:1,maxDistance:10000,rolloffFactor:1,coneInnerAngle:360,coneOuterAngle:360,coneOuterGain:0,setPosition(){},setOrientation(){}}); }
  createPeriodicWave(real, imag) {
    const w = {};
    Object.defineProperty(w, Symbol.toStringTag, { value: 'PeriodicWave', configurable: true });
    return w;
  }
  createScriptProcessor() { return _audioNode(this, {onaudioprocess:null,bufferSize:4096}); }
  createStereoPanner() { return _audioNode(this, {pan:_audioParam(0,-1,1)}); }
  createWaveShaper() { return _audioNode(this, {curve:null,oversample:'none'}); }
  decodeAudioData(buf) { return Promise.resolve(this.createBuffer(2,44100,44100)); }
  get renderQuantumSize() { return 128; }
};
globalThis.AudioContext = class AudioContext extends BaseAudioContext {
  constructor(options) {
    super();
    const b = _ctxBag(this);
    if (options && options.sampleRate) b.sampleRate = options.sampleRate;
    b.baseLatency = _fp('audioBaseLatency');
    b.outputLatency = 0;
    b.onerror = null;
  }
  get baseLatency() { return _ctxBag(this).baseLatency; }
  get outputLatency() { return _ctxBag(this).outputLatency; }
  get onerror() { return _ctxBag(this).onerror; }
  set onerror(v) { _ctxBag(this).onerror = v; }
  close() { _ctxSetState(this, 'closed'); return Promise.resolve(); }
  createMediaElementSource(el) { return _audioNode(this, {mediaElement:el||null}); }
  createMediaStreamDestination() { return _audioNode(this, {stream:{getTracks(){return [];},getAudioTracks(){return [];}}}); }
  createMediaStreamSource(stream) { return _audioNode(this, {mediaStream:stream||null}); }
  getOutputTimestamp() { return {contextTime:this.currentTime,performanceTime:0}; }
  resume() { _ctxSetState(this, 'running'); return Promise.resolve(); }
  suspend() { _ctxSetState(this, 'suspended'); return Promise.resolve(); }
  get playbackStats() { return {}; }
};
globalThis.OfflineAudioContext = class OfflineAudioContext extends BaseAudioContext {
  constructor(ch, len, rate) {
    super();
    if (ch == null && len == null && rate == null) {
      throw new TypeError("Failed to construct 'OfflineAudioContext': 3 arguments required, or use OfflineAudioContext(options).");
    }
    if (ch && typeof ch === 'object') {
      const o = ch;
      ch = o.numberOfChannels; len = o.length; rate = o.sampleRate;
    }
    const b = _ctxBag(this);
    b.numberOfChannels = Math.max(1, ch | 0 || 1);
    b.length = Math.max(0, len | 0 || 0);
    b.sampleRate = rate || 44100;
    b.oncomplete = null;
  }
  get oncomplete() { return _ctxBag(this).oncomplete; }
  set oncomplete(v) { _ctxBag(this).oncomplete = v; }
  get length() { return _ctxBag(this).length; }
  resume() { _ctxSetState(this, 'running'); return Promise.resolve(); }
  startRendering() {
    const b = _ctxBag(this);
    const buf = this.createBuffer(b.numberOfChannels, b.length, b.sampleRate);
    // Chrome truth (headless+headed probe 10-15): an empty graph renders
    // SILENCE - nonzero:0, sum:0. A graph with oscillator→compressor→dest
    // (classic audio FP) must be NON-zero — previously always silence and
    // shrank AntiSpam profileData vs Chrome.
    if (b.__graph || b.__toDest) {
      try {
        const ch0 = buf.getChannelData(0);
        const sr = b.sampleRate || 44100;
        // Deterministic triangle-ish + compressor-ish envelope from persona.
        const freq = 10000;
        const thr = _fp('compThreshold');
        const ratio = Math.max(1, _fp('compRatio'));
        for (let i = 0; i < ch0.length; i++) {
          const t = i / sr;
          let s = 2 * Math.abs(2 * ((t * freq) % 1) - 1) - 1; // triangle [-1,1]
          s *= 0.25 + 0.15 * _fpRand(800 + (i & 255));
          // soft-knee-ish fold toward threshold
          const a = Math.abs(s);
          const db = a > 1e-8 ? (20 * Math.log(a) / Math.LN10) : -100;
          if (db > thr) {
            const over = db - thr;
            const outDb = thr + over / ratio;
            const outA = Math.pow(10, outDb / 20);
            s = Math.sign(s) * outA;
          }
          ch0[i] = s;
        }
        for (let c = 1; c < buf.numberOfChannels; c++) {
          buf.getChannelData(c).set(ch0);
        }
      } catch (e) {}
    }
    const self = this;
    return Promise.resolve().then(() => {
      _ctxSetState(self, 'closed');
      const ev = { type: 'complete', renderedBuffer: buf, target: self };
      try { if (typeof b.oncomplete === 'function') b.oncomplete(ev); } catch (e) {}
      self.dispatchEvent(ev);
      return buf;
    });
  }
  suspend(time) {
    if (time === undefined) {
      throw new TypeError("Failed to execute 'suspend' on 'OfflineAudioContext': 1 argument required, but only 0 present.");
    }
    time = Number(time);
    if (!isFinite(time)) {
      throw new TypeError("Failed to execute 'suspend' on 'OfflineAudioContext': The provided double value is non-finite.");
    }
    return Promise.resolve();
  }
};
_markNativeProto(globalThis.BaseAudioContext.prototype);
_markNativeProto(globalThis.AudioContext.prototype);
_markNativeProto(globalThis.OfflineAudioContext.prototype);
// (300015 P4) V8 class syntax puts `constructor` FIRST in the prototype's
// own-names order; Chrome's IDL prototypes list it LAST (audio truth
// capture). Class prototype members are configurable, so delete +
// re-define moves it to the end without touching behavior. Audio family
// only — the wider ordering gap across all class-syntax interfaces is a
// known residual (name SETS match everywhere).
(function () {
  for (const P of [EventTarget.prototype, AudioBuffer.prototype,
                   BaseAudioContext.prototype, AudioContext.prototype,
                   OfflineAudioContext.prototype]) {
    try {
      const c = P.constructor;
      if (Object.getOwnPropertyNames(P)[0] === 'constructor') {
        delete P.constructor;
        Object.defineProperty(P, 'constructor', { value: c, writable: true, configurable: true });
      }
    } catch (e) {}
  }
})();

globalThis.speechSynthesis = {
  speaking: false, pending: false, paused: false,
  _loaded: false,
  getVoices() {
    // fp-diff-1014 P1: Chrome populates the voice list asynchronously — the
    // first synchronous call returns [] and the list appears only after the
    // voiceschanged event. Answering 78 voices on the first call is a
    // no-real-audio-subsystem tell. First call arms the populate timer;
    // Chrome's pages re-query inside onvoiceschanged.
    if (!this._loaded) {
      if (!this._armTimer) {
        this._armTimer = true;
        setTimeout(() => {
          this._loaded = true;
          try {
            const ev = { type: 'voiceschanged', target: this };
            if (typeof this.onvoiceschanged === 'function') this.onvoiceschanged(ev);
            if (this._vcListeners) {
              for (const fn of this._vcListeners) {
                try { fn(ev); } catch (e) {}
              }
            }
          } catch (e) {}
        }, 40);
      }
      return [];
    }
    // Real macOS Chrome exposes ~50 local system voices plus ~25 Google
    // network voices across dozens of languages. A single synthetic voice
    // is a fingerprint outlier (Castle voices collector).
    if (!this._voices) {
      const mk = (name, lang, local, def) => ({ name, lang, default: !!def, localService: !!local, voiceURI: name });
      this._voices = [
        mk('Alex', 'en-US', true, true), mk('Ava', 'en-US', true), mk('Ayumi', 'ja-JP', true),
        mk('Bad News', 'en-US', true), mk('Bahh', 'en-US', true), mk('Bells', 'en-US', true),
        mk('Boing', 'en-US', true), mk('Bruce', 'en-US', true), mk('Bubbles', 'en-US', true),
        mk('Carmit', 'he-IL', true), mk('Cellos', 'en-US', true), mk('Chan-yu', 'zh-TW', true),
        mk('Damayanti', 'id-ID', true), mk('Daniel', 'en-GB', true), mk('Deranged', 'en-US', true),
        mk('Diego', 'es-AR', true), mk('Ellen', 'nl-BE', true), mk('Fiona', 'en-GB', true),
        mk('Fred', 'en-US', true), mk('Good News', 'en-US', true), mk('Hesham', 'ar-SA', true),
        mk('Jester', 'en-US', true), mk('Joana', 'pt-PT', true), mk('Junior', 'en-US', true),
        mk('Kanya', 'th-TH', true), mk('Karen', 'en-AU', true), mk('Kathy', 'en-US', true),
        mk('Katja', 'de-DE', true), mk('Kyoko', 'ja-JP', true), mk('Laura', 'sk-SK', true),
        mk('Lekha', 'hi-IN', true), mk('Luciana', 'pt-BR', true), mk('Maged', 'ar-SA', true),
        mk('Mariska', 'hu-HU', true), mk('Mei-Jia', 'zh-TW', true), mk('Melina', 'el-GR', true),
        mk('Milena', 'ru-RU', true), mk('Moira', 'en-IE', true), mk('Monica', 'es-ES', true),
        mk('Nora', 'nb-NO', true), mk('Organ', 'en-US', true), mk('Paulina', 'es-MX', true),
        mk('Ralph', 'en-US', true), mk('Samantha', 'en-US', true), mk('Sara', 'da-DK', true),
        mk('Satu', 'fi-FI', true), mk('Sin-ji', 'zh-HK', true), mk('Tessa', 'en-GB', true),
        mk('Thomas', 'fr-FR', true), mk('Ting-Ting', 'zh-CN', true), mk('Tomas', 'cs-CZ', true),
        mk('Trinoids', 'en-US', true), mk('Veena', 'en-IN', true), mk('Victoria', 'en-US', true),
        mk('Whisper', 'en-US', true), mk('Xander', 'nl-NL', true), mk('Yelda', 'tr-TR', true),
        mk('Yuna', 'ko-KR', true), mk('Zosia', 'pl-PL', true), mk('Zuzana', 'cs-CZ', true),
        mk('Google US English', 'en-US', false), mk('Google UK English Female', 'en-GB', false),
        mk('Google UK English Male', 'en-GB', false), mk('Google Deutsch', 'de-DE', false),
        mk('Google español', 'es-ES', false), mk('Google français', 'fr-FR', false),
        mk('Google हिन्दी', 'hi-IN', false), mk('Google Bahasa Indonesia', 'id-ID', false),
        mk('Google italiano', 'it-IT', false), mk('Google 日本語', 'ja-JP', false),
        mk('Google 한국의', 'ko-KR', false), mk('Google Nederlands', 'nl-NL', false),
        mk('Google polski', 'pl-PL', false), mk('Google português do Brasil', 'pt-BR', false),
        mk('Google русский', 'ru-RU', false), mk('Google 普通话（中国大陆）', 'zh-CN', false),
        mk('Google 粤語（香港）', 'zh-HK', false), mk('Google闽南话（台湾）', 'zh-TW', false),
      ];
      __hideOwn(this);
    }
    return this._voices;
  },
  speak() {}, cancel() {}, pause() {}, resume() {},
  addEventListener(t, fn) {
    if (t === 'voiceschanged' && typeof fn === 'function') {
      (this._vcListeners = this._vcListeners || []).push(fn);
    }
  },
  removeEventListener(t, fn) {
    if (this._vcListeners) this._vcListeners = this._vcListeners.filter((f) => f !== fn);
  },
  onvoiceschanged: null,
};

_markNative(BaseAudioContext); _markNative(AudioContext); _markNative(OfflineAudioContext);
_markNative(SpeechSynthesisUtterance);
