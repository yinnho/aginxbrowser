// WebRTC / MediaStream / CacheStorage / setHTMLUnsafe faces — ratchet split
// out of bootstrap.js. Top level
// only declares bindings; nothing here is read before call time.
globalThis.MediaStream = class MediaStream { constructor(){this.id='';this.active=true;} getTracks(){return [];} getAudioTracks(){return [];} getVideoTracks(){return [];} addTrack(){} removeTrack(){} clone(){return new MediaStream();} };
globalThis.MediaStreamTrack = class MediaStreamTrack { constructor(){this.kind='';this.enabled=true;this.readyState='live';} stop(){} clone(){return new MediaStreamTrack();} };
// (#170) RTCPeerConnection is a WebIDL interface in Chrome: a bare call
// throws the DOM TypeError (not V8's class-constructor text), and
// Object.prototype.toString brands instances [object RTCPeerConnection]
// via Symbol.toStringTag. The AWSC collection on the 1688/taobao punish
// page walks both — a plain ES class answers V8's raw message and
// [object Object], a distinguishable JS-env signal. Same pattern family
// as the CSSStyleDeclaration branding (059015a).
// (300015 / fp-depth) Chrome gather fires host ICE candidates after
// setLocalDescription; an empty stub (n=0 candidates, empty SDP) shrinks
// AntiSpam profileData vs Chrome as/webprofile ~2176-2224. Seed cand+ufrag
// from persona _fpRand so the face is stable per device, not random/session.
const __RTCPCImpl = class RTCPeerConnection extends EventTarget {
  constructor(config){
    super();
    this.localDescription=null; this.remoteDescription=null;
    this.iceConnectionState='new'; this.iceGatheringState='new';
    this.signalingState='stable'; this.connectionState='new';
    this.onicecandidate=null; this.onicegatheringstatechange=null;
    this.onsignalingstatechange=null; this.onconnectionstatechange=null;
    this.__cfg = config || null;
    this.__closed = false;
    this.__ufrag = __rtcToken(4); this.__pwd = __rtcToken(24);
    const a = 1 + Math.floor(_fpRand(710) * 254);
    const b = 1 + Math.floor(_fpRand(711) * 254);
    this.__host = '192.168.' + a + '.' + b;
    this.__port = 40000 + Math.floor(_fpRand(712) * 20000);
  }
  __setGather(s){
    if (this.iceGatheringState === s) return;
    this.iceGatheringState = s;
    try { this.dispatchEvent(new Event('icegatheringstatechange')); } catch (e) {}
  }
  __emitCand(candStr){
    const cand = candStr == null ? null : new RTCIceCandidate({
      candidate: candStr, sdpMid: '0', sdpMLineIndex: 0,
    });
    const ev = new RTCPeerConnectionIceEvent('icecandidate', { candidate: cand });
    // dispatchEvent alone: onicecandidate is an EventHandler listener.
    try { this.dispatchEvent(ev); } catch (e) {}
  }
  __mkOfferSdp(){
    const fp = [];
    for (let i = 0; i < 32; i++) {
      const n = Math.floor(_fpRand(720 + i) * 256);
      fp.push((n < 16 ? '0' : '') + n.toString(16).toUpperCase());
    }
    return [
      'v=0',
      'o=- ' + String(Math.floor(1e15 + _fpRand(721) * 9e15)) + ' 2 IN IP4 127.0.0.1',
      's=-', 't=0 0',
      'a=group:BUNDLE 0', 'a=extmap-allow-mixed', 'a=msid-semantic: WMS',
      'm=application 9 UDP/DTLS/SCTP webrtc-datachannel',
      'c=IN IP4 0.0.0.0', 'a=ice-ufrag:' + this.__ufrag, 'a=ice-pwd:' + this.__pwd,
      'a=ice-options:trickle',
      'a=fingerprint:sha-256 ' + fp.join(':'),
      'a=setup:actpass', 'a=mid:0', 'a=sctp-port:5000', 'a=max-message-size:262144',
    ].join('\r\n') + '\r\n';
  }
  createOffer(){
    if (this.__closed) return Promise.reject(new DOMException('The RTCPeerConnection\'s signalingState is \'closed\'.', 'InvalidStateError'));
    return Promise.resolve(new RTCSessionDescription({ type: 'offer', sdp: this.__mkOfferSdp() }));
  }
  createAnswer(){
    if (this.__closed) return Promise.reject(new DOMException('The RTCPeerConnection\'s signalingState is \'closed\'.', 'InvalidStateError'));
    return Promise.resolve(new RTCSessionDescription({ type: 'answer', sdp: this.__mkOfferSdp() }));
  }
  setLocalDescription(desc){
    const self = this;
    if (self.__closed) return Promise.reject(new DOMException('The RTCPeerConnection\'s signalingState is \'closed\'.', 'InvalidStateError'));
    const d = desc || { type: 'offer', sdp: self.__mkOfferSdp() };
    self.localDescription = (d instanceof RTCSessionDescription) ? d : new RTCSessionDescription(d);
    self.signalingState = self.localDescription.type === 'answer' ? 'stable' : 'have-local-offer';
    self.__setGather('gathering');
    return Promise.resolve().then(function () {
      if (self.__closed) return;
      // Host UDP + TCP candidates (Chrome desktop typically exposes both).
      const foundation = String(Math.floor(1e8 + _fpRand(730) * 9e8));
      const pri = 2122260223;
      self.__emitCand('candidate:' + foundation + ' 1 udp ' + pri + ' ' + self.__host + ' ' + self.__port + ' typ host generation 0 ufrag ' + self.__ufrag + ' network-id 1 network-cost 10');
      self.__emitCand('candidate:' + foundation + ' 1 tcp ' + (pri - 1) + ' ' + self.__host + ' ' + (9) + ' typ host tcptype active generation 0 ufrag ' + self.__ufrag + ' network-id 1 network-cost 10');
      self.__emitCand(null);
      self.__setGather('complete');
    });
  }
  setRemoteDescription(desc){
    if (this.__closed) return Promise.reject(new DOMException('The RTCPeerConnection\'s signalingState is \'closed\'.', 'InvalidStateError'));
    this.remoteDescription = (desc instanceof RTCSessionDescription) ? desc : new RTCSessionDescription(desc || {});
    return Promise.resolve();
  }
  addIceCandidate(){ return Promise.resolve(); }
  close(){
    this.__closed = true;
    this.iceConnectionState = 'closed';
    this.connectionState = 'closed';
    this.signalingState = 'closed';
  }
  createDataChannel(label){
    return {
      label: label == null ? '' : String(label),
      readyState: 'connecting',
      close(){ this.readyState = 'closed'; },
      send(){},
      addEventListener(){}, removeEventListener(){},
    };
  }
  getStats(){ return Promise.resolve(new Map()); }
};
function __rtcToken(n){
  const alpha = 'abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789+/';
  let s = '';
  for (let i = 0; i < n; i++) s += alpha[Math.floor(_fpRand(740 + i) * alpha.length)];
  return s;
}
globalThis.RTCPeerConnection = function RTCPeerConnection() {
  if (!new.target) {
    throw new TypeError("Failed to construct 'RTCPeerConnection': Please use the 'new' operator, this DOM object constructor cannot be called as a function.");
  }
  return Reflect.construct(__RTCPCImpl, arguments, new.target);
};
globalThis.RTCPeerConnection.prototype = __RTCPCImpl.prototype;
globalThis.RTCSessionDescription = class RTCSessionDescription { constructor(d){this.type=d?.type;this.sdp=d?.sdp;} };
globalThis.RTCIceCandidate = class RTCIceCandidate { constructor(d){this.candidate=d?.candidate||'';} };
// RTC satellite interfaces — DataDome-class audits walk the whole RTCPeerConnection
// orbit (`'RTCDataChannel' in window`), and a peer with RTCPeerConnection but no
// RTCDataChannel/RTCTrackEvent is a JS-env tell. All are presence-level stubs:
// abstract ones throw Illegal constructor like Chrome, the two events derive from
// Event, RTCRtpTransceiver carries the probed stop() method.
globalThis.RTCDataChannel = class RTCDataChannel {
  constructor() { throw new TypeError("Illegal constructor"); }
  get label() { return ""; }
  get readyState() { return "connecting"; }
  close() {}
  send() { throw new TypeError("Failed to execute 'send' on 'RTCDataChannel'"); }
};
globalThis.RTCDtlsTransport = class RTCDtlsTransport {
  constructor() { throw new TypeError("Illegal constructor"); }
  get state() { return "new"; }
};
globalThis.RTCSctpTransport = class RTCSctpTransport {
  constructor() { throw new TypeError("Illegal constructor"); }
  get state() { return "connecting"; }
  get maxMessageSize() { return 65536; }
};
globalThis.RTCStatsReport = class RTCStatsReport {
  constructor() { throw new TypeError("Illegal constructor"); }
  forEach() {}
  get() { return undefined; }
  has() { return false; }
  get size() { return 0; }
};
globalThis.RTCEncodedAudioFrame = class RTCEncodedAudioFrame {
  constructor() { throw new TypeError("Illegal constructor"); }
  get timestamp() { return 0; }
  data() { return new ArrayBuffer(0); }
};
globalThis.RTCError = class RTCError extends globalThis.DOMException {
  constructor(init, message) {
    super(message !== undefined ? String(message) : "RTCError", "OperationError");
    this.errorDetail = init?.errorDetail ?? null;
    this.sdpLineNumber = init?.sdpLineNumber ?? null;
    this.sctpCauseCode = init?.sctpCauseCode ?? null;
    this.receivedAlert = init?.receivedAlert ?? null;
    this.sentAlert = init?.sentAlert ?? null;
  }
};
globalThis.RTCPeerConnectionIceEvent = class RTCPeerConnectionIceEvent extends globalThis.Event {
  constructor(type, init) {
    super(type || 'icecandidate', init);
    this.candidate = init && 'candidate' in init ? init.candidate : null;
  }
};
globalThis.RTCPeerConnectionIceErrorEvent = class RTCPeerConnectionIceErrorEvent extends globalThis.Event {
  constructor(type, init) { super(type || "icecandidateerror", init); this.errorCode = init?.errorCode ?? 0; this.errorText = init?.errorText ?? ""; }
  get address() { return null; }
  get port() { return null; }
  get url() { return ""; }
};
globalThis.RTCTrackEvent = class RTCTrackEvent extends globalThis.Event {
  constructor(type, init) { super(type || "track", init); this.receiver = init?.receiver ?? null; this.track = init?.track ?? null; this.streams = init?.streams ?? []; this.transceiver = init?.transceiver ?? null; }
};
globalThis.RTCRtpTransceiver = class RTCRtpTransceiver {
  constructor() { throw new TypeError("Illegal constructor"); }
  get mid() { return null; }
  get direction() { return "sendrecv"; }
  stop() {}
};
globalThis.webkitRTCPeerConnection = globalThis.RTCPeerConnection;
// (#170) The same brand across the whole RTC orbit — Chrome answers
// [object RTCDataChannel] / [object RTCSessionDescription] / … for every
// interface here; a class-without-tag reads as [object Object].
for (const __n of ['RTCPeerConnection','RTCSessionDescription','RTCIceCandidate','RTCDataChannel','RTCDtlsTransport','RTCSctpTransport','RTCStatsReport','RTCEncodedAudioFrame','RTCError','RTCPeerConnectionIceEvent','RTCPeerConnectionIceErrorEvent','RTCTrackEvent','RTCRtpTransceiver']) {
  const __k = globalThis[__n];
  if (__k && __k.prototype && !__k.prototype[Symbol.toStringTag]) {
    Object.defineProperty(__k.prototype, Symbol.toStringTag, { value: __n, enumerable: false, writable: false, configurable: true });
  }
}

_markNative(MediaStream); _markNative(MediaStreamTrack);
_markNative(RTCPeerConnection); _markNative(RTCSessionDescription); _markNative(RTCIceCandidate);

// setHTMLUnsafe / getHTML: shims over innerHTML. setHTMLUnsafe parses markup
// like innerHTML (the parse fixup expands declarative shadow roots the same
// way document parsing does); getHTML serializes like innerHTML.
Element.prototype.setHTMLUnsafe = function setHTMLUnsafe(html) { this.innerHTML = String(html == null ? "" : html); };
Element.prototype.getHTML = function getHTML() { return this.innerHTML; };
_markNative(Element.prototype.setHTMLUnsafe);
_markNative(Element.prototype.getHTML);
// Document.parseHTMLUnsafe(html): static that parses into a new HTML document.
if (typeof Document !== 'undefined' && typeof Document.parseHTMLUnsafe !== 'function') {
  Document.parseHTMLUnsafe = function parseHTMLUnsafe(html) {
    return new DOMParser().parseFromString(String(html == null ? "" : html), "text/html");
  };
  _markNative(Document.parseHTMLUnsafe);
}

globalThis.SpeechSynthesisUtterance = class SpeechSynthesisUtterance { constructor(t){this.text=t;this.lang='en-US';this.rate=1;this.pitch=1;this.volume=1;} };


globalThis.caches = {
  open() { return Promise.resolve({ match(){return Promise.resolve(undefined);}, put(){return Promise.resolve();}, delete(){return Promise.resolve(false);}, keys(){return Promise.resolve([]);} }); },
  match() { return Promise.resolve(undefined); },
  has() { return Promise.resolve(false); },
  delete() { return Promise.resolve(false); },
  keys() { return Promise.resolve([]); },
};
