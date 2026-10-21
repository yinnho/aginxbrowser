// MediaRecorder.isTypeSupported + Picture-in-Picture flags. Runs after
// global_surface.js (which lands the MediaRecorder constructor) and before the
// sweeps; uses _parseMediaType from media_source.js (shared global scope).
//
// Chrome 145 answers, measured: webm takes vp8/vp9/av1/avc1/h264/opus/pcm,
// mp4 takes avc1/av01/vp9/mp4a.40.2/opus (not h264/hvc1 spellings), audio/webm
// only opus, audio/mp4 aac or opus, matroska avc1; "" is true; the rest false.
(function () {
  const MR = globalThis.MediaRecorder;
  if (typeof MR === 'function' && !Object.prototype.hasOwnProperty.call(MR, 'isTypeSupported')) {
    const ok = {
      'video/webm': ['vp8', 'vp9', 'av1', 'avc1', 'h264', 'opus', 'pcm'],
      'video/mp4': ['avc1', 'av01', 'vp9', 'mp4a.40.2', 'opus'],
      'audio/webm': ['opus', 'pcm'],
      'audio/mp4': ['mp4a.40.2', 'opus'],
      'video/x-matroska': ['avc1', 'vp8', 'vp9', 'av1', 'opus'],
    };
    const isTypeSupported = function isTypeSupported(type) {
      if (arguments.length === 0) {
        throw new TypeError("Failed to execute 'isTypeSupported' on 'MediaRecorder': 1 argument required, but only 0 present.");
      }
      const { mime, codecs } = _parseMediaType(type);
      if (!String(type)) return true;
      const list = ok[mime];
      if (!list) return false;
      if (!codecs || !codecs.length) return true;
      return codecs.every((c) => {
        c = c.toLowerCase();
        return list.some((f) => c === f || c.indexOf(f + '.') === 0);
      });
    };
    Object.defineProperty(MR, 'isTypeSupported', { value: isTypeSupported, writable: true, configurable: true, enumerable: true });
    _markNative(MR.isTypeSupported);
  }
  const D = globalThis.Document && globalThis.Document.prototype;
  if (D && !('pictureInPictureEnabled' in D)) {
    Object.defineProperty(D, 'pictureInPictureEnabled', { get: function () { return true; }, enumerable: true, configurable: true });
    Object.defineProperty(D, 'pictureInPictureElement', { get: function () { return null; }, enumerable: true, configurable: true });
    D.exitPictureInPicture = function exitPictureInPicture() {
      return Promise.reject(new DOMException('No Picture-in-Picture element is active.', 'InvalidStateError'));
    };
    _markNative(D.exitPictureInPicture);
  }
  const V = globalThis.HTMLVideoElement && globalThis.HTMLVideoElement.prototype;
  if (V && !('requestPictureInPicture' in V)) {
    V.requestPictureInPicture = function requestPictureInPicture() {
      return Promise.reject(new DOMException('Metadata for the video element are not loaded yet.', 'InvalidStateError'));
    };
    _markNative(V.requestPictureInPicture);
    Object.defineProperty(V, 'disablePictureInPicture', { get: function () { return this.hasAttribute('disablepictureinpicture'); }, set: function (v) { if (v) this.setAttribute('disablepictureinpicture', ''); else this.removeAttribute('disablepictureinpicture'); }, enumerable: true, configurable: true });
    Object.defineProperty(V, 'onenterpictureinpicture', { value: null, writable: true, enumerable: true, configurable: true });
    Object.defineProperty(V, 'onleavepictureinpicture', { value: null, writable: true, enumerable: true, configurable: true });
  }
})();
