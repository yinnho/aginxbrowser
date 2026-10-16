// IndexedDB shim (#53).
// Split out of bootstrap.js wholesale (god-file ratchet, pure move): top level only
// declares bindings; every bootstrap helper it names is touched at call time.


// IndexedDB shim with spec-shaped open/upgrade semantics (#53). Storage and
// auth libraries (Dexie, localForage, Firebase, Supabase) gate their whole
// boot on `onupgradeneeded` firing once per version — they create object
// stores inside the versionchange transaction and the open promise never
// settles otherwise (xhs creator publish page dead-ends on its skeleton this
// way). Data stays page-lifetime (in-memory Maps owned by the database
// record, so put/getAll agree across transactions), while the *schema*
// (version + store names + keyPaths) is mirrored into localStorage so a
// later navigation re-opens at the installed version and skips the upgrade,
// like a warm real profile.
function _idbErr(name, msg) {
  try { return new DOMException(msg || name, name); } catch (e) { const err = new Error(msg || name); err.name = name; return err; }
}
function _idbStrList(namesView) {
  const base = {
    contains(n) { return namesView().indexOf(String(n)) >= 0; },
    get length() { return namesView().length; },
    item(i) { return namesView()[i] ?? null; },
    [Symbol.iterator]() { return namesView()[Symbol.iterator](); },
  };
  Object.defineProperty(base, Symbol.toStringTag, { value: 'DOMStringList' });
  // Chrome's DOMStringList exposes names through an indexed getter. #203:
  // idb's transaction proxy reads objectStoreNames[0]/[1] to auto-pick the
  // lone store, and a list without index access resolved undefined into
  // "NotFoundError: undefined is not in this transaction's scope". The
  // proxy keeps indices live against the closure view.
  return new Proxy(base, {
    get(t, k, r) {
      if (typeof k === 'string' && k !== '' && /^\d+$/.test(k)) {
        const names = namesView();
        const i = Number(k);
        return i < names.length ? names[i] : undefined;
      }
      return Reflect.get(t, k, r);
    },
  });
}
// Key ordering across types (number < date < string), per the IDB spec's
// comparability ladder. Arrays/binary keep insertion order — good enough for
// cursor scans over hashed app data.
function _idbCmp(a, b) {
  const rank = (v) => (typeof v === 'number' ? 0 : v instanceof Date ? 1 : typeof v === 'string' ? 2 : 3);
  const ta = rank(a), tb = rank(b);
  if (ta !== tb) return ta - tb;
  if (ta === 3) return 0;
  if (ta === 2) return a < b ? -1 : a > b ? 1 : 0;
  return +a < +b ? -1 : +a > +b ? 1 : 0;
}
function _idbIsRange(q) { return q !== null && q !== undefined && typeof q === 'object' && typeof q.includes === 'function'; }
function _idbNewRequest(source, tx) {
  const req = {
    result: undefined,
    error: null,
    source: source ?? null,
    transaction: tx ?? null,
    readyState: 'pending',
    onsuccess: null,
    onerror: null,
    onupgradeneeded: null,
    addEventListener(type, fn) { req['on' + type] = fn; },
    removeEventListener(type, fn) { if (req['on' + type] === fn) req['on' + type] = null; },
  };
  Object.setPrototypeOf(req, IDBRequest.prototype); // #209: instanceof IDBRequest
  return req;
}
// Requests settle asynchronously; after `onsuccess`/`onerror` dispatch (so a
// handler chaining another op on the same tx increments the live set first),
// the request detaches from its transaction and the tx may complete.
function _idbReqTx(tx, produceResult, source) {
  const req = _idbNewRequest(source, tx);
  if (tx) tx._live.add(req);
  Promise.resolve().then(() => {
    try {
      req.result = produceResult();
      req.readyState = 'done';
      if (typeof req.onsuccess === 'function') { try { req.onsuccess({ target: req, type: 'success' }); } catch (e) {} }
    } catch (e) {
      req.error = e; req.readyState = 'done';
      if (typeof req.onerror === 'function') { try { req.onerror({ target: req, type: 'error' }); } catch (e2) {} }
    }
    if (tx) { tx._live.delete(req); _idbMaybeComplete(tx); }
  });
  return req;
}
function _idbMaybeComplete(tx) {
  if (tx._done || tx._live.size > 0 || tx._completeQueued) return;
  tx._completeQueued = true;
  Promise.resolve().then(() => {
    tx._completeQueued = false;
    if (tx._done || tx._live.size > 0) return;
    tx._done = true;
    tx.readyState = 'finished';
    if (typeof tx.oncomplete === 'function') { try { tx.oncomplete({ type: 'complete', target: tx }); } catch (e) {} }
  });
}
// Database records live in script scope for the page lifetime. The schema
// (never the data) is mirrored to localStorage so a fresh navigation sees the
// same version a previous one installed and skips a spurious upgrade.
const _idbRegistryMap = new Map();
function _idbPersistMeta(rec) {
  try {
    const stores = [];
    rec.stores.forEach((st) => stores.push([st.name, st.keyPath, st.autoIncrement ? 1 : 0]));
    globalThis.localStorage.setItem('__diting_idb__' + rec.name, JSON.stringify({ v: rec.version, s: stores }));
  } catch (e) {}
}
function _idbRecord(name) {
  let rec = _idbRegistryMap.get(name);
  if (!rec) {
    rec = { name, version: 0, stores: new Map() };
    try {
      const raw = globalThis.localStorage.getItem('__diting_idb__' + name);
      if (raw) {
        const meta = JSON.parse(raw);
        rec.version = meta.v | 0;
        for (const s of meta.s || []) rec.stores.set(s[0], _idbStore(s[0], s[1] ?? null, !!s[2]));
      }
    } catch (e) {}
    _idbRegistryMap.set(name, rec);
  }
  return rec;
}
function _idbExtractKeyPath(keyPath, value) {
  if (Array.isArray(keyPath)) return keyPath.map((p) => _idbExtractKeyPath(p, value));
  let k = value;
  for (const part of String(keyPath).split('.')) k = k == null ? undefined : k[part];
  return k;
}
function _idbInjectKeyPath(keyPath, value, key) {
  const parts = String(keyPath).split('.');
  let o = value;
  for (let i = 0; i < parts.length - 1; i++) { if (o[parts[i]] == null) o[parts[i]] = {}; o = o[parts[i]]; }
  o[parts[parts.length - 1]] = key;
}
function _idbPrepareKey(st, value, key) {
  if (st.keyPath !== null) {
    if (key !== undefined && key !== null) throw _idbErr('DataError', 'key supplied to an inline-keys store');
    let k = _idbExtractKeyPath(st.keyPath, value);
    if (k === undefined || k === null) {
      if (st.autoIncrement) { k = ++st._seq; _idbInjectKeyPath(st.keyPath, value, k); }
      else throw _idbErr('DataError', 'value lacks keyPath ' + st.keyPath);
    }
    return k;
  }
  if (key !== undefined && key !== null) return key;
  if (st.autoIncrement) return ++st._seq;
  throw _idbErr('DataError', 'no key supplied and store has no keyPath');
}
function _idbSortedKeys(st, query, direction) {
  let keys = Array.from(st._data.keys());
  keys.sort(_idbCmp);
  if (_idbIsRange(query)) keys = keys.filter((k) => query.includes(k));
  else if (query !== undefined && query !== null) keys = keys.filter((k) => _idbCmp(k, query) === 0);
  if (direction && String(direction).indexOf('prev') === 0) keys.reverse();
  return keys;
}
function _idbStore(name, keyPath, autoIncrement) {
  const st = {
    name,
    keyPath: keyPath ?? null,
    autoIncrement: !!autoIncrement,
    transaction: null,
    _data: new Map(),
    _seq: 0,
    _indexes: new Map(),
  };
  st.indexNames = _idbStrList(() => Array.from(st._indexes.keys()));
  st.add = function (value, key) {
    return _idbReqTx(this.__tx, () => {
      const k = _idbPrepareKey(st, value, key);
      if (st._data.has(k)) throw _idbErr('ConstraintError', 'key ' + k + ' already exists in ' + st.name);
      st._data.set(k, value);
      return k;
    }, st);
  };
  st.put = function (value, key) {
    return _idbReqTx(this.__tx, () => {
      const k = _idbPrepareKey(st, value, key);
      st._data.set(k, value);
      return k;
    }, st);
  };
  st.get = function (query) {
    return _idbReqTx(this.__tx, () => {
      if (_idbIsRange(query)) { for (const k of _idbSortedKeys(st, query, 'next')) return st._data.get(k); return undefined; }
      return st._data.get(query);
    }, st);
  };
  st.getAll = function (query, count) {
    return _idbReqTx(this.__tx, () => {
      const out = [];
      for (const k of _idbSortedKeys(st, query, 'next')) {
        out.push(st._data.get(k));
        if (count != null && out.length >= count) break;
      }
      return out;
    }, st);
  };
  st.getAllKeys = function (query, count) {
    return _idbReqTx(this.__tx, () => {
      const out = _idbSortedKeys(st, query, 'next');
      return count != null ? out.slice(0, count) : out;
    }, st);
  };
  st.getKey = function (query) {
    return _idbReqTx(this.__tx, () => {
      const keys = _idbSortedKeys(st, query, 'next');
      return keys.length ? keys[0] : undefined;
    }, st);
  };
  st.delete = function (query) {
    return _idbReqTx(this.__tx, () => {
      if (_idbIsRange(query)) { for (const k of _idbSortedKeys(st, query, 'next')) st._data.delete(k); }
      else st._data.delete(query);
      return undefined;
    }, st);
  };
  st.clear = function () { return _idbReqTx(this.__tx, () => { st._data.clear(); return undefined; }, st); };
  st.count = function (query) {
    return _idbReqTx(this.__tx, () => (query === undefined || query === null ? st._data.size : _idbSortedKeys(st, query, 'next').length), st);
  };
  // Cursors re-fire `onsuccess` on the same request after continue(), and
  // hold the transaction open until they run past the last key.
  st.openCursor = function (query, direction) {
    const tx = this.__tx;
    const req = _idbNewRequest(st, tx);
    const keys = _idbSortedKeys(st, query, direction);
    let idx = keys.length ? 0 : -1;
    function fire() {
      Promise.resolve().then(() => {
        req.readyState = 'done';
        if (idx < 0 || idx >= keys.length) {
          req.result = null;
          if (tx) { tx._live.delete(req); _idbMaybeComplete(tx); }
        } else {
          const k = keys[idx];
          req.result = Object.setPrototypeOf({ // #209
            key: k,
            primaryKey: k,
            direction: direction || 'next',
            source: st,
            get value() { return st._data.get(k); },
            continue() { idx += 1; fire(); },
            advance(n) { idx += n; fire(); },
            update(v) { return _idbReqTx(tx, () => { st._data.set(k, v); return k; }, st); },
            delete() { return _idbReqTx(tx, () => { st._data.delete(k); return undefined; }, st); },
          }, IDBCursorWithValue.prototype);
        }
        if (typeof req.onsuccess === 'function') { try { req.onsuccess({ target: req, type: 'success' }); } catch (e) {} }
      });
    }
    if (tx) tx._live.add(req);
    fire();
    return req;
  };
  // Key cursors share the value-cursor shape; Dexie only reads .key from
  // them, so reusing the object is fine.
  st.openKeyCursor = function (query, direction) {
    return st.openCursor.call({ __tx: this.__tx }, query, direction);
  };
  st.createIndex = function (idxName, idxKeyPath, opts) {
    const n = String(idxName);
    if (st._indexes.has(n)) throw _idbErr('ConstraintError', 'index ' + n + ' already exists');
    const idx = Object.setPrototypeOf({ name: n, keyPath: idxKeyPath ?? n, unique: !!(opts && opts.unique), multiEntry: !!(opts && opts.multiEntry) }, IDBIndex.prototype); // #209
    const scan = () => {
      const pairs = [];
      st._data.forEach((v, k) => pairs.push([_idbExtractKeyPath(idx.keyPath, v), v, k]));
      return pairs;
    };
    const hit = (ik, q) => ik !== undefined && ik !== null && (Array.isArray(ik) ? ik.some((x) => _idbCmp(x, q) === 0) : _idbCmp(ik, q) === 0);
    idx.get = (q) => _idbReqTx(undefined, () => { for (const [ik, v] of scan()) if (hit(ik, q)) return v; return undefined; }, st);
    idx.getKey = (q) => _idbReqTx(undefined, () => { for (const [ik, , k] of scan()) if (hit(ik, q)) return k; return undefined; }, st);
    idx.getAll = (q) => _idbReqTx(undefined, () => scan().filter(([ik]) => hit(ik, q)).map(([, v]) => v), st);
    idx.getAllKeys = (q) => _idbReqTx(undefined, () => scan().filter(([ik]) => hit(ik, q)).map(([, , k]) => k), st);
    idx.count = (q) => _idbReqTx(undefined, () => scan().filter(([ik]) => hit(ik, q)).length, st);
    idx.openCursor = () => _idbReqTx(undefined, () => null, st);
    st._indexes.set(n, idx);
    return idx;
  };
  st.index = function (idxName) {
    const idx = st._indexes.get(String(idxName));
    if (!idx) throw _idbErr('NotFoundError', 'no index ' + idxName + ' on ' + st.name);
    return idx;
  };
  st.deleteIndex = function (idxName) { st._indexes.delete(String(idxName)); };
  Object.setPrototypeOf(st, IDBObjectStore.prototype); // #209
  return st;
}
const _IDB_STORE_FACADE_METHODS = ['add', 'put', 'get', 'getAll', 'getAllKeys', 'getKey', 'delete', 'clear', 'count', 'openCursor', 'openKeyCursor', 'index', 'createIndex', 'deleteIndex'];
// A store handle obtained from tx.objectStore() is bound to that tx (its ops
// ride the tx's complete tracking); the underlying data lives on the shared
// store record inside the database.
function _idbStoreFacade(tx, st) {
  const f = { name: st.name, keyPath: st.keyPath, autoIncrement: st.autoIncrement, indexNames: st.indexNames, transaction: tx };
  const bound = { __tx: tx };
  for (const m of _IDB_STORE_FACADE_METHODS) f[m] = st[m].bind(bound);
  Object.setPrototypeOf(f, IDBObjectStore.prototype); // #209
  return f;
}
function _idbTransaction(rec, db, storeNames, mode) {
  const names = (Array.isArray(storeNames) ? storeNames : [storeNames]).map(String);
  const scope = new Set(names);
  if (mode !== 'versionchange') {
    for (const n of names) {
      if (!rec.stores.has(n)) throw _idbErr('NotFoundError', 'no such object store: ' + n);
    }
  }
  const tx = {
    db,
    mode: mode || 'readonly',
    error: null,
    readyState: 'active',
    onabort: null,
    oncomplete: null,
    onerror: null,
    _live: new Set(),
    _done: false,
    _completeQueued: false,
    __scope: scope,
    get objectStoreNames() { return _idbStrList(() => Array.from(scope)); },
    objectStore(name) {
      const n = String(name);
      // versionchange scope is every store in the db, live: stores created
      // via db.createObjectStore() during the upgrade window must be
      // reachable through tx.objectStore() (Dexie's exact access shape).
      if (mode !== 'versionchange' && !scope.has(n)) throw _idbErr('NotFoundError', n + ' is not in this transaction\'s scope');
      const st = rec.stores.get(n);
      if (!st) throw _idbErr('NotFoundError', 'no such object store: ' + n);
      return _idbStoreFacade(tx, st);
    },
    abort() {
      if (!tx._done) {
        tx._done = true;
        tx.readyState = 'finished';
        tx.error = _idbErr('AbortError', 'transaction aborted');
        if (typeof tx.onabort === 'function') { try { tx.onabort({ type: 'abort', target: tx }); } catch (e) {} }
      }
    },
    commit() { _idbMaybeComplete(tx); },
    addEventListener(type, fn) { tx['on' + type] = fn; },
    removeEventListener(type, fn) { if (tx['on' + type] === fn) tx['on' + type] = null; },
  };
  _idbMaybeComplete(tx);
  Object.setPrototypeOf(tx, IDBTransaction.prototype); // #209
  return tx;
}
function _idbVersionTx(rec, db) {
  const tx = _idbTransaction(rec, db, Array.from(rec.stores.keys()), 'versionchange');
  tx.createObjectStore = function (n, opts) {
    const nm = String(n);
    if (rec.stores.has(nm)) throw _idbErr('ConstraintError', 'object store ' + nm + ' already exists');
    const store = _idbStore(nm, opts && opts.keyPath, opts && opts.autoIncrement);
    rec.stores.set(nm, store);
    tx.__scope.add(nm);
    return _idbStoreFacade(tx, store);
  };
  tx.deleteObjectStore = function (n) {
    const nm = String(n);
    tx.__scope.delete(nm);
    rec.stores.delete(nm);
  };
  return tx;
}
function _idbDatabase(rec) {
  const db = {
    // Per-connection, not on the shared rec: one connection's close() must
    // not poison the others' transactions (抖店 tcc opens @ecom/browser-tcc
    // from several modules — a shared flag killed the page with
    // InvalidStateError ×2 per navigation).
    _closed: false,
    name: rec.name,
    get version() { return rec.version; },
    get objectStoreNames() { return _idbStrList(() => Array.from(rec.stores.keys())); },
    createObjectStore(n, opts) {
      const nm = String(n);
      const existing = rec.stores.get(nm);
      const kp = opts && opts.keyPath;
      const ai = !!(opts && opts.autoIncrement);
      if (existing && existing.keyPath === (kp ?? null) && existing.autoIncrement === ai) return existing;
      if (existing) throw _idbErr('ConstraintError', 'object store ' + nm + ' already exists');
      const st = _idbStore(nm, kp, ai);
      rec.stores.set(nm, st);
      return st;
    },
    deleteObjectStore(n) { rec.stores.delete(String(n)); },
    transaction(storeNames, mode) {
      if (db._closed) throw _idbErr('InvalidStateError', 'database is closed');
      return _idbTransaction(rec, db, storeNames, mode);
    },
    close() { db._closed = true; },
    onversionchange: null,
    onabort: null,
    onerror: null,
    onclose: null,
    addEventListener(type, fn) { db['on' + type] = fn; },
    removeEventListener(type, fn) { if (db['on' + type] === fn) db['on' + type] = null; },
  };
  Object.setPrototypeOf(db, IDBDatabase.prototype); // #209
  return db;
}
// Standard IDB interface names (#209): real pages reference these directly
// — `x instanceof IDBRequest`, bare identifiers in module scope (抖店's
// "IDBRequest is not defined" loop). The engine's factories hand back plain
// literals, so each interface is an illegal-constructor function whose
// prototype those literals inherit: instanceof and Symbol.toStringTag hold,
// and `new IDBRequest()` throws the same TypeError Chrome does. Only
// IDBKeyRange and indexedDB are callable instances/namespaces, per spec.
function _idbIface(name, parent) {
  const f = function () { throw new TypeError('Illegal constructor'); };
  Object.defineProperty(f, 'name', { value: name, configurable: true });
  if (parent) Object.setPrototypeOf(f.prototype, parent.prototype);
  Object.defineProperty(f.prototype, Symbol.toStringTag, { value: name, configurable: true });
  _markNative(f);
  return f;
}
const IDBRequest = _idbIface('IDBRequest');
const IDBOpenDBRequest = _idbIface('IDBOpenDBRequest', IDBRequest);
const IDBDatabase = _idbIface('IDBDatabase');
const IDBTransaction = _idbIface('IDBTransaction');
const IDBObjectStore = _idbIface('IDBObjectStore');
const IDBIndex = _idbIface('IDBIndex');
const IDBCursor = _idbIface('IDBCursor');
const IDBCursorWithValue = _idbIface('IDBCursorWithValue', IDBCursor);
const IDBKeyRange = _idbIface('IDBKeyRange');
const IDBFactory = _idbIface('IDBFactory');
globalThis.IDBRequest = IDBRequest;
globalThis.IDBOpenDBRequest = IDBOpenDBRequest;
globalThis.IDBDatabase = IDBDatabase;
globalThis.IDBTransaction = IDBTransaction;
globalThis.IDBObjectStore = IDBObjectStore;
globalThis.IDBIndex = IDBIndex;
globalThis.IDBCursor = IDBCursor;
globalThis.IDBCursorWithValue = IDBCursorWithValue;
globalThis.IDBFactory = IDBFactory;
// #203: idb (jakearchibald/idb v7, bundled by 抖店's @ecom/browser-tcc)
// synthesizes DB-level conveniences — `(await openDB(...)).put(store, v, k)`
// — only when the operation names exist on the interface *prototypes*.
// #209 gave the interfaces constructors and the instances prototypes, but
// the methods stayed own props of the instance literals, so every
// `m in Interface.prototype` gate failed and `.put` resolved undefined.
// Prototype members forward to the receiver's own prop; calling one on a
// non-instance throws Illegal invocation, as Chrome does.
function _idbProtoOps(iface, names) {
  for (const m of names) {
    if (m in iface.prototype) continue;
    Object.defineProperty(iface.prototype, m, {
      writable: true, enumerable: true, configurable: true,
      value: function (...a) {
        if (!Object.prototype.hasOwnProperty.call(this, m)) {
          throw new TypeError('Illegal invocation');
        }
        return this[m].apply(this, a);
      },
    });
  }
}
_idbProtoOps(IDBFactory, ['open', 'deleteDatabase', 'databases', 'cmp']);
_idbProtoOps(IDBDatabase, ['createObjectStore', 'deleteObjectStore', 'transaction', 'close', 'addEventListener', 'removeEventListener']);
_idbProtoOps(IDBTransaction, ['abort', 'commit', 'objectStore', 'addEventListener', 'removeEventListener']);
_idbProtoOps(IDBObjectStore, ['put', 'add', 'get', 'getKey', 'getAll', 'getAllKeys', 'count', 'delete', 'clear', 'openCursor', 'openKeyCursor', 'index', 'createIndex', 'deleteIndex']);
_idbProtoOps(IDBIndex, ['get', 'getKey', 'getAll', 'getAllKeys', 'count', 'openCursor', 'openKeyCursor']);
_idbProtoOps(IDBCursor, ['advance', 'continue', 'continuePrimaryKey', 'update', 'delete']);
// Range instances inherit IDBKeyRange.prototype while keeping the existing
// per-factory closed/open semantics verbatim.
function _idbRangeShape(r) { Object.setPrototypeOf(r, IDBKeyRange.prototype); return r; }
IDBKeyRange.only = (v) => _idbRangeShape({ lower: v, upper: v, lowerOpen: false, upperOpen: false, includes(x) { return x === v; } });
IDBKeyRange.lowerBound = (v, open) => _idbRangeShape({ lower: v, upper: null, lowerOpen: !!open, upperOpen: false, includes(x) { return open ? x > v : x >= v; } });
IDBKeyRange.upperBound = (v, open) => _idbRangeShape({ lower: null, upper: v, lowerOpen: false, upperOpen: !!open, includes(x) { return open ? x < v : x <= v; } });
IDBKeyRange.bound = (l, u, lo, uo) => _idbRangeShape({ lower: l, upper: u, lowerOpen: !!lo, upperOpen: !!uo, includes(x) { return (lo ? x > l : x >= l) && (uo ? x < u : x <= u); } });
_markNative(IDBKeyRange.only); _markNative(IDBKeyRange.lowerBound);
_markNative(IDBKeyRange.upperBound); _markNative(IDBKeyRange.bound);
globalThis.IDBKeyRange = IDBKeyRange;
globalThis.indexedDB = {
  open(name, version) {
    if (version !== undefined && version !== null && (!Number.isInteger(version) || version < 1)) {
      throw new TypeError('The version provided (' + version + ') is not a positive integer');
    }
    const req = _idbNewRequest(null, null);
    Object.setPrototypeOf(req, IDBOpenDBRequest.prototype); // #209: open()/deleteDatabase() hand back open requests
    Promise.resolve().then(() => {
      const rec = _idbRecord(String(name));
      const current = rec.version;
      const target = version === undefined || version === null ? (current || 1) : version;
      if (target < current) {
        req.error = _idbErr('VersionError', 'requested version ' + target + ' is lower than current ' + current);
        req.readyState = 'done';
        if (typeof req.onerror === 'function') { try { req.onerror({ target: req, type: 'error' }); } catch (e) {} }
        return;
      }
      const db = _idbDatabase(rec);
      const finish = () => {
        req.result = db;
        req.transaction = null;
        req.readyState = 'done';
        if (typeof req.onsuccess === 'function') { try { req.onsuccess({ target: req, type: 'success' }); } catch (e) {} }
      };
      if (target > current) {
        const vtx = _idbVersionTx(rec, db);
        // The db is exposed as the request's result already during the
        // upgrade event — handlers install their schema through
        // e.target.result.createObjectStore. The versionchange tx rides the
        // REQUEST too (e.target.transaction — what Dexie reads), not just the
        // event object, and detaches again once the request settles (#54).
        req.result = db;
        req.transaction = vtx;
        try {
          rec.version = target;
          if (typeof req.onupgradeneeded === 'function') {
            req.onupgradeneeded({ type: 'upgradeneeded', target: req, oldVersion: current, newVersion: target, transaction: vtx });
          }
        } catch (e) {
          rec.version = current;
          req.result = undefined;
          req.transaction = null;
          req.error = e;
          req.readyState = 'done';
          if (typeof req.onerror === 'function') { try { req.onerror({ target: req, type: 'error' }); } catch (e2) {} }
          return;
        }
        _idbPersistMeta(rec);
        Promise.resolve().then(finish);
      } else {
        finish();
      }
    });
    return req;
  },
  deleteDatabase(name) {
    const req = _idbNewRequest(null, null);
    Object.setPrototypeOf(req, IDBOpenDBRequest.prototype); // #209: open()/deleteDatabase() hand back open requests
    Promise.resolve().then(() => {
      const nm = String(name);
      const rec = _idbRegistryMap.get(nm);
      try { globalThis.localStorage.removeItem('__diting_idb__' + nm); } catch (e) {}
      if (rec && typeof req.onupgradeneeded === 'function') {
        try { req.onupgradeneeded({ type: 'upgradeneeded', target: req, oldVersion: rec.version, newVersion: null, transaction: null }); } catch (e) {}
      }
      _idbRegistryMap.delete(nm);
      req.readyState = 'done';
      if (typeof req.onsuccess === 'function') { try { req.onsuccess({ target: req, type: 'success' }); } catch (e) {} }
    });
    return req;
  },
  databases() {
    const out = [];
    try {
      const ls = globalThis.localStorage;
      for (let i = 0; i < ls.length; i++) {
        const k = ls.key(i);
        if (k && k.indexOf('__diting_idb__') === 0) {
          const nm = k.slice('__diting_idb__'.length);
          let v = 0;
          try { v = (JSON.parse(ls.getItem(k)).v) | 0; } catch (e) {}
          out.push({ name: nm, version: v });
        }
      }
    } catch (e) {}
    return Promise.resolve(out);
  },
  cmp(a, b) { return _idbCmp(a, b); },
};
Object.setPrototypeOf(globalThis.indexedDB, IDBFactory.prototype);
