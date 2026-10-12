// (#98) Window-level scalar faces Chrome always exposes and page SDKs read
// unguarded. PDD's goods-list SDK does `window.name.split(...)` inside a
// windowName getter — with `name` absent the TypeError poisoned the React
// mount and #__next stayed empty (2026-10-12). Chrome's shape: name/status
// are [LegacyReplaceable] writable data properties; origin/closed are
// prototype getters. Defined as own properties with the Chrome descriptors
// pages can observe (enumerable name/status, accessor origin/closed reading
// live state at call time).
Object.defineProperty(globalThis, 'name', {
  value: '', writable: true, enumerable: true, configurable: true,
});
Object.defineProperty(globalThis, 'status', {
  value: '', writable: true, enumerable: true, configurable: true,
});
Object.defineProperty(globalThis, 'origin', {
  get() {
    try { return globalThis.location ? globalThis.location.origin : 'null'; }
    catch (e) { return 'null'; }
  },
  enumerable: true, configurable: true,
});
Object.defineProperty(globalThis, 'closed', {
  get() { return false; },
  enumerable: true, configurable: true,
});
