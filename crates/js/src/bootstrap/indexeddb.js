// In-memory IndexedDB. A pragmatic, pure-JS implementation covering the core of the API: open with
// versionchange/onupgradeneeded, object stores (in-line/out-of-line keys, autoIncrement), get/
// add/put/delete/clear/count/getAll(Keys), indexes (incl. multiEntry), cursors (next/prev) with
// continue/update/delete, and IDBKeyRange. Stored values are deep-copied with structuredClone;
// requests and transactions run asynchronously on the microtask queue, with transactions auto-
// committing once their request queue drains.
//
// Scope (documented limitations): databases live in memory for the realm's lifetime (no disk
// persistence — consistent with this engine's storage model); binary keys, prevunique/nextunique
// cursor directions, cross-connection blocked/versionchange semantics, and explicit-commit timing
// nuances are not implemented; Blob/File values are not specially handled.
(function () {
  if (typeof globalThis.indexedDB !== "undefined") { return; }
  function def(o, n, v) { Object.defineProperty(o, n, { value: v, enumerable: false, configurable: true, writable: true }); }
  var enqueue = (typeof queueMicrotask === "function") ? queueMicrotask : function (f) { Promise.resolve().then(f); };
  function err(name, msg) { return new globalThis.DOMException(msg || name, name); }
  function reportErr(e) { try { console.error(e); } catch (_) {} }

  // ---- keys: type rank (Number < Date < String < Array) + comparison ----------------------
  function rank(k) {
    if (typeof k === "number") { return 1; }
    if (k instanceof Date) { return 2; }
    if (typeof k === "string") { return 3; }
    if (Array.isArray(k)) { return 5; }
    return 0;
  }
  function validKey(k) {
    var r = rank(k);
    if (r === 0) { return false; }
    if (r === 1) { return !isNaN(k); }
    if (r === 2) { return !isNaN(k.getTime()); }
    if (r === 5) { for (var i = 0; i < k.length; i++) { if (!validKey(k[i])) { return false; } } }
    return true;
  }
  function cmpKey(a, b) {
    var ra = rank(a), rb = rank(b);
    if (ra !== rb) { return ra < rb ? -1 : 1; }
    if (ra === 1) { return a < b ? -1 : a > b ? 1 : 0; }
    if (ra === 2) { var at = a.getTime(), bt = b.getTime(); return at < bt ? -1 : at > bt ? 1 : 0; }
    if (ra === 3) { return a < b ? -1 : a > b ? 1 : 0; }
    if (ra === 5) {
      var n = Math.min(a.length, b.length);
      for (var i = 0; i < n; i++) { var c = cmpKey(a[i], b[i]); if (c !== 0) { return c; } }
      return a.length < b.length ? -1 : a.length > b.length ? 1 : 0;
    }
    return 0;
  }

  // ---- key paths ---------------------------------------------------------------------------
  function evalPath(value, path) {
    if (path === "") { return value; }
    var parts = String(path).split("."), cur = value;
    for (var i = 0; i < parts.length; i++) {
      if (cur == null || typeof cur !== "object") { return undefined; }
      cur = cur[parts[i]];
    }
    return cur;
  }
  function extractKey(value, keyPath) {
    if (keyPath == null) { return undefined; }
    if (Array.isArray(keyPath)) {
      var arr = [];
      for (var i = 0; i < keyPath.length; i++) {
        var k = evalPath(value, keyPath[i]);
        if (k === undefined) { return undefined; }
        arr.push(k);
      }
      return arr;
    }
    return evalPath(value, keyPath);
  }
  function injectKey(value, keyPath, key) {
    // Only meaningful for a string keyPath (autoIncrement requires a non-array keyPath).
    if (typeof keyPath !== "string") { return; }
    var parts = keyPath.split("."), cur = value;
    for (var i = 0; i < parts.length - 1; i++) {
      if (cur[parts[i]] == null || typeof cur[parts[i]] !== "object") { cur[parts[i]] = {}; }
      cur = cur[parts[i]];
    }
    cur[parts[parts.length - 1]] = key;
  }

  // ---- IDBKeyRange -------------------------------------------------------------------------
  function IDBKeyRange() {}
  function makeRange(lower, upper, lowerOpen, upperOpen) {
    var r = Object.create(IDBKeyRange.prototype);
    r.lower = lower; r.upper = upper; r.lowerOpen = !!lowerOpen; r.upperOpen = !!upperOpen;
    return r;
  }
  def(IDBKeyRange, "only", function (v) { if (!validKey(v)) { throw err("DataError", "invalid key"); } return makeRange(v, v, false, false); });
  def(IDBKeyRange, "lowerBound", function (v, open) { if (!validKey(v)) { throw err("DataError", "invalid key"); } return makeRange(v, undefined, open, false); });
  def(IDBKeyRange, "upperBound", function (v, open) { if (!validKey(v)) { throw err("DataError", "invalid key"); } return makeRange(undefined, v, false, open); });
  def(IDBKeyRange, "bound", function (l, u, lo, uo) {
    if (!validKey(l) || !validKey(u)) { throw err("DataError", "invalid key"); }
    var c = cmpKey(l, u);
    if (c > 0 || (c === 0 && (lo || uo))) { throw err("DataError", "lower bound greater than upper bound"); }
    return makeRange(l, u, lo, uo);
  });
  def(IDBKeyRange.prototype, "includes", function (k) { if (!validKey(k)) { throw err("DataError", "invalid key"); } return rangeIncludes(this, k); });
  function rangeIncludes(range, key) {
    if (range.lower !== undefined) { var c = cmpKey(key, range.lower); if (c < 0 || (c === 0 && range.lowerOpen)) { return false; } }
    if (range.upper !== undefined) { var c2 = cmpKey(key, range.upper); if (c2 > 0 || (c2 === 0 && range.upperOpen)) { return false; } }
    return true;
  }
  // A query may be undefined/null (everything), an IDBKeyRange, or a bare key (exact match).
  function inRange(query, key) {
    if (query === undefined || query === null) { return true; }
    if (query instanceof IDBKeyRange) { return rangeIncludes(query, key); }
    return cmpKey(query, key) === 0;
  }

  // ---- event-target mixin (supports on<type> props + addEventListener; error bubbles) ------
  function evtTarget(obj) {
    var listeners = {};
    def(obj, "addEventListener", function (type, cb) { if (cb) { (listeners[type] || (listeners[type] = [])).push(cb); } });
    def(obj, "removeEventListener", function (type, cb) { var l = listeners[type]; if (l) { var i = l.indexOf(cb); if (i >= 0) { l.splice(i, 1); } } });
    def(obj, "__fire", function (type, ev) {
      ev = ev || {};
      ev.type = type;
      if (ev.target === undefined) { ev.target = obj; }
      ev.currentTarget = obj;
      var on = obj["on" + type];
      if (typeof on === "function") { try { on.call(obj, ev); } catch (e) { reportErr(e); } }
      var l = listeners[type];
      if (l) { var snap = l.slice(); for (var i = 0; i < snap.length; i++) { try { snap[i].call(obj, ev); } catch (e2) { reportErr(e2); } } }
      return ev;
    });
  }

  // ---- requests ----------------------------------------------------------------------------
  function IDBRequest() {
    evtTarget(this);
    this.result = undefined; this.error = null; this.source = null; this.transaction = null;
    this.readyState = "pending"; this.onsuccess = null; this.onerror = null;
  }
  function IDBOpenDBRequest() { IDBRequest.call(this); this.onupgradeneeded = null; this.onblocked = null; }
  IDBOpenDBRequest.prototype = Object.create(IDBRequest.prototype);

  // ---- record helpers (records kept sorted ascending by key) -------------------------------
  function findRec(records, key) { for (var i = 0; i < records.length; i++) { if (cmpKey(records[i].key, key) === 0) { return i; } } return -1; }
  function insertRec(records, rec) { var i = 0; while (i < records.length && cmpKey(records[i].key, rec.key) < 0) { i++; } records.splice(i, 0, rec); }
  function firstInRange(records, query) { for (var i = 0; i < records.length; i++) { if (inRange(query, records[i].key)) { return records[i]; } } return null; }
  // Index view: [{indexKey, primaryKey, value}] sorted by (indexKey, primaryKey), expanding multiEntry.
  function indexRecords(sd, meta) {
    var out = [];
    for (var i = 0; i < sd.records.length; i++) {
      var rec = sd.records[i], ik = extractKey(rec.value, meta.keyPath);
      if (ik === undefined) { continue; }
      if (meta.multiEntry && Array.isArray(ik)) {
        for (var j = 0; j < ik.length; j++) { if (validKey(ik[j])) { out.push({ indexKey: ik[j], primaryKey: rec.key, value: rec.value }); } }
      } else if (validKey(ik)) {
        out.push({ indexKey: ik, primaryKey: rec.key, value: rec.value });
      }
    }
    out.sort(function (a, b) { var c = cmpKey(a.indexKey, b.indexKey); return c !== 0 ? c : cmpKey(a.primaryKey, b.primaryKey); });
    return out;
  }

  // ---- transactions ------------------------------------------------------------------------
  function IDBTransaction(db, storeNames, mode) {
    evtTarget(this);
    this.db = db; this.mode = mode || "readonly"; this.error = null;
    this.objectStoreNames = storeNames.slice().sort(cmpStr);
    this.oncomplete = null; this.onerror = null; this.onabort = null;
    this.__data = db.__data; this.__queue = []; this.__scheduled = false;
    this.__finished = false; this.__aborted = false; this.__stores = {};
    // Regular transactions auto-commit on the next microtask (even with no requests). Versionchange
    // transactions are driven explicitly by `open()`.
    if (this.mode !== "versionchange") { var self = this; this.__scheduled = true; enqueue(function () { self.__drain(); }); }
  }
  function cmpStr(a, b) { return a < b ? -1 : a > b ? 1 : 0; }
  def(IDBTransaction.prototype, "objectStore", function (name) {
    if (this.__finished) { throw err("InvalidStateError", "transaction finished"); }
    if (this.objectStoreNames.indexOf(name) < 0) { throw err("NotFoundError", "no object store named " + name); }
    if (!this.__stores[name]) {
      var sd = this.__data.stores[name];
      if (!sd) { throw err("NotFoundError", "no object store named " + name); }
      this.__stores[name] = new IDBObjectStore(this, sd);
    }
    return this.__stores[name];
  });
  def(IDBTransaction.prototype, "abort", function () {
    this.__aborted = true;
    if (!this.__finished) { var self = this; if (!this.__scheduled) { this.__scheduled = true; enqueue(function () { self.__drain(); }); } }
  });
  def(IDBTransaction.prototype, "commit", function () { var self = this; if (!this.__scheduled && !this.__finished) { this.__scheduled = true; enqueue(function () { self.__drain(); }); } });
  // Enqueue an operation under a fresh request and return it.
  def(IDBTransaction.prototype, "__push", function (op, source) {
    if (this.__finished) { throw err("TransactionInactiveError", "transaction is not active"); }
    var req = new IDBRequest(); req.source = source; req.transaction = this;
    this.__queue.push({ req: req, op: op });
    if (!this.__scheduled && this.mode !== "versionchange") { var self = this; this.__scheduled = true; enqueue(function () { self.__drain(); }); }
    return req;
  });
  // Enqueue an operation that re-fires success on an EXISTING request (cursor continuation).
  def(IDBTransaction.prototype, "__pushReq", function (req, op) {
    if (this.__finished) { throw err("TransactionInactiveError", "transaction is not active"); }
    this.__queue.push({ req: req, op: op });
  });
  def(IDBTransaction.prototype, "__drain", function () {
    while (this.__queue.length && !this.__aborted) {
      var task = this.__queue.shift(), req = task.req;
      try {
        req.result = task.op();
        req.error = null; req.readyState = "done";
        req.__fire("success", { target: req });
      } catch (e) {
        req.error = e; req.readyState = "done";
        req.__fire("error", { target: req });
        this.__fire("error", { target: req });
        // An unhandled request error aborts the transaction.
        this.__aborted = true; this.error = e;
      }
    }
    this.__finished = true; this.__scheduled = false;
    if (this.__aborted) { this.__fire("abort", { target: this }); }
    else { this.__fire("complete", { target: this }); }
  });

  // ---- object stores -----------------------------------------------------------------------
  function IDBObjectStore(txn, sd) {
    this.__txn = txn; this.__sd = sd;
    this.name = sd.name; this.keyPath = sd.keyPath; this.autoIncrement = sd.autoIncrement;
    this.transaction = txn; this.indexNames = Object.keys(sd.indexes).sort(cmpStr);
  }
  def(IDBObjectStore.prototype, "__write", function (value, key, noOverwrite) {
    if (this.__txn.mode === "readonly") { throw err("ReadOnlyError", "transaction is read-only"); }
    if (this.keyPath != null && key !== undefined) { throw err("DataError", "key provided for an in-line-key store"); }
    var sd = this.__sd, clonedValue = structuredClone(value);
    return this.__txn.__push(function () {
      var k = key;
      if (sd.keyPath != null) {
        k = extractKey(clonedValue, sd.keyPath);
        if (k === undefined && sd.autoIncrement) { k = sd.keyGen++; injectKey(clonedValue, sd.keyPath, k); }
      } else if (k === undefined) {
        if (sd.autoIncrement) { k = sd.keyGen++; } else { throw err("DataError", "no key and no key generator"); }
      }
      if (!validKey(k)) { throw err("DataError", "invalid key"); }
      if (sd.autoIncrement && typeof k === "number" && k >= sd.keyGen) { sd.keyGen = Math.floor(k) + 1; }
      var idx = findRec(sd.records, k);
      if (idx >= 0) {
        if (noOverwrite) { throw err("ConstraintError", "a record with that key already exists"); }
        sd.records[idx].value = clonedValue;
      } else {
        insertRec(sd.records, { key: k, value: clonedValue });
      }
      return k;
    }, this);
  });
  def(IDBObjectStore.prototype, "add", function (value, key) { return this.__write(value, key, true); });
  def(IDBObjectStore.prototype, "put", function (value, key) { return this.__write(value, key, false); });
  def(IDBObjectStore.prototype, "get", function (query) { var sd = this.__sd; return this.__txn.__push(function () { var r = firstInRange(sd.records, query); return r ? structuredClone(r.value) : undefined; }, this); });
  def(IDBObjectStore.prototype, "getKey", function (query) { var sd = this.__sd; return this.__txn.__push(function () { var r = firstInRange(sd.records, query); return r ? r.key : undefined; }, this); });
  def(IDBObjectStore.prototype, "getAll", function (query, count) { var sd = this.__sd; return this.__txn.__push(function () { var out = []; for (var i = 0; i < sd.records.length; i++) { if (inRange(query, sd.records[i].key)) { out.push(structuredClone(sd.records[i].value)); if (count && out.length >= count) { break; } } } return out; }, this); });
  def(IDBObjectStore.prototype, "getAllKeys", function (query, count) { var sd = this.__sd; return this.__txn.__push(function () { var out = []; for (var i = 0; i < sd.records.length; i++) { if (inRange(query, sd.records[i].key)) { out.push(sd.records[i].key); if (count && out.length >= count) { break; } } } return out; }, this); });
  def(IDBObjectStore.prototype, "count", function (query) { var sd = this.__sd; return this.__txn.__push(function () { var n = 0; for (var i = 0; i < sd.records.length; i++) { if (inRange(query, sd.records[i].key)) { n++; } } return n; }, this); });
  def(IDBObjectStore.prototype, "delete", function (query) {
    if (this.__txn.mode === "readonly") { throw err("ReadOnlyError", "transaction is read-only"); }
    var sd = this.__sd;
    return this.__txn.__push(function () { for (var i = sd.records.length - 1; i >= 0; i--) { if (inRange(query, sd.records[i].key)) { sd.records.splice(i, 1); } } return undefined; }, this);
  });
  def(IDBObjectStore.prototype, "clear", function () {
    if (this.__txn.mode === "readonly") { throw err("ReadOnlyError", "transaction is read-only"); }
    var sd = this.__sd;
    return this.__txn.__push(function () { sd.records.length = 0; return undefined; }, this);
  });
  def(IDBObjectStore.prototype, "createIndex", function (name, keyPath, options) {
    if (this.__txn.mode !== "versionchange") { throw err("InvalidStateError", "not in a versionchange transaction"); }
    var sd = this.__sd;
    if (sd.indexes[name]) { throw err("ConstraintError", "an index with that name already exists"); }
    sd.indexes[name] = { name: name, keyPath: keyPath, unique: !!(options && options.unique), multiEntry: !!(options && options.multiEntry) };
    this.indexNames = Object.keys(sd.indexes).sort(cmpStr);
    return new IDBIndex(this, sd.indexes[name]);
  });
  def(IDBObjectStore.prototype, "deleteIndex", function (name) {
    if (this.__txn.mode !== "versionchange") { throw err("InvalidStateError", "not in a versionchange transaction"); }
    if (!this.__sd.indexes[name]) { throw err("NotFoundError", "no index named " + name); }
    delete this.__sd.indexes[name];
    this.indexNames = Object.keys(this.__sd.indexes).sort(cmpStr);
  });
  def(IDBObjectStore.prototype, "index", function (name) { var m = this.__sd.indexes[name]; if (!m) { throw err("NotFoundError", "no index named " + name); } return new IDBIndex(this, m); });
  def(IDBObjectStore.prototype, "openCursor", function (query, direction) { return openCursor(this.__txn, this.__sd, null, this, query, direction, true); });
  def(IDBObjectStore.prototype, "openKeyCursor", function (query, direction) { return openCursor(this.__txn, this.__sd, null, this, query, direction, false); });

  // ---- indexes -----------------------------------------------------------------------------
  function IDBIndex(store, meta) {
    this.__store = store; this.__txn = store.__txn; this.__sd = store.__sd; this.__meta = meta;
    this.name = meta.name; this.keyPath = meta.keyPath; this.unique = meta.unique; this.multiEntry = meta.multiEntry;
    this.objectStore = store;
  }
  def(IDBIndex.prototype, "get", function (query) { var sd = this.__sd, meta = this.__meta; return this.__txn.__push(function () { var list = indexRecords(sd, meta); for (var i = 0; i < list.length; i++) { if (inRange(query, list[i].indexKey)) { return structuredClone(list[i].value); } } return undefined; }, this); });
  def(IDBIndex.prototype, "getKey", function (query) { var sd = this.__sd, meta = this.__meta; return this.__txn.__push(function () { var list = indexRecords(sd, meta); for (var i = 0; i < list.length; i++) { if (inRange(query, list[i].indexKey)) { return list[i].primaryKey; } } return undefined; }, this); });
  def(IDBIndex.prototype, "getAll", function (query, count) { var sd = this.__sd, meta = this.__meta; return this.__txn.__push(function () { var list = indexRecords(sd, meta), out = []; for (var i = 0; i < list.length; i++) { if (inRange(query, list[i].indexKey)) { out.push(structuredClone(list[i].value)); if (count && out.length >= count) { break; } } } return out; }, this); });
  def(IDBIndex.prototype, "getAllKeys", function (query, count) { var sd = this.__sd, meta = this.__meta; return this.__txn.__push(function () { var list = indexRecords(sd, meta), out = []; for (var i = 0; i < list.length; i++) { if (inRange(query, list[i].indexKey)) { out.push(list[i].primaryKey); if (count && out.length >= count) { break; } } } return out; }, this); });
  def(IDBIndex.prototype, "count", function (query) { var sd = this.__sd, meta = this.__meta; return this.__txn.__push(function () { var list = indexRecords(sd, meta), n = 0; for (var i = 0; i < list.length; i++) { if (inRange(query, list[i].indexKey)) { n++; } } return n; }, this); });
  def(IDBIndex.prototype, "openCursor", function (query, direction) { return openCursor(this.__txn, this.__sd, this.__meta, this, query, direction, true); });
  def(IDBIndex.prototype, "openKeyCursor", function (query, direction) { return openCursor(this.__txn, this.__sd, this.__meta, this, query, direction, false); });

  // ---- cursors -----------------------------------------------------------------------------
  function IDBCursor(txn, sd, meta, source, query, direction, withValue) {
    this.__txn = txn; this.__sd = sd; this.__meta = meta; this.source = source;
    this.__query = query; this.direction = direction || "next"; this.__withValue = withValue;
    this.__started = false; this.__req = null;
    this.key = undefined; this.primaryKey = undefined;
    if (withValue) { this.value = undefined; }
  }
  // Build the ordered list this cursor walks: store records (by key) or index records (by indexKey).
  def(IDBCursor.prototype, "__list", function () {
    if (this.__meta) { return indexRecords(this.__sd, this.__meta); }
    var out = [];
    for (var i = 0; i < this.__sd.records.length; i++) { out.push({ indexKey: this.__sd.records[i].key, primaryKey: this.__sd.records[i].key, value: this.__sd.records[i].value }); }
    return out;
  });
  def(IDBCursor.prototype, "__step", function (target) {
    var list = this.__list(), back = (this.direction === "prev" || this.direction === "prevunique"), found = null;
    for (var n = 0; n < list.length; n++) {
      var e = list[back ? list.length - 1 - n : n];
      if (!inRange(this.__query, e.indexKey)) { continue; }
      if (this.__started) {
        var c = cmpKey(e.indexKey, this.key);
        if (back ? c >= 0 : c <= 0) {
          if (c !== 0) { continue; }
          // same index key: disambiguate by primary key
          var pc = cmpKey(e.primaryKey, this.primaryKey);
          if (back ? pc >= 0 : pc <= 0) { continue; }
        }
      }
      if (target != null && (back ? cmpKey(e.indexKey, target) > 0 : cmpKey(e.indexKey, target) < 0)) { continue; }
      found = e; break;
    }
    if (!found) { this.key = undefined; this.primaryKey = undefined; if (this.__withValue) { this.value = undefined; } return null; }
    this.__started = true;
    this.key = found.indexKey; this.primaryKey = found.primaryKey;
    if (this.__withValue) { this.value = structuredClone(found.value); }
    return this;
  });
  def(IDBCursor.prototype, "continue", function (key) { var self = this; this.__txn.__pushReq(this.__req, function () { return self.__step(key !== undefined ? key : null); }); });
  def(IDBCursor.prototype, "advance", function (count) {
    var self = this, c = count;
    this.__txn.__pushReq(this.__req, function () { var r = null; for (var i = 0; i < c; i++) { r = self.__step(null); if (!r) { break; } } return r; });
  });
  def(IDBCursor.prototype, "update", function (value) {
    if (this.__txn.mode === "readonly") { throw err("ReadOnlyError", "transaction is read-only"); }
    var sd = this.__sd, pk = this.primaryKey, cloned = structuredClone(value);
    return this.__txn.__push(function () { var i = findRec(sd.records, pk); if (i >= 0) { sd.records[i].value = cloned; } return pk; }, this.source);
  });
  def(IDBCursor.prototype, "delete", function () {
    if (this.__txn.mode === "readonly") { throw err("ReadOnlyError", "transaction is read-only"); }
    var sd = this.__sd, pk = this.primaryKey;
    return this.__txn.__push(function () { var i = findRec(sd.records, pk); if (i >= 0) { sd.records.splice(i, 1); } return undefined; }, this.source);
  });
  function IDBCursorWithValue(txn, sd, meta, source, query, direction) { IDBCursor.call(this, txn, sd, meta, source, query, direction, true); }
  IDBCursorWithValue.prototype = Object.create(IDBCursor.prototype);
  function openCursor(txn, sd, meta, source, query, direction, withValue) {
    var cursor = withValue ? new IDBCursorWithValue(txn, sd, meta, source, query, direction)
                           : new IDBCursor(txn, sd, meta, source, query, direction, false);
    var req = txn.__push(function () { return cursor.__step(null); }, source);
    cursor.__req = req;
    return req;
  }

  // ---- database connection -----------------------------------------------------------------
  function IDBDatabase(name, data) {
    evtTarget(this);
    this.name = name; this.version = data.version; this.__data = data;
    this.objectStoreNames = Object.keys(data.stores).sort(cmpStr);
    this.onversionchange = null; this.onabort = null; this.onerror = null;
    this.__closed = false; this.__upgradeTxn = null;
  }
  def(IDBDatabase.prototype, "createObjectStore", function (name, opts) {
    if (!this.__upgradeTxn || this.__upgradeTxn.__finished) { throw err("InvalidStateError", "not in a versionchange transaction"); }
    var data = this.__data;
    if (data.stores[name]) { throw err("ConstraintError", "an object store with that name already exists"); }
    var keyPath = (opts && opts.keyPath !== undefined) ? opts.keyPath : null;
    var sd = { name: name, keyPath: keyPath, autoIncrement: !!(opts && opts.autoIncrement), keyGen: 1, indexes: {}, records: [] };
    data.stores[name] = sd;
    this.objectStoreNames = Object.keys(data.stores).sort(cmpStr);
    if (this.__upgradeTxn.objectStoreNames.indexOf(name) < 0) { this.__upgradeTxn.objectStoreNames.push(name); }
    return new IDBObjectStore(this.__upgradeTxn, sd);
  });
  def(IDBDatabase.prototype, "deleteObjectStore", function (name) {
    if (!this.__upgradeTxn || this.__upgradeTxn.__finished) { throw err("InvalidStateError", "not in a versionchange transaction"); }
    if (!this.__data.stores[name]) { throw err("NotFoundError", "no object store named " + name); }
    delete this.__data.stores[name];
    this.objectStoreNames = Object.keys(this.__data.stores).sort(cmpStr);
  });
  def(IDBDatabase.prototype, "transaction", function (storeNames, mode) {
    if (this.__closed) { throw err("InvalidStateError", "the database connection is closed"); }
    if (typeof storeNames === "string") { storeNames = [storeNames]; }
    storeNames = Array.prototype.slice.call(storeNames);
    if (!storeNames.length) { throw err("InvalidAccessError", "no object stores named"); }
    for (var i = 0; i < storeNames.length; i++) { if (!this.__data.stores[storeNames[i]]) { throw err("NotFoundError", "no object store named " + storeNames[i]); } }
    return new IDBTransaction(this, storeNames, mode || "readonly");
  });
  def(IDBDatabase.prototype, "close", function () { this.__closed = true; });

  // ---- versionchange event -----------------------------------------------------------------
  function IDBVersionChangeEvent(type, init) { this.type = type; this.oldVersion = (init && init.oldVersion) || 0; this.newVersion = (init && init.newVersion != null) ? init.newVersion : null; }

  // ---- the factory: indexedDB.open/deleteDatabase/databases/cmp ----------------------------
  var DBS = {};   // name -> { name, version, stores: { storeName -> storeData } }
  function open(name, version) {
    if (version !== undefined) { version = Math.floor(Number(version)); if (!(version >= 1)) { throw new TypeError("version must be >= 1"); } }
    var req = new IDBOpenDBRequest();
    enqueue(function () {
      var dbName = String(name), data = DBS[dbName];
      if (!data) { data = DBS[dbName] = { name: dbName, version: 0, stores: {} }; }
      var oldV = data.version, newV = (version === undefined) ? (oldV || 1) : version;
      if (newV < oldV) { req.error = err("VersionError", "requested version is less than the existing version"); req.readyState = "done"; req.__fire("error", { target: req }); return; }
      var conn = new IDBDatabase(dbName, data);
      req.result = conn;
      if (newV > oldV) {
        data.version = newV; conn.version = newV;
        var vtxn = new IDBTransaction(conn, Object.keys(data.stores), "versionchange");
        conn.__upgradeTxn = vtxn; req.transaction = vtxn;
        var ev = new IDBVersionChangeEvent("upgradeneeded", { oldVersion: oldV, newVersion: newV });
        ev.target = req;
        req.__fire("upgradeneeded", ev);
        vtxn.__drain();                 // run any data the handler queued, then fire vtxn.oncomplete
        conn.__upgradeTxn = null; req.transaction = null;
      }
      conn.version = data.version;
      req.readyState = "done";
      req.__fire("success", { target: req });
    });
    return req;
  }
  function deleteDatabase(name) {
    var req = new IDBOpenDBRequest();
    enqueue(function () {
      var dbName = String(name), data = DBS[dbName], oldV = data ? data.version : 0;
      delete DBS[dbName];
      req.result = undefined; req.readyState = "done";
      var ev = new IDBVersionChangeEvent("success", { oldVersion: oldV, newVersion: null });
      ev.target = req;
      req.__fire("success", ev);
    });
    return req;
  }
  function databases() {
    var out = [];
    for (var k in DBS) { if (Object.prototype.hasOwnProperty.call(DBS, k)) { out.push({ name: DBS[k].name, version: DBS[k].version }); } }
    return Promise.resolve(out);
  }
  function cmp(a, b) { if (!validKey(a) || !validKey(b)) { throw err("DataError", "invalid key"); } return cmpKey(a, b); }

  var factory = {};
  def(factory, "open", open);
  def(factory, "deleteDatabase", deleteDatabase);
  def(factory, "databases", databases);
  def(factory, "cmp", cmp);
  def(globalThis, "indexedDB", factory);

  // Expose the constructors (for instanceof checks and prototype access).
  def(globalThis, "IDBFactory", function () {});
  def(globalThis, "IDBRequest", IDBRequest);
  def(globalThis, "IDBOpenDBRequest", IDBOpenDBRequest);
  def(globalThis, "IDBDatabase", IDBDatabase);
  def(globalThis, "IDBTransaction", IDBTransaction);
  def(globalThis, "IDBObjectStore", IDBObjectStore);
  def(globalThis, "IDBIndex", IDBIndex);
  def(globalThis, "IDBCursor", IDBCursor);
  def(globalThis, "IDBCursorWithValue", IDBCursorWithValue);
  def(globalThis, "IDBKeyRange", IDBKeyRange);
  def(globalThis, "IDBVersionChangeEvent", IDBVersionChangeEvent);
})();
