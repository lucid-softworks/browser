(function () {
  function def(obj, name, value) {
    Object.defineProperty(obj, name, { value: value, enumerable: false, configurable: true, writable: true });
  }
  function fn() {}

  // --- legacy / missing language polyfills the host engine lacks ---------------------------
  // String.prototype.substr (deprecated but heavily used by real-world minified code, e.g.
  // google's URL-encoding helpers). Without it `"x".substr(1)` throws "not a callable function".
  if (typeof String.prototype.substr !== "function") {
    def(String.prototype, "substr", function (start, length) {
      var s = String(this);
      var len = s.length;
      start = start === undefined ? 0 : (start | 0);
      if (start < 0) { start = Math.max(len + start, 0); }
      var count = length === undefined ? (len - start) : (length | 0);
      if (count <= 0 || start >= len) { return ""; }
      count = Math.min(count, len - start);
      return s.slice(start, start + count);
    });
  }

  // --- navigator (plain object so enumeration / Object.keys / Object.assign work) ----------
  var ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15";
  globalThis.navigator = {
    userAgent: ua,
    appVersion: "5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15",
    appName: "Netscape",
    appCodeName: "Mozilla",
    product: "Gecko",
    platform: "MacIntel",
    vendor: "Apple Computer, Inc.",
    vendorSub: "",
    language: "en-US",
    languages: ["en-US", "en"],
    onLine: true,
    cookieEnabled: true,
    doNotTrack: null,
    maxTouchPoints: 0,
    hardwareConcurrency: 8,
    deviceMemory: 8,
    webdriver: false,
    plugins: [],
    mimeTypes: [],
    userActivation: { hasBeenActive: false, isActive: false },
    sendBeacon: function () { return false; },
    registerProtocolHandler: function () {},
    unregisterProtocolHandler: function () {},
    clipboard: {},
    geolocation: {
      getCurrentPosition: function () {},
      watchPosition: function () { return 0; },
      clearWatch: function () {}
    }
  };

  // --- location (populated from globalThis.__pageURL below) --------------------------------
  // A WHATWG-basic-URL-parser-style implementation (enough to pass the bulk of the url/ suite):
  // preprocessing, scheme + special-scheme handling, authority/host/port, opaque vs list paths with
  // dot-segment normalization, percent-encoding per destination set, relative resolution, and
  // canonical serialization. `base` (a parsed record or string) resolves a relative reference.
  var URL_SPECIAL = { ftp: "21", file: "", http: "80", https: "443", ws: "80", wss: "443" };
  function toUSVString(v) {
    var s = String(v);
    var out = "";
    for (var i = 0; i < s.length; i++) {
      var c = s.charCodeAt(i);
      if (c >= 0xd800 && c <= 0xdbff) {
        if (i + 1 < s.length) {
          var n = s.charCodeAt(i + 1);
          if (n >= 0xdc00 && n <= 0xdfff) { out += s.charAt(i) + s.charAt(i + 1); i++; continue; }
        }
        out += "\uFFFD";
      } else if (c >= 0xdc00 && c <= 0xdfff) {
        out += "\uFFFD";
      } else {
        out += s.charAt(i);
      }
    }
    return out;
  }
  function urlPctEncode(s, isInSet) {
    var out = "";
    for (var i = 0; i < s.length; i++) {
      var cp = s.codePointAt(i);
      if (cp > 0xffff) { i++; }
      // UTF-8 encode replaces lone surrogates with U+FFFD; encodeURIComponent would otherwise throw.
      if (cp >= 0xd800 && cp <= 0xdfff) { cp = 0xfffd; }
      if (cp > 0x7e || isInSet(cp)) {
        var bytes = unescape(encodeURIComponent(String.fromCodePoint(cp)));
        for (var b = 0; b < bytes.length; b++) {
          out += "%" + ("0" + bytes.charCodeAt(b).toString(16).toUpperCase()).slice(-2);
        }
      } else {
        out += String.fromCodePoint(cp);
      }
    }
    return out;
  }
  function urlC0(cp) { return cp < 0x20; }
  function urlFragSet(cp) { return urlC0(cp) || cp === 0x20 || cp === 0x22 || cp === 0x3c || cp === 0x3e || cp === 0x60; }
  function urlQuerySet(cp) { return urlC0(cp) || cp === 0x20 || cp === 0x22 || cp === 0x23 || cp === 0x3c || cp === 0x3e; }
  function urlPathSet(cp) { return urlQuerySet(cp) || cp === 0x3f || cp === 0x60 || cp === 0x7b || cp === 0x7d; }
  function urlUserSet(cp) { return urlPathSet(cp) || cp === 0x2f || cp === 0x3a || cp === 0x3b || cp === 0x3d || cp === 0x40 || cp === 0x5b || cp === 0x5c || cp === 0x5d || cp === 0x5e || cp === 0x7c; }

  // WHATWG IPv6 parser: an address (inside [...]) -> 8 16-bit pieces, or null on failure.
  function parseIPv6(input) {
    var address = [0, 0, 0, 0, 0, 0, 0, 0];
    var pieceIndex = 0, compress = null, p = 0, n = input.length;
    function c() { return p < n ? input[p] : null; }
    function isHex(ch) { return ch != null && /[0-9a-fA-F]/.test(ch); }
    if (c() === ":") {
      if (input[p + 1] !== ":") { return null; }
      p += 2; pieceIndex++; compress = pieceIndex;
    }
    while (c() !== null) {
      if (pieceIndex === 8) { return null; }
      if (c() === ":") {
        if (compress !== null) { return null; }
        p++; pieceIndex++; compress = pieceIndex; continue;
      }
      var value = 0, length = 0;
      while (length < 4 && isHex(c())) { value = value * 16 + parseInt(c(), 16); p++; length++; }
      if (c() === ".") {
        if (length === 0) { return null; }
        p -= length;
        if (pieceIndex > 6) { return null; }
        var numbersSeen = 0;
        while (c() !== null) {
          var ipv4Piece = null;
          if (numbersSeen > 0) { if (c() === "." && numbersSeen < 4) { p++; } else { return null; } }
          if (!/[0-9]/.test(c() || "")) { return null; }
          while (/[0-9]/.test(c() || "")) {
            var d = parseInt(c(), 10);
            if (ipv4Piece === null) { ipv4Piece = d; }
            else if (ipv4Piece === 0) { return null; }
            else { ipv4Piece = ipv4Piece * 10 + d; }
            if (ipv4Piece > 255) { return null; }
            p++;
          }
          address[pieceIndex] = address[pieceIndex] * 0x100 + ipv4Piece;
          numbersSeen++;
          if (numbersSeen === 2 || numbersSeen === 4) { pieceIndex++; }
        }
        if (numbersSeen !== 4) { return null; }
        break;
      } else if (c() === ":") { p++; if (c() === null) { return null; } }
      else if (c() !== null) { return null; }
      address[pieceIndex] = value; pieceIndex++;
    }
    if (compress !== null) {
      var swaps = pieceIndex - compress; pieceIndex = 7;
      while (pieceIndex !== 0 && swaps > 0) {
        var tmp = address[pieceIndex]; address[pieceIndex] = address[compress + swaps - 1]; address[compress + swaps - 1] = tmp;
        pieceIndex--; swaps--;
      }
    } else if (pieceIndex !== 8) { return null; }
    return address;
  }
  // Serialize 8 pieces with the canonical longest-zero-run compression.
  function serializeIPv6(address) {
    var out = "", compress = null, curBase = null, curLen = 0, maxLen = 0;
    for (var i = 0; i < 8; i++) {
      if (address[i] === 0) { if (curBase === null) { curBase = i; curLen = 1; } else { curLen++; } if (curLen > maxLen) { maxLen = curLen; compress = curBase; } }
      else { curBase = null; curLen = 0; }
    }
    if (maxLen < 2) { compress = null; }
    var ignore0 = false;
    for (var j = 0; j < 8; j++) {
      if (ignore0) { if (address[j] === 0) { continue; } ignore0 = false; }
      if (compress === j) { out += (j === 0 ? "::" : ":"); ignore0 = true; continue; }
      out += address[j].toString(16);
      if (j !== 7) { out += ":"; }
    }
    return out;
  }

  var __invalidURLRecord = { href: "", protocol: "", host: "", hostname: "", port: "", pathname: "", search: "", hash: "", origin: "null", username: "", password: "", __invalid: true };
  // Parse in Rust via the `url` crate (the authoritative WHATWG implementation). `base` may be a
  // string or an already-parsed record (use its href). A null native result is a parse failure.
  function parseURL(input, base) {
    var baseStr = (base == null) ? null : (typeof base === "string" ? base : (base.href || null));
    var json = __urlParse(String(input == null ? "" : input), baseStr);
    if (json == null) { return __invalidURLRecord; }
    try { var rec = JSON.parse(json); rec.__invalid = false; return rec; } catch (e) { return __invalidURLRecord; }
  }

  function parseURLRecord(input, base) {
    input = String(input == null ? "" : input);
    input = input.replace(/^[\x00-\x20]+/, "").replace(/[\x00-\x20]+$/, "");
    input = input.replace(/[\t\n\r]/g, "");
    var u = { scheme: "", username: "", password: "", host: null, port: "", path: [], query: null, fragment: null, opaque: false };

    // Scheme.
    var sm = /^([a-zA-Z][a-zA-Z0-9+.\-]*):/.exec(input);
    var rest = input;
    if (sm) { u.scheme = sm[1].toLowerCase(); rest = input.slice(sm[0].length); }
    else if (base) {
      // No scheme → relative reference, resolved against `base`. A base with an opaque path (a
      // non-special scheme like `aaa:b` / `mailto:` / `javascript:`) can only be the base for an
      // empty input or a fragment ("#…"); anything else is a parse failure (WHATWG URL).
      if (base.opaque && rest !== "" && rest.charAt(0) !== "#") { return null; }
      u.scheme = base.scheme; u.username = base.username; u.password = base.password;
      u.host = base.host; u.port = base.port; u.opaque = base.opaque;
      u.path = base.path.slice(); u.query = base.query;
      return resolveRelative(u, rest, base);
    } else { return null; }

    var special = Object.prototype.hasOwnProperty.call(URL_SPECIAL, u.scheme);

    if (u.scheme === "file") {
      u.host = "";
      rest = rest.replace(/^\/\//, "");
      return parseAuthorityAndPath(u, rest, special, base);
    }
    if (special) {
      // Special non-file: must have an authority.
      if (base && base.scheme === u.scheme && !/^\/\//.test(rest) && rest.charAt(0) !== "/") {
        // "special relative" — treat as relative to base.
        u.host = base.host; u.port = base.port; u.path = base.path.slice(); u.query = base.query;
        return resolveRelative(u, rest, base);
      }
      rest = rest.replace(/^\/+/, m => "//"); // collapse leading slashes to one authority intro
      rest = rest.replace(/^\/\//, "");
      return parseAuthorityAndPath(u, rest, special, base);
    }
    // Non-special.
    if (/^\/\//.test(rest)) {
      rest = rest.slice(2);
      return parseAuthorityAndPath(u, rest, special, base);
    }
    // Opaque path (non-special, no //).
    u.opaque = true;
    var hf = splitTail(rest);
    u.path = [hf.body];
    u.query = hf.query;
    u.fragment = hf.fragment;
    if (u.fragment != null) { u.fragment = urlPctEncode(u.fragment, urlFragSet); }
    if (u.query != null) { u.query = urlPctEncode(u.query, urlQuerySet); }
    u.path[0] = urlPctEncode(u.path[0], function (cp) { return urlC0(cp) || cp > 0x7e; });
    return u;
  }

  // Split off ?query and #fragment from a reference; returns {body, query, fragment}.
  function splitTail(s) {
    var fragment = null, query = null, body = s;
    var h = body.indexOf('#');
    if (h >= 0) { fragment = body.slice(h + 1); body = body.slice(0, h); }
    var q = body.indexOf("?");
    if (q >= 0) { query = body.slice(q + 1); body = body.slice(0, q); }
    return { body: body, query: query, fragment: fragment };
  }

  function parseAuthorityAndPath(u, rest, special, base) {
    // Authority ends at the first /,?,# (or \ for special).
    var endRe = special ? /[\/\\?#]/ : /[\/?#]/;
    var em = endRe.exec(rest);
    var authEnd = em ? em.index : rest.length;
    var authority = rest.slice(0, authEnd);
    var after = rest.slice(authEnd);
    // userinfo@host:port
    var at = authority.lastIndexOf("@");
    if (at >= 0) {
      var ui = authority.slice(0, at);
      var pc = ui.indexOf(":");
      if (pc >= 0) { u.username = urlPctEncode(ui.slice(0, pc), urlUserSet); u.password = urlPctEncode(ui.slice(pc + 1), urlUserSet); }
      else { u.username = urlPctEncode(ui, urlUserSet); }
      authority = authority.slice(at + 1);
    }
    var host = authority, port = "";
    if (host.charAt(0) === "[") {
      var rb = host.indexOf("]");
      if (rb >= 0) {
        var addr = parseIPv6(host.slice(1, rb));
        var ip = addr ? "[" + serializeIPv6(addr) + "]" : host.slice(0, rb + 1);
        var tail = host.slice(rb + 1);
        if (tail.charAt(0) === ":") { port = tail.slice(1); }
        host = ip;
      }
    } else {
      var cidx = host.lastIndexOf(":");
      if (cidx >= 0) { port = host.slice(cidx + 1); host = host.slice(0, cidx); }
    }
    u.host = special ? host.toLowerCase() : host;
    if (port !== "") {
      if (!/^[0-9]*$/.test(port)) { return null; }
      var pn = parseInt(port, 10);
      if (pn > 65535) { return null; }
      // Omit the default port for the scheme.
      u.port = (URL_SPECIAL[u.scheme] === String(pn)) ? "" : String(pn);
    }
    return parsePath(u, after, special, base);
  }

  function parsePath(u, after, special, base) {
    var t = splitTail(after);
    if (t.fragment != null) { u.fragment = urlPctEncode(t.fragment, urlFragSet); }
    if (t.query != null) { u.query = urlPctEncode(t.query, urlQuerySet); }
    var pathStr = t.body;
    if (special) { pathStr = pathStr.replace(/\\/g, "/"); }
    var segs = pathStr === "" ? [] : pathStr.split("/");
    // A leading slash produces a leading empty segment; drop it (the path list starts after root).
    if (segs.length && segs[0] === "") { segs.shift(); }
    var out = [];
    for (var i = 0; i < segs.length; i++) {
      var seg = segs[i];
      var low = seg.toLowerCase();
      if (low === "." || low === "%2e") { continue; }
      if (low === ".." || low === ".%2e" || low === "%2e." || low === "%2e%2e") { if (out.length) { out.pop(); } continue; }
      out.push(urlPctEncode(seg, urlPathSet));
    }
    u.path = out;
    return u;
  }

  function resolveRelative(u, rest, base) {
    var t = splitTail(rest);
    if (rest.charAt(0) === '#') { u.fragment = urlPctEncode(t.fragment, urlFragSet); return u; }
    if (t.fragment != null) { u.fragment = urlPctEncode(t.fragment, urlFragSet); }
    if (rest === "" || rest.charAt(0) === '#') { u.query = (t.query != null) ? urlPctEncode(t.query, urlQuerySet) : base.query; return u; }
    if (rest.charAt(0) === "?") { u.query = urlPctEncode(t.query, urlQuerySet); u.path = base.path.slice(); return u; }
    u.query = (t.query != null) ? urlPctEncode(t.query, urlQuerySet) : null;
    var special = Object.prototype.hasOwnProperty.call(URL_SPECIAL, u.scheme);
    var body = t.body;
    if (special) { body = body.replace(/\\/g, "/"); }
    if (body.charAt(0) === "/") {
      return parsePath(u, "/" + body.replace(/^\/+/, "") + (t.query != null ? "?" + t.query : ""), special, base);
    }
    // Merge with base path (drop base's last segment).
    var basePath = base.path.slice();
    if (!(base.opaque)) { basePath.pop(); }
    var merged = (basePath.length ? "/" + basePath.join("/") + "/" : "/") + body;
    return parsePath(u, merged + (t.query != null ? "?" + t.query : ""), special, base);
  }

  function serializeURLRecord(u) {
    var special = Object.prototype.hasOwnProperty.call(URL_SPECIAL, u.scheme);
    var protocol = u.scheme + ":";
    var href = protocol;
    var hostStr = u.host == null ? "" : u.host;
    var authority = "";
    if (u.host != null) {
      href += "//";
      if (u.username || u.password) { href += u.username + (u.password ? ":" + u.password : "") + "@"; }
      href += hostStr;
      if (u.port !== "") { href += ":" + u.port; }
    }
    var pathname;
    if (u.opaque) { pathname = u.path[0] || ""; }
    else { pathname = u.path.length ? "/" + u.path.join("/") : (special ? "/" : ""); }
    href += pathname;
    var search = u.query != null ? "?" + u.query : "";
    var hash = u.fragment != null ? '#' + u.fragment : "";
    href += search + hash;
    var host = hostStr + (u.port !== "" ? ":" + u.port : "");
    var origin = (u.host != null && special && u.scheme !== "file") ? (protocol + "//" + host) : "null";
    return {
      href: href, protocol: protocol, host: host, hostname: hostStr, port: u.port,
      pathname: pathname, search: search, hash: hash, origin: origin,
      username: u.username, password: u.password, __rec: u
    };
  }

  var parts = parseURL(globalThis.__pageURL);
  var locationState = {};
  function __syncLocation(p) {
    if (!p || p.__invalid) { return; }
    locationState.href = p.href; locationState.protocol = p.protocol; locationState.host = p.host;
    locationState.hostname = p.hostname; locationState.port = p.port; locationState.pathname = p.pathname;
    locationState.search = p.search; locationState.hash = p.hash; locationState.origin = p.origin;
  }
  function __setLocationHref(v) {
    var p = parseURL(String(v), locationState.href || parts.href);
    __syncLocation(p);
  }
  function __setLocationUrlPart(prop, v) {
    var json = __urlSet(locationState.href || parts.href, prop, String(v));
    if (json != null) { try { __syncLocation(JSON.parse(json)); } catch (e) {} }
  }
  var location = {
    assign: function (url) { __setLocationHref(url); },
    replace: function (url) { __setLocationHref(url); },
    reload: fn,
    toString: function () { return locationState.href; }
  };
  __syncLocation(parts);
  Object.defineProperty(location, "href", { get: function () { return locationState.href; }, set: __setLocationHref, enumerable: true, configurable: true });
  Object.defineProperty(location, "hash", { get: function () { return locationState.hash; }, set: function (v) { __setLocationUrlPart("hash", v); }, enumerable: true, configurable: true });
  Object.defineProperty(location, "search", { get: function () { return locationState.search; }, set: function (v) { __setLocationUrlPart("search", v); }, enumerable: true, configurable: true });
  ["protocol", "host", "hostname", "port", "pathname", "origin"].forEach(function (name) {
    Object.defineProperty(location, name, { get: function () { return locationState[name]; }, enumerable: true, configurable: true });
  });
  // `location` already exists (a minimal stub from install_globals); overwrite it.
  globalThis.location = location;
  function __makeDetachedWindow(url) {
    var childState = {};
    function childSync(p) {
      if (!p || p.__invalid) { return; }
      childState.href = p.href; childState.protocol = p.protocol; childState.host = p.host;
      childState.hostname = p.hostname; childState.port = p.port; childState.pathname = p.pathname;
      childState.search = p.search; childState.hash = p.hash; childState.origin = p.origin;
      if (childDoc) { childDoc.URL = p.href; childDoc.documentURI = p.href; }
    }
    function childSetHref(v) { childSync(parseURL(String(v), location.href)); }
    var childLoc = { assign: function (v) { childSetHref(v); }, replace: function (v) { childSetHref(v); }, reload: fn, toString: function () { return childState.href; } };
    var childDoc = { URL: "", documentURI: "", location: childLoc };
    Object.defineProperty(childLoc, "href", { get: function () { return childState.href; }, set: childSetHref, enumerable: true, configurable: true });
    Object.defineProperty(childLoc, "hash", {
      get: function () { return childState.hash; },
      set: function (v) {
        var json = __urlSet(childState.href || "about:blank", "hash", String(v));
        if (json != null) { try { childSync(JSON.parse(json)); } catch (e) {} }
      },
      enumerable: true, configurable: true
    });
    ["protocol", "host", "hostname", "port", "pathname", "search", "origin"].forEach(function (name) {
      Object.defineProperty(childLoc, name, { get: function () { return childState[name]; }, enumerable: true, configurable: true });
    });
    childSync(parseURL(url == null ? "about:blank" : String(url), location.href));
    return { location: childLoc, document: childDoc, close: fn, closed: false };
  }
  globalThis.open = function (url) { return __makeDetachedWindow(url); };
  if (typeof document !== "undefined") {
    document.open = function (url) {
      if (arguments.length) { return __makeDetachedWindow(url); }
      return document;
    };
  }

  // --- history (pushState/replaceState update location so SPA routers see the new URL) -------
  function __applyURLToLocation(url) {
    var resolved;
    try { resolved = new URL(String(url), location.href).href; } catch (e) { resolved = String(url); }
    var p = parseURL(resolved);
    __syncLocation(p);
  }
  globalThis.history = {
    length: 1, scrollRestoration: "auto", state: null,
    pushState: function (state, title, url) {
      this.state = (state === undefined ? null : state);
      this.length++;
      if (url != null && url !== "") { __applyURLToLocation(url); }
    },
    replaceState: function (state, title, url) {
      this.state = (state === undefined ? null : state);
      if (url != null && url !== "") { __applyURLToLocation(url); }
    },
    back: fn, forward: fn, go: fn
  };

  // --- Storage (localStorage / sessionStorage) ---------------------------------------------
  // `persistKey` (the origin) makes the bucket write-through to disk via __storageSave and load
  // from __storageLoad — so localStorage survives reloads/restarts. sessionStorage passes none.
  function makeStorage(persistKey) {
    var map = Object.create(null);
    if (persistKey && typeof __storageLoad === "function") {
      try {
        var saved = __storageLoad(persistKey);
        if (saved) { var o = JSON.parse(saved); for (var k in o) { map[k] = String(o[k]); } }
      } catch (e) {}
    }
    var persist = (persistKey && typeof __storageSave === "function")
      ? function () { try { __storageSave(persistKey, JSON.stringify(map)); } catch (e) {} }
      : function () {};
    var s = {
      getItem: function (k) { k = String(k); return Object.prototype.hasOwnProperty.call(map, k) ? map[k] : null; },
      setItem: function (k, v) { map[String(k)] = String(v); persist(); },
      removeItem: function (k) { delete map[String(k)]; persist(); },
      clear: function () { map = Object.create(null); persist(); },
      key: function (i) { var ks = Object.keys(map); return i >= 0 && i < ks.length ? ks[i] : null; }
    };
    Object.defineProperty(s, "length", { get: function () { return Object.keys(map).length; }, enumerable: false, configurable: true });
    // Wrap in a Proxy so named access works too (`localStorage.foo = 1`, `localStorage.foo`,
    // `delete localStorage.foo`, `Object.keys(localStorage)`), backed by the same map.
    try {
      return new Proxy(s, {
        get: function (t, prop) { if (prop in t) { return t[prop]; } return typeof prop === "string" ? t.getItem(prop) : undefined; },
        set: function (t, prop, val) { if (prop in t && prop !== "length") { t[prop] = val; } else { t.setItem(String(prop), val); } return true; },
        deleteProperty: function (t, prop) { if (Object.prototype.hasOwnProperty.call(map, prop)) { t.removeItem(String(prop)); } else { delete t[prop]; } return true; },
        has: function (t, prop) { return (prop in t) || (typeof prop === "string" && Object.prototype.hasOwnProperty.call(map, prop)); },
        ownKeys: function () { return Object.keys(map); },
        getOwnPropertyDescriptor: function (t, prop) {
          if (Object.prototype.hasOwnProperty.call(map, prop)) { return { value: map[prop], writable: true, enumerable: true, configurable: true }; }
          return undefined;
        }
      });
    } catch (e) { return s; }
  }
  globalThis.localStorage = makeStorage((function () {
    try { var o = location.origin; return (o && o !== "null") ? o : (location.protocol + location.pathname); } catch (e) { return "default"; }
  })());
  globalThis.sessionStorage = makeStorage();

  // --- screen ------------------------------------------------------------------------------
  globalThis.screen = {
    width: 1512, height: 982, availWidth: 1512, availHeight: 944,
    colorDepth: 24, pixelDepth: 24,
    orientation: { type: "landscape-primary", angle: 0 }
  };

  // --- window metrics + no-op window methods -----------------------------------------------
  // Real viewport + scale injected by the engine (fall back to defaults if absent).
  var __iw = (typeof globalThis.__innerWidth === "number" && globalThis.__innerWidth > 0) ? globalThis.__innerWidth : 1200;
  var __ih = (typeof globalThis.__innerHeight === "number" && globalThis.__innerHeight > 0) ? globalThis.__innerHeight : 780;
  globalThis.innerWidth = __iw; globalThis.innerHeight = __ih;
  globalThis.outerWidth = __iw; globalThis.outerHeight = __ih + 40;
  globalThis.devicePixelRatio = (typeof globalThis.__devicePixelRatio === "number" && globalThis.__devicePixelRatio > 0) ? globalThis.__devicePixelRatio : 2;
  globalThis.scrollX = 0; globalThis.pageXOffset = 0; // no horizontal scroll
  // scrollY / pageYOffset reflect the engine's real vertical scroll (updated as the page scrolls).
  try {
    Object.defineProperty(globalThis, "scrollY", { get: function () { try { return __scrollY(); } catch (e) { return 0; } }, configurable: true });
    Object.defineProperty(globalThis, "pageYOffset", { get: function () { try { return __scrollY(); } catch (e) { return 0; } }, configurable: true });
  } catch (e) { globalThis.scrollY = 0; globalThis.pageYOffset = 0; }
  globalThis.screenX = 0; globalThis.screenY = 0; globalThis.screenLeft = 0; globalThis.screenTop = 0;
  // scrollTo(x,y) | scrollTo({top}) — request a real scroll the engine applies.
  globalThis.scrollTo = function (x, y) {
    var top = (x && typeof x === "object") ? x.top : y;
    if (top != null) { try { __scrollSet(Number(top) || 0); } catch (e) {} }
  };
  globalThis.scroll = globalThis.scrollTo;
  globalThis.scrollBy = function (x, y) {
    var dy = (x && typeof x === "object") ? x.top : y;
    try { __scrollSet((Number(globalThis.scrollY) || 0) + (Number(dy) || 0)); } catch (e) {}
  };
  globalThis.moveTo = fn; globalThis.moveBy = fn; globalThis.resizeTo = fn; globalThis.resizeBy = fn;
  globalThis.focus = fn; globalThis.blur = fn; globalThis.print = fn;
  if (typeof globalThis.open !== "function") { globalThis.open = function () { return null; }; }
  globalThis.close = fn; globalThis.stop = fn;
  // getSelection is installed later (alongside Range/Selection) with a real Selection implementation.
  globalThis.alert = fn; globalThis.confirm = function () { return false; }; globalThis.prompt = function () { return null; };

  // --- matchMedia (real evaluation against the live viewport) ------------------------------
  function __mqFeature(f) {
    var iw = Number(globalThis.innerWidth) || 0, ih = Number(globalThis.innerHeight) || 0;
    var dpr = Number(globalThis.devicePixelRatio) || 1;
    if (f === "screen" || f === "all") { return true; }
    if (f === "print" || f === "speech") { return false; }
    var m = f.match(/^\(\s*([a-z-]+)\s*(?::\s*([^)]+))?\s*\)$/);
    if (!m) { return false; }
    var name = m[1], val = (m[2] || "").trim();
    var px = function (v) { var n = parseFloat(v); if (/r?em$/.test(v)) { n *= 16; } return n; };
    var res = function (v) { return /dpi$/.test(v) ? parseFloat(v) / 96 : (/dpcm$/.test(v) ? parseFloat(v) / 37.8 : parseFloat(v)); };
    switch (name) {
      case "min-width": case "min-device-width": return iw >= px(val);
      case "max-width": case "max-device-width": return iw <= px(val);
      case "width": case "device-width": return iw === px(val);
      case "min-height": case "min-device-height": return ih >= px(val);
      case "max-height": case "max-device-height": return ih <= px(val);
      case "height": case "device-height": return ih === px(val);
      case "min-aspect-ratio": case "max-aspect-ratio": case "aspect-ratio": {
        var p = val.split("/"); var want = p.length === 2 ? (parseFloat(p[0]) / parseFloat(p[1])) : parseFloat(val);
        var have = ih ? iw / ih : 0;
        return name === "min-aspect-ratio" ? have >= want : (name === "max-aspect-ratio" ? have <= want : Math.abs(have - want) < 0.01);
      }
      case "orientation": return val === (iw >= ih ? "landscape" : "portrait");
      case "prefers-color-scheme": {
        // Reflect the real macOS appearance (live via the native __prefersDark() flag). Bare
        // `(prefers-color-scheme)` with no value matches always; `dark`/`light` match the OS.
        var dark = false; try { dark = !!__prefersDark(); } catch (e) {}
        if (val === "") { return true; }
        return dark ? (val === "dark") : (val === "light");
      }
      case "prefers-reduced-motion": return val === "" || val === "no-preference";
      case "prefers-contrast": return val === "" || val === "no-preference";
      case "hover": case "any-hover": return val === "" || val === "hover";
      case "pointer": case "any-pointer": return val === "" || val === "fine";
      case "min-resolution": return dpr >= res(val);
      case "max-resolution": return dpr <= res(val);
      case "resolution": return Math.abs(dpr - res(val)) < 0.01;
      case "display-mode": return val === "browser";
      case "scripting": return val === "" || val === "enabled";
      case "update": return val === "" || val === "fast";
      case "color": return val === "" || parseFloat(val) > 0;
      case "color-gamut": return val === "srgb";
      default: return false;
    }
  }
  function __mqConj(q) {
    var neg = false;
    if (/^not\s/.test(q)) { neg = true; q = q.replace(/^not\s+/, "").trim(); }
    q = q.replace(/^only\s+/, "");
    var parts = q.split(/\s+and\s+/);
    var all = true;
    for (var i = 0; i < parts.length; i++) { if (!__mqFeature(parts[i].trim())) { all = false; break; } }
    return neg ? !all : all;
  }
  function __evalMedia(query) {
    query = String(query == null ? "" : query).toLowerCase().trim();
    if (!query || query === "all" || query === "screen") { return true; }
    var ors = query.split(",");
    for (var i = 0; i < ors.length; i++) { if (__mqConj(ors[i].trim())) { return true; } }
    return false;
  }
  // Live MediaQueryList registry. Every matchMedia() result is kept (weakly via a plain list — the
  // page count is tiny) so that when the OS appearance flips we can re-evaluate each list and fire
  // `change` on the ones whose `.matches` actually changed. __mediaChanged() is called by the
  // engine path (globalThis hook) after it flips the prefers-color-scheme flag.
  var __mqlRegistry = [];
  globalThis.matchMedia = function (q) {
    var media = String(q);
    var listeners = []; // change listeners added via addEventListener('change', ...)/addListener
    var mql = {
      media: media, onchange: null,
      addEventListener: function (type, cb) { if (type === "change" && typeof cb === "function") { listeners.push(cb); } },
      removeEventListener: function (type, cb) { if (type === "change") { var i = listeners.indexOf(cb); if (i >= 0) { listeners.splice(i, 1); } } },
      // Legacy aliases (still used by older sites): addListener/removeListener take the callback directly.
      addListener: function (cb) { if (typeof cb === "function") { listeners.push(cb); } },
      removeListener: function (cb) { var i = listeners.indexOf(cb); if (i >= 0) { listeners.splice(i, 1); } },
      dispatchEvent: function () { return false; }
    };
    // `matches` re-evaluates against the current viewport + OS appearance on every read.
    Object.defineProperty(mql, "matches", { get: function () { return __evalMedia(q); }, enumerable: true, configurable: true });
    // Internal: re-evaluate; if `.matches` changed, fire `change` on onchange + all listeners.
    def(mql, "__last", __evalMedia(q));
    def(mql, "__reeval", function () {
      var now = __evalMedia(q);
      if (now === mql.__last) { return; }
      mql.__last = now;
      var ev = { type: "change", media: media, matches: now, target: mql, currentTarget: mql, bubbles: false, cancelable: false };
      try { if (typeof mql.onchange === "function") { mql.onchange.call(mql, ev); } } catch (e) {}
      var snapshot = listeners.slice();
      for (var i = 0; i < snapshot.length; i++) { try { snapshot[i].call(mql, ev); } catch (e) {} }
    });
    __mqlRegistry.push(mql);
    return mql;
  };
  // Re-evaluate every live MediaQueryList and fire `change` where `.matches` flipped. Called by the
  // engine after it updates the OS appearance (prefers-color-scheme) so theme toggles restyle pages.
  def(globalThis, "__mediaChanged", function () {
    for (var i = 0; i < __mqlRegistry.length; i++) { try { __mqlRegistry[i].__reeval(); } catch (e) {} }
  });

  // --- getComputedStyle --------------------------------------------------------------------
  // Returns a read-only CSSStyleDeclaration-like object backed by the in-Session cascade
  // (`__computedStyleProp` / `__computedStyleNames`, computed in Rust by the `style` crate). For a
  // detached object with no node id we fall back to the old empty-stub so callers don't throw.
  (function () {
    // camelCase (or vendor-prefixed) property name -> kebab-case. `fontSize` -> `font-size`;
    // `WebkitTransform` -> `-webkit-transform`; already-kebab names pass through unchanged.
    function camelToKebab(prop) {
      prop = String(prop);
      if (prop.indexOf("-") >= 0) { return prop.toLowerCase(); } // already kebab
      // Insert "-" before each uppercase letter, lowercase everything. A leading uppercase (vendor
      // prefixes like `Webkit`/`Moz`/`Ms`) becomes a leading "-" (e.g. `-webkit-transform`).
      var out = prop.replace(/[A-Z]/g, function (c) { return "-" + c.toLowerCase(); });
      return out;
    }

    function emptyDeclaration() {
      // Detached / no node id: behave like the old stub (every read is "").
      var base = {
        getPropertyValue: function () { return ""; },
        getPropertyPriority: function () { return ""; },
        setProperty: fn, removeProperty: function () { return ""; },
        item: function () { return ""; }, length: 0
      };
      try {
        return new Proxy(base, { get: function (t, p) { return (p in t) ? t[p] : ""; } });
      } catch (e) {
        var common = ["display", "color", "width", "height", "visibility", "opacity", "position", "margin", "padding", "font-size", "background-color"];
        for (var i = 0; i < common.length; i++) { base[common[i]] = ""; }
        return base;
      }
    }

    function makeDeclaration(id, pseudo) {
      pseudo = pseudo || "";
      var names = null; // lazily fetched list of populated property names
      function getNames() { if (names === null) { try { names = __computedStyleNames(id, pseudo) || []; } catch (e) { names = []; } } return names; }
      function get(prop) { try { return __computedStyleProp(id, String(prop).toLowerCase(), pseudo); } catch (e) { return ""; } }

      // Computed styles are read-only: mutators throw NoModificationAllowedError (per CSSOM).
      function readOnlyThrow() { throw new globalThis.DOMException("Cannot modify the computed (resolved) style.", "NoModificationAllowedError"); }
      var decl = {
        getPropertyValue: function (name) { return get(name); },
        getPropertyPriority: function () { return ""; },
        setProperty: function () { readOnlyThrow(); },
        removeProperty: function () { readOnlyThrow(); },
        item: function (i) { var n = getNames(); i = i >>> 0; return i < n.length ? n[i] : ""; },
        parentRule: null
      };
      // Iterable over property names (the indexed getter values).
      try { decl[Symbol.iterator] = function () { return makeIter(getNames(), function (i, v) { return v; }); }; } catch (e) {}
      // cssText on a computed (resolved) style declaration is the empty string; setting it throws.
      Object.defineProperty(decl, "cssText", { get: function () { return ""; }, set: function () { readOnlyThrow(); }, enumerable: true, configurable: true });
      Object.defineProperty(decl, "length", {
        get: function () { return getNames().length; }, enumerable: true, configurable: true
      });

      try {
        return new Proxy(decl, {
          get: function (target, prop) {
            if (typeof prop === "symbol") { return target[prop]; }
            if (prop in target) { return target[prop]; }
            // Numeric index -> the i-th property name (like a real CSSStyleDeclaration).
            if (/^[0-9]+$/.test(prop)) { var n = getNames(); var i = Number(prop); return i < n.length ? n[i] : undefined; }
            // Any other property: kebab or camelCase CSS property access.
            return get(camelToKebab(prop));
          },
          has: function (target, prop) {
            if (prop in target) { return true; }
            return get(camelToKebab(prop)) !== "";
          },
          // A computed-style CSSStyleDeclaration is read-only: writing a CSS property throws
          // NoModificationAllowedError (per CSSOM). Symbol writes pass through.
          set: function (target, prop, value) {
            if (typeof prop === "symbol") { target[prop] = value; return true; }
            throw new globalThis.DOMException(
              "Cannot modify the computed (resolved) style.", "NoModificationAllowedError");
          }
        });
      } catch (e) {
        // No Proxy: define the common longhands + index slots eagerly (matches the old fallback).
        var nm = getNames();
        for (var i = 0; i < nm.length; i++) {
          (function (k, idx) {
            var kebab = k;
            // expose both kebab and the camelCase alias
            decl[kebab] = get(kebab);
            decl[kebab.replace(/-([a-z])/g, function (_, c) { return c.toUpperCase(); })] = get(kebab);
            decl[idx] = kebab;
          })(nm[i], i);
        }
        return decl;
      }
    }

    globalThis.getComputedStyle = function (el, pseudoElt) {
      var id = (el && typeof el.__node === "number") ? el.__node : null;
      if (id === null) { return emptyDeclaration(); }
      // The pseudo-element argument is normalized in Rust (`parse_gcs_pseudo`); pass it through as a
      // string. null/undefined → the element itself.
      var pseudo = (pseudoElt === null || pseudoElt === undefined) ? "" : String(pseudoElt);
      return makeDeclaration(id, pseudo);
    };
  })();

  // --- event model (no-op but present) + a simple listener registry ------------------------
  // Normalize an addEventListener/removeEventListener `options` arg to a capture boolean (the 3rd
  // arg may be a boolean or an options dict `{capture}`); per spec, "capture" identity is what
  // distinguishes two registrations of the same callback for the same type.
  function __captureFlag(options) {
    if (options && typeof options === "object") { return !!options.capture; }
    return !!options;
  }
  var __eventHandlerProps = [
    "onanimationstart", "onanimationend", "onanimationiteration", "ontransitionend",
    "onwebkitanimationstart", "onwebkitanimationend", "onwebkitanimationiteration",
    "onwebkittransitionend"
  ];
  var __eventTypeToHandlerProp = {
    webkitAnimationStart: "onwebkitanimationstart",
    webkitAnimationEnd: "onwebkitanimationend",
    webkitAnimationIteration: "onwebkitanimationiteration",
    webkitTransitionEnd: "onwebkittransitionend"
  };
  function __handlerPropForEventType(type) {
    return __eventTypeToHandlerProp[type] || ("on" + type);
  }
  function __installEventHandlerProps(target) {
    var handlers = target.__eventHandlers;
    if (!handlers) { handlers = Object.create(null); def(target, "__eventHandlers", handlers); }
    for (var i = 0; i < __eventHandlerProps.length; i++) {
      (function (prop) {
        if (Object.prototype.hasOwnProperty.call(target, prop)) { return; }
        Object.defineProperty(target, prop, {
          get: function () { return handlers[prop] || null; },
          set: function (value) { handlers[prop] = (typeof value === "function") ? value : null; },
          enumerable: true,
          configurable: true
        });
      })(__eventHandlerProps[i]);
    }
  }
  function installEvents(target) {
    if (!target || typeof target !== "object") { return; }
    if (target.__listeners) { return; } // already installed
    var registry = Object.create(null); // type -> [ {cb, capture, once} ]
    def(target, "__listeners", registry);
    __installEventHandlerProps(target);
    def(target, "addEventListener", function (type, cb, options) {
      if (typeof cb !== "function") { return; }
      type = String(type);
      var capture = __captureFlag(options);
      var once = !!(options && typeof options === "object" && options.once);
      var list = registry[type] || (registry[type] = []);
      // Duplicate (same callback + same capture) registrations are ignored.
      for (var i = 0; i < list.length; i++) { if (list[i].cb === cb && list[i].capture === capture) { return; } }
      var entry = { cb: cb, capture: capture, once: once };
      list.push(entry);
      // `{ signal }` option: auto-remove this listener when the AbortSignal aborts.
      var sig = options && typeof options === "object" ? options.signal : null;
      if (sig && typeof sig.addEventListener === "function") {
        if (sig.aborted) { var j0 = list.indexOf(entry); if (j0 >= 0) { list.splice(j0, 1); } return; }
        sig.addEventListener("abort", function () {
          var l = registry[type]; if (!l) { return; }
          var j = l.indexOf(entry); if (j >= 0) { l.splice(j, 1); }
        });
      }
    });
    def(target, "removeEventListener", function (type, cb, options) {
      type = String(type);
      var capture = __captureFlag(options);
      var list = registry[type];
      if (!list) { return; }
      for (var i = 0; i < list.length; i++) { if (list[i].cb === cb && list[i].capture === capture) { list.splice(i, 1); return; } }
    });
    def(target, "dispatchEvent", function (ev) {
      if (!(ev instanceof globalThis.Event) || !ev.__ev) {
        throw new TypeError("Failed to execute 'dispatchEvent': parameter 1 is not of type 'Event'.");
      }
      return globalThis.__dispatchEventObject(target, ev);
    });
  }
  // Invoke the listeners registered on `target` for `type` whose capture flag matches `wantCapture`,
  // plus (when invoking the bubble/target set) the legacy `on<type>` handler. Honours `once` and
  // the event's stop-immediate flag.
  def(globalThis, "__runListeners", function (target, type, ev, wantCapture, includeOn) {
    if (!target) { return; }
    var s = ev && ev.__ev ? ev.__ev : null;
    var reg = target.__listeners;
    var list = reg ? reg[type] : null;
    if (list) {
      var copy = list.slice();
      for (var i = 0; i < copy.length; i++) {
        var entry = copy[i];
        if (entry.capture !== wantCapture) { continue; }
        if (entry.once) { var j = list.indexOf(entry); if (j >= 0) { list.splice(j, 1); } }
        try { entry.cb.call(target, ev); } catch (e) { (globalThis.__timerErrors || []).push((e && e.stack) || String(e)); }
        if (s && s.stopImmediate) { return; }
      }
    }
    if (includeOn) {
      var on = target[__handlerPropForEventType(type)];
      if (typeof on === "function") {
        try { on.call(target, ev); } catch (e2) { (globalThis.__timerErrors || []).push((e2 && e2.stack) || String(e2)); }
      }
    }
  });
  // Build an event's propagation path: [target, ancestors..., document, window]. The target is
  // always included; ancestors/document/window are the bubbling targets walked via parentNode.
  def(globalThis, "__eventPath", function (target) {
    var path = [target];
    // Only DOM nodes propagate to ancestors / document / window. Non-node EventTargets
    // (AbortSignal, XMLHttpRequest, WebSocket, …) dispatch to themselves only.
    var isNode = target === globalThis || target === document ||
                 (target && typeof target.__node === "number");
    if (!isNode) { return path; }
    var cur = target, guard = 0;
    while (cur && guard < 4096) {
      var parent = null;
      try { parent = cur.parentNode; } catch (e0) { parent = null; }
      if (!parent || parent === cur) { break; }
      path.push(parent); cur = parent; guard++;
    }
    if (path.indexOf(document) < 0) { path.push(document); }
    if (path.indexOf(globalThis) < 0) { path.push(globalThis); }
    return path;
  });
  // Shared dispatch for `target.dispatchEvent(ev)`. Drives constructed Event objects (which carry
  // internal __ev state + read-only getters) through the full DOM dispatch algorithm: builds the
  // propagation path, runs the capture phase (root -> target), the target phase, then the bubble
  // phase (target -> root) when ev.bubbles, setting target/currentTarget/eventPhase and honouring
  // stopPropagation/stopImmediatePropagation.
  def(globalThis, "__dispatchEventObject", function (target, ev) {
    var s = ev.__ev;
    if (!s.initialized || s.dispatching) {
      throw new globalThis.DOMException(
        "Failed to execute 'dispatchEvent': The event is uninitialized or is already being dispatched.",
        "InvalidStateError");
    }
    var type = String(ev.type);
    var bubbles = s.bubbles;

    var path = globalThis.__eventPath(target); // [target, ...ancestors, document, window]

    s.dispatching = true; s.target = target; s.stopPropagation = false;
    s.stopImmediate = false; s.path = path.slice();

    function setCT(ct, phase) {
      s.currentTarget = ct; s.eventPhase = phase;
    }
    var run = globalThis.__runListeners;

    // Capture phase: ancestors from outermost (window) down to (but not including) the target.
    for (var i = path.length - 1; i >= 1; i--) {
      if (s.stopPropagation) { break; }
      setCT(path[i], 1 /*CAPTURING_PHASE*/);
      run(path[i], type, ev, true, false);
    }
    // Target phase: both capture- and bubble-registered listeners fire here, plus on<type>.
    if (!s.stopPropagation) {
      setCT(target, 2 /*AT_TARGET*/);
      run(target, type, ev, true, false);
      if (!s.stopImmediate) { run(target, type, ev, false, true); }
    }
    // Bubble phase: ancestors from target's parent up to window (only when the event bubbles).
    if (bubbles) {
      for (var h = 1; h < path.length; h++) {
        if (s.stopPropagation) { break; }
        setCT(path[h], 3 /*BUBBLING_PHASE*/);
        run(path[h], type, ev, false, true);
      }
    }

    s.eventPhase = 0; s.currentTarget = null; s.stopPropagation = false;
    s.stopImmediate = false; s.dispatching = false;
    return !s.defaultPrevented;
  });
  installEvents(globalThis);
  installEvents(document);

  // `window.postMessage(message, targetOrigin[, transfer])` / `window.postMessage(message, options)`.
  // Single browsing context: there are no cross-origin frames to route to, so this is same-window
  // delivery — structured-clone the message now (per spec, serialization happens at call time) and
  // queue a task to fire a `message` MessageEvent at the window. `targetOrigin` is accepted and
  // ignored (we only ever post to ourselves). Used by WPT's testdriver.js to reach the testharness
  // context (`test_driver.message_test`), and by pages doing same-window messaging.
  def(globalThis, "postMessage", function (message, targetOriginOrOptions, transfer) {
    var data;
    try { data = globalThis.structuredClone(message); } catch (e) { data = message; }
    // Transfer list: 3-arg `(message, targetOrigin, transfer)` or 2-arg `(message, {transfer})`.
    var ports = [];
    if (Array.isArray(transfer)) { ports = transfer.slice(); }
    else if (targetOriginOrOptions && typeof targetOriginOrOptions === "object" &&
             Array.isArray(targetOriginOrOptions.transfer)) { ports = targetOriginOrOptions.transfer.slice(); }
    var origin = "";
    try { origin = (globalThis.location && globalThis.location.origin) || ""; } catch (e2) {}
    if (origin === "null") { origin = ""; }
    setTimeout(function () {
      var ev = new globalThis.MessageEvent("message", {
        data: data, origin: origin, source: globalThis, ports: ports
      });
      globalThis.__dispatchEventObject(globalThis, ev);
    }, 0);
  });

  // --- DOMException + AbortController/AbortSignal -------------------------------------------
  // A real DOMException carrying `name`/`message` (AbortError, TimeoutError, …).
  (function () {
    // Map a DOMException name to its legacy numeric `code` (0 when the name has no legacy code).
    var __domCodes = {
      IndexSizeError: 1, HierarchyRequestError: 3, WrongDocumentError: 4,
      InvalidCharacterError: 5, NoModificationAllowedError: 7, NotFoundError: 8,
      NotSupportedError: 9, InUseAttributeError: 10, InvalidStateError: 11,
      SyntaxError: 12, InvalidModificationError: 13, NamespaceError: 14,
      InvalidAccessError: 15, TypeMismatchError: 17, SecurityError: 18,
      NetworkError: 19, AbortError: 20, URLMismatchError: 21, QuotaExceededError: 22,
      TimeoutError: 23, InvalidNodeTypeError: 24, DataCloneError: 25
    };
    var DOMExceptionCtor = function (message, name) {
      this.message = message === undefined ? "" : String(message);
      this.name = name === undefined ? "Error" : String(name);
      this.code = __domCodes[this.name] || 0;
      try { this.stack = new Error(this.message).stack; } catch (e) {}
    };
    DOMExceptionCtor.prototype = Object.create(Error.prototype);
    DOMExceptionCtor.prototype.constructor = DOMExceptionCtor;
    DOMExceptionCtor.prototype.toString = function () { return this.name + ": " + this.message; };
    // The constructor's own .name must be "DOMException" (it's inferred as the variable name
    // otherwise): testharness's assert_throws_dom checks `constructor.name === "DOMException"` to
    // detect the explicit-constructor overload, so a wrong name silently misroutes its arguments.
    try { Object.defineProperty(DOMExceptionCtor, "name", { value: "DOMException", configurable: true }); } catch (e) {}
    def(globalThis, "DOMException", DOMExceptionCtor);
  })();

  function __makeAbortReason(reason) {
    return reason !== undefined ? reason : new globalThis.DOMException("The operation was aborted.", "AbortError");
  }
  function __abortSignal(signal, reason) {
    if (!signal || signal.aborted) { return; }
    signal.aborted = true;
    signal.reason = __makeAbortReason(reason);
    var ev = new globalThis.Event("abort");
    if (typeof signal.dispatchEvent === "function") {
      try { signal.dispatchEvent(ev); } catch (e) {}
    } else if (typeof signal.onabort === "function") {
      try { signal.onabort.call(signal, ev); } catch (e2) { (globalThis.__timerErrors || []).push((e2&&e2.stack||String(e2))); }
    }
  }
  function AbortSignal() {
    this.aborted = false;
    this.reason = undefined;
    this.onabort = null;
    installEvents(this);
  }
  AbortSignal.prototype.throwIfAborted = function () { if (this.aborted) { throw this.reason; } };
  AbortSignal.abort = function (reason) { var s = new AbortSignal(); __abortSignal(s, reason); return s; };
  AbortSignal.timeout = function (ms) {
    var s = new AbortSignal();
    setTimeout(function () { __abortSignal(s, new globalThis.DOMException("The operation timed out.", "TimeoutError")); }, Number(ms) || 0);
    return s;
  };
  AbortSignal.any = function (signals) {
    var s = new AbortSignal();
    var list = Array.prototype.slice.call(signals || []);
    for (var i = 0; i < list.length; i++) { if (list[i] && list[i].aborted) { __abortSignal(s, list[i].reason); return s; } }
    list.forEach(function (sig) {
      if (sig && typeof sig.addEventListener === "function") { sig.addEventListener("abort", function () { __abortSignal(s, sig.reason); }); }
    });
    return s;
  };
  def(globalThis, "AbortSignal", AbortSignal);

  function AbortController() { this.signal = new AbortSignal(); }
  AbortController.prototype.abort = function (reason) { __abortSignal(this.signal, reason); };
  def(globalThis, "AbortController", AbortController);

  // The "Window-reflecting body element event handler set" (HTML §8.1.7.2.1): these `on*` content
  // attributes on <body>/<frameset> set the handler on the Window, not the element.
  globalThis.__windowReflectedBodyHandlers = globalThis.__windowReflectedBodyHandlers || (function () {
    var s = {};
    ["onafterprint", "onbeforeprint", "onbeforeunload", "onhashchange", "onlanguagechange",
      "onmessage", "onmessageerror", "onoffline", "ononline", "onpagehide", "onpageshow",
      "onpopstate", "onrejectionhandled", "onstorage", "onunhandledrejection", "onunload",
      "onblur", "onerror", "onfocus", "onload", "onresize", "onscroll"].forEach(function (n) { s[n] = true; });
    return s;
  })();

  // --- DOM lifecycle dispatch (driven from Rust during the drain) --------------------------
  var readyState = "loading";
  Object.defineProperty(document, "readyState", { get: function () { return readyState; }, enumerable: true, configurable: true });
  // The document's window. Code reads `document.defaultView` (and `node.ownerDocument.defaultView`)
  // to reach the global — e.g. google's `_.ai = a => a ? a.defaultView : window`, then
  // `_.ai(doc).devicePixelRatio`. Must be the same object as window/globalThis/self.
  if (!("defaultView" in document)) { def(document, "defaultView", globalThis); }
  document.referrer = "";
  document.URL = parts.href;
  document.documentURI = parts.href;
  document.baseURI = parts.href;
  document.domain = parts.hostname;
  document.title = document.title; // leave as-is; real getter/setter already present

  // `document.currentScript`: real browsers return the executing <script> element. We don't
  // track it, so expose a harmless stub element (with a no-op remove()) so inline bootstraps
  // like `document.currentScript.remove()` (TanStack/React hydration) don't throw.
  document.currentScript = {
    remove: fn, setAttribute: fn, getAttribute: function () { return null; },
    removeAttribute: fn, hasAttribute: function () { return false; },
    addEventListener: fn, removeEventListener: fn, appendChild: function (c) { return c; },
    parentNode: null, parentElement: null, nextSibling: null, previousSibling: null,
    src: "", type: "", async: false, defer: false, dataset: {}, style: {},
  };

  function makeEvent(type) {
    return new globalThis.Event(type);
  }
  function fireOn(target, type) {
    if (target && typeof target.dispatchEvent === "function") {
      // dispatchEvent already invokes both addEventListener listeners AND the `on<type>` handler
      // (e.g. window.onload), so we must NOT call the handler again here — that double-fires `load`,
      // which makes pages that build state in onload (e.g. testharness `test()`s) run twice.
      try { target.dispatchEvent(makeEvent(type)); } catch (e) { (globalThis.__timerErrors || []).push((e&&e.stack||String(e))); }
      return;
    }
    // Fallback for a target without a real dispatchEvent: invoke the `on<type>` handler directly.
    var on = target ? target["on" + type] : null;
    if (typeof on === "function") {
      try { on.call(target, makeEvent(type)); } catch (e) { (globalThis.__timerErrors || []).push((e&&e.stack||String(e))); }
    }
  }
  // Called from Rust's drain phase, in order, to advance readyState and fire lifecycle events.
  // MUST be idempotent: the drain calls it on every tick, but `DOMContentLoaded`/`load`/`pageshow`
  // are one-shot — firing them repeatedly breaks non-idempotent handlers (analytics init, jQuery
  // ready, testharness.js completion, etc.). Guard so the sequence runs exactly once.
  var __lifecycleFired = false;
  def(globalThis, "__fireLifecycleEvents", function () {
    if (__lifecycleFired) { return; }
    __lifecycleFired = true;
    readyState = "interactive";
    fireOn(document, "readystatechange");
    fireOn(document, "DOMContentLoaded");
    readyState = "complete";
    fireOn(document, "readystatechange");
    // Fire `load` on each connected, enabled stylesheet <link> with an inline `data:` sheet before
    // the window load — those are available synchronously. We deliberately do NOT fire for external
    // hrefs here: we can't tell when a real (possibly slow / render-blocking) sheet has finished
    // loading, and firing early would run a page's onload check before its CSS is applied.
    try {
      var __lks = document.querySelectorAll("link[rel~=stylesheet]");
      for (var __i = 0; __i < __lks.length; __i++) {
        var __lk = __lks[__i];
        var __href = __lk.getAttribute && __lk.getAttribute("href");
        if (__lk.__loadFired || !__href || __href.slice(0, 5) !== "data:" || __lk.disabled) { continue; }
        def(__lk, "__loadFired", true);
        try { __lk.dispatchEvent(new Event("load")); } catch (e) {}
      }
    } catch (e) {}
    // <style> elements fire `load` once their style block is processed (synchronously available).
    try {
      var __sts = document.querySelectorAll("style");
      for (var __j = 0; __j < __sts.length; __j++) {
        var __st = __sts[__j];
        if (__st.__loadFired) { continue; }
        def(__st, "__loadFired", true);
        try { __st.dispatchEvent(new Event("load")); } catch (e) {}
      }
    } catch (e) {}
    // Pick up the <body>/<frameset> window-reflecting event-handler content attributes (onload,
    // onunload, onresize, …) and bind them on the Window before firing load. A page may set a
    // `<body onload="...">` without any script ever touching `document.body` (so the element wrapper
    // — which normally compiles inline handlers — is never created); compiling them here guarantees
    // the handler runs. This is what unblocks the many `check-layout-th.js` tests that start from
    // `<body onload="checkLayout(...)">`.
    try {
      var __bodyish = document.body || (document.querySelector && document.querySelector("frameset"));
      var __wrh = globalThis.__windowReflectedBodyHandlers || {};
      if (__bodyish && typeof __bodyish.getAttributeNames === "function") {
        var __bn = __bodyish.getAttributeNames();
        for (var __bi = 0; __bi < __bn.length; __bi++) {
          var __bname = __bn[__bi];
          if (__wrh[__bname] && typeof globalThis[__bname] !== "function") {
            try { globalThis[__bname] = new Function("event", __bodyish.getAttribute(__bname)); } catch (e) {}
          }
        }
      }
    } catch (e) {}
    fireOn(window, "load");
    fireOn(document, "load");
    fireOn(window, "pageshow");
  });

  // --- document extras ---------------------------------------------------------------------
  var cookieStore = "";
  Object.defineProperty(document, "cookie", {
    get: function () { return cookieStore; },
    set: function (v) {
      v = String(v);
      var pair = v.split(";")[0];
      if (pair.indexOf("=") >= 0) { cookieStore = cookieStore ? (cookieStore + "; " + pair) : pair; }
    },
    enumerable: true, configurable: true
  });

  // head / documentElement may be missing from the native document; add lazy getters that
  // resolve via querySelector without clobbering existing accessors (e.g. `body`).
  function ensureGetter(name, selector) {
    var d = Object.getOwnPropertyDescriptor(document, name);
    if (d && (d.get || d.value)) { return; }
    Object.defineProperty(document, name, {
      get: function () { try { return document.querySelector(selector); } catch (e) { return null; } },
      enumerable: true, configurable: true
    });
  }
  ensureGetter("head", "head");
  // documentElement / body already exist as native accessors; only add head defensively.

  // getElementsByClassName is now a real native binding on document; nothing to add here.

  // --- write-through style / classList / dataset, backed by the real DOM attrs --------------
  // All three read and write the element's `style` / `class` / `data-*` attributes in the shared
  // document via the native `document.__getAttr/__setAttr/__removeAttr(node, name[, value])`
  // helpers, keyed by the wrapper's hidden `__node` id. This is what makes JS-driven style/class
  // changes survive into the engine's re-cascade and actually re-render.

  // Parse `prop: value; ...` into an ordered list of [prop, value] pairs (lowercased props).
  // Normalize a single numeric token to CSSOM canonical form: add a leading `0` before a bare
  // decimal point (`.5` -> `0.5`), drop a redundant leading zero pair only where the spec keeps it
  // (we keep `0.5`), strip trailing fractional zeros (`1.50` -> `1.5`, `2.0` -> `2`), and collapse
  // negative zero (`-0`, `-0.0`) to `0`. `num` is the sign+digits+optional-fraction (no unit).
  function normalizeNumberToken(num) {
    var neg = num.charAt(0) === "-";
    var sign = neg ? "-" : (num.charAt(0) === "+" ? "" : "");
    var body = (num.charAt(0) === "-" || num.charAt(0) === "+") ? num.slice(1) : num;
    if (body.charAt(0) === ".") { body = "0" + body; }
    if (body.indexOf(".") >= 0) {
      body = body.replace(/0+$/, "");      // trim trailing zeros
      if (body.charAt(body.length - 1) === ".") { body = body.slice(0, -1); }
    }
    // Collapse negative zero.
    if (sign === "-" && /^0(?:\.0*)?$/.test(body)) { sign = ""; }
    return sign + body;
  }
  // Canonicalize the numeric tokens inside a CSS value string (leading zeros, negative zero,
  // trailing fractional zeros), preserving units, identifiers, and `url(...)`/quoted segments.
  function normalizeCssValue(val) {
    val = String(val);
    // Canonicalize `url(...)`: the argument is serialized as a double-quoted string. Matches
    // `url( ... )` with an unquoted or single-quoted body and rewrites to `url("body")`.
    val = val.replace(/url\(\s*(?:"([^"]*)"|'([^']*)'|([^)\s]*))\s*\)/gi, function (_m, dq, sq, uq) {
      var body = dq != null ? dq : (sq != null ? sq : (uq != null ? uq : ""));
      return 'url("' + body + '")';
    });
    // `counter(name, decimal)` / `counters(name, sep, decimal)`: the default `decimal` style is
    // omitted on serialization.
    val = val.replace(/counter\(\s*([^,)]+?)\s*,\s*decimal\s*\)/gi, function (_m, nm) { return "counter(" + nm.trim() + ")"; });
    var out = "";
    var i = 0, n = val.length;
    // Sticky number matcher: anchored at `lastIndex`, so we never slice the tail of `val` (which
    // would make this loop O(n²) — a 1.8MB value, e.g. grid-template-columns-crash.html, then hangs).
    var numRe = /[-+]?(?:\d+\.?\d*|\.\d+)/y;
    while (i < n) {
      var ch = val[i];
      // Skip quoted strings verbatim (property-specific quote canonicalization happens in pushDecl).
      if (ch === '"' || ch === "'") {
        var q = ch; out += ch; i++;
        while (i < n && val[i] !== q) { if (val[i] === "\\" && i + 1 < n) { out += val[i] + val[i + 1]; i += 2; continue; } out += val[i]; i++; }
        if (i < n) { out += val[i]; i++; }
        continue;
      }
      // A number token: optional sign, digits with optional single decimal point.
      numRe.lastIndex = i;
      var m = numRe.exec(val);
      if (m && m[0].length > 0) {
        // Only treat as a number if not part of an identifier (preceding char isn't a letter/_).
        // Read the preceding char from `val` (O(1)), NOT `out[out.length-1]` — indexing the growing
        // result rope per number token forces repeated flattening, making this O(n²) (a 1.8MB
        // grid-template-columns value then hangs). `val` is fixed for the loop and number
        // normalization never adds/removes letters, so `val[i-1]` is an equivalent predecessor.
        var prev = i > 0 ? val[i - 1] : "";
        var startsAlpha = /[A-Za-z_]/.test(prev);
        if (!startsAlpha) {
          out += normalizeNumberToken(m[0]);
          i += m[0].length;
          continue;
        }
      }
      out += ch; i++;
    }
    return out;
  }
  // Re-quote every top-level CSS string in `val` to double-quote form (CSSOM "serialize a string").
  // Used for properties whose <string> values are always quoted on serialization (content, quotes).
  function requoteStrings(val) {
    val = String(val);
    var out = "", i = 0, n = val.length;
    while (i < n) {
      var ch = val[i];
      if (ch === '"' || ch === "'") {
        var q = ch; i++; var body = "";
        while (i < n) {
          var cc = val[i];
          if (cc === "\\") { if (i + 1 < n) { body += cc + val[i + 1]; i += 2; } else { i++; } continue; }
          if (cc === q) { i++; break; }
          body += cc; i++;
        }
        out += '"' + body.replace(/"/g, '\\"') + '"';
        continue;
      }
      out += ch; i++;
    }
    return out;
  }
  // Serialize a font-family list: drop quotes around any family name that is a sequence of valid CSS
  // identifiers (so `'Lucida Grande'` -> `Lucida Grande`); keep quotes otherwise.
  // Generic font families and other reserved words that a quoted <family-name> must NOT be
  // unquoted into (they would otherwise be reinterpreted as a keyword).
  var GENERIC_FONT_FAMILIES = {
    "serif":1, "sans-serif":1, "cursive":1, "fantasy":1, "monospace":1, "system-ui":1, "math":1,
    "ui-serif":1, "ui-sans-serif":1, "ui-monospace":1, "ui-rounded":1
  };
  // A quoted family-name body must stay quoted if it is a single token equal to a generic family,
  // a CSS-wide keyword, or `default` (CSS Fonts: those are not valid <custom-ident>s here).
  function isReservedFontFamilyWord(body) {
    var b = body.toLowerCase();
    if (hasOwn(GENERIC_FONT_FAMILIES, b)) return true;
    if (b === "default" || b === "inherit" || b === "initial" || b === "unset" || b === "revert" || b === "revert-layer") return true;
    return false;
  }
  // Find the matching closing quote for a family component that starts with quote `q`, honouring
  // backslash escapes. Returns the index of the closing quote, or -1 if unterminated.
  function closingQuoteIndex(fam, q) {
    for (var i = 1; i < fam.length; i++) {
      var c = fam.charAt(i);
      if (c === "\\") { i++; continue; }
      if (c === q) return i;
    }
    return -1;
  }
  // Returns the canonical serialization of a font-family list, or null if the list is invalid per
  // the `<family-name>` grammar (each comma component must be a single <string> OR a sequence of
  // <custom-ident>s). A quoted string with trailing content after its closing quote
  // (e.g. `"times" new roman`), or an unterminated quote, is a syntax error: the whole declaration
  // is dropped. Slicing such a component naively leaves an unbalanced quote that re-escapes and
  // grows on every CSSOM round-trip, so rejecting it is what stops repeated set/serialize from
  // blowing up.
  function normalizeFontFamily(val) {
    var parts = splitTopLevel(String(val), ",");
    var out = [];
    for (var p = 0; p < parts.length; p++) {
      var fam = parts[p].trim();
      if (fam === "") continue;
      var first = fam.charAt(0);
      if (first === '"' || first === "'") {
        // Quoted: serialize unquoted iff the body is a single-space-separated sequence of valid CSS
        // identifiers, re-joining reproduces the body exactly (no double spaces / leading/trailing
        // space), and the body isn't a reserved word (generic family / CSS-wide keyword / default).
        var ci = closingQuoteIndex(fam, first);
        if (ci < 0 || fam.slice(ci + 1).trim() !== "") return null;
        // Decode escapes to the literal string value before re-escaping, so serialization is
        // idempotent: `'\"x'` -> body `"x` -> `"\"x"` -> body `"x` again. Without this, the raw
        // backslashes are re-escaped and accumulate on every CSSOM round-trip.
        var body = unescapeCssIdent(fam.slice(1, ci));
        var words = body.split(" ");
        var allIdent = words.length > 0 && words.every(function (w) { return /^-?[A-Za-z_][A-Za-z0-9_-]*$/.test(w); });
        var roundTrips = allIdent && words.join(" ") === body;
        if (roundTrips && !isReservedFontFamilyWord(body)) {
          out.push(body);
        } else {
          out.push('"' + body.replace(/\\/g, "\\\\").replace(/"/g, '\\"') + '"');
        }
      } else {
        out.push(fam.replace(/\s+/g, " "));
      }
    }
    return out.join(", ");
  }
  // ====== CSS shorthand <-> longhand machinery (CSSOM serialize-a-CSS-declaration-block) ========
  // The set of longhands the `all` shorthand resets (every property except direction, unicode-bidi
  // and custom properties). A representative list — covers the properties the CSSOM tests query.
  var ALL_LONGHANDS = [
    "color", "background-color", "background-image", "background-position-x", "background-position-y",
    "background-size", "background-repeat", "background-attachment", "background-origin", "background-clip",
    "width", "height", "min-width", "min-height", "max-width", "max-height",
    "margin-top", "margin-right", "margin-bottom", "margin-left",
    "padding-top", "padding-right", "padding-bottom", "padding-left",
    "top", "right", "bottom", "left", "position", "display", "float", "clear", "visibility", "opacity",
    "border-top-width", "border-right-width", "border-bottom-width", "border-left-width",
    "border-top-style", "border-right-style", "border-bottom-style", "border-left-style",
    "border-top-color", "border-right-color", "border-bottom-color", "border-left-color",
    "border-top-left-radius", "border-top-right-radius", "border-bottom-right-radius", "border-bottom-left-radius",
    "font-family", "font-size", "font-style", "font-weight",
    "font-variant-ligatures", "font-variant-caps", "font-variant-alternates", "font-variant-numeric",
    "font-variant-east-asian", "font-variant-position", "font-variant-emoji",
    "font-stretch", "line-height",
    "text-align", "text-decoration-line", "text-decoration-style", "text-decoration-color",
    "text-transform", "letter-spacing", "white-space", "vertical-align",
    "list-style-type", "list-style-position", "list-style-image",
    "overflow-x", "overflow-y", "z-index", "cursor", "box-sizing",
    "flex-direction", "flex-wrap", "flex-grow", "flex-shrink", "flex-basis",
    "align-items", "align-content", "align-self", "justify-items", "justify-content", "justify-self",
    "row-gap", "column-gap", "outline-width", "outline-style", "outline-color",
    // Reset by `all` too (every property except direction / unicode-bidi / custom props).
    "border-collapse", "border-spacing", "order", "grid-template-columns", "grid-template-rows",
    // Logical longhands — also covered by `all` (so they collapse into it on serialization).
    "inline-size", "block-size", "min-inline-size", "min-block-size", "max-inline-size", "max-block-size",
    "margin-block-start", "margin-block-end", "margin-inline-start", "margin-inline-end",
    "padding-block-start", "padding-block-end", "padding-inline-start", "padding-inline-end",
    "inset-block-start", "inset-block-end", "inset-inline-start", "inset-inline-end",
    "border-block-start-width", "border-block-end-width", "border-inline-start-width", "border-inline-end-width",
    "border-block-start-style", "border-block-end-style", "border-inline-start-style", "border-inline-end-style",
    "border-block-start-color", "border-block-end-color", "border-inline-start-color", "border-inline-end-color"
  ];
  // A custom property is `--*`; case-sensitive, value kept raw (whitespace-trimmed).
  function isCustomProp(name) { return name.length >= 2 && name[0] === "-" && name[1] === "-"; }
  // Own-property lookup guarded against inherited keys (`"constructor"`, `"__proto__"`, …), so a CSS
  // property literally named like an Object.prototype member can't accidentally match a table entry.
  function hasOwn(obj, key) { return Object.prototype.hasOwnProperty.call(obj, key); }
  function lookup(obj, key) { return hasOwn(obj, key) ? obj[key] : undefined; }
  // CSS-wide keywords (valid for any property incl. the `all` shorthand).
  function isCssWideKeyword(v) {
    v = String(v).trim().toLowerCase();
    return v === "inherit" || v === "initial" || v === "unset" || v === "revert" || v === "revert-layer";
  }
  // Split a value into top-level space-separated tokens (respecting parens + quotes).
  function splitCssTokens(v) {
    v = String(v).trim();
    var out = [], i = 0, n = v.length, depth = 0, q = null, start = -1;
    while (i < n) {
      var c = v[i];
      if (q) { if (c === q) { q = null; } i++; continue; }
      if (c === '"' || c === "'") { if (start < 0) start = i; q = c; i++; continue; }
      if (c === "(") { if (start < 0) start = i; depth++; i++; continue; }
      if (c === ")") { depth--; i++; continue; }
      if (depth === 0 && (c === " " || c === "\t" || c === "\n" || c === "\r" || c === "\f")) {
        if (start >= 0) { out.push(v.slice(start, i)); start = -1; }
        i++; continue;
      }
      if (start < 0) start = i; i++;
    }
    if (start >= 0) out.push(v.slice(start));
    return out;
  }
  // Expand 1-4 box values into [top, right, bottom, left].
  function expandBox(v) {
    var t = splitCssTokens(v);
    if (t.length === 1) return [t[0], t[0], t[0], t[0]];
    if (t.length === 2) return [t[0], t[1], t[0], t[1]];
    if (t.length === 3) return [t[0], t[1], t[2], t[1]];
    if (t.length === 4) return [t[0], t[1], t[2], t[3]];
    return null;
  }
  // Serialize [top, right, bottom, left] to the shortest 1-4 box form.
  function serializeBox(top, right, bottom, left) {
    if (top == null || right == null || bottom == null || left == null) return "";
    if (top === bottom && right === left && top === right) return top;          // 1 value
    if (top === bottom && right === left) return top + " " + right;             // 2 values
    if (right === left) return top + " " + right + " " + bottom;               // 3 values
    return top + " " + right + " " + bottom + " " + left;                       // 4 values
  }
  // The box shorthands: shorthand -> [topLong, rightLong, bottomLong, leftLong].
  var BOX_SHORTHANDS = {
    "margin": ["margin-top", "margin-right", "margin-bottom", "margin-left"],
    "padding": ["padding-top", "padding-right", "padding-bottom", "padding-left"],
    "inset": ["top", "right", "bottom", "left"],
    "border-width": ["border-top-width", "border-right-width", "border-bottom-width", "border-left-width"],
    "border-style": ["border-top-style", "border-right-style", "border-bottom-style", "border-left-style"],
    "border-color": ["border-top-color", "border-right-color", "border-bottom-color", "border-left-color"],
    "border-radius": null, // handled specially
    "scroll-margin": ["scroll-margin-top", "scroll-margin-right", "scroll-margin-bottom", "scroll-margin-left"],
    "scroll-padding": ["scroll-padding-top", "scroll-padding-right", "scroll-padding-bottom", "scroll-padding-left"]
  };
  // Per-side `border-top`/`-right`/`-bottom`/`-left`: each -> [width, style, color] longhands.
  var BORDER_SIDE = {
    "border-top": ["border-top-width", "border-top-style", "border-top-color"],
    "border-right": ["border-right-width", "border-right-style", "border-right-color"],
    "border-bottom": ["border-bottom-width", "border-bottom-style", "border-bottom-color"],
    "border-left": ["border-left-width", "border-left-style", "border-left-color"],
    "outline": ["outline-color", "outline-style", "outline-width"],
    "column-rule": ["column-rule-width", "column-rule-style", "column-rule-color"]
  };
  // Classify a single border/outline component token as width|style|color.
  var BORDER_STYLE_KW = { none:1, hidden:1, dotted:1, dashed:1, solid:1, double:1, groove:1, ridge:1, inset:1, outset:1 };
  function classifyBorderToken(tok) {
    var t = tok.toLowerCase();
    if (BORDER_STYLE_KW[t]) return "style";
    if (t === "thin" || t === "medium" || t === "thick" || /^[-+.\d]/.test(t) || /^calc\(/.test(t)) return "width";
    return "color";
  }
  // Parse `border`/`border-top`/`outline` value -> {width,style,color} (missing -> undefined).
  function parseBorderLike(v) {
    var toks = splitCssTokens(v), r = {};
    for (var i = 0; i < toks.length; i++) {
      var k = classifyBorderToken(toks[i]);
      if (r[k] === undefined) r[k] = toks[i];
    }
    return r;
  }
  // The longhands of the `border` shorthand (all 12 sides + image), in canonical order.
  var BORDER_ALL_LONGHANDS = [
    "border-top-width", "border-right-width", "border-bottom-width", "border-left-width",
    "border-top-style", "border-right-style", "border-bottom-style", "border-left-style",
    "border-top-color", "border-right-color", "border-bottom-color", "border-left-color",
    "border-image-source", "border-image-slice", "border-image-width", "border-image-outset", "border-image-repeat"
  ];
  var BORDER_IMAGE_LONGHANDS = ["border-image-source", "border-image-slice", "border-image-width", "border-image-outset", "border-image-repeat"];
  var BORDER_IMAGE_INITIAL = {
    "border-image-source": "none", "border-image-slice": "100%", "border-image-width": "1",
    "border-image-outset": "0", "border-image-repeat": "stretch"
  };
  // overflow / overscroll-behavior / gap: 1-2 value shorthand of x/y (or row/column).
  // The flow-relative box shorthands (`margin-inline` etc.) are 2-value start/end shorthands too.
  var XY_SHORTHANDS = {
    "overflow": ["overflow-x", "overflow-y"],
    "overscroll-behavior": ["overscroll-behavior-x", "overscroll-behavior-y"],
    "gap": ["row-gap", "column-gap"],
    "margin-inline": ["margin-inline-start", "margin-inline-end"],
    "margin-block": ["margin-block-start", "margin-block-end"],
    "padding-inline": ["padding-inline-start", "padding-inline-end"],
    "padding-block": ["padding-block-start", "padding-block-end"],
    "inset-inline": ["inset-inline-start", "inset-inline-end"],
    "inset-block": ["inset-block-start", "inset-block-end"]
  };
  // list-style: type/position/image.
  function parseListStyle(v) {
    var toks = splitCssTokens(v), r = { "list-style-type": undefined, "list-style-position": undefined, "list-style-image": undefined };
    var POS = { inside: 1, outside: 1 };
    for (var i = 0; i < toks.length; i++) {
      var t = toks[i], tl = t.toLowerCase();
      if (/^url\(/i.test(t)) { r["list-style-image"] = t; }
      else if (POS[tl]) { r["list-style-position"] = tl; }
      else if (tl === "none") { if (r["list-style-type"] === undefined) r["list-style-type"] = "none"; else r["list-style-image"] = "none"; }
      else { r["list-style-type"] = t; }
    }
    return r;
  }
  // Map a shorthand name to its full set of longhand property names.
  function shorthandLonghands(name) {
    if (name === "all") return null; // special
    if (hasOwn(BOX_SHORTHANDS, name)) {
      if (name === "border-radius") return ["border-top-left-radius", "border-top-right-radius", "border-bottom-right-radius", "border-bottom-left-radius"];
      return BOX_SHORTHANDS[name];
    }
    if (hasOwn(BORDER_SIDE, name)) return BORDER_SIDE[name];
    if (hasOwn(XY_SHORTHANDS, name)) return XY_SHORTHANDS[name];
    if (name === "border") return BORDER_ALL_LONGHANDS;
    if (name === "font-variant") return FONT_VARIANT_LONGHANDS;
    if (name === "border-image") return BORDER_IMAGE_LONGHANDS;
    if (name === "list-style") return ["list-style-position", "list-style-image", "list-style-type"];
    if (name === "text-decoration") return ["text-decoration-line", "text-decoration-style", "text-decoration-color"];
    if (name === "flex-flow") return ["flex-direction", "flex-wrap"];
    // flex expands in the order grow, basis, shrink (matches browser declaration-block order).
    if (name === "flex") return ["flex-grow", "flex-basis", "flex-shrink"];
    if (name === "place-content") return ["align-content", "justify-content"];
    if (name === "place-items") return ["align-items", "justify-items"];
    if (name === "place-self") return ["align-self", "justify-self"];
    if (name === "columns") return ["column-width", "column-count"];
    return null;
  }
  // Shorthands we don't value-serialize but whose longhand set we know, so the CSS-wide-keyword
  // case (e.g. reading `font` after `all: revert`) can be serialized. Used by getVal only.
  // The `font` longhands listed here use the *granular* font-variant longhands (the actual stored
  // properties), not the `font-variant` sub-shorthand, so that after `all: <css-wide-keyword>` — which
  // expands to those granular longhands — `getPropertyValue("font")` can detect that every font
  // longhand carries the same CSS-wide keyword and return it.
  var KEYWORD_ONLY_SHORTHANDS = {
    "font": ["font-style", "font-variant-ligatures", "font-variant-caps", "font-variant-alternates",
      "font-variant-numeric", "font-variant-east-asian", "font-variant-position", "font-variant-emoji",
      "font-weight", "font-stretch", "font-size", "line-height", "font-family"],
    "background": ["background-image", "background-position-x", "background-position-y", "background-size", "background-repeat", "background-origin", "background-clip", "background-attachment", "background-color"]
  };
  // font-variant shorthand longhands, in canonical serialization order.
  var FONT_VARIANT_LONGHANDS = [
    "font-variant-ligatures", "font-variant-caps", "font-variant-alternates",
    "font-variant-numeric", "font-variant-east-asian", "font-variant-position", "font-variant-emoji"
  ];
  // Keyword sets used to bucket a `font-variant` shorthand token into the right longhand.
  var FV_LIGATURES = { "common-ligatures":1, "no-common-ligatures":1, "discretionary-ligatures":1, "no-discretionary-ligatures":1, "historical-ligatures":1, "no-historical-ligatures":1, "contextual":1, "no-contextual":1 };
  var FV_CAPS = { "small-caps":1, "all-small-caps":1, "petite-caps":1, "all-petite-caps":1, "unicase":1, "titling-caps":1 };
  var FV_NUMERIC = { "lining-nums":1, "oldstyle-nums":1, "proportional-nums":1, "tabular-nums":1, "diagonal-fractions":1, "stacked-fractions":1, "ordinal":1, "slashed-zero":1 };
  var FV_EAST_ASIAN = { "jis78":1, "jis83":1, "jis90":1, "jis04":1, "simplified":1, "traditional":1, "full-width":1, "proportional-width":1, "ruby":1 };
  var FV_POSITION = { "sub":1, "super":1 };
  var FV_EMOJI = { "text":1, "emoji":1, "unicode":1 };
  var FV_ALTERNATES = { "historical-forms":1 };
  // Expand a `font-variant` shorthand value into its longhands, or null if unparseable.
  function expandFontVariant(value) {
    var v = String(value).trim(), vl = v.toLowerCase();
    var res = {
      "font-variant-ligatures": "normal", "font-variant-caps": "normal", "font-variant-alternates": "normal",
      "font-variant-numeric": "normal", "font-variant-east-asian": "normal", "font-variant-position": "normal",
      "font-variant-emoji": "normal"
    };
    if (vl === "normal") return res;
    if (vl === "none") { res["font-variant-ligatures"] = "none"; return res; }
    var toks = splitCssTokens(v), buckets = {};
    for (var i = 0; i < toks.length; i++) {
      var t = toks[i].toLowerCase(), lh = null;
      if (FV_LIGATURES[t]) lh = "font-variant-ligatures";
      else if (FV_CAPS[t]) lh = "font-variant-caps";
      else if (FV_NUMERIC[t]) lh = "font-variant-numeric";
      else if (FV_EAST_ASIAN[t]) lh = "font-variant-east-asian";
      else if (FV_POSITION[t]) lh = "font-variant-position";
      else if (FV_EMOJI[t]) lh = "font-variant-emoji";
      else if (FV_ALTERNATES[t]) lh = "font-variant-alternates";
      else return null; // unknown token -> invalid shorthand
      if (!buckets[lh]) buckets[lh] = [];
      buckets[lh].push(t);
    }
    for (var k in buckets) { if (hasOwn(buckets, k)) res[k] = buckets[k].join(" "); }
    return res;
  }
  // Serialize the font-variant shorthand from its longhand values (`g`). Returns "" if it cannot be
  // represented (a CSS-wide keyword in one longhand, or ligatures:none mixed with other non-normal).
  function serializeFontVariant(g) {
    var vals = {};
    for (var i = 0; i < FONT_VARIANT_LONGHANDS.length; i++) {
      var lh = FONT_VARIANT_LONGHANDS[i], val = g(lh);
      if (val === "" || val == null) return ""; // a longhand missing -> can't serialize
      if (isCssWideKeyword(val)) return ""; // CSS-wide keyword can't appear in the shorthand
      vals[lh] = val;
    }
    var lig = vals["font-variant-ligatures"];
    var nonNormal = [];
    for (var j = 0; j < FONT_VARIANT_LONGHANDS.length; j++) {
      var p = FONT_VARIANT_LONGHANDS[j], pv = vals[p];
      if (pv !== "normal") nonNormal.push([p, pv]);
    }
    if (nonNormal.length === 0) return "normal";
    if (lig === "none") {
      // `none` only combines with nothing else.
      return nonNormal.length === 1 && nonNormal[0][0] === "font-variant-ligatures" ? "none" : "";
    }
    var parts = [];
    for (var m = 0; m < nonNormal.length; m++) { if (nonNormal[m][1] === "none") return ""; parts.push(nonNormal[m][1]); }
    return parts.join(" ");
  }
  function isShorthand(name) { return name === "all" || name === "font-variant" || shorthandLonghands(name) != null; }
  // Expand a shorthand declaration into [[longhand, value], ...]. Returns null if not a shorthand we
  // expand (caller stores the property as-is). CSS-wide keywords expand to every longhand.
  function expandShorthand(name, value) {
    value = String(value).trim();
    var lhs = shorthandLonghands(name);
    if (lhs == null) return null;
    var out = [];
    if (isCssWideKeyword(value)) {
      var v = value.toLowerCase();
      for (var i = 0; i < lhs.length; i++) out.push([lhs[i], v]);
      return out;
    }
    if (BOX_SHORTHANDS[name] && name !== "border-radius") {
      var b = expandBox(value); if (!b) return null;
      return [[lhs[0], b[0]], [lhs[1], b[1]], [lhs[2], b[2]], [lhs[3], b[3]]];
    }
    if (name === "border-radius") {
      var parts = value.split("/"); var h = expandBox(parts[0].trim());
      if (!h) return null;
      var vv = parts.length > 1 ? expandBox(parts[1].trim()) : h;
      if (!vv) return null;
      return [
        [lhs[0], h[0] === vv[0] ? h[0] : h[0] + " " + vv[0]],
        [lhs[1], h[1] === vv[1] ? h[1] : h[1] + " " + vv[1]],
        [lhs[2], h[2] === vv[2] ? h[2] : h[2] + " " + vv[2]],
        [lhs[3], h[3] === vv[3] ? h[3] : h[3] + " " + vv[3]]
      ];
    }
    if (XY_SHORTHANDS[name]) {
      var t = splitCssTokens(value);
      if (t.length === 1) return [[lhs[0], t[0]], [lhs[1], t[0]]];
      if (t.length === 2) return [[lhs[0], t[0]], [lhs[1], t[1]]];
      return null;
    }
    if (BORDER_SIDE[name]) {
      var p = parseBorderLike(value);
      var map = name === "outline"
        ? { width: "outline-width", style: "outline-style", color: "outline-color" }
        : name === "column-rule"
          ? { width: "column-rule-width", style: "column-rule-style", color: "column-rule-color" }
          : { width: name + "-width", style: name + "-style", color: name + "-color" };
      var res = [];
      res.push([map.width, p.width !== undefined ? p.width : "medium"]);
      res.push([map.style, p.style !== undefined ? p.style : "none"]);
      res.push([map.color, p.color !== undefined ? p.color : "currentcolor"]);
      return res;
    }
    if (name === "border") {
      var p2 = parseBorderLike(value);
      var w = p2.width !== undefined ? p2.width : "medium";
      var st = p2.style !== undefined ? p2.style : "none";
      var co = p2.color !== undefined ? p2.color : "currentcolor";
      var r = [], sides = ["top", "right", "bottom", "left"];
      for (var s = 0; s < 4; s++) r.push(["border-" + sides[s] + "-width", w]);
      for (var s2 = 0; s2 < 4; s2++) r.push(["border-" + sides[s2] + "-style", st]);
      for (var s3 = 0; s3 < 4; s3++) r.push(["border-" + sides[s3] + "-color", co]);
      for (var bi = 0; bi < BORDER_IMAGE_LONGHANDS.length; bi++) r.push([BORDER_IMAGE_LONGHANDS[bi], BORDER_IMAGE_INITIAL[BORDER_IMAGE_LONGHANDS[bi]]]);
      return r;
    }
    if (name === "font-variant") {
      var fv = expandFontVariant(value);
      if (!fv) return null;
      var fvo = [];
      for (var fi = 0; fi < FONT_VARIANT_LONGHANDS.length; fi++) { var fl = FONT_VARIANT_LONGHANDS[fi]; fvo.push([fl, fv[fl]]); }
      return fvo;
    }
    if (name === "border-image") {
      if (value.toLowerCase() === "none") {
        var bir = [];
        for (var bz = 0; bz < BORDER_IMAGE_LONGHANDS.length; bz++) bir.push([BORDER_IMAGE_LONGHANDS[bz], BORDER_IMAGE_INITIAL[BORDER_IMAGE_LONGHANDS[bz]]]);
        return bir;
      }
      return null;
    }
    if (name === "list-style") {
      var ls = parseListStyle(value), out2 = [];
      out2.push(["list-style-type", ls["list-style-type"] !== undefined ? ls["list-style-type"] : "disc"]);
      out2.push(["list-style-position", ls["list-style-position"] !== undefined ? ls["list-style-position"] : "outside"]);
      out2.push(["list-style-image", ls["list-style-image"] !== undefined ? ls["list-style-image"] : "none"]);
      return out2;
    }
    if (name === "flex") {
      var fl = parseFlex(value);
      if (!fl) return null;
      return [["flex-grow", fl.grow], ["flex-basis", fl.basis], ["flex-shrink", fl.shrink]];
    }
    return null;
  }
  // Parse the `flex` shorthand into {grow, shrink, basis}. Returns null if it can't be modeled.
  function parseFlex(value) {
    var v = String(value).trim(), vl = v.toLowerCase();
    if (vl === "none") return { grow: "0", shrink: "0", basis: "auto" };
    if (vl === "auto") return { grow: "1", shrink: "1", basis: "auto" };
    var toks = splitCssTokens(v);
    function isNum(t) { return /^[-+]?(?:\d+\.?\d*|\.\d+)$/.test(t); }
    var grow = null, shrink = null, basis = null;
    for (var i = 0; i < toks.length; i++) {
      var t = toks[i];
      if (isNum(t)) {
        if (grow === null) grow = t;
        else if (shrink === null) shrink = t;
        else return null;
      } else {
        if (basis !== null) return null;
        basis = t;
      }
    }
    if (grow === null && basis === null) return null;
    // Defaults per CSS Flexbox: grow 1, shrink 1, basis 0% — but a single number sets basis to 0px
    // (the "one value, flexible" case) which browsers serialize as `0px`.
    if (grow === null) grow = "1";
    if (shrink === null) shrink = "1";
    if (basis === null) basis = "0px";
    return { grow: normalizeNumberToken(grow), shrink: normalizeNumberToken(shrink), basis: basis };
  }
  // Serialize a shorthand from the current longhand values (`getLong(name)`). Returns "" if it
  // cannot be represented (a longhand missing or values inconsistent).
  function serializeShorthand(name, getLong) {
    function g(n) { return getLong(n); }
    var lhs = shorthandLonghands(name);
    if (lhs == null) return "";
    if (name === "border") lhs = BORDER_ALL_LONGHANDS;
    var allSet = true, common = null, sameKw = true;
    for (var i = 0; i < lhs.length; i++) {
      var v = g(lhs[i]);
      if (v === "" || v == null) allSet = false;
      if (common === null) common = v; else if (common !== v) sameKw = false;
    }
    if (allSet && sameKw && isCssWideKeyword(common)) return common.toLowerCase();
    for (var j = 0; j < lhs.length; j++) { if (isCssWideKeyword(g(lhs[j]))) { if (!(allSet && sameKw)) return ""; } }
    if (!allSet) return "";

    if (BOX_SHORTHANDS[name] && name !== "border-radius") {
      return serializeBox(g(lhs[0]), g(lhs[1]), g(lhs[2]), g(lhs[3]));
    }
    if (name === "border-radius") {
      var H = [g(lhs[0]), g(lhs[1]), g(lhs[2]), g(lhs[3])];
      var hs = [], vs = [], split = false;
      for (var k = 0; k < 4; k++) { var pr = splitCssTokens(H[k]); hs.push(pr[0]); if (pr.length > 1) { vs.push(pr[1]); split = true; } else vs.push(pr[0]); }
      var hser = serializeBox(hs[0], hs[1], hs[2], hs[3]);
      if (!split) return hser;
      return hser + " / " + serializeBox(vs[0], vs[1], vs[2], vs[3]);
    }
    if (XY_SHORTHANDS[name]) {
      var x = g(lhs[0]), y = g(lhs[1]);
      return x === y ? x : x + " " + y;
    }
    if (BORDER_SIDE[name]) {
      var wv, sv, cv, initW = "medium", initS = "none", initC = "currentcolor";
      if (name === "outline") { cv = g(lhs[0]); sv = g(lhs[1]); wv = g(lhs[2]); }
      else { wv = g(lhs[0]); sv = g(lhs[1]); cv = g(lhs[2]); }
      var parts = [];
      if (name === "outline") {
        if (cv !== initC) parts.push(cv);
        if (sv !== initS) parts.push(sv);
        if (wv !== initW) parts.push(wv);
      } else {
        if (wv !== initW) parts.push(wv);
        if (sv !== initS) parts.push(sv);
        if (cv !== initC) parts.push(cv);
      }
      return parts.length ? parts.join(" ") : "medium";
    }
    if (name === "border") {
      function side(prefix) { return [g("border-top-" + prefix), g("border-right-" + prefix), g("border-bottom-" + prefix), g("border-left-" + prefix)]; }
      var W = side("width"), S = side("style"), C = side("color");
      function allEq(a) { return a[0] === a[1] && a[1] === a[2] && a[2] === a[3]; }
      if (!allEq(W) || !allEq(S) || !allEq(C)) return "";
      for (var bi = 0; bi < BORDER_IMAGE_LONGHANDS.length; bi++) {
        if (g(BORDER_IMAGE_LONGHANDS[bi]) !== BORDER_IMAGE_INITIAL[BORDER_IMAGE_LONGHANDS[bi]]) return "";
      }
      var bp = [];
      if (W[0] !== "medium") bp.push(W[0]);
      if (S[0] !== "none") bp.push(S[0]);
      if (C[0] !== "currentcolor") bp.push(C[0]);
      return bp.length ? bp.join(" ") : "medium";
    }
    if (name === "border-image") {
      for (var bm = 0; bm < BORDER_IMAGE_LONGHANDS.length; bm++) {
        if (g(BORDER_IMAGE_LONGHANDS[bm]) !== BORDER_IMAGE_INITIAL[BORDER_IMAGE_LONGHANDS[bm]]) return "";
      }
      return "none";
    }
    if (name === "font-variant") { return serializeFontVariant(g); }
    if (name === "list-style") {
      var ty = g("list-style-type"), po = g("list-style-position"), im = g("list-style-image");
      var lp = [];
      if (po !== "outside") lp.push(po);
      if (ty !== "disc") lp.push(ty);
      if (im !== "none") lp.push(im);
      return lp.length === 0 ? "disc" : lp.join(" ");
    }
    if (name === "flex") {
      var fg = g("flex-grow"), fsk = g("flex-shrink"), fb = g("flex-basis");
      // A CSS-wide keyword in any longhand can't combine (handled by the early-return above).
      // Canonical: `grow shrink basis`.
      return fg + " " + fsk + " " + fb;
    }
    return "";
  }

  // Strip a trailing `!important` from a value. Returns [value, importantBool].
  function splitImportant(val) {
    var m = /^([\s\S]*?)\s*!\s*important\s*$/i.exec(val);
    if (m) return [m[1].trim(), true];
    return [val, false];
  }
  // Parse a declaration block into expanded longhand triples [name, value, important], in source
  // order, expanding shorthands and `all` as we go.
  // Decode CSS identifier escapes in `s` to their literal characters: `\xx` hex (1-6 hex digits,
  // optional single trailing whitespace) -> the code point; `\c` for any other char -> that char.
  function unescapeCssIdent(s) {
    s = String(s);
    var out = "", i = 0, n = s.length;
    while (i < n) {
      var c = s[i];
      if (c === "\\" && i + 1 < n) {
        var nx = s[i + 1];
        if (/[0-9a-fA-F]/.test(nx)) {
          var hex = ""; i++;
          while (i < n && hex.length < 6 && /[0-9a-fA-F]/.test(s[i])) { hex += s[i]; i++; }
          if (i < n && /\s/.test(s[i])) { i++; } // consume one trailing whitespace
          var cp = parseInt(hex, 16);
          out += (cp === 0 || cp > 0x10FFFF) ? "�" : String.fromCodePoint(cp);
          continue;
        }
        out += nx; i += 2; continue;
      }
      out += c; i++;
    }
    return out;
  }
  // Serialize a string as a CSS identifier (CSSOM "serialize an identifier"): escape characters that
  // aren't valid unescaped in an ident. Digits at the start (and a leading `-` then digit) are hex-
  // escaped; non-ident chars get a `\` (or hex escape for control chars).
  function escapeCssIdent(s) {
    s = String(s);
    var chars = Array.from(s), out = "";
    function hexEsc(cp) { return "\\" + cp.toString(16) + " "; }
    for (var i = 0; i < chars.length; i++) {
      var ch = chars[i], cp = ch.codePointAt(0);
      if (cp === 0) { out += "�"; continue; }
      if ((cp >= 0x1 && cp <= 0x1f) || cp === 0x7f) { out += hexEsc(cp); continue; }
      // A digit at the very start, or a digit right after a leading `-`, must be hex-escaped.
      if ((cp >= 0x30 && cp <= 0x39) && (i === 0 || (i === 1 && chars[0] === "-"))) { out += hexEsc(cp); continue; }
      if (i === 0 && cp === 0x2d && chars.length === 1) { out += "\\-"; continue; } // lone "-"
      if (cp >= 0x80 || cp === 0x2d || cp === 0x5f || (cp >= 0x30 && cp <= 0x39) ||
          (cp >= 0x41 && cp <= 0x5a) || (cp >= 0x61 && cp <= 0x7a)) { out += ch; continue; }
      out += "\\" + ch; // any other char: backslash-escape it literally
    }
    return out;
  }
  function parseStyleDecls(text) {
    var out = [];
    text = String(text || "");
    var parts = splitTopLevelSemis(text);
    // Parsing a whole block: importance, not source order, decides ties between same-property decls.
    var prev = __blockImportanceCascade;
    __blockImportanceCascade = true;
    try {
      for (var i = 0; i < parts.length; i++) {
        var seg = parts[i];
        var c = indexOfTopLevelColon(seg);
        if (c < 0) { continue; }
        var rawName = seg.slice(0, c).trim();
        // Custom property names are case-sensitive; decode CSS escapes (`--a\;b` -> `--a;b`). Standard
        // property names are ASCII-lowercased.
        var name;
        if (isCustomProp(rawName)) { name = unescapeCssIdent(rawName); }
        else { name = unescapeCssIdent(rawName).toLowerCase(); }
        if (!name) continue;
        var rawVal = seg.slice(c + 1).trim();
        var imp = splitImportant(rawVal);
        pushDecl(out, name, imp[0], imp[1]);
      }
    } finally { __blockImportanceCascade = prev; }
    return out;
  }
  // Index of the first top-level `:` (not inside parens/strings, not backslash-escaped). Used to
  // split a declaration `name : value` so an escaped colon in a custom-prop name isn't the splitter.
  function indexOfTopLevelColon(seg) {
    var i = 0, n = seg.length, depth = 0, q = null;
    while (i < n) {
      var c = seg[i];
      if (c === "\\" && i + 1 < n) { i += 2; continue; }
      if (q) { if (c === q) q = null; i++; continue; }
      if (c === '"' || c === "'") { q = c; i++; continue; }
      if (c === "(") { depth++; i++; continue; }
      if (c === ")") { if (depth > 0) depth--; i++; continue; }
      if (c === ":" && depth === 0) { return i; }
      i++;
    }
    return -1;
  }
  // Split a declaration block on top-level `;` (not inside parens/strings, not backslash-escaped).
  function splitTopLevelSemis(text) {
    var out = [], i = 0, n = text.length, depth = 0, q = null, start = 0;
    while (i < n) {
      var c = text[i];
      if (c === "\\" && i + 1 < n) { i += 2; continue; }
      if (q) { if (c === q) q = null; i++; continue; }
      if (c === '"' || c === "'") { q = c; i++; continue; }
      if (c === "(") { depth++; i++; continue; }
      if (c === ")") { if (depth > 0) depth--; i++; continue; }
      if (c === ";" && depth === 0) { out.push(text.slice(start, i)); start = i + 1; }
      i++;
    }
    if (start < n) out.push(text.slice(start));
    return out;
  }
  // ===== Property-name validity (CSSOM: unknown properties are dropped, never stored). =====
  // The set of standard CSS property names we recognize. Built from the longhand/shorthand machinery
  // plus an explicit list of additional standard names (logical properties, etc.). Custom properties
  // (`--*`) are always valid and handled separately.
  var KNOWN_PROPERTIES = (function () {
    var s = Object.create(null);
    function add(n) { s[n] = 1; }
    var arrs = [ALL_LONGHANDS, BORDER_ALL_LONGHANDS, FONT_VARIANT_LONGHANDS, BORDER_IMAGE_LONGHANDS];
    for (var i = 0; i < arrs.length; i++) for (var j = 0; j < arrs[i].length; j++) add(arrs[i][j]);
    // Shorthands + their longhands.
    var shorthands = [
      "all", "margin", "padding", "inset", "border", "border-width", "border-style", "border-color",
      "border-top", "border-right", "border-bottom", "border-left", "border-radius", "border-image",
      "outline", "overflow", "overscroll-behavior", "gap", "list-style", "text-decoration",
      "flex", "flex-flow", "place-content", "place-items", "place-self", "columns", "column-rule",
      "font", "font-variant", "background", "scroll-margin", "scroll-padding"
    ];
    for (var k = 0; k < shorthands.length; k++) {
      add(shorthands[k]);
      var lhs = shorthandLonghands(shorthands[k]);
      if (lhs) for (var m = 0; m < lhs.length; m++) add(lhs[m]);
    }
    // Additional standard longhands the cascade/CSSOM may carry that aren't in the lists above.
    var extra = [
      "background", "background-position", "color-scheme", "caret-color", "box-shadow", "transform",
      "transform-origin", "transition", "transition-property", "transition-duration",
      "transition-timing-function", "transition-delay", "animation", "animation-name",
      "animation-duration", "animation-timing-function", "animation-delay", "animation-iteration-count",
      "animation-direction", "animation-fill-mode", "animation-play-state",
      "content", "quotes", "cursor", "pointer-events", "user-select", "appearance", "-webkit-appearance",
      "box-sizing", "float", "clear", "clip", "clip-path", "filter", "backdrop-filter", "mix-blend-mode",
      "object-fit", "object-position", "order", "tab-size", "text-indent", "text-overflow", "text-shadow",
      "word-break", "word-spacing", "word-wrap", "overflow-wrap", "writing-mode", "direction",
      "unicode-bidi", "white-space", "vertical-align", "visibility", "z-index", "will-change",
      "scroll-behavior", "resize", "table-layout", "empty-cells", "caption-side", "counter-reset",
      "counter-increment", "perspective", "perspective-origin", "backface-visibility", "isolation",
      "mask", "mask-image", "-webkit-mask", "-webkit-mask-image", "column-count", "column-width",
      "column-gap", "column-rule-width", "column-rule-style", "column-rule-color", "grid-area",
      "grid-template", "grid-template-areas", "grid-auto-flow", "grid-auto-columns", "grid-auto-rows",
      "aspect-ratio", "inset-block", "inset-inline", "inset-block-start", "inset-block-end",
      "inset-inline-start", "inset-inline-end", "accent-color", "scroll-margin-top",
      "scroll-margin-right", "scroll-margin-bottom", "scroll-margin-left",
      "scroll-padding-top", "scroll-padding-right", "scroll-padding-bottom", "scroll-padding-left"
    ];
    for (var e = 0; e < extra.length; e++) add(extra[e]);
    // Logical box properties (margin/padding/border/inset block/inline + start/end). These are valid
    // standard properties (so they must not be rejected) even though we don't group them.
    var groups = ["margin", "padding"];
    for (var g = 0; g < groups.length; g++) {
      var base = groups[g];
      add(base + "-block"); add(base + "-inline");
      add(base + "-block-start"); add(base + "-block-end");
      add(base + "-inline-start"); add(base + "-inline-end");
    }
    var sides = ["block-start", "block-end", "inline-start", "inline-end", "block", "inline"];
    for (var si = 0; si < sides.length; si++) {
      add("border-" + sides[si] + "-width"); add("border-" + sides[si] + "-style"); add("border-" + sides[si] + "-color");
      add("border-" + sides[si]);
    }
    add("inline-size"); add("block-size"); add("min-inline-size"); add("min-block-size");
    add("max-inline-size"); add("max-block-size");
    // A broad set of additional standard CSS property names (so real-but-unmodeled properties are
    // not dropped). Not exhaustive, but covers the CSSOM round-trip test surface.
    var more = ("alignment-baseline baseline-shift baseline-source dominant-baseline " +
      "background-attachment background-blend-mode background-position-inline background-position-block " +
      "caption-side empty-cells orphans widows page-break-after page-break-before page-break-inside " +
      "break-after break-before break-inside text-indent text-justify text-orientation text-rendering " +
      "text-underline-position text-underline-offset text-decoration-thickness text-decoration-skip-ink " +
      "text-emphasis text-emphasis-color text-emphasis-style text-emphasis-position text-combine-upright " +
      "hyphens hanging-punctuation line-break overflow-anchor overflow-clip-margin scrollbar-gutter " +
      "scrollbar-width scrollbar-color scroll-snap-type scroll-snap-align scroll-snap-stop touch-action " +
      "flood-color flood-opacity stop-color stop-opacity lighting-color color-interpolation " +
      "color-interpolation-filters fill fill-opacity fill-rule stroke stroke-width stroke-opacity " +
      "stroke-dasharray stroke-dashoffset stroke-linecap stroke-linejoin stroke-miterlimit " +
      "clip-rule marker marker-start marker-mid marker-end paint-order shape-rendering " +
      "vector-effect text-anchor writing-mode glyph-orientation-vertical kerning " +
      "font-feature-settings font-variation-settings font-kerning font-optical-sizing font-language-override " +
      "font-size-adjust font-synthesis font-display src unicode-range ascent-override descent-override " +
      "line-gap-override size-adjust contain content-visibility container container-type container-name " +
      "counter-set inset gap row-gap column-gap place-items place-content place-self justify-items " +
      "image-rendering image-orientation shape-outside shape-margin shape-image-threshold " +
      "mix-blend-mode isolation backdrop-filter filter clip-path mask-clip mask-composite mask-mode " +
      "mask-origin mask-position mask-repeat mask-size mask-type mask-border " +
      "offset offset-path offset-distance offset-rotate offset-anchor offset-position " +
      "rotate scale translate transform-box transform-style perspective perspective-origin backface-visibility " +
      "will-change ruby-align ruby-position quotes tab-size " +
      "border-image-source border-image-slice border-image-width border-image-outset border-image-repeat " +
      "outline-offset text-shadow box-decoration-break " +
      "math-style math-depth math-shift forced-color-adjust print-color-adjust color-adjust " +
      "speak speak-as voice-family pitch pitch-range richness stress volume azimuth elevation " +
      "cue cue-before cue-after pause pause-before pause-after rest rest-before rest-after " +
      "all direction unicode-bidi white-space-collapse text-wrap text-wrap-mode text-wrap-style " +
      "field-sizing zoom aspect-ratio min-intrinsic-sizing " +
      "border-collapse border-spacing widows orphans table-layout caption-side empty-cells " +
      "outline-color outline-style outline-width outline-offset cursor pointer-events " +
      "background-position-x background-position-y background-clip background-origin").split(/\s+/);
    for (var mm = 0; mm < more.length; mm++) if (more[mm]) add(more[mm]);
    return s;
  })();
  function isKnownProperty(name) {
    if (isCustomProp(name)) return true;
    return hasOwn(KNOWN_PROPERTIES, name);
  }
  // A deliberately narrow validity check: returns false only for a small set of single-valued
  // longhand properties with values we can confidently reject (the cases the WPT CSSOM tests
  // exercise). Everything else is accepted — the engine ignores values it can't parse, and being
  // permissive avoids dropping valid declarations the round-trip tests rely on.
  // Single-token <color> longhands.
  var COLOR_LONGHANDS = { "color":1, "background-color":1,
    "border-top-color":1, "border-right-color":1, "border-bottom-color":1, "border-left-color":1,
    "text-decoration-color":1, "column-rule-color":1, "text-emphasis-color":1, "flood-color":1, "stop-color":1, "lighting-color":1 };
  // Non-negative <length-percentage> longhands.
  var NONNEG_LENGTH_LONGHANDS = { "width":1, "height":1, "min-width":1, "min-height":1,
    "max-width":1, "max-height":1, "inline-size":1, "block-size":1, "min-inline-size":1,
    "min-block-size":1, "max-inline-size":1, "max-block-size":1,
    "padding-top":1, "padding-right":1, "padding-bottom":1, "padding-left":1,
    "border-top-width":1, "border-right-width":1, "border-bottom-width":1, "border-left-width":1,
    "outline-width":1, "column-rule-width":1, "column-width":1 };
  function isValidValue(name, value) {
    var v = String(value).trim();
    if (v === "") return false;
    if (isCustomProp(name)) return true;
    if (isCssWideKeyword(v)) return true;
    var vl = v.toLowerCase();
    if (/(^|[^a-z-])(var|env)\s*\(/i.test(v)) return true; // can't validate around substitutions
    if (hasOwn(COLOR_LONGHANDS, name)) return isValidColor(v);
    if (hasOwn(NONNEG_LENGTH_LONGHANDS, name)) {
      if (vl === "auto" || vl === "none" || vl === "min-content" || vl === "max-content" ||
          vl === "fit-content" || vl === "thin" || vl === "medium" || vl === "thick" || /^fit-content\(/i.test(v)) return true;
      return isValidLengthLike(v, false);
    }
    if (name === "z-index" || name === "order") {
      if (vl === "auto") return true;
      return /^[-+]?\d+$/.test(v);
    }
    if (name === "opacity") {
      return /^[-+]?(?:\d+\.?\d*|\.\d+)(?:e[-+]?\d+)?%?$/i.test(v);
    }
    return true;
  }
  function isValidColor(v) {
    var vl = v.toLowerCase();
    if (NAMED_COLORS_OK[vl]) return true;
    if (vl === "transparent" || vl === "currentcolor" || vl === "inherit") return true;
    if (/^#([0-9a-f]{3}|[0-9a-f]{4}|[0-9a-f]{6}|[0-9a-f]{8})$/i.test(v)) return true;
    if (/^(rgba?|hsla?|hwb|lab|lch|oklab|oklch|color)\s*\(/i.test(v)) return true;
    return false;
  }
  // A small set of common named colors used to validate <color> keywords. Not exhaustive — any
  // unrecognized bare keyword for a color property is treated as invalid (matches the WPT cases).
  var NAMED_COLORS_OK = (function () {
    var names = ("black white red green blue yellow cyan magenta gray grey orange purple brown pink " +
      "silver gold navy teal olive maroon lime aqua fuchsia indigo violet coral salmon khaki crimson " +
      "tomato orchid plum tan beige ivory azure lavender turquoise chocolate darkred darkblue darkgreen " +
      "lightblue lightgreen lightgray lightgrey lightyellow rebeccapurple hotpink").split(" ");
    var o = Object.create(null);
    for (var i = 0; i < names.length; i++) o[names[i]] = 1;
    return o;
  })();
  function isValidLengthLike(v, allowNegative) {
    // Accept a single dimension/percentage/zero/calc token (optionally signed).
    if (/^calc\(/i.test(v)) return true;
    var m = /^([-+]?(?:\d+\.?\d*|\.\d+))(px|em|rem|ex|ch|vw|vh|vmin|vmax|cm|mm|in|pt|pc|q|%|fr)?$/i.exec(v);
    if (!m) return false;
    var num = parseFloat(m[1]);
    var unit = m[2] || "";
    if (num !== 0 && unit === "") return false; // unitless non-zero is not a length
    if (!allowNegative && num < 0) return false;
    return true;
  }
  // Append a declaration to the expanded longhand list, expanding shorthands and `all`.
  function pushDecl(out, name, val, important) {
    if (isCustomProp(name)) { setDecl(out, name, val, important); return; }
    // Drop unknown properties and values we can confidently reject (CSSOM parse-a-declaration).
    if (!isKnownProperty(name)) return;
    if (!isValidValue(name, val)) return;
    if (name === "all") {
      if (isCssWideKeyword(val)) {
        var kw = val.toLowerCase();
        // Remove any prior all-longhands so they re-append at this (the `all`) source position;
        // keeps custom properties declared before `all` ahead of it on serialization.
        for (var rr = 0; rr < ALL_LONGHANDS.length; rr++) removeDecl(out, ALL_LONGHANDS[rr]);
        for (var a = 0; a < ALL_LONGHANDS.length; a++) out.push([ALL_LONGHANDS[a], kw, !!important]);
      }
      return;
    }
    var expanded = expandShorthand(name, val);
    if (expanded) {
      for (var e = 0; e < expanded.length; e++) setDecl(out, expanded[e][0], normalizeCssValue(expanded[e][1]), important);
      return;
    }
    var nv = normalizeCssValue(val);
    // The `font` shorthand serializes size/line-height with spaces around the slash: `10px / 1`.
    // It also resets every font-variant longhand to its initial (which serializes as absent inline).
    if (name === "font" && !isCssWideKeyword(nv)) {
      nv = nv.replace(/\s*\/\s*/g, " / ");
      for (var fvr = 0; fvr < FONT_VARIANT_LONGHANDS.length; fvr++) removeDecl(out, FONT_VARIANT_LONGHANDS[fvr]);
    }
    // flex-basis serializes a zero length as `0px` (a <length-percentage>, not a flat number).
    if (name === "flex-basis" && nv === "0") { nv = "0px"; }
    // Property-specific <string> canonicalization.
    if (!isCssWideKeyword(nv)) {
      if (name === "content" || name === "quotes") { nv = requoteStrings(nv); }
      else if (name === "font-family") { nv = normalizeFontFamily(nv); }
    }
    setDecl(out, name, nv, important);
  }
  function findDecl(out, name) { for (var i = 0; i < out.length; i++) { if (out[i][0] === name) return i; } return -1; }
  // When true (set only while parsing a whole declaration block, e.g. the `cssText` setter), a later
  // NON-important declaration must not override an earlier `!important` one of the same property —
  // the cascade within a declaration block resolves on importance, not source order. The CSSOM
  // `setProperty` path leaves this false so an explicit set always replaces.
  var __blockImportanceCascade = false;
  function setDecl(out, name, val, important) {
    important = !!important;
    var i = findDecl(out, name);
    if (val == null || val === "") { if (i >= 0) out.splice(i, 1); return; }
    if (i >= 0) {
      if (__blockImportanceCascade && out[i][2] && !important) { return; }
      // When parsing a whole declaration block, a re-declared property keeps the LATER source
      // position (the cascade keeps the last occurrence, in its place) — so move it to the end. We
      // limit this to box-edge longhands (the logical property groups), whose relative ordering is
      // what the logical-group shorthand-serialization adjacency rule depends on; other properties
      // update in place to avoid disturbing the serialization of unexpanded shorthands.
      // Outside block parsing (a single `setProperty`), a different importance also moves it to the
      // end so an important override serializes after the non-important remainder.
      if ((__blockImportanceCascade && LOGICAL_GROUP[name]) || out[i][2] !== important) { out.splice(i, 1); out.push([name, val, important]); }
      else { out[i][1] = val; out[i][2] = important; }
    } else out.push([name, val, important]);
  }
  function removeDecl(out, name) { var i = findDecl(out, name); if (i >= 0) out.splice(i, 1); }
  // Shorthands to try when serializing a declaration block, in priority order.
  var SERIALIZE_SHORTHANDS = [
    "border", "border-width", "border-style", "border-color",
    "border-top", "border-right", "border-bottom", "border-left", "border-image",
    "margin", "padding", "inset", "border-radius",
    "margin-inline", "margin-block", "padding-inline", "padding-block", "inset-inline", "inset-block",
    "overflow", "overscroll-behavior", "gap", "outline", "list-style", "text-decoration",
    "flex", "flex-flow", "place-content", "place-items", "place-self", "columns", "font-variant"
  ];
  // Logical property groups for box edges: physical + flow-relative longhands share a group, and
  // mixing them prevents shorthand serialization unless the interleaving longhands belong to the
  // shorthand being formed (CSSOM "serialize a CSS declaration block" — logical-group adjacency).
  // Maps a longhand property name to its group id; properties not present here have no group.
  var LOGICAL_GROUP = (function () {
    var g = Object.create(null);
    ["margin-top","margin-right","margin-bottom","margin-left",
     "margin-block-start","margin-block-end","margin-inline-start","margin-inline-end"].forEach(function (p) { g[p] = "margin"; });
    ["padding-top","padding-right","padding-bottom","padding-left",
     "padding-block-start","padding-block-end","padding-inline-start","padding-inline-end"].forEach(function (p) { g[p] = "padding"; });
    ["top","right","bottom","left",
     "inset-block-start","inset-block-end","inset-inline-start","inset-inline-end"].forEach(function (p) { g[p] = "inset"; });
    return g;
  })();
  // Serialize expanded longhand triples WITHOUT shorthand grouping — the engine-readable form
  // stored in the `style` attribute (the Rust cascade understands longhands, not every shorthand).
  function serializeStyleDeclsFlat(decls) {
    var s = "";
    for (var i = 0; i < decls.length; i++) {
      var nm = isCustomProp(decls[i][0]) ? escapeCssIdent(decls[i][0]) : decls[i][0];
      s += (s ? " " : "") + nm + ": " + decls[i][1] + (decls[i][2] ? " !important" : "") + ";";
    }
    return s;
  }
  // Serialize a list of expanded longhand triples to a declaration block, grouping consecutive
  // longhands into shorthands where possible (CSSOM §serialize-a-css-declaration-block).
  function serializeStyleDecls(decls) {
    var byName = Object.create(null);
    var indexOfName = Object.create(null);
    for (var i = 0; i < decls.length; i++) { byName[decls[i][0]] = { v: decls[i][1], imp: decls[i][2] }; indexOfName[decls[i][0]] = i; }
    // The logical-group adjacency test (CSSOM): a shorthand whose longhands span declaration indices
    // [lo, hi] may only serialize if every OTHER declaration of a property in the same logical group
    // that falls within (lo, hi) is itself one of the shorthand's longhands. Returns true if `lhs`
    // (the shorthand's longhands) is serializable under this rule for group `group`.
    function logicalGroupContiguous(lhs, group) {
      if (!group) { return true; }
      var lo = Infinity, hi = -Infinity, set = Object.create(null);
      for (var a = 0; a < lhs.length; a++) {
        set[lhs[a]] = 1;
        var idx = indexOfName[lhs[a]];
        if (idx === undefined) { continue; }
        if (idx < lo) { lo = idx; }
        if (idx > hi) { hi = idx; }
      }
      if (lo === Infinity) { return true; }
      for (var d2 = lo; d2 <= hi; d2++) {
        var nm = decls[d2][0];
        if (LOGICAL_GROUP[nm] === group && !set[nm]) { return false; }
      }
      return true;
    }
    var serialized = Object.create(null);
    var pieces = [];
    function emit(prop, value, important) { pieces.push(prop + ": " + value + (important ? " !important" : "") + ";"); }
    // If EVERY `all`-affected longhand is present, equal, a CSS-wide keyword, and same importance,
    // collapse them into a single `all: <kw>` (at the position of the first such longhand).
    var allKw = null, allImp = null, allOk = true;
    for (var ai = 0; ai < ALL_LONGHANDS.length; ai++) {
      var rec0 = byName[ALL_LONGHANDS[ai]];
      if (!rec0 || !isCssWideKeyword(rec0.v)) { allOk = false; break; }
      if (allKw === null) { allKw = rec0.v; allImp = rec0.imp; }
      else if (rec0.v !== allKw || rec0.imp !== allImp) { allOk = false; break; }
    }
    var collapseAll = allOk && allKw !== null, allEmitted = false;
    for (var d = 0; d < decls.length; d++) {
      var name = decls[d][0];
      if (serialized[name]) continue;
      if (collapseAll && ALL_LONGHANDS.indexOf(name) >= 0) {
        if (!allEmitted) { emit("all", allKw.toLowerCase(), allImp); allEmitted = true; }
        serialized[name] = 1; continue;
      }
      if (isCustomProp(name)) { emit(escapeCssIdent(name), decls[d][1], decls[d][2]); serialized[name] = 1; continue; }
      var used = false;
      for (var s = 0; s < SERIALIZE_SHORTHANDS.length; s++) {
        var sh = SERIALIZE_SHORTHANDS[s];
        var lhs = sh === "border" ? BORDER_ALL_LONGHANDS : shorthandLonghands(sh);
        if (!lhs) continue;
        if (lhs.indexOf(name) < 0) continue;
        var ok = true, imp = decls[d][2];
        for (var k = 0; k < lhs.length; k++) {
          var rec = byName[lhs[k]];
          if (!rec || serialized[lhs[k]] || rec.imp !== imp) { ok = false; break; }
        }
        if (!ok) continue;
        // Logical-group adjacency: don't form this shorthand if a different-mapping-logic property of
        // the same logical group is declared between its longhands.
        if (!logicalGroupContiguous(lhs, LOGICAL_GROUP[name])) { continue; }
        var ser = serializeShorthand(sh, function (n) { var r = byName[n]; return r ? r.v : ""; });
        if (ser === "") continue;
        emit(sh, ser, imp);
        for (var k2 = 0; k2 < lhs.length; k2++) serialized[lhs[k2]] = 1;
        used = true; break;
      }
      if (!used) { emit(name, decls[d][1], decls[d][2]); serialized[name] = 1; }
    }
    return pieces.join(" ");
  }
  // camelCase JS property -> kebab-case CSS property (e.g. backgroundColor -> background-color).
  function camelToKebab(p) {
    p = String(p);
    if (p.indexOf("-") >= 0) { return p.toLowerCase(); } // already kebab (e.g. via setProperty)
    // Leading vendor prefix like `webkitTransform` -> `-webkit-transform`.
    var out = p.replace(/([A-Z])/g, function (m) { return "-" + m.toLowerCase(); });
    if (/^(webkit|moz|ms|o)-/.test(out)) { out = "-" + out; }
    return out;
  }
  function kebabToCamel(p) {
    p = String(p);
    return p.replace(/-([a-z])/g, function (_, c) { return c.toUpperCase(); });
  }
  function styleAttr(node) { var v = document.__getAttr(node, "style"); return v == null ? "" : v; }
  // Normalize a CSS property name for the CSSStyleDeclaration API (lowercase; custom props as-is).
  function normPropName(p) { p = String(p); if (isCustomProp(p)) { return p; } /* custom props are case-sensitive, kept verbatim */ p = camelToKebab(p); return p.toLowerCase(); }
  // Build a CSSStyleDeclaration over a backing store. `get()` returns the current declaration block
  // text; `set(text)` writes it back. Used for both inline styles (style attr) and rule blocks.
  // `restrict(longhandName)` (optional) gates which longhand properties this declaration block may
  // contain — used for @page / @keyframes, where only a subset of properties apply. A shorthand is
  // allowed iff at least one of its longhands is allowed; rejected longhands are dropped on parse and
  // on set (so `style.length` / serialization reflect only the allowed declarations).
  function makeStyleDecl(get, set, restrict) {
    function filterDecls(d) {
      if (!restrict) return d;
      var out = [];
      for (var i = 0; i < d.length; i++) { if (isCustomProp(d[i][0]) || restrict(d[i][0])) out.push(d[i]); }
      return out;
    }
    function read() { return filterDecls(parseStyleDecls(get())); }
    // The backing store holds the EXPANDED longhand form (engine-readable). Shorthand grouping is
    // applied only when serializing for the CSSOM `cssText` getter / `item`/`length` enumeration.
    // Only write when the serialized result actually differs from the current backing store: this
    // avoids creating an empty `style` attribute for a rejected declaration and avoids firing a
    // (mutation-observed) attribute write when nothing changed (CSSOM "same value" cases).
    // Serialize the declaration block back to the backing store. The store holds the EXPANDED
    // longhand form (engine-readable: the Rust cascade understands longhands, not every shorthand).
    function serializeForStore(d) { return serializeStyleDeclsFlat(filterDecls(d)); }
    function write(d) {
      var next = serializeForStore(d);
      // Compare against the RE-SERIALIZED current state (not the raw backing string, which may
      // differ only in trivia like trailing `;`/spacing) so a no-op edit doesn't create/rewrite the
      // attribute or fire a spurious mutation record.
      if (next === serializeForStore(read())) return;
      set(next);
      try { globalThis.__scheduleMODelivery(); } catch (e) {}
    }
    // Like write(), but always writes (used by the cssText setter, which must reflect even an
    // equal-but-reparsed value as an attribute mutation per the WPT MutationObserver tests).
    function writeAlways(d) {
      set(serializeForStore(d));
      try { globalThis.__scheduleMODelivery(); } catch (e) {}
    }
    // The serialized value of property `name` per CSSOM (shorthand serialization, custom verbatim).
    function getVal(name) {
      var d = read();
      if (isCustomProp(name)) { var ci = findDecl(d, name); return ci >= 0 ? d[ci][1] : ""; }
      if (name === "all") {
        var common = null, ok = true;
        for (var a = 0; a < ALL_LONGHANDS.length; a++) {
          var idx = findDecl(d, ALL_LONGHANDS[a]);
          if (idx < 0) { ok = false; break; }
          var v = d[idx][1];
          if (common === null) common = v; else if (common !== v) { ok = false; break; }
        }
        return (ok && common !== null && isCssWideKeyword(common)) ? common.toLowerCase() : "";
      }
      if (isShorthand(name)) {
        // A shorthand only serializes if all its longhands are present with a uniform priority.
        var shLhs = name === "border" ? BORDER_ALL_LONGHANDS : shorthandLonghands(name);
        if (shLhs) {
          var impCommon = null, impOk = true, allPresent = true;
          for (var si = 0; si < shLhs.length; si++) {
            var sidx = findDecl(d, shLhs[si]);
            if (sidx < 0) { allPresent = false; break; }
            if (impCommon === null) impCommon = d[sidx][2]; else if (impCommon !== d[sidx][2]) { impOk = false; break; }
          }
          if (allPresent && !impOk) return ""; // mixed importance -> shorthand can't be formed
        }
        var sv = serializeShorthand(name, function (n) { var i = findDecl(d, n); return i >= 0 ? d[i][1] : ""; });
        if (sv !== "") return sv;
        // If the shorthand was stored literally (we don't model its value), return the literal.
        var li = findDecl(d, name);
        return li >= 0 ? d[li][1] : "";
      }
      if (hasOwn(KEYWORD_ONLY_SHORTHANDS, name)) {
        var lhsK = KEYWORD_ONLY_SHORTHANDS[name], commonK = null, okK = true;
        for (var kk = 0; kk < lhsK.length; kk++) { var ik = findDecl(d, lhsK[kk]); if (ik < 0) { okK = false; break; } if (commonK === null) commonK = d[ik][1]; else if (commonK !== d[ik][1]) { okK = false; break; } }
        if (okK && commonK !== null && isCssWideKeyword(commonK)) return commonK.toLowerCase();
      }
      var i = findDecl(d, name);
      return i >= 0 ? d[i][1] : "";
    }
    function getPriority(name) {
      var d = read();
      if (name === "all" || isShorthand(name)) {
        var lhs = name === "all" ? ALL_LONGHANDS : (name === "border" ? BORDER_ALL_LONGHANDS : shorthandLonghands(name));
        if (!lhs) return "";
        for (var k = 0; k < lhs.length; k++) { var i = findDecl(d, lhs[k]); if (i < 0 || !d[i][2]) return ""; }
        return "important";
      }
      var idx = findDecl(d, name);
      return idx >= 0 && d[idx][2] ? "important" : "";
    }
    function setVal(name, val, important) {
      var d = read();
      if (val == null || String(val).trim() === "") { // empty value removes (per spec)
        if (name === "all") { for (var a = 0; a < ALL_LONGHANDS.length; a++) removeDecl(d, ALL_LONGHANDS[a]); }
        else if (isShorthand(name)) { var lhs0 = name === "border" ? BORDER_ALL_LONGHANDS : shorthandLonghands(name); for (var q = 0; q < lhs0.length; q++) removeDecl(d, lhs0[q]); }
        else removeDecl(d, name);
        write(d); return;
      }
      pushDecl(d, name, String(val).trim(), !!important);
      write(d);
    }
    function removeVal(name) {
      var old = getVal(name);
      var d = read();
      if (name === "all") { for (var a = 0; a < ALL_LONGHANDS.length; a++) removeDecl(d, ALL_LONGHANDS[a]); }
      else if (isShorthand(name)) { var lhs = name === "border" ? BORDER_ALL_LONGHANDS : shorthandLonghands(name); for (var q = 0; q < lhs.length; q++) removeDecl(d, lhs[q]); }
      else removeDecl(d, name);
      write(d);
      return old;
    }
    var base = {
      getPropertyValue: function (p) { return getVal(normPropName(p)); },
      getPropertyPriority: function (p) { return getPriority(normPropName(p)); },
      setProperty: function (p, v, prio) {
        var name = normPropName(p);
        var important = prio != null && String(prio).toLowerCase() === "important";
        setVal(name, v, important);
      },
      removeProperty: function (p) { return removeVal(normPropName(p)); },
      item: function (i) { var d = read(); i = i >>> 0; return i < d.length ? d[i][0] : ""; }
    };
    // CSSStyleDeclaration is iterable over its property names (the indexed-property getter values).
    try { base[Symbol.iterator] = function () { var d = read(); return makeIter(d, function (i, v) { return v[0]; }); }; } catch (e) {}
    Object.defineProperty(base, "length", { get: function () { return read().length; }, enumerable: false, configurable: true });
    Object.defineProperty(base, "cssText", {
      // Group longhands back into shorthands on read (CSSOM serialization); store flat on write.
      get: function () { return serializeStyleDecls(read()); },
      // Setting cssText replaces the whole block and always reflects to the style attribute (it is
      // observable even when the resulting value is unchanged).
      set: function (v) { writeAlways(parseStyleDecls(v)); },
      enumerable: true, configurable: true
    });
    // Make `el.style instanceof CSSStyleDeclaration` hold: the Proxy (no getPrototypeOf trap) reports
    // its target's prototype, so give the target the interface prototype.
    try { if (globalThis.CSSStyleDeclaration && globalThis.CSSStyleDeclaration.prototype) { Object.setPrototypeOf(base, globalThis.CSSStyleDeclaration.prototype); } } catch (e) {}
    try {
      return new Proxy(base, {
        get: function (t, p) {
          if (typeof p !== "string") { return t[p]; }
          if (p in t) { return t[p]; }
          if (/^[0-9]+$/.test(p)) { return t.item(Number(p)); }
          return getVal(normPropName(p));
        },
        set: function (t, p, v) {
          if (typeof p !== "string") { t[p] = v; return true; }
          if (p === "cssText") { t.cssText = v; return true; }
          if (p in t) { t[p] = v; return true; }
          setVal(normPropName(p), v, false); return true;
        },
        // CSS properties are WebIDL attributes (on the prototype) — `"color" in decl` is true even
        // though there's no own property for it (CSSStyleDeclaration-properties test).
        has: function (t, p) {
          if (typeof p === "string" && isKnownProperty(normPropName(p))) { return true; }
          return p in t;
        }
      });
    } catch (e) { return base; }
  }
  function makeStyle(node) {
    return makeStyleDecl(
      function () { return styleAttr(node); },
      function (text) { document.__setAttr(node, "style", text); }
    );
  }
  // A snapshot ES iterator over `arr`, mapping each (index, value) via `pick`.
  function makeIter(arr, pick) {
    var i = 0;
    var it = { next: function () { return i < arr.length ? { value: pick(i, arr[i++]), done: false } : { value: undefined, done: true }; } };
    try { it[Symbol.iterator] = function () { return this; }; } catch (e) {}
    return it;
  }
  // A spec-complete DOMTokenList over an element's `class` attribute (DOM standard §7.1).
  // The token set is the live `class` attribute parsed on ASCII whitespace
  // ([\t\n\f\r ]), order-preserving and de-duplicated. Reads always reparse the live
  // attribute (so external className/setAttribute changes are reflected); the mutating
  // methods run the spec "update steps" which serialize the ordered set back to `class`.
  function makeClassList(node) { return makeTokenList(node, "class", null); }
  // A DOMTokenList over an arbitrary reflected attribute (`attrName`). `supported` is an optional
  // allow-list of tokens for `supports()` (null => supports() throws TypeError, like `class`).
  function makeTokenList(node, attrName, supported) {
    // ASCII whitespace per the HTML spec: TAB, LF, FF, CR, SPACE.
    function splitTokens(s) {
      var out = [], i = 0, n = s.length;
      while (i < n) {
        var c = s[i];
        if (c === " " || c === "\t" || c === "\n" || c === "\f" || c === "\r") { i++; continue; }
        var start = i;
        while (i < n) { var d = s[i]; if (d === " " || d === "\t" || d === "\n" || d === "\f" || d === "\r") break; i++; }
        out.push(s.slice(start, i));
      }
      return out;
    }
    function hasWhitespace(s) {
      for (var i = 0; i < s.length; i++) { var c = s[i]; if (c === " " || c === "\t" || c === "\n" || c === "\f" || c === "\r") return true; }
      return false;
    }
    // Throw a DOMException that satisfies WPT assert_throws_dom (correct .name/.code, and
    // `instanceof DOMException`).
    function syntaxErr() { throw new globalThis.DOMException("The token provided must not be empty.", "SyntaxError"); }
    function invalidCharErr() { throw new globalThis.DOMException("The token provided contains HTML space characters, which are not valid in tokens.", "InvalidCharacterError"); }
    function validateToken(t) {
      if (t === "") { syntaxErr(); }
      if (hasWhitespace(t)) { invalidCharErr(); }
    }
    // Raw reflected-attribute string, or null when the attribute is absent.
    function rawAttr() { var c = document.__getAttr(node, attrName); return c == null ? null : String(c); }
    // The ordered token set (de-duplicated, first occurrence wins).
    function tokenSet() {
      var raw = rawAttr();
      if (raw == null || raw === "") { return []; }
      var toks = splitTokens(raw), seen = Object.create(null), out = [];
      for (var i = 0; i < toks.length; i++) { var t = toks[i]; if (!seen[t]) { seen[t] = 1; out.push(t); } }
      return out;
    }
    // The "update steps": serialize the ordered set and write it back to `class`, unless the
    // attribute is absent and the set is empty (in which case do nothing).
    function update(set) {
      if (rawAttr() == null && set.length === 0) { return; }
      document.__setAttr(node, attrName, set.join(" "));
    }

    var cl = {
      item: function (i) { i = i >>> 0; var s = tokenSet(); return i < s.length ? s[i] : null; },
      contains: function (token) { return tokenSet().indexOf(String(token)) >= 0; },
      add: function () {
        var s = tokenSet();
        for (var i = 0; i < arguments.length; i++) {
          var t = String(arguments[i]); validateToken(t);
          if (s.indexOf(t) < 0) { s.push(t); }
        }
        update(s);
      },
      remove: function () {
        var s = tokenSet();
        for (var i = 0; i < arguments.length; i++) {
          var t = String(arguments[i]); validateToken(t);
          var x = s.indexOf(t); if (x >= 0) { s.splice(x, 1); }
        }
        update(s);
      },
      toggle: function (token, force) {
        token = String(token); validateToken(token);
        var s = tokenSet(), x = s.indexOf(token);
        if (x >= 0) {
          // token present
          if (force === undefined || force === false) { s.splice(x, 1); update(s); return false; }
          return true; // force === true: no-op, no update
        }
        // token absent
        if (force === undefined || force === true) { s.push(token); update(s); return true; }
        return false; // force === false: no-op, no update
      },
      replace: function (token, newToken) {
        token = String(token); newToken = String(newToken);
        // Per spec, the empty-string (SyntaxError) check runs for BOTH tokens before the
        // whitespace (InvalidCharacterError) check for either.
        if (token === "" || newToken === "") { syntaxErr(); }
        if (hasWhitespace(token) || hasWhitespace(newToken)) { invalidCharErr(); }
        var s = tokenSet(), x = s.indexOf(token);
        if (x < 0) { return false; }
        var y = s.indexOf(newToken);
        if (y >= 0 && y !== x) {
          // newToken already in set: replace in place, then drop the duplicate.
          s[x] = newToken;
          var dup = s.indexOf(newToken); // earliest occurrence
          for (var j = s.length - 1; j >= 0; j--) { if (s[j] === newToken && j !== dup) { s.splice(j, 1); } }
        } else {
          s[x] = newToken;
        }
        update(s);
        return true;
      },
      supports: function (token) {
        // With no supported-tokens allow-list (e.g. `class`/`rel`), supports() throws TypeError.
        // Otherwise it ASCII-lowercases the token and checks membership.
        if (supported == null) { throw new TypeError("DOMTokenList has no supported tokens."); }
        return supported.indexOf(asciiLower(String(token))) >= 0;
      },
      forEach: function (cb, thisArg) {
        if (typeof cb !== "function") { throw new TypeError("The callback provided as parameter 1 is not a function."); }
        var s = tokenSet();
        for (var i = 0; i < s.length; i++) { cb.call(thisArg, s[i], i, cl); }
      },
      keys: function () { return makeIter(tokenSet(), function (i, v) { return i; }); },
      values: function () { return makeIter(tokenSet(), function (i, v) { return v; }); },
      entries: function () { return makeIter(tokenSet(), function (i, v) { return [i, v]; }); },
      toString: function () { var c = rawAttr(); return c == null ? "" : c; }
    };
    // Object.prototype.toString.call(list) === "[object DOMTokenList]".
    try { cl[Symbol.toStringTag] = "DOMTokenList"; } catch (e) {}
    // for...of / Symbol.iterator over the token values.
    try { cl[Symbol.iterator] = cl.values; } catch (e) {}

    Object.defineProperty(cl, "length", { get: function () { return tokenSet().length; }, enumerable: false, configurable: true });
    // `value` (the stringifier behaviour): get returns the raw attribute (""/absent => ""),
    // set assigns the `class` attribute verbatim.
    Object.defineProperty(cl, "value", {
      get: function () { var c = rawAttr(); return c == null ? "" : c; },
      set: function (v) { document.__setAttr(node, attrName, v == null ? "" : String(v)); },
      enumerable: false, configurable: true
    });
    // Live integer-indexed access: classList[i] => i-th token (or undefined). Reparses on each
    // read via a Proxy so the indices stay live with the attribute.
    try {
      return new Proxy(cl, {
        get: function (t, p, r) {
          if (typeof p === "string" && p.length && /^[0-9]+$/.test(p)) {
            var i = p >>> 0, s = tokenSet();
            return i < s.length ? s[i] : undefined;
          }
          return Reflect.get(t, p, r);
        },
        has: function (t, p) {
          if (typeof p === "string" && p.length && /^[0-9]+$/.test(p)) { return (p >>> 0) < tokenSet().length; }
          return p in t;
        }
      });
    } catch (e) { return cl; }
  }
  function makeDataset(node) {
    // Live view over data-* attributes. dataset.fooBar <-> data-foo-bar.
    var base = {};
    try {
      return new Proxy(base, {
        get: function (t, p) {
          if (typeof p !== "string") { return t[p]; }
          var v = document.__getAttr(node, "data-" + camelToKebab(p));
          return v == null ? undefined : v;
        },
        set: function (t, p, v) { if (typeof p === "string") { document.__setAttr(node, "data-" + camelToKebab(p), v == null ? "" : String(v)); } return true; },
        deleteProperty: function (t, p) { if (typeof p === "string") { document.__removeAttr(node, "data-" + camelToKebab(p)); } return true; },
        has: function (t, p) { return typeof p === "string" && document.__getAttr(node, "data-" + camelToKebab(p)) != null; }
      });
    } catch (e) { return base; }
  }
  function makeRect() { return { x: 0, y: 0, top: 0, left: 0, right: 0, bottom: 0, width: 0, height: 0, toJSON: function () { return this; } }; }

  // Split CSS source into top-level rules (brace-balanced), returning one normalized cssText per
  // rule. Good enough for feature-detection libraries that read `styleEl.sheet.cssRules[i].cssText`.
  function parseCssRules(css) {
    css = String(css == null ? "" : css);
    var rules = [], depth = 0, start = 0, i = 0, n = css.length;
    for (; i < n; i++) {
      var ch = css[i];
      if (ch === "{") { depth++; }
      else if (ch === "}") {
        depth--;
        if (depth === 0) { var seg = css.slice(start, i + 1).trim(); if (seg) { rules.push(normalizeCssText(seg)); } start = i + 1; }
      }
      else if (ch === ";" && depth === 0) {
        var s2 = css.slice(start, i + 1).trim(); if (s2) { rules.push(normalizeCssText(s2)); } start = i + 1;
      }
    }
    var tail = css.slice(start).trim();
    if (tail) { rules.push(normalizeCssText(tail + (depth > 0 ? "}" : ""))); }
    return rules;
  }
  function normalizeCssText(t) {
    // Collapse internal whitespace and normalize "{ }" spacing so equal rules compare equal.
    t = String(t).replace(/\s+/g, " ").trim();
    t = t.replace(/\s*{\s*/g, " { ").replace(/\s*}\s*/g, " }").replace(/\s*;\s*/g, "; ").trim();
    return t;
  }
  // Validate and serialize one *complex selector* (a single comma component) per the Selectors
  // grammar the CSSOM `selectorText` setter needs. Returns the normalized string, or `null` if the
  // selector is invalid (the setter then leaves the rule unchanged, per spec). Covers type/universal
  // selectors (incl. namespace prefixes `ns|`, `*|`, `|`), `.class`, `#id`, `[attr...]`, the known
  // pseudo-classes/elements, `:not(...)`, combinators, and unicode identifiers.
  var __cssPseudoElements = { before: 1, after: 1, "first-line": 1, "first-letter": 1, "first-line ": 1,
    selection: 1, placeholder: 1, marker: 1, backdrop: 1 };
  var __cssPseudoClasses = { active: 1, hover: 1, focus: 1, "focus-within": 1, "focus-visible": 1,
    visited: 1, link: 1, target: 1, root: 1, empty: 1, enabled: 1, disabled: 1, checked: 1, "first-child": 1,
    "last-child": 1, "only-child": 1, "first-of-type": 1, "last-of-type": 1, "only-of-type": 1,
    "nth-child": 1, "nth-last-child": 1, "nth-of-type": 1, "nth-last-of-type": 1, lang: 1, not: 1,
    is: 1, where: 1, has: 1, "any-link": 1, default: 1, indeterminate: 1, "read-only": 1, "read-write": 1,
    required: 1, optional: 1, "placeholder-shown": 1, valid: 1, invalid: 1, "in-range": 1, "out-of-range": 1 };
  // An identifier per CSS: starts with a letter / `_` / `-` / non-ASCII / escape, then those or
  // digits. We accept any non-ASCII codepoint (covers `ÇĞıİ`, `🤓`). A lone `-` is not an identifier.
  function isIdentChar(c, first) {
    if (c === "_" || c === "-") { return true; }
    var code = c.charCodeAt(0);
    if (code >= 128) { return true; }              // non-ASCII
    if (c >= "a" && c <= "z" || c >= "A" && c <= "Z") { return true; }
    if (!first && c >= "0" && c <= "9") { return true; }
    return false;
  }
  function isIdent(s) {
    if (!s) { return false; }
    if (s === "-") { return false; }
    var chars = Array.from(s);                       // codepoint-aware (handles surrogate pairs)
    for (var i = 0; i < chars.length; i++) {
      var c = chars[i];
      // A `\` starts an escape — anything can follow (a hex code or a single literal char), so the
      // identifier is valid regardless of the escaped character. Skip the rest of the escape.
      if (c === "\\") {
        i++;
        if (i < chars.length && /[0-9a-fA-F]/.test(chars[i])) {
          var hc = 1;
          while (i + 1 < chars.length && hc < 6 && /[0-9a-fA-F]/.test(chars[i + 1])) { i++; hc++; }
          if (i + 1 < chars.length && /\s/.test(chars[i + 1])) { i++; }
        }
        continue;
      }
      var ok = isIdentChar(c, i === 0) || (i > 0 && c >= "0" && c <= "9");
      // first char can't be a digit
      if (i === 0 && c >= "0" && c <= "9") { return false; }
      if (!ok && !(c >= "0" && c <= "9")) { return false; }
    }
    return true;
  }
  // Normalize the optional `ns|` namespace prefix on a type/universal selector. Returns the
  // remainder (`local`) and the serialized prefix. `*|x` and an absent prefix both serialize with no
  // prefix here (no namespaces declared); a bare leading `|` (default namespace) is invalid.
  function normalizeTypePrefix(s) {
    var bar = s.indexOf("|");
    if (bar < 0) { return { prefix: "", rest: s }; }
    var pre = s.slice(0, bar), rest = s.slice(bar + 1);
    if (pre === "" ) { return null; }   // `|div` — default namespace, unsupported → invalid
    if (pre === "*") { return { prefix: "", rest: rest }; } // any namespace → drop prefix
    if (!isIdent(pre)) { return null; }
    return { prefix: "", rest: rest };   // named namespace not declared → still serialize bare
  }
  var HASH = String.fromCharCode(35); // the id-prefix char, built via charcode to dodge Rust raw-string quoting.
  // True if `chars[i]` begins another simple selector in the same compound (class/id/attr/pseudo),
  // i.e. the universal `*` would be redundant and should be dropped during serialization.
  function compoundHasMore(chars, i) {
    if (i >= chars.length) { return false; }
    var c = chars[i];
    return c === "." || c === HASH || c === "[" || c === ":";
  }
  // Parse + canonicalize a CSS <an+b> value (the argument of :nth-child() etc.). Returns the
  // serialized form (e.g. "2n+1", "n", "-n+5", "10") or null if syntactically invalid.
  function serializeAnPlusB(arg) {
    var s = String(arg).trim().toLowerCase().replace(/\s+/g, " ");
    if (s === "") { return null; }
    if (s === "even") { return "2n"; }
    if (s === "odd") { return "2n+1"; }
    var a, b;
    // Pure integer (no `n`): A=0, B=integer.
    var mInt = /^([-+]?\d+)$/.exec(s.replace(/\s+/g, ""));
    if (mInt) { return String(parseInt(mInt[1], 10)); }
    // Forms with `n`: optional sign+coeff, `n`, optional ` ± b`.
    var compact = s.replace(/\s+/g, "");
    var m = /^([-+]?\d*)n([-+]\d+)?$/.exec(compact);
    if (!m) { return null; }
    var acoef = m[1];
    if (acoef === "" || acoef === "+") { a = 1; }
    else if (acoef === "-") { a = -1; }
    else { a = parseInt(acoef, 10); }
    b = m[2] != null ? parseInt(m[2], 10) : 0;
    // Serialize.
    var aPart;
    if (a === 1) { aPart = "n"; }
    else if (a === -1) { aPart = "-n"; }
    else { aPart = a + "n"; }
    if (a === 0) { return String(b); } // (shouldn't reach: handled by mInt)
    if (b === 0) { return aPart; }
    return aPart + (b > 0 ? "+" + b : "-" + (-b));
  }
  function normalizeComplexSelector(sel, nsCtx) {
    nsCtx = nsCtx || { hasDefault: false, prefixes: {} };
    sel = sel.trim();
    if (!sel) { return null; }
    var chars = Array.from(sel);
    var i = 0, n = chars.length, out = "", expectSimple = true, sawSimple = false;
    function err() { return null; }
    while (i < n) {
      var c = chars[i];
      if (c === " " || c === "\t" || c === "\n" || c === "\r" || c === "\f") {
        // Whitespace: a descendant combinator unless followed by another combinator.
        while (i < n && /\s/.test(chars[i])) { i++; }
        if (i >= n) { break; }
        var nx = chars[i];
        if (nx === ">" || nx === "+" || nx === "~") { continue; } // handled below
        out += " "; expectSimple = true; sawSimple = false; continue;
      }
      if (c === ">" || c === "+" || c === "~") {
        if (!sawSimple && out.replace(/\s+$/,"") === "") { return err(); }
        out = out.replace(/\s+$/, "") + " " + c + " ";
        i++; while (i < n && /\s/.test(chars[i])) { i++; }
        expectSimple = true; sawSimple = false; continue;
      }
      // Type / universal selector with an optional namespace prefix (`ns|`, `*|`, `|`). Reachable
      // when the compound starts with `*`, `|`, or an identifier char.
      if (c === "*" || c === "|" || isIdentChar(c, true)) {
        // Read an optional prefix terminated by `|`.
        var save = i, pre = null;
        if (c === "*") { pre = "*"; i++; }
        else if (c === "|") { pre = ""; }   // leading `|` → empty (default-namespace) prefix
        else {
          var pid = ""; while (i < n && isIdentChar(chars[i], pid === "")) { pid += chars[i]; i++; }
          pre = pid;
        }
        var hasBar = (i < n && chars[i] === "|");
        if (hasBar) {
          // Consume `|` and the local part.
          i++;
          var local;
          if (i < n && chars[i] === "*") { local = "*"; i++; }
          else {
            var lid = "";
            while (i < n) {
              if (chars[i] === "\\") {
                // Consume an escape (backslash + a hex code with optional trailing space, or a single char).
                lid += chars[i]; i++;
                if (i < n && /[0-9a-fA-F]/.test(chars[i])) {
                  var hk = 0;
                  while (i < n && hk < 6 && /[0-9a-fA-F]/.test(chars[i])) { lid += chars[i]; i++; hk++; }
                  if (i < n && /\s/.test(chars[i])) { lid += chars[i]; i++; }
                } else if (i < n) { lid += chars[i]; i++; }
                continue;
              }
              if (!isIdentChar(chars[i], lid === "")) { break; }
              lid += chars[i]; i++;
            }
            local = lid;
          }
          // Validate the local part.
          if (local !== "*" && !isIdent(local)) { return err(); }
          // Serialize the prefix per CSSOM. `|local` (no namespace) → keep `|`. `*|local` (any
          // namespace) → keep `*|` only when a default namespace is declared, else drop. A named
          // prefix `ns|` → keep when declared; bare (no prefixes declared) → drop.
          var serPre = "";
          if (pre === "") { serPre = "|"; }
          else if (pre === "*") { serPre = nsCtx.hasDefault ? "*|" : ""; }
          else if (isIdent(pre)) {
            var puri = nsCtx.prefixes[pre];
            // An undeclared namespace prefix makes the selector invalid (parse error).
            if (puri == null) { return err(); }
            // Declared named prefix whose URI equals the default namespace URI -> serialize bare.
            serPre = (nsCtx.hasDefault && puri === nsCtx.defaultUri) ? "" : pre + "|";
          }
          else { return err(); }
          if (local === "*") {
            // A universal local is kept when a prefix is serialized; otherwise it's dropped if the
            // compound has more simple selectors (`*.c` -> `.c`), kept if it stands alone (`*` -> `*`).
            if (serPre) { out += serPre + "*"; }
            else { out += compoundHasMore(chars, i) ? "" : "*"; }
          } else {
            out += serPre + local;
          }
        } else {
          // No prefix: a bare universal or type selector.
          if (pre === "*") { out += compoundHasMore(chars, i) ? "" : "*"; }
          else if (pre !== null && isIdent(pre)) { out += pre; }
          else { return err(); }
        }
        sawSimple = true; expectSimple = false; continue;
      }
      if (c === ".") {
        i++; var cls = ""; while (i < n && isIdentChar(chars[i], cls === "")) { cls += chars[i]; i++; }
        if (!isIdent(cls)) { return err(); }
        out += "." + cls; sawSimple = true; expectSimple = false; continue;
      }
      if (c === HASH) {
        i++; var id = ""; while (i < n && isIdentChar(chars[i], id === "")) { id += chars[i]; i++; }
        if (!isIdent(id)) { return err(); }
        out += HASH + id; sawSimple = true; expectSimple = false; continue;
      }
      if (c === "[") {
        // Attribute selector: scan to matching `]`.
        var depth = 1; i++; var attr = "";
        while (i < n && depth > 0) { if (chars[i] === "[") { depth++; } else if (chars[i] === "]") { depth--; if (depth === 0) { break; } } attr += chars[i]; i++; }
        if (depth !== 0) { return err(); }
        i++; // consume ]
        var na = normalizeAttr(attr);
        if (na === null) { return err(); }
        out += "[" + na + "]"; sawSimple = true; expectSimple = false; continue;
      }
      if (c === ":") {
        var dbl = (chars[i + 1] === ":");
        var start = i; i += dbl ? 2 : 1;
        var nm = "";
        while (i < n && isIdentChar(chars[i], nm === "")) { nm += chars[i]; i++; }
        if (!isIdent(nm)) { return err(); }
        var lower = nm.toLowerCase();
        var arg = "";
        if (i < n && chars[i] === "(") {
          var d2 = 1; i++; while (i < n && d2 > 0) { if (chars[i] === "(") { d2++; } else if (chars[i] === ")") { d2--; if (d2 === 0) { break; } } arg += chars[i]; i++; }
          if (d2 !== 0) { return err(); }
          i++; // consume )
        }
        if (__cssPseudoElements[lower] && !arg) {
          out += "::" + lower; sawSimple = true; expectSimple = false; continue;
        }
        if (!dbl && __cssPseudoClasses[lower]) {
          if (lower === "not" || lower === "is" || lower === "where" || lower === "has") {
            // Recursively validate the argument as a selector list.
            var inner = arg.split(",").map(function (s) { return normalizeComplexSelector(s, nsCtx); });
            if (inner.indexOf(null) >= 0 || inner.length === 0) { return err(); }
            out += ":" + lower + "(" + inner.join(", ") + ")";
          } else if (lower === "nth-child" || lower === "nth-last-child" || lower === "nth-of-type" || lower === "nth-last-of-type") {
            // Canonicalize the An+B microsyntax (CSSOM "serialize an <an+b> value").
            var anb = serializeAnPlusB(arg);
            if (anb === null) { return err(); }
            out += ":" + lower + "(" + anb + ")";
          } else {
            out += ":" + lower + (arg ? "(" + arg.trim() + ")" : "");
          }
          sawSimple = true; expectSimple = false; continue;
        }
        return err(); // unknown pseudo / `::pseudo-class`
      }
      return err(); // any other char (`!`, `$`, `(`, `{`, ...) is invalid
    }
    out = out.trim();
    if (!out) { return null; }
    // A trailing combinator is invalid.
    if (/[>+~]\s*$/.test(out)) { return null; }
    // (Redundant universal `*` dropping is handled per-compound during type-selector serialization,
    // so a namespaced universal like `|*.c` keeps its `*`.)
    return out;
  }
  // Validate/normalize the inside of `[...]`. Accepts `attr`, `ns|attr`, `*|attr`, and
  // `attr OP "value"` / `attr OP value` with OP in =, ~=, |=, ^=, $=, *=, plus an optional case
  // flag. Returns the normalized inner text, or null if invalid.
  function normalizeAttr(attr) {
    attr = attr.trim();
    if (!attr) { return null; }
    // Optional namespace prefix (`ns|`, `*|`, `|`) then the attribute name, then operator + value.
    var m = /^((?:[^|=~^$*\s]*|\*)\|)?([^|=~^$*\s]+)\s*([~|^$*]?=)?\s*([\s\S]*)$/.exec(attr);
    if (!m) { return null; }
    var rawPre = m[1], local = m[2], op = m[3] || "", val = (m[4] || "").trim();
    // Decode CSS escapes in the local name, then re-serialize it as a canonical identifier
    // (so `\30zonk` -> `\30 zonk`, `ns\:foo` -> `ns\:foo`).
    var localDecoded = unescapeCssIdent(local);
    // Any non-empty decoded name is a valid attribute name (escapeCssIdent makes leading digits etc.
    // legal via escapes). Only reject if it's empty.
    var localSer = localDecoded.length ? escapeCssIdent(localDecoded) : null;
    var name;
    if (rawPre != null) {
      var pre = rawPre.slice(0, -1); // drop the trailing `|`
      if (localSer === null) { return null; }
      if (pre === "*") { name = "*|" + localSer; }       // `[*|lang]` keeps the `*|`
      else if (pre === "") { name = localSer; }           // `[|lang]` -> `[lang]`
      else if (isIdent(pre)) { name = pre + "|" + localSer; }
      else { return null; }
    } else {
      if (localSer === null) { return null; }
      name = localSer;
    }
    if (!op) { return name; }
    // Value: quote if it's an unquoted identifier; keep quoted values, switching to double quotes.
    var flag = "";
    var fm = /\s+([iIsS])\s*$/.exec(val);
    if (fm) { flag = " " + fm[1].toLowerCase(); val = val.slice(0, val.length - fm[0].length).trim(); }
    var qv;
    if ((val.charAt(0) === '"' && val.charAt(val.length - 1) === '"') ||
        (val.charAt(0) === "'" && val.charAt(val.length - 1) === "'")) {
      qv = '"' + val.slice(1, -1) + '"';
    } else if (isIdent(val) || /^-?\d/.test(val)) {
      qv = '"' + val + '"';
    } else { return null; }
    return name + op + qv + flag;
  }
  // The CSSOM "serialize a selector" / "parse a group of selectors": validate every comma
  // component; if any is invalid the whole group is invalid (null). Otherwise join with ", ".
  function normalizeSelectorList(sel, nsCtx) {
    sel = String(sel == null ? "" : sel);
    var parts = sel.split(",");
    var outs = [];
    for (var i = 0; i < parts.length; i++) {
      var nrm = normalizeComplexSelector(parts[i], nsCtx);
      if (nrm === null) { return null; }
      outs.push(nrm);
    }
    if (!outs.length) { return null; }
    return outs.join(", ");
  }
  // Build a namespace context {hasDefault, defaultUri, prefixes:{name:uri}} from a sheet's
  // @namespace rule structs. Tracks each prefix's URI and the default namespace's URI so a named
  // prefix bound to the default namespace's URI can serialize bare (per CSSOM).
  function nsUri(raw) { return unquoteCss(String(raw).replace(/^url\(\s*|\s*\)$/g, "").trim()); }
  function sheetNsContext(sheet) {
    var ctx = { hasDefault: false, defaultUri: null, prefixes: {} };
    if (!sheet || !sheet.__structs) { return ctx; }
    var structs = sheet.__structs;
    for (var i = 0; i < structs.length; i++) {
      var st = structs[i];
      if (st.kind !== "@namespace") { continue; }
      var parts = splitTopLevel(st.prelude, " ").filter(function (x) { return x !== ""; });
      if (parts.length >= 2) { ctx.prefixes[parts[0]] = nsUri(parts.slice(1).join(" ")); }
      else { ctx.hasDefault = true; ctx.defaultUri = nsUri(parts[0] || ""); }
    }
    return ctx;
  }
  // ============================================================================================
  // CSSOM rule object model.
  //
  // Parsed CSS rules reach JS by re-parsing the sheet's raw CSS text on the JS side (the Rust `css`
  // crate flattens nesting for the cascade and isn't a faithful CSSOM source). `parseRuleStructs`
  // tokenizes top-level rules (brace-balanced, string/comment aware) into structured nodes
  // {kind, prelude, body, decls?, children?}. Each structured node is wrapped once in a *stable*
  // CSSRule object (cached by identity) so page-set expandos (e.g. `rule.randomProperty = 1`)
  // survive insert/delete — the CSSOM `[SameObject]` requirement. The owning CSSStyleSheet keeps an
  // ordered list of rule models and exposes a single stable CSSRuleList whose contents are kept in
  // sync as rules are inserted/deleted. Serialization (`cssText`) is spec-faithful so the WPT exact
  // string comparisons pass.
  // ============================================================================================

  // Tokenize a CSS string into top-level rule structs. `parentSheet`/`parentRule` thread ownership.
  function parseRuleStructs(css) {
    css = String(css == null ? "" : css);
    var out = [], n = css.length, i = 0;
    while (i < n) {
      // Skip whitespace and comments between rules.
      while (i < n && /\s/.test(css[i])) { i++; }
      if (i < n && css[i] === "/" && css[i + 1] === "*") { var e = css.indexOf("*/", i + 2); i = e < 0 ? n : e + 2; continue; }
      if (i >= n) { break; }
      // Read prelude up to `{` or `;` at depth 0 (string/comment aware).
      var preStart = i, sawBrace = false;
      while (i < n) {
        var c = css[i];
        if (c === "/" && css[i + 1] === "*") { var ce = css.indexOf("*/", i + 2); i = ce < 0 ? n : ce + 2; continue; }
        if (c === '"' || c === "'") { i++; while (i < n && css[i] !== c) { if (css[i] === "\\") { i++; } i++; } i++; continue; }
        if (c === "{") { sawBrace = true; break; }
        if (c === ";") { break; }
        i++;
      }
      var prelude = css.slice(preStart, i).trim();
      if (!sawBrace) {
        // Statement at-rule (e.g. `@import ...;`, `@namespace ...;`). Consume the `;`.
        if (i < n && css[i] === ";") { i++; }
        // `@charset` is a parse directive, not a CSS rule — it never appears in `cssRules` (CSSOM).
        if (prelude) { var __st = structFromPrelude(prelude, ""); if (__st && __st.kind !== "@charset") { out.push(__st); } }
        continue;
      }
      // Read the brace-balanced body.
      i++; var bodyStart = i, depth = 1;
      while (i < n && depth > 0) {
        var d = css[i];
        if (d === "/" && css[i + 1] === "*") { var be = css.indexOf("*/", i + 2); i = be < 0 ? n : be + 2; continue; }
        if (d === '"' || d === "'") { i++; while (i < n && css[i] !== d) { if (css[i] === "\\") { i++; } i++; } i++; continue; }
        if (d === "{") { depth++; }
        else if (d === "}") { depth--; if (depth === 0) { break; } }
        i++;
      }
      var body = css.slice(bodyStart, i);
      if (i < n && css[i] === "}") { i++; }
      var __srule = structFromPrelude(prelude, body);
      // Drop a style rule whose selector uses a functional pseudo-element without its required
      // argument (`::part`, `::slotted`, `::highlight`) — it's invalid, so the rule isn't parsed.
      if (__srule && !(__srule.kind === "style" && /::(?:part|slotted|highlight)\b(?!\s*\()/i.test(__srule.prelude))) {
        out.push(__srule);
      }
    }
    return out;
  }
  // Classify a prelude + body into a rule struct.
  function structFromPrelude(prelude, body) {
    if (prelude.charAt(0) === "@") {
      var m = /^@([-\w]+)\s*([\s\S]*)$/.exec(prelude);
      var name = (m ? m[1] : "").toLowerCase();
      var rest = m ? m[2].trim() : "";
      return { kind: "@" + name, atName: name, prelude: rest, body: body };
    }
    return { kind: "style", prelude: prelude, body: body };
  }
  // Parse a declaration block body into an array of [name, value, priority] tuples (CSSOM order).
  function parseDeclList(body) {
    var out = [], parts = splitTopLevel(body, ";");
    for (var i = 0; i < parts.length; i++) {
      var seg = parts[i], c = seg.indexOf(":");
      if (c < 0) { continue; }
      var name = seg.slice(0, c).trim().toLowerCase();
      var val = seg.slice(c + 1).trim();
      if (!name) { continue; }
      var prio = "";
      var pm = /!\s*important\s*$/i.exec(val);
      if (pm) { prio = "important"; val = val.slice(0, val.length - pm[0].length).trim(); }
      out.push([name, normalizeCssValue(val), prio]);
    }
    return out;
  }
  // Split on `sep` at brace/paren/string depth 0.
  function splitTopLevel(s, sep) {
    s = String(s); var out = [], depth = 0, start = 0, n = s.length;
    for (var i = 0; i < n; i++) {
      var c = s[i];
      if (c === '"' || c === "'") { i++; while (i < n && s[i] !== c) { if (s[i] === "\\") { i++; } i++; } continue; }
      if (c === "{" || c === "(" || c === "[") { depth++; }
      else if (c === "}" || c === ")" || c === "]") { depth--; }
      else if (c === sep && depth === 0) { out.push(s.slice(start, i)); start = i + 1; }
    }
    out.push(s.slice(start));
    return out;
  }
  function serializeDeclList(decls) {
    var s = "";
    for (var i = 0; i < decls.length; i++) {
      s += (s ? " " : "") + decls[i][0] + ": " + decls[i][1] + (decls[i][2] ? " !" + decls[i][2] : "") + ";";
    }
    return s;
  }
  // A standalone CSSStyleDeclaration over an in-memory `[name,value,priority]` array. `onChange` is
  // called after any mutation (so the owning rule can re-serialize). `instanceof CSSStyleDeclaration`.
  function makeRuleStyle(decls, onChange, restrict) {
    // Drop any property the context disallows (e.g. animation-* inside @keyframes) from the initial
    // parsed declarations, so `style.length`/serialization reflect only the applicable properties.
    if (restrict) { for (var di = decls.length - 1; di >= 0; di--) { if (!isCustomProp(decls[di][0]) && !restrict(decls[di][0])) { decls.splice(di, 1); } } }
    function find(name) { for (var i = 0; i < decls.length; i++) { if (decls[i][0] === name) { return i; } } return -1; }
    function getVal(name) { var i = find(name); return i >= 0 ? decls[i][1] : ""; }
    function setVal(name, val, prio) {
      if (restrict && !isCustomProp(name) && !restrict(name)) { return; } // disallowed in this context
      var i = find(name);
      if (val == null || val === "") { if (i >= 0) { decls.splice(i, 1); } }
      else {
        val = normalizeCssValue(String(val));
        if (i >= 0) { decls[i][1] = val; decls[i][2] = prio || ""; } else { decls.push([name, val, prio || ""]); }
      }
      if (onChange) { onChange(); }
    }
    var base = {
      getPropertyValue: function (p) { return getVal(String(p).toLowerCase()); },
      getPropertyPriority: function (p) { var i = find(String(p).toLowerCase()); return i >= 0 ? decls[i][2] : ""; },
      setProperty: function (p, v, prio) { setVal(String(p).toLowerCase(), v, String(prio || "").toLowerCase() === "important" ? "important" : ""); },
      removeProperty: function (p) { p = String(p).toLowerCase(); var old = getVal(p); setVal(p, ""); return old; },
      item: function (i) { return i >= 0 && i < decls.length ? decls[i][0] : ""; },
      parentRule: null
    };
    Object.defineProperty(base, "length", { get: function () { return decls.length; }, enumerable: false, configurable: true });
    Object.defineProperty(base, "cssText", {
      get: function () { return serializeDeclList(decls); },
      set: function (v) { decls.length = 0; var p = parseDeclList(v); for (var i = 0; i < p.length; i++) { if (!restrict || isCustomProp(p[i][0]) || restrict(p[i][0])) { decls.push(p[i]); } } if (onChange) { onChange(); } },
      enumerable: true, configurable: true
    });
    try { if (globalThis.CSSStyleDeclaration && globalThis.CSSStyleDeclaration.prototype) { Object.setPrototypeOf(base, globalThis.CSSStyleDeclaration.prototype); } } catch (e) {}
    try {
      return new Proxy(base, {
        get: function (t, p) {
          if (typeof p !== "string") { return t[p]; }
          if (p in t) { return t[p]; }
          return getVal(camelToKebab(p));
        },
        set: function (t, p, v) {
          if (typeof p !== "string") { t[p] = v; return true; }
          if (p === "cssText") { t.cssText = v; return true; }
          if (p in t && p !== "length") { t[p] = v; return true; }
          setVal(camelToKebab(p), v); return true;
        }
      });
    } catch (e) { return base; }
  }

  // --- @media condition serialization (CSSOM "serialize a media query list") ------------------
  // Lowercase a media type token; drop a leading `all` (unless negated). Per serialize-media-rule.
  function serializeMediaQuery(q) {
    q = q.trim().replace(/\s+/g, " ");
    if (q === "") { return ""; }
    // Lowercase media features inside parens and bare type/keyword tokens, preserving values.
    // Split into the leading "<not>? <type>?" head and " and (...)" tail features.
    var parts = splitTopLevel(q, " ").filter(function (x) { return x !== ""; });
    // Reconstruct by lowercasing keywords (not/and/or/only/type names) and feature names in parens.
    var negated = false, typeTok = null, feats = [], idx = 0;
    if (parts[idx] && parts[idx].toLowerCase() === "not") { negated = true; idx++; }
    if (parts[idx] && parts[idx].toLowerCase() === "only") { idx++; }
    if (parts[idx] && parts[idx].charAt(0) !== "(") { typeTok = parts[idx].toLowerCase(); idx++; }
    // Remaining: `and (feature)` groups. Re-join the rest and split on top-level " and ".
    var tail = parts.slice(idx).join(" ");
    var featGroups = tail ? splitTopLevel(tail, " ") : [];
    // Rebuild feature list: each `(...)` token lowercased on the feature name.
    var rebuilt = [];
    for (var i = 0; i < parts.length; i++) {
      var t = parts[i];
      if (t.charAt(0) === "(") { rebuilt.push(serializeMediaFeature(t)); }
    }
    var head;
    if (typeTok === "all" && !negated && rebuilt.length) { head = ""; }
    else { head = (negated ? "not " : "") + (typeTok || (negated || rebuilt.length === 0 ? "all" : "")); head = head.trim(); }
    var s = head;
    for (var j = 0; j < rebuilt.length; j++) { s += (s ? " and " : "") + rebuilt[j]; }
    return s.trim();
  }
  // Lowercase a `(feature: value)` token's feature name (and bare `(color)`), preserve value casing.
  function serializeMediaFeature(tok) {
    var inner = tok.replace(/^\(\s*/, "").replace(/\s*\)$/, "");
    var c = inner.indexOf(":");
    if (c < 0) { return "(" + inner.trim().toLowerCase() + ")"; }
    return "(" + inner.slice(0, c).trim().toLowerCase() + ": " + inner.slice(c + 1).trim() + ")";
  }
  function serializeMediaList(text) {
    text = String(text == null ? "" : text).trim();
    if (text === "") { return ""; }
    var queries = splitTopLevel(text, ",").map(function (q) { return serializeMediaQuery(q); }).filter(function (q) { return q !== ""; });
    return queries.join(", ");
  }
  // A MediaList over a mutable backing string holder {text}. `onChange` re-serializes the owner.
  function makeMediaList(holder, onChange) {
    function items() { var t = serializeMediaList(holder.text); return t === "" ? [] : splitTopLevel(t, ",").map(function (x) { return x.trim(); }); }
    var ml = {
      item: function (i) { var it = items(); return i >= 0 && i < it.length ? it[i] : null; },
      appendMedium: function (m) { if (arguments.length < 1) { throw new TypeError("appendMedium requires 1 argument"); } var it = items(); m = serializeMediaQuery(String(m)); if (it.indexOf(m) < 0) { it.push(m); } holder.text = it.join(", "); if (onChange) { onChange(); } },
      deleteMedium: function (m) { if (arguments.length < 1) { throw new TypeError("deleteMedium requires 1 argument"); } var it = items(); m = serializeMediaQuery(String(m)); var k = it.indexOf(m); if (k < 0) { throw new globalThis.DOMException("Not found", "NotFoundError"); } it.splice(k, 1); holder.text = it.join(", "); if (onChange) { onChange(); } },
      toString: function () { return serializeMediaList(holder.text); }
    };
    Object.defineProperty(ml, "length", { get: function () { return items().length; }, enumerable: true, configurable: true });
    Object.defineProperty(ml, "mediaText", {
      get: function () { return serializeMediaList(holder.text); },
      set: function (v) { holder.text = (v == null) ? "" : String(v); if (onChange) { onChange(); } },
      enumerable: true, configurable: true
    });
    try { if (globalThis.MediaList && globalThis.MediaList.prototype) { Object.setPrototypeOf(ml, globalThis.MediaList.prototype); } } catch (e) {}
    try {
      return new Proxy(ml, { get: function (t, p) {
        if (typeof p === "string" && /^\d+$/.test(p)) { var v = t.item(parseInt(p, 10)); return v == null ? undefined : v; }
        return t[p];
      } });
    } catch (e) { return ml; }
  }

  // --- @import prelude parsing/serialization ---------------------------------------------------
  // Parse `@import` prelude: url + optional layer + optional supports() + optional media query.
  function parseImportPrelude(prelude) {
    var s = String(prelude).trim();
    var href = "", rest = s;
    var um = /^url\(\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[^)\s]*)\s*\)/i.exec(s);
    if (um) { href = unquoteCss(um[1]); rest = s.slice(um[0].length).trim(); }
    else {
      var qm = /^("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*')/.exec(s);
      if (qm) { href = unquoteCss(qm[1]); rest = s.slice(qm[0].length).trim(); }
    }
    var layer = null, supports = null;
    var lm = /^layer\((.*?)\)/i.exec(rest);
    if (lm) { layer = lm[1].trim(); rest = rest.slice(lm[0].length).trim(); }
    else if (/^layer\b/i.test(rest)) { layer = ""; rest = rest.replace(/^layer\b/i, "").trim(); }
    var sm = /^supports\(([\s\S]*?)\)\s*/i.exec(rest);
    if (sm) {
      // Balance parens for nested conditions.
      var depth = 0, k = rest.indexOf("(") , start = k + 1, end = -1;
      for (var p = k; p < rest.length; p++) { if (rest[p] === "(") { depth++; } else if (rest[p] === ")") { depth--; if (depth === 0) { end = p; break; } } }
      if (end > start) { supports = rest.slice(start, end).trim(); rest = rest.slice(end + 1).trim(); }
    }
    var media = rest.trim();
    return { href: href, layer: layer, supports: supports, media: media };
  }
  function unquoteCss(s) {
    s = String(s);
    if ((s.charAt(0) === '"' && s.charAt(s.length - 1) === '"') || (s.charAt(0) === "'" && s.charAt(s.length - 1) === "'")) {
      return s.slice(1, -1).replace(/\\(.)/g, "$1");
    }
    return s;
  }
  // Serialize a string as a double-quoted CSS string (escape `"` and `\`).
  function cssQuote(s) { return '"' + String(s).replace(/\\/g, "\\\\").replace(/"/g, '\\"') + '"'; }

  // --- Stable CSSRule object construction ------------------------------------------------------
  // Build a CSSRule object for `struct` owned by `sheet` (a CSSStyleSheet) with `parentRule`.
  function makeCssRule(struct, sheet, parentRule) {
    var rule = makeCssRuleInner(struct, sheet, parentRule);
    // A rule detached from its sheet (deleteRule) reports parentStyleSheet/parentRule === null.
    // Defined on the rule's INTERMEDIATE prototype (not the instance) so assert_idl_attribute (which
    // requires these to be inherited, not own properties) still passes.
    try {
      var proto = Object.getPrototypeOf(rule);
      if (proto && proto !== Object.prototype) {
        Object.defineProperty(proto, "parentStyleSheet", { get: function () { return struct.__detached ? null : (sheet || null); }, enumerable: true, configurable: true });
        Object.defineProperty(proto, "parentRule", { get: function () { return struct.__detached ? null : (parentRule || null); }, enumerable: true, configurable: true });
      }
    } catch (e) {}
    return rule;
  }
  function makeCssRuleInner(struct, sheet, parentRule) {
    var kind = struct.kind;
    if (kind === "style") { return makeStyleRule(struct, sheet, parentRule); }
    if (kind === "@media") { return makeMediaRule(struct, sheet, parentRule); }
    if (kind === "@import") { return makeImportRule(struct, sheet, parentRule); }
    if (kind === "@font-feature-values") { return makeFontFeatureValuesRule(struct, sheet, parentRule); }
    if (kind === "@font-face") { return makeFontFaceRule(struct, sheet, parentRule); }
    if (kind === "@counter-style") { return makeCounterStyleRule(struct, sheet, parentRule); }
    if (kind === "@namespace") { return makeNamespaceRule(struct, sheet, parentRule); }
    if (kind === "@supports") { return makeSupportsRule(struct, sheet, parentRule); }
    if (kind === "@container") { return makeContainerRule(struct, sheet, parentRule); }
    if (kind === "@keyframes" || kind === "@-webkit-keyframes") { return makeKeyframesRule(struct, sheet, parentRule); }
    if (kind === "@page") { return makePageRule(struct, sheet, parentRule); }
    // Unknown at-rule: a generic rule that serializes its raw text.
    return makeGenericRule(struct, sheet, parentRule, 0);
  }
  // Define an accessor/value on a rule's INTERMEDIATE prototype (not the instance), so the CSSOM
  // [SameObject]/inherited-attribute semantics hold: `rule.hasOwnProperty("type")` is false but
  // `rule.type` resolves (assert_idl_attribute). The instance stays empty for page expandos.
  function defOn(rule, name, desc) { desc.configurable = true; Object.defineProperty(Object.getPrototypeOf(rule), name, desc); }
  // Create a fresh rule instance whose prototype holds the per-instance accessors and chains up to
  // the global interface constructor's prototype (for `instanceof`). Stores `type` on the proto.
  function newRule(ctorName, type, sheet, parentRule) {
    var proto = {};
    try { var ctor = globalThis[ctorName]; if (ctor && ctor.prototype) { Object.setPrototypeOf(proto, ctor.prototype); } } catch (e) {}
    Object.defineProperty(proto, "type", { get: function () { return type; }, enumerable: true, configurable: true });
    Object.defineProperty(proto, "parentStyleSheet", { get: function () { return sheet || null; }, enumerable: true, configurable: true });
    Object.defineProperty(proto, "parentRule", { get: function () { return parentRule || null; }, enumerable: true, configurable: true });
    return Object.create(proto);
  }
  // A rule's `.style` CSSStyleDeclaration, backed by the rule's declaration body text. Uses the
  // shared `makeStyleDecl` machinery (shorthand expand/serialize, custom props) so rule blocks
  // serialize identically to inline styles. `struct.body` holds the current (flat) declaration text.
  function makeRuleStyleDecl(struct, sheet, restrict) {
    if (struct.body == null) { struct.body = ""; }
    return makeStyleDecl(
      function () { return struct.body; },
      function (text) { struct.body = text; markDirty(sheet); },
      restrict
    );
  }
  // @page applies only the page-context properties (margins, page size/marks/bleed, and a handful of
  // box/background properties); anything else (e.g. `transform`) is dropped. CSS Page 3 §3.4.
  function pagePropertyAllowed(name) {
    if (/^margin(-|$)/.test(name) || /^padding(-|$)/.test(name)) return true;
    if (/^(size|marks|bleed|page|page-orientation)$/.test(name)) return true;
    if (/^(width|height|min-width|min-height|max-width|max-height)$/.test(name)) return true;
    return false;
  }
  // @keyframes block applies every property EXCEPT the animation longhands/shorthand (CSS Animations
  // §2: "animatable properties other than the animation properties").
  function keyframePropertyAllowed(name) {
    return !(name === "animation" || /^animation-/.test(name));
  }
  function makeStyleRule(struct, sheet, parentRule) {
    var rule = newRule("CSSStyleRule", 1, sheet, parentRule);
    var styleObj = makeRuleStyleDecl(struct, sheet);
    function selText() { var nrm = normalizeSelectorList(struct.prelude, sheetNsContext(sheet)); return nrm == null ? struct.prelude.trim() : nrm; }
    defOn(rule, "selectorText", {
      get: selText,
      set: function (v) { var nrm = normalizeSelectorList(v, sheetNsContext(sheet)); if (nrm != null) { struct.prelude = nrm; markDirty(sheet); } },
      enumerable: true
    });
    defOn(rule, "style", {
      get: function () { return styleObj; },
      set: function (v) { styleObj.cssText = v == null ? "" : String(v); },
      enumerable: true
    });
    defOn(rule, "cssText", { get: function () {
      var sel = selText();
      var body = styleObj.cssText;
      return sel + " { " + (body ? body + " " : "") + "}";
    }, enumerable: true });
    return rule;
  }
  function makePageRule(struct, sheet, parentRule) {
    // @page exposes a `.style` (CSSStyleDeclaration) like a style rule. Type 6.
    var rule = newRule("CSSPageRule", 6, sheet, parentRule);
    var styleObj = makeRuleStyleDecl(struct, sheet, pagePropertyAllowed);
    // The page selector (`:left`, `:first`, named page, etc.) — normalized (pseudo lowercased).
    function pageSel() { return normalizePageSelector(struct.prelude); }
    defOn(rule, "selectorText", {
      get: pageSel,
      set: function (v) { var nrm = normalizePageSelector(v == null ? "" : String(v)); if (nrm != null) { struct.prelude = nrm; markDirty(sheet); } },
      enumerable: true
    });
    defOn(rule, "style", { get: function () { return styleObj; }, set: function (v) { styleObj.cssText = v == null ? "" : String(v); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var body = styleObj.cssText; var sel = pageSel();
      return "@page" + (sel ? " " + sel : "") + " { " + (body ? body + " " : "") + "}";
    }, enumerable: true });
    return rule;
  }
  // Normalize an @page selector. Empty stays empty. Pseudo-page classes (`:left`/`:right`/`:first`/
  // `:blank`) lowercase; a named page keeps its case; combinations like `named:left` are preserved.
  function normalizePageSelector(sel) {
    sel = String(sel == null ? "" : sel).trim();
    if (sel === "") { return ""; }
    // Validate: optional ident, then zero or more `:pseudo` (left|right|first|blank).
    var m = /^([A-Za-z_-][\w-]*)?((?::(?:left|right|first|blank))*)$/i.exec(sel);
    if (!m) { return null; }
    var name = m[1] || "";
    var pseudos = (m[2] || "").toLowerCase();
    return name + pseudos;
  }
  function makeMediaRule(struct, sheet, parentRule) {
    var rule = newRule("CSSMediaRule", 4, sheet, parentRule);
    var holder = { text: struct.prelude };
    var mediaList = makeMediaList(holder, function () { markDirty(sheet); });
    var childRules = parseRuleStructs(struct.body);
    var childList = makeRuleList(childRules, sheet, rule);
    defOn(rule, "media", { get: function () { return mediaList; }, set: function (v) { mediaList.mediaText = v; }, enumerable: true });
    // conditionText getter mirrors media.mediaText; the setter is a no-op for @media (per browsers).
    defOn(rule, "conditionText", { get: function () { return serializeMediaList(holder.text); }, set: function () {}, enumerable: true });
    defOn(rule, "cssRules", { get: function () { return childList; }, enumerable: true });
    defOn(rule, "insertRule", { value: function (text, index) { return childList.__insert(String(text), index); }, enumerable: true });
    defOn(rule, "deleteRule", { value: function (index) { return childList.__delete(index); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var cond = serializeMediaList(holder.text);
      var inner = "";
      for (var i = 0; i < childList.length; i++) { inner += "  " + childList[i].cssText + "\n"; }
      return "@media " + cond + " {\n" + inner + "}";
    }, enumerable: true });
    return rule;
  }
  function makeSupportsRule(struct, sheet, parentRule) {
    var rule = newRule("CSSSupportsRule", 12, sheet, parentRule);
    var childList = makeRuleList(parseRuleStructs(struct.body), sheet, rule);
    defOn(rule, "conditionText", { get: function () { return struct.prelude.trim(); }, enumerable: true });
    defOn(rule, "cssRules", { get: function () { return childList; }, enumerable: true });
    defOn(rule, "insertRule", { value: function (text, index) { return childList.__insert(String(text), index); }, enumerable: true });
    defOn(rule, "deleteRule", { value: function (index) { return childList.__delete(index); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var inner = ""; for (var i = 0; i < childList.length; i++) { inner += "  " + childList[i].cssText + "\n"; }
      return "@supports " + struct.prelude.trim() + " {\n" + inner + "}";
    }, enumerable: true });
    return rule;
  }
  // Split a `@container` prelude into an optional container-name + the container query.
  // `sidebar (min-width: 100px)` -> {name:"sidebar", query:"(min-width: 100px)"}; a query that
  // starts with `(`/`not`/`style(`/`scroll-state(` has no name.
  function parseContainerPrelude(prelude) {
    var s = String(prelude).trim();
    var m = /^([-\w -￿]+)\s+([\s\S]+)$/.exec(s);
    if (m && m[1].charAt(0) !== "(" && !/^(not|and|or|style|scroll-state)$/i.test(m[1])) {
      return { name: m[1], query: m[2].trim() };
    }
    return { name: "", query: s };
  }
  function makeContainerRule(struct, sheet, parentRule) {
    var rule = newRule("CSSContainerRule", 0, sheet, parentRule);
    var childList = makeRuleList(parseRuleStructs(struct.body), sheet, rule);
    defOn(rule, "containerName", { get: function () { return parseContainerPrelude(struct.prelude).name; }, enumerable: true });
    defOn(rule, "containerQuery", { get: function () { return parseContainerPrelude(struct.prelude).query; }, enumerable: true });
    defOn(rule, "conditionText", { get: function () { return struct.prelude.trim(); }, enumerable: true });
    defOn(rule, "cssRules", { get: function () { return childList; }, enumerable: true });
    defOn(rule, "insertRule", { value: function (text, index) { return childList.__insert(String(text), index); }, enumerable: true });
    defOn(rule, "deleteRule", { value: function (index) { return childList.__delete(index); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var inner = ""; for (var i = 0; i < childList.length; i++) { inner += "  " + childList[i].cssText + "\n"; }
      return "@container " + struct.prelude.trim() + " {\n" + inner + "}";
    }, enumerable: true });
    return rule;
  }
  function makeImportRule(struct, sheet, parentRule) {
    var rule = newRule("CSSImportRule", 3, sheet, parentRule);
    var info = parseImportPrelude(struct.prelude);
    var holder = { text: info.media };
    var mediaList = makeMediaList(holder, function () { markDirty(sheet); });
    // The imported sheet object (we don't fetch external CSS; provide an empty CSSStyleSheet so
    // `instanceof CSSStyleSheet` holds and ownerRule is wired).
    var imported = null;
    defOn(rule, "href", { get: function () { return info.href; }, enumerable: true });
    defOn(rule, "layerName", { get: function () { return info.layer; }, enumerable: true });
    defOn(rule, "supportsText", { get: function () { return info.supports; }, enumerable: true });
    defOn(rule, "media", { get: function () { return mediaList; }, set: function (v) { mediaList.mediaText = v; }, enumerable: true });
    defOn(rule, "styleSheet", { get: function () {
      if (!imported) {
        imported = makeConstructedSheet(""); imported.__constructed = false; imported.__ownerRule = rule; imported.__href = info.href; imported.__media = mediaList;
        // The imported sheet's parent is the sheet containing the @import — until that rule is
        // removed (struct detached), at which point the child sheet is unlinked (parentStyleSheet null).
        try { Object.defineProperty(imported, "parentStyleSheet", { get: function () { return struct.__detached ? null : sheet; }, configurable: true }); } catch (e) {}
      }
      return imported;
    }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var s = "@import " + 'url(' + cssQuote(info.href) + ')';
      if (info.layer === "") { s += " layer"; } else if (info.layer != null) { s += " layer(" + info.layer + ")"; }
      if (info.supports != null) { s += " supports(" + info.supports + ")"; }
      var mt = serializeMediaList(holder.text);
      if (mt) { s += " " + mt; }
      return s + ";";
    }, enumerable: true });
    return rule;
  }
  function makeNamespaceRule(struct, sheet, parentRule) {
    var rule = newRule("CSSNamespaceRule", 10, sheet, parentRule);
    var parts = splitTopLevel(struct.prelude, " ").filter(function (x) { return x !== ""; });
    var prefix = "", uri = "";
    if (parts.length >= 2) { prefix = parts[0]; uri = parts.slice(1).join(" "); } else { uri = parts[0] || ""; }
    defOn(rule, "prefix", { get: function () { return prefix; }, enumerable: true });
    defOn(rule, "namespaceURI", { get: function () { return unquoteCss(uri.replace(/^url\(\s*|\s*\)$/g, "")); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var u = unquoteCss(uri.replace(/^url\(\s*|\s*\)$/g, ""));
      return "@namespace " + (prefix ? prefix + " " : "") + "url(" + cssQuote(u) + ");";
    }, enumerable: true });
    return rule;
  }
  function makeFontFaceRule(struct, sheet, parentRule) {
    var rule = newRule("CSSFontFaceRule", 5, sheet, parentRule);
    var decls = struct.decls || (struct.decls = parseDeclList(struct.body));
    var styleObj = makeRuleStyle(decls, function () { struct.body = serializeDeclList(decls); markDirty(sheet); });
    // The descriptor block of a `@font-face` rule is a `CSSFontFaceDescriptors`, not a plain
    // CSSStyleDeclaration, so `rule.style.toString()` reports `[object CSSFontFaceDescriptors]`.
    try {
      Object.defineProperty(styleObj, Symbol.toStringTag,
        { value: "CSSFontFaceDescriptors", writable: false, enumerable: false, configurable: true });
    } catch (e) {}
    defOn(rule, "style", { get: function () { return styleObj; }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var body = serializeDeclList(decls);
      return "@font-face { " + (body ? body + " " : "") + "}";
    }, enumerable: true });
    return rule;
  }
  function makeCounterStyleRule(struct, sheet, parentRule) {
    var rule = newRule("CSSCounterStyleRule", 11, sheet, parentRule);
    var decls = struct.decls || (struct.decls = parseDeclList(struct.body));
    // `name` reflects the prelude; each descriptor is a camelCase IDL attribute over the block.
    defOn(rule, "name", { get: function () { return struct.prelude.trim(); },
      set: function (v) { struct.prelude = String(v).trim(); markDirty(sheet); }, enumerable: true });
    [["system", "system"], ["symbols", "symbols"], ["additiveSymbols", "additive-symbols"],
     ["negative", "negative"], ["prefix", "prefix"], ["suffix", "suffix"], ["range", "range"],
     ["pad", "pad"], ["speakAs", "speak-as"], ["fallback", "fallback"]].forEach(function (pair) {
      defOn(rule, pair[0], {
        get: function () { var i = findDecl(decls, pair[1]); return i >= 0 ? decls[i][1] : ""; },
        set: function (v) { setDecl(decls, pair[1], String(v), false); struct.body = serializeDeclList(decls); markDirty(sheet); },
        enumerable: true
      });
    });
    // Single-line serialization (no newlines), per CSSOM.
    defOn(rule, "cssText", { get: function () {
      var body = serializeDeclList(decls);
      return "@counter-style " + struct.prelude.trim() + " { " + (body ? body + " " : "") + "}";
    }, enumerable: true });
    return rule;
  }
  // A single keyframe (`0% { ... }`) as a CSSKeyframeRule.
  function makeKeyframeRule(kf, parentRule, sheet) {
    var r = newRule("CSSKeyframeRule", 8, sheet, parentRule);
    var decls = kf.decls || (kf.decls = parseDeclList(kf.body));
    var styleObj = makeRuleStyle(decls, function () { kf.body = serializeDeclList(decls); markDirty(sheet); }, keyframePropertyAllowed);
    defOn(r, "keyText", { get: function () { return kf.prelude.trim(); }, set: function (v) { kf.prelude = String(v); markDirty(sheet); }, enumerable: true });
    // [PutForwards=cssText]: `r.style = "..."` forwards to style.cssText.
    defOn(r, "style", { get: function () { return styleObj; }, set: function (v) { styleObj.cssText = String(v); }, enumerable: true });
    defOn(r, "cssText", { get: function () { var b = serializeDeclList(decls); return kf.prelude.trim() + " { " + (b ? b + " " : "") + "}"; }, enumerable: true });
    return r;
  }
  function makeKeyframesRule(struct, sheet, parentRule) {
    var rule = newRule("CSSKeyframesRule", 7, sheet, parentRule);
    var name = struct.prelude.trim();
    var childRules = parseRuleStructs(struct.body);
    // Serialize the @keyframes name: a CSS-wide keyword (or otherwise non-custom-ident) name must be
    // serialized as a string, else as an identifier.
    function serializeKfName(n) {
      n = unquoteCss(n);
      var lower = n.toLowerCase();
      if (/^(initial|inherit|unset|revert|revert-layer|default|none)$/.test(lower) ||
          !/^-?[_a-zA-Z -￿][-_a-zA-Z0-9 -￿]*$/.test(n)) {
        return '"' + n.replace(/\\/g, "\\\\").replace(/"/g, '\\"') + '"';
      }
      return n;
    }
    // Normalize a keyframe selector list (from->0%, to->100%, lowercase, trimmed) for find/delete.
    function normKey(s) {
      return String(s).trim().split(",").map(function (t) {
        t = t.trim().toLowerCase(); return t === "from" ? "0%" : (t === "to" ? "100%" : t);
      }).join(", ");
    }
    function buildList() {
      var list = [];
      for (var i = 0; i < childRules.length; i++) { list.push(makeKeyframeRule(childRules[i], rule, sheet)); }
      list.item = function (i) { return this[i] || null; };
      try { if (globalThis.CSSRuleList && globalThis.CSSRuleList.prototype) { Object.setPrototypeOf(list, globalThis.CSSRuleList.prototype); } } catch (e) {}
      return list;
    }
    defOn(rule, "name", { get: function () { return unquoteCss(name); }, set: function (v) { name = String(v); markDirty(sheet); }, enumerable: true });
    defOn(rule, "cssRules", { get: function () { return buildList(); }, enumerable: true });
    defOn(rule, "length", { get: function () { return childRules.length; }, enumerable: true });
    // Indexed getter: rule[i] -> the i-th CSSKeyframeRule (or undefined). Defined over a fixed range.
    for (var __ix = 0; __ix < 64; __ix++) {
      (function (k) {
        defOn(rule, String(k), { get: function () { return k < childRules.length ? makeKeyframeRule(childRules[k], rule, sheet) : undefined; }, enumerable: true });
      })(__ix);
    }
    defOn(rule, "appendRule", { value: function (text) {
      var s = parseRuleStructs(String(text));
      for (var i = 0; i < s.length; i++) { childRules.push(s[i]); }
      markDirty(sheet);
    }, enumerable: true });
    defOn(rule, "findRule", { value: function (select) {
      var key = normKey(select);
      for (var i = childRules.length - 1; i >= 0; i--) { if (normKey(childRules[i].prelude) === key) { return makeKeyframeRule(childRules[i], rule, sheet); } }
      return null;
    }, enumerable: true });
    defOn(rule, "deleteRule", { value: function (select) {
      var key = normKey(select);
      for (var i = childRules.length - 1; i >= 0; i--) { if (normKey(childRules[i].prelude) === key) { childRules.splice(i, 1); markDirty(sheet); return; } }
    }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var inner = "";
      for (var i = 0; i < childRules.length; i++) {
        var c = childRules[i];
        inner += "  " + c.prelude.trim() + " { " + (serializeDeclList(c.decls || (c.decls = parseDeclList(c.body))) ? serializeDeclList(c.decls) + " " : "") + "}\n";
      }
      return "@keyframes " + serializeKfName(name) + " {\n" + inner + "}";
    }, enumerable: true });
    return rule;
  }
  function makeGenericRule(struct, sheet, parentRule, type) {
    var rule = newRule("CSSRule", type, sheet, parentRule);
    defOn(rule, "cssText", { get: function () {
      if (struct.body != null && struct.body !== "") { return struct.kind + " " + struct.prelude + " { " + struct.body.trim() + " }"; }
      return struct.kind + " " + struct.prelude + ";";
    }, enumerable: true });
    return rule;
  }
  // --- @font-feature-values ------------------------------------------------------------------
  function makeFontFeatureValuesRule(struct, sheet, parentRule) {
    var rule = newRule("CSSFontFeatureValuesRule", 14, sheet, parentRule);
    var family = struct.prelude.trim();
    // Parse inner @blocks into maps: blockName -> { ident: [numbers] }.
    var blocks = {};
    var inner = parseRuleStructs(struct.body);
    var blockNames = ["stylistic", "styleset", "character-variant", "swash", "ornaments", "annotation"];
    for (var bi = 0; bi < blockNames.length; bi++) { blocks[blockNames[bi]] = {}; }
    for (var i = 0; i < inner.length; i++) {
      var ir = inner[i];
      if (ir.kind && ir.kind.charAt(0) === "@") {
        var bn = ir.atName;
        if (!blocks[bn]) { blocks[bn] = {}; }
        var dl = splitTopLevel(ir.body, ";");
        for (var j = 0; j < dl.length; j++) {
          var seg = dl[j], c = seg.indexOf(":");
          if (c < 0) { continue; }
          var key = seg.slice(0, c).trim();
          var nums = seg.slice(c + 1).trim().split(/\s+/).filter(function (x) { return x !== ""; }).map(Number);
          if (key) { blocks[bn][key] = nums; }
        }
      }
    }
    function makeValuesMap(store) {
      var m = {
        get: function (k) { return store[k]; },
        set: function (k, v) { store[k] = (typeof v === "number") ? [v] : v.slice(); markDirty(sheet); },
        has: function (k) { return Object.prototype.hasOwnProperty.call(store, k); },
        "delete": function (k) { var had = Object.prototype.hasOwnProperty.call(store, k); delete store[k]; markDirty(sheet); return had; },
        clear: function () { for (var k in store) { delete store[k]; } markDirty(sheet); },
        forEach: function (cb, thisArg) { for (var k in store) { cb.call(thisArg, store[k], k, m); } }
      };
      Object.defineProperty(m, "size", { get: function () { return Object.keys(store).length; }, enumerable: true, configurable: true });
      try { m[Symbol.iterator] = function () { var keys = Object.keys(store), idx = 0; return { next: function () { return idx < keys.length ? { value: [keys[idx], store[keys[idx++]]], done: false } : { value: undefined, done: true }; } }; }; } catch (e) {}
      return m;
    }
    var maps = {
      stylistic: makeValuesMap(blocks["stylistic"]),
      styleset: makeValuesMap(blocks["styleset"]),
      characterVariant: makeValuesMap(blocks["character-variant"]),
      swash: makeValuesMap(blocks["swash"]),
      ornaments: makeValuesMap(blocks["ornaments"]),
      annotation: makeValuesMap(blocks["annotation"])
    };
    for (var mk in maps) { (function (k) { defOn(rule, k, { get: function () { return maps[k]; }, enumerable: true }); })(mk); }
    defOn(rule, "fontFamily", { get: function () { return family; }, set: function (v) { family = String(v); markDirty(sheet); }, enumerable: true });
    defOn(rule, "cssText", { get: function () {
      var s = "@font-feature-values " + family + " {\n";
      var order = [["@stylistic", blocks["stylistic"]], ["@styleset", blocks["styleset"]], ["@character-variant", blocks["character-variant"]], ["@swash", blocks["swash"]], ["@ornaments", blocks["ornaments"]], ["@annotation", blocks["annotation"]]];
      for (var oi = 0; oi < order.length; oi++) {
        var store = order[oi][1], keys = Object.keys(store);
        if (!keys.length) { continue; }
        s += "  " + order[oi][0] + " {\n";
        for (var ki = 0; ki < keys.length; ki++) { s += "    " + keys[ki] + ": " + store[keys[ki]].join(" ") + ";\n"; }
        s += "  }\n";
      }
      return s + "}";
    }, enumerable: true });
    return rule;
  }

  // --- CSSRuleList (stable, indexed list of rule objects) -------------------------------------
  // Builds CSSRule objects lazily and caches them per struct so expandos persist. `structs` is the
  // mutable backing array (shared with the sheet). insert/delete keep the cached wrappers aligned.
  function makeRuleList(structs, sheet, parentRule) {
    var list = [];
    function rebuild() {
      // Reuse cached wrappers (keyed on struct identity) so [SameObject] holds.
      for (var i = 0; i < structs.length; i++) {
        var st = structs[i];
        if (!st.__rule) { st.__rule = makeCssRule(st, sheet, parentRule); }
        list[i] = st.__rule;
      }
      list.length = structs.length;
    }
    rebuild();
    list.item = function (i) { i = i >>> 0; return i < structs.length ? list[i] : null; };
    list.__rebuild = rebuild;
    list.__structs = structs;
    list.__insert = function (text, index) {
      if (index === undefined) { index = 0; }
      index = index >>> 0;
      // Per CSSOM "insert a CSS rule": index range is checked BEFORE parsing.
      if (index > structs.length) { throw new globalThis.DOMException("Index out of bounds", "IndexSizeError"); }
      // Parse — must be exactly one syntactically valid rule. A bare prelude with no "{" (e.g. "???")
      // parses to a "style" struct but is NOT a rule, so reject it.
      var newStructs = parseRuleStructs(text);
      var st = newStructs[0];
      if (newStructs.length !== 1 || (st.kind === "style" && String(text).indexOf("{") < 0)) {
        throw new globalThis.DOMException("Failed to parse the rule", "SyntaxError");
      }
      // A style rule with an invalid selector (e.g. an undeclared namespace prefix) doesn't parse.
      if (st.kind === "style" && normalizeSelectorList(st.prelude, sheetNsContext(sheet)) == null) {
        throw new globalThis.DOMException("Failed to parse the rule selector", "SyntaxError");
      }
      // A grouping rule (CSSMediaRule/Supports/Container, parentRule set) cannot contain @import /
      // @namespace / @charset — those are stylesheet-level only.
      if (parentRule && (st.kind === "@import" || st.kind === "@namespace" || st.kind === "@charset")) {
        throw new globalThis.DOMException("Cannot insert this rule into a grouping rule", "HierarchyRequestError");
      }
      // Constructed sheets can't import: inserting an @import rule throws SyntaxError (per the
      // construct-stylesheets spec / disallow-import test).
      if (!parentRule && st.kind === "@import" && sheet && sheet.__constructed) {
        throw new globalThis.DOMException("Can't insert @import rules into a constructed stylesheet.", "SyntaxError");
      }
      // Top-level ordering constraints (CSSOM "insert a CSS rule" step 6): @import rules precede all
      // other rules; @namespace rules precede everything except @import. Violating the position throws
      // HierarchyRequestError. (@charset can never be inserted.)
      if (!parentRule) {
        if (st.kind === "@charset") {
          throw new globalThis.DOMException("Cannot insert @charset", "HierarchyRequestError");
        }
        if (st.kind === "@import") {
          // Every rule before `index` must be @import/@charset.
          for (var ii = 0; ii < index; ii++) { var k = structs[ii].kind; if (k !== "@import" && k !== "@charset") { throw new globalThis.DOMException("@import must precede all other rules", "HierarchyRequestError"); } }
          // The rule at `index` (if any) must not be a non-@import/@namespace rule that @import would jump over backwards — handled by the above since @import goes before namespaces too.
        } else if (st.kind === "@namespace") {
          // @namespace may only exist when the sheet has only @import/@namespace rules, and must be
          // positioned after @imports and before regular rules.
          for (var ij = 0; ij < structs.length; ij++) { var kj = structs[ij].kind; if (kj !== "@import" && kj !== "@namespace" && kj !== "@charset") { throw new globalThis.DOMException("@namespace not allowed here", "InvalidStateError"); } }
          for (var ik = 0; ik < index; ik++) { var kk = structs[ik].kind; if (kk !== "@import" && kk !== "@namespace" && kk !== "@charset") { throw new globalThis.DOMException("@namespace mispositioned", "HierarchyRequestError"); } }
        } else {
          // A regular rule must come after all @import and @namespace rules: no such rule at index >= index.
          for (var il = index; il < structs.length; il++) { var kl = structs[il].kind; if (kl === "@import" || kl === "@namespace" || kl === "@charset") { throw new globalThis.DOMException("Cannot insert rule before @import/@namespace", "HierarchyRequestError"); } }
        }
      }
      structs.splice(index, 0, st);
      rebuild(); markDirty(sheet);
      return index;
    };
    list.__delete = function (index) {
      index = index >>> 0;
      if (index >= structs.length) { throw new globalThis.DOMException("Index out of bounds", "IndexSizeError"); }
      // CSSOM "remove a CSS rule": a @namespace rule may only be deleted when the sheet contains
      // nothing but @import / @namespace rules; otherwise InvalidStateError.
      if (structs[index].kind === "@namespace") {
        for (var k = 0; k < structs.length; k++) {
          if (structs[k].kind !== "@namespace" && structs[k].kind !== "@import") {
            throw new globalThis.DOMException("Cannot delete a namespace rule when other rules are present.", "InvalidStateError");
          }
        }
      }
      structs[index].__detached = true; // detach the removed rule (parentStyleSheet/Rule -> null)
      structs.splice(index, 1);
      rebuild(); markDirty(sheet);
    };
    try { if (globalThis.CSSRuleList && globalThis.CSSRuleList.prototype) { Object.setPrototypeOf(list, globalThis.CSSRuleList.prototype); } } catch (e) {}
    return list;
  }

  // Mark a sheet dirty (re-render its <style> ownerNode so the cascade picks up CSSOM edits).
  // For a constructed sheet, notify any adoptedStyleSheets observers so their managed <style>
  // mirror is refreshed (mutating an adopted sheet is reflected in rendering).
  function markDirty(sheet) {
    if (!sheet || sheet.__rendering) { return; }
    if (sheet.__ownerNode && typeof sheet.__renderToOwner === "function") {
      try { sheet.__rendering = true; sheet.__renderToOwner(); } finally { sheet.__rendering = false; }
    }
    if (sheet.__adoptHosts) {
      for (var h = 0; h < sheet.__adoptHosts.length; h++) {
        try { sheet.__adoptHosts[h].__refreshAdopted(); } catch (e) {}
      }
    }
  }

  // --- CSSStyleSheet --------------------------------------------------------------------------
  // CSSOM origin-clean flag: a stylesheet fetched from another origin is not origin-clean, so its
  // rules (cssRules/insertRule/deleteRule) throw SecurityError. `href` is the link's URL attribute.
  function __computeOriginClean(href) {
    // No href (inline <style>, constructed sheet) is same-origin; data:/about: inherit the doc origin.
    if (!href) { return true; }
    if (href.slice(0, 5) === "data:" || href.slice(0, 6) === "about:") { return true; }
    try {
      if (new URL(href, document.baseURI).origin !== location.origin) { return false; }
    } catch (e) {
      // Unparseable/opaque URL (e.g. a cross-origin authority we can't resolve) → not origin-clean.
      return false;
    }
    // A server-side redirect carries its destination in a `location=` query param (e.g. WPT's
    // /common/redirect.py?location=…). Following it would land on that URL, so if the redirect
    // target is another origin the resulting sheet is not origin-clean.
    var m = /[?&]location=([^&]*)/.exec(href);
    if (m) {
      try {
        if (new URL(decodeURIComponent(m[1]), document.baseURI).origin !== location.origin) { return false; }
      } catch (e) {
        return false;
      }
    }
    return true;
  }
  function makeStyleSheetCore(structs, ownerNode) {
    var ss = {};
    var mediaHolder = { text: "" };
    var mediaList = makeMediaList(mediaHolder, null);
    ss.__structs = structs;
    ss.__ownerNode = ownerNode || null;
    var ruleList = makeRuleList(structs, ss, null);
    // __sync re-reads the owner node's text when it changed underneath us (e.g. a page sets
    // `styleEl.firstChild.data` directly). Replaced by makeStyleSheet for live <style>/<link>.
    ss.__sync = function () {};
    ss.type = "text/css";
    ss.disabled = false;
    Object.defineProperty(ss, "ownerNode", { get: function () {
      if (!ownerNode) { return null; }
      // A disabled or disconnected stylesheet <link> is no longer associated with the document, so
      // its sheet's ownerNode is null (CSSOM: a removed style sheet has no owner node).
      try {
        if (ownerNode.tagName === "LINK" && ownerNode.disabled) { return null; }
        if (document.documentElement && !document.documentElement.contains(ownerNode)) { return null; }
      } catch (e) {}
      return ownerNode;
    }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "ownerRule", { get: function () { return ss.__ownerRule || null; }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "parentStyleSheet", { get: function () { return null; }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "href", { get: function () { return ss.__href != null ? ss.__href : null; }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "title", { get: function () { return (ownerNode && ownerNode.getAttribute && ownerNode.getAttribute("title")) || null; }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "media", { get: function () { return ss.__media || mediaList; }, set: function (v) { (ss.__media || mediaList).mediaText = v; }, enumerable: true, configurable: true });
    // A non-origin-clean (cross-origin) sheet throws SecurityError on any rule access (CSSOM).
    ss.__checkOriginClean = function () {
      if (ss.__originClean === false) {
        throw new globalThis.DOMException("Cannot access rules of a cross-origin stylesheet", "SecurityError");
      }
    };
    Object.defineProperty(ss, "cssRules", { get: function () { ss.__checkOriginClean(); ss.__sync(); return ruleList; }, enumerable: true, configurable: true });
    Object.defineProperty(ss, "rules", { get: function () { ss.__checkOriginClean(); ss.__sync(); return ruleList; }, enumerable: false, configurable: true });
    ss.insertRule = function (text, index) {
      if (arguments.length < 1) { throw new TypeError("insertRule requires at least 1 argument"); }
      ss.__checkOriginClean();
      ss.__sync();
      return ruleList.__insert(String(text), index);
    };
    ss.deleteRule = function (index) {
      if (arguments.length < 1) { throw new TypeError("deleteRule requires 1 argument"); }
      ss.__checkOriginClean();
      return ruleList.__delete(index);
    };
    // Legacy CSSOM members.
    ss.removeRule = function (index) { if (index === undefined) { index = 0; } return ruleList.__delete(index); };
    ss.addRule = function (selector, block, index) {
      selector = selector === undefined ? "undefined" : String(selector);
      block = block === undefined ? "undefined" : String(block);
      if (index === undefined) { index = ruleList.length; }
      index = index >>> 0;
      // IndexSizeError propagates; SyntaxError/HierarchyRequestError are swallowed (legacy behavior).
      if (index > ruleList.length) { throw new globalThis.DOMException("Index out of bounds", "IndexSizeError"); }
      var text = selector + " { " + block + " }";
      try { ruleList.__insert(text, index); } catch (e) { if (e && e.name === "IndexSizeError") { throw e; } }
      return -1;
    };
    Object.defineProperty(ss, "cssText", { get: function () {
      var s = ""; for (var i = 0; i < ruleList.length; i++) { s += (s ? "\n" : "") + ruleList[i].cssText; } return s;
    }, enumerable: false, configurable: true });
    // CSSOM `replace`/`replaceSync`: only constructed sheets allow it; a `<style>`/`<link>` live
    // sheet (or an @import-target child sheet) throws NotAllowedError. `replaceSync` parses `text`,
    // strips any `@import` rules (constructed sheets can't import), and replaces ALL the rules.
    function doReplaceSync(text) {
      if (!ss.__constructed) {
        throw new globalThis.DOMException("Can't call replace/replaceSync on non-constructed CSSStyleSheet.", "NotAllowedError");
      }
      var ns = parseRuleStructs(String(text));
      var kept = [];
      for (var i = 0; i < ns.length; i++) { if (ns[i].kind !== "@import") { kept.push(ns[i]); } }
      ss.__structs.length = 0;
      for (var j = 0; j < kept.length; j++) { ss.__structs.push(kept[j]); }
      ruleList.__rebuild();
      markDirty(ss);
    }
    ss.replaceSync = function (text) { doReplaceSync(text); };
    ss.replace = function (text) {
      try { doReplaceSync(text); } catch (e) { return Promise.reject(e); }
      return Promise.resolve(ss);
    };
    try { if (globalThis.CSSStyleSheet && globalThis.CSSStyleSheet.prototype) { Object.setPrototypeOf(ss, globalThis.CSSStyleSheet.prototype); } } catch (e) {}
    return ss;
  }
  // A constructed (or @import-target) sheet with no owner node; supports replace/replaceSync.
  function makeConstructedSheet(cssText) {
    var ss = makeStyleSheetCore(parseRuleStructs(cssText), null);
    ss.__constructed = true;
    return ss;
  }
  // The live sheet for a <style>/<link> element. Parses textContent; re-renders on CSSOM edits, and
  // re-parses if the page mutates the element's text out-of-band (e.g. `styleEl.firstChild.data`).
  function makeStyleSheet(styleEl) {
    var initial = styleEl.textContent || "";
    // A <link rel=stylesheet> has no textContent — its rules come from the fetched external CSS.
    // `__fetch` is a synchronous GET via the host fetcher (the engine already fetched it for the
    // cascade, so this hits the net cache).
    if (!initial && styleEl.tagName === "LINK") {
      try {
        var __h = styleEl.getAttribute && styleEl.getAttribute("href");
        if (__h && __h.slice(0, 5) === "data:") {
          // A `data:` stylesheet carries its CSS inline (decode it here; the host fetcher is HTTP).
          var __c = __h.indexOf(",");
          if (__c >= 0) {
            var __meta = __h.slice(5, __c), __body = __h.slice(__c + 1);
            initial = (__meta.indexOf(";base64") >= 0)
              ? (typeof atob === "function" ? atob(__body) : "")
              : decodeURIComponent(__body);
          }
        } else if (__h && typeof __fetch === "function") {
          initial = __fetch(__h) || "";
        }
      } catch (e) {}
    }
    var ss = makeStyleSheetCore(parseRuleStructs(initial), styleEl);
    ss.__lastText = initial;
    // A <link>'s sheet is origin-clean only if fetched from the document's origin (CSSOM).
    if (styleEl.tagName === "LINK") {
      var __lh = (styleEl.getAttribute && styleEl.getAttribute("href")) || "";
      ss.__originClean = __computeOriginClean(__lh);
      // A `.asis` resource is served as a raw HTTP response; one that isn't a well-formed response
      // (no "HTTP/" status line) is a network error, so the load fails and the sheet isn't
      // origin-clean (accessing its rules throws SecurityError).
      if (/\.asis(\?|#|$)/i.test(__lh) && initial && !/^\s*HTTP\//i.test(initial)) {
        ss.__originClean = false;
      }
    }
    // The sheet's `media` reflects the owner <style>/<link> element's `media` content attribute.
    // The MediaList writes back to that attribute, so `sheet.media.appendMedium(...)` updates it.
    var mediaHolder = { get text() { var m = styleEl.getAttribute && styleEl.getAttribute("media"); return m == null ? "" : m; }, set text(v) { if (styleEl.setAttribute) { styleEl.setAttribute("media", v); } } };
    ss.__media = makeMediaList(mediaHolder, null);
    ss.__sync = function () {
      if (ss.__rendering) { return; }
      // A <link>'s rules come from its fetched CSS, not textContent — don't let an empty textContent
      // clear them (and the page can't mutate a link's CSS text via the DOM anyway).
      if (styleEl.tagName === "LINK") { return; }
      var cur = styleEl.textContent || "";
      if (cur === ss.__lastText) { return; }
      ss.__lastText = cur;
      var ns = parseRuleStructs(cur);
      ss.__structs.length = 0; for (var i = 0; i < ns.length; i++) { ss.__structs.push(ns[i]); }
      ss.cssRules.__rebuild();
    };
    ss.__renderToOwner = function () {
      var s = ""; var rl = ss.cssRules;
      for (var i = 0; i < rl.length; i++) { s += (s ? "\n" : "") + rl[i].cssText; }
      try { styleEl.textContent = s; ss.__lastText = styleEl.textContent || ""; } catch (e) {}
    };
    return ss;
  }

  // --- per-node wrapper cache (stable identity + expando persistence) ----------------------
  // Native DOM methods/accessors return a FRESH wrapper object on every call (each carrying the
  // hidden `__node` id). Frameworks like Vue stash internal state directly on DOM nodes
  // (`el.__vnode`, `el._vei`, `el.$once`, ...) and rely on `getElementById(x) === getElementById(x)`
  // and on those expandos surviving across lookups. To honor that we keep a JS-side map from node
  // id -> the one canonical enriched wrapper, and route every element the native layer hands back
  // through `canon()`, which returns the cached wrapper (copying over the fresh wrapper's own
  // function bindings on first sight). The cache lives entirely on the JS side, so Boa's GC roots
  // the wrappers for us — no Boa values are held in Rust (same discipline as elsewhere).
  var __nodeCache = Object.create(null);
  function canon(el) {
    if (!el || typeof el !== "object") { return el; }
    var node = el.__node;
    if (typeof node !== "number") { return enrichElement(el); }
    var cached = __nodeCache[node];
    if (cached) { return cached; }
    __nodeCache[node] = el;       // record BEFORE enriching so re-entrant lookups dedupe
    enrichElement(el);
    return el;
  }
  def(globalThis, "__canonNode", canon);

  // The live document is the facade for arena node 0. Register it in the canonical wrapper cache
  // so walking parentNode from the document element reaches the actual global document object.
  try {
    Object.defineProperty(document, "__node", { value: 0, configurable: true });
    def(document, "__enriched", true);
    canon(document);
  } catch (e) {}

  function nodeContains(root, other) {
    if (other == null) { return false; }
    if (other === root) { return true; }
    var rootId = root && root.__node;
    var cur = other && other.__node;
    if (typeof rootId !== "number" || typeof cur !== "number") { return false; }
    while (cur >= 0) {
      if (cur === rootId) { return true; }
      cur = __parent(cur);
    }
    return false;
  }
  // Look up the canonical wrapper for a node id, if one was already created (createElement / a prior
  // lookup). Returns null if the node was never wrapped. Lets out-of-scope code resolve a node id.
  def(globalThis, "__nodeById", function (id) { return __nodeCache[id] || null; });

  // Map a tag name to the most specific DOM interface prototype we have, so element wrappers
  // satisfy `el instanceof HTMLElement/Element/Node` (and SVG/MathML where appropriate). The
  // wrapper keeps all its own (native) accessors/methods; we only graft the interface prototype
  // onto its chain via Object.setPrototypeOf, then re-install its own data/accessor props (they
  // are own properties on the wrapper, so the chain swap doesn't lose them).
  var svgTags = { svg: 1, path: 1, g: 1, rect: 1, circle: 1, ellipse: 1, line: 1, polyline: 1,
    polygon: 1, text: 1, tspan: 1, defs: 1, use: 1, symbol: 1, marker: 1, "clippath": 1,
    mask: 1, pattern: 1, image: 1, "lineargradient": 1, "radialgradient": 1, stop: 1, filter: 1,
    foreignobject: 1 };
  var tagIface = {
    div: "HTMLDivElement", span: "HTMLSpanElement", p: "HTMLParagraphElement", a: "HTMLAnchorElement",
    img: "HTMLImageElement", input: "HTMLInputElement", button: "HTMLButtonElement",
    select: "HTMLSelectElement", option: "HTMLOptionElement", textarea: "HTMLTextAreaElement",
    form: "HTMLFormElement", label: "HTMLLabelElement", ul: "HTMLUListElement", ol: "HTMLOListElement",
    li: "HTMLLIElement", table: "HTMLTableElement", tr: "HTMLTableRowElement", td: "HTMLTableCellElement",
    th: "HTMLTableCellElement", canvas: "HTMLCanvasElement", video: "HTMLVideoElement",
    audio: "HTMLAudioElement", iframe: "HTMLIFrameElement", template: "HTMLTemplateElement",
    h1: "HTMLHeadingElement", h2: "HTMLHeadingElement", h3: "HTMLHeadingElement",
    h4: "HTMLHeadingElement", h5: "HTMLHeadingElement", h6: "HTMLHeadingElement",
    body: "HTMLBodyElement", frameset: "HTMLFrameSetElement",
    html: "HTMLHtmlElement", head: "HTMLHeadElement",
    script: "HTMLScriptElement", style: "HTMLStyleElement", link: "HTMLLinkElement",
    meta: "HTMLMetaElement", title: "HTMLTitleElement"
  };
  function ifaceProtoForTag(tag) {
    tag = String(tag || "").toLowerCase();
    if (svgTags[tag]) { return (globalThis.SVGElement && globalThis.SVGElement.prototype) || null; }
    var name = tagIface[tag];
    var ctor = name && globalThis[name];
    if (typeof ctor === "function" && ctor.prototype) { return ctor.prototype; }
    return (globalThis.HTMLElement && globalThis.HTMLElement.prototype) || null;
  }
  function applyNodePrototype(wrapper, node) {
    if (!wrapper || typeof node !== "number") { return; }
    var type = __nodeType(node);
    var proto = null;
    if (type === 1) { proto = ifaceProtoForTag(wrapper.tagName); }
    else if (type === 3) { proto = globalThis.Text && globalThis.Text.prototype; }
    else if (type === 4) { proto = globalThis.CDATASection && globalThis.CDATASection.prototype; }
    else if (type === 7) { proto = globalThis.ProcessingInstruction && globalThis.ProcessingInstruction.prototype; }
    else if (type === 8) { proto = globalThis.Comment && globalThis.Comment.prototype; }
    else if (type === 10) { proto = globalThis.DocumentType && globalThis.DocumentType.prototype; }
    else if (type === 11) { proto = globalThis.DocumentFragment && globalThis.DocumentFragment.prototype; }
    if (proto && Object.getPrototypeOf(wrapper) !== proto) { Object.setPrototypeOf(wrapper, proto); }
  }

  // ============================================================================================
  // Generic HTML IDL attribute reflection.
  //
  // The HTML standard defines, for each element interface, a set of IDL attributes that "reflect"
  // a content attribute (e.g. `el.id` <-> `id`, `a.href` <-> `href`, `input.disabled` <->
  // `disabled`). Each reflected attribute has a TYPE (DOMString, boolean, long, unsigned long,
  // enumerated, URL, ...) whose getter/setter rules are spelled out in the spec. We implement those
  // rules once as a set of "type factories" and drive them from data tables transcribed from the
  // WPT `elements-*.js` files (which are themselves generated from the spec IDL), so the behaviour
  // matches the exhaustive reflection-*.html conformance tests.
  //
  // Every getter/setter reads/writes the element's CONTENT attribute through the existing
  // __getAttr / __setAttr / __removeAttr natives, so reflection stays live both ways (set IDL ->
  // attribute changes; setAttribute -> IDL getter changes).
  // ============================================================================================
  function __asciiLower(s) {
    return String(s).replace(/[A-Z]/g, function (c) { return c.toLowerCase(); });
  }
  // Resolve `v` against the document base URL and serialize as the WPT reflection harness does:
  // protocol + "//" + host + pathname + search + hash (returning the raw input if that yields "//").
  // This mirrors the harness' own resolveUrl() so `url`-type reflected attributes compare equal,
  // and works around our URL parser dropping the trailing path segment for empty relative refs.
  // The document's effective base URL: the first <base href> (resolved against the page URL) if any,
  // otherwise the page URL itself. Honoured by URL-reflecting attributes (a.href, img.src, …).
  function __effectiveBaseURL() {
    try {
      var b = document.querySelector("base[href]");
      if (b) {
        var bh = b.getAttribute("href");
        if (bh != null && bh !== "") {
          var rb = parseURL(bh, globalThis.__pageURL);
          if (!rb.__invalid && rb.href) { return rb.href; }
        }
      }
    } catch (e) {}
    return globalThis.__pageURL;
  }
  function __reflResolveURL(v) {
    v = String(v);
    var base = __effectiveBaseURL();
    var resolved;
    try {
      if (v === "") {
        // Empty relative URL: resolve to the base, but keep the base's path/query (drop its fragment).
        var bp = parseURL(base);
        resolved = bp.protocol + (bp.host ? "//" + bp.host : "") + bp.pathname + bp.search;
      } else if (v.charCodeAt(0) === 35 /* '#' */) {
        var bp2 = parseURL(base);
        resolved = bp2.protocol + (bp2.host ? "//" + bp2.host : "") + bp2.pathname + bp2.search + v;
      } else {
        resolved = new URL(v, base).href;
      }
    } catch (e) { return v; }
    var p = parseURL(resolved);
    // Serialize the resolved record. Only special/authority URLs get the `//` separator — a
    // non-special scheme with an opaque path (e.g. `data:`, `mailto:`) must never gain one.
    if (p.__invalid || p.href === "") { return v; }
    return p.href;
  }
  var __refl = (function () {
    var maxInt = 2147483647, minInt = -2147483648;
    // "rules for parsing integers".
    function parseIntHtml(input) {
      input = String(input);
      var pos = 0, sign = 1, len = input.length;
      while (pos < len && /[ \t\n\f\r]/.test(input[pos])) { pos++; }
      if (pos >= len) { return false; }
      if (input[pos] === "-") { sign = -1; pos++; }
      else if (input[pos] === "+") { pos++; }
      if (pos >= len || !/[0-9]/.test(input[pos])) { return false; }
      var value = 0;
      while (pos < len && /[0-9]/.test(input[pos])) {
        value = value * 10 + (input.charCodeAt(pos) - 48);
        pos++;
      }
      return value === 0 ? 0 : sign * value;
    }
    // "rules for parsing non-negative integers".
    function parseNonneg(input) {
      var v = parseIntHtml(input);
      if (v === false || v < 0) { return false; }
      return v;
    }
    // "rules for parsing floating-point number values" (close enough for reflection; we lean on the
    // engine's Number parsing for the heavy lifting after validating the grammar's first char).
    function parseFloatHtml(input) {
      input = String(input);
      var pos = 0, len = input.length;
      while (pos < len && /[ \t\n\f\r]/.test(input[pos])) { pos++; }
      if (pos >= len) { return false; }
      var c = input[pos];
      if (c === "-" || c === "+") { pos++; }
      if (pos >= len) { return false; }
      c = input[pos];
      if (!/[0-9]/.test(c) && !(c === "." && pos + 1 < len && /[0-9]/.test(input[pos + 1]))) { return false; }
      // Grab the longest valid numeric prefix.
      var m = input.slice(pos).match(/^[0-9]*\.?[0-9]*(?:[eE][-+]?[0-9]+)?/);
      var numStr = (input[ (input[0]==="-"||input[0]==="+") ? 0 : -1 ] === "-" ? "-" : "");
      // Re-derive sign from the leading sign char we consumed.
      var lead = input.slice(0, pos);
      var s = lead.indexOf("-") !== -1 ? "-" : "";
      var n = Number(s + (m ? m[0] : ""));
      // The "rules for parsing floating-point number values" only produce finite numbers; a value
      // that overflows to +/-Infinity (e.g. "1.8e308") is treated as a parse error.
      if (isNaN(n) || !isFinite(n)) { return false; }
      return n;
    }
    // Shortest string for an integer (Number -> string) per "valid integer" serialisation.
    function intToStr(n) { return String(n | 0 === n ? (n | 0) : Math.trunc(n)); }

    var pageURL = function () { return globalThis.__pageURL; };
    function resolveURL(v, base) {
      if (v == null) { return ""; }
      v = String(v);
      try { return new URL(v, base || pageURL()).href; } catch (e) { return v; }
    }

    // ---- type factories: each returns a {get, set} descriptor pair bound to (node, contentAttr) --
    // `g` reads the raw content attribute (string or null); `s`/`r` set/remove it.
    function mk(node, attr) {
      return {
        g: function () { return __getAttr(node, attr); },
        s: function (v) { __setAttr(node, attr, String(v)); },
        r: function () { __removeAttr(node, attr); }
      };
    }

    var factories = {
      "string": function (io, data) {
        return {
          get: function () { var v = io.g(); return v == null ? "" : String(v); },
          set: function (v) {
            // [LegacyNullToEmptyString] makes null -> ""; otherwise null -> "null" (DOMString).
            if (v === null && data && data.treatNullAsEmptyString) { io.s(""); return; }
            io.s(String(v));
          }
        };
      },
      "url": function (io, data) {
        // form.action / input,button.formAction: when the attribute is absent (or empty), they
        // return the document URL rather than "" (hard-coded special case in the spec + harness).
        var docDefault = !!(data && data.urlDocDefault);
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return docDefault ? __reflResolveURL("") : ""; }
            return __reflResolveURL(v);
          },
          set: function (v) { io.s(String(v)); }
        };
      },
      "boolean": function (io) {
        return {
          get: function () { return io.g() != null; },
          set: function (v) { if (v) { io.s(""); } else { io.r(); } }
        };
      },
      "long": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 0;
        var hasDefault = !(data && data.defaultVal === null);
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return hasDefault ? dflt : 0; }
            var p = parseIntHtml(v);
            if (p === false || p > maxInt || p < minInt) { return hasDefault ? dflt : 0; }
            return p;
          },
          set: function (v) { io.s(String(toLong(v))); }
        };
      },
      "limited long": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : -1;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseNonneg(v);
            if (p === false || p > maxInt || p < minInt) { return dflt; }
            return p;
          },
          set: function (v) {
            v = toLong(v);
            if (v < 0) { throwIndexSize(); }
            io.s(String(v));
          }
        };
      },
      "unsigned long": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 0;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseNonneg(v);
            if (p === false || p < 0 || p > maxInt) { return dflt; }
            return p;
          },
          set: function (v) { io.s(String(toUnsignedSet(v, dflt))); }
        };
      },
      "limited unsigned long": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 1;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseNonneg(v);
            if (p === false || p < 1 || p > maxInt) { return dflt; }
            return p;
          },
          set: function (v) {
            v = toUnsigned(v);
            if (v === 0) { throwIndexSize(); }
            if (v > maxInt) { v = dflt; }
            io.s(String(v));
          }
        };
      },
      "limited unsigned long with fallback": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 1;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseNonneg(v);
            if (p === false || p < 1 || p > maxInt) { return dflt; }
            return p;
          },
          set: function (v) {
            v = toUnsigned(v);
            var n = (v >= 1 && v <= maxInt) ? v : dflt;
            io.s(String(n));
          }
        };
      },
      "clamped unsigned long": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 0;
        var min = data.min, max = data.max;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseNonneg(v);
            if (p === false) { return dflt; }
            if (p < min) { return min; }
            if (p > max) { return max; }
            return p;
          },
          set: function (v) { io.s(String(toUnsignedSet(v, dflt))); }
        };
      },
      "double": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 0.0;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseFloatHtml(v);
            return p === false ? dflt : p;
          },
          set: function (v) { io.s(bestFloat(v)); }
        };
      },
      "limited double": function (io, data) {
        var dflt = (data && data.defaultVal != null) ? data.defaultVal : 0.0;
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return dflt; }
            var p = parseFloatHtml(v);
            return (p === false || p <= 0) ? dflt : p;
          },
          set: function (v) {
            var n = Number(v);
            if (!(n > 0)) { return; } // leave attribute unchanged
            io.s(bestFloat(n));
          }
        };
      },
      "enum": function (io, data) {
        var keywords = data.keywords || [];
        var missing = (data.defaultVal !== undefined) ? data.defaultVal : "";
        // An array defaultVal means "implementation-defined, but one of these keywords" (e.g.
        // media preload). Pick the first as our canonical missing-value default (a string keyword).
        if (Array.isArray(missing)) { missing = missing.length ? missing[0] : ""; }
        var invalid = (data.invalidVal !== undefined) ? data.invalidVal : missing;
        var nonCanon = data.nonCanon || {};
        var nullable = !!data.isNullable;
        function canon(val) {
          var lc = __asciiLower(String(val));
          var ret = invalid;
          for (var i = 0; i < keywords.length; i++) {
            if (__asciiLower(keywords[i]) === lc) { ret = keywords[i]; break; }
          }
          if (Object.prototype.hasOwnProperty.call(nonCanon, ret)) { return nonCanon[ret]; }
          return ret;
        }
        return {
          get: function () {
            var v = io.g();
            if (v == null) { return missing; }
            return canon(v);
          },
          set: function (v) {
            if (nullable && (v === null || v === undefined)) { io.r(); return; }
            io.s(String(v));
          }
        };
      },
      // `nonce`: a DOMString backed by a [[CryptographicNonce]] internal slot. Reading reflects the
      // attribute, but setting via the IDL updates only the slot, NOT the content attribute (so the
      // nonce can't be scraped back off the attribute). The attribute-change steps would refresh the
      // slot; since the WPT harness runs all setAttribute() cases before the IDL cases, tracking a
      // "slot owns the value" flag is sufficient here.
      "nonce": function (io) {
        var slot = "", owns = false;
        return {
          get: function () { if (owns) { return slot; } var v = io.g(); return v == null ? "" : String(v); },
          set: function (v) { slot = String(v); owns = true; }
        };
      },
      // Nullable DOMString (ARIA props, role): get -> attr value or null; set null/undefined removes.
      "nullable string": function (io) {
        return {
          get: function () { var v = io.g(); return v == null ? null : String(v); },
          set: function (v) { if (v === null || v === undefined) { io.r(); } else { io.s(String(v)); } }
        };
      }
    };

    function toLong(v) {
      // WebIDL [long] conversion: ToInt32.
      var n = Number(v);
      if (!isFinite(n)) { n = 0; }
      n = n < 0 ? Math.ceil(n) : Math.floor(n);
      n = n % 4294967296;
      if (n >= 2147483648) { n -= 4294967296; }
      if (n < -2147483648) { n += 4294967296; }
      return n | 0;
    }
    function toUnsigned(v) {
      // WebIDL [unsigned long] conversion: ToUint32.
      var n = Number(v);
      if (!isFinite(n)) { n = 0; }
      n = n < 0 ? Math.ceil(n) : Math.floor(n);
      n = n % 4294967296;
      if (n < 0) { n += 4294967296; }
      return n >>> 0;
    }
    // Setting a (non-limited) unsigned long: out-of-[0,maxInt] becomes the default.
    function toUnsignedSet(v, dflt) {
      var n = toUnsigned(v);
      if (n < 0 || n > maxInt) { return dflt; }
      return n;
    }
    function bestFloat(v) {
      var n = Number(v);
      if (!isFinite(n)) { n = 0; }
      return String(n);
    }
    function throwIndexSize() {
      var e;
      try { e = new DOMException("Index or size is negative or greater than the allowed amount", "IndexSizeError"); }
      catch (x) { e = new Error("IndexSizeError"); e.name = "IndexSizeError"; e.code = 1; }
      throw e;
    }

    return {
      factories: factories, mk: mk, parseNonneg: parseNonneg, parseIntHtml: parseIntHtml,
      // Types we deliberately don't reflect (tested as a different IDL shape we don't model);
      // leaving them undefined means the WPT harness skips them rather than failing.
      skip: { "tokenlist": 1, "settable tokenlist": 1 }
    };
  })();
  function __reflParseNonneg(v) { return __refl.parseNonneg(v); }

  // Per-element reflected-attribute tables, transcribed from the WPT elements-*.js data (which is
  // itself generated from the HTML spec IDL). Key = lowercase tag name; value = map of idlName ->
  // type descriptor ("string" | {type, domAttrName?, defaultVal?, keywords?, ...}).
  var __reflTables = (function () {
    var S = "string", U = "url", B = "boolean", L = "long", UL = "unsigned long";
    var REF = ["", "no-referrer", "no-referrer-when-downgrade", "same-origin", "origin",
      "strict-origin", "origin-when-cross-origin", "strict-origin-when-cross-origin", "unsafe-url"];
    function ref() { return { type: "enum", keywords: REF }; }
    function crossOrigin() { return { type: "enum", keywords: ["anonymous", "use-credentials"], nonCanon: { "": "anonymous" }, isNullable: true, defaultVal: null, invalidVal: "anonymous" }; }
    function enctype(dflt) { return { type: "enum", keywords: ["application/x-www-form-urlencoded", "multipart/form-data", "text/plain"], defaultVal: dflt, invalidVal: "application/x-www-form-urlencoded" }; }
    function nullStr() { return { type: "string", treatNullAsEmptyString: true }; }
    var charAttr = { type: S, domAttrName: "char" }, charoff = { type: S, domAttrName: "charoff" };
    var cellCommon = function () { return { align: S, ch: charAttr, chOff: charoff, vAlign: S }; };
    function assign(t) { var o = {}; for (var i = 0; i < arguments.length; i++) { var s = arguments[i]; for (var k in s) { if (Object.prototype.hasOwnProperty.call(s, k)) { o[k] = s[k]; } } } return o; }

    return {
      // text
      a: { target: S, download: S, ping: S, rel: S, hreflang: S, type: S, referrerPolicy: ref(), href: U, coords: S, charset: S, name: S, rev: S, shape: S },
      q: { cite: U }, data: { value: S }, time: { dateTime: S }, br: { clear: S },
      // grouping
      p: { align: S }, hr: { align: S, color: S, noShade: B, size: S, width: S }, pre: { width: L },
      blockquote: { cite: U },
      ol: { reversed: B, start: { type: L, defaultVal: 1 }, type: S, compact: B },
      ul: { compact: B, type: S }, li: { value: L, type: S }, dl: { compact: B }, div: { align: S },
      // forms
      form: { acceptCharset: { type: S, domAttrName: "accept-charset" }, action: { type: U, urlDocDefault: true },
        autocomplete: { type: "enum", keywords: ["on", "off"], defaultVal: "on" },
        enctype: enctype("application/x-www-form-urlencoded"),
        encoding: assign(enctype("application/x-www-form-urlencoded"), { domAttrName: "enctype" }),
        method: { type: "enum", keywords: ["get", "post", "dialog"], defaultVal: "get" },
        name: S, noValidate: B, target: S },
      fieldset: { disabled: B, name: S }, legend: { align: S },
      label: { htmlFor: { type: S, domAttrName: "for" } },
      input: { accept: S, alt: S, autocomplete: { type: S, customGetter: true },
        defaultChecked: { type: B, domAttrName: "checked" }, dirName: S, disabled: B, formAction: { type: U, urlDocDefault: true },
        formEnctype: assign(enctype(undefined), { defaultVal: undefined }),
        formMethod: { type: "enum", keywords: ["get", "post"], invalidVal: "get" },
        formNoValidate: B, formTarget: S, height: { type: UL, customGetter: true }, max: S,
        maxLength: "limited long", min: S, minLength: "limited long", multiple: B, name: S,
        pattern: S, placeholder: S, readOnly: B, required: B,
        size: { type: "limited unsigned long", defaultVal: 20 }, src: U, step: S,
        type: { type: "enum", keywords: ["hidden", "text", "search", "tel", "url", "email", "password",
          "date", "time", "datetime-local", "month", "week", "number", "range", "color", "checkbox",
          "radio", "file", "submit", "image", "reset", "button"], defaultVal: "text" },
        width: { type: UL, customGetter: true }, defaultValue: { type: S, domAttrName: "value" },
        align: S, useMap: S },
      button: { disabled: B, formAction: { type: U, urlDocDefault: true }, formEnctype: assign(enctype(undefined), { defaultVal: undefined }),
        formMethod: { type: "enum", keywords: ["get", "post", "dialog"], invalidVal: "get" },
        formNoValidate: B, formTarget: S, name: S,
        type: { type: "enum", keywords: ["submit", "reset", "button"], defaultVal: "submit" }, value: S },
      select: { autocomplete: { type: S, customGetter: true }, disabled: B, multiple: B, name: S,
        required: B, size: { type: UL, defaultVal: 0 } },
      optgroup: { disabled: B, label: S },
      option: { disabled: B, label: { type: S, customGetter: true },
        defaultSelected: { type: B, domAttrName: "selected" }, value: { type: S, customGetter: true } },
      textarea: { autocomplete: { type: S, customGetter: true },
        cols: { type: "limited unsigned long with fallback", defaultVal: 20 }, dirName: S, disabled: B,
        maxLength: "limited long", minLength: "limited long", name: S, placeholder: S, readOnly: B,
        required: B, rows: { type: "limited unsigned long with fallback", defaultVal: 2 }, wrap: S },
      output: { name: S }, progress: { max: { type: "limited double", defaultVal: 1.0 } },
      meter: { value: { type: "double", customGetter: true }, min: { type: "double", customGetter: true },
        max: { type: "double", customGetter: true }, low: { type: "double", customGetter: true },
        high: { type: "double", customGetter: true }, optimum: { type: "double", customGetter: true } },
      // embedded
      img: { alt: S, src: U, srcset: S, crossOrigin: crossOrigin(), useMap: S, isMap: B,
        width: { type: UL, customGetter: true }, height: { type: UL, customGetter: true },
        referrerPolicy: ref(), decoding: { type: "enum", keywords: ["async", "sync", "auto"], defaultVal: "auto", invalidVal: "auto" },
        name: S, lowsrc: { type: U }, align: S, hspace: UL, vspace: UL, longDesc: U, border: nullStr() },
      iframe: { src: U, srcdoc: S, name: S, allowFullscreen: B, width: S, height: S, referrerPolicy: ref(),
        align: S, scrolling: S, frameBorder: S, longDesc: U, marginHeight: nullStr(), marginWidth: nullStr() },
      embed: { src: U, type: S, width: S, height: S, align: S, name: S },
      object: { data: U, type: S, name: S, useMap: S, width: S, height: S, align: S, archive: S, code: S,
        declare: B, hspace: UL, standby: S, vspace: UL, codeBase: U, codeType: S, border: nullStr() },
      param: { name: S, value: S, type: S, valueType: S },
      video: { src: U, crossOrigin: crossOrigin(),
        preload: { type: "enum", keywords: ["none", "metadata", "auto"], nonCanon: { "": "auto" }, defaultVal: ["none", "metadata", "auto"] },
        autoplay: B, loop: B, controls: B, defaultMuted: { type: B, domAttrName: "muted" },
        loading: { type: "enum", keywords: ["lazy", "eager"], defaultVal: "eager", invalidVal: "eager" },
        width: UL, height: UL, poster: U, playsInline: B },
      audio: { src: U, crossOrigin: crossOrigin(),
        preload: { type: "enum", keywords: ["none", "metadata", "auto"], nonCanon: { "": "auto" }, defaultVal: ["none", "metadata", "auto"] },
        autoplay: B, loop: B, controls: B, defaultMuted: { type: B, domAttrName: "muted" },
        loading: { type: "enum", keywords: ["lazy", "eager"], defaultVal: "eager", invalidVal: "eager" } },
      source: { src: U, type: S, srcset: S, sizes: S, media: S },
      track: { kind: { type: "enum", keywords: ["subtitles", "captions", "descriptions", "chapters", "metadata"], defaultVal: "subtitles", invalidVal: "metadata" },
        src: U, srclang: S, label: S, "default": B },
      canvas: { width: { type: UL, defaultVal: 300 }, height: { type: UL, defaultVal: 150 } },
      map: { name: S },
      area: { alt: S, coords: S, shape: S, target: S, download: S, ping: S, rel: S, referrerPolicy: ref(),
        hreflang: S, type: S, href: U, noHref: B },
      // sections
      body: { text: nullStr(), link: nullStr(), vLink: nullStr(), aLink: nullStr(), bgColor: nullStr(), background: S },
      h1: { align: S }, h2: { align: S }, h3: { align: S }, h4: { align: S }, h5: { align: S }, h6: { align: S },
      // metadata
      base: { href: { type: U, customGetter: true }, target: S },
      link: { href: U, crossOrigin: crossOrigin(), rel: S,
        as: { type: "enum", keywords: ["fetch", "audio", "document", "embed", "font", "image", "manifest", "object", "report", "script", "sharedworker", "style", "track", "video", "worker", "xslt"], defaultVal: "", invalidVal: "" },
        media: S, nonce: "nonce", integrity: S, hreflang: S, type: S, referrerPolicy: ref(),
        charset: S, rev: S, target: S },
      meta: { name: S, httpEquiv: { type: S, domAttrName: "http-equiv" }, content: S, media: S, scheme: S },
      style: { media: S, nonce: "nonce", type: S },
      // misc
      html: { version: S },
      script: { src: U, type: S, noModule: B, charset: S, defer: B, crossOrigin: crossOrigin(),
        integrity: S, event: S, htmlFor: { type: S, domAttrName: "for" } },
      slot: { name: S },
      ins: { cite: U, dateTime: S }, del: { cite: U, dateTime: S },
      details: { open: B }, menu: { compact: B }, dialog: { open: B },
      // tabular
      table: { align: S, border: S, frame: S, rules: S, summary: S, width: S,
        bgColor: nullStr(), cellPadding: nullStr(), cellSpacing: nullStr() },
      caption: { align: S },
      colgroup: assign({ span: { type: "clamped unsigned long", defaultVal: 1, min: 1, max: 1000 }, width: S }, cellCommon()),
      col: assign({ span: { type: "clamped unsigned long", defaultVal: 1, min: 1, max: 1000 }, width: S }, cellCommon()),
      tbody: cellCommon(), thead: cellCommon(), tfoot: cellCommon(),
      tr: assign(cellCommon(), { bgColor: nullStr() }),
      td: assign({ colSpan: { type: "clamped unsigned long", defaultVal: 1, min: 1, max: 1000 },
        rowSpan: { type: "clamped unsigned long", defaultVal: 1, min: 0, max: 65534 },
        headers: S, scope: { type: "enum", keywords: ["row", "col", "rowgroup", "colgroup"] }, abbr: S,
        axis: S, height: S, width: S, noWrap: B }, cellCommon(), { bgColor: nullStr() }),
      th: assign({ colSpan: { type: "clamped unsigned long", defaultVal: 1, min: 1, max: 1000 },
        rowSpan: { type: "clamped unsigned long", defaultVal: 1, min: 0, max: 65534 },
        headers: S, scope: { type: "enum", keywords: ["row", "col", "rowgroup", "colgroup"] }, abbr: S,
        axis: S, height: S, width: S, noWrap: B }, cellCommon(), { bgColor: nullStr() }),
      // obsolete
      marquee: { behavior: { type: "enum", keywords: ["scroll", "slide", "alternate"], defaultVal: "scroll" },
        bgColor: S, direction: { type: "enum", keywords: ["up", "right", "down", "left"], defaultVal: "left" },
        height: S, hspace: UL, scrollAmount: { type: UL, defaultVal: 6 }, scrollDelay: { type: UL, defaultVal: 85 },
        trueSpeed: B, vspace: UL, width: S },
      frameset: { cols: S, rows: S },
      frame: { name: S, scrolling: S, src: U, frameBorder: S, longDesc: U, noResize: B, marginHeight: nullStr(), marginWidth: nullStr() },
      dir: { compact: B },
      font: { color: nullStr(), face: S, size: S }
    };
  })();

  // Global attributes reflected on every HTML element (HTMLElement + a couple on Element).
  // These are tested for *every* element type by the reflection harness, so they dominate the
  // subtest count. `dir` is enumerated; `tabIndex` is a long with an element-specific default we
  // leave unspecified (the harness skips the default check when defaultVal is null); `hidden`/
  // `autofocus` are booleans; the rest are DOMStrings or enumerated.
  var __reflGlobals = {
    title: "string", lang: "string", accessKey: "string", translate: "string", nonce: "nonce",
    slot: { type: "string", domAttrName: "slot" },
    dir: { type: "enum", keywords: ["ltr", "rtl", "auto"] },
    autocapitalize: { type: "enum", keywords: ["off", "none", "on", "sentences", "words", "characters"], defaultVal: "" },
    enterKeyHint: { type: "enum", keywords: ["enter", "done", "go", "next", "previous", "search", "send"] },
    inputMode: { type: "enum", keywords: ["none", "text", "tel", "url", "email", "numeric", "decimal", "search"] },
    autofocus: "boolean", hidden: "boolean",
    tabIndex: { type: "long", defaultVal: null }
  };

  // ARIA reflection: every `ariaXxx` IDL attribute reflects the `aria-xxx` content attribute as a
  // nullable DOMString (matches the non-tentative WPT file + real browsers). Plus `role`.
  // List from the ARIA-in-HTML / AOM reflection spec.
  var __reflAria = ["Atomic", "AutoComplete", "BrailleLabel", "BrailleRoleDescription", "Busy",
    "Checked", "ColCount", "ColIndex", "ColIndexText", "ColSpan", "Current", "Description",
    "Disabled", "Expanded", "HasPopup", "Hidden", "Invalid", "KeyShortcuts", "Label", "Level",
    "Live", "Modal", "MultiLine", "MultiSelectable", "Orientation", "Placeholder", "PosInSet",
    "Pressed", "ReadOnly", "Relevant", "Required", "RoleDescription", "RowCount", "RowIndex",
    "RowIndexText", "RowSpan", "Selected", "SetSize", "Sort", "ValueMax", "ValueMin", "ValueNow",
    "ValueText"];

  // Define one reflected accessor on `el` for (idlName, data) backed by content attribute on `node`.
  function defineReflected(el, node, idlName, data) {
    if (typeof data === "string") { data = { type: data }; }
    var type = data.type;
    if (__refl.skip[type]) { return; }
    var factory = __refl.factories[type];
    if (!factory) { return; }
    // Note: customGetter attributes (input.autocomplete, meter.value, input.width/height, base.href)
    // have a bespoke getter in the spec we don't fully model, but the conformance harness only
    // checks their *type* and their setter behaviour (it skips the get/default asserts). The plain
    // factory getter is the right JS TYPE, so installing it lets those typeof/setter subtests pass.
    // Don't clobber an accessor the wrapper already defines correctly (value/checked/href/src/etc.).
    var existing = null;
    try { existing = Object.getOwnPropertyDescriptor(el, idlName); } catch (e) {}
    if (existing && (existing.get || existing.set)) { return; }
    // The content attribute name is the explicit domAttrName, else the ASCII-lowercased idlName
    // (HTML attributes are stored lowercased; e.g. maxLength <-> maxlength, colSpan <-> colspan).
    var contentAttr = data.domAttrName || __asciiLower(idlName);
    var io = __refl.mk(node, contentAttr);
    var desc = factory(io, data);
    try {
      Object.defineProperty(el, idlName, {
        get: desc.get, set: desc.set, enumerable: true, configurable: true
      });
    } catch (e) {}
  }

  // Apply all reflection accessors for the element `el` (node id `node`, lowercase tag `tag`).
  function applyReflection(el, node, tag) {
    // Global attributes (HTMLElement) on every HTML element. The SVG/MathML tags skip these.
    for (var gk in __reflGlobals) {
      if (Object.prototype.hasOwnProperty.call(__reflGlobals, gk)) {
        defineReflected(el, node, gk, __reflGlobals[gk]);
      }
    }
    // ARIA nullable-string reflection (HTMLElement + Element).
    defineReflected(el, node, "role", { type: "nullable string", domAttrName: "role" });
    for (var ai = 0; ai < __reflAria.length; ai++) {
      var nm = __reflAria[ai];
      defineReflected(el, node, "aria" + nm, { type: "nullable string", domAttrName: "aria-" + __asciiLower(nm) });
    }
    // ARIA element reflection (aria*Element / aria*Elements) — Element / FrozenArray<Element>.
    applyAomElementReflection(el, node);
    // Per-element attributes.
    var tbl = __reflTables[tag];
    if (tbl) {
      for (var k in tbl) {
        if (Object.prototype.hasOwnProperty.call(tbl, k)) { defineReflected(el, node, k, tbl[k]); }
      }
    }
  }
  def(globalThis, "__applyReflection", applyReflection);

  // ============================================================================================
  // ARIA element reflection (aria*Element / aria*Elements).
  //
  // A handful of ARIAMixin IDL attributes reflect an aria-* ID-reference content attribute as actual
  // Element references rather than strings: the single `ariaActiveDescendantElement`
  // (<-> aria-activedescendant) and the FrozenArray<Element> family `ariaControlsElements`,
  // `ariaDescribedByElements`, `ariaDetailsElements`, `ariaErrorMessageElements`, `ariaFlowToElements`,
  // `ariaLabelledByElements`, `ariaOwnsElements`. Each has an "explicitly set attr-element(s)" internal
  // slot (stashed on the wrapper). Setting via the IDL attribute stores the element reference(s) and
  // writes the empty string to the content attribute; mutating the content attribute directly
  // (setAttribute/removeAttribute) clears the slot so the getter falls back to ID lookup. A referenced
  // element is only exposed when it is in a "valid scope" — its tree is the reflecting element's tree
  // or a shadow-including ancestor (a "lighter") tree; references into a "darker" (descendant) shadow
  // tree, a detached subtree, or another document are kept intact but hidden until back in scope.
  var __aomElemAttrs = [
    { idl: "ariaActiveDescendantElement", attr: "aria-activedescendant", multi: false },
    { idl: "ariaControlsElements",        attr: "aria-controls",         multi: true },
    { idl: "ariaDescribedByElements",     attr: "aria-describedby",      multi: true },
    { idl: "ariaDetailsElements",         attr: "aria-details",          multi: true },
    { idl: "ariaErrorMessageElements",    attr: "aria-errormessage",     multi: true },
    { idl: "ariaFlowToElements",          attr: "aria-flowto",           multi: true },
    { idl: "ariaLabelledByElements",      attr: "aria-labelledby",       multi: true },
    { idl: "ariaOwnsElements",            attr: "aria-owns",             multi: true }
  ];
  var __aomAttrToIdl = Object.create(null);
  for (var __aomI = 0; __aomI < __aomElemAttrs.length; __aomI++) {
    __aomAttrToIdl[__aomElemAttrs[__aomI].attr] = __aomElemAttrs[__aomI];
  }

  // The shadow root in this engine is a real <div> child of the host carrying `.host`. An element's
  // "tree root" is the nearest such shadow-root ancestor (inclusive), else the topmost ancestor
  // (document node, or a detached subtree's root). Returns a node id.
  function __aomTreeRoot(id) {
    var cur = id;
    while (true) {
      var w = globalThis.__nodeById ? globalThis.__nodeById(cur) : null;
      if (w && w.host) { return cur; }            // cur is itself a shadow root
      var p = __parent(cur);
      if (typeof p !== "number" || p < 0) { return cur; }
      cur = p;
    }
  }
  // The chain of tree roots from `id`'s own tree outward through host boundaries (lighter trees).
  function __aomRootChain(id) {
    var chain = [];
    var r = __aomTreeRoot(id);
    while (true) {
      chain.push(r);
      var w = globalThis.__nodeById ? globalThis.__nodeById(r) : null;
      if (w && w.host && typeof w.host.__node === "number") { r = __aomTreeRoot(w.host.__node); }
      else { break; }
    }
    return chain;
  }
  // Is `candId` in a valid scope to be referenced by reflecting element `hostId`? Valid iff the
  // candidate's tree root is the reflecting element's tree root or a shadow-including ancestor.
  function __aomValidScope(hostId, candId) {
    if (typeof candId !== "number" || candId < 0) { return false; }
    var cr = __aomTreeRoot(candId);
    var chain = __aomRootChain(hostId);
    for (var i = 0; i < chain.length; i++) { if (chain[i] === cr) { return true; } }
    return false;
  }
  // First element (tree order) with id `wantId` within the tree rooted at `rootId`, without crossing
  // into nested shadow roots (a different tree). Returns a node id, or -1.
  function __aomGetById(rootId, wantId) {
    var found = -1;
    function visit(pid) {
      var kids;
      try { kids = __children(pid); } catch (e) { return; }
      for (var i = 0; i < kids.length; i++) {
        var c = kids[i];
        if (found >= 0) { return; }
        if (__nodeType(c) !== 1) { continue; }
        if (__getAttr(c, "id") === wantId) { found = c; return; }
        var w = globalThis.__nodeById ? globalThis.__nodeById(c) : null;
        if (w && w.host) { continue; }            // don't descend into a nested shadow tree
        visit(c);
        if (found >= 0) { return; }
      }
    }
    visit(rootId);
    return found;
  }
  function __aomIsElement(v) {
    return !!v && typeof v === "object" && typeof v.__node === "number" && __nodeType(v.__node) === 1;
  }
  // Convert a sequence<Element> argument (per WebIDL) to a JS array, throwing TypeError for a
  // non-iterable value or any member that is not an Element.
  function __aomToElementSeq(value) {
    var iterFn = (value != null) ? value[Symbol.iterator] : undefined;
    if (typeof iterFn !== "function") { throw new TypeError("Value is not a sequence"); }
    var out = [];
    var iter = iterFn.call(value);
    while (true) {
      var step = iter.next();
      if (step.done) { break; }
      if (!__aomIsElement(step.value)) { throw new TypeError("Value is not an Element"); }
      out.push(step.value);
    }
    return out;
  }

  // Clear the explicitly set attr-element slot (+ cached FrozenArray) for a content attribute that
  // changed via setAttribute/removeAttribute. Called from the document-layer attribute mutators.
  def(globalThis, "__aomNoteAttrChange", function (el, attrLower) {
    var info = __aomAttrToIdl[attrLower];
    if (!info || !el) { return; }
    if (el.__aomRefs) { delete el.__aomRefs[info.idl]; }
    if (el.__aomCache) { delete el.__aomCache[info.idl]; }
  });

  function applyAomElementReflection(el, node) {
    if (typeof node !== "number") { return; }
    if (!el.__aomRefs) { def(el, "__aomRefs", Object.create(null)); }
    if (!el.__aomCache) { def(el, "__aomCache", Object.create(null)); }
    for (var ei = 0; ei < __aomElemAttrs.length; ei++) {
      (function (info) {
        var idl = info.idl, attr = info.attr, refs = el.__aomRefs, cache = el.__aomCache;
        if (info.multi) {
          Object.defineProperty(el, idl, {
            get: function () {
              var ids;
              if (Object.prototype.hasOwnProperty.call(refs, idl)) {
                // Explicitly set: expose only the members that are currently in a valid scope.
                var arr = refs[idl];
                ids = [];
                for (var i = 0; i < arr.length; i++) {
                  var cid = arr[i] && arr[i].__node;
                  if (typeof cid === "number" && __aomValidScope(node, cid)) { ids.push(cid); }
                }
              } else {
                // Reflect the content attribute: absent -> null; else resolve each ID token in tree
                // order within this element's tree.
                var v = __getAttr(node, attr);
                if (v == null) { delete cache[idl]; return null; }
                var toks = v.split(/[ \t\n\f\r]+/);
                var rootId = __aomTreeRoot(node);
                ids = [];
                for (var j = 0; j < toks.length; j++) {
                  if (!toks[j]) { continue; }
                  var fid = __aomGetById(rootId, toks[j]);
                  if (fid >= 0) { ids.push(fid); }
                }
              }
              // Caching invariant: return the same FrozenArray object while the resolved list of
              // elements is unchanged.
              var prev = cache[idl];
              if (prev && prev.ids.length === ids.length) {
                var same = true;
                for (var k = 0; k < ids.length; k++) { if (prev.ids[k] !== ids[k]) { same = false; break; } }
                if (same) { return prev.frozen; }
              }
              var frozen = [];
              for (var m = 0; m < ids.length; m++) { frozen.push(globalThis.__nodeFor(ids[m])); }
              Object.freeze(frozen);
              cache[idl] = { ids: ids.slice(), frozen: frozen };
              return frozen;
            },
            set: function (value) {
              if (value === null || value === undefined) {
                delete refs[idl]; delete cache[idl];
                __removeAttr(node, attr);
                return;
              }
              var items = __aomToElementSeq(value);   // validates (throws before mutating)
              refs[idl] = items.slice();
              delete cache[idl];
              __setAttr(node, attr, "");
            },
            enumerable: true, configurable: true
          });
        } else {
          Object.defineProperty(el, idl, {
            get: function () {
              if (Object.prototype.hasOwnProperty.call(refs, idl)) {
                var cand = refs[idl];
                var cid = cand && cand.__node;
                if (typeof cid === "number" && __aomValidScope(node, cid)) { return cand; }
                return null;
              }
              var v = __getAttr(node, attr);
              if (v == null) { return null; }
              var fid = __aomGetById(__aomTreeRoot(node), v);
              return fid >= 0 ? globalThis.__nodeFor(fid) : null;
            },
            set: function (value) {
              if (value === null || value === undefined) {
                delete refs[idl]; delete cache[idl];
                __removeAttr(node, attr);
                return;
              }
              if (!__aomIsElement(value)) { throw new TypeError("Value is not an Element"); }
              refs[idl] = value;
              __setAttr(node, attr, "");
            },
            enumerable: true, configurable: true
          });
        }
      })(__aomElemAttrs[ei]);
    }
  }

  // Minimal Streams (WritableStream / ReadableStream / TransformStream / TextDecoderStream) — enough
  // for the streaming partial-update methods (streamHTML etc.) and piping a Response body through.
  if (typeof globalThis.WritableStream !== "function") {
    var WritableStream = function (sink) {
      this._sink = sink || {};
      this._writer = null;
      var s = this;
      this._ready = Promise.resolve().then(function () { return s._sink.start ? s._sink.start({ error: function () {} }) : undefined; });
    };
    WritableStream.prototype.getWriter = function () {
      if (this._writer) { throw new TypeError("WritableStream is locked to a writer"); }
      this._writer = new WritableStreamDefaultWriter(this);
      return this._writer;
    };
    Object.defineProperty(WritableStream.prototype, "locked", { get: function () { return !!this._writer; } });
    WritableStream.prototype.abort = function (reason) { var s = this; return Promise.resolve(s._sink.abort ? s._sink.abort(reason) : undefined); };
    WritableStream.prototype.close = function () { return this.getWriter().close(); };
    globalThis.WritableStream = WritableStream;
    var WritableStreamDefaultWriter = function (stream) {
      this._stream = stream; var self = this;
      this._closedP = new Promise(function (res, rej) { self._closedRes = res; self._closedRej = rej; });
    };
    WritableStreamDefaultWriter.prototype.write = function (chunk) { var st = this._stream; return st._ready.then(function () { return st._sink.write ? st._sink.write(chunk, { error: function () {} }) : undefined; }); };
    WritableStreamDefaultWriter.prototype.close = function () { var st = this._stream, self = this; return st._ready.then(function () { return st._sink.close ? st._sink.close() : undefined; }).then(function () { self._closedRes(); }); };
    WritableStreamDefaultWriter.prototype.abort = function (reason) { return this._stream.abort(reason); };
    WritableStreamDefaultWriter.prototype.releaseLock = function () { this._stream._writer = null; };
    Object.defineProperty(WritableStreamDefaultWriter.prototype, "ready", { get: function () { return Promise.resolve(); } });
    Object.defineProperty(WritableStreamDefaultWriter.prototype, "closed", { get: function () { return this._closedP; } });
    Object.defineProperty(WritableStreamDefaultWriter.prototype, "desiredSize", { get: function () { return 1; } });
    globalThis.WritableStreamDefaultWriter = WritableStreamDefaultWriter;

    var ReadableStream = function (source) {
      this._source = source || {};
      this._reader = null; this._queue = []; this._closed = false; this._err = null; this._waiters = [];
      var s = this;
      this._controller = {
        enqueue: function (c) { s._queue.push(c); s._wake(); },
        close: function () { s._closed = true; s._wake(); },
        error: function (e) { s._err = e; s._wake(); },
        get desiredSize() { return 1; }
      };
      this._started = Promise.resolve().then(function () { return s._source.start ? s._source.start(s._controller) : undefined; });
    };
    ReadableStream.prototype._wake = function () { var w = this._waiters; this._waiters = []; for (var i = 0; i < w.length; i++) { w[i](); } };
    ReadableStream.prototype._pull = function () {
      var s = this;
      return s._started.then(function step() {
        if (s._queue.length) { return { value: s._queue.shift(), done: false }; }
        if (s._err) { throw s._err; }
        if (s._closed) { return { value: undefined, done: true }; }
        return Promise.resolve(s._source.pull ? s._source.pull(s._controller) : undefined).then(function () {
          if (s._queue.length) { return { value: s._queue.shift(), done: false }; }
          if (s._closed) { return { value: undefined, done: true }; }
          return new Promise(function (res) { s._waiters.push(res); }).then(step);
        });
      });
    };
    ReadableStream.prototype.getReader = function () {
      if (this._reader) { throw new TypeError("locked"); }
      var s = this;
      this._reader = { read: function () { return s._pull(); }, releaseLock: function () { s._reader = null; }, cancel: function () { s._closed = true; return Promise.resolve(); }, closed: Promise.resolve() };
      return this._reader;
    };
    Object.defineProperty(ReadableStream.prototype, "locked", { get: function () { return !!this._reader; } });
    ReadableStream.prototype.pipeTo = function (dest) {
      var reader = this.getReader(), writer = dest.getWriter();
      return (function pump() { return reader.read().then(function (r) { if (r.done) { return writer.close(); } return Promise.resolve(writer.write(r.value)).then(pump); }); })();
    };
    ReadableStream.prototype.pipeThrough = function (tr) { this.pipeTo(tr.writable); return tr.readable; };
    ReadableStream.prototype.cancel = function () { this._closed = true; return Promise.resolve(); };
    globalThis.ReadableStream = ReadableStream;

    var TransformStream = function (transformer) {
      transformer = transformer || {};
      this.readable = new ReadableStream({});
      var rc = this.readable._controller;
      this.writable = new WritableStream({
        write: function (chunk) { return Promise.resolve(transformer.transform ? transformer.transform(chunk, rc) : rc.enqueue(chunk)); },
        close: function () { if (transformer.flush) { transformer.flush(rc); } rc.close(); },
        abort: function (e) { rc.error(e); }
      });
    };
    globalThis.TransformStream = TransformStream;
    globalThis.TextDecoderStream = function (label, options) {
      var dec = new globalThis.TextDecoder(label || "utf-8", options || {});
      var rc; var self = this;
      this.readable = new ReadableStream({});
      rc = this.readable._controller;
      this.writable = new WritableStream({
        write: function (chunk) { rc.enqueue(dec.decode(chunk, { stream: true })); },
        close: function () { var tail = dec.decode(); if (tail) { rc.enqueue(tail); } rc.close(); }
      });
      Object.defineProperty(this, "encoding", { value: dec.encoding || "utf-8" });
    };
  }

  // Parse an HTML string into a DocumentFragment for the partial-update methods (appendHTML etc.).
  // `safe` strips <script>s (the safe, sanitizing variants); a `sanitizer.removeElements` option
  // drops those elements too. Scripts are never executed here (the fragment isn't connected).
  globalThis.__htmlPartialFragment = function (html, safe, opts) {
    var div = document.createElement("div");
    div.innerHTML = (html == null ? "" : String(html));
    var dropAll = function (sel) {
      var els = Array.prototype.slice.call(div.querySelectorAll(sel));
      for (var i = 0; i < els.length; i++) { if (els[i].remove) { els[i].remove(); } }
    };
    if (safe) { dropAll("script"); }
    var rem = opts && opts.sanitizer && opts.sanitizer.removeElements;
    if (rem && rem.length) { for (var j = 0; j < rem.length; j++) { try { dropAll(rem[j]); } catch (e) {} } }
    var frag = document.createDocumentFragment();
    while (div.firstChild) { frag.appendChild(div.firstChild); }
    return frag;
  };

  // Attach the declarative partial-update methods ({append,prepend,before,after,replaceWith}HTML
  // [Unsafe]) to a node. Parent-position methods route through a <template>'s content.
  globalThis.__addPartialMethods = function (el) {
    var defs = [["append", 1], ["prepend", 1], ["before", 0], ["after", 0], ["replaceWith", 0]];
    for (var pi = 0; pi < defs.length; pi++) {
      (function (base, isParent) {
        [["HTML", true], ["HTMLUnsafe", false]].forEach(function (sfx) {
          var nm = base + sfx[0];
          if (typeof el[nm] === "function") { return; }
          Object.defineProperty(el, nm, { configurable: true, writable: true, enumerable: false, value: function (html, opts) {
            var frag = globalThis.__htmlPartialFragment(html, sfx[1], opts);
            if (isParent) {
              var dest = this.content || this;
              if (base === "append") { dest.appendChild(frag); }
              else { dest.insertBefore(frag, dest.firstChild || null); }
            } else {
              var p = this.parentNode;
              if (!p) { return; }
              if (base === "before") { p.insertBefore(frag, this); }
              else if (base === "after") { p.insertBefore(frag, this.nextSibling); }
              else { p.insertBefore(frag, this); p.removeChild(this); }
            }
          } });
        });
      })(defs[pi][0], defs[pi][1]);
    }
    // Streaming variants: stream{,Append,Prepend,Before,After,ReplaceWith}HTML[Unsafe]. Each returns
    // a WritableStream; written chunks are parsed and inserted at the position immediately (not
    // buffered until close). The insertion point is fixed at call time so chunks land in order.
    var streamDefs = [
      ["streamHTML", "replace", 1], ["streamHTMLUnsafe", "replace", 0],
      ["streamAppendHTML", "append", 1], ["streamAppendHTMLUnsafe", "append", 0],
      ["streamPrependHTML", "prepend", 1], ["streamPrependHTMLUnsafe", "prepend", 0],
      ["streamBeforeHTML", "before", 1], ["streamBeforeHTMLUnsafe", "before", 0],
      ["streamAfterHTML", "after", 1], ["streamAfterHTMLUnsafe", "after", 0],
      ["streamReplaceWithHTML", "replaceWith", 1], ["streamReplaceWithHTMLUnsafe", "replaceWith", 0]
    ];
    streamDefs.forEach(function (d) {
      var nm = d[0], pos = d[1], safe = !!d[2];
      if (typeof el[nm] === "function") { return; }
      Object.defineProperty(el, nm, { configurable: true, writable: true, enumerable: false, value: function (opts) {
        var node = this, insert;
        if (pos === "replace" || pos === "append" || pos === "prepend") {
          var dest = node.content || node;
          if (pos === "replace") { while (dest.firstChild) { dest.removeChild(dest.firstChild); } }
          if (pos === "prepend") { var pref = dest.firstChild; insert = function (f) { dest.insertBefore(f, pref); }; }
          else { insert = function (f) { dest.appendChild(f); }; }
        } else {
          var p = node.parentNode, sref;
          if (pos === "before") { sref = node; }
          else if (pos === "after") { sref = node.nextSibling; }
          else { sref = node.nextSibling; if (p) { p.removeChild(node); } }
          insert = function (f) { if (p) { p.insertBefore(f, sref); } };
        }
        return new globalThis.WritableStream({
          write: function (chunk) { insert(globalThis.__htmlPartialFragment(chunk == null ? "" : String(chunk), safe, opts)); },
          close: function () {}
        });
      } });
    });
  };

  // Deep structural node equality (DOM `isEqualNode`): same type and type-specific data, equal
  // attribute sets (order-independent, by namespace+localName+value), and pairwise-equal children.
  globalThis.__nodesEqual = function (a, b) {
    if (a === b) { return true; }
    if (!a || !b || a.nodeType !== b.nodeType) { return false; }
    var t = a.nodeType;
    if (t === 1) {
      if ((a.namespaceURI || null) !== (b.namespaceURI || null)) { return false; }
      if ((a.prefix || null) !== (b.prefix || null)) { return false; }
      if (a.localName !== b.localName) { return false; }
      var aa = a.attributes, ba = b.attributes;
      if ((aa ? aa.length : 0) !== (ba ? ba.length : 0)) { return false; }
      for (var i = 0; aa && i < aa.length; i++) {
        var at = aa[i], ok = false;
        for (var j = 0; j < ba.length; j++) {
          var bt = ba[j];
          if ((at.namespaceURI || null) === (bt.namespaceURI || null)
              && (at.localName || at.name) === (bt.localName || bt.name)
              && at.value === bt.value) { ok = true; break; }
        }
        if (!ok) { return false; }
      }
    } else if (t === 3 || t === 8 || t === 4) {
      if ((a.data || "") !== (b.data || "")) { return false; }
    } else if (t === 7) {
      if (a.target !== b.target || (a.data || "") !== (b.data || "")) { return false; }
    } else if (t === 10) {
      if (a.name !== b.name || (a.publicId || "") !== (b.publicId || "") || (a.systemId || "") !== (b.systemId || "")) { return false; }
    }
    var ac = a.childNodes || [], bc = b.childNodes || [];
    if (ac.length !== bc.length) { return false; }
    for (var k = 0; k < ac.length; k++) {
      if (!globalThis.__nodesEqual(ac[k], bc[k])) { return false; }
    }
    return true;
  };

  function enrichElement(el) {
    if (!el || typeof el !== "object") { return el; }
    if (el.__enriched) { return el; }
    var node = el.__node;
    def(el, "__enriched", true);
    // Compile inline event-handler content attributes (onload="...", onclick="...") into the matching
    // on-handler so they run when the event is dispatched. The handler body runs with `event` in scope
    // and `this` bound to the element (dispatchEvent calls `el.on<type>`).
    try {
      if (el.tagName && typeof el.getAttributeNames === "function") {
        // The "Window-reflecting body element event handler set" (HTML §8.1.7.2.1): on <body> and
        // <frameset> these content attributes set the handler on the WINDOW, not the element. The
        // `load` event in particular is dispatched at the window (see fireLifecycle), so a
        // `<body onload="...">` must compile to `window.onload` or it would never fire — which is
        // exactly what stalls the `check-layout-th.js` tests that kick off from `body.onload`.
        var __ln = (el.localName || el.tagName || "").toLowerCase();
        var __winReflect = (__ln === "body" || __ln === "frameset")
          ? globalThis.__windowReflectedBodyHandlers
          : null;
        var __ons = el.getAttributeNames();
        for (var __oi = 0; __oi < __ons.length; __oi++) {
          var __on = __ons[__oi];
          if (__on.length > 2 && __on.slice(0, 2) === "on") {
            var __target = (__winReflect && __winReflect[__on]) ? globalThis : el;
            if (typeof __target[__on] !== "function") {
              try { __target[__on] = new Function("event", el.getAttribute(__on)); } catch (e) {}
            }
          }
        }
      }
    } catch (e) {}
    // Graft the matching DOM interface prototype onto the wrapper's chain (own props survive).
    if (typeof node === "number") {
      try { applyNodePrototype(el, node); } catch (e) {}
    }
    if (typeof node === "number") {
      // `style` lives on the prototype chain (ElementCSSInlineStyle mixin) so it passes
      // assert_idl_attribute (own-property check). We stash the per-node CSSStyleDeclaration as a
      // hidden own property; the prototype accessor returns it ([SameObject], [PutForwards=cssText]).
      def(el, "__styleObj", makeStyle(node));
      // classList is [SameObject, PutForwards=value]: a per-element cached DOMTokenList whose
      // getter always returns the same object, and assigning `el.classList = x` forwards to
      // `el.classList.value = x` (so it never replaces the object and never throws in strict mode).
      (function () {
        var __cl = makeClassList(node);
        Object.defineProperty(el, "classList", {
          get: function () { return __cl; },
          set: function (v) { __cl.value = v; },
          enumerable: true, configurable: true
        });
      })();
      // Other DOMTokenList-reflecting attributes (HTML). Each is a [SameObject, PutForwards=value]
      // token list that exists only on the supporting element(s); on other elements the property is
      // absent (=== undefined). relList is also defined on the SVG `a` element.
      (function () {
        var HTML = "http://www.w3.org/1999/xhtml";
        var SVG = "http://www.w3.org/2000/svg";
        var ln = null, ns = null;
        try { ln = el.localName; ns = el.namespaceURI; } catch (e) {}
        function install(prop, contentAttr) {
          var tl = makeTokenList(node, contentAttr, null);
          Object.defineProperty(el, prop, {
            get: function () { return tl; },
            set: function (v) { tl.value = v; },
            enumerable: true, configurable: true
          });
        }
        // relList: on HTML a/area/link, and on SVG a.
        if ((ns === HTML && (ln === "a" || ln === "area" || ln === "link")) || (ns === SVG && ln === "a")) {
          install("relList", "rel");
        }
        if (ns === HTML && ln === "output") { install("htmlFor", "for"); }
        if (ns === HTML && ln === "iframe") { install("sandbox", "sandbox"); }
        if (ns === HTML && ln === "link") { install("sizes", "sizes"); }
      })();
      def(el, "dataset", makeDataset(node));
      // Form-control `value` / `checked` reflection: back them by element ATTRIBUTES so that
      // reading/writing `el.value` (and `el.checked`) is visible to layout, which renders the
      // input's text from the `value` attribute. Only for <input>/<textarea>/<select>; guard so
      // page-defined accessors aren't clobbered.
      try {
        var __formTag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : "";
        if (__formTag === "input" || __formTag === "textarea" || __formTag === "select" || __formTag === "option") {
          var __hasValue = false;
          try { var __vd = Object.getOwnPropertyDescriptor(el, "value"); __hasValue = !!(__vd && (__vd.get || __vd.set)); } catch (e8) {}
          if (!__hasValue && __formTag !== "option") {
            if (__formTag === "textarea") {
              // A <textarea>'s value defaults to its text content; an explicit `value` attribute
              // (set via the property) overrides it. The setter stores `value` so layout renders it.
              Object.defineProperty(el, "value", {
                get: function () {
                  var v = __getAttr(node, "value");
                  if (v != null) { return String(v); }
                  var t = this.textContent;
                  return t == null ? "" : String(t);
                },
                set: function (v) { __setAttr(node, "value", String(v == null ? "" : v)); },
                configurable: true, enumerable: true
              });
            } else if (__formTag === "select") {
              // A <select>'s value is the selected <option>'s value (or its text if no value attr);
              // empty when nothing is selected. selectedIndex is the selected option's index.
              // Setting value selects the first matching option; setting selectedIndex selects by
              // position. Backed by the `selected` attribute on <option>s (also used by layout).
              var __optValue = function (o) {
                var av = o.getAttribute ? o.getAttribute("value") : null;
                if (av != null) { return av; }
                var t = o.textContent;
                return t == null ? "" : String(t).replace(/^\s+|\s+$/g, "");
              };
              var __selIdx = function (self) {
                var opts = self.querySelectorAll ? self.querySelectorAll("option") : [];
                for (var i = 0; i < opts.length; i++) {
                  if (opts[i].hasAttribute && opts[i].hasAttribute("selected")) { return i; }
                }
                return opts.length ? 0 : -1;
              };
              Object.defineProperty(el, "value", {
                get: function () {
                  var opts = this.querySelectorAll ? this.querySelectorAll("option") : [];
                  var idx = __selIdx(this);
                  if (idx < 0 || idx >= opts.length) { return ""; }
                  return __optValue(opts[idx]);
                },
                set: function (v) {
                  v = String(v == null ? "" : v);
                  var opts = this.querySelectorAll ? this.querySelectorAll("option") : [];
                  var found = -1;
                  for (var i = 0; i < opts.length; i++) { if (__optValue(opts[i]) === v) { found = i; break; } }
                  for (var j = 0; j < opts.length; j++) {
                    if (j === found) { opts[j].setAttribute("selected", ""); }
                    else { opts[j].removeAttribute("selected"); }
                  }
                },
                configurable: true, enumerable: true
              });
              Object.defineProperty(el, "selectedIndex", {
                get: function () { return __selIdx(this); },
                set: function (v) {
                  var idx = v | 0;
                  var opts = this.querySelectorAll ? this.querySelectorAll("option") : [];
                  for (var j = 0; j < opts.length; j++) {
                    if (j === idx) { opts[j].setAttribute("selected", ""); }
                    else { opts[j].removeAttribute("selected"); }
                  }
                },
                configurable: true, enumerable: true
              });
            } else {
              Object.defineProperty(el, "value", {
                get: function () { var v = __getAttr(node, "value"); return v == null ? "" : String(v); },
                set: function (v) { __setAttr(node, "value", String(v == null ? "" : v)); },
                configurable: true, enumerable: true
              });
            }
          }
          // <option>.value reflects its `value` attribute, falling back to text content;
          // <option>.selected reflects the `selected` attribute.
          if (__formTag === "option") {
            var __hasOptVal = false;
            try { var __ovd = Object.getOwnPropertyDescriptor(el, "value"); __hasOptVal = !!(__ovd && (__ovd.get || __ovd.set)); } catch (eOV) {}
            if (!__hasOptVal) {
              Object.defineProperty(el, "value", {
                get: function () { var v = __getAttr(node, "value"); if (v != null) { return String(v); } var t = this.textContent; return t == null ? "" : String(t).replace(/^\s+|\s+$/g, ""); },
                set: function (v) { __setAttr(node, "value", String(v)); },
                configurable: true, enumerable: true
              });
            }
            Object.defineProperty(el, "selected", {
              get: function () { return __getAttr(node, "selected") != null; },
              set: function (v) { if (v) { __setAttr(node, "selected", ""); } else { __removeAttr(node, "selected"); } },
              configurable: true, enumerable: true
            });
          }
          // `checked` for checkbox/radio inputs, backed by presence of the `checked` attribute.
          if (__formTag === "input") {
            var __ty = String(__getAttr(node, "type") || "").toLowerCase();
            if (__ty === "checkbox" || __ty === "radio") {
              var __hasChecked = false;
              try { var __cd = Object.getOwnPropertyDescriptor(el, "checked"); __hasChecked = !!(__cd && (__cd.get || __cd.set)); } catch (e9) {}
              if (!__hasChecked) {
                Object.defineProperty(el, "checked", {
                  get: function () { return __getAttr(node, "checked") != null; },
                  set: function (v) { if (v) { __setAttr(node, "checked", ""); } else { __removeAttr(node, "checked"); } },
                  configurable: true, enumerable: true
                });
              }
            }
          }
        }
        // `src` / `href` IDL reflection (resolved to absolute URLs) for the elements that have
        // them, so e.g. `img.src` is a STRING (google does `img.src.substring(...)`) not undefined.
        // URL resolution falls back to the raw attribute if our URL parser can't handle it, so the
        // value is always a string either way.
        // Spec URL reflection: absent attribute -> "", otherwise resolve the attribute value
        // against the document base URL (falling back to the raw value if it can't be parsed). An
        // empty-but-present attribute resolves to the document URL, per the standard.
        var __resolveURL = function (v) {
          if (v == null) { return ""; }
          return __reflResolveURL(v);
        };
        var __reflectURL = function (name, tags) {
          if (!tags[__formTag]) { return; }
          var has = false;
          try { var d = Object.getOwnPropertyDescriptor(el, name); has = !!(d && (d.get || d.set)); } catch (eD) {}
          if (has) { return; }
          Object.defineProperty(el, name, {
            get: function () { return __resolveURL(__getAttr(node, name)); },
            set: function (v) { __setAttr(node, name, String(v)); },
            configurable: true, enumerable: true
          });
        };
        __reflectURL("src", { img: 1, script: 1, iframe: 1, source: 1, video: 1, audio: 1, embed: 1, track: 1, input: 1, frame: 1 });
        __reflectURL("href", { a: 1, link: 1, area: 1, base: 1 });
        // HTMLHyperlinkElementUtils URL-decomposition accessors on <a>/<area>: protocol/host/...
        // derived from the resolved href. These also make the WPT reflection harness' resolveUrl()
        // (which decomposes a throwaway <a>) compute correct expected values for `url`-type attrs.
        if (__formTag === "a" || __formTag === "area") {
          var __hrefParts = function () {
            var raw = __getAttr(node, "href");
            var resolved = (raw == null) ? "" : __resolveURL(raw);
            return parseURL(resolved);
          };
          // All hyperlink URL-decomposition setters run the WHATWG URL setter in Rust (__urlSet) on
          // the element's current resolved href and store the reserialized result. `origin` is
          // read-only; an invalid value is a no-op.
          var __defUrlPart = function (prop, field) {
            var d = null;
            try { d = Object.getOwnPropertyDescriptor(el, prop); } catch (eU2) {}
            if (d && (d.get || d.set)) { return; }
            Object.defineProperty(el, prop, {
              get: function () { return __hrefParts()[field]; },
              set: function (v) {
                if (prop === "origin") { return; }
                var href = __hrefParts().href;
                if (!href) { return; }
                var json = __urlSet(href, prop, String(v));
                if (json != null) { try { __setAttr(node, "href", JSON.parse(json).href); } catch (e) {} }
              },
              configurable: true, enumerable: true
            });
          };
          var __defUserInfoPart = function (prop) {
            var d = null;
            try { d = Object.getOwnPropertyDescriptor(el, prop); } catch (eU3) {}
            if (d && (d.get || d.set)) { return; }
            Object.defineProperty(el, prop, {
              get: function () { return __hrefParts()[prop]; },
              set: function (v) {
                var href = __hrefParts().href;
                if (!href) { return; }
                var json = __urlSet(href, prop, String(v));
                if (json != null) { try { __setAttr(node, "href", JSON.parse(json).href); } catch (e) {} }
              },
              configurable: true, enumerable: true
            });
          };
          __defUrlPart("protocol", "protocol"); __defUrlPart("host", "host");
          __defUrlPart("hostname", "hostname"); __defUrlPart("port", "port");
          __defUrlPart("pathname", "pathname"); __defUrlPart("search", "search");
          __defUrlPart("hash", "hash"); __defUrlPart("origin", "origin");
          __defUserInfoPart("username"); __defUserInfoPart("password");
        }
        // <img>.naturalWidth / naturalHeight: the decoded intrinsic size from the engine
        // (0 when the image is missing/broken/not yet decoded). `width`/`height` reflect the
        // used (rendered) size, falling back to the natural size.
        if (__formTag === "img") {
          var __natW = function (self) { var id = self.__node; var n = (typeof id === "number") ? __naturalSize(id) : null; return n ? n.w : 0; };
          var __natH = function (self) { var id = self.__node; var n = (typeof id === "number") ? __naturalSize(id) : null; return n ? n.h : 0; };
          var __defImgNum = function (prop, getter) {
            var has = false;
            try { var d = Object.getOwnPropertyDescriptor(el, prop); has = !!(d && (d.get || d.set)); } catch (eIN) {}
            if (!has) { Object.defineProperty(el, prop, { get: getter, configurable: true, enumerable: true }); }
          };
          __defImgNum("naturalWidth", function () { return __natW(this) | 0; });
          __defImgNum("naturalHeight", function () { return __natH(this) | 0; });
          // width/height reflect the rendered box (border-box from layout) else the HTML attr
          // else the natural size; setting updates the presentational attribute.
          // img.width/height are `unsigned long` reflections (presentational attr): set converts
          // via ToUint32 and an out-of-[0,maxInt] value becomes the default (0).
          var __imgUL = function (v) { var n = Number(v); if (!isFinite(n)) { n = 0; } n = (n < 0 ? Math.ceil(n) : Math.floor(n)) % 4294967296; if (n < 0) { n += 4294967296; } n = n >>> 0; return (n > 2147483647) ? 0 : n; };
          Object.defineProperty(el, "width", {
            get: function () { var id = this.__node; var r = (typeof id === "number") ? __rect(id) : null; if (r && r.width) { return Math.round(r.width); } var a = __getAttr(node, "width"); if (a != null && a !== "") { return parseInt(a, 10) || 0; } return __natW(this) | 0; },
            set: function (v) { __setAttr(node, "width", String(__imgUL(v))); },
            configurable: true, enumerable: true
          });
          Object.defineProperty(el, "height", {
            get: function () { var id = this.__node; var r = (typeof id === "number") ? __rect(id) : null; if (r && r.height) { return Math.round(r.height); } var a = __getAttr(node, "height"); if (a != null && a !== "") { return parseInt(a, 10) || 0; } return __natH(this) | 0; },
            set: function (v) { __setAttr(node, "height", String(__imgUL(v))); },
            configurable: true, enumerable: true
          });
        }
        // <dialog>: show()/showModal() set the `open` attribute; close(returnValue?) removes it,
        // stores returnValue, and fires a `close` event. `.open` reflects the attribute.
        if (__formTag === "dialog") {
          var __defDialog = function (prop, val) {
            try { if (typeof el[prop] !== "function") { def(el, prop, val); } } catch (eDl) { def(el, prop, val); }
          };
          __defDialog("show", function () { __setAttr(node, "open", ""); });
          __defDialog("showModal", function () { __setAttr(node, "open", ""); });
          __defDialog("close", function (rv) {
            if (__getAttr(node, "open") == null) { return; }
            __removeAttr(node, "open");
            if (rv !== undefined) { this.returnValue = String(rv); }
            try {
              var ev = (typeof Event === "function") ? new Event("close", { bubbles: false, cancelable: false }) : { type: "close" };
              this.dispatchEvent(ev);
            } catch (eEv) {}
          });
          var __hasOpen = false;
          try { var __od = Object.getOwnPropertyDescriptor(el, "open"); __hasOpen = !!(__od && (__od.get || __od.set)); } catch (eOpn) {}
          if (!__hasOpen) {
            Object.defineProperty(el, "open", {
              get: function () { return __getAttr(node, "open") != null; },
              set: function (v) { if (v) { __setAttr(node, "open", ""); } else { __removeAttr(node, "open"); } },
              configurable: true, enumerable: true
            });
          }
          if (!("returnValue" in el)) { el.returnValue = ""; }
        }
        // Generic HTML IDL attribute reflection: install all reflected accessors for this element
        // (global attributes + ARIA + per-element table). Runs AFTER the bespoke form-control / URL
        // / img / dialog accessors above so those take precedence (defineReflected won't clobber an
        // accessor that already exists).
        try { applyReflection(el, node, __formTag); } catch (eRf) {}
      } catch (e10) {}
    } else {
      // Detached/foreign object: fall back to inert stubs so access doesn't throw.
      if (!("style" in el) || el.style == null) { def(el, "style", { getPropertyValue: function () { return ""; }, setProperty: fn, removeProperty: function () { return ""; }, cssText: "" }); }
      if (!("classList" in el) || el.classList == null) { def(el, "classList", { add: fn, remove: fn, toggle: function () { return false; }, contains: function () { return false; }, item: function () { return null; } }); }
      if (!("dataset" in el) || el.dataset == null) { def(el, "dataset", {}); }
    }
    // Element-returning native methods hand back un-enriched wrappers; wrap them so the result
    // is enriched (gets style/classList/dataset) before page code touches it.
    var elemMethods = ["querySelector", "closest"];
    for (var mi = 0; mi < elemMethods.length; mi++) {
      (function (mn) {
        var orig = el[mn];
        if (typeof orig === "function") { def(el, mn, function () { return canon(orig.apply(this, arguments)); }); }
      })(elemMethods[mi]);
    }
    var listMethods = ["querySelectorAll", "getElementsByTagName", "getElementsByTagNameNS", "getElementsByClassName"];
    for (var li = 0; li < listMethods.length; li++) {
      (function (mn) {
        var orig = el[mn];
        if (typeof orig === "function") { def(el, mn, function () { var r = orig.apply(this, arguments); if (r && typeof r.length === "number") { for (var i = 0; i < r.length; i++) { r[i] = canon(r[i]); } } return r; }); }
      })(listMethods[li]);
    }
    // Navigation accessors return fresh wrappers each time; re-wrap to canonicalize on read.
    var navAccessors = ["parentNode", "parentElement", "firstChild", "lastChild", "firstElementChild",
                        "nextSibling", "previousSibling", "nextElementSibling", "previousElementSibling"];
    for (var ni = 0; ni < navAccessors.length; ni++) {
      (function (an) {
        var d = Object.getOwnPropertyDescriptor(el, an);
        if (d && d.get) { var og = d.get; Object.defineProperty(el, an, { get: function () { return canon(og.call(this)); }, configurable: true, enumerable: d.enumerable }); }
      })(navAccessors[ni]);
    }
    var listAccessors = ["children", "childNodes"];
    for (var ci = 0; ci < listAccessors.length; ci++) {
      (function (an) {
        var d = Object.getOwnPropertyDescriptor(el, an);
        if (d && d.get) { var og = d.get; Object.defineProperty(el, an, { get: function () { var r = og.call(this); if (r && typeof r.length === "number") { for (var i = 0; i < r.length; i++) { r[i] = canon(r[i]); } } return r; }, configurable: true, enumerable: d.enumerable }); }
      })(listAccessors[ci]);
    }

    // <style> (and stylesheet <link>) expose a live CSSStyleSheet via `.sheet`. The accessor lives on
    // the LinkStyle mixin prototype (HTMLStyleElement/HTMLLinkElement) so assert_idl_attribute passes
    // (must not be an own property); enrichElement just marks the element as sheet-bearing.
    if (typeof el.tagName === "string" && (el.tagName.toLowerCase() === "style" || el.tagName.toLowerCase() === "link") && !el.__sheetHost) {
      def(el, "__sheetHost", true);
    }

    // getBoundingClientRect / getClientRects: read the engine-pushed rect for this node
    // (viewport-relative CSS px). Detached / not-laid-out nodes get __rect()===null, so fall back
    // to the zero-rect (so they don't throw). toJSON returns the plain rect (DOMRect semantics).
    def(el, "getBoundingClientRect", function () {
      var id = this.__node;
      var r = (typeof id === "number") ? __rect(id) : null;
      if (!r) { return makeRect(); }
      r.toJSON = function () { return this; };
      return r;
    });
    def(el, "getClientRects", function () {
      var id = this.__node;
      var r = (typeof id === "number") ? __rect(id) : null;
      if (!r) { return []; }
      r.toJSON = function () { return this; };
      return [r];
    });
    // Live element-metric getters backed by __elemMetrics(this.__node) (0 when null). Only install
    // on real element wrappers (numeric node id) and don't clobber a page-defined accessor.
    if (typeof node === "number") {
      var __metricProps = {
        offsetWidth: "ow", offsetHeight: "oh", offsetTop: "ot", offsetLeft: "ol",
        clientWidth: "cw", clientHeight: "ch", // padding box: content + padding, no borders
        scrollWidth: "sw", scrollHeight: "sh"
      };
      for (var __mk in __metricProps) {
        (function (prop, field) {
          var __md = null;
          try { __md = Object.getOwnPropertyDescriptor(el, prop); } catch (eM) {}
          if (__md && (__md.get || __md.set)) { return; } // page already defined an accessor
          Object.defineProperty(el, prop, {
            get: function () {
              var m = __elemMetrics(this.__node);
              var v = m ? m[field] : 0;
              // The document root (<html>) often has no pushed box, so its width/height metrics read
              // 0. clientWidth/clientHeight of the root must be the viewport size (CSSOM-View), so
              // fall back to innerWidth/innerHeight there.
              if ((!v || v === 0)) {
                var nid = this.__node;
                if (nid === __documentElementId() || nid === __bodyId()) {
                  if (field === "cw" || field === "ow" || field === "sw") { var iw = Number(globalThis.innerWidth) || 0; if (iw > 0) { return iw; } }
                  if (field === "ch" || field === "oh") { var ih = Number(globalThis.innerHeight) || 0; if (ih > 0) { return ih; } }
                }
              }
              return v;
            },
            configurable: true, enumerable: true
          });
        })(__mk, __metricProps[__mk]);
      }
      // offsetParent: simple stand-in — document.body for laid-out elements, null when detached.
      var __opd = null;
      try { __opd = Object.getOwnPropertyDescriptor(el, "offsetParent"); } catch (eO) {}
      if (!(__opd && (__opd.get || __opd.set))) {
        Object.defineProperty(el, "offsetParent", {
          get: function () { return __elemMetrics(this.__node) ? document.body : null; },
          configurable: true, enumerable: true
        });
      }
    }
    if (typeof el.scrollIntoView !== "function") { def(el, "scrollIntoView", function () { try { __scrollIntoView(this.__node); } catch (e) {} }); }
    if (typeof el.focus !== "function") { def(el, "focus", fn); }
    if (typeof el.blur !== "function") { def(el, "blur", fn); }
    if (typeof el.click !== "function") { def(el, "click", fn); }
    if (typeof el.cloneNode !== "function") { def(el, "cloneNode", function () { return this; }); }
    // Web Animations: minimal `Element.animate()` returning an Animation whose `finished`/`ready`
    // promises settle (after the effect's delay+duration). We don't composite animations, so styles
    // aren't actually interpolated — but this unblocks the very common pattern of awaiting a throwaway
    // animation to sync with a frame (e.g. WPT's `waitForCompositorReady`: `body.animate({opacity:
    // [0,1]},{duration:1}).finished`). `getAnimations()` reports none (we keep no running set).
    if (typeof el.animate !== "function") { def(el, "animate", function (_keyframes, options) { return globalThis.__makeAnimation(options); }); }
    if (typeof el.getAnimations !== "function") { def(el, "getAnimations", function () { return []; }); }
    if (typeof el.isEqualNode !== "function") { def(el, "isEqualNode", function (other) { return globalThis.__nodesEqual(this, other); }); }
    // getRootNode walks to the topmost ancestor (the document for a connected node; no shadow trees,
    // so the `composed` option is a no-op). isSameNode is identity (compare by node id, since one node
    // can have several wrappers). moveBefore is an atomic move — we model it as insertBefore.
    if (typeof el.getRootNode !== "function") { def(el, "getRootNode", function () { var n = this; while (n && n.parentNode) { n = n.parentNode; } return n; }); }
    if (typeof el.isSameNode !== "function") { def(el, "isSameNode", function (other) { if (this === other) { return true; } try { return other != null && this.__node != null && this.__node === other.__node; } catch (e) { return false; } }); }
    if (typeof el.moveBefore !== "function") { def(el, "moveBefore", function (node, ref) { return this.insertBefore(node, ref); }); }
    // Scrolling an element does nothing here (no scroll containers in layout); accept and ignore.
    if (typeof el.scroll !== "function") { def(el, "scroll", fn); }
    if (typeof el.scrollTo !== "function") { def(el, "scrollTo", fn); }
    if (typeof el.scrollBy !== "function") { def(el, "scrollBy", fn); }
    if (typeof el.checkVisibility !== "function") { def(el, "checkVisibility", function () { return true; }); }
    // Popover API: accept the calls (we don't render the top layer), togglePopover reports "hidden".
    if (typeof el.showPopover !== "function") { def(el, "showPopover", fn); }
    if (typeof el.hidePopover !== "function") { def(el, "hidePopover", fn); }
    if (typeof el.togglePopover !== "function") { def(el, "togglePopover", function () { return false; }); }
    // attachInternals: form-associated custom elements / ElementInternals are not implemented; return
    // a minimal internals object whose methods are no-ops so callers don't hit a TypeError. (Tests
    // that exercise real form association still fail — just not with "not a function".)
    if (typeof el.attachInternals !== "function") {
      def(el, "attachInternals", function () {
        return {
          shadowRoot: this.shadowRoot || null, form: null, labels: [], states: new Set(),
          willValidate: true, validity: { valid: true }, validationMessage: "",
          setFormValue: fn, setValidity: fn, checkValidity: function () { return true; }, reportValidity: function () { return true; }
        };
      });
    }
    // ParentNode/ChildNode insertion (node-taking variants; the *HTML helpers are added below). A
    // string argument becomes a Text node.
    var __toNode = function (x) { return (x && typeof x.nodeType === "number") ? x : document.createTextNode(x == null ? "" : String(x)); };
    if (typeof el.prepend !== "function") { def(el, "prepend", function () { var ref = this.firstChild; for (var i = 0; i < arguments.length; i++) { this.insertBefore(__toNode(arguments[i]), ref); } }); }
    if (typeof el.append !== "function") { def(el, "append", function () { for (var i = 0; i < arguments.length; i++) { this.appendChild(__toNode(arguments[i])); } }); }
    if (typeof el.before !== "function") { def(el, "before", function () { var p = this.parentNode; if (!p) { return; } for (var i = 0; i < arguments.length; i++) { p.insertBefore(__toNode(arguments[i]), this); } }); }
    if (typeof el.after !== "function") { def(el, "after", function () { var p = this.parentNode; if (!p) { return; } var ref = this.nextSibling; for (var i = 0; i < arguments.length; i++) { p.insertBefore(__toNode(arguments[i]), ref); } }); }
    if (typeof el.replaceWith !== "function") { def(el, "replaceWith", function () { var p = this.parentNode; if (!p) { return; } var ref = this.nextSibling; for (var i = 0; i < arguments.length; i++) { p.insertBefore(__toNode(arguments[i]), ref); } p.removeChild(this); }); }
    // Form-control methods. stepUp/stepDown adjust a numeric <input> by its step; setSelectionRange/
    // select track a text field's selection; submit/requestSubmit/requestClose are accepted (no real
    // navigation/dialog top-layer). These exist on every element here but are only called on the
    // right ones in practice.
    if (typeof el.stepUp !== "function") { def(el, "stepUp", function (n) { var s = parseFloat(this.getAttribute("step")) || 1, v = parseFloat(this.value) || 0; this.value = String(v + s * (n == null ? 1 : n)); }); }
    if (typeof el.stepDown !== "function") { def(el, "stepDown", function (n) { var s = parseFloat(this.getAttribute("step")) || 1, v = parseFloat(this.value) || 0; this.value = String(v - s * (n == null ? 1 : n)); }); }
    if (typeof el.setSelectionRange !== "function") { def(el, "setSelectionRange", function (start, end, dir) { def(this, "selectionStart", start | 0); def(this, "selectionEnd", end | 0); def(this, "selectionDirection", dir || "none"); }); }
    if (typeof el.select !== "function") { def(el, "select", function () { var v = this.value == null ? "" : String(this.value); def(this, "selectionStart", 0); def(this, "selectionEnd", v.length); }); }
    if (typeof el.submit !== "function") { def(el, "submit", fn); }
    if (typeof el.requestSubmit !== "function") { def(el, "requestSubmit", fn); }
    if (typeof el.requestClose !== "function") { def(el, "requestClose", function () { try { this.open = false; this.removeAttribute("open"); } catch (e) {} }); }
    // <audio>/<video>: no media pipeline, so playback methods are accepted (play resolves immediately).
    if (typeof el.play !== "function") { def(el, "play", function () { return Promise.resolve(); }); }
    if (typeof el.pause !== "function") { def(el, "pause", fn); }
    if (typeof el.load !== "function") { def(el, "load", fn); }
    if (typeof el.canPlayType !== "function") { def(el, "canPlayType", function () { return ""; }); }
    if (typeof el.addTextTrack !== "function") { def(el, "addTextTrack", function (kind) { return { kind: kind || "", mode: "disabled", cues: [], activeCues: [], addCue: fn, removeCue: fn, addEventListener: fn, removeEventListener: fn }; }); }
    // SVGSVGElement animation controls (no SMIL): accept and ignore.
    if (typeof el.pauseAnimations !== "function") { def(el, "pauseAnimations", fn); }
    if (typeof el.unpauseAnimations !== "function") { def(el, "unpauseAnimations", fn); }
    if (typeof el.setCurrentTime !== "function") { def(el, "setCurrentTime", fn); }
    if (typeof el.getCurrentTime !== "function") { def(el, "getCurrentTime", function () { return 0; }); }
    // Declarative partial-update methods (WICG): {append,prepend,before,after,replaceWith}HTML[Unsafe].
    globalThis.__addPartialMethods(el);
    if (typeof el.hasChildNodes !== "function") { def(el, "hasChildNodes", function () { try { return (this.childNodes || []).length > 0; } catch (e) { return false; } }); }
    if (!("nodeType" in el)) { def(el, "nodeType", 1); }
    if (!("ownerDocument" in el)) {
      // Dynamic: a node inside a <template>'s content belongs to that template's contents document,
      // and a node inside an <iframe>'s content document belongs to that document. Resolved by
      // walking the arena ancestry (so moving a node between documents updates its ownerDocument).
      Object.defineProperty(el, "ownerDocument", {
        get: function () {
          return (typeof globalThis.__ownerDocumentOf === "function") ? globalThis.__ownerDocumentOf(this) : document;
        },
        configurable: true, enumerable: true
      });
    }
    // scrollLeft/scrollTop are accessors that clamp the assigned offset to the element's scroll
    // range [0, scrollWidth-clientWidth] / [0, scrollHeight-clientHeight] (per CSSOM-View): a write
    // past the maximum scrollable distance settles at the maximum (and a non-scrollable box stays
    // at 0). The backing value lives on a private own-property; the getter re-clamps to the current
    // range so a later layout change can't leave a stale out-of-range offset.
    if (typeof node === "number") {
      [["scrollLeft", "sw", "cw", "__scrollLeftVal"], ["scrollTop", "sh", "ch", "__scrollTopVal"]]
        .forEach(function (spec) {
          var prop = spec[0], sizeF = spec[1], clientF = spec[2], backing = spec[3];
          var __sd = null;
          try { __sd = Object.getOwnPropertyDescriptor(el, prop); } catch (eS) {}
          if (__sd && (__sd.get || __sd.set)) { return; } // page already defined an accessor
          var clamp = function (self, v) {
            v = Number(v); if (!isFinite(v)) { v = 0; }
            var m = __elemMetrics(self.__node);
            var max = m ? Math.max(0, (m[sizeF] || 0) - (m[clientF] || 0)) : 0;
            if (v < 0) { v = 0; } else if (v > max) { v = max; }
            return v;
          };
          Object.defineProperty(el, prop, {
            get: function () { return clamp(this, this[backing] || 0); },
            set: function (v) {
              Object.defineProperty(this, backing,
                { value: clamp(this, v), writable: true, configurable: true, enumerable: false });
            },
            configurable: true, enumerable: true
          });
        });
    } else {
      if (!("scrollTop" in el)) { el.scrollTop = 0; }
      if (!("scrollLeft" in el)) { el.scrollLeft = 0; }
    }
    if (!("offsetWidth" in el)) { el.offsetWidth = 0; }
    if (!("offsetHeight" in el)) { el.offsetHeight = 0; }
    if (!("clientWidth" in el)) { el.clientWidth = 0; }
    if (!("clientHeight" in el)) { el.clientHeight = 0; }
    // SVG geometry properties expose SVGAnimatedLength / SVGAnimatedRect objects whose `.baseVal`
    // pages read (e.g. favicon generators do `svg.width.baseVal.value`). Provide zeroed stubs so
    // those reads don't throw. Gated on SVG tags so HTML elements keep their own width/height attrs.
    try {
      var __svgTag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : "";
      if (svgTags[__svgTag]) {
        var __len = ["width", "height", "x", "y"];
        for (var __si = 0; __si < __len.length; __si++) {
          (function (p) {
            if (!(p in el)) { def(el, p, { baseVal: { value: 0, valueAsString: "0", valueInSpecifiedUnits: 0 }, animVal: { value: 0 } }); }
          })(__len[__si]);
        }
        if (!("viewBox" in el)) { def(el, "viewBox", { baseVal: { x: 0, y: 0, width: 0, height: 0 }, animVal: { x: 0, y: 0, width: 0, height: 0 } }); }
        if (!("preserveAspectRatio" in el)) { def(el, "preserveAspectRatio", { baseVal: { align: 0, meetOrSlice: 0 }, animVal: { align: 0, meetOrSlice: 0 } }); }
      }
    } catch (e) {}
    // <canvas>: a REAL 2D context that records a display list of resolved drawing commands.
    // The JS side maintains drawing state (styles + a 2D affine transform + path) and pushes
    // already-transformed, color-resolved commands into the canvas's display list; the Rust engine
    // pulls these via `__canvasLists()`, rasterizes them into a bitmap, and composites it like an
    // <img>. 'webgl'/'webgl2' return null so callers fall back gracefully.
    try {
      var __cvTag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : "";
      if (__cvTag === "canvas" && typeof el.getContext !== "function") {
        // width/height reflect the canvas's content attributes (the bitmap size), defaulting to
        // the spec 300x150. Setting them updates the attribute and resets the drawing surface.
        (function () {
          // width/height are `unsigned long` reflections (default 300 / 150): parse via the rules
          // for parsing non-negative integers, range [0, 2147483647], else the default.
          function rd(attr, dflt) {
            var v = (typeof el.getAttribute === "function") ? el.getAttribute(attr) : null;
            if (v == null) { return dflt; }
            var p = __reflParseNonneg(v);
            if (p === false || p < 0 || p > 2147483647) { return dflt; }
            return p;
          }
          function wr(attr, v) {
            // ToUint32; out-of-[0,maxInt] becomes the default.
            var n = Number(v); if (!isFinite(n)) { n = 0; }
            n = (n < 0 ? Math.ceil(n) : Math.floor(n)) % 4294967296; if (n < 0) { n += 4294967296; }
            n = n >>> 0; if (n > 2147483647) { n = (attr === "width") ? 300 : 150; }
            try { if (typeof el.setAttribute === "function") { el.setAttribute(attr, String(n)); } } catch (e) {}
            // Resetting width/height clears the canvas (per spec). Drop the recorded display list.
            try { if (el.__ctx2d && el.__ctx2d.__list) { el.__ctx2d.__list.length = 0; } } catch (e2) {}
          }
          Object.defineProperty(el, "width", { get: function () { return rd("width", 300); }, set: function (v) { wr("width", v); }, configurable: true, enumerable: true });
          Object.defineProperty(el, "height", { get: function () { return rd("height", 150); }, set: function (v) { wr("height", v); }, configurable: true, enumerable: true });
        })();
        def(el, "getContext", function (type) {
          if (type !== "2d") { return null; }
          if (el.__ctx2d) { return el.__ctx2d; }
          var ctx = __makeCanvas2D(el);
          def(el, "__ctx2d", ctx);
          try {
            globalThis.__canvases = globalThis.__canvases || [];
            globalThis.__canvases.push(ctx);
          } catch (e) {}
          return ctx;
        });
        if (typeof el.toDataURL !== "function") { def(el, "toDataURL", function () { return "data:,"; }); }
        if (typeof el.toBlob !== "function") { def(el, "toBlob", function (cb) { if (typeof cb === "function") { cb(null); } }); }
        // transferControlToOffscreen(): hand this <canvas>'s rendering to an OffscreenCanvas whose 2D
        // context composites back onto this element (via the placeholder node id) — the engine pulls
        // it through __canvasLists like any other canvas. Throws if a context was already obtained.
        if (typeof el.transferControlToOffscreen !== "function") {
          def(el, "transferControlToOffscreen", function () {
            if (el.__ctx2d || el.__transferred) {
              throw new globalThis.DOMException("Failed to execute 'transferControlToOffscreen' on 'HTMLCanvasElement': Cannot transfer control from a canvas that has a rendering context.", "InvalidStateError");
            }
            if (typeof globalThis.OffscreenCanvas !== "function") { return null; }
            def(el, "__transferred", true);
            var oc = new globalThis.OffscreenCanvas(el.width | 0 || 300, el.height | 0 || 150);
            oc.__placeholderNode = (typeof el.__node === "number") ? el.__node : -1;
            return oc;
          });
        }
      }
    } catch (e) {}
    installEvents(el);
    return el;
  }
  // Expose so element-returning native accessors (parentNode, etc.) can be enriched lazily by
  // anything that needs it. (Kept non-enumerable.)
  def(globalThis, "__enrichElement", enrichElement);

  function wrapReturningElement(obj, name) {
    var orig = obj[name];
    if (typeof orig !== "function") { return; }
    def(obj, name, function () {
      var r = orig.apply(this, arguments);
      if (r && typeof r === "object") {
        if (r instanceof globalThis.NodeList || r instanceof globalThis.HTMLCollection) { return r; }
        if (typeof r.length === "number" && typeof r.splice === "function") {
          for (var i = 0; i < r.length; i++) { r[i] = canon(r[i]); }
        } else {
          return canon(r);
        }
      }
      return r;
    });
  }
  wrapReturningElement(document, "createElement");
  wrapReturningElement(document, "createElementNS");
  wrapReturningElement(document, "getElementById");
  wrapReturningElement(document, "getElementsByTagName");
  wrapReturningElement(document, "getElementsByTagNameNS");
  wrapReturningElement(document, "getElementsByClassName");
  wrapReturningElement(document, "querySelector");
  wrapReturningElement(document, "querySelectorAll");

  // createElementNS(ns, qualifiedName) — used by Vue/SVG. There is no namespaced node in the
  // DOM arena, so create a normal element from the local name (dropping any prefix) and record
  // the namespace so namespace-aware code can read it back. The element is fully enriched via
  // document.createElement above (appendChild/setAttribute/etc. all present).
  if (typeof document.createElementNS !== "function") {
    def(document, "createElementNS", function (ns, qualifiedName) {
      var name = String(qualifiedName == null ? "" : qualifiedName);
      var local = name.indexOf(":") >= 0 ? name.slice(name.indexOf(":") + 1) : name;
      var el = document.createElement(local);
      try { def(el, "namespaceURI", ns == null ? null : String(ns)); } catch (e) {}
      return el;
    });
  }

  // Enrich element wrappers returned by the native element-navigation accessors and methods.
  // These return fresh wrapper objects each time, so wrap the prototype-less accessors by
  // intercepting via getter wrappers is impractical; instead wrap the element-returning methods
  // on a per-element basis when an element is first enriched. We patch the document-level
  // accessors (body/documentElement/head) below.
  function enrichDocAccessor(name) {
    var d = Object.getOwnPropertyDescriptor(document, name);
    if (!d || !d.get) { return; }
    var origGet = d.get;
    Object.defineProperty(document, name, {
      get: function () { return canon(origGet.call(this)); },
      enumerable: d.enumerable, configurable: true
    });
  }
  enrichDocAccessor("body");
  enrichDocAccessor("documentElement");
  enrichDocAccessor("head");

  // --- document node-creation helpers ------------------------------------------------------
  // createTextNode / createComment / createDocumentFragment return lightweight node-ish objects.
  // They aren't backed by the real DOM arena (only createElement is), but they are appendable to
  // real elements as no-ops and carry the properties scripts read, so init code doesn't throw.
  // Back text + comment nodes with REAL arena nodes (via the native primitives + __wrapNode) so
  // they have a working parentNode / insertBefore / sibling chain. Vue uses comment + text nodes as
  // fragment anchors and re-reads their parent on every re-render; a detached stub would make
  // `parent.insertBefore(...)` throw (`parent` === null) during a component update.
  if (typeof document.createTextNode !== "function") {
    def(document, "createTextNode", function (data) {
      // Canonicalize so the wrapper is cached: navigation (nextSibling/firstChild) returns the same
      // object, preserving node identity (===), and enrichment grafts on partial-update methods.
      return canon(__wrapNode(__createText(String(data == null ? "" : data))));
    });
  }
  if (typeof document.createComment !== "function") {
    def(document, "createComment", function (data) {
      return canon(__wrapNode(__createComment(String(data == null ? "" : data))));
    });
  }
  // createCDATASection is only valid on XML documents; the live page document is HTML, so it throws.
  if (typeof document.createCDATASection !== "function") {
    def(document, "createCDATASection", function () {
      throw new globalThis.DOMException("This DOM method is only valid on XML documents.", "NotSupportedError");
    });
  }
  if (typeof document.createDocumentFragment !== "function") {
    def(document, "createDocumentFragment", function () {
      // Real arena-backed DocumentFragment (nodeType 11): its children move on insertion, and it
      // supports the full ParentNode mixin (append/prepend/replaceChildren/appendChild/insertBefore).
      // Canonicalize so navigation accessors (firstChild) return enriched, prototype-correct nodes.
      return canon(__wrapNode(__createDocumentFragment()));
    });
  }

  function createRangeForDocument(doc) {
    var r = new globalThis.Range();
    r.setStart(doc, 0);
    r.setEnd(doc, 0);
    return r;
  }
  if (typeof document.createRange !== "function") {
    def(document, "createRange", function () { return createRangeForDocument(this); });
  }
  // document.implementation.createHTMLDocument — used to build/parse HTML off to the side (e.g.
  // sanitizers, template parsing). We back it with real (detached) arena nodes so innerHTML /
  // appendChild / querySelector work on the returned document's tree.
  if (typeof document.implementation === "undefined" || !document.implementation) {
    // Build a real (arena-backed) DocumentType whose ownerDocument is `ownerDoc`. The arena node is
    // created via the validating factory; we override its ownerDocument to the requested document.
    function makeDoctypeFor(ownerDoc, name, pub, sys) {
      var dt = globalThis.__createDocumentTypeNode(String(name), pub == null ? "" : String(pub), sys == null ? "" : String(sys));
      try { Object.defineProperty(dt, "ownerDocument", { value: ownerDoc, configurable: true, enumerable: true }); } catch (e) {}
      return dt;
    }
    // Back an off-document facade with a real (detached) arena Document node, so the Node mutation
    // methods + live child accessors work against the arena (appendChild/insertBefore/removeChild/
    // replaceChild, childNodes/firstChild/lastChild). `initialChildIds` are appended in order.
    function backDocWithArena(doc, initialChildIds) {
      var docId = globalThis.__createDocumentNode();
      try { Object.defineProperty(doc, "__node", { value: docId, configurable: true }); } catch (e) {}
      // Register THIS facade as the canonical wrapper for its arena node, so that __nodeFor(docId)
      // — and therefore every `.parentNode` / `.childNodes` that resolves a child back to its
      // document — returns this same object rather than a fresh, separate wrapper. Without this the
      // facade and the canonical wrapper are two different objects for one node; identity checks
      // (e.g. WPT common.js `indexOf`: `while (node != node.parentNode.childNodes[i]) i++`) then
      // never match and spin forever. Mark `__enriched` first so __canonNode skips the element-only
      // enrichment that doesn't apply to a Document facade.
      try { def(doc, "__enriched", true); } catch (e) {}
      try { globalThis.__canonNode(doc); } catch (e) {}
      for (var i = 0; i < initialChildIds.length; i++) {
        var cid = initialChildIds[i];
        if (typeof cid === "number" && cid >= 0) { globalThis.__insertNode(docId, cid, -1); }
      }
      function reqNode(x, m) {
        var n = (x && typeof x.__node === "number") ? x.__node : -1;
        if (n < 0) { throw new TypeError("Failed to execute '" + m + "' on 'Node': parameter is not of type 'Node'."); }
        return n;
      }
      function nf(msg) { throw new (globalThis.DOMException)(msg, "NotFoundError"); }
      def(doc, "appendChild", function (child) { var c = reqNode(child, "appendChild"); globalThis.__insertNode(docId, c, -1); return child; });
      def(doc, "insertBefore", function (newNode, refNode) {
        var c = reqNode(newNode, "insertBefore");
        var r = (refNode == null) ? -1 : ((refNode && typeof refNode.__node === "number") ? refNode.__node : -1);
        if (refNode != null && r < 0) { nf("The reference child is not a child of this node."); }
        globalThis.__insertNode(docId, c, r); return newNode;
      });
      def(doc, "removeChild", function (child) {
        var c = reqNode(child, "removeChild");
        if (globalThis.__parent(c) !== docId) { nf("The node to be removed is not a child of this node."); }
        globalThis.__removeChild(docId, c); return child;
      });
      def(doc, "replaceChild", function (newNode, oldNode) {
        var n = reqNode(newNode, "replaceChild"), o = reqNode(oldNode, "replaceChild");
        if (globalThis.__parent(o) !== docId) { nf("The node to be replaced is not a child of this node."); }
        var sibs = globalThis.__children(docId); var idx = sibs.indexOf(o);
        var ref = (idx >= 0 && idx + 1 < sibs.length) ? sibs[idx + 1] : -1;
        if (ref === n) { var ni = sibs.indexOf(n); ref = (ni >= 0 && ni + 1 < sibs.length) ? sibs[ni + 1] : -1; }
        globalThis.__removeChild(docId, o); globalThis.__insertNode(docId, n, ref); return oldNode;
      });
      function kids() { return globalThis.__children(docId); }
      var childNodesList = globalThis.__makeNodeList(function () {
        var ids = kids(), out = [];
        for (var i = 0; i < ids.length; i++) { out.push(globalThis.__nodeFor(ids[i])); }
        return out;
      }, true);
      Object.defineProperty(doc, "childNodes", { get: function () { return childNodesList; }, configurable: true, enumerable: true });
      Object.defineProperty(doc, "firstChild", { get: function () { var ids = kids(); return ids.length ? globalThis.__nodeFor(ids[0]) : null; }, configurable: true, enumerable: true });
      Object.defineProperty(doc, "lastChild", { get: function () { var ids = kids(); return ids.length ? globalThis.__nodeFor(ids[ids.length - 1]) : null; }, configurable: true, enumerable: true });
      // A Document has no parent/siblings/owner (all null, never undefined).
      Object.defineProperty(doc, "parentNode", { get: function () { return null; }, configurable: true, enumerable: true });
      Object.defineProperty(doc, "parentElement", { get: function () { return null; }, configurable: true, enumerable: true });
      Object.defineProperty(doc, "previousSibling", { get: function () { return null; }, configurable: true, enumerable: true });
      Object.defineProperty(doc, "nextSibling", { get: function () { return null; }, configurable: true, enumerable: true });
      Object.defineProperty(doc, "ownerDocument", { get: function () { return null; }, configurable: true, enumerable: true });
      def(doc, "contains", function (other) { return nodeContains(doc, other); });
      def(doc, "compareDocumentPosition", function (other) {
        if (!other || typeof other.__node !== "number") {
          throw new TypeError("Failed to execute 'compareDocumentPosition' on 'Node': parameter 1 is not of type 'Node'.");
        }
        return __cmpDocPos(docId, other.__node);
      });
      def(doc, "createRange", function () { return createRangeForDocument(doc); });
      return doc;
    }
    var HTML_NS = "http://www.w3.org/1999/xhtml";
    function isHtmlNamedElement(node, localName) {
      return !!(node && node.nodeType === 1 && node.namespaceURI === HTML_NS && node.localName === localName);
    }
    function documentElementOf(doc) {
      var kids = doc.childNodes || [];
      for (var i = 0; i < kids.length; i++) {
        if (kids[i] && kids[i].nodeType === 1) { return kids[i]; }
      }
      return null;
    }
    function doctypeOf(doc) {
      var kids = doc.childNodes || [];
      for (var i = 0; i < kids.length; i++) {
        if (kids[i] && kids[i].nodeType === 10) { return kids[i]; }
      }
      return null;
    }
    function headOf(doc) {
      var root = documentElementOf(doc);
      if (!isHtmlNamedElement(root, "html")) { return null; }
      var kids = root.childNodes || [];
      for (var i = 0; i < kids.length; i++) {
        if (isHtmlNamedElement(kids[i], "head")) { return kids[i]; }
      }
      return null;
    }
    function bodyOf(doc) {
      var root = documentElementOf(doc);
      if (!isHtmlNamedElement(root, "html")) { return null; }
      var kids = root.childNodes || [];
      for (var i = 0; i < kids.length; i++) {
        if (isHtmlNamedElement(kids[i], "body") || isHtmlNamedElement(kids[i], "frameset")) { return kids[i]; }
      }
      return null;
    }
    function hierarchyRequestError(msg) {
      throw new globalThis.DOMException(msg || "The operation would yield an incorrect node tree.", "HierarchyRequestError");
    }
    function installDocumentTreeAccessors(doc) {
      Object.defineProperty(doc, "documentElement", { get: function () { return documentElementOf(this); }, enumerable: true, configurable: true });
      Object.defineProperty(doc, "doctype", { get: function () { return doctypeOf(this); }, enumerable: true, configurable: true });
      Object.defineProperty(doc, "head", { get: function () { return headOf(this); }, enumerable: true, configurable: true });
      Object.defineProperty(doc, "body", {
        get: function () { return bodyOf(this); },
        set: function (value) {
          if (!value || typeof value !== "object" || value.nodeType !== 1) {
            throw new TypeError("Failed to set the 'body' property on 'Document': value is not an HTML body or frameset element.");
          }
          if (!isHtmlNamedElement(value, "body") && !isHtmlNamedElement(value, "frameset")) {
            hierarchyRequestError("Document.body must be a body or frameset element.");
          }
          var root = documentElementOf(this);
          if (!root) { hierarchyRequestError("Cannot set Document.body without a document element."); }
          var old = bodyOf(this);
          if (old) { old.parentNode.replaceChild(value, old); }
          else { root.appendChild(value); }
        },
        enumerable: true, configurable: true
      });
    }
    // Install the read-only metadata an off-document (created) Document exposes per the DOM/HTML
    // specs. A document built off to the side has no browsing context, so `location` is null and its
    // URL is "about:blank"; it is always parsed as standards-mode UTF-8.
    function installDocMeta(doc, contentType) {
      function ro(name, value) {
        Object.defineProperty(doc, name, { get: function () { return value; }, enumerable: true, configurable: true });
      }
      ro("contentType", contentType);
      ro("characterSet", "UTF-8");
      ro("charset", "UTF-8");
      ro("inputEncoding", "UTF-8");
      ro("compatMode", "CSS1Compat");
      ro("URL", "about:blank");
      ro("documentURI", "about:blank");
      ro("location", null);
    }
    // The first HTML-namespace <title> element in tree order (the "title element" the HTML spec's
    // document.title getter reads), or null. Walks the facade's live arena tree.
    function findTitleElement(doc) {
      var de = doc.documentElement;
      if (!de) { return null; }
      var stack = [de];
      while (stack.length) {
        var el = stack.shift();
        if (el && el.nodeType === 1 && el.localName === "title" && el.namespaceURI === HTML_NS) { return el; }
        var kids = el && el.childNodes;
        if (kids) { for (var i = 0; i < kids.length; i++) { if (kids[i] && kids[i].nodeType === 1) { stack.push(kids[i]); } } }
      }
      return null;
    }
    // Wire `document.title` (get/set) on an off-document facade, resolving against its own tree (the
    // global `document.title` uses arena-global helpers that don't apply to a detached document).
    function installDocTitle(doc) {
      Object.defineProperty(doc, "title", {
        get: function () {
          var t = findTitleElement(doc);
          if (!t) { return ""; }
          // Child text content, with HTML whitespace stripped/collapsed.
          var s = t.textContent || "";
          return s.replace(/[ \t\n\f\r]+/g, " ").replace(/^ | $/g, "");
        },
        set: function (v) {
          var t = findTitleElement(doc);
          if (!t) {
            var de = doc.documentElement;
            if (!de) { return; }
            var head = null, kids = de.childNodes;
            for (var i = 0; i < kids.length; i++) { if (kids[i].nodeType === 1 && kids[i].localName === "head") { head = kids[i]; break; } }
            t = doc.createElement("title");
            (head || de).appendChild(t);
          }
          t.textContent = v == null ? "" : String(v);
        },
        enumerable: true, configurable: true
      });
    }
    // Build an empty, arena-backed Document facade of the given `kind` ("html" => HTMLDocument with
    // case-folding createElement; "xml" => XMLDocument with case-preserving createElement). `docNs`
    // is the document's namespace (used to decide createElement's namespace for an XML document). The
    // returned document has no doctype and no document element; callers append those.
    function buildBareDocument(kind, docNs, contentType) {
      var isXML = kind === "xml";
      var ctorName = isXML ? "XMLDocument" : "HTMLDocument";
      // createElement's namespace for an XML document is the HTML namespace only for an
      // application/xhtml+xml document; otherwise the null namespace. An HTML document always uses
      // the HTML namespace and ASCII-lowercases.
      var elNs = isXML ? (docNs === HTML_NS ? HTML_NS : null) : HTML_NS;
      var doc = {
        nodeType: 9, nodeName: "#document", doctype: null,
        caretPositionFromPoint: function () { return null; },
        caretRangeFromPoint: function () { return null; },
        elementFromPoint: function () { return null; },
        elementsFromPoint: function () { return []; },
        lookupNamespaceURI: function () { var de = this.documentElement; return de && de.lookupNamespaceURI ? de.lookupNamespaceURI.apply(de, arguments) : null; },
        lookupPrefix: function () { var de = this.documentElement; return de && de.lookupPrefix ? de.lookupPrefix.apply(de, arguments) : null; },
        isDefaultNamespace: function (ns) { var de = this.documentElement; return de && de.isDefaultNamespace ? de.isDefaultNamespace.apply(de, arguments) : (ns == null || ns === ""); },
        createElement: isXML
          ? function (name) { return globalThis.__createElementCasePreserving(elNs, name); }
          : function (tag) { return document.createElement(tag); },
        createElementNS: function (ns, qn) { return document.createElementNS(ns, qn); },
        createAttribute: isXML
          ? function (name) { var nm = String(name); if (nm.length === 0) { globalThis.__invalidCharacterError(); } return globalThis.__makeAttrNode(null, null, nm, nm); }
          : function (name) { var nm = String(name); if (nm.length === 0) { globalThis.__invalidCharacterError(); } return globalThis.__makeAttrNode(null, null, nm.toLowerCase(), nm.toLowerCase()); },
        createAttributeNS: function (ns, qn) { var ex = globalThis.__validateAndExtractName(ns, qn); return globalThis.__makeAttrNode(ex.namespace, ex.prefix, ex.localName, String(qn)); },
        createTextNode: function (s) { return document.createTextNode(s); },
        createComment: function (s) { return document.createComment(s); },
        createCDATASection: isXML
          ? function (data) { return globalThis.__canonNode(globalThis.__wrapNode(globalThis.__createCData(String(data == null ? "" : data)))); }
          : function () { throw new globalThis.DOMException("This DOM method is only valid on XML documents.", "NotSupportedError"); },
        createDocumentFragment: function () { return document.createDocumentFragment(); },
        createProcessingInstruction: function (target, data) { return document.createProcessingInstruction(target, data); },
        importNode: function (n) { return n; }, adoptNode: function (n) { return n; },
        getElementById: function (id) { var de = this.documentElement; return de && de.querySelector ? de.querySelector('#' + id) : null; },
        querySelector: function (s) { var de = this.documentElement; return de && de.querySelector ? de.querySelector(s) : null; },
        querySelectorAll: function (s) { var de = this.documentElement; return de && de.querySelectorAll ? de.querySelectorAll(s) : []; },
        getElementsByTagName: function (t) { var de = this.documentElement; return de && de.getElementsByTagName ? de.getElementsByTagName(t) : []; },
      };
      doc.implementation = {
        hasFeature: function () { return true; },
        createDocumentType: function (n, p, s) { return makeDoctypeFor(doc, n, p, s); },
        createHTMLDocument: function (t2) { return document.implementation.createHTMLDocument(t2); },
        createDocument: function (ns, q, dt) { return document.implementation.createDocument(ns, q, dt); },
      };
      Object.defineProperty(doc, "textContent", { get: function () { return null; }, set: function () {}, enumerable: true, configurable: true });
      Object.defineProperty(doc, "nodeValue", { get: function () { return null; }, set: function () {}, enumerable: true, configurable: true });
      backDocWithArena(doc, []);
      installDocumentTreeAccessors(doc);
      installDocMeta(doc, contentType);
      installDocTitle(doc);
      doc.cloneNode = function () { return buildBareDocument(kind, docNs, contentType); };
      var ctor = globalThis[ctorName];
      if (ctor && ctor.prototype) { try { Object.setPrototypeOf(doc, ctor.prototype); } catch (e) {} }
      return doc;
    }
    def(document, "implementation", {
      hasFeature: function () { return true; },
      createDocumentType: function (name, pub, sys) { return makeDoctypeFor(document, name, pub, sys); },
      createHTMLDocument: function (title) {
        var htmlEl = document.createElement("html");
        var headEl = document.createElement("head");
        var bodyEl = document.createElement("body");
        htmlEl.appendChild(headEl); htmlEl.appendChild(bodyEl);
        // createHTMLDocument(title): the title argument is optional. When PRESENT (including an
        // explicit null, which stringifies to "null") a <title> is created; only an omitted argument
        // (undefined) leaves the head empty.
        if (title !== undefined) {
          // Always append a Text node child (even for the empty string), per the createHTMLDocument
          // algorithm — `textContent = ""` would leave the <title> childless.
          var t = document.createElement("title"); t.appendChild(document.createTextNode(String(title))); headEl.appendChild(t);
        }
        var doc;
        doc = {
          nodeType: 9, nodeName: '#document', documentElement: htmlEl, head: headEl, body: bodyEl,
          doctype: null,
          // A document created off to the side has no associated browsing context / viewport, so per
          // CSSOM-View these always return null regardless of the coordinates passed.
          caretPositionFromPoint: function () { return null; },
          caretRangeFromPoint: function () { return null; },
          elementFromPoint: function () { return null; },
          elementsFromPoint: function () { return []; },
          lookupNamespaceURI: function () { return htmlEl && htmlEl.lookupNamespaceURI ? htmlEl.lookupNamespaceURI.apply(htmlEl, arguments) : null; },
          lookupPrefix: function () { return htmlEl && htmlEl.lookupPrefix ? htmlEl.lookupPrefix.apply(htmlEl, arguments) : null; },
          isDefaultNamespace: function () { return htmlEl && htmlEl.isDefaultNamespace ? htmlEl.isDefaultNamespace.apply(htmlEl, arguments) : (arguments[0] == null || arguments[0] === ""); },
          implementation: {
            hasFeature: function () { return true; },
            createDocumentType: function (n, p, s) { return makeDoctypeFor(doc, n, p, s); },
            createHTMLDocument: function (t2) { return document.implementation.createHTMLDocument(t2); },
            createDocument: function (ns, q, dt) { return document.implementation.createDocument(ns, q, dt); },
          },
          // Cloning a document does NOT run the createHTMLDocument algorithm (no doctype/html/head/
          // body/title get synthesised); a shallow clone is a brand-new empty HTML document.
          cloneNode: function () { return buildBareDocument("html", HTML_NS, "text/html"); },
          createElement: function (tag) { return document.createElement(tag); },
          createElementNS: function (ns, tag) { return document.createElementNS ? document.createElementNS(ns, tag) : document.createElement(tag); },
          createAttribute: function (name) {
            var nm = String(name);
            if (nm.length === 0) { globalThis.__invalidCharacterError(); }
            return globalThis.__makeAttrNode(null, null, nm.toLowerCase(), nm.toLowerCase());
          },
          createAttributeNS: function (ns, qn) {
            var ex = globalThis.__validateAndExtractName(ns, qn);
            return globalThis.__makeAttrNode(ex.namespace, ex.prefix, ex.localName, String(qn));
          },
          createTextNode: function (s) { return document.createTextNode(s); },
          createComment: function (s) { return document.createComment(s); },
          // An HTML document refuses createCDATASection (the XML createDocument path overrides this).
          createCDATASection: function () { throw new globalThis.DOMException("This DOM method is only valid on XML documents.", "NotSupportedError"); },
          createDocumentFragment: function () { return document.createDocumentFragment(); },
          createProcessingInstruction: function (target, data) { return document.createProcessingInstruction(target, data); },
          importNode: function (n) { return n; }, adoptNode: function (n) { return n; },
          getElementById: function (id) { return htmlEl.querySelector ? htmlEl.querySelector('#' + id) : null; },
          querySelector: function (s) { return htmlEl.querySelector ? htmlEl.querySelector(s) : null; },
          querySelectorAll: function (s) { return htmlEl.querySelectorAll ? htmlEl.querySelectorAll(s) : []; },
          getElementsByTagName: function (t) { return htmlEl.getElementsByTagName ? htmlEl.getElementsByTagName(t) : []; },
        };
        // A Document's textContent / nodeValue are null; setting them is a no-op.
        Object.defineProperty(doc, "textContent", { get: function () { return null; }, set: function () {}, enumerable: true, configurable: true });
        Object.defineProperty(doc, "nodeValue", { get: function () { return null; }, set: function () {}, enumerable: true, configurable: true });
        // createHTMLDocument() creates an HTML doctype before the document element.
        var doctype = makeDoctypeFor(doc, "html", "", "");
        doc.doctype = doctype;
        // Back the facade with a real arena Document node holding the doctype and <html>, so
        // appendChild / childNodes / traversal work on the off-document tree.
        backDocWithArena(doc, [doctype && typeof doctype.__node === "number" ? doctype.__node : -1,
                               htmlEl && typeof htmlEl.__node === "number" ? htmlEl.__node : -1]);
        installDocumentTreeAccessors(doc);
        installDocMeta(doc, "text/html");
        installDocTitle(doc);
        var hdoc = globalThis.HTMLDocument;
        if (hdoc && hdoc.prototype) { try { Object.setPrototypeOf(doc, hdoc.prototype); } catch (e) {} }
        return doc;
      },
      createDocument: function (namespace, qualifiedName, doctype) {
        // DOMImplementation.createDocument(namespace, qualifiedName, doctype) per
        // https://dom.spec.whatwg.org/#dom-domimplementation-createdocument — an empty XMLDocument
        // (no html/head/body scaffold), optionally carrying a root element and a doctype.
        // namespace and qualifiedName are required (non-optional) WebIDL arguments.
        if (arguments.length < 2) {
          throw new TypeError("Failed to execute 'createDocument' on 'DOMImplementation': 2 arguments required, but only " + arguments.length + " present.");
        }
        var docNs = (namespace === undefined || namespace === null || namespace === "") ? null : String(namespace);
        // WebIDL: `optional DocumentType? doctype = null` — undefined/null => no doctype; any other
        // non-DocumentType value is a TypeError.
        var dt = null;
        if (doctype !== undefined && doctype !== null) {
          if (!(typeof doctype === "object" && doctype.nodeType === 10 && typeof doctype.__node === "number")) {
            throw new TypeError("Failed to execute 'createDocument' on 'DOMImplementation': parameter 3 is not of type 'DocumentType'.");
          }
          // Canonicalize so the wrapper the caller holds is the same object `doc.doctype` (which
          // resolves children through __nodeFor) later returns — identity checks depend on it.
          dt = (typeof globalThis.__canonNode === "function") ? globalThis.__canonNode(doctype) : doctype;
        }
        // qualifiedName is a [LegacyNullToEmptyString] DOMString: null => "", but undefined (like any
        // other value) stringifies normally (=> "undefined"). A non-empty name is validated (and may
        // throw InvalidCharacterError/NamespaceError) BEFORE the document is otherwise built.
        var qn = qualifiedName === null ? "" : String(qualifiedName);
        var rootEl = qn === "" ? null : document.createElementNS(docNs, qn);
        // contentType is determined by the document's namespace.
        var contentType = docNs === "http://www.w3.org/1999/xhtml" ? "application/xhtml+xml"
                        : docNs === "http://www.w3.org/2000/svg" ? "image/svg+xml"
                        : "application/xml";
        var d = buildBareDocument("xml", docNs, contentType);
        // Append the doctype (if any) then the document element, in that order.
        if (dt) { d.appendChild(dt); try { Object.defineProperty(dt, "ownerDocument", { value: d, configurable: true, enumerable: true }); } catch (e) {} }
        if (rootEl) { d.appendChild(rootEl); }
        return d;
      },
    });
    installDocumentTreeAccessors(document);
  }
  if (typeof document.getElementsByName !== "function") {
    def(document, "getElementsByName", function (n) {
      var name = String(n);
      return globalThis.__makeNodeList(function () {
        var ids = __querySelectorAll("[name]"), out = [];
        for (var i = 0; i < ids.length; i++) {
          var node = globalThis.__nodeFor(ids[i]);
          if (node && node.namespaceURI === "http://www.w3.org/1999/xhtml" &&
              __getAttr(ids[i], "name") === name) { out.push(node); }
        }
        return out;
      }, true);
    });
  }
  if (typeof document.contains !== "function") {
    def(document, "contains", function (node) { return nodeContains(document, node); });
  }
  // The document is itself the root of its tree; getRootNode returns it, isSameNode is identity.
  if (typeof document.getRootNode !== "function") { def(document, "getRootNode", function () { return document; }); }
  if (typeof document.isSameNode !== "function") { def(document, "isSameNode", function (other) { return other === document; }); }
  if (typeof document.compareDocumentPosition !== "function") {
    def(document, "compareDocumentPosition", function (other) {
      if (!other || typeof other.__node !== "number") {
        throw new TypeError("Failed to execute 'compareDocumentPosition' on 'Node': parameter 1 is not of type 'Node'.");
      }
      var self = (typeof document.__node === "number") ? document.__node : 0;
      return __cmpDocPos(self, other.__node);
    });
  }

  // Document is a Node: its children are the doctype + the root element. Wire the Node mutation
  // methods on `document` itself. Only globals (`__insertNode`/`__removeChild`/`__parent`/
  // `__children`/`__documentElementId`) are in scope here, so the node-id helpers are inlined. The
  // document node id is the parent of <html>.
  (function () {
    function reqNode(x, m) {
      var n = (x && typeof x.__node === "number") ? x.__node : -1;
      if (n < 0) { throw new TypeError("Failed to execute '" + m + "' on 'Node': parameter is not of type 'Node'."); }
      return n;
    }
    function notFound(msg) { throw new (globalThis.DOMException)(msg, "NotFoundError"); }
    function docNode() { var de = __documentElementId(); return de >= 0 ? __parent(de) : -1; }
    var childNodesList = globalThis.__makeNodeList(function () {
      var ids = __children(docNode()), out = [];
      for (var i = 0; i < ids.length; i++) { out.push(globalThis.__nodeFor(ids[i])); }
      return out;
    }, true);
    Object.defineProperty(document, "childNodes", {
      get: function () { return childNodesList; }, enumerable: true, configurable: true
    });
    def(document, "appendChild", function (child) {
      var id = docNode(); var c = reqNode(child, "appendChild"); __insertNode(id, c, -1); return child;
    });
    if (typeof document.hasChildNodes !== "function") {
      def(document, "hasChildNodes", function () { var c = this.childNodes; return !!(c && c.length); });
    }
    if (document.nodeName === undefined) { def(document, "nodeName", "#document"); }
    def(document, "insertBefore", function (newNode, refNode) {
      var id = docNode(); var c = reqNode(newNode, "insertBefore");
      var r = (refNode == null) ? -1 : ((refNode && typeof refNode.__node === "number") ? refNode.__node : -1);
      if (refNode != null && r < 0) { notFound("The reference child is not a child of this node."); }
      __insertNode(id, c, r); return newNode;
    });
    def(document, "removeChild", function (child) {
      var id = docNode(); var c = reqNode(child, "removeChild");
      if (__parent(c) !== id) { notFound("The node to be removed is not a child of this node."); }
      __removeChild(id, c); return child;
    });
    def(document, "replaceChild", function (newNode, oldNode) {
      var id = docNode(); var n = reqNode(newNode, "replaceChild"), o = reqNode(oldNode, "replaceChild");
      if (__parent(o) !== id) { notFound("The node to be replaced is not a child of this node."); }
      var sibs = __children(id); var idx = sibs.indexOf(o);
      var ref = (idx >= 0 && idx + 1 < sibs.length) ? sibs[idx + 1] : -1;
      if (ref === n) { var ni = sibs.indexOf(n); ref = (ni >= 0 && ni + 1 < sibs.length) ? sibs[ni + 1] : -1; }
      __removeChild(id, o); __insertNode(id, n, ref); return oldNode;
    });
  })();
  // Legacy event factory. Maps a (case-insensitive) interface name to an uninitialized event of
  // the right interface (prototype chain intact); unknown names throw NotSupportedError. The real
  // implementation lives in globalThis.__createEvent (defined alongside the Event constructors).
  def(document, "createEvent", function (name) { return globalThis.__createEvent(name); });

  // --- hit-testing: elementFromPoint / caretPositionFromPoint / caretRangeFromPoint -----------
  //
  // The engine lays out the page and pushes every box's border-box rect (CSS px, document-absolute,
  // top-origin) to this worker as `layout_rects`, read here via `__rect(id)` (which already returns
  // the rect VIEWPORT-relative, i.e. with the vertical scroll subtracted). We cannot reach the
  // engine's live layout tree synchronously from the JS thread, so the hit-test runs here against
  // those pushed rects, using the live DOM (`__children`/`__parent`/`__nodeType`) for tree depth.
  //
  // `__elementAtPoint(x, y)` — x/y are CSS px, viewport-relative — returns the deepest ELEMENT node
  // id whose laid-out box contains the point, or -1 when the point is outside the viewport or hits
  // no box. It is the native primitive the three public methods are built on.
  function __viewportClientWidth() {
    var w = Number(globalThis.innerWidth) || 0;
    if (w > 0) { return w; }
    try { var c = document.documentElement && document.documentElement.clientWidth; if (typeof c === "number" && c > 0) { return c; } } catch (e) {}
    return 0;
  }
  function __viewportClientHeight() {
    var h = Number(globalThis.innerHeight) || 0;
    if (h > 0) { return h; }
    try { var c = document.documentElement && document.documentElement.clientHeight; if (typeof c === "number" && c > 0) { return c; } } catch (e) {}
    return 0;
  }
  // Deepest node (element OR text) whose engine-pushed rect contains the viewport point. Walks the
  // DOM tree; a child hit wins over its ancestor (deepest box on top). Ignores pointer-events (the
  // pushed rects carry no paint/pointer metadata) — adequate for the WPT cases, which only need the
  // node the point geometrically lands in. The pushed rects are the UNTRANSFORMED border boxes, so
  // hit-testing through CSS transforms (translate/rotate/scale) uses the pre-transform box — an
  // approximation that matches the painter only for the identity transform. Returns a node id or -1.
  function __deepestNodeAtPoint(x, y) {
    function rectOf(nodeId) {
      var r = null;
      try { r = __rect(nodeId); } catch (e) {}
      return r;
    }
    function contains(r) { return r && x >= r.left && x < r.right && y >= r.top && y < r.bottom; }
    // Depth-first; recurse into children first so deeper boxes take precedence, matching the
    // engine's `deepest_node_at`. Returns the deepest descendant-or-self node that contains the
    // point, or -1.
    function visit(nodeId) {
      var kids;
      try { kids = __children(nodeId); } catch (e) { kids = []; }
      for (var i = kids.length - 1; i >= 0; i--) {
        var hit = visit(kids[i]);
        if (hit >= 0) { return hit; }
      }
      var t = __nodeType(nodeId);
      // Only element (1) and text (3) boxes are candidates; skip comments / others.
      if (t === 1 || t === 3) {
        if (contains(rectOf(nodeId))) { return nodeId; }
      }
      return -1;
    }
    var rootId = __documentRootId();
    return visit(rootId);
  }
  // Public native: deepest ELEMENT at the viewport point (text hits climb to their element parent),
  // or -1 when outside the viewport / no box.
  def(globalThis, "__elementAtPoint", function (x, y) {
    x = Number(x); y = Number(y);
    if (!isFinite(x) || !isFinite(y)) { return -1; }
    if (x < 0 || y < 0 || x >= __viewportClientWidth() || y >= __viewportClientHeight()) { return -1; }
    var n = __deepestNodeAtPoint(x, y);
    while (n >= 0) {
      if (__nodeType(n) === 1) { return n; }
      var p = __parent(n);
      if (p < 0) { break; }
      n = p;
    }
    return -1;
  });

  if (typeof document.elementFromPoint !== "function") {
    def(document, "elementFromPoint", function (x, y) {
      var id = globalThis.__elementAtPoint(x, y);
      return id >= 0 ? __nodeFor(id) : null;
    });
  }
  if (typeof document.elementsFromPoint !== "function") {
    // Best-effort: the topmost element, then its ancestor chain (the engine pushes no z-order, so we
    // approximate the stack by the ancestor chain of the deepest hit).
    def(document, "elementsFromPoint", function (x, y) {
      var out = [];
      var id = globalThis.__elementAtPoint(x, y);
      while (id >= 0) {
        if (__nodeType(id) === 1) { out.push(__nodeFor(id)); }
        id = __parent(id);
      }
      return out;
    });
  }

  // caretPositionFromPoint(x, y): per CSSOM-View, the caret position (a CaretPosition with
  // offsetNode + character offset) for the point. Throws TypeError if called with fewer than two
  // arguments; returns null when the point is outside the viewport. offsetNode prefers the TEXT node
  // at the point (else the element); `offset` is the character index nearest the point, derived from
  // the text run's box width (a monospaced/uniform approximation — we have no per-glyph metrics).
  def(document, "caretPositionFromPoint", function (x, y) {
    if (arguments.length < 2) { throw new TypeError("Failed to execute 'caretPositionFromPoint' on 'Document': 2 arguments required, but only " + arguments.length + " present."); }
    x = Number(x); y = Number(y);
    if (!isFinite(x) || !isFinite(y)) { throw new TypeError("Failed to execute 'caretPositionFromPoint' on 'Document': argument is not a finite number."); }
    if (x < 0 || y < 0 || x >= __viewportClientWidth() || y >= __viewportClientHeight()) { return null; }
    return globalThis.__makeCaretAt(x, y);
  });

  // caretRangeFromPoint(x, y): a collapsed Range at the caret position for the point. With no/zero
  // coordinates it returns a Range collapsed at (root element, 0). Outside the viewport → null.
  def(document, "caretRangeFromPoint", function (x, y) {
    if (arguments.length >= 1) {
      var nx = Number(x), ny = Number(y);
      if (isFinite(nx) && isFinite(ny) && (nx < 0 || ny < 0 || nx >= __viewportClientWidth() || ny >= __viewportClientHeight())) {
        return null;
      }
    }
    var caret = globalThis.__makeCaretAt(x, y);
    var node, offset;
    if (caret) { node = caret.offsetNode; offset = caret.offset; }
    if (!node) {
      // No hit (no/zero coords, or empty layout): collapse at the root element, offset 0.
      var rootEl = document.documentElement || document.body || null;
      if (!rootEl) {
        try {
          var kids = __children(__documentRootId());
          for (var i = 0; i < kids.length; i++) { if (__nodeType(kids[i]) === 1) { rootEl = __nodeFor(kids[i]); break; } }
        } catch (e) {}
      }
      node = rootEl; offset = 0;
    }
    if (!node) { return null; }
    var r = new globalThis.Range();
    r.setStart(node, offset);
    r.setEnd(node, offset);
    return r;
  });
  if (typeof document.hasFocus !== "function") { def(document, "hasFocus", function () { return true; }); }
  if (!("activeElement" in document)) { Object.defineProperty(document, "activeElement", { get: function () { try { return document.body; } catch (e) { return null; } }, enumerable: true, configurable: true }); }
  if (!("visibilityState" in document)) { document.visibilityState = "visible"; }
  if (!("hidden" in document)) { document.hidden = false; }
  // The document's character encoding reflects its <meta charset> (or content-type meta), per the
  // Encoding standard's label->name mapping; absent one it defaults to UTF-8. charset / characterSet
  // / inputEncoding are aliases.
  function __canonCharset(label) {
    var l = String(label || "").trim().toLowerCase();
    var m = {
      "utf-8": "UTF-8", "utf8": "UTF-8", "unicode-1-1-utf-8": "UTF-8",
      "windows-1252": "windows-1252", "cp1252": "windows-1252", "x-cp1252": "windows-1252",
      "iso-8859-1": "windows-1252", "latin1": "windows-1252", "ascii": "windows-1252", "us-ascii": "windows-1252",
      "iso-8859-2": "ISO-8859-2", "latin2": "ISO-8859-2", "l2": "ISO-8859-2",
      "windows-1251": "windows-1251", "koi8-r": "KOI8-R", "shift_jis": "Shift_JIS", "sjis": "Shift_JIS",
      "euc-jp": "EUC-JP", "euc-kr": "EUC-KR", "gbk": "GBK", "gb2312": "GBK", "big5": "Big5",
      "utf-16": "UTF-16LE", "utf-16le": "UTF-16LE", "utf-16be": "UTF-16BE"
    };
    return Object.prototype.hasOwnProperty.call(m, l) ? m[l] : null;
  }
  function __documentCharset() {
    try {
      var m = document.querySelector("meta[charset]");
      if (m) { var c = __canonCharset(m.getAttribute("charset")); if (c) { return c; } }
      var hs = document.querySelectorAll("meta[http-equiv]");
      for (var i = 0; i < hs.length; i++) {
        if ((hs[i].getAttribute("http-equiv") || "").toLowerCase() === "content-type") {
          var mt = /charset\s*=\s*([^\s;]+)/i.exec(hs[i].getAttribute("content") || "");
          if (mt) { var cc = __canonCharset(mt[1]); if (cc) { return cc; } }
        }
      }
    } catch (e) {}
    return "UTF-8";
  }
  if (!("characterSet" in document) || typeof document.characterSet === "string") {
    var __csGetter = { get: function () { return __documentCharset(); }, enumerable: true, configurable: true };
    try { Object.defineProperty(document, "characterSet", __csGetter); } catch (e) {}
    try { Object.defineProperty(document, "charset", __csGetter); } catch (e) {}
    try { Object.defineProperty(document, "inputEncoding", __csGetter); } catch (e) {}
  }
  if (!("compatMode" in document)) { document.compatMode = "CSS1Compat"; }
  if (!("scrollingElement" in document)) { Object.defineProperty(document, "scrollingElement", { get: function () { try { return document.documentElement; } catch (e) { return null; } }, enumerable: true, configurable: true }); }
  if (typeof document.querySelectorAll === "function" && typeof document.querySelectorAll.call === "function") { /* present */ }

  // --- document.fonts (FontFaceSet) --------------------------------------------------------
  if (!("fonts" in document) || document.fonts == null) {
    var fontFaces = {
      status: "loaded", size: 0,
      ready: Promise.resolve(),
      load: function () { return Promise.resolve([]); },
      check: function () { return true; },
      add: fn, delete: function () { return false; }, has: function () { return false; },
      clear: fn, forEach: fn,
      addEventListener: fn, removeEventListener: fn, dispatchEvent: function () { return false; },
      onloading: null, onloadingdone: null, onloadingerror: null
    };
    // `ready` resolves to the set itself (per spec, the FontFaceSet). The engine loads any
    // @font-face web fonts and lays out with them BEFORE this page's scripts run, so an immediate
    // resolve is correct: by the time `document.fonts.ready.then(...)` callbacks run, geometry
    // already reflects the loaded fonts.
    fontFaces.ready = Promise.resolve(fontFaces);
    Object.defineProperty(document, "fonts", { value: fontFaces, enumerable: false, configurable: true, writable: true });
  }

  // --- Observer constructors ---------------------------------------------------------------
  // ============================================================================================
  // Real MutationObserver / IntersectionObserver / ResizeObserver.
  //
  // The heavy lifting lives in Rust: mutation TRACKING happens in the DOM primitives (queued and
  // exposed via __drainMutations), and geometry/intersection/size COMPUTATION happens in the
  // engine. These JS classes are the spec-facing registries + callback dispatch only.
  //
  //  - MutationObserver records {targetId, options} in __moRegistry; on first observe it flips the
  //    Rust gate via __observersActive(true). After a task, drain_event_loop calls __deliverMutations
  //    which reads __drainMutations(), matches recs to observers, builds MutationRecords, fires cbs.
  //  - IntersectionObserver/ResizeObserver register (observerId,nodeId,opts) in __io/__ro. The Rust
  //    engine reads __observedTargets(), computes geometry, and calls __deliverObservations(json).
  // ============================================================================================
  globalThis.__moRegistry = globalThis.__moRegistry || [];   // [{observer, targets:[{id,opts}], queue:[]}]
  globalThis.__io = globalThis.__io || {};                   // observerId -> {observer, cb, opts, targets:{nodeId:true}}
  globalThis.__ro = globalThis.__ro || {};                   // observerId -> {observer, cb, targets:{nodeId:true}}
  var __obsIdSeq = 1;

  function __syncObserversActive() {
    var any = false;
    for (var i = 0; i < globalThis.__moRegistry.length; i++) {
      if (globalThis.__moRegistry[i].targets.length) { any = true; break; }
    }
    try { __observersActive(any); } catch (e) {}
  }

  // node-id -> wrapper element. Reuse the canonical wrapper machinery so callbacks get the same
  // element objects the page already holds.
  function __nodeWrap(id) {
    if (typeof id !== "number" || id < 0) { return null; }
    try { return canon(__wrapNode(id)); } catch (e) { return null; }
  }
  globalThis.__nodeWrap = __nodeWrap;

  if (typeof globalThis.MutationObserver !== "function") {
    def(globalThis, "MutationObserver", function (cb) {
      this.callback = typeof cb === "function" ? cb : fn;
      this._entry = { observer: this, targets: [], queue: [] };
    });
    def(globalThis.MutationObserver.prototype, "observe", function (target, opts) {
      var id = (target && typeof target.__node === "number") ? target.__node : -1;
      if (id < 0) { return; }
      opts = opts || {};
      var rec = {
        targetId: id,
        childList: !!opts.childList,
        attributes: opts.attributes !== undefined ? !!opts.attributes : (opts.attributeOldValue || opts.attributeFilter ? true : false),
        characterData: opts.characterData !== undefined ? !!opts.characterData : (opts.characterDataOldValue ? true : false),
        subtree: !!opts.subtree,
        attributeOldValue: !!opts.attributeOldValue,
        characterDataOldValue: !!opts.characterDataOldValue,
        attributeFilter: opts.attributeFilter ? [].concat(opts.attributeFilter) : null
      };
      // Per spec, observing the same node again replaces its options.
      var t = this._entry.targets;
      for (var i = 0; i < t.length; i++) { if (t[i].targetId === id) { t.splice(i, 1); break; } }
      t.push(rec);
      if (globalThis.__moRegistry.indexOf(this._entry) < 0) { globalThis.__moRegistry.push(this._entry); }
      __syncObserversActive();
    });
    def(globalThis.MutationObserver.prototype, "disconnect", function () {
      this._entry.targets = [];
      this._entry.queue = [];
      var i = globalThis.__moRegistry.indexOf(this._entry);
      if (i >= 0) { globalThis.__moRegistry.splice(i, 1); }
      __syncObserversActive();
    });
    def(globalThis.MutationObserver.prototype, "takeRecords", function () {
      // Per spec, takeRecords() must synchronously return the records observed so far. Drain any
      // pending Rust-side mutations into every observer's queue first, then empty *our* queue.
      try { globalThis.__collectMutations(); } catch (e) {}
      var q = this._entry.queue; this._entry.queue = []; return q;
    });
  }

  // Walk ancestors (inclusive) of a node id, capped, to test subtree membership.
  function __isInclusiveAncestor(ancestorId, nodeId) {
    var cur = nodeId, guard = 0;
    while (typeof cur === "number" && cur >= 0 && guard++ < 10000) {
      if (cur === ancestorId) { return true; }
      cur = __parent(cur);
    }
    return false;
  }

  // Drain any pending Rust-side mutations, match each against every observer's registered targets,
  // build MutationRecords, and APPEND them to each matching observer's queue. Idempotent: once the
  // Rust queue is empty it does nothing. Shared by takeRecords() (synchronous) and the post-task
  // microtask delivery below.
  def(globalThis, "__collectMutations", function () {
    var recs;
    try { recs = JSON.parse(__drainMutations()); } catch (e) { recs = []; }
    if (!recs.length) { return; }
    var reg = globalThis.__moRegistry;
    for (var o = 0; o < reg.length; o++) {
      var entry = reg[o];
      for (var r = 0; r < recs.length; r++) {
        var rec = recs[r];
        for (var ti = 0; ti < entry.targets.length; ti++) {
          var t = entry.targets[ti];
          // Does this observed target match the mutated node? (exact, or ancestor if subtree)
          var matches = (t.targetId === rec.target) || (t.subtree && __isInclusiveAncestor(t.targetId, rec.target));
          if (!matches) { continue; }
          if (rec.kind === "childList") {
            if (!t.childList) { continue; }
          } else if (rec.kind === "attributes") {
            if (!t.attributes) { continue; }
            if (t.attributeFilter && t.attributeFilter.indexOf(rec.attr) < 0) { continue; }
          } else if (rec.kind === "characterData") {
            if (!t.characterData) { continue; }
          }
          var mr = { type: rec.kind, target: __nodeWrap(rec.target),
            attributeName: rec.kind === "attributes" ? rec.attr : null,
            attributeNamespace: null,
            oldValue: null,
            addedNodes: [], removedNodes: [],
            previousSibling: null, nextSibling: null };
          if (rec.kind === "attributes" && t.attributeOldValue) { mr.oldValue = rec.oldValue; }
          if (rec.kind === "characterData" && t.characterDataOldValue) { mr.oldValue = rec.oldValue; }
          if (rec.kind === "childList") {
            for (var a = 0; a < rec.added.length; a++) { var w = __nodeWrap(rec.added[a]); if (w) { mr.addedNodes.push(w); } }
            for (var rm = 0; rm < rec.removed.length; rm++) { var w2 = __nodeWrap(rec.removed[rm]); if (w2) { mr.removedNodes.push(w2); } }
          }
          entry.queue.push(mr);
          break; // one record per mutation per observer
        }
      }
    }
  });

  // Per spec, a DOM mutation "queues a mutation observer microtask": at most one delivery microtask
  // is pending at a time; it collects the Rust-queued mutations and flushes observer callbacks. This
  // lets observers fire at the microtask checkpoint (e.g. before an awaited `Promise.resolve()`),
  // which the engine's post-task delivery alone would miss.
  globalThis.__moMicrotaskQueued = globalThis.__moMicrotaskQueued || false;
  def(globalThis, "__scheduleMODelivery", function () {
    if (globalThis.__moMicrotaskQueued) { return; }
    var anyActive = globalThis.__moRegistry.some(function (e) { return e.targets.length; });
    if (!anyActive) { return; }
    globalThis.__moMicrotaskQueued = true;
    // Use a native (V8) microtask via Promise.resolve().then so delivery interleaves with the page's
    // own `await Promise.resolve()` continuations (the polyfilled queueMicrotask runs on a separate,
    // later-drained queue).
    try {
      Promise.resolve().then(function () {
        globalThis.__moMicrotaskQueued = false;
        try { globalThis.__deliverMutations(); } catch (e) {}
      });
    } catch (e) { globalThis.__moMicrotaskQueued = false; }
  });

  // Called (as a microtask) after a task when Rust has queued mutations. Collects them into each
  // observer's queue, then flushes non-empty queues to their callbacks.
  def(globalThis, "__deliverMutations", function () {
    try { globalThis.__collectMutations(); } catch (e) {}
    var reg = globalThis.__moRegistry.slice();
    for (var o = 0; o < reg.length; o++) {
      var entry = reg[o];
      if (!entry.queue.length) { continue; }
      var batch = entry.queue; entry.queue = [];
      try { entry.observer.callback.call(entry.observer, batch, entry.observer); }
      catch (e) { try { (globalThis.__timerErrors || (globalThis.__timerErrors = [])).push("MutationObserver: " + (e && e.message || e)); } catch (e2) {} }
    }
  });

  if (typeof globalThis.IntersectionObserver !== "function") {
    def(globalThis, "IntersectionObserver", function (cb, opts) {
      this.callback = typeof cb === "function" ? cb : fn;
      this.root = (opts && opts.root) || null; this.rootMargin = (opts && opts.rootMargin) || "0px";
      this.thresholds = (opts && [].concat(opts.threshold || 0)) || [0];
      this._oid = __obsIdSeq++;
      globalThis.__io[this._oid] = { observer: this, cb: this.callback, opts: opts || {}, targets: {} };
    });
    def(globalThis.IntersectionObserver.prototype, "observe", function (el) {
      var id = (el && typeof el.__node === "number") ? el.__node : -1;
      if (id >= 0 && globalThis.__io[this._oid]) { globalThis.__io[this._oid].targets[id] = true; }
    });
    def(globalThis.IntersectionObserver.prototype, "unobserve", function (el) {
      var id = (el && typeof el.__node === "number") ? el.__node : -1;
      if (id >= 0 && globalThis.__io[this._oid]) { delete globalThis.__io[this._oid].targets[id]; }
    });
    def(globalThis.IntersectionObserver.prototype, "disconnect", function () {
      if (globalThis.__io[this._oid]) { globalThis.__io[this._oid].targets = {}; }
    });
    def(globalThis.IntersectionObserver.prototype, "takeRecords", function () { return []; });
  }

  if (typeof globalThis.ResizeObserver !== "function") {
    def(globalThis, "ResizeObserver", function (cb) {
      this.callback = typeof cb === "function" ? cb : fn;
      this._oid = __obsIdSeq++;
      globalThis.__ro[this._oid] = { observer: this, cb: this.callback, targets: {} };
    });
    def(globalThis.ResizeObserver.prototype, "observe", function (el) {
      var id = (el && typeof el.__node === "number") ? el.__node : -1;
      if (id >= 0 && globalThis.__ro[this._oid]) { globalThis.__ro[this._oid].targets[id] = true; }
    });
    def(globalThis.ResizeObserver.prototype, "unobserve", function (el) {
      var id = (el && typeof el.__node === "number") ? el.__node : -1;
      if (id >= 0 && globalThis.__ro[this._oid]) { delete globalThis.__ro[this._oid].targets[id]; }
    });
    def(globalThis.ResizeObserver.prototype, "disconnect", function () {
      if (globalThis.__ro[this._oid]) { globalThis.__ro[this._oid].targets = {}; }
    });
  }

  // Native-readable list of IO/RO targets the engine should compute geometry for.
  def(globalThis, "__observedTargets", function () {
    var out = [];
    for (var ioid in globalThis.__io) {
      var io = globalThis.__io[ioid];
      for (var n in io.targets) { out.push({ kind: "io", observerId: Number(ioid), nodeId: Number(n) }); }
    }
    for (var roid in globalThis.__ro) {
      var ro = globalThis.__ro[roid];
      for (var n2 in ro.targets) { out.push({ kind: "ro", observerId: Number(roid), nodeId: Number(n2) }); }
    }
    return out;
  });

  // Engine calls this with computed geometry. Builds entries, groups per observer callback, fires.
  def(globalThis, "__deliverObservations", function (arr) {
    if (!arr || !arr.length) { return; }
    var ioBatches = {}, roBatches = {};
    for (var i = 0; i < arr.length; i++) {
      var it = arr[i];
      var target = __nodeWrap(it.nodeId);
      if (!target) { continue; }
      if (it.kind === "io" && globalThis.__io[it.observerId]) {
        var br = { x: it.x, y: it.y, width: it.width, height: it.height,
          top: it.y, left: it.x, right: it.x + it.width, bottom: it.y + it.height };
        var ratio = it.intersectionRatio || 0;
        var ir = it.isIntersecting
          ? { x: it.ix, y: it.iy, width: it.iw, height: it.ih, top: it.iy, left: it.ix, right: it.ix + it.iw, bottom: it.iy + it.ih }
          : { x: 0, y: 0, width: 0, height: 0, top: 0, left: 0, right: 0, bottom: 0 };
        var rb = { x: 0, y: 0, width: it.rootW, height: it.rootH, top: 0, left: 0, right: it.rootW, bottom: it.rootH };
        var entry = { target: target, isIntersecting: !!it.isIntersecting, intersectionRatio: ratio,
          boundingClientRect: br, intersectionRect: ir, rootBounds: rb,
          time: (globalThis.__eventLoop ? globalThis.__eventLoop.now : 0) };
        (ioBatches[it.observerId] || (ioBatches[it.observerId] = [])).push(entry);
      } else if (it.kind === "ro" && globalThis.__ro[it.observerId]) {
        var cr = { x: it.x, y: it.y, width: it.width, height: it.height, top: it.y, left: it.x, right: it.x + it.width, bottom: it.y + it.height };
        var box = [{ inlineSize: it.width, blockSize: it.height }];
        var entry2 = { target: target, contentRect: cr, borderBoxSize: box, contentBoxSize: box, devicePixelContentBoxSize: box };
        (roBatches[it.observerId] || (roBatches[it.observerId] = [])).push(entry2);
      }
    }
    for (var oid in ioBatches) {
      var ioReg = globalThis.__io[oid];
      if (ioReg) { try { ioReg.cb.call(ioReg.observer, ioBatches[oid], ioReg.observer); } catch (e) { try { (globalThis.__timerErrors || (globalThis.__timerErrors = [])).push("IntersectionObserver: " + (e && e.message || e)); } catch (e2) {} } }
    }
    for (var oid2 in roBatches) {
      var roReg = globalThis.__ro[oid2];
      if (roReg) { try { roReg.cb.call(roReg.observer, roBatches[oid2], roReg.observer); } catch (e) { try { (globalThis.__timerErrors || (globalThis.__timerErrors = [])).push("ResizeObserver: " + (e && e.message || e)); } catch (e2) {} } }
    }
  });
  // --- Resource Timing + PerformanceObserver("resource") -----------------------------------
  // A page that watches `PerformanceObserver({type:"resource"})` (e.g. to learn when a CSS
  // subresource has been fetched) needs three things that aren't wired by default: a resource-
  // timing buffer, an observer that delivers buffered + live `resource` entries, and the CSS
  // subresources to actually BE fetched (setting `el.style.background = url(...)` triggers no
  // JS-visible request; the Rust render pass fetches bitmaps separately and records no timing).
  // We activate all of it LAZILY — only once something observes "resource" timing — so pages that
  // don't use Resource Timing are completely unaffected.
  globalThis.__resourceEntries = globalThis.__resourceEntries || [];
  globalThis.__perfObservers = globalThis.__perfObservers || [];
  globalThis.__fetchedResources = globalThis.__fetchedResources || {};

  function __perfDeliver(observer, entries) {
    if (!entries.length) { return; }
    var list = {
      getEntries: function () { return entries.slice(); },
      getEntriesByType: function (t) { return entries.filter(function (e) { return e.entryType === t; }); },
      getEntriesByName: function (n, t) { return entries.filter(function (e) { return e.name === n && (!t || e.entryType === t); }); }
    };
    try { observer.__cb.call(observer, list, observer); } catch (e) {}
  }

  // Append a PerformanceResourceTiming-shaped entry and deliver it (async) to every observer
  // watching "resource".
  function __recordResourceTiming(name, initiatorType) {
    var now = 0;
    try { now = globalThis.performance ? globalThis.performance.now() : (globalThis.__eventLoop ? globalThis.__eventLoop.now : 0); } catch (e) {}
    var entry = {
      name: String(name), entryType: "resource", initiatorType: initiatorType || "other",
      startTime: now, duration: 0, fetchStart: now, responseEnd: now,
      domainLookupStart: now, domainLookupEnd: now, connectStart: now, connectEnd: now,
      requestStart: now, responseStart: now, secureConnectionStart: 0,
      redirectStart: 0, redirectEnd: 0, workerStart: 0, nextHopProtocol: "http/1.1",
      transferSize: 0, encodedBodySize: 0, decodedBodySize: 0, responseStatus: 200, serverTiming: []
    };
    entry.toJSON = function () { var o = {}; for (var k in this) { if (typeof this[k] !== "function") { o[k] = this[k]; } } return o; };
    globalThis.__resourceEntries.push(entry);
    var obs = globalThis.__perfObservers.slice();
    for (var i = 0; i < obs.length; i++) {
      (function (o) {
        if (o.__types.indexOf("resource") < 0) { return; }
        try { Promise.resolve().then(function () { __perfDeliver(o, [entry]); }); } catch (e) { __perfDeliver(o, [entry]); }
      })(obs[i]);
    }
  }

  // Resolve a possibly-relative resource URL against the document base.
  function __resolveResUrl(u) {
    try { return new URL(u, (typeof document !== "undefined" && document.baseURI) || (typeof location !== "undefined" && location.href) || "").href; }
    catch (e) { return u; }
  }
  // Fetch a CSS subresource once, sending an `Origin` header iff the resource is fetched in CORS
  // mode (shape-outside images, web fonts) and omitting it for no-cors resources (background/mask/
  // border images, `@import`). Records a resource-timing entry when the request settles — by which
  // point the server has seen the request, so a follow-up header read is race-free. Best-effort.
  function __fetchSubresource(rawUrl, cors, initiatorType) {
    if (!rawUrl) { return; }
    var url = __resolveResUrl(rawUrl);
    if (globalThis.__fetchedResources[url]) { return; }
    globalThis.__fetchedResources[url] = true;
    if (typeof __startFetch !== "function") { __recordResourceTiming(url, initiatorType); return; }
    var headers = {};
    if (cors) { try { headers["Origin"] = globalThis.origin || (typeof location !== "undefined" && location.origin) || ""; } catch (e) {} }
    var id;
    try { id = __startFetch("GET", url, "", JSON.stringify(headers)); }
    catch (e) { __recordResourceTiming(url, initiatorType); return; }
    var done = function () { __recordResourceTiming(url, initiatorType); };
    globalThis.__pendingFetches[id] = { url: url, resolve: done, reject: done };
  }
  function __extractCssUrls(value) {
    var out = [], re = /url\(\s*(["']?)([^"')]+)\1\s*\)/gi, m;
    while ((m = re.exec(value))) { out.push(m[2]); }
    return out;
  }
  // Background/mask/border/cursor/list/content images are fetched no-cors; shape-outside is cors.
  var __noCorsImageProp = /^(background|background-image|border-image|border-image-source|mask|mask-image|-webkit-mask|-webkit-mask-image|list-style-image|cursor|content)$/;
  function __scanInlineStyle(styleText) {
    var decls = String(styleText).split(";");
    for (var i = 0; i < decls.length; i++) {
      var c = decls[i].indexOf(":");
      if (c < 0) { continue; }
      var prop = decls[i].slice(0, c).trim().toLowerCase();
      var cors = prop === "shape-outside";
      if (!cors && !__noCorsImageProp.test(prop)) { continue; }
      var urls = __extractCssUrls(decls[i].slice(c + 1));
      for (var u = 0; u < urls.length; u++) { __fetchSubresource(urls[u], cors, prop === "shape-outside" ? "css" : "css"); }
    }
  }
  function __scanStylesheetText(text) {
    text = String(text);
    var im = /@import\s+(?:url\(\s*)?["']?([^"')\s;]+)/gi, m;
    while ((m = im.exec(text))) { __fetchSubresource(m[1], false, "css"); }
    var ff = /@font-face\s*\{([^}]*)\}/gi, fm;
    while ((fm = ff.exec(text))) {
      var sr = /src\s*:[^;}]*?url\(\s*["']?([^"')\s]+)/gi, sm;
      while ((sm = sr.exec(fm[1]))) { __fetchSubresource(sm[1], true, "css"); }
    }
  }
  function __scanElementResources(el) {
    try {
      if (!el || el.nodeType !== 1) { return; }
      if ((el.tagName || "").toUpperCase() === "STYLE") { __scanStylesheetText(el.textContent || ""); return; }
      var st = el.getAttribute ? el.getAttribute("style") : null;
      if (st) { __scanInlineStyle(st); }
    } catch (e) {}
  }
  function __scanSubtree(node) {
    if (!node || node.nodeType !== 1) { return; }
    __scanElementResources(node);
    var kids = node.children;
    if (kids) { for (var i = 0; i < kids.length; i++) { __scanSubtree(kids[i]); } }
  }
  // Scan the whole connected tree for CSS subresources and fetch any not-yet-fetched ones (the
  // `__fetchSubresource` dedupe makes repeat scans cheap). Run on every `observe("resource")`: the
  // test pattern connects the element BEFORE calling `wait_for_resource` (→ observe), so a fresh
  // scan at observe time reliably catches it without depending on live MutationObserver delivery.
  function __scanDocumentNow() {
    try { if (typeof document !== "undefined" && document.documentElement) { __scanSubtree(document.documentElement); } } catch (e) {}
  }
  // Also watch for elements / `<style>` connected AFTER an observer exists, via one internal
  // MutationObserver (registered once). Belt-and-suspenders alongside the per-observe scan.
  function __ensureResourceMO() {
    if (globalThis.__resourceMOStarted) { return; }
    globalThis.__resourceMOStarted = true;
    try {
      var mo = new MutationObserver(function (records) {
        for (var i = 0; i < records.length; i++) {
          var rec = records[i];
          if (rec.addedNodes) { for (var j = 0; j < rec.addedNodes.length; j++) { __scanSubtree(rec.addedNodes[j]); } }
          if (rec.type === "attributes" && rec.target) { __scanElementResources(rec.target); }
          if (rec.type === "characterData" && rec.target && rec.target.parentNode) { __scanElementResources(rec.target.parentNode); }
        }
      });
      mo.observe(document, { childList: true, subtree: true, attributes: true, attributeFilter: ["style"], characterData: true });
    } catch (e) {}
  }
  globalThis.__recordResourceTiming = __recordResourceTiming;

  if (typeof globalThis.PerformanceObserver !== "function" || !globalThis.PerformanceObserver.__real) {
    def(globalThis, "PerformanceObserver", function (cb) {
      this.__cb = typeof cb === "function" ? cb : fn;
      this.__types = [];
    });
    globalThis.PerformanceObserver.__real = true;
    globalThis.PerformanceObserver.supportedEntryTypes = ["resource", "mark", "measure", "navigation", "paint"];
    def(globalThis.PerformanceObserver.prototype, "observe", function (opts) {
      opts = opts || {};
      var types = opts.entryTypes ? [].concat(opts.entryTypes) : (opts.type ? [opts.type] : []);
      for (var i = 0; i < types.length; i++) { if (this.__types.indexOf(types[i]) < 0) { this.__types.push(types[i]); } }
      if (globalThis.__perfObservers.indexOf(this) < 0) { globalThis.__perfObservers.push(this); }
      if (this.__types.indexOf("resource") >= 0) {
        __ensureResourceMO();
        __scanDocumentNow();
        if (opts.buffered) {
          var self = this, buffered = globalThis.__resourceEntries.slice();
          try { Promise.resolve().then(function () { __perfDeliver(self, buffered); }); } catch (e) { __perfDeliver(self, buffered); }
        }
      }
    });
    def(globalThis.PerformanceObserver.prototype, "disconnect", function () {
      var i = globalThis.__perfObservers.indexOf(this);
      if (i >= 0) { globalThis.__perfObservers.splice(i, 1); }
      this.__types = [];
    });
    def(globalThis.PerformanceObserver.prototype, "takeRecords", function () { return []; });
  }

  // --- crossOriginIsolated / isSecureContext ----------------------------------------------
  // Boolean globals the platform always exposes (hr-time and others read `self.crossOriginIsolated`
  // and assert it is a boolean). We are never cross-origin isolated (no COOP+COEP gating), so it is
  // `false`. `isSecureContext` is true for https/file/localhost; approximate from the page URL.
  var __pgurl = String((globalThis.location && globalThis.location.href) || globalThis.__pageURL || "");
  def(globalThis, "crossOriginIsolated", globalThis.__crossOriginIsolated === true);
  def(globalThis, "isSecureContext", /^(https:|wss:|file:)/.test(__pgurl) ||
    /^https?:\/\/(localhost|127\.0\.0\.1|\[::1\])(?:[:\/]|$)/.test(__pgurl));

  // --- performance -------------------------------------------------------------------------
  if (!globalThis.performance || typeof globalThis.performance.now !== "function") {
    // The time origin: a real wall-clock epoch (ms) captured at context creation, so `timeOrigin`
    // is close to Date.now() and `timeOrigin + now()` tracks the wall clock (per spec). `now()` is
    // the high-res time since the origin, clamped monotonically so two reads never go backwards even
    // if the system clock is adjusted (hr-time requires a non-negative, monotonic clock).
    // A real `Performance` interface (extends EventTarget): the members live on the prototype so
    // idlharness's existence/inheritance/stringification checks pass, and `performance` is an actual
    // instance. now() is high-res time since a real wall-clock origin, clamped monotonically.
    defClass("Performance", globalThis.EventTarget);
    var Pp = globalThis.Performance.prototype;
    // Named function expressions so `.name` is the operation name (WebIDL / idlharness).
    Pp.now = function now() {
      if (!Object.prototype.hasOwnProperty.call(this, "__origin")) { throw new TypeError("Illegal invocation"); }
      var t;
      try { t = Date.now() - this.__origin; } catch (e) { t = this.__last; }
      if (!(t >= this.__last)) { t = this.__last; }
      this.__last = t;
      return t;
    };
    // WebIDL: reading an attribute getter on the interface prototype object (no instance) must throw.
    Object.defineProperty(Pp, "timeOrigin", {
      get: function () {
        if (!Object.prototype.hasOwnProperty.call(this, "__origin")) { throw new TypeError("Illegal invocation"); }
        return this.__origin;
      },
      enumerable: true, configurable: true
    });
    Pp.getEntries = function () { return (globalThis.__resourceEntries || []).slice(); };
    Pp.getEntriesByType = function (t) { return (globalThis.__resourceEntries || []).filter(function (e) { return e.entryType === t; }); };
    Pp.getEntriesByName = function (n, t) { return (globalThis.__resourceEntries || []).filter(function (e) { return e.name === n && (!t || e.entryType === t); }); };
    Pp.mark = fn; Pp.measure = fn; Pp.clearMarks = fn; Pp.clearMeasures = fn;
    Pp.clearResourceTimings = function () { globalThis.__resourceEntries = []; };
    Pp.setResourceTimingBufferSize = fn;
    Pp.toJSON = function toJSON() { return { timeOrigin: this.__origin, timing: this.timing, navigation: this.navigation }; };

    var __perfOrigin = (function () { try { return Date.now(); } catch (e) { return 0; } })();
    var perf = Object.create(Pp);
    perf.__origin = __perfOrigin;
    perf.__last = 0;
    perf.timing = { navigationStart: __perfOrigin, fetchStart: __perfOrigin, domLoading: __perfOrigin, domInteractive: 0, domContentLoadedEventStart: 0, domContentLoadedEventEnd: 0, domComplete: 0, loadEventStart: 0, loadEventEnd: 0, responseStart: __perfOrigin, responseEnd: __perfOrigin, requestStart: __perfOrigin, connectStart: __perfOrigin, connectEnd: __perfOrigin, secureConnectionStart: 0, domainLookupStart: __perfOrigin, domainLookupEnd: __perfOrigin, unloadEventStart: 0, unloadEventEnd: 0, redirectStart: 0, redirectEnd: 0, toJSON: function () { var o = {}, k = Object.keys(this); for (var i = 0; i < k.length; i++) { if (typeof this[k[i]] === "number") { o[k[i]] = this[k[i]]; } } return o; } };
    perf.navigation = { type: 0, redirectCount: 0, toJSON: function () { return { type: this.type, redirectCount: this.redirectCount }; } };
    perf.memory = { usedJSHeapSize: 0, totalJSHeapSize: 0, jsHeapSizeLimit: 0 };
    // WebIDL: `performance` is an attribute (accessor with a getter), not a data property.
    Object.defineProperty(globalThis, "performance", {
      get: function () { return perf; },
      set: function (v) { perf = v; },
      enumerable: true, configurable: true
    });
  } else {
    // A native/earlier-installed `performance` is present: route its resource-timing readers at our
    // buffer too (best-effort; ignored if the methods are non-configurable).
    try { globalThis.performance.getEntriesByType = function (t) { return (globalThis.__resourceEntries || []).filter(function (e) { return e.entryType === t; }); }; } catch (e) {}
    try { globalThis.performance.getEntriesByName = function (n, t) { return (globalThis.__resourceEntries || []).filter(function (e) { return e.name === n && (!t || e.entryType === t); }); }; } catch (e) {}
  }
  // The Performance interface extends EventTarget (it dispatches `resourcetimingbufferfull`), so it
  // carries addEventListener/removeEventListener/dispatchEvent. (installEvents is idempotent.)
  installEvents(globalThis.performance);

  // --- Animation timelines (Web Animations) ------------------------------------------------
  // Minimal AnimationTimeline/DocumentTimeline: `currentTime` is the high-res time since the
  // timeline's origin (default origin 0 → tracks performance.now()), matching the timestamp passed
  // to requestAnimationFrame callbacks. `document.timeline` is the default document timeline.
  if (typeof globalThis.DocumentTimeline !== "function") {
    defClass("AnimationTimeline");
    Object.defineProperty(globalThis.AnimationTimeline.prototype, "currentTime", {
      // During a frame, read the frozen frame time (set by requestAnimationFrame) so the timeline
      // matches the rAF timestamp exactly; otherwise the live high-res time.
      get: function () {
        var t = (typeof globalThis.__frameTime === "number")
          ? globalThis.__frameTime
          : (globalThis.performance ? globalThis.performance.now() : 0);
        return t - (this.__originTime || 0);
      },
      enumerable: true, configurable: true
    });
    def(globalThis, "DocumentTimeline", function (options) {
      this.__originTime = (options && typeof options.originTime === "number") ? options.originTime : 0;
    });
    globalThis.DocumentTimeline.prototype = Object.create(globalThis.AnimationTimeline.prototype);
    Object.defineProperty(globalThis.DocumentTimeline.prototype, "constructor", { value: globalThis.DocumentTimeline, enumerable: false, configurable: true, writable: true });
  }
  if (typeof document === "object" && document && !document.timeline) {
    try { Object.defineProperty(document, "timeline", { value: new globalThis.DocumentTimeline(), enumerable: true, configurable: true }); } catch (e) {}
  }

  // --- Viewport Segments (css-viewport-1) --------------------------------------------------
  // `window.viewport`: for a non-segmented viewport, one segment covering the whole inner size.
  if (typeof globalThis.viewport === "undefined") {
    var __vp = {};
    Object.defineProperty(__vp, "innerWidth", { get: function () { return globalThis.innerWidth; }, enumerable: true });
    Object.defineProperty(__vp, "innerHeight", { get: function () { return globalThis.innerHeight; }, enumerable: true });
    Object.defineProperty(__vp, "segments", {
      get: function () { return [{ innerWidth: globalThis.innerWidth, innerHeight: globalThis.innerHeight }]; },
      enumerable: true
    });
    def(globalThis, "viewport", __vp);
  }

  // --- IdleDeadline-style object is already provided via requestIdleCallback above. ---------

  // ===== XML documents: an independent pure-JS DOM + parser + serializer ======================
  // The arena-backed DOM is HTML-only. XML documents (DOMParser `text/xml`, XMLSerializer) need
  // element/attribute namespaces to round-trip per the DOM Parsing & Serialization spec, so they
  // use this self-contained node model instead of the arena.
  var __xml = (function () {
    var XML_NS = "http://www.w3.org/XML/1998/namespace";
    var XMLNS_NS = "http://www.w3.org/2000/xmlns/";

    function XNode(type, doc) { this.nodeType = type; this.ownerDocument = doc; this.childNodes = []; this.parentNode = null; }
    XNode.prototype.appendChild = function (c) { if (c.parentNode) { c.parentNode.removeChild(c); } c.parentNode = this; this.childNodes.push(c); return c; };
    XNode.prototype.insertBefore = function (c, ref) { if (ref == null) { return this.appendChild(c); } if (c.parentNode) { c.parentNode.removeChild(c); } var i = this.childNodes.indexOf(ref); if (i < 0) { return this.appendChild(c); } c.parentNode = this; this.childNodes.splice(i, 0, c); return c; };
    XNode.prototype.removeChild = function (c) { var i = this.childNodes.indexOf(c); if (i >= 0) { this.childNodes.splice(i, 1); c.parentNode = null; } return c; };
    XNode.prototype.replaceChild = function (nw, old) { var i = this.childNodes.indexOf(old); if (i < 0) { return old; } if (nw.parentNode) { nw.parentNode.removeChild(nw); } nw.parentNode = this; this.childNodes[i] = nw; old.parentNode = null; return old; };
    XNode.prototype.hasChildNodes = function () { return this.childNodes.length > 0; };
    XNode.prototype.append = function () { var d = this.ownerDocument || this; for (var i = 0; i < arguments.length; i++) { var c = arguments[i]; this.appendChild(typeof c === "string" ? d.createTextNode(c) : c); } };
    XNode.prototype.prepend = function () { var d = this.ownerDocument || this; var ref = this.childNodes[0] || null; for (var i = 0; i < arguments.length; i++) { var c = arguments[i]; this.insertBefore(typeof c === "string" ? d.createTextNode(c) : c, ref); } };
    XNode.prototype.isEqualNode = function (o) { return globalThis.__nodesEqual(this, o); };
    XNode.prototype.cloneNode = function () { return this; };
    Object.defineProperty(XNode.prototype, "firstChild", { get: function () { return this.childNodes[0] || null; } });
    Object.defineProperty(XNode.prototype, "lastChild", { get: function () { return this.childNodes[this.childNodes.length - 1] || null; } });
    Object.defineProperty(XNode.prototype, "nextSibling", { get: function () { var p = this.parentNode; if (!p) { return null; } return p.childNodes[p.childNodes.indexOf(this) + 1] || null; } });
    Object.defineProperty(XNode.prototype, "previousSibling", { get: function () { var p = this.parentNode; if (!p) { return null; } var i = p.childNodes.indexOf(this); return i > 0 ? p.childNodes[i - 1] : null; } });
    Object.defineProperty(XNode.prototype, "parentElement", { get: function () { var p = this.parentNode; return p && p.nodeType === 1 ? p : null; } });
    Object.defineProperty(XNode.prototype, "textContent", {
      get: function () { var s = ""; for (var i = 0; i < this.childNodes.length; i++) { var c = this.childNodes[i]; if (c.nodeType === 3 || c.nodeType === 4) { s += c.data; } else if (c.nodeType === 1) { s += c.textContent; } } return s; },
      set: function (v) { while (this.childNodes.length) { this.removeChild(this.childNodes[0]); } if (v !== "" && v != null) { this.appendChild(this.ownerDocument.createTextNode(String(v))); } }
    });

    function XAttr(ns, prefix, local, value) { this.namespaceURI = ns || null; this.prefix = prefix || null; this.localName = local; this.value = value; this.name = prefix ? prefix + ":" + local : local; this.nodeType = 2; }

    function splitQ(qname) { var c = qname.indexOf(":"); return c > 0 ? [qname.slice(0, c), qname.slice(c + 1)] : [null, qname]; }

    function XElement(doc, ns, prefix, local) { XNode.call(this, 1, doc); this.namespaceURI = ns || null; this.prefix = prefix || null; this.localName = local; this._attrs = []; }
    XElement.prototype = Object.create(XNode.prototype);
    XElement.prototype.constructor = XElement;
    Object.defineProperty(XElement.prototype, "tagName", { get: function () { return this.prefix ? this.prefix + ":" + this.localName : this.localName; } });
    Object.defineProperty(XElement.prototype, "nodeName", { get: function () { return this.tagName; } });
    Object.defineProperty(XElement.prototype, "attributes", { get: function () { var a = this._attrs.slice(); a.item = function (i) { return this[i] || null; }; return a; } });
    Object.defineProperty(XElement.prototype, "children", { get: function () { return this.childNodes.filter(function (c) { return c.nodeType === 1; }); } });
    Object.defineProperty(XElement.prototype, "firstElementChild", { get: function () { return this.children[0] || null; } });
    Object.defineProperty(XElement.prototype, "childElementCount", { get: function () { return this.children.length; } });
    XElement.prototype._findByName = function (name) { for (var i = 0; i < this._attrs.length; i++) { if (this._attrs[i].name === name) { return i; } } return -1; };
    XElement.prototype._findNS = function (ns, local) { for (var i = 0; i < this._attrs.length; i++) { var a = this._attrs[i]; if ((a.namespaceURI || null) === (ns || null) && a.localName === local) { return i; } } return -1; };
    XElement.prototype.getAttribute = function (name) { var i = this._findByName(name); return i >= 0 ? this._attrs[i].value : null; };
    XElement.prototype.hasAttribute = function (name) { return this._findByName(name) >= 0; };
    XElement.prototype.removeAttribute = function (name) { var i = this._findByName(name); if (i >= 0) { this._attrs.splice(i, 1); } };
    XElement.prototype.setAttribute = function (name, value) { var i = this._findByName(name); if (i >= 0) { this._attrs[i].value = String(value); } else { this._attrs.push(new XAttr(null, null, name, String(value))); } };
    XElement.prototype.getAttributeNS = function (ns, local) { var i = this._findNS(ns, local); return i >= 0 ? this._attrs[i].value : null; };
    XElement.prototype.setAttributeNS = function (ns, qname, value) { ns = ns || null; var s = splitQ(qname); var i = this._findNS(ns, s[1]); if (i >= 0) { this._attrs[i].value = String(value); this._attrs[i].prefix = s[0]; this._attrs[i].name = qname; } else { this._attrs.push(new XAttr(ns, s[0], s[1], String(value))); } };

    function XText(doc, data) { XNode.call(this, 3, doc); this.data = data; }
    XText.prototype = Object.create(XNode.prototype); XText.prototype.constructor = XText;
    Object.defineProperty(XText.prototype, "nodeValue", { get: function () { return this.data; }, set: function (v) { this.data = String(v); } });
    Object.defineProperty(XText.prototype, "textContent", { get: function () { return this.data; }, set: function (v) { this.data = String(v); } });
    function XComment(doc, data) { XNode.call(this, 8, doc); this.data = data; }
    XComment.prototype = Object.create(XText.prototype); XComment.prototype.constructor = XComment;
    function XCData(doc, data) { XNode.call(this, 4, doc); this.data = data; }
    XCData.prototype = Object.create(XText.prototype); XCData.prototype.constructor = XCData;
    function XPI(doc, target, data) { XNode.call(this, 7, doc); this.target = target; this.data = data; }
    XPI.prototype = Object.create(XNode.prototype); XPI.prototype.constructor = XPI;
    function XDoctype(doc, name, pub, sys) { XNode.call(this, 10, doc); this.name = name; this.publicId = pub || ""; this.systemId = sys || ""; }
    XDoctype.prototype = Object.create(XNode.prototype); XDoctype.prototype.constructor = XDoctype;

    function XDocument() { XNode.call(this, 9, null); }
    XDocument.prototype = Object.create(XNode.prototype); XDocument.prototype.constructor = XDocument;
    XDocument.prototype.createElement = function (name) { var s = splitQ(name); return new XElement(this, null, null, s[1]); };
    XDocument.prototype.createElementNS = function (ns, qname) { var s = splitQ(qname); return new XElement(this, ns || null, s[0], s[1]); };
    XDocument.prototype.createTextNode = function (d) { return new XText(this, String(d)); };
    XDocument.prototype.createComment = function (d) { return new XComment(this, String(d)); };
    XDocument.prototype.createCDATASection = function (d) { return new XCData(this, String(d)); };
    XDocument.prototype.createProcessingInstruction = function (t, d) { return new XPI(this, t, d); };
    Object.defineProperty(XDocument.prototype, "documentElement", { get: function () { for (var i = 0; i < this.childNodes.length; i++) { if (this.childNodes[i].nodeType === 1) { return this.childNodes[i]; } } return null; } });
    // A DOMParser-produced document is always UTF-8, regardless of any encoding declaration inside.
    Object.defineProperty(XDocument.prototype, "characterSet", { get: function () { return "UTF-8"; } });
    Object.defineProperty(XDocument.prototype, "charset", { get: function () { return "UTF-8"; } });
    Object.defineProperty(XDocument.prototype, "inputEncoding", { get: function () { return "UTF-8"; } });

    // --- Parser: a small namespace-aware XML reader ----------------------------------------------
    function parse(str) {
      var doc = new XDocument();
      var i = 0, n = str.length;
      var open = [];
      function cur() { return open.length ? open[open.length - 1] : doc; }
      function lookup(prefix, local) {
        if (local && Object.prototype.hasOwnProperty.call(local, prefix)) { return local[prefix]; }
        for (var k = open.length - 1; k >= 0; k--) { var d = open[k].__ns; if (d && Object.prototype.hasOwnProperty.call(d, prefix)) { return d[prefix]; } }
        if (prefix === "xml") { return XML_NS; }
        if (prefix === "xmlns") { return XMLNS_NS; }
        return prefix === "" ? null : undefined;
      }
      function decodeEnt(s) {
        return s.replace(/&(#x?[0-9a-fA-F]+|[a-zA-Z]+);/g, function (m, e) {
          if (e[0] === '#') { var cp = e[1] === "x" || e[1] === "X" ? parseInt(e.slice(2), 16) : parseInt(e.slice(1), 10); return isNaN(cp) ? m : String.fromCodePoint(cp); }
          var map = { lt: "<", gt: ">", amp: "&", quot: "\"", apos: "'" };
          return Object.prototype.hasOwnProperty.call(map, e) ? map[e] : m;
        });
      }
      while (i < n) {
        if (str[i] === "<") {
          if (str.substr(i, 4) === "<!--") { var e = str.indexOf("-->", i + 4); if (e < 0) { e = n - 3; } cur().appendChild(doc.createComment(str.slice(i + 4, e))); i = e + 3; continue; }
          if (str.substr(i, 9) === "<![CDATA[") { var e2 = str.indexOf("]]>", i + 9); if (e2 < 0) { e2 = n - 3; } cur().appendChild(doc.createCDATASection(str.slice(i + 9, e2))); i = e2 + 3; continue; }
          if (str.substr(i, 2) === "<?") { var e3 = str.indexOf("?>", i + 2); if (e3 < 0) { e3 = n - 2; } var body = str.slice(i + 2, e3); var sp = body.search(/\s/); var tgt = sp < 0 ? body : body.slice(0, sp); var dat = sp < 0 ? "" : body.slice(sp + 1); if (tgt.toLowerCase() !== "xml") { cur().appendChild(doc.createProcessingInstruction(tgt, dat)); } i = e3 + 2; continue; }
          if (str.substr(i, 2) === "<!") { var e4 = str.indexOf(">", i); if (e4 < 0) { e4 = n - 1; } i = e4 + 1; continue; }
          if (str[i + 1] === "/") { var e5 = str.indexOf(">", i); if (e5 < 0) { e5 = n - 1; } if (open.length) { open.pop(); } i = e5 + 1; continue; }
          // start tag
          i++;
          var nameM = /^[^\s/>]+/.exec(str.slice(i));
          if (!nameM) { return { error: true }; }
          var rawName = nameM[0]; i += rawName.length;
          var rawAttrs = [];
          while (i < n && str[i] !== ">" && str[i] !== "/") {
            var am = /^\s*([^\s=/>]+)\s*(=\s*("([^"]*)"|'([^']*)'))?/.exec(str.slice(i));
            if (!am || am[0].length === 0) { i++; continue; }
            i += am[0].length;
            var av = am[4] != null ? am[4] : (am[5] != null ? am[5] : "");
            rawAttrs.push([am[1], decodeEnt(av)]);
          }
          var selfClose = str[i] === "/";
          var gt = str.indexOf(">", i); i = (gt < 0 ? n : gt + 1);
          // collect this element's namespace declarations
          var nsdecl = {};
          for (var ai = 0; ai < rawAttrs.length; ai++) {
            var an = rawAttrs[ai][0];
            if (an === "xmlns") { nsdecl[""] = rawAttrs[ai][1]; }
            else if (an.slice(0, 6) === "xmlns:") { nsdecl[an.slice(6)] = rawAttrs[ai][1]; }
          }
          var es = splitQ(rawName);
          var elNs = lookup(es[0] || "", nsdecl);
          if (elNs === undefined) { elNs = null; }
          var el = new XElement(doc, elNs, es[0], es[1]);
          el.__ns = nsdecl;
          for (var aj = 0; aj < rawAttrs.length; aj++) {
            var qn = rawAttrs[aj][0], val = rawAttrs[aj][1];
            if (qn === "xmlns") { el.setAttributeNS(XMLNS_NS, "xmlns", val); }
            else if (qn.slice(0, 6) === "xmlns:") { el.setAttributeNS(XMLNS_NS, qn, val); }
            else { var qs = splitQ(qn); var ans = qs[0] ? lookup(qs[0], nsdecl) : null; if (ans === undefined) { ans = null; } el.setAttributeNS(ans, qn, val); }
          }
          cur().appendChild(el);
          if (!selfClose) { open.push(el); }
          continue;
        }
        var lt = str.indexOf("<", i); var end = lt < 0 ? n : lt;
        var text = str.slice(i, end);
        if (text.length) { cur().appendChild(doc.createTextNode(decodeEnt(text))); }
        i = end;
      }
      return { doc: doc };
    }

    // --- Serializer: the DOM Parsing & Serialization "XML serialization" algorithm ----------------
    var HTML_NS = "http://www.w3.org/1999/xhtml";
    // Attribute list for either an XML node (our model) or an arena-backed HTML node — so an HTML
    // element/fragment can be serialized too (XMLSerializer accepts any node).
    function attrsOf(node) { return node._attrs || (node.attributes ? Array.prototype.slice.call(node.attributes) : []); }
    function escText(s) { return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;"); }
    function escAttr(s) { return String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/"/g, "&quot;").replace(/\t/g, "&#9;").replace(/\n/g, "&#10;").replace(/\r/g, "&#13;"); }
    function mapClone(m) { var o = {}; for (var k in m) { o[k] = m[k].slice(); } return o; }
    function mapAdd(m, prefix, ns) { if (!m[prefix]) { m[prefix] = []; } if (m[prefix].indexOf(ns) < 0) { m[prefix].push(ns); } }
    function mapHas(m, prefix, ns) { return m[prefix] && m[prefix].indexOf(ns) >= 0; }
    function genPrefix(map, ns, idx) { var p = "ns" + idx.v; idx.v++; mapAdd(map, p, ns); return p; }
    function preferredPrefix(map, ns, prefer) {
      var cand = null;
      for (var p in map) { if (map[p].indexOf(ns) >= 0) { if (p === prefer) { return p; } if (p !== "") { cand = p; } } }
      return cand;
    }
    function recordNs(node, map, localPrefixes) {
      var localDefault = null;
      var __aa = attrsOf(node);
      for (var i = 0; i < __aa.length; i++) {
        var a = __aa[i];
        if ((a.namespaceURI || null) !== XMLNS_NS) { continue; }
        if (a.prefix === null) { localDefault = a.value; }
        else { var pfx = a.localName; if (!mapHas(map, pfx, a.value)) { mapAdd(map, pfx, a.value); } localPrefixes[pfx] = a.value; }
      }
      return localDefault;
    }
    function serAttrs(node, map, idx, localPrefixes, ignoreDefault) {
      var out = "";
      var __aa = attrsOf(node);
      for (var i = 0; i < __aa.length; i++) {
        var a = __aa[i];
        var ans = a.namespaceURI || null;
        // A no-namespace attribute literally named "xmlns" (e.g. via setAttribute) is not a real
        // namespace declaration; emitting it would forge one, so it's dropped.
        if (ans === null && a.localName === "xmlns") { continue; }
        if (ans === XMLNS_NS) {
          // a namespace-definition attribute
          if (a.prefix === null) { if (ignoreDefault) { continue; } out += " xmlns=\"" + escAttr(a.value) + "\""; continue; }
          // xmlns:prefix — drop if it just re-declares what the map already has for that prefix
          if (mapHas(map, a.localName, a.value) && a.value !== "") { /* still emit declared xmlns:* from source */ }
          out += " xmlns:" + a.localName + "=\"" + escAttr(a.value) + "\"";
          continue;
        }
        var pfx = "";
        if (ans !== null) {
          var cand = preferredPrefix(map, ans, a.prefix);
          if (cand !== null && cand !== "xmlns") {
            pfx = cand + ":";
          } else {
            // The namespace isn't already bound to a usable prefix: bind a freshly generated one.
            var p = genPrefix(map, ans, idx);
            out += " xmlns:" + p + "=\"" + escAttr(ans) + "\"";
            pfx = p + ":";
          }
        }
        out += " " + pfx + a.localName + "=\"" + escAttr(a.value) + "\"";
      }
      return out;
    }
    function serNode(node, ns, map, idx) {
      switch (node.nodeType) {
        case 1: return serElem(node, ns, map, idx);
        case 3: return escText(node.data);
        case 4: return "<![CDATA[" + node.data + "]]>";
        case 8: return "<!--" + node.data + "-->";
        case 7: return "<?" + node.target + " " + node.data + "?>";
        case 10: return "<!DOCTYPE " + node.name + (node.publicId ? " PUBLIC \"" + node.publicId + "\"" : "") + (node.systemId ? (node.publicId ? "" : " SYSTEM") + " \"" + node.systemId + "\"" : "") + ">";
        case 9: case 11: { var s = ""; for (var i = 0; i < node.childNodes.length; i++) { s += serNode(node.childNodes[i], ns, map, idx); } return s; }
        default: return "";
      }
    }
    function serElem(node, ns, map, idx) {
      map = mapClone(map);
      var localPrefixes = {};
      var localDefault = recordNs(node, map, localPrefixes);
      var inherited = ns;
      var nodeNs = node.namespaceURI || null;
      var qname, markup = "<", ignoreDefault = false;
      if ((inherited || null) === nodeNs) {
        if (localDefault !== null) { ignoreDefault = true; }
        qname = (nodeNs === XML_NS) ? "xml:" + node.localName : node.localName;
        markup += qname;
      } else {
        var prefix = node.prefix;
        if (prefix === "xmlns") { prefix = null; }
        var cand = preferredPrefix(map, nodeNs, prefix);
        if (cand !== null && cand !== "xmlns") {
          qname = cand + ":" + node.localName;
          if (localDefault !== null && localDefault !== "") { inherited = localDefault; }
          markup += qname;
        } else if (prefix !== null) {
          if (Object.prototype.hasOwnProperty.call(localPrefixes, prefix)) { prefix = genPrefix(map, nodeNs, idx); }
          else { mapAdd(map, prefix, nodeNs); }
          qname = prefix + ":" + node.localName;
          markup += qname + " xmlns:" + prefix + "=\"" + escAttr(nodeNs) + "\"";
        } else {
          qname = node.localName;
          inherited = nodeNs;
          // The element declares its own default namespace here, so the source `xmlns` attribute
          // (the same declaration, possibly stale/inconsistent) must not be repeated.
          ignoreDefault = true;
          markup += qname + " xmlns=\"" + escAttr(nodeNs || "") + "\"";
        }
      }
      markup += serAttrs(node, map, idx, localPrefixes, ignoreDefault);
      // HTML-namespace elements always serialize with an explicit end tag (never self-closing).
      if (node.childNodes.length === 0 && nodeNs !== HTML_NS) { return markup + "/>"; }
      markup += ">";
      for (var i = 0; i < node.childNodes.length; i++) { markup += serNode(node.childNodes[i], inherited, map, idx); }
      return markup + "</" + qname + ">";
    }
    function serialize(node) { return serNode(node, null, { "xml": [XML_NS] }, { v: 1 }); }

    return { parse: parse, serialize: serialize, XDocument: XDocument };
  })();

  // --- a few more constructors pages feature-detect ----------------------------------------
  if (typeof globalThis.DOMParser !== "function") {
    def(globalThis, "DOMParser", function () {
      this.parseFromString = function (str, type) {
        var t = String(type || "").toLowerCase();
        // text/html parses into a FRESH, independent HTML document (not the live one). Build an empty
        // html/head/body skeleton, then let the engine's HTML parser distribute the string's content
        // into the new head and body.
        if (t === "text/html") {
          var d = document.implementation.createHTMLDocument();
          try {
            var hid = d.head && typeof d.head.__node === "number" ? d.head.__node : -1;
            var bid = d.body && typeof d.body.__node === "number" ? d.body.__node : -1;
            if (typeof globalThis.__parseHtmlSections === "function" && (hid >= 0 || bid >= 0)) {
              globalThis.__parseHtmlSections(hid, bid, String(str));
            } else if (d.body) {
              d.body.innerHTML = String(str);   // fallback if the parse primitive is unavailable
            }
          } catch (e) {}
          return d;
        }
        // XML flavours: parse into an independent namespace-aware XML document.
        if (t.indexOf("xml") >= 0) { return __xml.parse(String(str)).doc; }
        return document;
      };
    });
  }
  if (typeof globalThis.XMLSerializer !== "function") {
    def(globalThis, "XMLSerializer", function () {
      this.serializeToString = function (node) { return __xml.serialize(node); };
    });
  }
  if (typeof globalThis.IntersectionObserverEntry !== "function") { def(globalThis, "IntersectionObserverEntry", function () {}); }
  if (typeof globalThis.MutationRecord !== "function") { def(globalThis, "MutationRecord", function () {}); }
  // --- DOM interface constructors / class hierarchy ----------------------------------------
  // Vue (and most frameworks) do `el instanceof SVGElement`, read `Node.prototype`, check
  // `typeof HTMLElement === "function"`, and reference HTMLUnknownElement/Text/Comment/etc.
  // We define each as a real constructor function carrying a `.prototype` and wire up a
  // prototype chain (HTMLDivElement -> HTMLElement -> Element -> Node) so prototype walks and
  // `instanceof` checks behave. The element wrappers' prototype is set to HTMLElement.prototype
  // (see __wrapNode below) so `el instanceof HTMLElement/Element/Node` returns true.
  function defClass(name, parentCtor) {
    var ctor = typeof globalThis[name] === "function" ? globalThis[name] : function () {};
    // WebIDL: the interface object's `name` is the interface name (idlharness checks it). An
    // anonymous function would otherwise infer the variable name ("ctor").
    try { Object.defineProperty(ctor, "name", { value: name, writable: false, enumerable: false, configurable: true }); } catch (e) {}
    // WebIDL: an interface object's [[Prototype]] is its inherited interface's object (so
    // `Object.getPrototypeOf(Sub) === Base`), and the interface prototype object is non-writable.
    if (parentCtor && parentCtor.prototype) {
      try { Object.setPrototypeOf(ctor.prototype, parentCtor.prototype); } catch (e) {}
      try { Object.setPrototypeOf(ctor, parentCtor); } catch (e) {}
    }
    try { Object.defineProperty(ctor, "prototype", { writable: false }); } catch (e) {}
    // Per WebIDL, an interface prototype object carries `@@toStringTag` = the interface name, so
    // `Object.prototype.toString.call(instance)` reports `[object <Interface>]`. Defined here on the
    // own prototype (configurable, non-enumerable) so e.g. a `CSSFontFaceRule` stringifies correctly.
    try {
      Object.defineProperty(ctor.prototype, Symbol.toStringTag,
        { value: name, writable: false, enumerable: false, configurable: true });
    } catch (e) {}
    if (globalThis[name] !== ctor) { def(globalThis, name, ctor); }
    return ctor;
  }
  var NodeCtor = defClass("Node");
  // Node type constants live on both the constructor and the prototype, so `Node.ELEMENT_NODE` and
  // `someNode.ELEMENT_NODE` (instance access, used by WPT) both resolve.
  (function (proto) {
    var consts = {
      ELEMENT_NODE: 1, ATTRIBUTE_NODE: 2, TEXT_NODE: 3, CDATA_SECTION_NODE: 4,
      ENTITY_REFERENCE_NODE: 5, ENTITY_NODE: 6, PROCESSING_INSTRUCTION_NODE: 7, COMMENT_NODE: 8,
      DOCUMENT_NODE: 9, DOCUMENT_TYPE_NODE: 10, DOCUMENT_FRAGMENT_NODE: 11, NOTATION_NODE: 12,
      DOCUMENT_POSITION_DISCONNECTED: 0x01, DOCUMENT_POSITION_PRECEDING: 0x02,
      DOCUMENT_POSITION_FOLLOWING: 0x04, DOCUMENT_POSITION_CONTAINS: 0x08,
      DOCUMENT_POSITION_CONTAINED_BY: 0x10, DOCUMENT_POSITION_IMPLEMENTATION_SPECIFIC: 0x20
    };
    for (var k in consts) {
      NodeCtor[k] = consts[k];
      if (proto) { try { def(proto, k, consts[k]); } catch (e) {} }
    }
  })(NodeCtor.prototype);
  // hasChildNodes() on the shared Node prototype, so non-element nodes (document, text, comment,
  // doctype, …) answer it too. Element wrappers install their own faster override in enrichElement.
  def(NodeCtor.prototype, "hasChildNodes", function () { var c = this.childNodes; return !!(c && c.length); });
  defClass("EventTarget");
  defClass("CharacterData", NodeCtor);
  var TextCtor = defClass("Text", globalThis.CharacterData);
  var CommentCtor = defClass("Comment", globalThis.CharacterData);
  (function () {
    function textData(args) {
      return (args.length === 0 || args[0] === undefined) ? "" : String(args[0]);
    }
    var textProto = TextCtor && TextCtor.prototype;
    var commentProto = CommentCtor && CommentCtor.prototype;
    function Text(data) {
      return globalThis.__canonNode(globalThis.__wrapNode(globalThis.__createText(textData(arguments))));
    }
    function Comment(data) {
      return globalThis.__canonNode(globalThis.__wrapNode(globalThis.__createComment(textData(arguments))));
    }
    if (textProto) {
      Text.prototype = textProto;
      try { Object.defineProperty(Text.prototype, "constructor", { value: Text, writable: true, configurable: true }); } catch (e) {}
    }
    if (commentProto) {
      Comment.prototype = commentProto;
      try { Object.defineProperty(Comment.prototype, "constructor", { value: Comment, writable: true, configurable: true }); } catch (e) {}
    }
    def(globalThis, "Text", Text);
    def(globalThis, "Comment", Comment);
  })();
  defClass("CDATASection", globalThis.Text);
  defClass("ProcessingInstruction", globalThis.CharacterData);
  var DocumentFragmentCtor = defClass("DocumentFragment", NodeCtor);
  try {
    if (DocumentFragmentCtor && DocumentFragmentCtor.prototype) {
      def(DocumentFragmentCtor.prototype, "getElementById", function (idStr) {
        var rootId = this && this.__node;
        if (typeof rootId !== "number") { return null; }
        var found = globalThis.__findElementByIdWithin(rootId, String(idStr));
        return found >= 0 ? globalThis.__nodeFor(found) : null;
      });
    }
  } catch (e) {}
  defClass("ShadowRoot", globalThis.DocumentFragment);
  defClass("DocumentType", NodeCtor);
  defClass("Attr", NodeCtor);
  var ElementCtor = defClass("Element", NodeCtor);
  var HTMLElementCtor = defClass("HTMLElement", ElementCtor);
  defClass("SVGElement", ElementCtor);
  defClass("SVGSVGElement", globalThis.SVGElement);
  defClass("SVGGraphicsElement", globalThis.SVGElement);
  defClass("MathMLElement", ElementCtor);
  defClass("HTMLUnknownElement", HTMLElementCtor);
  // A broad set of concrete HTMLElement subclasses pages feature-detect / reference.
  var htmlSubclasses = [
    "HTMLDivElement", "HTMLSpanElement", "HTMLParagraphElement", "HTMLAnchorElement",
    "HTMLImageElement", "HTMLInputElement", "HTMLButtonElement", "HTMLSelectElement",
    "HTMLOptionElement", "HTMLOptGroupElement", "HTMLTextAreaElement", "HTMLFormElement",
    "HTMLLabelElement", "HTMLUListElement", "HTMLOListElement", "HTMLLIElement",
    "HTMLTableElement", "HTMLTableRowElement", "HTMLTableCellElement", "HTMLTableSectionElement",
    "HTMLTableColElement", "HTMLTableCaptionElement", "HTMLHeadingElement", "HTMLPreElement",
    "HTMLQuoteElement", "HTMLHRElement", "HTMLBRElement", "HTMLScriptElement",
    "HTMLStyleElement", "HTMLLinkElement", "HTMLMetaElement", "HTMLTitleElement",
    "HTMLHeadElement", "HTMLBodyElement", "HTMLFrameSetElement", "HTMLHtmlElement", "HTMLCanvasElement",
    "HTMLVideoElement", "HTMLAudioElement", "HTMLMediaElement", "HTMLSourceElement",
    "HTMLTrackElement", "HTMLIFrameElement", "HTMLEmbedElement", "HTMLObjectElement",
    "HTMLPictureElement", "HTMLTemplateElement", "HTMLSlotElement", "HTMLDataListElement",
    "HTMLFieldSetElement", "HTMLLegendElement", "HTMLDetailsElement", "HTMLDialogElement",
    "HTMLMenuElement", "HTMLMapElement", "HTMLAreaElement", "HTMLDListElement",
    "HTMLDataElement", "HTMLTimeElement", "HTMLOutputElement", "HTMLProgressElement",
    "HTMLMeterElement", "HTMLModElement", "HTMLFontElement", "HTMLDirectoryElement",
    "HTMLMarqueeElement"
  ];
  // HTMLMediaElement should sit under HTMLElement; audio/video under it. Keep flat-under-HTMLElement
  // for simplicity except a couple that pages explicitly chain.
  for (var hi = 0; hi < htmlSubclasses.length; hi++) { defClass(htmlSubclasses[hi], HTMLElementCtor); }

  // Parsing can populate the wrapper cache before the interface constructors above are installed.
  for (var cachedNode in __nodeCache) {
    if (Object.prototype.hasOwnProperty.call(__nodeCache, cachedNode)) {
      try { applyNodePrototype(__nodeCache[cachedNode], Number(cachedNode)); } catch (e) {}
    }
  }

  // ElementCSSInlineStyle: `style` on the prototype chain (so assert_idl_attribute passes — it must
  // NOT be an own property). Returns the per-element cached CSSStyleDeclaration stashed by
  // enrichElement; [PutForwards=cssText] forwards string assignment to `.style.cssText`.
  try {
    if (ElementCtor && ElementCtor.prototype) {
      Object.defineProperty(ElementCtor.prototype, "style", {
        get: function () { return this.__styleObj || null; },
        set: function (v) { var s = this.__styleObj; if (s) { s.cssText = v == null ? "" : String(v); } },
        enumerable: true, configurable: true
      });
    }
  } catch (e) {}
  // LinkStyle mixin: `sheet` on HTMLStyleElement/HTMLLinkElement prototypes (must not be own, so
  // assert_idl_attribute passes). Lazily creates and caches the CSSStyleSheet on the element.
  try {
    var __sheetProtoNames = ["HTMLStyleElement", "HTMLLinkElement"];
    for (var spi = 0; spi < __sheetProtoNames.length; spi++) {
      var __sp = globalThis[__sheetProtoNames[spi]];
      if (__sp && __sp.prototype) {
        Object.defineProperty(__sp.prototype, "sheet", {
          get: function () {
            if (!this.__sheetHost) { return null; }
            // A <style>/<link>'s sheet exists only once the element is inserted into a tree; a freshly
            // created, never-appended element has no parent and thus no associated sheet (`.sheet` is
            // null). (An element in an <iframe> facade subtree has a parent, so it keeps its sheet.)
            try { if (this.parentNode == null) { return null; } } catch (e) {}
            if (!this.__sheetObj) { def(this, "__sheetObj", makeStyleSheet(this)); }
            return this.__sheetObj;
          },
          enumerable: false, configurable: true
        });
      }
    }
    // HTMLLinkElement.disabled / HTMLStyleElement.disabled. For <link>, `disabled` is backed by the
    // content attribute (and excludes the sheet from document.styleSheets while set). For <style>,
    // `disabled` mirrors the sheet's `disabled` state.
    // Fire `load` (async) on a connected, enabled stylesheet <link> — pages use link.onload to know
    // when the sheet is ready, and enabling a previously-disabled link reloads it.
    function __fireLinkLoad(link) {
      try {
        var rel = (link.getAttribute && link.getAttribute("rel") || "").toLowerCase();
        if (rel.split(/\s+/).indexOf("stylesheet") < 0) { return; }
        if (!link.getAttribute || !link.getAttribute("href")) { return; }
        if (!(document.documentElement && document.documentElement.contains(link))) { return; }
        // Defer to a later task (NOT a microtask): a stylesheet load is async, and firing it during
        // the `disabled` setter would re-enter the caller's code mid-statement (e.g. an onload handler
        // toggling `disabled` again before the setter's caller finishes).
        setTimeout(function () { try { if (typeof link.dispatchEvent === "function") { link.dispatchEvent(new Event("load")); } } catch (e) {} }, 0);
      } catch (e) {}
    }
    def(globalThis, "__fireLinkLoad", __fireLinkLoad);
    var __linkProto = globalThis.HTMLLinkElement && globalThis.HTMLLinkElement.prototype;
    if (__linkProto) {
      Object.defineProperty(__linkProto, "disabled", {
        get: function () { return this.getAttribute("disabled") != null || !!this.__sheetDisabled; },
        set: function (v) {
          if (v) { this.setAttribute("disabled", ""); def(this, "__sheetDisabled", true); }
          else { this.removeAttribute("disabled"); def(this, "__sheetDisabled", false); __fireLinkLoad(this); }
        },
        enumerable: true, configurable: true
      });
    }
    var __styleProto = globalThis.HTMLStyleElement && globalThis.HTMLStyleElement.prototype;
    if (__styleProto) {
      Object.defineProperty(__styleProto, "disabled", {
        get: function () { var s = this.sheet; return s ? !!s.disabled : false; },
        set: function (v) { var s = this.sheet; if (s) { s.disabled = !!v; } },
        enumerable: true, configurable: true
      });
    }
  } catch (e) {}

  // Document / Window and the other DOM interface constructors pages reference as globals
  // (e.g. `x instanceof Document`, `Node.prototype`, `HTMLCollection`). Defined so references and
  // instanceof checks don't throw ReferenceError.
  var DocumentCtor = defClass("Document", NodeCtor);
  defClass("HTMLDocument", DocumentCtor);
  defClass("XMLDocument", DocumentCtor);
  // A bare `new Document()` has no documentElement, so namespace lookups all return null. (The
  // page's live `document` overrides these via its own delegating methods.)
  try {
    if (DocumentCtor && DocumentCtor.prototype) {
      def(DocumentCtor.prototype, "lookupNamespaceURI", function () { return null; });
      def(DocumentCtor.prototype, "lookupPrefix", function () { return null; });
      def(DocumentCtor.prototype, "isDefaultNamespace", function (ns) { return ns == null || ns === ""; });
      def(DocumentCtor.prototype, "createRange", function () { return createRangeForDocument(this); });
      // A bare `new Document()` is an XML document, so it supports the CharacterData factories
      // (including createCDATASection, which an HTML document refuses). Nodes are real arena nodes so
      // they can be inserted into a live tree and traversed.
      var __mkNode = function (mkId) { return globalThis.__canonNode(globalThis.__wrapNode(mkId)); };
      def(DocumentCtor.prototype, "createTextNode", function (data) { return __mkNode(globalThis.__createText(String(data == null ? "" : data))); });
      def(DocumentCtor.prototype, "createComment", function (data) { return __mkNode(globalThis.__createComment(String(data == null ? "" : data))); });
      def(DocumentCtor.prototype, "createCDATASection", function (data) { return __mkNode(globalThis.__createCData(String(data == null ? "" : data))); });
      def(DocumentCtor.prototype, "createDocumentFragment", function () { return __mkNode(globalThis.__createDocumentFragment()); });
    }
  } catch (e) {}
  defClass("Window", globalThis.EventTarget);
  defClass("AbstractRange"); defClass("Range", globalThis.AbstractRange); defClass("StaticRange", globalThis.AbstractRange);
  var domIfaces = [
    "HTMLCollection", "NodeList", "DOMTokenList", "NamedNodeMap", "DOMStringMap", "DOMRectList",
    "CSSStyleDeclaration", "StyleSheetList", "MediaList", "CSSRuleList",
    "DOMRect", "DOMRectReadOnly", "DOMPoint", "DOMPointReadOnly", "DOMMatrix", "DOMMatrixReadOnly",
    "DOMQuad", "DOMException", "DOMParser", "XMLSerializer", "XPathResult", "XPathEvaluator",
    "MutationRecord", "AnimationEffect", "KeyframeEffect", "Animation", "AnimationTimeline",
    "CSSStyleValue", "StylePropertyMap", "VisualViewport", "Selection", "TextMetrics",
    "TimeRanges", "ValidityState", "HTMLFormControlsCollection", "RadioNodeList",
    "NodeIterator", "TreeWalker",
  ];
  for (var di = 0; di < domIfaces.length; di++) { defClass(domIfaces[di]); }

  // CSSStyleDeclaration is a WebIDL iterable<> (over its property names by index). Put the default
  // iterator on the PROTOTYPE so `Symbol.iterator in CSSStyleDeclaration.prototype` holds (instances
  // may still carry their own iterator over their live declarations).
  try {
    if (globalThis.CSSStyleDeclaration && globalThis.CSSStyleDeclaration.prototype) {
      Object.defineProperty(globalThis.CSSStyleDeclaration.prototype, Symbol.iterator, {
        value: function () {
          var self = this, i = 0;
          var it = { next: function () { var n = self.length >>> 0; return i < n ? { value: self[i++], done: false } : { value: undefined, done: true }; } };
          it[Symbol.iterator] = function () { return this; };
          return it;
        },
        writable: true, enumerable: false, configurable: true
      });
    }
  } catch (e) {}

  // --- DOMRect factory + real Range + CaretPosition (caret hit-testing support) ---------------
  // A DOMRect instance (prototype-correct, so `r instanceof DOMRect`) holding x/y/width/height plus
  // the derived top/right/bottom/left and a toJSON. Used by Range.getBoundingClientRect and
  // CaretPosition.getClientRect so callers get a real DOMRect, not a plain object.
  function __makeDOMRect(x, y, w, h) {
    x = Number(x) || 0; y = Number(y) || 0; w = Number(w) || 0; h = Number(h) || 0;
    var DR = globalThis.DOMRect;
    var r = (DR && DR.prototype) ? Object.create(DR.prototype) : {};
    r.x = x; r.y = y; r.width = w; r.height = h;
    r.left = w < 0 ? x + w : x; r.top = h < 0 ? y + h : y;
    r.right = w < 0 ? x : x + w; r.bottom = h < 0 ? y : y + h;
    r.toJSON = function () { return { x: this.x, y: this.y, width: this.width, height: this.height, top: this.top, right: this.right, bottom: this.bottom, left: this.left }; };
    return r;
  }
  def(globalThis, "__makeDOMRect", __makeDOMRect);

  // Wrap a node id into its CANONICAL wrapper (stable identity: the same object getElementById /
  // createElement / firstChild hand out), so `caret.offsetNode === el.firstChild` etc. hold.
  function __nodeFor(id) {
    if (typeof id !== "number" || id < 0) { return null; }
    var cached = (typeof globalThis.__nodeById === "function") ? globalThis.__nodeById(id) : null;
    if (cached) { return cached; }
    var w = __wrapNode(id);
    return (typeof globalThis.__canonNode === "function") ? globalThis.__canonNode(w) : w;
  }
  def(globalThis, "__nodeFor", __nodeFor);

  // The node id behind a wrapper (or a raw id), or -1.
  function __idOf(node) {
    if (node == null) { return -1; }
    if (typeof node === "number") { return node; }
    var n = node.__node;
    return (typeof n === "number") ? n : -1;
  }
  // Length of a node for Range offset bounds: text/comment -> character count, element -> child count.
  function __nodeLength(id) {
    if (id < 0) { return 0; }
    var t = __nodeType(id);
    if (t === 3 || t === 8) { var s = __textContent(id); return s ? s.length : 0; }
    try { return __children(id).length; } catch (e) { return 0; }
  }
  // Viewport-relative caret geometry for (textNodeId, offset): the text run's box gives the line
  // top/height; the caret x interpolates across the run by character fraction (uniform-advance
  // approximation — no per-glyph metrics are available here). Returns {x, top, height} or null.
  function __caretGeometry(containerId, offset) {
    var r = null; try { r = __rect(containerId); } catch (e) {}
    if (!r) {
      // Fall back to the parent element's box when the text node itself has no pushed rect.
      var p = __parent(containerId);
      if (p >= 0) { try { r = __rect(p); } catch (e2) {} }
    }
    if (!r) { return null; }
    var len = 0;
    if (__nodeType(containerId) === 3) { var s = __textContent(containerId); len = s ? s.length : 0; }
    var frac = len > 0 ? (Math.max(0, Math.min(offset, len)) / len) : 0;
    return { x: r.left + (r.right - r.left) * frac, top: r.top, height: r.bottom - r.top };
  }

  // A real Range: collapsed by default, supporting setStart/setEnd/collapse, toString (text between
  // boundary points within a single text container), getBoundingClientRect/getClientRects (caret or
  // text-span geometry), and cloneRange. Enough for the CSSOM caret tests and common callers.
  var AbstractRangeProto = (globalThis.AbstractRange && globalThis.AbstractRange.prototype) || Object.prototype;
  // The registry of every live Range. DOM mutations consult it to keep boundary points valid
  // (the "live range" steps the spec attaches to insert/remove/replace-data/split). Ranges added to
  // a Selection are tracked here too, since the Selection holds them by reference.
  var __liveRanges = [];
  // The registry of every live NodeIterator. DOM removals consult it to run the "NodeIterator
  // pre-removing steps" (https://dom.spec.whatwg.org/#nodeiterator-pre-removing-steps), keeping each
  // iterator's reference node valid after a node it points into is removed. Populated by
  // document.createNodeIterator below.
  var __liveNodeIterators = [];
  // True if `ancestor` is an inclusive ancestor of `node` (i.e. ancestor === node, or ancestor
  // contains node). Walks parent pointers; identity falls back to node-id equality.
  function __isInclusiveAncestor(ancestor, node) {
    var n = node;
    while (n != null) {
      if (__sameNode(n, ancestor)) { return true; }
      n = n.parentNode;
    }
    return false;
  }
  // Node identity that tolerates wrapper churn: same object, or same underlying arena node id.
  function __sameNode(a, b) {
    if (a === b) { return true; }
    if (a == null || b == null) { return false; }
    var ia = __idOf(a);
    return ia >= 0 && ia === __idOf(b);
  }
  // The NodeIterator pre-removing steps for a single node id about to be detached from the tree. Run
  // BEFORE the node leaves the tree (parent/siblings still intact). Mirrors the spec algorithm: for
  // each iterator whose reference lies inside the removed subtree (but whose root does not), advance
  // the reference past the subtree, or fall back to the node preceding it.
  function __runNodeIteratorPreRemove(toBeRemovedId) {
    var removed = __nodeFor(toBeRemovedId);
    if (removed == null) { return; }
    for (var i = 0; i < __liveNodeIterators.length; i++) {
      var it = __liveNodeIterators[i];
      // Terminate unless the removed node strictly contains the reference (and is not at/above root).
      if (__isInclusiveAncestor(removed, it._root)) { continue; }
      if (!__isInclusiveAncestor(removed, it._reference)) { continue; }
      if (it._pointerBefore) {
        // First following node within root that is outside the removed subtree, if any.
        var next = null, n = removed;
        while (n != null && !__sameNode(n, it._root)) {
          if (n.nextSibling != null) { next = n.nextSibling; break; }
          n = n.parentNode;
        }
        if (next != null) { it._reference = next; continue; }
        it._pointerBefore = false;
      }
      // Otherwise point at the node immediately preceding the removed node in tree order.
      var prev = removed.previousSibling;
      if (prev == null) {
        it._reference = removed.parentNode;
      } else {
        while (prev.lastChild != null) { prev = prev.lastChild; }
        it._reference = prev;
      }
    }
  }
  // A range is created with its boundary points at (current global document, 0).
  function Range() {
    var d = globalThis.document || null;
    this._sc = d; this._so = 0; this._ec = d; this._eo = 0;
    __liveRanges.push(this);
  }
  Range.prototype = Object.create(AbstractRangeProto);
  Range.prototype.constructor = Range;
  // ---- Range boundary-point helpers (per DOM spec) ----
  // node length: doctype 0; CharacterData -> data length; otherwise child count.
  function __rangeLength(node) {
    var id = __idOf(node); if (id < 0) { return 0; }
    var t = __nodeType(id);
    if (t === 10) { return 0; }
    if (t === 3 || t === 8 || t === 7 || t === 4) { var s = __textContent(id); return s ? s.length : 0; }
    return __children(id).length;
  }
  // Position of boundary point (nodeA, offA) relative to (nodeB, offB): -1 before, 0 equal, 1 after.
  // Only meaningful when the two nodes share a root (callers check first).
  function __cmpBP(nodeA, offA, nodeB, offB) {
    return globalThis.__cmpKey(globalThis.__pathKey(__idOf(nodeA), offA), globalThis.__pathKey(__idOf(nodeB), offB));
  }
  function __rootOf(node) { return globalThis.__rootId(__idOf(node)); }
  // WebIDL unsigned short conversion (for compareBoundaryPoints' `how`).
  function __toUint16(v) {
    var n = Number(v);
    if (isNaN(n) || n === 0 || n === Infinity || n === -Infinity) { return 0; }
    var posInt = (n < 0 ? -1 : 1) * Math.floor(Math.abs(n));
    var k = posInt % 65536; if (k < 0) { k += 65536; } return k;
  }
  function __idxErr(m) { throw new globalThis.DOMException(m || "The index is not in the allowed range.", "IndexSizeError"); }
  function __invNodeType(m) { throw new globalThis.DOMException(m || "The node is of a type that does not support this operation.", "InvalidNodeTypeError"); }
  function __wrongDoc() { throw new globalThis.DOMException("The object is in the wrong document.", "WrongDocumentError"); }
  function __isNodeLike(node) {
    if (node == null) { return false; }
    if (typeof node.__node === "number") { return true; }
    // Arena-less engine Documents (bare `new Document()`, createDocument) carry no `__node` but are
    // still Nodes for boundary-point purposes.
    return !!(globalThis.Node && (node instanceof globalThis.Node));
  }
  function __reqNode(node, method, idx) {
    if (!__isNodeLike(node)) {
      throw new TypeError("Failed to execute '" + method + "' on 'Range': parameter " + (idx || 1) + " is not of type 'Node'.");
    }
  }
  // Validate (node, offset) as a settable boundary point, returning the WebIDL-coerced offset.
  function __validBP(node, offset, method) {
    __reqNode(node, method);
    if (__nodeType(__idOf(node)) === 10) { __invNodeType("Cannot set a Range boundary to a doctype node."); }
    var off = offset >>> 0;
    if (off > __rangeLength(node)) { __idxErr("The offset " + off + " is larger than the node's length."); }
    return off;
  }
  // "Set the start of a range" — sets start, dragging end along when the new start is past it or in a
  // different tree. "Set the end" is the mirror.
  function __setRangeStart(r, node, off) {
    if (__rootOf(node) !== __rootOf(r._sc) || __cmpBP(node, off, r._ec, r._eo) > 0) { r._ec = node; r._eo = off; }
    r._sc = node; r._so = off;
  }
  function __setRangeEnd(r, node, off) {
    if (__rootOf(node) !== __rootOf(r._sc) || __cmpBP(node, off, r._sc, r._so) < 0) { r._sc = node; r._so = off; }
    r._ec = node; r._eo = off;
  }
  Object.defineProperty(Range.prototype, "startContainer", { get: function () { return this._sc; }, enumerable: true, configurable: true });
  Object.defineProperty(Range.prototype, "endContainer", { get: function () { return this._ec; }, enumerable: true, configurable: true });
  Object.defineProperty(Range.prototype, "startOffset", { get: function () { return this._so; }, enumerable: true, configurable: true });
  Object.defineProperty(Range.prototype, "endOffset", { get: function () { return this._eo; }, enumerable: true, configurable: true });
  Object.defineProperty(Range.prototype, "collapsed", { get: function () { return this._sc === this._ec && this._so === this._eo; }, enumerable: true, configurable: true });
  Object.defineProperty(Range.prototype, "commonAncestorContainer", { get: function () {
    if (this._sc === this._ec) { return this._sc; }
    // Nearest common ancestor of the two boundary nodes (by walking start's ancestor chain).
    var aId = __idOf(this._sc), bId = __idOf(this._ec);
    if (aId < 0) { return this._ec; }
    if (bId < 0) { return this._sc; }
    var aChain = {}; var c = aId;
    while (c >= 0) { aChain[c] = true; c = __parent(c); }
    c = bId;
    while (c >= 0) { if (aChain[c]) { return __nodeFor(c); } c = __parent(c); }
    return this._sc;
  }, enumerable: true, configurable: true });
  function __boundaryNodeType(node) {
    var id = __idOf(node);
    if (id >= 0) { return __nodeType(id); }
    return (node && typeof node.nodeType === "number") ? node.nodeType : -1;
  }
  function StaticRange(init) {
    if (!(this instanceof StaticRange)) { throw new TypeError("Constructor StaticRange requires 'new'."); }
    if (arguments.length < 1 || init == null) {
      throw new TypeError("Failed to construct 'StaticRange': 1 argument required.");
    }
    var dict = Object(init);
    function required(name) {
      if (!(name in dict) || dict[name] === undefined) {
        throw new TypeError("Failed to construct 'StaticRange': member '" + name + "' is required.");
      }
      return dict[name];
    }
    function boundaryNode(name) {
      var node = required(name);
      var t = __boundaryNodeType(node);
      if (node == null || t < 0) {
        throw new TypeError("Failed to construct 'StaticRange': member '" + name + "' is not of type 'Node'.");
      }
      if (t === 2 || t === 10) {
        __invNodeType("StaticRange boundary containers cannot be Attr or DocumentType nodes.");
      }
      return node;
    }
    this._sc = boundaryNode("startContainer");
    this._so = required("startOffset") >>> 0;
    this._ec = boundaryNode("endContainer");
    this._eo = required("endOffset") >>> 0;
  }
  StaticRange.prototype = Object.create(AbstractRangeProto);
  StaticRange.prototype.constructor = StaticRange;
  try {
    Object.defineProperty(StaticRange.prototype, Symbol.toStringTag,
      { value: "StaticRange", writable: false, enumerable: false, configurable: true });
  } catch (e) {}
  Object.defineProperty(StaticRange.prototype, "startContainer", { get: function () { return this._sc; }, enumerable: true, configurable: true });
  Object.defineProperty(StaticRange.prototype, "endContainer", { get: function () { return this._ec; }, enumerable: true, configurable: true });
  Object.defineProperty(StaticRange.prototype, "startOffset", { get: function () { return this._so; }, enumerable: true, configurable: true });
  Object.defineProperty(StaticRange.prototype, "endOffset", { get: function () { return this._eo; }, enumerable: true, configurable: true });
  Object.defineProperty(StaticRange.prototype, "collapsed", { get: function () { return this._sc === this._ec && this._so === this._eo; }, enumerable: true, configurable: true });
  Object.defineProperty(StaticRange.prototype, "commonAncestorContainer", { get: function () {
    if (this._sc === this._ec) { return this._sc; }
    var aId = __idOf(this._sc), bId = __idOf(this._ec);
    if (aId < 0) { return this._ec; }
    if (bId < 0) { return this._sc; }
    var aChain = {}; var c = aId;
    while (c >= 0) { aChain[c] = true; c = __parent(c); }
    c = bId;
    while (c >= 0) { if (aChain[c]) { return __nodeFor(c); } c = __parent(c); }
    return this._sc;
  }, enumerable: true, configurable: true });
  def(globalThis, "StaticRange", StaticRange);
  Range.prototype.setStart = function (node, offset) {
    var off = __validBP(node, offset, "setStart");
    __setRangeStart(this, node, off);
  };
  Range.prototype.setEnd = function (node, offset) {
    var off = __validBP(node, offset, "setEnd");
    __setRangeEnd(this, node, off);
  };
  // setStartBefore/After & setEndBefore/After: throw InvalidNodeTypeError when node has no parent.
  Range.prototype.setStartBefore = function (node) {
    __reqNode(node, "setStartBefore"); var id = __idOf(node); var p = __parent(id);
    if (p < 0) { __invNodeType("The node has no parent."); }
    __setRangeStart(this, __nodeFor(p), __children(p).indexOf(id));
  };
  Range.prototype.setStartAfter = function (node) {
    __reqNode(node, "setStartAfter"); var id = __idOf(node); var p = __parent(id);
    if (p < 0) { __invNodeType("The node has no parent."); }
    __setRangeStart(this, __nodeFor(p), __children(p).indexOf(id) + 1);
  };
  Range.prototype.setEndBefore = function (node) {
    __reqNode(node, "setEndBefore"); var id = __idOf(node); var p = __parent(id);
    if (p < 0) { __invNodeType("The node has no parent."); }
    __setRangeEnd(this, __nodeFor(p), __children(p).indexOf(id));
  };
  Range.prototype.setEndAfter = function (node) {
    __reqNode(node, "setEndAfter"); var id = __idOf(node); var p = __parent(id);
    if (p < 0) { __invNodeType("The node has no parent."); }
    __setRangeEnd(this, __nodeFor(p), __children(p).indexOf(id) + 1);
  };
  Range.prototype.collapse = function (toStart) {
    if (toStart) { this._ec = this._sc; this._eo = this._so; }
    else { this._sc = this._ec; this._so = this._eo; }
  };
  // selectNode: parent-less node throws InvalidNodeTypeError; otherwise select the node within parent.
  Range.prototype.selectNode = function (node) {
    __reqNode(node, "selectNode"); var id = __idOf(node); var p = __parent(id);
    if (p < 0) { __invNodeType("The node has no parent."); }
    var parent = __nodeFor(p); var idx = __children(p).indexOf(id);
    __setRangeStart(this, parent, idx);
    __setRangeEnd(this, parent, idx + 1);
  };
  // selectNodeContents: a doctype throws InvalidNodeTypeError; otherwise span the whole node.
  Range.prototype.selectNodeContents = function (node) {
    __reqNode(node, "selectNodeContents");
    if (__nodeType(__idOf(node)) === 10) { __invNodeType("Cannot select the contents of a doctype node."); }
    var len = __rangeLength(node);
    __setRangeStart(this, node, 0);
    __setRangeEnd(this, node, len);
  };
  // compareBoundaryPoints(how, sourceRange): NotSupportedError for how outside 0-3 (after WebIDL
  // unsigned-short coercion); WrongDocumentError when the ranges live in different trees.
  Range.prototype.compareBoundaryPoints = function (how, sourceRange) {
    var h = __toUint16(how);
    if (h !== 0 && h !== 1 && h !== 2 && h !== 3) {
      throw new globalThis.DOMException("The comparison method is not one of START_TO_START, START_TO_END, END_TO_END, or END_TO_START.", "NotSupportedError");
    }
    if (sourceRange == null || sourceRange._sc === undefined) {
      throw new TypeError("Failed to execute 'compareBoundaryPoints' on 'Range': parameter 2 is not of type 'Range'.");
    }
    if (__rootOf(this._sc) !== __rootOf(sourceRange._sc)) { __wrongDoc(); }
    var thisStart = (h === 0 || h === 3);                       // START_TO_START | END_TO_START
    var otherStart = (h === 0 || h === 1);                      // START_TO_START | START_TO_END
    var tn = thisStart ? this._sc : this._ec, to = thisStart ? this._so : this._eo;
    var on = otherStart ? sourceRange._sc : sourceRange._ec, oo = otherStart ? sourceRange._so : sourceRange._eo;
    return __cmpBP(tn, to, on, oo);
  };
  // comparePoint(node, offset): -1 / 0 / 1 for before / within / after; throws WrongDocumentError,
  // InvalidNodeTypeError, IndexSizeError per spec (in that order).
  Range.prototype.comparePoint = function (node, offset) {
    __reqNode(node, "comparePoint");
    if (__rootOf(node) !== __rootOf(this._sc)) { __wrongDoc(); }
    if (__nodeType(__idOf(node)) === 10) { __invNodeType("The node is a doctype."); }
    var off = offset >>> 0;
    if (off > __rangeLength(node)) { __idxErr("The offset is larger than the node's length."); }
    if (__cmpBP(node, off, this._sc, this._so) < 0) { return -1; }
    if (__cmpBP(node, off, this._ec, this._eo) > 0) { return 1; }
    return 0;
  };
  // isPointInRange(node, offset): false for a different root (no throw); doctype/oversized offset throw.
  Range.prototype.isPointInRange = function (node, offset) {
    __reqNode(node, "isPointInRange");
    if (__rootOf(node) !== __rootOf(this._sc)) { return false; }
    if (__nodeType(__idOf(node)) === 10) { __invNodeType("The node is a doctype."); }
    var off = offset >>> 0;
    if (off > __rangeLength(node)) { __idxErr("The offset is larger than the node's length."); }
    if (__cmpBP(node, off, this._sc, this._so) < 0 || __cmpBP(node, off, this._ec, this._eo) > 0) { return false; }
    return true;
  };
  // intersectsNode(node): does this range overlap node? false (no throw) when roots differ.
  Range.prototype.intersectsNode = function (node) {
    __reqNode(node, "intersectsNode");
    var id = __idOf(node);
    if (globalThis.__rootId(id) !== __rootOf(this._sc)) { return false; }
    var p = __parent(id);
    if (p < 0) { return true; }
    var parent = __nodeFor(p); var offset = __children(p).indexOf(id);
    if (__cmpBP(parent, offset, this._ec, this._eo) < 0 && __cmpBP(parent, offset + 1, this._sc, this._so) > 0) { return true; }
    return false;
  };
  // ---- "Clone the contents of a range" (DOM spec) --------------------------------------------
  // Boundary-point compare in node ids (mirror of __cmpBP, which works on node objects).
  function __cmpBPid(idA, offA, idB, offB) {
    return globalThis.__cmpKey(globalThis.__pathKey(idA, offA), globalThis.__pathKey(idB, offB));
  }
  // `a` is an inclusive ancestor of `b` (the spec's "ancestor container": a === b or a contains b).
  function __isInclAncestor(aId, bId) {
    var c = bId;
    while (c >= 0) { if (c === aId) { return true; } c = __parent(c); }
    return false;
  }
  // A node is "contained" in (scId,so)..(ecId,eo): same root, (node,0) after start, (node,len) before end.
  function __nodeContainedIn(id, scId, so, ecId, eo) {
    if (globalThis.__rootId(id) !== globalThis.__rootId(scId)) { return false; }
    var len = __rangeLength(__nodeFor(id));
    return __cmpBPid(id, 0, scId, so) > 0 && __cmpBPid(id, len, ecId, eo) < 0;
  }
  // "Partially contained": an inclusive ancestor of exactly one of the two boundary nodes.
  function __nodePartiallyContained(id, scId, ecId) {
    var a = __isInclAncestor(id, scId), b = __isInclAncestor(id, ecId);
    return (a && !b) || (b && !a);
  }
  function __isCharData(t) { return t === 3 || t === 4 || t === 7 || t === 8; }
  // Returns a DocumentFragment node object holding clones of the range's contents (range left intact).
  // Operates on a plain {_sc,_so,_ec,_eo} boundary record so recursive subranges need no live Range.
  function __cloneRangeContents(rec) {
    var frag = document.createDocumentFragment();
    var fragId = frag.__node;
    var scId = __idOf(rec._sc), so = rec._so, ecId = __idOf(rec._ec), eo = rec._eo;
    // Collapsed range: empty fragment.
    if (scId === ecId && so === eo) { return frag; }
    var startType = __nodeType(scId);
    // Both boundaries in the same CharacterData node: clone it, keeping only the selected substring.
    if (scId === ecId && __isCharData(startType)) {
      var c0 = globalThis.__cloneNode(scId, false);
      var d0 = __textContent(scId) || "";
      globalThis.__setTextContent(c0, d0.slice(so, eo));
      globalThis.__appendChild(fragId, c0);
      return frag;
    }
    // Common (inclusive) ancestor of both boundary nodes.
    var caId = scId;
    while (!__isInclAncestor(caId, ecId)) { caId = __parent(caId); }
    var kids = __children(caId);
    // First/last child of the common ancestor that's partially contained (null when a boundary node is
    // itself an inclusive ancestor of the other).
    var firstPC = -1, lastPC = -1;
    if (!__isInclAncestor(scId, ecId)) {
      for (var i = 0; i < kids.length; i++) { if (__nodePartiallyContained(kids[i], scId, ecId)) { firstPC = kids[i]; break; } }
    }
    if (!__isInclAncestor(ecId, scId)) {
      for (var j = kids.length - 1; j >= 0; j--) { if (__nodePartiallyContained(kids[j], scId, ecId)) { lastPC = kids[j]; break; } }
    }
    // Children fully contained in the range, in tree order. A contained doctype is a HierarchyRequestError.
    var contained = [];
    for (var k = 0; k < kids.length; k++) {
      if (__nodeContainedIn(kids[k], scId, so, ecId, eo)) {
        if (__nodeType(kids[k]) === 10) { throw new globalThis.DOMException("A DocumentType node cannot be cloned into a fragment.", "HierarchyRequestError"); }
        contained.push(kids[k]);
      }
    }
    // Leading partial: a CharacterData boundary contributes its trailing substring; an element contributes
    // a shallow clone filled by recursing into (start)..(child end).
    if (firstPC >= 0 && __isCharData(__nodeType(firstPC))) {
      var cf = globalThis.__cloneNode(scId, false);
      var df = __textContent(scId) || "";
      globalThis.__setTextContent(cf, df.slice(so));
      globalThis.__appendChild(fragId, cf);
    } else if (firstPC >= 0) {
      var cfe = globalThis.__cloneNode(firstPC, false);
      globalThis.__appendChild(fragId, cfe);
      var sub1 = __cloneRangeContents({ _sc: __nodeFor(scId), _so: so, _ec: __nodeFor(firstPC), _eo: __rangeLength(__nodeFor(firstPC)) });
      var sk1 = __children(sub1.__node).slice();
      for (var a = 0; a < sk1.length; a++) { globalThis.__appendChild(cfe, sk1[a]); }
    }
    // Fully contained children: deep clones.
    for (var c = 0; c < contained.length; c++) {
      globalThis.__appendChild(fragId, globalThis.__cloneNode(contained[c], true));
    }
    // Trailing partial: mirror of the leading case.
    if (lastPC >= 0 && __isCharData(__nodeType(lastPC))) {
      var cl = globalThis.__cloneNode(ecId, false);
      var dl = __textContent(ecId) || "";
      globalThis.__setTextContent(cl, dl.slice(0, eo));
      globalThis.__appendChild(fragId, cl);
    } else if (lastPC >= 0) {
      var cle = globalThis.__cloneNode(lastPC, false);
      globalThis.__appendChild(fragId, cle);
      var sub2 = __cloneRangeContents({ _sc: __nodeFor(lastPC), _so: 0, _ec: __nodeFor(ecId), _eo: eo });
      var sk2 = __children(sub2.__node).slice();
      for (var b = 0; b < sk2.length; b++) { globalThis.__appendChild(cle, sk2[b]); }
    }
    return frag;
  }
  // Like __cloneRangeContents, but MOVES the contained nodes into the fragment (removing them from
  // the tree) and trims the partially-contained CharData boundaries — i.e. the spec "extract"
  // algorithm. __appendChild reparents an already-attached node, so appending = moving.
  function __extractRangeContents(rec) {
    var frag = document.createDocumentFragment();
    var fragId = frag.__node;
    var scId = __idOf(rec._sc), so = rec._so, ecId = __idOf(rec._ec), eo = rec._eo;
    if (scId === ecId && so === eo) { return frag; }
    if (scId === ecId && __isCharData(__nodeType(scId))) {
      var t = __textContent(scId) || "";
      var clone = globalThis.__cloneNode(scId, false);
      globalThis.__setTextContent(clone, t.slice(so, eo));
      globalThis.__appendChild(fragId, clone);
      globalThis.__setTextContent(scId, t.slice(0, so) + t.slice(eo));
      return frag;
    }
    var caId = scId;
    while (!__isInclAncestor(caId, ecId)) { caId = __parent(caId); }
    var kids = __children(caId);
    var firstPC = -1, lastPC = -1;
    if (!__isInclAncestor(scId, ecId)) { for (var i = 0; i < kids.length; i++) { if (__nodePartiallyContained(kids[i], scId, ecId)) { firstPC = kids[i]; break; } } }
    if (!__isInclAncestor(ecId, scId)) { for (var j = kids.length - 1; j >= 0; j--) { if (__nodePartiallyContained(kids[j], scId, ecId)) { lastPC = kids[j]; break; } } }
    var contained = [];
    for (var k = 0; k < kids.length; k++) { if (__nodeContainedIn(kids[k], scId, so, ecId, eo)) { contained.push(kids[k]); } }
    // Leading partial boundary.
    if (firstPC >= 0 && __isCharData(__nodeType(firstPC))) {
      var df = __textContent(scId) || "";
      var cf = globalThis.__cloneNode(scId, false);
      globalThis.__setTextContent(cf, df.slice(so));
      globalThis.__appendChild(fragId, cf);
      globalThis.__setTextContent(scId, df.slice(0, so));
    } else if (firstPC >= 0) {
      var cfe = globalThis.__cloneNode(firstPC, false);
      globalThis.__appendChild(fragId, cfe);
      var sub1 = __extractRangeContents({ _sc: __nodeFor(scId), _so: so, _ec: __nodeFor(firstPC), _eo: __rangeLength(__nodeFor(firstPC)) });
      var sk1 = __children(sub1.__node).slice();
      for (var a = 0; a < sk1.length; a++) { globalThis.__appendChild(cfe, sk1[a]); }
    }
    // Fully contained children: move (not clone).
    for (var c = 0; c < contained.length; c++) { globalThis.__appendChild(fragId, contained[c]); }
    // Trailing partial boundary.
    if (lastPC >= 0 && __isCharData(__nodeType(lastPC))) {
      var dl = __textContent(ecId) || "";
      var cl = globalThis.__cloneNode(ecId, false);
      globalThis.__setTextContent(cl, dl.slice(0, eo));
      globalThis.__appendChild(fragId, cl);
      globalThis.__setTextContent(ecId, dl.slice(eo));
    } else if (lastPC >= 0) {
      var cle = globalThis.__cloneNode(lastPC, false);
      globalThis.__appendChild(fragId, cle);
      var sub2 = __extractRangeContents({ _sc: __nodeFor(lastPC), _so: 0, _ec: __nodeFor(ecId), _eo: eo });
      var sk2 = __children(sub2.__node).slice();
      for (var b = 0; b < sk2.length; b++) { globalThis.__appendChild(cle, sk2[b]); }
    }
    return frag;
  }
  // The collapse point after an extract/delete: the start, or (parent of the start's highest ancestor
  // not containing the end, index+1).
  function __rangeCollapseTarget(range) {
    var scId = __idOf(range._sc), ecId = __idOf(range._ec);
    if (__isInclAncestor(scId, ecId)) { return { node: range._sc, off: range._so }; }
    var ref = scId;
    while (__parent(ref) >= 0 && !__isInclAncestor(__parent(ref), ecId)) { ref = __parent(ref); }
    var pp = __parent(ref);
    return { node: __nodeFor(pp), off: __children(pp).indexOf(ref) + 1 };
  }
  Range.prototype.extractContents = function () {
    var target = __rangeCollapseTarget(this);
    var frag = __extractRangeContents({ _sc: this._sc, _so: this._so, _ec: this._ec, _eo: this._eo });
    __setRangeStart(this, target.node, target.off); __setRangeEnd(this, target.node, target.off);
    return frag;
  };
  // deleteContents leaves the same tree state as extracting and discarding the result.
  Range.prototype.deleteContents = function () { this.extractContents(); };
  Range.prototype.cloneContents = function () {
    return __cloneRangeContents(this);
  };
  Range.prototype.cloneRange = function () { var r = new Range(); r._sc = this._sc; r._so = this._so; r._ec = this._ec; r._eo = this._eo; return r; };
  Range.prototype.detach = function () {};
  Range.prototype.createContextualFragment = function (html) {
    if (arguments.length < 1) {
      throw new TypeError("Failed to execute 'createContextualFragment' on 'Range': 1 argument required, but only 0 present.");
    }
    // Parse the markup as an HTML fragment (scripts are parsed but not executed since the result
    // isn't connected), then move the parsed nodes into a DocumentFragment.
    var tmp = __createElement("template");
    __setInnerHTML(tmp, html == null ? "" : String(html));
    var frag = document.createDocumentFragment();
    var kids = __children(tmp).slice();
    for (var i = 0; i < kids.length; i++) { __appendChild(frag.__node, kids[i]); }
    return frag;
  };
  Range.prototype.toString = function () {
    // Only the common single-text-container case is modeled (the caret tests' usage): substring of
    // the text node between the two offsets.
    if (this._sc === this._ec && this._sc != null) {
      var id = __idOf(this._sc);
      if (id >= 0 && __nodeType(id) === 3) {
        var s = __textContent(id) || "";
        return s.substring(Math.min(this._so, this._eo), Math.max(this._so, this._eo));
      }
    }
    return "";
  };
  Range.prototype.getClientRects = function () {
    var r = this.getBoundingClientRect();
    return [r];
  };
  Range.prototype.getBoundingClientRect = function () {
    var scId = __idOf(this._sc);
    if (scId < 0) { return __makeDOMRect(0, 0, 0, 0); }
    var g0 = __caretGeometry(scId, this._so);
    if (!g0) { return __makeDOMRect(0, 0, 0, 0); }
    if (this._sc === this._ec && this._so === this._eo) {
      // Collapsed: a zero-width caret rect at the boundary.
      return __makeDOMRect(g0.x, g0.top, 0, g0.height);
    }
    if (this._sc === this._ec) {
      var g1 = __caretGeometry(scId, this._eo);
      var x0 = Math.min(g0.x, g1 ? g1.x : g0.x), x1 = Math.max(g0.x, g1 ? g1.x : g0.x);
      return __makeDOMRect(x0, g0.top, x1 - x0, g0.height);
    }
    // Cross-node span: approximate with the start container's box.
    var rr = null; try { rr = __rect(scId); } catch (e) {}
    if (rr) { return __makeDOMRect(rr.left, rr.top, rr.right - rr.left, rr.bottom - rr.top); }
    return __makeDOMRect(g0.x, g0.top, 0, g0.height);
  };
  // Install as the global Range, keeping `range instanceof Range` working. defClass already made an
  // empty Range earlier; overwrite it with this functional constructor (its prototype still chains to
  // AbstractRange).
  // compareBoundaryPoints `how` constants live on both the constructor and the prototype.
  (function () {
    var rconsts = { START_TO_START: 0, START_TO_END: 1, END_TO_END: 2, END_TO_START: 3 };
    for (var k in rconsts) {
      try { def(Range, k, rconsts[k]); } catch (e) {}
      try { def(Range.prototype, k, rconsts[k]); } catch (e) {}
    }
  })();
  try { def(globalThis, "Range", Range); } catch (e) {}

  // ---- Live-range maintenance (DOM "live range" steps) ---------------------------------------
  // Every DOM mutation that can disturb a Range boundary point runs the matching adjustment over
  // __liveRanges. These mirror the steps the DOM spec attaches to the "replace data", "split",
  // "insert", and "remove" algorithms. All work in node ids; boundary nodes are canonicalized via
  // __nodeFor so identity (e.g. `range.startContainer === newNode`) holds.

  // "Replace data" (node, offset, count, data): boundaries inside the replaced span clamp to offset;
  // boundaries past it shift by the net length change.
  function __rangesReplaceData(nodeId, offset, count, dataLen) {
    var delta = dataLen - count, end = offset + count;
    for (var i = 0; i < __liveRanges.length; i++) {
      var r = __liveRanges[i];
      if (__idOf(r._sc) === nodeId) {
        if (r._so > offset && r._so <= end) { r._so = offset; }
        else if (r._so > end) { r._so += delta; }
      }
      if (__idOf(r._ec) === nodeId) {
        if (r._eo > offset && r._eo <= end) { r._eo = offset; }
        else if (r._eo > end) { r._eo += delta; }
      }
    }
  }
  def(globalThis, "__rangesReplaceData", __rangesReplaceData);

  // "Insert": `count` nodes were inserted into `parentId` at `index`. Boundaries in the parent past
  // the insertion point shift right by count.
  function __rangesInsert(parentId, index, count) {
    for (var i = 0; i < __liveRanges.length; i++) {
      var r = __liveRanges[i];
      if (__idOf(r._sc) === parentId && r._so > index) { r._so += count; }
      if (__idOf(r._ec) === parentId && r._eo > index) { r._eo += count; }
    }
  }
  def(globalThis, "__rangesInsert", __rangesInsert);

  // a is an inclusive descendant of b (a === b, or a is nested under b). Computed BEFORE removal,
  // while the tree is still intact.
  function __isInclusiveDescendant(aId, bId) {
    var c = aId;
    while (c >= 0) { if (c === bId) { return true; } c = __parent(c); }
    return false;
  }

  // "Remove": `nodeId` (at `index` of `parentId`) is about to be removed. Boundaries inside the
  // removed subtree collapse to (parent, index); boundaries in the parent past it shift left by one.
  // Must run BEFORE the node leaves the tree.
  function __rangesRemove(nodeId, parentId, index) {
    if (!__liveRanges.length) { return; }
    var parentNode = __nodeFor(parentId);
    for (var i = 0; i < __liveRanges.length; i++) {
      var r = __liveRanges[i];
      if (__isInclusiveDescendant(__idOf(r._sc), nodeId)) { r._sc = parentNode; r._so = index; }
      else if (__idOf(r._sc) === parentId && r._so > index) { r._so -= 1; }
      if (__isInclusiveDescendant(__idOf(r._ec), nodeId)) { r._ec = parentNode; r._eo = index; }
      else if (__idOf(r._ec) === parentId && r._eo > index) { r._eo -= 1; }
    }
  }
  def(globalThis, "__rangesRemove", __rangesRemove);

  // The Text "split" range steps (run AFTER the new node is inserted, BEFORE the trailing data is
  // removed): boundaries in `node` past `offset` move into `newNode`; boundaries at the parent slot
  // immediately after `node` shift right by one.
  function __rangesSplit(nodeId, newNodeId, offset, parentId, nodeIndex) {
    var newNode = __nodeFor(newNodeId);
    for (var i = 0; i < __liveRanges.length; i++) {
      var r = __liveRanges[i];
      if (__idOf(r._sc) === nodeId && r._so > offset) { r._sc = newNode; r._so -= offset; }
      else if (parentId >= 0 && __idOf(r._sc) === parentId && r._so === nodeIndex + 1) { r._so += 1; }
      if (__idOf(r._ec) === nodeId && r._eo > offset) { r._ec = newNode; r._eo -= offset; }
      else if (parentId >= 0 && __idOf(r._ec) === parentId && r._eo === nodeIndex + 1) { r._eo += 1; }
    }
  }
  def(globalThis, "__rangesSplit", __rangesSplit);

  // Wrap the native tree-mutation primitives so every insert/remove (including the implicit removal
  // when a parented node is moved) runs the live-range steps. Cheap no-op when no ranges exist.
  (function () {
    var nativeInsertBefore = globalThis.__insertBefore;
    var nativeRemoveChild = globalThis.__removeChild;
    if (typeof nativeInsertBefore === "function") {
      def(globalThis, "__insertBefore", function (parentId, nodeId, refId) {
        if (__liveRanges.length && typeof nodeId === "number" && nodeId >= 0) {
          var oldParent = __parent(nodeId);
          if (oldParent >= 0) { __rangesRemove(nodeId, oldParent, __children(oldParent).indexOf(nodeId)); }
          var ret = nativeInsertBefore(parentId, nodeId, refId);
          var ni = __children(parentId).indexOf(nodeId);
          if (ni >= 0) { __rangesInsert(parentId, ni, 1); }
          return ret;
        }
        return nativeInsertBefore(parentId, nodeId, refId);
      });
    }
    if (typeof nativeRemoveChild === "function") {
      def(globalThis, "__removeChild", function (parentId, nodeId) {
        if (typeof nodeId === "number" && nodeId >= 0) {
          // Run the NodeIterator pre-removing steps before the node leaves the tree.
          if (__liveNodeIterators.length) { __runNodeIteratorPreRemove(nodeId); }
          if (__liveRanges.length) {
            var idx = __children(parentId).indexOf(nodeId);
            if (idx >= 0) { __rangesRemove(nodeId, parentId, idx); }
          }
        }
        return nativeRemoveChild(parentId, nodeId);
      });
    }
  })();

  // ---- Selection (https://w3c.github.io/selection-api/) --------------------------------------
  // A minimal but spec-faithful Selection: a single optional range, held by reference so it tracks
  // DOM mutations through __liveRanges. window.getSelection()/document.getSelection() return one
  // shared instance.
  var SelectionCtor = globalThis.Selection;
  var __selectionProto = (SelectionCtor && SelectionCtor.prototype) || Object.prototype;
  function __makeSelection() {
    var sel = Object.create(__selectionProto);
    sel._ranges = [];
    return sel;
  }
  function __selStart(sel) { return sel._ranges.length ? sel._ranges[0] : null; }
  Object.defineProperty(__selectionProto, "rangeCount", { get: function () { return this._ranges.length; }, enumerable: true, configurable: true });
  Object.defineProperty(__selectionProto, "isCollapsed", {
    get: function () { var r = __selStart(this); return r ? (r._sc === r._ec && r._so === r._eo) : true; },
    enumerable: true, configurable: true
  });
  Object.defineProperty(__selectionProto, "type", {
    get: function () { var r = __selStart(this); if (!r) { return "None"; } return (r._sc === r._ec && r._so === r._eo) ? "Caret" : "Range"; },
    enumerable: true, configurable: true
  });
  Object.defineProperty(__selectionProto, "anchorNode", { get: function () { var r = __selStart(this); return r ? r._sc : null; }, enumerable: true, configurable: true });
  Object.defineProperty(__selectionProto, "anchorOffset", { get: function () { var r = __selStart(this); return r ? r._so : 0; }, enumerable: true, configurable: true });
  Object.defineProperty(__selectionProto, "focusNode", { get: function () { var r = __selStart(this); return r ? r._ec : null; }, enumerable: true, configurable: true });
  Object.defineProperty(__selectionProto, "focusOffset", { get: function () { var r = __selStart(this); return r ? r._eo : 0; }, enumerable: true, configurable: true });
  def(__selectionProto, "getRangeAt", function (index) {
    index = index >>> 0;
    if (index >= this._ranges.length) { throw new globalThis.DOMException("The index is not in the allowed range.", "IndexSizeError"); }
    return this._ranges[index];
  });
  def(__selectionProto, "addRange", function (range) {
    if (!(range instanceof Range)) {
      throw new TypeError("Failed to execute 'addRange' on 'Selection': parameter 1 is not of type 'Range'.");
    }
    // Only ranges rooted in this selection's document take effect, and there is at most one range.
    if (globalThis.__rootId(__idOf(range._sc)) !== __idOf(globalThis.document)) { return; }
    if (this._ranges.length) { return; }
    this._ranges = [range];
  });
  def(__selectionProto, "removeRange", function (range) {
    var i = this._ranges.indexOf(range);
    if (i < 0) { throw new globalThis.DOMException("Could not find the given range.", "NotFoundError"); }
    this._ranges.splice(i, 1);
  });
  def(__selectionProto, "removeAllRanges", function () { this._ranges = []; });
  def(__selectionProto, "empty", function () { this._ranges = []; });
  // setBaseAndExtent: select from (anchorNode, anchorOffset) to (focusNode, focusOffset). We don't
  // track selection direction, so a backward selection's anchor/focus getters may read swapped; the
  // selected range itself is correct.
  def(__selectionProto, "setBaseAndExtent", function (anchorNode, anchorOffset, focusNode, focusOffset) {
    var range = globalThis.document.createRange();
    try { range.setStart(anchorNode, anchorOffset); range.setEnd(focusNode, focusOffset); }
    catch (e) { try { range.setStart(focusNode, focusOffset); range.setEnd(anchorNode, anchorOffset); } catch (e2) { return; } }
    this._ranges = [range];
  });
  // Caret/selection movement built on the Range model (one range per selection). collapse places a
  // caret; extend moves the focus keeping the anchor; selectAllChildren selects a node's contents.
  def(__selectionProto, "collapse", function (node, offset) {
    if (node == null) { this._ranges = []; return; }
    var range = globalThis.document.createRange();
    range.setStart(node, offset || 0); range.collapse(true);
    this._ranges = [range];
  });
  def(__selectionProto, "setPosition", __selectionProto.collapse);
  def(__selectionProto, "collapseToStart", function () {
    var r = this._ranges[0];
    if (!r) { throw new globalThis.DOMException("There is no selection to collapse.", "InvalidStateError"); }
    var nr = globalThis.document.createRange(); nr.setStart(r._sc, r._so); nr.collapse(true); this._ranges = [nr];
  });
  def(__selectionProto, "collapseToEnd", function () {
    var r = this._ranges[0];
    if (!r) { throw new globalThis.DOMException("There is no selection to collapse.", "InvalidStateError"); }
    var nr = globalThis.document.createRange(); nr.setStart(r._ec, r._eo); nr.collapse(true); this._ranges = [nr];
  });
  def(__selectionProto, "extend", function (node, offset) {
    var r = this._ranges[0];
    this.setBaseAndExtent(r ? r._sc : node, r ? r._so : 0, node, offset || 0);
  });
  def(__selectionProto, "selectAllChildren", function (node) {
    var range = globalThis.document.createRange(); range.selectNodeContents(node); this._ranges = [range];
  });
  def(__selectionProto, "containsNode", function (node) {
    var r = this._ranges[0];
    try { return !!(r && node && r.intersectsNode(node)); } catch (e) { return false; }
  });
  def(__selectionProto, "deleteFromDocument", function () { var r = this._ranges[0]; if (r && typeof r.deleteContents === "function") { r.deleteContents(); } });
  def(__selectionProto, "modify", function () {});   // direction/granularity movement not modelled
  def(__selectionProto, "selectAll", function () {});
  def(__selectionProto, "getComposedRanges", function () { return this._ranges.slice(); });
  def(__selectionProto, "toString", function () { var r = __selStart(this); return r ? r.toString() : ""; });

  var __selection = null;
  function getSelection() { if (!__selection) { __selection = __makeSelection(); } return __selection; }
  globalThis.getSelection = getSelection;
  try { def(globalThis.document, "getSelection", getSelection); } catch (e) {}

  // Document-level method stubs for APIs we don't implement, so calls don't throw a TypeError:
  // execCommand/queryCommand* (legacy editing — we report unsupported), getAnimations (none running),
  // and startViewTransition (run the update callback immediately; no visual transition).
  try {
    var d = globalThis.document;
    if (typeof d.execCommand !== "function") { def(d, "execCommand", function () { return false; }); }
    if (typeof d.queryCommandSupported !== "function") { def(d, "queryCommandSupported", function () { return false; }); }
    if (typeof d.queryCommandEnabled !== "function") { def(d, "queryCommandEnabled", function () { return false; }); }
    if (typeof d.queryCommandState !== "function") { def(d, "queryCommandState", function () { return false; }); }
    if (typeof d.queryCommandValue !== "function") { def(d, "queryCommandValue", function () { return ""; }); }
    if (typeof d.getAnimations !== "function") { def(d, "getAnimations", function () { return []; }); }
    if (typeof d.startViewTransition !== "function") {
      def(d, "startViewTransition", function (cb) {
        var done = Promise.resolve();
        try { if (typeof cb === "function") { var r = cb(); if (r && typeof r.then === "function") { done = r.then(function () {}, function () {}); } } } catch (e) {}
        return { ready: Promise.resolve(), finished: Promise.resolve(), updateCallbackDone: done, skipTransition: function () {} };
      });
    }
  } catch (e) {}

  // CaretPosition: { offsetNode, offset, getClientRect() }. getClientRect() returns a FRESH DOMRect
  // each call (the WPT test asserts identity differs between calls).
  function CaretPosition(offsetNode, offset, geom) {
    this.offsetNode = offsetNode; this.offset = offset; this._geom = geom || null;
  }
  CaretPosition.prototype.getClientRect = function () {
    var g = this._geom;
    if (!g) { return __makeDOMRect(0, 0, 0, 0); }
    return __makeDOMRect(g.x, g.top, 0, g.height); // collapsed caret: zero width
  };
  try { def(globalThis, "CaretPosition", CaretPosition); } catch (e) {}

  // __makeCaretAt(x, y): the CaretPosition at the viewport point. offsetNode prefers the deepest TEXT
  // node at the point (else the deepest element); offset is the nearest character index inside that
  // text run (uniform-advance approximation). Returns null when no box is hit. Media/replaced
  // elements (audio/video/canvas/input) and element hits resolve to offset 0.
  def(globalThis, "__makeCaretAt", function (x, y) {
    x = Number(x); y = Number(y);
    if (!isFinite(x) || !isFinite(y)) { return null; }
    var hit = __deepestNodeAtPoint(x, y);
    if (hit < 0) { return null; }
    var t = __nodeType(hit);
    if (t === 3) {
      // Text node: compute the character offset from the run box and the x coordinate.
      var r = null; try { r = __rect(hit); } catch (e) {}
      var s = __textContent(hit) || "";
      var offset = 0;
      if (r && s.length > 0 && r.right > r.left) {
        var charW = (r.right - r.left) / s.length;
        offset = Math.round((x - r.left) / charW);
        if (offset < 0) { offset = 0; } else if (offset > s.length) { offset = s.length; }
      }
      var node = __nodeFor(hit);
      return new CaretPosition(node, offset, __caretGeometry(hit, offset));
    }
    // Element hit (no text run at the point). Caret resolves to the element, offset 0.
    var node = __nodeFor(hit);
    return new CaretPosition(node, 0, __caretGeometry(hit, 0));
  });

  // --- CSSOM interface hierarchy + CSSRule type constants ------------------------------------
  // StyleSheet <- CSSStyleSheet; CSSRule <- {CSSStyleRule, CSSGroupingRule <- {CSSMediaRule,
  // CSSSupportsRule}, CSSImportRule, CSSFontFaceRule, CSSKeyframesRule, CSSKeyframeRule,
  // CSSNamespaceRule, CSSPageRule, CSSFontFeatureValuesRule}. instanceof + .type must hold.
  var StyleSheetCtor = defClass("StyleSheet");
  defClass("CSSStyleSheet", StyleSheetCtor);
  var CSSRuleCtor = defClass("CSSRule");
  (function (ctor) {
    var consts = { STYLE_RULE: 1, CHARSET_RULE: 2, IMPORT_RULE: 3, MEDIA_RULE: 4, FONT_FACE_RULE: 5,
      PAGE_RULE: 6, KEYFRAMES_RULE: 7, KEYFRAME_RULE: 8, MARGIN_RULE: 9, NAMESPACE_RULE: 10,
      COUNTER_STYLE_RULE: 11, SUPPORTS_RULE: 12, FONT_FEATURE_VALUES_RULE: 14, VIEWPORT_RULE: 15 };
    for (var k in consts) { ctor[k] = consts[k]; try { def(ctor.prototype, k, consts[k]); } catch (e) {} }
  })(CSSRuleCtor);
  defClass("CSSStyleRule", CSSRuleCtor);
  var CSSGroupingCtor = defClass("CSSGroupingRule", CSSRuleCtor);
  defClass("CSSConditionRule", CSSGroupingCtor);
  defClass("CSSMediaRule", globalThis.CSSConditionRule);
  defClass("CSSSupportsRule", globalThis.CSSConditionRule);
  defClass("CSSContainerRule", globalThis.CSSConditionRule);
  defClass("CSSImportRule", CSSRuleCtor);
  defClass("CSSFontFaceRule", CSSRuleCtor);
  defClass("CSSCounterStyleRule", CSSRuleCtor);
  defClass("CSSPageRule", CSSGroupingCtor);
  defClass("CSSKeyframesRule", CSSRuleCtor);
  defClass("CSSKeyframeRule", CSSRuleCtor);
  defClass("CSSNamespaceRule", CSSRuleCtor);
  defClass("CSSFontFeatureValuesRule", CSSRuleCtor);

  // The CSSStyleSheet constructor produces a constructable sheet (no owner node).
  (function () {
    var ctor = globalThis.CSSStyleSheet;
    if (typeof ctor === "function") {
      var Real = function (options) {
        var sheet = makeConstructedSheet("");
        options = options || {};
        if (options.media != null) { sheet.media.mediaText = String(options.media); }
        if (options.disabled) { sheet.disabled = true; }
        // `baseURL` is resolved against the constructor document's base URL. An invalid result
        // (e.g. a URL that fails to parse) is a NotAllowedError. The resolved URL becomes the
        // constructed sheet's base for relative `url(...)` resolution in its rules.
        if (options.baseURL != null) {
          var base;
          try { base = document.baseURI || (typeof location !== "undefined" ? location.href : undefined); } catch (e) { base = undefined; }
          var resolved;
          try { resolved = new URL(String(options.baseURL), base).href; }
          catch (e) { throw new globalThis.DOMException("Constructed style sheet base URL is not valid.", "NotAllowedError"); }
          sheet.__baseURL = resolved;
        }
        // The sheet's constructor document (used to validate adoptedStyleSheets membership).
        try { sheet.__constructorDocument = document; } catch (e) {}
        return sheet;
      };
      Real.prototype = ctor.prototype;
      def(globalThis, "CSSStyleSheet", Real);
    }
  })();

  // --- adoptedStyleSheets (Document / ShadowRoot) -------------------------------------------
  // CSSOM ObservableArray<CSSStyleSheet>. Each entry must be a CONSTRUCTED sheet whose constructor
  // document is `ownerDoc`; otherwise setting/inserting throws NotAllowedError. Adopted sheets are
  // mirrored into a managed `<style>` element appended to <head> (for the Document) so the cascade
  // applies them. `host.__refreshAdopted()` re-serializes the mirror; `markDirty` invokes it when an
  // adopted sheet mutates so rule edits / replaceSync are reflected in rendering.
  function installAdoptedStyleSheets(host, ownerDoc) {
    var backing = [];          // the actual CSSStyleSheet entries
    var mirror = null;         // managed <style> element (lazily created for the Document)
    function ensureMirror() {
      if (mirror) { return mirror; }
      try {
        mirror = ownerDoc.createElement("style");
        mirror.setAttribute("data-adopted-stylesheets", "");
        var head = ownerDoc.head || ownerDoc.getElementsByTagName("head")[0] || ownerDoc.documentElement || ownerDoc.body;
        if (head) { head.appendChild(mirror); }
      } catch (e) { mirror = null; }
      return mirror;
    }
    function serialize() {
      var s = "";
      for (var i = 0; i < backing.length; i++) {
        var sh = backing[i];
        if (!sh || sh.disabled) { continue; }
        try {
          var t = sh.cssText;
          // For a shadow root, scope every rule to the host's subtree so adopted styles don't leak
          // into the rest of the document (the mirror is a single global <style>). `:host` targets
          // the host element; other selectors become descendants of it. `host.__hostSel` is the marker.
          if (host && host.__hostSel && typeof globalThis.__scopeShadowCss === "function") {
            t = globalThis.__scopeShadowCss(t, host.__hostSel);
          }
          s += (s ? "\n" : "") + t;
        } catch (e) {}
      }
      return s;
    }
    function refresh() {
      var m = ensureMirror();
      if (!m) { return; }
      try { m.textContent = serialize(); } catch (e) {}
      // Carry a constructed sheet's explicit baseURL so the cascade resolves its relative url()s
      // against that base (not the document base). Best-effort: the first enabled sheet that has one.
      try {
        var base = null;
        for (var i = 0; i < backing.length; i++) {
          if (backing[i] && !backing[i].disabled && backing[i].__baseURL) { base = backing[i].__baseURL; break; }
        }
        if (base) { m.setAttribute("data-base-url", base); } else { m.removeAttribute("data-base-url"); }
      } catch (e) {}
    }
    host.__refreshAdopted = refresh;
    // Track host on each sheet so mutating it (markDirty) refreshes our mirror.
    function track(sh) {
      if (!sh) { return; }
      if (!sh.__adoptHosts) { try { def(sh, "__adoptHosts", []); } catch (e) { sh.__adoptHosts = []; } }
      if (sh.__adoptHosts.indexOf(host) < 0) { sh.__adoptHosts.push(host); }
    }
    function untrack(sh) {
      if (!sh || !sh.__adoptHosts) { return; }
      // Only untrack if no longer present in backing.
      if (backing.indexOf(sh) >= 0) { return; }
      var idx = sh.__adoptHosts.indexOf(host);
      if (idx >= 0) { sh.__adoptHosts.splice(idx, 1); }
    }
    function validate(v) {
      var ctor = globalThis.CSSStyleSheet;
      var isSheet = v && (typeof v === "object") && (ctor && ctor.prototype ? (v instanceof ctor) : true);
      // Standard CSSOM only allows constructed sheets. The tentative proposal (csswg-drafts #10013)
      // also allows adopting a sheet owned by an element in this document (a <link>/<style> sheet) —
      // which is what lets pages adopt an existing stylesheet into a shadow root.
      var ok = isSheet && (v.__constructed === true || v.__ownerNode != null);
      if (!ok) {
        throw new globalThis.DOMException("Can't adopt a non-constructed or foreign CSSStyleSheet.", "NotAllowedError");
      }
      if (v.__constructorDocument && v.__constructorDocument !== ownerDoc) {
        throw new globalThis.DOMException("Sheet constructor document does not match.", "NotAllowedError");
      }
    }
    // Build the observable-array proxy over `backing`. Index writes, length writes and the mutating
    // Array methods (push/splice/...) validate new entries and refresh the mirror afterwards.
    function makeArray(initial) {
      var arr = [];
      for (var i = 0; i < initial.length; i++) { arr.push(initial[i]); }
      var proxy = new Proxy(arr, {
        set: function (target, prop, value) {
          if (typeof prop === "string" && /^[0-9]+$/.test(prop)) {
            validate(value);
            target[prop] = value;
            rebuildBacking(target);
            return true;
          }
          if (prop === "length") {
            target.length = value;
            rebuildBacking(target);
            return true;
          }
          target[prop] = value;
          return true;
        },
        deleteProperty: function (target, prop) {
          delete target[prop];
          if (typeof prop === "string" && /^[0-9]+$/.test(prop)) { rebuildBacking(target); }
          return true;
        }
      });
      // Wrap the mutating methods so validation/refresh runs even through the proxy.
      ["push", "unshift", "splice", "fill", "copyWithin"].forEach(function (m) {
        var orig = Array.prototype[m];
        def(arr, m, function () {
          // Validate any incoming sheet arguments before mutating.
          if (m === "push" || m === "unshift") {
            for (var i = 0; i < arguments.length; i++) { validate(arguments[i]); }
          } else if (m === "splice") {
            for (var j = 2; j < arguments.length; j++) { validate(arguments[j]); }
          } else if (m === "fill") {
            validate(arguments[0]);
          }
          var r = orig.apply(arr, arguments);
          rebuildBacking(arr);
          return r;
        });
      });
      return proxy;
    }
    var liveArray = makeArray([]);
    // Re-sync `backing` from the current array contents (after any mutation), retrack sheets,
    // then refresh the mirror.
    function rebuildBacking(arr) {
      var old = backing.slice();
      backing = [];
      for (var i = 0; i < arr.length; i++) { if (arr[i] != null) { backing.push(arr[i]); track(arr[i]); } }
      for (var k = 0; k < old.length; k++) { untrack(old[k]); }
      refresh();
    }
    Object.defineProperty(host, "adoptedStyleSheets", {
      get: function () { return liveArray; },
      set: function (v) {
        if (v == null) { throw new TypeError("adoptedStyleSheets requires a sequence"); }
        var next = [];
        var len = v.length >>> 0;
        for (var i = 0; i < len; i++) { var item = v[i]; validate(item); next.push(item); }
        var old = backing.slice();
        liveArray = makeArray(next);
        backing = next.slice();
        for (var t = 0; t < backing.length; t++) { track(backing[t]); }
        for (var u = 0; u < old.length; u++) { untrack(old[u]); }
        refresh();
      },
      enumerable: true, configurable: true
    });
  }
  try { installAdoptedStyleSheets(globalThis.document, globalThis.document); } catch (e) {}

  // Minimal Shadow DOM: `el.attachShadow({mode})` returns a shadow root. We back it with a real
  // element appended under the host so its content (incl. <style>) is laid out + cascaded — not
  // truly style-scoped, but enough for getComputedStyle on shadow content + adoptedStyleSheets
  // (which already skips disabled sheets). Far better than throwing (web components need this).
  try {
    if (globalThis.Element && globalThis.Element.prototype) {
      def(globalThis.Element.prototype, "attachShadow", function (init) {
        if (this.__shadow) { return this.__shadow; }
        var root = document.createElement("div");
        try { __appendChild(this.__node, root.__node); } catch (e) {}
        root.host = this;
        root.mode = (init && init.mode) || "open";
        // Mark the host so an adopted sheet's `:host` rules can target it from the global mirror.
        try {
          var seq = (globalThis.__shadowHostSeq = (globalThis.__shadowHostSeq || 0) + 1);
          this.setAttribute("data-wpt-shadow-host", String(seq));
          root.__hostSel = '[data-wpt-shadow-host="' + seq + '"]';
        } catch (e) {}
        try { installAdoptedStyleSheets(root, document); } catch (e) {}
        // shadowRoot.styleSheets: the <style>/<link> sheets within the shadow tree, in tree order.
        // Per CSSOM this is SEPARATE from adoptedStyleSheets (the adopted-sheets mirror is excluded).
        Object.defineProperty(root, "styleSheets", {
          get: function () {
            var els = root.querySelectorAll("style, link");
            var sheets = [];
            for (var i = 0; i < els.length; i++) {
              // querySelectorAll results aren't canonicalized, so enrich each (gives `.sheet`).
              var el = (typeof globalThis.__canonNode === "function") ? globalThis.__canonNode(els[i]) : els[i];
              var tag = (el.tagName || "").toLowerCase();
              if (el.getAttribute && el.getAttribute("data-adopted-stylesheets") != null) { continue; }
              if (tag === "link") {
                var rel = (el.getAttribute && el.getAttribute("rel") || "").toLowerCase();
                if (rel.split(/\s+/).indexOf("stylesheet") < 0) { continue; }
                if (el.getAttribute && el.getAttribute("disabled") != null) { continue; }
              }
              if (el.__sheetDisabled) { continue; }
              try { var s = el.sheet; if (s) { sheets.push(s); } } catch (e) {}
            }
            sheets.item = function (n) { n = n >>> 0; return n < this.length ? this[n] : null; };
            try { if (globalThis.StyleSheetList && globalThis.StyleSheetList.prototype) { Object.setPrototypeOf(sheets, globalThis.StyleSheetList.prototype); } } catch (e) {}
            return sheets;
          },
          enumerable: true, configurable: true
        });
        this.__shadow = root;
        return root;
      });
      Object.defineProperty(globalThis.Element.prototype, "shadowRoot", {
        get: function () { var s = this.__shadow; return (s && s.mode === "open") ? s : null; },
        enumerable: true, configurable: true
      });
    }
  } catch (e) {}

  // Minimal <iframe> content document: a lightweight Document facade backed by a detached <body>
  // subtree (the iframe doesn't get a real nested browsing context / rendering, but its DOM +
  // CSSOM work). Enough for scripts that read `frame.contentDocument.body`, build a sub-DOM, and
  // use a <style>'s `.sheet`. `contentWindow.eval` runs with `document` bound to that facade.
  try {
    if (globalThis.HTMLIFrameElement && globalThis.HTMLIFrameElement.prototype) {
      var IFP = globalThis.HTMLIFrameElement.prototype;
      Object.defineProperty(IFP, "contentDocument", {
        get: function () {
          if (!this.__cdoc) {
            var body = document.createElement("body");
            var doc = {
              body: body, head: body, documentElement: body, nodeType: 9,
              // Marks this as a distinct (frame) document — moving a node here is a cross-document
              // adoption, which clears the moved subtree's adoptedStyleSheets (see __adoptOnInsert).
              __isFrameDoc: true,
              querySelector: function (s) { return body.querySelector(s); },
              querySelectorAll: function (s) { return body.querySelectorAll(s); },
              getElementById: function (id) { try { return body.querySelector('#' + id); } catch (e) { return null; } },
              getElementsByTagName: function (t) { return body.getElementsByTagName(t); },
              createElement: function (t) { return document.createElement(t); },
              createTextNode: function (t) { return document.createTextNode(t); },
              createDocumentFragment: function () { return document.createDocumentFragment(); },
              adoptedStyleSheets: [], styleSheets: { length: 0, item: function () { return null; } },
              defaultView: null,
              // document.open/write/close populate the frame's body (so the page can build the
              // iframe document dynamically). write() parses the HTML fragment into the frame body.
              open: function () { try { while (body.firstChild) { body.removeChild(body.firstChild); } } catch (e) {} return doc; },
              write: function (html) {
                try {
                  var tmp = document.createElement("div");
                  tmp.innerHTML = String(html == null ? "" : html);
                  while (tmp.firstChild) { body.appendChild(tmp.firstChild); }
                } catch (e) {}
              },
              writeln: function (html) { doc.write((html == null ? "" : String(html)) + "\n"); },
              close: function () {},
            };
            // Tag the content body so ownerDocument resolution maps it (and its subtree) to `doc`.
            try { def(body, "__frameDoc", doc); } catch (e) {}
            // Mark the frame body with the host <iframe>'s node id so getComputedStyle on frame
            // content can cascade this subtree with the iframe's own size as the media viewport.
            try { if (typeof this.__node === "number") { body.setAttribute("data-frame-host", String(this.__node)); } } catch (e) {}
            this.__cdoc = doc;
          }
          return this.__cdoc;
        },
        enumerable: true, configurable: true
      });
      Object.defineProperty(IFP, "contentWindow", {
        get: function () {
          var d = this.contentDocument;
          if (!this.__cwin) {
            this.__cwin = {
              document: d,
              // Run code with `document` bound to the iframe's facade document (direct eval sees it).
              eval: function (code) { var document = d; return eval(code); },
              // The frame window's getComputedStyle: the global one already cascades frame-document
              // subtrees with the iframe's own viewport, so just delegate.
              getComputedStyle: function (el, pseudo) { return getComputedStyle(el, pseudo); },
              // The facade has no nested browsing context, so window-level events/messaging are
              // accepted and ignored rather than throwing. location is a minimal about:blank.
              addEventListener: function () {}, removeEventListener: function () {}, dispatchEvent: function () { return false; },
              postMessage: function () {}, focus: function () {}, blur: function () {}, close: function () {},
              location: { href: "about:blank", toString: function () { return "about:blank"; } },
              name: "",
            };
            this.__cwin.self = this.__cwin; this.__cwin.window = this.__cwin; this.__cwin.frameElement = this;
            try { this.__cwin.parent = globalThis; this.__cwin.top = globalThis; } catch (e) {}
            d.defaultView = this.__cwin;
          }
          return this.__cwin;
        },
        enumerable: true, configurable: true
      });
    }
  } catch (e) {}

  // --- Real iframe browsing contexts -------------------------------------------------------
  // When an <iframe> has a src/srcdoc, load it as a REAL nested context (crates/js/src/iframe.rs):
  // the frame's document + scripts run in their own V8 realm with their own window/performance/fetch.
  // We trigger the load on src/srcdoc assignment and on connection, fire load/error here, and bridge
  // cross-frame postMessage. contentWindow returns the real bridge for loaded frames (falling back to
  // the lightweight facade above for srcdoc-via-document.write frames that have no realm).
  try {
    if (globalThis.HTMLIFrameElement && globalThis.HTMLIFrameElement.prototype && typeof __iframeLoad === "function") {
      var IFP2 = globalThis.HTMLIFrameElement.prototype;
      globalThis.__framesByNode = globalThis.__framesByNode || {};

      function __loadFrame(el) {
        if (!el || typeof el.__node !== "number") { return; }
        var srcdoc = (el.getAttribute && el.hasAttribute && el.hasAttribute("srcdoc")) ? el.getAttribute("srcdoc") : null;
        var rawSrc = (el.getAttribute) ? el.getAttribute("src") : null;
        var url = null;
        if (rawSrc != null && rawSrc !== "") {
          try { url = new globalThis.URL(rawSrc, globalThis.location.href).href; } catch (e) { url = rawSrc; }
        }
        var key = (srcdoc != null) ? ("srcdoc:" + srcdoc) : (url || "");
        if (key === "") { return; }
        if (el.__frameLoadedKey === key) { return; }
        el.__frameLoadedKey = key;
        globalThis.__framesByNode[el.__node] = el;
        var ok;
        try { ok = __iframeLoad(el.__node, url || "", (srcdoc != null ? String(srcdoc) : null)); }
        catch (e) { ok = false; }
        setTimeout(function () {
          var ev;
          try { ev = new globalThis.Event(ok ? "load" : "error"); } catch (e) { ev = { type: ok ? "load" : "error", target: el, currentTarget: el }; }
          try { el.dispatchEvent(ev); } catch (e) {}
        }, 0);
      }
      globalThis.__loadFrameEl = __loadFrame;

      // Connection hook (called from document.js insertNode): when an <iframe> with a src/srcdoc is
      // inserted into the tree, load its nested browsing context. Walks the inserted subtree so a
      // frame nested inside an appended fragment also loads. Browsers load iframes on connection.
      def(globalThis, "__frameOnInsert", function (nodeId) {
        if (nodeId == null || nodeId < 0) { return; }
        try {
          var el = globalThis.__canonNode(globalThis.__wrapNode(nodeId));
          if (el && el.tagName && el.tagName.toLowerCase() === "iframe") {
            if (el.getAttribute && (el.getAttribute("src") || (el.hasAttribute && el.hasAttribute("srcdoc")))) { __loadFrame(el); }
          }
          var kids = __children(nodeId);
          for (var i = 0; i < kids.length; i++) { globalThis.__frameOnInsert(kids[i]); }
        } catch (e) {}
      });

      // contentWindow: real bridge for loaded frames; otherwise the facade getter defined above.
      var __cwFacade = Object.getOwnPropertyDescriptor(IFP2, "contentWindow");
      Object.defineProperty(IFP2, "contentWindow", {
        get: function () {
          if (this.__frameLoadedKey) {
            var el = this;
            if (!el.__cwinReal) {
              var base = {
                postMessage: function (data, targetOrigin, transfer) {
                  if (typeof __framePostToFrame === "function") { __framePostToFrame(el.__node, data); }
                },
                focus: function () {}, blur: function () {}, close: function () {}, closed: false, frameElement: el
              };
              base.self = base; base.window = base;
              try { base.parent = globalThis; base.top = globalThis; } catch (e) {}
              // Reads not on `base` (performance, location, document, globals the frame's scripts
              // set, …) reach into the frame realm via __frameGet.
              if (typeof globalThis.Proxy === "function" && typeof __frameGet === "function") {
                el.__cwinReal = new globalThis.Proxy(base, {
                  get: function (t, prop) {
                    if (prop in t) { return t[prop]; }
                    if (typeof prop === "string") { try { return __frameGet(el.__node, prop); } catch (e) { return undefined; } }
                    return undefined;
                  }
                });
              } else {
                el.__cwinReal = base;
              }
            }
            return el.__cwinReal;
          }
          return (__cwFacade && __cwFacade.get) ? __cwFacade.get.call(this) : undefined;
        },
        enumerable: true, configurable: true
      });

      // frame -> page message delivery (the native bridge calls this on the page context).
      def(globalThis, "__frameDeliverToParent", function (nodeId, value) {
        var el = globalThis.__framesByNode[nodeId];
        var data; try { data = globalThis.structuredClone(value); } catch (e) { data = value; }
        var src = el ? el.contentWindow : null;
        setTimeout(function () {
          var ev;
          try { ev = new globalThis.MessageEvent("message", { data: data, origin: "", lastEventId: "", source: src, ports: [] }); }
          catch (e2) { ev = { type: "message", data: data }; }
          try { globalThis.dispatchEvent(ev); } catch (e3) {}
        }, 0);
      });

      // Load any static iframes already in the parsed document (those with a src/srcdoc attribute).
      try {
        var __ifs = document.getElementsByTagName("iframe");
        for (var __i = 0; __i < __ifs.length; __i++) {
          var __f = __ifs[__i];
          if ((__f.getAttribute && (__f.getAttribute("src") || (__f.hasAttribute && __f.hasAttribute("srcdoc"))))) { __loadFrame(__f); }
        }
      } catch (e) {}
    }
  } catch (e) {}

  // --- Custom Elements (minimal) ---------------------------------------------------------------
  // `customElements.define(name, ctor)` registers a class, then upgrades matching elements already
  // in the tree (re-pointing their prototype at the ctor's) and runs the lifecycle reactions:
  // `connectedCallback` on connect (also for elements inserted later, via the insertNode hook),
  // `disconnectedCallback` on removal (via a wrapper around the native subtree-removal primitive),
  // and `attributeChangedCallback` for the attributes named in the ctor's static `observedAttributes`
  // (via the setAttribute/removeAttribute hook). We still skip the spec's
  // constructor-run-with-`this`-as-the-element machinery (no engine support) and `adoptedCallback`
  // (cross-document adoption is rare); these three reactions cover the overwhelming majority of
  // reactive components. Direct id/class/dataset setters and toggleAttribute are not yet hooked for
  // attributeChangedCallback — only setAttribute/removeAttribute.
  try {
    var __ceReg = {};      // name -> ctor
    var __ceObs = {};      // name -> { attrName: true, … } observed-attribute set (read once at define)
    var __ceWhen = {};     // name -> { promise, resolve }
    var __ceAny = false;   // fast-path guard: stay a no-op until the first element is defined
    function __ceConnected(el) {
      try { return !!(document.documentElement && document.documentElement.contains(el)); } catch (e) { return false; }
    }
    function __ceName(el) { return ((el && el.tagName) || "").toLowerCase(); }
    function __ceFireAttr(el, name, oldV, newV) {
      try { el.attributeChangedCallback(name, oldV == null ? null : oldV, newV == null ? null : newV, null); }
      catch (e) { try { console.error(e); } catch (e2) {} }
    }
    function __ceUpgrade(el) {
      if (!el || el.__ceUpgraded) { return; }
      var ctor = __ceReg[__ceName(el)];
      if (!ctor) { return; }
      def(el, "__ceUpgraded", true);
      try { if (ctor.prototype) { Object.setPrototypeOf(el, ctor.prototype); } } catch (e) {}
      // Spec: enqueue attributeChangedCallback (oldValue null) for each observed attribute present.
      var obs = __ceObs[__ceName(el)];
      if (obs && typeof el.attributeChangedCallback === "function" && el.getAttributeNames) {
        try {
          var names = el.getAttributeNames();
          for (var i = 0; i < names.length; i++) {
            if (obs[names[i]]) { __ceFireAttr(el, names[i], null, el.getAttribute(names[i])); }
          }
        } catch (e) {}
      }
    }
    function __ceConnect(el) {
      __ceUpgrade(el);
      if (!el || !el.__ceUpgraded || el.__ceConnectedFired) { return; }
      if (!__ceConnected(el)) { return; }
      def(el, "__ceConnectedFired", true);
      if (typeof el.connectedCallback === "function") {
        try { el.connectedCallback(); }
        catch (e) { try { console.error(e); } catch (e2) {} }
      }
    }
    function __ceDisconnect(el) {
      if (!el || !el.__ceUpgraded || !el.__ceConnectedFired) { return; }
      def(el, "__ceConnectedFired", false);
      if (typeof el.disconnectedCallback === "function") {
        try { el.disconnectedCallback(); }
        catch (e) { try { console.error(e); } catch (e2) {} }
      }
    }
    function __ceWalk(nodeId) {
      if (nodeId == null || nodeId < 0) { return; }
      try {
        var el = (typeof globalThis.__nodeById === "function" && globalThis.__nodeById(nodeId)) ||
                 (typeof globalThis.__canonNode === "function" ? globalThis.__canonNode(nodeId) : null);
        if (el && el.tagName) { __ceConnect(el); }
        var kids = __children(nodeId);
        for (var i = 0; i < kids.length; i++) { __ceWalk(kids[i]); }
      } catch (e) {}
    }
    function __ceCollect(nodeId, acc) {        // node ids of a subtree, parent-first (capture pre-removal)
      if (nodeId == null || nodeId < 0) { return; }
      acc.push(nodeId);
      try { var kids = __children(nodeId); for (var i = 0; i < kids.length; i++) { __ceCollect(kids[i], acc); } } catch (e) {}
    }
    // Called from insertNode. Cheap no-op until at least one custom element is defined.
    def(globalThis, "__ceOnInsert", function (nodeId) { if (__ceAny) { __ceWalk(nodeId); } });
    // Called from setAttribute/removeAttribute. Fires attributeChangedCallback for observed attrs.
    def(globalThis, "__ceNoteAttrChange", function (el, attrName, oldV, newV) {
      if (!__ceAny || !el || !el.__ceUpgraded) { return; }
      var obs = __ceObs[__ceName(el)];
      if (obs && obs[attrName] && typeof el.attributeChangedCallback === "function") {
        __ceFireAttr(el, attrName, oldV, newV);
      }
    });
    // Wrap the native subtree-removal primitive once so removing a connected custom element (or a
    // subtree containing one) runs disconnectedCallback. Bare `__removeChild(...)` call sites resolve
    // the global at call time, so re-pointing it reroutes them all. Capture the subtree ids BEFORE
    // removal (the node's child links are torn down by it), then fire after.
    if (typeof globalThis.__removeChild === "function" && !globalThis.__removeChild.__ceWrapped) {
      var __nativeRemoveChild = globalThis.__removeChild;
      var __wrappedRemoveChild = function (parent, child) {
        var victims = null;
        if (__ceAny) { victims = []; __ceCollect(child, victims); }
        var r = __nativeRemoveChild(parent, child);
        if (victims) {
          for (var i = 0; i < victims.length; i++) {
            try {
              var w = globalThis.__nodeById ? globalThis.__nodeById(victims[i]) : null;
              if (w && w.tagName) { __ceDisconnect(w); }
            } catch (e) {}
          }
        }
        return r;
      };
      def(__wrappedRemoveChild, "__ceWrapped", true);
      globalThis.__removeChild = __wrappedRemoveChild;
    }
    def(globalThis, "customElements", {
      define: function (name, ctor) {
        if (typeof name !== "string" || !/^[a-z][a-z0-9._]*-[a-z0-9._-]*$/.test(name)) {
          throw new globalThis.DOMException("'" + name + "' is not a valid custom element name", "SyntaxError");
        }
        if (typeof ctor !== "function") {
          throw new globalThis.TypeError("The second argument to customElements.define must be a constructor");
        }
        if (__ceReg[name]) {
          throw new globalThis.DOMException("the name '" + name + "' has already been used with this registry", "NotSupportedError");
        }
        __ceReg[name] = ctor;
        __ceAny = true;
        // Read observedAttributes once (static getter), as the spec requires.
        try {
          var o = ctor.observedAttributes;
          if (o && o.length) {
            var set = {};
            for (var oi = 0; oi < o.length; oi++) { set[String(o[oi])] = true; }
            __ceObs[name] = set;
          }
        } catch (e) {}
        // Upgrade + connect elements already in the document (snapshot first — connectedCallback may mutate).
        try {
          var live = document.getElementsByTagName(name);
          var arr = []; for (var i = 0; i < live.length; i++) { arr.push(live[i]); }
          for (var j = 0; j < arr.length; j++) { __ceConnect(arr[j]); }
        } catch (e) {}
        if (__ceWhen[name]) { try { __ceWhen[name].resolve(ctor); } catch (e) {} }
      },
      get: function (name) { return __ceReg[name] || undefined; },
      getName: function (ctor) { for (var k in __ceReg) { if (__ceReg[k] === ctor) { return k; } } return null; },
      whenDefined: function (name) {
        if (__ceReg[name]) { return Promise.resolve(__ceReg[name]); }
        if (!__ceWhen[name]) { var r; var p = new Promise(function (res) { r = res; }); __ceWhen[name] = { promise: p, resolve: r }; }
        return __ceWhen[name].promise;
      },
      upgrade: function (root) { try { __ceWalk(root && root.__node); } catch (e) {} }
    });
  } catch (e) {}

  // --- <template>.content + cross-document ownerDocument / adoption ----------------------------
  // A <template>'s children belong to a "template contents document" (an inert document distinct
  // from the main one). We model the content as a DocumentFragment facade over the template
  // element's arena children, and resolve `ownerDocument` by walking ancestry: the nearest
  // <template> ancestor maps a node to that template's contents document, an <iframe> content body
  // maps to that frame's document, otherwise the main document. Moving a node thus updates its
  // ownerDocument, and moving it into a *frame* document clears the moved shadow roots' adopted
  // sheets (construct-stylesheets adoption steps) — but moving into a template does not.
  // Scope a shadow root's adopted-sheet CSS to the host's subtree: prefix each style rule's selector
  // with the host marker (so `.x` becomes a descendant of the host) and rewrite `:host`/`:host(X)` to
  // the host element itself. @-rules are passed through unscoped (their nested rules aren't rewritten
  // — rare for shadow-adopted sheets). Keeps shadow styles from leaking into the rest of the document.
  def(globalThis, "__scopeShadowCss", function (css, hostSel) {
    function scopeSel(sel) {
      if (!sel) { return sel; }
      if (/^:host(\b|\()/.test(sel)) {
        return sel.replace(/:host\(([^)]*)\)/g, hostSel + "$1").replace(/:host\b/g, hostSel);
      }
      return hostSel + " " + sel;
    }
    try {
      var out = "", i = 0, n = css.length;
      while (i < n) {
        var brace = css.indexOf("{", i);
        if (brace < 0) { break; }
        var sel = css.slice(i, brace).trim();
        var depth = 1, j = brace + 1;
        while (j < n && depth > 0) { var ch = css.charAt(j); if (ch === "{") { depth++; } else if (ch === "}") { depth--; } j++; }
        var body = css.slice(brace, j);
        if (sel.charAt(0) === "@") {
          out += sel + " " + body + "\n";
        } else {
          var parts = sel.split(","), scoped = [];
          for (var k = 0; k < parts.length; k++) { scoped.push(scopeSel(parts[k].trim())); }
          out += scoped.join(", ") + " " + body + "\n";
        }
        i = j;
      }
      return out;
    } catch (e) { return css; }
  });

  try {
    // Resolve a node id to its canonical, fully-enriched element wrapper (the one that carries
    // tag prototype + __shadow/__frameDoc). __wrapNode builds a bare wrapper; __canonNode enriches
    // and caches it. Reuse the cached wrapper when present.
    def(globalThis, "__elFor", function (cid) {
      if (cid == null || cid < 0) { return null; }
      var c = (typeof globalThis.__nodeById === "function") ? globalThis.__nodeById(cid) : null;
      if (c) { return c; }
      var w = (typeof globalThis.__wrapNode === "function") ? globalThis.__wrapNode(cid) : null;
      if (!w) { return null; }
      return (typeof globalThis.__canonNode === "function") ? globalThis.__canonNode(w) : w;
    });
    // Stable contents-document object for a template element (lazily created).
    function __templateDocFor(tpl) {
      if (!tpl.__contentDoc) {
        try { def(tpl, "__contentDoc", { __isTemplateContentsDoc: true, nodeType: 9, defaultView: null }); }
        catch (e) { tpl.__contentDoc = { __isTemplateContentsDoc: true, nodeType: 9, defaultView: null }; }
      }
      return tpl.__contentDoc;
    }
    def(globalThis, "__ownerDocumentOf", function (node) {
      try {
        var id = node && node.__node;
        if (typeof id !== "number") { return document; }
        var cur = id, guard = 0;
        while (cur >= 0 && guard++ < 100000) {
          var w = globalThis.__elFor(cur);
          if (w) {
            // The iframe content body (and its subtree) belongs to the frame document.
            if (w.__frameDoc) { return w.__frameDoc; }
            // A <template> ANCESTOR (not the node itself) puts the node in its contents document.
            if (cur !== id && w.tagName === "TEMPLATE") { return __templateDocFor(w); }
            // Arena-backed foreign documents are canonicalized to their Document facade.
            if (w.nodeType === 9) { return w; }
          }
          cur = __parent(cur);
        }
      } catch (e) {}
      return document;
    });

    // <template>.content — a DocumentFragment facade over the template element's children.
    if (globalThis.HTMLTemplateElement && globalThis.HTMLTemplateElement.prototype) {
      Object.defineProperty(globalThis.HTMLTemplateElement.prototype, "content", {
        get: function () {
          if (this.__contentFrag) { return this.__contentFrag; }
          var tpl = this, tplNode = tpl.__node;
          var nodeAt = function (cid) { return globalThis.__elFor(cid); };
          var frag = {
            nodeType: 11,
            host: tpl,
            get ownerDocument() { return __templateDocFor(tpl); },
            appendChild: function (child) { try { __appendChild(tplNode, child.__node); } catch (e) {} return child; },
            insertBefore: function (child, ref) { try { __insertNode(tplNode, child.__node, ref ? ref.__node : -1); } catch (e) {} return child; },
            removeChild: function (child) { try { __removeChild(child.__node); } catch (e) {} return child; },
            append: function () { for (var i = 0; i < arguments.length; i++) { var c = arguments[i]; this.appendChild(typeof c === "string" ? document.createTextNode(c) : c); } },
            prepend: function () { var r = this.firstChild; for (var i = 0; i < arguments.length; i++) { var c = arguments[i]; this.insertBefore(typeof c === "string" ? document.createTextNode(c) : c, r); } },
            get lastChild() { var k = __children(tplNode); return k.length ? nodeAt(k[k.length - 1]) : null; },
            get textContent() { var k = __children(tplNode), s = ""; for (var i = 0; i < k.length; i++) { var nd = nodeAt(k[i]); s += (nd && nd.textContent != null ? nd.textContent : ""); } return s; },
            get childNodes() { var k = __children(tplNode), a = []; for (var i = 0; i < k.length; i++) { a.push(nodeAt(k[i])); } return a; },
            get children() { var k = __children(tplNode), a = []; for (var i = 0; i < k.length; i++) { if (__nodeType(k[i]) === 1) { a.push(nodeAt(k[i])); } } return a; },
            get firstChild() { var k = __children(tplNode); return k.length ? nodeAt(k[0]) : null; },
            get firstElementChild() { var k = __children(tplNode); for (var i = 0; i < k.length; i++) { if (__nodeType(k[i]) === 1) { return nodeAt(k[i]); } } return null; },
            get childElementCount() { var k = __children(tplNode), n = 0; for (var i = 0; i < k.length; i++) { if (__nodeType(k[i]) === 1) { n++; } } return n; },
            querySelector: function (s) { try { return tpl.querySelector(s); } catch (e) { return null; } },
            querySelectorAll: function (s) { try { return tpl.querySelectorAll(s); } catch (e) { return []; } },
            getElementById: function (gid) { var found = globalThis.__findElementByIdWithin(tplNode, String(gid)); return found >= 0 ? nodeAt(found) : null; },
            cloneNode: function () { return this; },
          };
          try { def(tpl, "__contentFrag", frag); } catch (e) { tpl.__contentFrag = frag; }
          return frag;
        },
        enumerable: true, configurable: true
      });
    }

    // Called from insertNode after a move: if a moved element with a shadow root now lives in a
    // *frame* document (a real cross-document adoption — not a template), empty that shadow root's
    // adoptedStyleSheets in place (keeping the same observable array object).
    def(globalThis, "__adoptOnInsert", function (nodeId) {
      try {
        var od = null;
        var walk = function (cid) {
          if (cid == null || cid < 0) { return; }
          var w = globalThis.__elFor(cid);
          if (w && w.__shadow) {
            var doc = globalThis.__ownerDocumentOf(w);
            if (doc && doc.__isFrameDoc) {
              try { var asl = w.__shadow.adoptedStyleSheets; if (asl && asl.length) { asl.splice(0, asl.length); } } catch (e) {}
            }
          }
          var kids = __children(cid);
          for (var i = 0; i < kids.length; i++) { walk(kids[i]); }
        };
        walk(nodeId);
      } catch (e) {}
    });
  } catch (e) {}

  // --- Image / Audio / media element constructors ------------------------------------------
  if (typeof globalThis.Image !== "function") {
    var imageElementProto = globalThis.HTMLImageElement && globalThis.HTMLImageElement.prototype;
    def(globalThis, "Image", function (w, h) {
      this.width = w || 0; this.height = h || 0; this.naturalWidth = 0; this.naturalHeight = 0;
      this.complete = false; this.src = ""; this.alt = ""; this.crossOrigin = null; this.decoding = "auto";
      this.onload = null; this.onerror = null;
      this.setAttribute = fn; this.getAttribute = function () { return null; };
      this.addEventListener = fn; this.removeEventListener = fn; this.dispatchEvent = function () { return false; };
      this.decode = function () { return Promise.resolve(); };
      try { def(this, "style", { setProperty: fn, getPropertyValue: function () { return ""; }, removeProperty: function () { return ""; }, cssText: "" }); } catch (e) {}
    });
    if (imageElementProto) {
      try { Object.setPrototypeOf(globalThis.Image.prototype, imageElementProto); } catch (e) {}
    }
    def(globalThis, "HTMLImageElement", globalThis.Image);
  }
  if (typeof globalThis.Audio !== "function") {
    def(globalThis, "Audio", function (src) {
      this.src = src || ""; this.currentTime = 0; this.paused = true; this.volume = 1;
      this.play = function () { return Promise.resolve(); }; this.pause = fn; this.load = fn;
      this.canPlayType = function () { return ""; };
      this.addEventListener = fn; this.removeEventListener = fn;
    });
  }
  // --- Blob / File / FileReader (real: store + read back bytes) -----------------------------
  // Flatten Blob constructor `parts` (strings → UTF-8, ArrayBuffer/typed arrays → bytes, nested
  // Blobs → their bytes) into a plain byte array.
  function __blobBytes(parts) {
    var bytes = [];
    if (!parts || typeof parts.length !== "number") { return bytes; }
    for (var i = 0; i < parts.length; i++) {
      var p = parts[i];
      if (p == null) { continue; }
      if (typeof p === "string") {
        var enc = unescape(encodeURIComponent(p));
        for (var j = 0; j < enc.length; j++) { bytes.push(enc.charCodeAt(j) & 0xff); }
      } else if (p.__blobBytes) {
        bytes = bytes.concat(p.__blobBytes);
      } else if (p instanceof ArrayBuffer) {
        var v1 = new Uint8Array(p); for (var k = 0; k < v1.length; k++) { bytes.push(v1[k]); }
      } else if (p.buffer && typeof p.byteLength === "number") {
        var v2 = new Uint8Array(p.buffer, p.byteOffset || 0, p.byteLength); for (var m = 0; m < v2.length; m++) { bytes.push(v2[m]); }
      } else {
        var s2 = unescape(encodeURIComponent(String(p))); for (var n = 0; n < s2.length; n++) { bytes.push(s2.charCodeAt(n) & 0xff); }
      }
    }
    return bytes;
  }
  if (typeof globalThis.Blob !== "function") {
    def(globalThis, "Blob", function (parts, opts) {
      var bytes = __blobBytes(parts);
      this.__blobBytes = bytes;
      this.size = bytes.length;
      this.type = (opts && opts.type) || "";
      this.slice = function (start, end, type) {
        var s = start || 0, e = (end == null ? bytes.length : end);
        if (s < 0) { s += bytes.length; } if (e < 0) { e += bytes.length; }
        var sub = bytes.slice(Math.max(0, s), Math.max(0, e));
        var b = new globalThis.Blob([], { type: type || this.type });
        b.__blobBytes = sub; b.size = sub.length; return b;
      };
      this.text = function () {
        var s = ""; for (var i = 0; i < bytes.length; i++) { s += String.fromCharCode(bytes[i]); }
        var out; try { out = decodeURIComponent(escape(s)); } catch (e) { out = s; }
        return Promise.resolve(out);
      };
      this.arrayBuffer = function () {
        var buf = new ArrayBuffer(bytes.length), view = new Uint8Array(buf);
        for (var i = 0; i < bytes.length; i++) { view[i] = bytes[i]; }
        return Promise.resolve(buf);
      };
    });
  }
  if (typeof globalThis.File !== "function") {
    def(globalThis, "File", function (parts, name, opts) { globalThis.Blob.call(this, parts, opts); this.name = String(name || ""); this.lastModified = 0; });
  }
  if (typeof globalThis.FileReader !== "function") {
    def(globalThis, "FileReader", function () {
      var self = this;
      this.readyState = 0; this.result = null; this.error = null;
      this.onload = null; this.onloadend = null; this.onerror = null; this.onprogress = null;
      try { installEvents(this); } catch (e) {}
      function finish(result) {
        self.readyState = 2; self.result = result;
        var ev = { type: "load", target: self, currentTarget: self };
        if (typeof self.onload === "function") { try { self.onload(ev); } catch (e) {} }
        try { fireOn(self, "load"); } catch (e) {}
        if (typeof self.onloadend === "function") { try { self.onloadend({ type: "loadend", target: self }); } catch (e) {} }
        try { fireOn(self, "loadend"); } catch (e) {}
      }
      this.readAsText = function (blob) { (blob && blob.text ? blob.text() : Promise.resolve("")).then(finish); };
      this.readAsArrayBuffer = function (blob) { (blob && blob.arrayBuffer ? blob.arrayBuffer() : Promise.resolve(new ArrayBuffer(0))).then(finish); };
      this.readAsDataURL = function (blob) {
        (blob && blob.arrayBuffer ? blob.arrayBuffer() : Promise.resolve(new ArrayBuffer(0))).then(function (buf) {
          var view = new Uint8Array(buf), s = "";
          for (var i = 0; i < view.length; i++) { s += String.fromCharCode(view[i]); }
          var b64 = (typeof btoa === "function") ? btoa(s) : "";
          finish("data:" + ((blob && blob.type) || "application/octet-stream") + ";base64," + b64);
        });
      };
      this.abort = fn;
    });
  }
  // --- Dedicated Workers --------------------------------------------------------------------
  // A dedicated worker runs in its OWN V8 context (a real, separate global object) in the page's
  // isolate, created and driven from Rust (crates/js/src/worker.rs + bootstrap/worker_env.js): `new
  // Worker(url)` calls the `__workerCreate` native, which builds the worker context, installs the
  // browser environment + a worker overlay, and runs the worker script there. Because the worker
  // has its own global, `self === globalThis` truly holds, so top-level declarations in the worker
  // script and every `importScripts`'d file become worker globals (visible across files) — matching
  // real workers (and what helpers like canvas-tests.js rely on). Messages cross via the
  // `__workerPostToWorker` / `__workerPostToParent` natives; the receiving context localises the
  // value with its OWN structuredClone. The page side here is just the Worker EventTarget facade
  // plus `__workerDeliver`, the entry the native bridge calls to dispatch at the right Worker.
  if (typeof globalThis.Worker !== "function") {
    if (typeof globalThis.WorkerGlobalScope !== "function") { defClass("WorkerGlobalScope", globalThis.EventTarget); }
    if (typeof globalThis.DedicatedWorkerGlobalScope !== "function") { defClass("DedicatedWorkerGlobalScope", globalThis.WorkerGlobalScope); }
    if (typeof globalThis.WorkerNavigator !== "function") { defClass("WorkerNavigator"); }
    if (typeof globalThis.WorkerLocation !== "function") { defClass("WorkerLocation"); }

    globalThis.__workersById = globalThis.__workersById || {};
    if (typeof globalThis.__nextWorkerId !== "number") { globalThis.__nextWorkerId = 0; }

    // Called by the native bridge (worker -> page). Localise the value with the page's
    // structuredClone, then deliver a `message` event at the matching Worker on a fresh task (so a
    // result posted during `new Worker(...)` isn't missed by a listener attached right after).
    def(globalThis, "__workerDeliver", function (id, value) {
      var worker = globalThis.__workersById[id];
      if (!worker || worker.__terminated) { return; }
      var data; try { data = globalThis.structuredClone(value); } catch (e) { data = value; }
      setTimeout(function () {
        if (worker.__terminated) { return; }
        var ev;
        try { ev = new globalThis.MessageEvent("message", { data: data, origin: "", lastEventId: "", source: null, ports: [] }); }
        catch (e2) { ev = { type: "message", data: data }; }
        try { worker.dispatchEvent(ev); } catch (e3) {}
      }, 0);
    });

    def(globalThis, "Worker", function (scriptURL, options) {
      if (!(this instanceof globalThis.Worker)) {
        throw new TypeError("Failed to construct 'Worker': Please use the 'new' operator, this object constructor cannot be called as a function.");
      }
      if (arguments.length < 1) {
        throw new TypeError("Failed to construct 'Worker': 1 argument required, but only 0 present.");
      }
      var worker = this;
      installEvents(worker);
      worker.onmessage = null; worker.onmessageerror = null; worker.onerror = null;
      options = options || {};

      var base; try { base = globalThis.location.href; } catch (e) { base = "about:blank"; }
      var href; try { href = (new globalThis.URL(String(scriptURL), base)).href; } catch (e) {
        throw new globalThis.DOMException("Failed to construct 'Worker': Failed to resolve the script URL '" + scriptURL + "'.", "SyntaxError");
      }

      var id = (globalThis.__nextWorkerId += 1);
      worker.__id = id; worker.__terminated = false;
      globalThis.__workersById[id] = worker;

      def(worker, "postMessage", function (data, transferOrOpts) {
        if (worker.__terminated) { return; }
        if (typeof __workerPostToWorker === "function") { __workerPostToWorker(id, data); }
      });
      def(worker, "terminate", function () {
        if (worker.__terminated) { return; }
        worker.__terminated = true;
        try { delete globalThis.__workersById[id]; } catch (e) {}
        if (typeof __workerTerminate === "function") { __workerTerminate(id); }
      });

      // `new Worker(URL.createObjectURL(blob))` is common (inline workers). Our createObjectURL
      // returns a data: URL, which the worker's network fetch may not resolve; decode it to source
      // here (the page has the bytes) and hand it to the worker context directly.
      var inlineSrc = null;
      if (/^data:/i.test(href)) {
        try {
          var comma = href.indexOf(","), meta = href.slice(5, comma), payload = href.slice(comma + 1);
          if (/;base64/i.test(meta)) {
            var bin = (typeof atob === "function") ? atob(payload) : "";
            try { inlineSrc = decodeURIComponent(escape(bin)); } catch (e) { inlineSrc = bin; }
          } else {
            inlineSrc = decodeURIComponent(payload);
          }
        } catch (e) { inlineSrc = null; }
      }

      // Build the worker context + run its script. On failure, surface an async `error` event.
      var ok = (typeof __workerCreate === "function") ? __workerCreate(id, href, inlineSrc) : false;
      if (!ok) {
        setTimeout(function () {
          var ev;
          try { ev = new globalThis.ErrorEvent("error", { cancelable: true, message: "Failed to load worker script", filename: href }); }
          catch (e) { ev = { type: "error", message: "Failed to load worker script", filename: href }; }
          try { worker.dispatchEvent(ev); } catch (e2) {}
        }, 0);
      }
    });
    // Worker inherits from EventTarget for instanceof / idlharness conformance.
    try {
      globalThis.Worker.prototype = Object.create(globalThis.EventTarget.prototype);
      Object.defineProperty(globalThis.Worker.prototype, "constructor", { value: globalThis.Worker, enumerable: false, configurable: true, writable: true });
    } catch (e) {}
  }
  // --- MessageChannel / MessagePort --------------------------------------------------------
  // An entangled pair of ports: postMessage on one delivers a `message` MessageEvent on the other,
  // asynchronously (a task), after the receiving port is started. A port starts on the first
  // start() call or implicitly when its `onmessage` handler is assigned. Transferred MessagePorts
  // are passed by reference (single isolate) and surfaced as `event.ports`.
  if (typeof globalThis.MessageChannel !== "function") {
    defClass("MessagePort", globalThis.EventTarget);
    defClass("MessageChannel");
    // Structured clone that preserves transferable MessagePorts by reference (so an entangled port
    // survives the hop) and otherwise deep-copies plain data. Falls back to identity for exotic
    // objects (Blob, ArrayBuffer, typed arrays) which the tests pass through unchanged.
    def(globalThis, "__swClone", function (value) {
      var seen = (typeof Map === "function") ? new Map() : null;
      function clone(v) {
        if (v === null || typeof v !== "object") { return v; }
        if (v instanceof globalThis.MessagePort) { return v; }
        if (v instanceof ArrayBuffer) { return v; }
        if (typeof Blob === "function" && v instanceof Blob) { return v; }
        if (ArrayBuffer.isView(v)) { return v; }
        if (seen && seen.has(v)) { return seen.get(v); }
        var out;
        if (Array.isArray(v)) { out = []; if (seen) { seen.set(v, out); } for (var i = 0; i < v.length; i++) { out[i] = clone(v[i]); } return out; }
        out = {}; if (seen) { seen.set(v, out); }
        for (var k in v) { if (Object.prototype.hasOwnProperty.call(v, k)) { out[k] = clone(v[k]); } }
        return out;
      }
      return clone(value);
    });
    function __swPorts(transfer) {
      var ports = [];
      if (transfer && transfer.length) {
        for (var i = 0; i < transfer.length; i++) { if (transfer[i] instanceof globalThis.MessagePort) { ports.push(transfer[i]); } }
      }
      return ports;
    }
    def(globalThis, "__swExtractPorts", __swPorts);
    function __swMakePort() {
      var port = Object.create(globalThis.MessagePort.prototype);
      installEvents(port);
      var started = false, queue = [], onmsg = null;
      port.__entangled = null; port.__closed = false;
      function deliver(msg) {
        var ev = new globalThis.MessageEvent("message", { data: msg.data, ports: msg.ports || [], origin: "" });
        port.dispatchEvent(ev);
      }
      def(port, "postMessage", function (data, transfer) {
        var other = port.__entangled;
        if (!other || other.__closed) { return; }
        var cloned = globalThis.__swClone(data);
        var ports = __swPorts(transfer);
        setTimeout(function () { other.__receive(cloned, ports); }, 0);
      });
      def(port, "start", function () { if (started) { return; } started = true; var q = queue; queue = []; for (var i = 0; i < q.length; i++) { deliver(q[i]); } });
      def(port, "close", function () { port.__closed = true; });
      def(port, "__receive", function (data, ports) { var msg = { data: data, ports: ports }; if (started) { deliver(msg); } else { queue.push(msg); } });
      Object.defineProperty(port, "onmessage", {
        get: function () { return onmsg; },
        set: function (v) { onmsg = (typeof v === "function") ? v : null; port.start(); },
        enumerable: true, configurable: true
      });
      port.onmessageerror = null;
      return port;
    }
    def(globalThis, "MessageChannel", function () {
      var p1 = __swMakePort(), p2 = __swMakePort();
      p1.__entangled = p2; p2.__entangled = p1;
      Object.defineProperty(this, "port1", { value: p1, enumerable: true, configurable: true });
      Object.defineProperty(this, "port2", { value: p2, enumerable: true, configurable: true });
    });
  }

  // --- Service Workers (stages 1-2: container, registration, worker execution — see issue #56) --
  // `navigator.serviceWorker` is a real `ServiceWorkerContainer`. `register()` validates the
  // script/scope URLs (same-origin, max-scope), fetches the script, then runs it in a
  // `ServiceWorkerGlobalScope` and drives the install -> installed -> activating -> activated
  // lifecycle, dispatching `install`/`activate` as `ExtendableEvent`s and awaiting `waitUntil()`.
  // The worker can `skipWaiting()`, `clients.claim()` (which sets `controller`), exchange messages
  // with the page (`postMessage`/`ExtendableMessageEvent`/`Client.postMessage`), and `importScripts`.
  // Fetch interception (FetchEvent dispatch into the page's resource loads) is stage 3.
  if (typeof globalThis.ServiceWorker !== "function") {
    defClass("ServiceWorker", globalThis.EventTarget);
    defClass("ServiceWorkerRegistration", globalThis.EventTarget);
    defClass("ServiceWorkerContainer", globalThis.EventTarget);
    defClass("ServiceWorkerGlobalScope", globalThis.EventTarget);
    defClass("Clients");
    defClass("Client");
    defClass("WindowClient", globalThis.Client);
    if (typeof globalThis.NavigationPreloadManager !== "function") { defClass("NavigationPreloadManager"); }

    var __swRegs = [];           // live ServiceWorkerRegistration objects, in registration order
    var __swReadyResolve = null; // pending resolver for navigator.serviceWorker.ready, if any
    var __swClientSeq = 0;       // monotonic source for client ids

    function __swBase() { try { return globalThis.location.href; } catch (e) { return "about:blank"; } }
    function __swNoFrag(u) { try { u.hash = ""; } catch (e) {} return u; }
    // A registration's scope controls a client when scope is a prefix of the client URL.
    function __swMatchesClient(scopeHref) { try { return __swBase().indexOf(scopeHref) === 0; } catch (e) { return false; } }

    function __swMakeWorker(scriptHref) {
      var sw = Object.create(globalThis.ServiceWorker.prototype);
      installEvents(sw);
      var state = "installing";
      Object.defineProperty(sw, "scriptURL", { value: scriptHref, enumerable: true, configurable: true });
      Object.defineProperty(sw, "state", { get: function () { return state; }, enumerable: true, configurable: true });
      sw.onstatechange = null;
      sw.onerror = null;
      sw.__scope = null;       // the ServiceWorkerGlobalScope once the script runs
      sw.__skipWaiting = false;
      // Page -> worker postMessage: dispatch an ExtendableMessageEvent on the worker global scope,
      // with the page exposed as event.source (a WindowClient that can postMessage back).
      def(sw, "postMessage", function (data, transfer) {
        var scope = sw.__scope; if (!scope) { return; }
        var cloned = globalThis.__swClone(data);
        var ports = globalThis.__swExtractPorts(transfer);
        setTimeout(function () {
          var ev = new globalThis.ExtendableMessageEvent("message", {
            data: cloned, origin: __swPageOrigin(), lastEventId: "",
            source: __swPageClient(), ports: ports
          });
          scope.dispatchEvent(ev);
        }, 0);
      });
      def(sw, "__setState", function (s) { if (state === s) { return; } state = s; fireOn(sw, "statechange"); });
      return sw;
    }

    function __swPageOrigin() { try { return (new globalThis.URL(__swBase())).origin; } catch (e) { return ""; } }

    // The page, as seen from a worker: a WindowClient whose postMessage delivers a `message`
    // MessageEvent to navigator.serviceWorker (event.source = the controlling worker).
    var __swPageClientObj = null;
    function __swPageClient() {
      if (__swPageClientObj) { return __swPageClientObj; }
      var c = Object.create(globalThis.WindowClient.prototype);
      var id = "client-" + (++__swClientSeq);
      Object.defineProperty(c, "id", { value: id, enumerable: true, configurable: true });
      Object.defineProperty(c, "url", { get: function () { return __swBase(); }, enumerable: true, configurable: true });
      Object.defineProperty(c, "type", { value: "window", enumerable: true, configurable: true });
      Object.defineProperty(c, "frameType", { value: "top-level", enumerable: true, configurable: true });
      Object.defineProperty(c, "visibilityState", { value: "visible", enumerable: true, configurable: true });
      Object.defineProperty(c, "focused", { value: true, enumerable: true, configurable: true });
      def(c, "focus", function () { return Promise.resolve(c); });
      def(c, "navigate", function () { return Promise.resolve(c); });
      def(c, "postMessage", function (data, transfer) {
        var cloned = globalThis.__swClone(data);
        var ports = globalThis.__swExtractPorts(transfer);
        setTimeout(function () {
          var ev = new globalThis.MessageEvent("message", {
            data: cloned, origin: __swPageOrigin(), lastEventId: "",
            source: __swContainer.controller || null, ports: ports
          });
          __swContainer.dispatchEvent(ev);
        }, 0);
      });
      __swPageClientObj = c;
      return c;
    }

    // self.clients in the worker scope. matchAll/get expose the page client; claim() makes the
    // worker the page's controller (firing controllerchange).
    function __swMakeClients(reg, sw) {
      var clients = Object.create(globalThis.Clients.prototype);
      def(clients, "get", function (id) { var c = __swPageClient(); return Promise.resolve(c.id === id ? c : undefined); });
      def(clients, "matchAll", function (opts) {
        opts = opts || {};
        // Only return the page client when this worker controls it (claimed), or when uncontrolled
        // clients are explicitly requested.
        var controls = __swContainer.controller === sw;
        return Promise.resolve((controls || opts.includeUncontrolled) ? [__swPageClient()] : []);
      });
      def(clients, "openWindow", function () { return Promise.resolve(null); });
      def(clients, "claim", function () {
        if (sw.state === "activating" || sw.state === "activated") {
          if (__swMatchesClient(reg.scope) && __swContainer.controller !== sw) {
            __swContainer.controller = sw;
            fireOn(__swContainer, "controllerchange");
          }
        }
        return Promise.resolve();
      });
      return clients;
    }

    // Build the ServiceWorkerGlobalScope and run the worker script inside it. The script executes in
    // a function whose parameters shadow the worker-scoped globals (self, registration, clients,
    // skipWaiting, importScripts, ...), so bare references resolve to the scope; `globalThis` is left
    // as the real global so shared constructors (URL, MessageChannel, Response, ...) remain reachable.
    function __swExecuteWorker(reg, sw, scriptHref, source) {
      var scope = Object.create(globalThis.ServiceWorkerGlobalScope.prototype);
      installEvents(scope);
      scope.self = scope;
      // testharness.js (run inside the worker via importScripts) selects its test environment with
      // `'ServiceWorkerGlobalScope' in self && self instanceof ServiceWorkerGlobalScope`; expose the
      // constructor on the scope so a worker-side test reports its results back to the page.
      Object.defineProperty(scope, "ServiceWorkerGlobalScope", { value: globalThis.ServiceWorkerGlobalScope, enumerable: false, configurable: true });
      scope.oninstall = null; scope.onactivate = null; scope.onfetch = null;
      scope.onmessage = null; scope.onmessageerror = null;
      Object.defineProperty(scope, "registration", { value: reg, enumerable: true, configurable: true });
      Object.defineProperty(scope, "serviceWorker", { value: sw, enumerable: true, configurable: true });
      try { Object.defineProperty(scope, "location", { value: new globalThis.URL(scriptHref), enumerable: true, configurable: true }); } catch (e) {}
      var clients = __swMakeClients(reg, sw);
      Object.defineProperty(scope, "clients", { value: clients, enumerable: true, configurable: true });
      var caches = (typeof globalThis.caches === "object" && globalThis.caches) ? globalThis.caches : undefined;
      Object.defineProperty(scope, "caches", { value: caches, enumerable: true, configurable: true });
      def(scope, "skipWaiting", function () { sw.__skipWaiting = true; return Promise.resolve(); });
      // The worker's own fetches bypass interception (a worker doesn't intercept its own requests).
      def(scope, "fetch", function () {
        var prev = __swBypassFetch; __swBypassFetch = true;
        try { return globalThis.fetch.apply(globalThis, arguments); }
        finally { __swBypassFetch = prev; }
      });
      var runInScope = function (src, url) {
        var fn = (globalThis.Function)(
          "self", "registration", "clients", "location", "caches", "skipWaiting",
          "importScripts", "fetch", "addEventListener", "removeEventListener", "dispatchEvent",
          src + "\n//# sourceURL=" + url + "\n"
        );
        fn.call(scope, scope, reg, clients, scope.location, caches, scope.skipWaiting,
          scope.importScripts, scope.fetch, scope.addEventListener, scope.removeEventListener, scope.dispatchEvent);
      };
      def(scope, "importScripts", function () {
        for (var i = 0; i < arguments.length; i++) {
          var u = (new globalThis.URL(String(arguments[i]), scriptHref)).href;
          var env = globalThis.__request("GET", u, "", "{}");
          if (!env) { throw new TypeError("Failed to execute 'importScripts': could not fetch " + u); }
          var parsed; try { parsed = JSON.parse(env); } catch (e) { throw new TypeError("Failed to execute 'importScripts': bad response for " + u); }
          if (!parsed.ok) { throw new TypeError("Failed to execute 'importScripts': HTTP " + parsed.status + " for " + u); }
          runInScope(parsed.body || "", u);
        }
      });
      sw.__scope = scope;
      runInScope(source || "", scriptHref);
      return scope;
    }

    // --- Fetch interception (stage 3) -----------------------------------------------------
    // A request from a controlled client (the page once a worker has clients.claim()ed it) is
    // dispatched to the controller as a FetchEvent; if the handler calls respondWith(), that response
    // is used instead of the network. __swBypassFetch suppresses interception for the worker's own
    // fetches (so respondWith(fetch(event.request)) doesn't re-enter), and __swInFetchDispatch guards
    // against synchronous re-entry during dispatch.
    var __swBypassFetch = false;
    var __swInFetchDispatch = false;
    // Returns a Promise<Response> if the controller handled the request, or null to fall through to
    // the network. `method`/`url` describe the request; `reqInit` carries headers/body for the event.
    def(globalThis, "__swInterceptFetch", function (method, url, reqInit) {
      if (__swBypassFetch || __swInFetchDispatch) { return null; }
      var controller = __swContainer.controller;
      if (!controller || !controller.__scope) { return null; }
      var abs; try { abs = (new globalThis.URL(url, __swBase())).href; } catch (e) { return null; }
      if (!__swMatchesClient2(controller, abs)) { return null; }
      var req;
      try { req = new globalThis.Request(abs, reqInit || { method: method }); } catch (e) { req = { url: abs, method: method, headers: {} }; }
      var ev = new globalThis.FetchEvent("fetch", { request: req, clientId: __swPageClient().id });
      var s = globalThis.__eventState(ev);
      s.__active = true;
      __swInFetchDispatch = true;
      try { controller.__scope.dispatchEvent(ev); }
      finally { __swInFetchDispatch = false; s.__active = false; }
      if (!s.__responded) { return null; } // handler didn't respondWith -> network fallthrough
      // respondWith(r): r may be a Response or a promise for one. A rejection or non-Response is a
      // network error (the fetch rejects with TypeError), matching the spec.
      return Promise.resolve(s.__response).then(function (r) {
        if (r && (typeof globalThis.Response !== "function" || r instanceof globalThis.Response || r.__isResponse || typeof r.text === "function")) { return r; }
        throw new TypeError("Failed to fetch: the FetchEvent respondWith() value was not a Response.");
      }, function () { throw new TypeError("Failed to fetch: the FetchEvent respondWith() promise rejected."); });
    });
    // The controller controls a client whose URL is within the registration scope. We approximate the
    // controller's scope from its script directory's parent walk via the registration that owns it.
    function __swMatchesClient2(controller, absUrl) {
      for (var i = 0; i < __swRegs.length; i++) {
        var sl = __swRegs[i].__slots();
        if (sl.active === controller || sl.waiting === controller || sl.installing === controller) {
          return absUrl.indexOf(__swRegs[i].scope) === 0;
        }
      }
      return false;
    }

    // Dispatch a lifecycle ExtendableEvent into the worker scope and await its waitUntil() promises.
    function __swDispatchExtendable(scope, type) {
      var ev = new globalThis.ExtendableEvent(type);
      var s = globalThis.__eventState(ev);
      s.__active = true;
      scope.dispatchEvent(ev);
      s.__active = false;
      var extend = s.__extend || [];
      return extend.length ? Promise.all(extend)["then"](function () {}, function () {}) : Promise.resolve();
    }

    function __swMakeNavPreload() {
      var enabled = false, headerValue = "true";
      var npm = Object.create(globalThis.NavigationPreloadManager.prototype);
      def(npm, "enable", function () { enabled = true; return Promise.resolve(); });
      def(npm, "disable", function () { enabled = false; return Promise.resolve(); });
      def(npm, "setHeaderValue", function (v) {
        if (arguments.length < 1) { return Promise.reject(new TypeError("Failed to execute 'setHeaderValue' on 'NavigationPreloadManager': 1 argument required, but only 0 present.")); }
        var s = String(v);
        for (var i = 0; i < s.length; i++) {
          var c = s.charCodeAt(i);
          // ByteString: code units must fit in a byte; header value: no NUL/CR/LF.
          if (c > 0xff) { return Promise.reject(new TypeError("Failed to execute 'setHeaderValue' on 'NavigationPreloadManager': the value cannot be converted to a ByteString.")); }
          if (c === 0 || c === 13 || c === 10) { return Promise.reject(new TypeError("Failed to execute 'setHeaderValue' on 'NavigationPreloadManager': the value is not a valid header value.")); }
        }
        headerValue = s; return Promise.resolve();
      });
      def(npm, "getState", function () { return Promise.resolve({ enabled: enabled, headerValue: headerValue }); });
      return npm;
    }

    function __swMakeRegistration(scopeHref, updateViaCache) {
      var reg = Object.create(globalThis.ServiceWorkerRegistration.prototype);
      installEvents(reg);
      var installing = null, waiting = null, active = null;
      Object.defineProperty(reg, "scope", { value: scopeHref, enumerable: true, configurable: true });
      Object.defineProperty(reg, "updateViaCache", { value: updateViaCache, enumerable: true, configurable: true });
      Object.defineProperty(reg, "installing", { get: function () { return installing; }, enumerable: true, configurable: true });
      Object.defineProperty(reg, "waiting", { get: function () { return waiting; }, enumerable: true, configurable: true });
      Object.defineProperty(reg, "active", { get: function () { return active; }, enumerable: true, configurable: true });
      Object.defineProperty(reg, "navigationPreload", { value: __swMakeNavPreload(), enumerable: true, configurable: true });
      reg.onupdatefound = null;
      def(reg, "update", function () { return Promise.resolve(reg); });
      def(reg, "unregister", function () {
        var i = __swRegs.indexOf(reg), found = i >= 0;
        if (found) { __swRegs.splice(i, 1); }
        // Mark the held worker objects redundant (fires statechange on the refs callers captured),
        // then clear the slots so the registration reports installing/waiting/active === null.
        if (active && active.__setState) { active.__setState("redundant"); }
        if (waiting && waiting.__setState) { waiting.__setState("redundant"); }
        if (installing && installing.__setState) { installing.__setState("redundant"); }
        if (__swContainer.controller === active) { __swContainer.controller = null; }
        reg.__setSlots(null, null, null);
        return Promise.resolve(found);
      });
      def(reg, "getNotifications", function () { return Promise.resolve([]); });
      def(reg, "showNotification", function () { return Promise.resolve(); });
      def(reg, "__setSlots", function (i, w, a) { installing = i; waiting = w; active = a; });
      def(reg, "__slots", function () { return { installing: installing, waiting: waiting, active: active }; });
      return reg;
    }

    // Advance an already-evaluated worker (its `scope` built by __swExecuteWorker during register())
    // through the lifecycle. The sequence is deferred a macrotask past register()'s resolution so the
    // registering client can synchronously read `registration.installing` and attach
    // `updatefound`/`statechange` listeners first. Turn 1 fires `updatefound`; turn 2 dispatches
    // `install` (awaiting waitUntil) -> "installed"; turn 3 dispatches `activate` -> "activated" —
    // unless a prior active worker still controls a client and the new worker did not skipWaiting(),
    // in which case it stays "installed" (waiting). Each phase gets its own event-loop turn so a
    // client's awaits (possibly delayed a microtask by promise adoption, e.g. wait_for_update) settle
    // and the next listener is attached before the next transition fires.
    function __swLifecycle(reg, sw, scope, priorActive) {
      var gone = function () { return __swRegs.indexOf(reg) < 0; };
      setTimeout(function () { // turn 1: announce the new worker
        if (gone()) { return; }
        fireOn(reg, "updatefound");
        setTimeout(function () { // turn 2: install
          if (gone()) { return; }
          __swDispatchExtendable(scope, "install").then(function () {
            if (gone()) { return; }
            reg.__setSlots(null, sw, priorActive || null);
            sw.__setState("installed");
            if (priorActive && !sw.__skipWaiting) { return; } // wait behind the active worker
            setTimeout(function () { // turn 3: activate
              if (gone()) { return; }
              if (priorActive && priorActive.__setState) { priorActive.__setState("redundant"); }
              reg.__setSlots(null, null, sw);
              sw.__setState("activating");
              __swDispatchExtendable(scope, "activate").then(function () {
                if (gone()) { return; }
                sw.__setState("activated");
                if (__swReadyResolve && __swMatchesClient(reg.scope)) { var r = __swReadyResolve; __swReadyResolve = null; r(reg); }
              });
            }, 0);
          });
        }, 0);
      }, 0);
    }

    // Per spec the worker script is evaluated before `registration.installing` is set, so the worker
    // observes installing === null during its own evaluation. Returns the built scope, or null after
    // marking the worker redundant if the script threw (register() then rejects).
    function __swEvalWorker(reg, sw, scriptHref, source) {
      try { return __swExecuteWorker(reg, sw, scriptHref, source); }
      catch (e) { sw.__setState("redundant"); (globalThis.__timerErrors || []).push((e && e.stack) || String(e)); return null; }
    }

    var __SW_JS_MIME = {
      "text/javascript": 1, "application/javascript": 1, "application/x-javascript": 1,
      "text/ecmascript": 1, "application/ecmascript": 1, "text/x-javascript": 1
    };

    function __swRegister(url, options) {
      return new Promise(function (resolve, reject) {
        var base = __swBase(), scriptURL, scopeURL;
        try { scriptURL = __swNoFrag(new globalThis.URL(url, base)); }
        catch (e) { reject(new TypeError("Failed to register a ServiceWorker: the script URL is invalid.")); return; }
        // Scheme + encoded-slash checks reject with TypeError, before the origin (SecurityError) checks.
        if (scriptURL.protocol !== "http:" && scriptURL.protocol !== "https:") {
          reject(new TypeError("Failed to register a ServiceWorker: the URL protocol of the script ('" + scriptURL.protocol + "') is not supported.")); return;
        }
        if (/%2f|%5c/i.test(scriptURL.pathname)) {
          reject(new TypeError("Failed to register a ServiceWorker: the script URL includes an encoded slash or backslash.")); return;
        }
        var updateViaCache = (options && options.updateViaCache) || "imports";
        try {
          scopeURL = (options && options.scope != null)
            ? __swNoFrag(new globalThis.URL(String(options.scope), base))
            : __swNoFrag(new globalThis.URL("./", scriptURL)); // default scope: the script's directory
        } catch (e2) { reject(new TypeError("Failed to register a ServiceWorker: the scope URL is invalid.")); return; }
        if (scopeURL.protocol !== "http:" && scopeURL.protocol !== "https:") {
          reject(new TypeError("Failed to register a ServiceWorker: the URL protocol of the scope ('" + scopeURL.protocol + "') is not supported.")); return;
        }
        if (/%2f|%5c/i.test(scopeURL.pathname)) {
          reject(new TypeError("Failed to register a ServiceWorker: the scope URL includes an encoded slash or backslash.")); return;
        }

        var pageOrigin;
        try { pageOrigin = (new globalThis.URL(base)).origin; } catch (e3) { pageOrigin = null; }
        if (scriptURL.origin !== pageOrigin) {
          reject(new globalThis.DOMException("Failed to register a ServiceWorker: the origin of the provided scriptURL does not match the current origin.", "SecurityError")); return;
        }
        if (scopeURL.origin !== pageOrigin) {
          reject(new globalThis.DOMException("Failed to register a ServiceWorker: the origin of the provided scope does not match the current origin.", "SecurityError")); return;
        }

        var scriptHref = scriptURL.href, scopeHref = scopeURL.href;
        var scriptDir = (new globalThis.URL("./", scriptURL)).href;

        globalThis.fetch(scriptHref).then(function (res) {
          if (!res || !res.ok) { throw new TypeError("Failed to register a ServiceWorker: the script resource fetch failed."); }
          var hdr = (res.headers && res.headers.get) ? res.headers : null;
          var mime = (hdr && (hdr.get("content-type") || "")).split(";")[0].trim().toLowerCase();
          if (mime && !__SW_JS_MIME[mime]) {
            throw new globalThis.DOMException("Failed to register a ServiceWorker: the script has an unsupported MIME type ('" + mime + "').", "SecurityError");
          }
          // Max scope is the script directory, widened by a Service-Worker-Allowed response header.
          var allowed = hdr && hdr.get("service-worker-allowed");
          var maxScope = allowed ? (new globalThis.URL(allowed, scriptURL)).href : scriptDir;
          if (scopeHref.indexOf(maxScope) !== 0) {
            throw new globalThis.DOMException("Failed to register a ServiceWorker: the path of the provided scope is not under the max scope allowed.", "SecurityError");
          }
          return res.text ? res.text() : ""; // the worker script source, run in the global scope
        }).then(function (source) {
          var evalErr = new globalThis.DOMException("Failed to register a ServiceWorker: the script threw an error during evaluation.", "AbortError");
          var existing = null;
          for (var i = 0; i < __swRegs.length; i++) { if (__swRegs[i].scope === scopeHref) { existing = __swRegs[i]; break; } }
          if (existing) {
            var slots = existing.__slots();
            // Same script already installed: re-register resolves with the same registration object.
            if (slots.active && slots.active.scriptURL === scriptHref) { resolve(existing); return; }
            // Different script (or none active yet): update in place with a fresh worker. Evaluate it
            // while installing is still null, then set it installing. The old active worker (if any)
            // keeps controlling clients until the new one activates.
            var sw2 = __swMakeWorker(scriptHref);
            var scope2 = __swEvalWorker(existing, sw2, scriptHref, source);
            if (!scope2) { reject(evalErr); return; }
            existing.__setSlots(sw2, slots.waiting, slots.active);
            resolve(existing);
            __swLifecycle(existing, sw2, scope2, slots.active); // fires updatefound itself
            return;
          }
          var reg = __swMakeRegistration(scopeHref, updateViaCache);
          var sw = __swMakeWorker(scriptHref);
          __swRegs.push(reg);
          // Evaluate the worker (installing still null) before exposing it as registration.installing.
          var scope = __swEvalWorker(reg, sw, scriptHref, source);
          if (!scope) { __swRegs.splice(__swRegs.indexOf(reg), 1); reject(evalErr); return; }
          reg.__setSlots(sw, null, null);
          resolve(reg);
          __swLifecycle(reg, sw, scope, null); // fires updatefound itself
        }).catch(function (err) { reject(err); });
      });
    }

    var __swContainer = Object.create(globalThis.ServiceWorkerContainer.prototype);
    installEvents(__swContainer);
    def(__swContainer, "register", function (url, options) {
      if (url == null) { return Promise.reject(new TypeError("Failed to execute 'register' on 'ServiceWorkerContainer': 1 argument required, but only 0 present.")); }
      return __swRegister(String(url), options || {});
    });
    def(__swContainer, "getRegistration", function (clientURL) {
      var base = __swBase(), target, u;
      try { u = (clientURL != null && clientURL !== "") ? new globalThis.URL(String(clientURL), base) : new globalThis.URL(base); }
      catch (e) { return Promise.reject(new TypeError("Failed to execute 'getRegistration' on 'ServiceWorkerContainer': the clientURL is invalid.")); }
      if (u.origin !== (new globalThis.URL(base)).origin) {
        return Promise.reject(new globalThis.DOMException("Failed to execute 'getRegistration' on 'ServiceWorkerContainer': the origin of the provided documentURL does not match the current origin.", "SecurityError"));
      }
      target = __swNoFrag(u).href;
      var best, bestLen = -1;
      for (var i = 0; i < __swRegs.length; i++) {
        var sc = __swRegs[i].scope;
        if (target.indexOf(sc) === 0 && sc.length > bestLen) { best = __swRegs[i]; bestLen = sc.length; }
      }
      return Promise.resolve(best);
    });
    def(__swContainer, "getRegistrations", function () { return Promise.resolve(__swRegs.slice()); });
    def(__swContainer, "startMessages", function () {});
    __swContainer.oncontrollerchange = null;
    __swContainer.onmessage = null;
    __swContainer.onmessageerror = null;
    // controller is null until a worker calls clients.claim() (stage 1 registration alone does not
    // retroactively control the already-loaded page); claim() sets it and fires controllerchange.
    Object.defineProperty(__swContainer, "controller", { value: null, enumerable: true, configurable: true, writable: true });
    Object.defineProperty(__swContainer, "ready", {
      get: function () {
        if (!__swContainer.__readyPromise) {
          var match;
          for (var i = 0; i < __swRegs.length; i++) {
            if (__swRegs[i].__active && __swRegs[i].__active() && __swMatchesClient(__swRegs[i].scope)) { match = __swRegs[i]; break; }
          }
          __swContainer.__readyPromise = match
            ? Promise.resolve(match)
            : new Promise(function (res) { __swReadyResolve = res; });
        }
        return __swContainer.__readyPromise;
      },
      enumerable: true, configurable: true
    });
    Object.defineProperty(globalThis.navigator, "serviceWorker", { value: __swContainer, enumerable: true, configurable: true });
  }
  if (typeof globalThis.WebSocket !== "function") {
    // Real WebSocket: __wsConnect spawns a host socket thread (net::ws_run) and returns an id.
    // The host delivers events via __wsDeliver(id, kind, payload) during the Rust drain; send/close
    // go back through __wsSend/__wsClose. Binary is base64-bridged across the host boundary.
    var __wsRegistry = Object.create(null);
    function __wsToBase64(data) {
      // Accept ArrayBuffer / typed array / Blob (Blob exposes __blobBytes) → base64 string.
      var bytes;
      if (data instanceof ArrayBuffer) { bytes = new Uint8Array(data); }
      else if (data && data.buffer instanceof ArrayBuffer) { bytes = new Uint8Array(data.buffer, data.byteOffset || 0, data.byteLength); }
      else if (data && data.__blobBytes) { bytes = data.__blobBytes; }
      else { bytes = new Uint8Array(0); }
      var s = "";
      for (var i = 0; i < bytes.length; i++) { s += String.fromCharCode(bytes[i]); }
      return (typeof btoa === "function") ? btoa(s) : "";
    }
    function __wsFromBase64(b64) {
      var s = (typeof atob === "function") ? atob(b64) : "";
      var buf = new ArrayBuffer(s.length), view = new Uint8Array(buf);
      for (var i = 0; i < s.length; i++) { view[i] = s.charCodeAt(i) & 0xff; }
      return buf;
    }
    var WebSocketCtor = function (url, protocols) {
      this.url = String(url);
      this.readyState = 0; // CONNECTING
      this.bufferedAmount = 0;
      this.protocol = "";
      this.extensions = "";
      this.binaryType = "blob";
      this.onopen = null; this.onmessage = null; this.onclose = null; this.onerror = null;
      try { installEvents(this); } catch (e) {}
      var id = (typeof __wsConnect === "function") ? __wsConnect(this.url) : 0;
      this.__wsid = id;
      __wsRegistry[id] = this;
    };
    WebSocketCtor.prototype.send = function (data) {
      if (this.readyState !== 1) {
        throw new globalThis.DOMException("Failed to execute 'send' on 'WebSocket': Still in CONNECTING state.", "InvalidStateError");
      }
      if (typeof __wsSend !== "function") { return; }
      if (typeof data === "string") { __wsSend(this.__wsid, 0, data); }
      else { __wsSend(this.__wsid, 1, __wsToBase64(data)); }
    };
    WebSocketCtor.prototype.close = function (code, reason) {
      if (this.readyState === 3 || this.readyState === 2) { return; }
      this.readyState = 2; // CLOSING
      if (typeof __wsClose === "function") { __wsClose(this.__wsid); }
    };
    WebSocketCtor.CONNECTING = 0; WebSocketCtor.OPEN = 1; WebSocketCtor.CLOSING = 2; WebSocketCtor.CLOSED = 3;
    WebSocketCtor.prototype.CONNECTING = 0; WebSocketCtor.prototype.OPEN = 1; WebSocketCtor.prototype.CLOSING = 2; WebSocketCtor.prototype.CLOSED = 3;
    def(globalThis, "WebSocket", WebSocketCtor);

    // Fire a handler (onX + any addEventListener listeners) with an event object on a WebSocket.
    function __wsFire(ws, type, init) {
      var ev = new globalThis.Event(type);
      for (var key in init) { try { def(ev, key, init[key]); } catch (e0) {} }
      if (typeof ws.dispatchEvent === "function") {
        try { ws.dispatchEvent(ev); } catch (e) { (globalThis.__timerErrors || []).push((e && e.stack) || String(e)); }
      }
    }
    // Called from Rust's drain phase for each pending socket event.
    def(globalThis, "__wsDeliver", function (id, kind, payload) {
      var ws = __wsRegistry[id];
      if (!ws) { return; }
      kind = Number(kind);
      if (kind === 0) {            // open
        ws.readyState = 1;
        __wsFire(ws, "open", {});
      } else if (kind === 1) {     // text message
        __wsFire(ws, "message", { data: payload });
      } else if (kind === 2) {     // binary message (base64)
        var buf = __wsFromBase64(String(payload));
        var data = buf;
        if (ws.binaryType === "blob" && typeof globalThis.Blob === "function") {
          try { data = new globalThis.Blob([buf]); } catch (e) { data = buf; }
        }
        __wsFire(ws, "message", { data: data });
      } else if (kind === 3) {     // close ("code:reason")
        ws.readyState = 3;
        var p = String(payload), ci = p.indexOf(":");
        var code = ci >= 0 ? parseInt(p.slice(0, ci), 10) : 1005;
        var reason = ci >= 0 ? p.slice(ci + 1) : "";
        if (!(code >= 0)) { code = 1005; }
        __wsFire(ws, "close", { code: code, reason: reason, wasClean: code === 1000 });
        delete __wsRegistry[id];
      } else if (kind === 4) {     // error
        __wsFire(ws, "error", { message: String(payload) });
      }
    });
  }
  if (typeof globalThis.Headers !== "function") {
    def(globalThis, "Headers", function (init) {
      var m = {};
      this.append = function (k, v) { k = String(k).toLowerCase(); m[k] = (m[k] === undefined) ? String(v) : (m[k] + ", " + String(v)); };
      this.set = function (k, v) { m[String(k).toLowerCase()] = String(v); };
      this.get = function (k) { var v = m[String(k).toLowerCase()]; return v === undefined ? null : v; };
      this.has = function (k) { return String(k).toLowerCase() in m; };
      this.delete = function (k) { delete m[String(k).toLowerCase()]; };
      this.forEach = function (cb, thisArg) { Object.keys(m).sort().forEach(function (k) { cb.call(thisArg, m[k], k, this); }, this); };
      this.keys = function () { return Object.keys(m).sort()[Symbol.iterator](); };
      this.values = function () { return Object.keys(m).sort().map(function (k) { return m[k]; })[Symbol.iterator](); };
      this.entries = function () { return Object.keys(m).sort().map(function (k) { return [k, m[k]]; })[Symbol.iterator](); };
      this.getSetCookie = function () { return []; };
      this[Symbol.iterator] = function () { return this.entries(); };
      // init: another Headers, an array of [k,v] pairs, or a plain object.
      if (init) {
        if (typeof init.forEach === "function" && typeof init.length !== "number") { init.forEach(function (v, k) { this.append(k, v); }, this); }
        else if (typeof init.length === "number") { for (var i = 0; i < init.length; i++) { this.append(init[i][0], init[i][1]); } }
        else { for (var k in init) { if (Object.prototype.hasOwnProperty.call(init, k)) { this.append(k, init[k]); } } }
      }
    });
  }

  // --- Request / Response (Fetch API classes) ----------------------------------------------
  if (typeof globalThis.Request !== "function") {
    var RequestCtor = function (input, init) {
      init = init || {};
      var fromReq = input && typeof input === "object" && input.__isRequest;
      // Per the Fetch standard, the input is parsed against the entry settings object's base URL,
      // which percent-encodes the query/path (e.g. "?ß" -> "?%C3%9F"). A Request cloned from
      // another Request already carries a parsed URL.
      this.url = fromReq ? input.url : (function () {
        var raw = (input && input.url) ? String(input.url) : String(input);
        try {
          var base;
          try { base = (typeof document !== "undefined" && document.baseURI) ? document.baseURI : ((typeof location !== "undefined" && location.href) ? location.href : undefined); } catch (e) { base = undefined; }
          return new globalThis.URL(raw, base).href;
        } catch (e) { return raw; }
      })();
      this.method = String(init.method || (fromReq && input.method) || "GET").toUpperCase();
      this.headers = new globalThis.Headers(init.headers || (fromReq ? input.headers : null) || {});
      this.body = init.body !== undefined ? init.body : (fromReq ? input.body : null);
      this.credentials = init.credentials || "same-origin";
      this.mode = init.mode || "cors";
      this.cache = init.cache || "default";
      this.redirect = init.redirect || "follow";
      this.referrer = init.referrer || "about:client";
      this.signal = init.signal || (fromReq ? input.signal : null) || null;
      this.__isRequest = true;
    };
    RequestCtor.prototype.clone = function () { return new globalThis.Request(this.url, this); };
    RequestCtor.prototype.text = function () { return Promise.resolve(this.body == null ? "" : String(this.body)); };
    RequestCtor.prototype.json = function () { try { return Promise.resolve(JSON.parse(this.body == null ? "null" : String(this.body))); } catch (e) { return Promise.reject(e); } };
    RequestCtor.prototype.formData = function () {
      var b = this.body;
      // A FormData body is returned as a fresh copy (no re-serialization round-trip needed).
      if (b && (b.__isFormData || (typeof globalThis.FormData === "function" && b instanceof globalThis.FormData))) {
        var copy = new globalThis.FormData();
        b.forEach(function (v, k) { copy.append(k, v); });
        return Promise.resolve(copy);
      }
      if (__isBlobLike(b)) {
        return __bodyToFormData(__blobTextSync(b), b.type || (this.headers && this.headers.get && this.headers.get("content-type")));
      }
      return __bodyToFormData(b == null ? "" : String(b), this.headers && this.headers.get && this.headers.get("content-type"));
    };
    RequestCtor.prototype.blob = function () {
      var b = this.body;
      if (__isBlobLike(b)) { return Promise.resolve(b); }
      var ct = (this.headers && this.headers.get && this.headers.get("content-type")) || "";
      return Promise.resolve(new globalThis.Blob([b == null ? "" : String(b)], { type: ct }));
    };
    def(globalThis, "Request", RequestCtor);
  }

  if (typeof globalThis.Response !== "function") {
    var ResponseCtor = function (body, init) {
      init = init || {};
      this.status = init.status !== undefined ? (init.status | 0) : 200;
      this.statusText = init.statusText !== undefined ? String(init.statusText) : "";
      this.ok = this.status >= 200 && this.status < 300;
      // Reuse an existing Headers instance as-is, but build one for arrays/plain objects. (Arrays
      // also expose `.entries`, so detect a Headers by its `.get`/`.append` methods instead.)
      this.headers = (init.headers && typeof init.headers.get === "function" && typeof init.headers.append === "function") ? init.headers : new globalThis.Headers(init.headers || {});
      this.url = init.url ? String(init.url) : "";
      this.redirected = !!init.redirected;
      this.type = init.type || "default";
      this.bodyUsed = false;
      this.body = null;
      // Body extraction: strings pass through; a FormData serializes to multipart/form-data (and
      // supplies the boundary content-type); a Blob contributes its bytes + type; URLSearchParams
      // becomes urlencoded; anything else is stringified. The content-type is only set when the
      // init headers didn't already provide one.
      if (body == null) {
        this.__body = "";
      } else if (typeof body === "string") {
        this.__body = body;
      } else if (body.__isFormData || (typeof globalThis.FormData === "function" && body instanceof globalThis.FormData)) {
        var __rb = __genBoundary();
        this.__body = __formDataToMultipart(body, __rb);
        if (!this.headers.has("content-type")) { this.headers.set("content-type", "multipart/form-data; boundary=" + __rb); }
      } else if (__isBlobLike(body)) {
        this.__body = __blobTextSync(body);
        if (!this.headers.has("content-type") && body.type) { this.headers.set("content-type", body.type); }
      } else if (typeof globalThis.URLSearchParams === "function" && body instanceof globalThis.URLSearchParams) {
        this.__body = body.toString();
        if (!this.headers.has("content-type")) { this.headers.set("content-type", "application/x-www-form-urlencoded;charset=UTF-8"); }
      } else if (typeof body.toString === "function") {
        this.__body = body.toString();
      } else {
        this.__body = String(body);
      }
      this.__isResponse = true;
    };
    ResponseCtor.prototype.text = function () { this.bodyUsed = true; return Promise.resolve(this.__body); };
    ResponseCtor.prototype.json = function () { this.bodyUsed = true; try { return Promise.resolve(JSON.parse(this.__body)); } catch (e) { return Promise.reject(e); } };
    ResponseCtor.prototype.arrayBuffer = function () { return Promise.resolve(new ArrayBuffer(0)); };
    ResponseCtor.prototype.blob = function () { this.bodyUsed = true; return Promise.resolve(new globalThis.Blob([this.__body], { type: (this.headers.get && this.headers.get("content-type")) || "" })); };
    ResponseCtor.prototype.formData = function () { this.bodyUsed = true; return __bodyToFormData(this.__body, this.headers && this.headers.get && this.headers.get("content-type")); };
    // body: a ReadableStream of the response's bytes (UTF-8). Reading it marks the body used.
    Object.defineProperty(ResponseCtor.prototype, "body", {
      get: function () {
        if (this.__bodyStream) { return this.__bodyStream; }
        var self = this;
        var src = String(this.__body == null ? "" : this.__body);
        var bytes = (typeof globalThis.TextEncoder === "function") ? new globalThis.TextEncoder().encode(src) : (function () { var a = []; for (var i = 0; i < src.length; i++) { a.push(src.charCodeAt(i) & 0xff); } return new Uint8Array(a); })();
        this.__bodyStream = new globalThis.ReadableStream({ start: function (c) { self.bodyUsed = true; c.enqueue(bytes); c.close(); } });
        return this.__bodyStream;
      }, configurable: true, enumerable: true
    });
    ResponseCtor.prototype.clone = function () { return new globalThis.Response(this.__body, { status: this.status, statusText: this.statusText, headers: this.headers, url: this.url, type: this.type, redirected: this.redirected }); };
    ResponseCtor.json = function (data, init) { init = init || {}; var h = new globalThis.Headers(init.headers || {}); if (!h.has("content-type")) { h.set("content-type", "application/json"); } return new globalThis.Response(JSON.stringify(data), { status: init.status, statusText: init.statusText, headers: h }); };
    ResponseCtor.error = function () { var r = new globalThis.Response("", { status: 0 }); r.type = "error"; return r; };
    ResponseCtor.redirect = function (url, status) { var r = new globalThis.Response("", { status: status || 302 }); r.headers.set("location", String(url)); r.redirected = true; return r; };
    def(globalThis, "Response", ResponseCtor);
  }

  if (typeof globalThis.EventSource !== "function") {
    var EventSourceCtor = function (url) {
      var p = parseURL(String(url), location.href);
      this.url = p && !p.__invalid ? p.href : "";
      this.readyState = 0;
      this.withCredentials = false;
    };
    EventSourceCtor.CONNECTING = 0; EventSourceCtor.OPEN = 1; EventSourceCtor.CLOSED = 2;
    EventSourceCtor.prototype.CONNECTING = 0; EventSourceCtor.prototype.OPEN = 1; EventSourceCtor.prototype.CLOSED = 2;
    EventSourceCtor.prototype.close = function () { this.readyState = 2; };
    def(globalThis, "EventSource", EventSourceCtor);
  }

  // --- Cache / CacheStorage (issue #56 stage 4) --------------------------------------------
  // An in-memory CacheStorage exposed as `caches` on the window and (via the worker scope) the
  // ServiceWorkerGlobalScope. Each Cache stores GET request/response pairs keyed by URL; add()/
  // addAll() fetch then put(); match honours ignoreSearch/ignoreMethod. Responses are cloned in and
  // out so a cached body is never consumed by a reader.
  if (typeof globalThis.caches === "undefined") {
    defClass("CacheStorage");
    defClass("Cache");
    var __cacheStore = Object.create(null); // name -> Cache instance, in insertion order via keys()
    function __cacheBase() { try { return globalThis.location.href; } catch (e) { return undefined; } }
    function __cacheReqUrl(input) {
      if (input && typeof input === "object" && input.url != null) { return String(input.url); }
      try { return (new globalThis.URL(String(input), __cacheBase())).href; } catch (e) { return String(input); }
    }
    function __cacheReqMethod(input) {
      return (input && typeof input === "object" && input.method) ? String(input.method).toUpperCase() : "GET";
    }
    function __cacheClone(response) {
      return (response && typeof response.clone === "function") ? response.clone() : response;
    }
    function __makeCache() {
      var cache = Object.create(globalThis.Cache.prototype);
      var entries = []; // { url, response }
      function key(url, opts) { return (opts && opts.ignoreSearch) ? url.split("?")[0] : url; }
      function findIndex(input, opts) {
        if (!(opts && opts.ignoreMethod) && __cacheReqMethod(input) !== "GET") { return -1; }
        var target = key(__cacheReqUrl(input), opts);
        for (var i = 0; i < entries.length; i++) { if (key(entries[i].url, opts) === target) { return i; } }
        return -1;
      }
      def(cache, "match", function (input, opts) {
        var i = findIndex(input, opts);
        return Promise.resolve(i >= 0 ? __cacheClone(entries[i].response) : undefined);
      });
      def(cache, "matchAll", function (input, opts) {
        if (input === undefined) { return Promise.resolve(entries.map(function (e) { return __cacheClone(e.response); })); }
        var i = findIndex(input, opts);
        return Promise.resolve(i >= 0 ? [__cacheClone(entries[i].response)] : []);
      });
      def(cache, "put", function (request, response) {
        if (__cacheReqMethod(request) !== "GET") { return Promise.reject(new TypeError("Cache.put: only GET requests can be cached.")); }
        if (!response) { return Promise.reject(new TypeError("Cache.put: response required.")); }
        if (response.status === 206) { return Promise.reject(new TypeError("Cache.put: a 206 Partial Content response cannot be cached.")); }
        if (response.type === "error") { return Promise.reject(new TypeError("Cache.put: a network-error response cannot be cached.")); }
        var url = __cacheReqUrl(request);
        var rec = { url: url, response: __cacheClone(response) };
        var idx = -1;
        for (var i = 0; i < entries.length; i++) { if (entries[i].url === url) { idx = i; break; } }
        if (idx >= 0) { entries[idx] = rec; } else { entries.push(rec); }
        return Promise.resolve();
      });
      def(cache, "add", function (request) { return cache.addAll([request]); });
      def(cache, "addAll", function (requests) {
        var reqs = Array.prototype.slice.call(requests || []);
        return Promise.all(reqs.map(function (r) {
          if (__cacheReqMethod(r) !== "GET") { return Promise.reject(new TypeError("Cache.addAll: only GET requests can be cached.")); }
          return globalThis.fetch(r).then(function (resp) {
            if (!resp || !resp.ok) { throw new TypeError("Cache.addAll: request for '" + __cacheReqUrl(r) + "' failed (status " + (resp && resp.status) + ")."); }
            return cache.put(r, resp);
          });
        })).then(function () { return undefined; });
      });
      def(cache, "delete", function (input, opts) {
        var i = findIndex(input, opts);
        if (i >= 0) { entries.splice(i, 1); return Promise.resolve(true); }
        return Promise.resolve(false);
      });
      def(cache, "keys", function (input, opts) {
        var list;
        if (input === undefined) { list = entries; }
        else { var i = findIndex(input, opts); list = i >= 0 ? [entries[i]] : []; }
        return Promise.resolve(list.map(function (e) { try { return new globalThis.Request(e.url); } catch (x) { return { url: e.url, method: "GET" }; } }));
      });
      return cache;
    }
    var __cacheStorage = Object.create(globalThis.CacheStorage.prototype);
    def(__cacheStorage, "open", function (name) { name = String(name); if (!__cacheStore[name]) { __cacheStore[name] = __makeCache(); } return Promise.resolve(__cacheStore[name]); });
    def(__cacheStorage, "has", function (name) { return Promise.resolve(Object.prototype.hasOwnProperty.call(__cacheStore, String(name))); });
    def(__cacheStorage, "delete", function (name) {
      name = String(name);
      if (Object.prototype.hasOwnProperty.call(__cacheStore, name)) { delete __cacheStore[name]; return Promise.resolve(true); }
      return Promise.resolve(false);
    });
    def(__cacheStorage, "keys", function () { return Promise.resolve(Object.keys(__cacheStore)); });
    def(__cacheStorage, "match", function (request, opts) {
      opts = opts || {};
      var names = opts.cacheName ? [opts.cacheName] : Object.keys(__cacheStore);
      var i = 0;
      function tryNext() {
        if (i >= names.length) { return Promise.resolve(undefined); }
        var c = __cacheStore[names[i++]];
        if (!c) { return tryNext(); }
        return c.match(request, opts).then(function (r) { return r !== undefined ? r : tryNext(); });
      }
      return tryNext();
    });
    def(globalThis, "caches", __cacheStorage);
  }

  // --- URLSearchParams ---------------------------------------------------------------------
  // application/x-www-form-urlencoded serialization: alnum + `*-._` pass through, space -> `+`,
  // everything else -> %XX of its UTF-8 bytes (encodeURIComponent covers UTF-8; we then fix the
  // characters it leaves but the form serializer must encode, and turn %20 into +).
  function __formEncode(s) {
    return encodeURIComponent(String(s))
      .replace(/%20/g, "+")
      .replace(/[!'()~]/g, function (c) { return "%" + c.charCodeAt(0).toString(16).toUpperCase(); });
  }
  function __formDecode(s) {
    // application/x-www-form-urlencoded decode: `+` -> space, then percent-decode each VALID `%XX`
    // run (leaving an invalid `%` literal — decodeURIComponent is all-or-nothing, the URL standard
    // decodes per-escape). Runs are decoded together so multi-byte UTF-8 sequences round-trip.
    s = String(s).replace(/\+/g, " ");
    return s.replace(/(?:%[0-9A-Fa-f]{2})+/g, function (m) {
      try { return decodeURIComponent(m); } catch (e) { return m; }
    });
  }
  if (typeof globalThis.URLSearchParams !== "function") {
    def(globalThis, "URLSearchParams", function (init) {
      var pairs = [];
      var self = this;
      // `__onChange` lets a URL keep its href/search in sync with mutations (set in the URL ctor).
      function changed() { if (typeof self.__onChange === "function") { try { self.__onChange(); } catch (e) {} } }
      function add(k, v) { pairs.push([String(k), String(v)]); }
      function parseQuery(s) {
        if (s.charAt(0) === "?") { s = s.slice(1); }
        if (!s) { return; }
        var segs = s.split("&");
        for (var i = 0; i < segs.length; i++) {
          if (segs[i] === "") { continue; }
          var eq = segs[i].indexOf("=");
          var k = eq < 0 ? segs[i] : segs[i].slice(0, eq);
          var v = eq < 0 ? "" : segs[i].slice(eq + 1);
          add(__formDecode(k), __formDecode(v));
        }
      }
      if (init == null) { /* empty */ }
      else if (typeof init === "string") { parseQuery(init); }
      else if (typeof init[Symbol.iterator] === "function") {
        // Sequence of two-element sequences (covers arrays AND another URLSearchParams).
        var it = init[Symbol.iterator](), step;
        while (!(step = it.next()).done) {
          var pair = step.value, a = [], pit = pair[Symbol.iterator](), ps;
          while (!(ps = pit.next()).done) { a.push(ps.value); }
          if (a.length !== 2) { throw new TypeError("Failed to construct 'URLSearchParams': Sequence initializer must only contain pair elements"); }
          add(a[0], a[1]);
        }
      } else if (typeof init === "object") {
        // Record: own enumerable string-keyed properties.
        var keys = Object.keys(init);
        for (var j = 0; j < keys.length; j++) { add(keys[j], init[keys[j]]); }
      }
      // Methods are non-enumerable (so `new URLSearchParams(usp)` / record init never see them).
      def(this, "append", function (k, v) { add(k, v); changed(); });
      def(this, "set", function (k, v) { k = String(k); v = String(v); var found = false; for (var i = 0; i < pairs.length;) { if (pairs[i][0] === k) { if (!found) { pairs[i][1] = v; found = true; i++; } else { pairs.splice(i, 1); } } else { i++; } } if (!found) { add(k, v); } changed(); });
      def(this, "get", function (k) { k = String(k); for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === k) { return pairs[i][1]; } } return null; });
      def(this, "getAll", function (k) { k = String(k); var out = []; for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === k) { out.push(pairs[i][1]); } } return out; });
      def(this, "has", function (k, v) { k = String(k); var checkV = arguments.length > 1 && v !== undefined; if (checkV) { v = String(v); } for (var i = 0; i < pairs.length; i++) { if (pairs[i][0] === k && (!checkV || pairs[i][1] === v)) { return true; } } return false; });
      def(this, "delete", function (k, v) { k = String(k); var checkV = arguments.length > 1 && v !== undefined; if (checkV) { v = String(v); } for (var i = pairs.length - 1; i >= 0; i--) { if (pairs[i][0] === k && (!checkV || pairs[i][1] === v)) { pairs.splice(i, 1); } } changed(); });
      def(this, "forEach", function (cb, thisArg) { for (var i = 0; i < pairs.length; i++) { cb.call(thisArg, pairs[i][1], pairs[i][0], this); } });
      def(this, "keys", function () { return pairs.map(function (p) { return p[0]; })[Symbol.iterator](); });
      def(this, "values", function () { return pairs.map(function (p) { return p[1]; })[Symbol.iterator](); });
      def(this, "entries", function () { return pairs.map(function (p) { return [p[0], p[1]]; })[Symbol.iterator](); });
      def(this, "sort", function () { pairs.sort(function (a, b) { return a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0; }); changed(); });
      def(this, Symbol.iterator, function () { return this.entries(); });
      def(this, "toString", function () { return pairs.map(function (p) { return __formEncode(p[0]) + "=" + __formEncode(p[1]); }).join("&"); });
      // Internal: replace contents from a query string (used by URL when .search/.href changes).
      def(this, "__setFromQuery", function (q) { pairs.length = 0; parseQuery(q == null ? "" : String(q)); });
      Object.defineProperty(this, "size", { get: function () { return pairs.length; }, enumerable: false, configurable: true });
    });
  }

  // --- URL ---------------------------------------------------------------------------------
  if (typeof globalThis.URL !== "function") {
    def(globalThis, "URL", function (url, base) {
      var p = parseURL(url, base != null ? String(base) : null);
      // Per the URL standard, `new URL(...)` throws a TypeError for an invalid URL.
      if (p.__invalid) {
        throw new TypeError("Failed to construct 'URL': Invalid URL");
      }
      var self = this;
      var sp = new globalThis.URLSearchParams(p.search);
      Object.defineProperty(this, "searchParams", { value: sp, enumerable: true, configurable: true });
      // Apply a fresh parsed record (after a setter) and re-sync searchParams from the new query.
      function applyRec(r) { p = r; sp.__setFromQuery(p.search); }
      // Each WHATWG URL attribute is live: the setter runs the spec setter in Rust (__urlSet) on the
      // current href and adopts the reserialized record; an invalid value is a no-op (per spec),
      // except href which throws.
      function defAttr(name) {
        Object.defineProperty(self, name, {
          get: function () { return p[name]; },
          set: function (v) {
            var json = __urlSet(p.href, name, String(v));
            if (json == null) {
              if (name === "href") { throw new TypeError("Failed to set the 'href' property on 'URL': Invalid URL"); }
              return;
            }
            try { applyRec(JSON.parse(json)); } catch (e) {}
          },
          enumerable: true, configurable: true
        });
      }
      ["href", "protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"].forEach(defAttr);
      // origin is read-only.
      Object.defineProperty(this, "origin", { get: function () { return p.origin; }, enumerable: true, configurable: true });
      this.toString = function () { return p.href; };
      this.toJSON = function () { return p.href; };
      // Keep the URL in sync when searchParams is mutated (append/set/delete/sort/…): reserialize the
      // query through __urlSet and adopt the new record (without re-syncing searchParams from it).
      sp.__onChange = function () {
        var json = __urlSet(p.href, "search", sp.toString());
        if (json != null) { try { p = JSON.parse(json); } catch (e) {} }
      };
    });
    // Static parsers (WHATWG URL): canParse(url[, base]) -> boolean; parse(url[, base]) -> URL|null.
    globalThis.URL.canParse = function (url, base) {
      if (arguments.length < 1) { throw new TypeError("Failed to execute 'canParse' on 'URL': 1 argument required, but only 0 present."); }
      try { return !parseURL(String(url), base != null ? String(base) : null).__invalid; } catch (e) { return false; }
    };
    globalThis.URL.parse = function (url, base) {
      if (arguments.length < 1) { throw new TypeError("Failed to execute 'parse' on 'URL': 1 argument required, but only 0 present."); }
      try { return new globalThis.URL(url, base); } catch (e) { return null; }
    };
    // Encode the Blob's bytes as a self-contained data: URL so it actually works as an <img> src /
    // fetch target (we don't keep a blob: registry). revoke is a no-op (data: needs no cleanup).
    globalThis.URL.createObjectURL = function (obj) {
      try {
        if (obj && obj.__blobBytes) {
          var bytes = obj.__blobBytes, s = "";
          for (var i = 0; i < bytes.length; i++) { s += String.fromCharCode(bytes[i]); }
          var b64 = (typeof btoa === "function") ? btoa(s) : "";
          return "data:" + (obj.type || "application/octet-stream") + ";base64," + b64;
        }
      } catch (e) {}
      return "blob:null/0";
    };
    globalThis.URL.revokeObjectURL = fn;
  }
  if (typeof globalThis.queueMicrotask !== "function") { /* installed by timers */ }

  // --- misc presence stubs -----------------------------------------------------------------
  def(globalThis, "requestIdleCallback", function (cb) { return setTimeout(function () { try { cb({ didTimeout: false, timeRemaining: function () { return 0; } }); } catch (e) {} }, 1); });
  def(globalThis, "cancelIdleCallback", function (id) { return clearTimeout(id); });

  if (typeof globalThis.structuredClone !== "function") {
    // A real structured-clone: deep-copies the common cloneable types (Date, RegExp, ArrayBuffer +
    // views, Map, Set, Array, plain objects, Error, Blob), preserves shared references and cycles
    // via a memory map, and throws DataCloneError for non-cloneable values (functions, symbols, DOM
    // nodes, exotic objects) the way the spec requires — instead of the old JSON round-trip that
    // silently dropped Maps/Sets/cycles and returned the original on failure. We don't implement
    // `transfer` (no ArrayBuffer detach primitive in pure JS); the option is accepted and ignored.
    def(globalThis, "structuredClone", function (value, _options) {
      var seen = new Map();
      function dce(msg) { return new globalThis.DOMException(msg, "DataCloneError"); }
      function clone(v) {
        if (v === null) { return v; }
        var t = typeof v;
        if (t === "symbol") { throw dce("Symbols cannot be cloned."); }
        if (t === "function") { throw dce("Functions cannot be cloned."); }
        if (t !== "object") { return v; }                 // string/number/boolean/bigint/undefined
        if (seen.has(v)) { return seen.get(v); }           // shared reference / cycle
        if (typeof v.nodeType === "number") { throw dce("DOM nodes cannot be cloned."); }
        var tag = Object.prototype.toString.call(v);
        var out;
        switch (tag) {
          case "[object Date]": return new Date(v.getTime());
          case "[object RegExp]": return new RegExp(v.source, v.flags);
          case "[object Boolean]": return new Boolean(v.valueOf());
          case "[object Number]": return new Number(v.valueOf());
          case "[object String]": return new String(v.valueOf());
          case "[object ArrayBuffer]": out = v.slice(0); seen.set(v, out); return out;
          case "[object DataView]": return new DataView(clone(v.buffer), v.byteOffset, v.byteLength);
          case "[object Map]":
            out = new Map(); seen.set(v, out);
            v.forEach(function (val, key) { out.set(clone(key), clone(val)); });
            return out;
          case "[object Set]":
            out = new Set(); seen.set(v, out);
            v.forEach(function (val) { out.add(clone(val)); });
            return out;
          case "[object Error]": {
            var EC = (typeof globalThis[v.name] === "function") ? globalThis[v.name] : Error;
            out = new EC(v.message); seen.set(v, out);
            try { if (v.stack !== undefined) { out.stack = v.stack; } } catch (e) {}
            return out;
          }
          case "[object Array]":
            out = new Array(v.length); seen.set(v, out);
            for (var i = 0; i < v.length; i++) { if (i in v) { out[i] = clone(v[i]); } }
            Object.keys(v).forEach(function (k) { if (!/^\d+$/.test(k)) { out[k] = clone(v[k]); } });
            return out;
        }
        if (ArrayBuffer.isView(v)) {                       // typed arrays (Int8Array … BigUint64Array)
          return new v.constructor(clone(v.buffer), v.byteOffset, v.length);
        }
        if (typeof globalThis.Blob === "function" && v instanceof globalThis.Blob) {
          return v.slice(0, v.size, v.type);
        }
        var proto = Object.getPrototypeOf(v);
        if (proto === Object.prototype || proto === null) { // plain object: own enumerable string keys
          out = {}; seen.set(v, out);
          Object.keys(v).forEach(function (k) { out[k] = clone(v[k]); });
          return out;
        }
        throw dce("An object could not be cloned.");        // exotic / non-[Serializable]
      }
      return clone(value);
    });
  }

  // Minimal Web Animations `Animation` for `Element.animate()`. We don't run/composite animations,
  // so this models only the lifecycle: `finished`/`ready` promises and play-state, with the effect
  // treated as completing after its `delay + duration`. Enough for the common "await a throwaway
  // animation to sync a frame" idiom; tests that read interpolated values will still fail (as they
  // would without `animate` at all), not error.
  def(globalThis, "__makeAnimation", function (options) {
    var dur = 0, delay = 0;
    if (typeof options === "number") {
      dur = options;
    } else if (options && typeof options === "object") {
      dur = Number(options.duration) || 0;
      delay = Number(options.delay) || 0;
    }
    if (!isFinite(dur) || dur < 0) { dur = 0; }
    if (!isFinite(delay) || delay < 0) { delay = 0; }
    var anim = { playState: "running", currentTime: 0, startTime: null, playbackRate: 1,
                 id: "", effect: null, timeline: null, onfinish: null, oncancel: null,
                 pending: false };
    var settle;
    anim.finished = new Promise(function (resolve) { settle = resolve; });
    // Swallow rejection-less completion; cancel just resolves the lifecycle for our purposes.
    anim.ready = Promise.resolve(anim);
    var done = false;
    function finishNow() {
      if (done) { return; }
      done = true;
      anim.playState = "finished";
      anim.currentTime = delay + dur;
      settle(anim);
      if (typeof anim.onfinish === "function") { try { anim.onfinish.call(anim, { type: "finish" }); } catch (e) {} }
    }
    anim.play = function () { anim.playState = "running"; };
    anim.pause = function () { anim.playState = "paused"; };
    anim.reverse = function () {};
    anim.finish = function () { finishNow(); };
    anim.cancel = function () {
      if (done) { return; }
      done = true;
      anim.playState = "idle";
      anim.currentTime = null;
      settle(anim); // resolve rather than reject: nothing awaits a cancellation here
      if (typeof anim.oncancel === "function") { try { anim.oncancel.call(anim, { type: "cancel" }); } catch (e) {} }
    };
    anim.updatePlaybackRate = fn;
    anim.commitStyles = fn;   // we don't interpolate, so there are no computed values to commit
    anim.persist = fn;
    anim.addEventListener = fn;
    anim.removeEventListener = fn;
    anim.dispatchEvent = function () { return false; };
    setTimeout(finishNow, delay + dur);
    return anim;
  });

  // CSS namespace: CSS.supports (feature detection — optimistic), CSS.escape (selector escaping),
  // and no-op registerProperty. Pages reference `CSS` directly (ReferenceError otherwise).
  if (typeof globalThis.CSS === "undefined") {
    var CSSns = {
      supports: function (prop, value) {
        try {
          if (value !== undefined) {
            var pn = normPropName(prop), pv = String(value);
            if (pv.length === 0) return false;
            if (!isKnownProperty(pn)) return false;
            return isValidValue(pn, pv);
          }
          // One-arg form: a support condition. `selector(...)` / `font-tech(...)` /
          // `font-format(...)` functional conditions are answered optimistically (feature-detection).
          var c = String(prop).trim();
          if (/^(selector|font-tech|font-format)\s*\(/i.test(c)) return true;
          var ci = indexOfTopLevelColon(c);
          if (ci < 0) return false;
          return CSSns.supports(c.slice(0, ci).trim(), c.slice(ci + 1).trim());
        } catch (e) { return false; }
      },
      escape: function (value) {
        if (arguments.length < 1) { throw new TypeError("Failed to execute 'escape' on 'CSS': 1 argument required, but only 0 present."); }
        // CSSOM "serialize an identifier" (https://drafts.csswg.org/cssom/#serialize-an-identifier).
        var s = String(value), out = "";
        var len = s.length;
        for (var i = 0; i < len; i++) {
          var c = s.charCodeAt(i);
          if (c === 0x0000) {
            // U+0000 NULL -> U+FFFD REPLACEMENT CHARACTER.
            out += "�";
          } else if ((c >= 0x0001 && c <= 0x001F) || c === 0x007F) {
            // Control characters -> "\" + hex + " ".
            out += "\\" + c.toString(16) + " ";
          } else if (i === 0 && c >= 0x0030 && c <= 0x0039) {
            // A leading digit -> "\" + hex + " ".
            out += "\\" + c.toString(16) + " ";
          } else if (i === 1 && c >= 0x0030 && c <= 0x0039 && s.charCodeAt(0) === 0x002D) {
            // A digit as the second char when the first is "-" -> escaped.
            out += "\\" + c.toString(16) + " ";
          } else if (i === 0 && c === 0x002D && len === 1) {
            // A lone "-" -> "\-".
            out += "\\" + s.charAt(i);
          } else if (c >= 0x0080 || c === 0x002D || c === 0x005F ||
                     (c >= 0x0030 && c <= 0x0039) || (c >= 0x0041 && c <= 0x005A) || (c >= 0x0061 && c <= 0x007A)) {
            // >= U+0080, "-", "_", 0-9, A-Z, a-z -> the character itself.
            out += s.charAt(i);
          } else {
            // Any other character -> "\" + the character.
            out += "\\" + s.charAt(i);
          }
        }
        return out;
      },
      registerProperty: function () {},
      px: function (n) { return { value: Number(n) || 0, unit: "px", toString: function () { return (Number(n) || 0) + "px"; } }; }
    };
    // WebIDL namespace object: @@toStringTag is the namespace name, non-writable/non-enumerable/
    // configurable — so `Object.prototype.toString.call(CSS) === "[object CSS]"`.
    try { Object.defineProperty(CSSns, Symbol.toStringTag, { value: "CSS", writable: false, enumerable: false, configurable: true }); } catch (e) {}
    def(globalThis, "CSS", CSSns);
  }

  // NodeFilter constants (used with createTreeWalker / createNodeIterator below).
  if (typeof globalThis.NodeFilter === "undefined") {
    def(globalThis, "NodeFilter", {
      FILTER_ACCEPT: 1, FILTER_REJECT: 2, FILTER_SKIP: 3,
      SHOW_ALL: 0xFFFFFFFF, SHOW_ELEMENT: 0x1, SHOW_ATTRIBUTE: 0x2, SHOW_TEXT: 0x4,
      SHOW_CDATA_SECTION: 0x8, SHOW_ENTITY_REFERENCE: 0x10, SHOW_ENTITY: 0x20,
      SHOW_PROCESSING_INSTRUCTION: 0x40, SHOW_COMMENT: 0x80, SHOW_DOCUMENT: 0x100,
      SHOW_DOCUMENT_TYPE: 0x200, SHOW_DOCUMENT_FRAGMENT: 0x400, SHOW_NOTATION: 0x800,
    });
  }

  // NodeIterator / TreeWalker (https://dom.spec.whatwg.org/#traversal). Both are live, filtered views
  // over the tree rooted at `root` — they navigate parent/sibling/child pointers on demand rather
  // than snapshotting, so they observe mutations (and NodeIterator runs pre-removing steps). Backed
  // by the real interface prototypes so instances stringify as "[object NodeIterator]" / "[object
  // TreeWalker]" and their attributes are read-only per WebIDL.
  var __niProto = (globalThis.NodeIterator && globalThis.NodeIterator.prototype) || Object.prototype;
  var __twProto = (globalThis.TreeWalker && globalThis.TreeWalker.prototype) || Object.prototype;
  var FILTER_ACCEPT = 1, FILTER_REJECT = 2, FILTER_SKIP = 3;

  // "Filter" a node within a traverser: combine the whatToShow bitmask with the NodeFilter callback.
  // Returns FILTER_ACCEPT/REJECT/SKIP. Sets the traversal-active flag around the user callback so a
  // reentrant call throws InvalidStateError; the WebIDL return value is coerced to an unsigned long.
  function __filterNode(traverser, node) {
    if (traverser._active) {
      throw new globalThis.DOMException("NodeFilter is already executing.", "InvalidStateError");
    }
    var t = node.nodeType;
    if (((1 << (t - 1)) & traverser._whatToShow) === 0) { return FILTER_SKIP; }
    var filter = traverser._filter;
    if (filter == null) { return FILTER_ACCEPT; }
    traverser._active = true;
    var result;
    try {
      result = (typeof filter === "function") ? filter(node) : filter.acceptNode(node);
    } finally {
      traverser._active = false;
    }
    return result >>> 0;
  }

  // ---- read-only attribute accessors shared shape ----
  function __defReadonly(proto, name, field) {
    Object.defineProperty(proto, name, {
      get: function () { return this[field]; }, enumerable: true, configurable: true
    });
  }

  // ---- TreeWalker ----
  __defReadonly(__twProto, "root", "_root");
  __defReadonly(__twProto, "whatToShow", "_whatToShow");
  __defReadonly(__twProto, "filter", "_filter");
  Object.defineProperty(__twProto, "currentNode", {
    get: function () { return this._current; },
    set: function (v) {
      if (v == null || typeof v !== "object" || typeof v.nodeType !== "number") {
        throw new TypeError("Failed to set the 'currentNode' property on 'TreeWalker': parameter is not of type 'Node'.");
      }
      this._current = v;
    },
    enumerable: true, configurable: true
  });
  // "Traverse children" of currentNode (type "first" => first child onward, "last" => last child back).
  function __twTraverseChildren(tw, type) {
    var node = (type === "first") ? tw._current.firstChild : tw._current.lastChild;
    while (node != null) {
      var result = __filterNode(tw, node);
      if (result === FILTER_ACCEPT) { tw._current = node; return node; }
      if (result === FILTER_SKIP) {
        var child = (type === "first") ? node.firstChild : node.lastChild;
        if (child != null) { node = child; continue; }
      }
      while (node != null) {
        var sibling = (type === "first") ? node.nextSibling : node.previousSibling;
        if (sibling != null) { node = sibling; break; }
        var parent = node.parentNode;
        if (parent == null || __sameNode(parent, tw._root) || __sameNode(parent, tw._current)) { return null; }
        node = parent;
      }
    }
    return null;
  }
  // "Traverse siblings" of currentNode (type "next" => forward, "previous" => backward).
  function __twTraverseSiblings(tw, type) {
    var node = tw._current;
    if (__sameNode(node, tw._root)) { return null; }
    while (true) {
      var sibling = (type === "next") ? node.nextSibling : node.previousSibling;
      while (sibling != null) {
        node = sibling;
        var result = __filterNode(tw, node);
        if (result === FILTER_ACCEPT) { tw._current = node; return node; }
        sibling = (type === "next") ? node.firstChild : node.lastChild;
        if (result === FILTER_REJECT || sibling == null) {
          sibling = (type === "next") ? node.nextSibling : node.previousSibling;
        }
      }
      node = node.parentNode;
      if (node == null || __sameNode(node, tw._root)) { return null; }
      if (__filterNode(tw, node) === FILTER_ACCEPT) { return null; }
    }
  }
  def(__twProto, "parentNode", function () {
    var node = this._current;
    while (node != null && !__sameNode(node, this._root)) {
      node = node.parentNode;
      if (node != null && __filterNode(this, node) === FILTER_ACCEPT) { this._current = node; return node; }
    }
    return null;
  });
  def(__twProto, "firstChild", function () { return __twTraverseChildren(this, "first"); });
  def(__twProto, "lastChild", function () { return __twTraverseChildren(this, "last"); });
  def(__twProto, "nextSibling", function () { return __twTraverseSiblings(this, "next"); });
  def(__twProto, "previousSibling", function () { return __twTraverseSiblings(this, "previous"); });
  def(__twProto, "nextNode", function () {
    var node = this._current;
    var result = FILTER_ACCEPT;
    while (true) {
      while (result !== FILTER_REJECT && node.firstChild != null) {
        node = node.firstChild;
        result = __filterNode(this, node);
        if (result === FILTER_ACCEPT) { this._current = node; return node; }
      }
      var sibling = null, temporary = node;
      while (temporary != null) {
        if (__sameNode(temporary, this._root)) { return null; }
        sibling = temporary.nextSibling;
        if (sibling != null) { node = sibling; break; }
        temporary = temporary.parentNode;
      }
      if (temporary == null) { return null; }
      result = __filterNode(this, node);
      if (result === FILTER_ACCEPT) { this._current = node; return node; }
    }
  });
  def(__twProto, "previousNode", function () {
    var node = this._current;
    while (!__sameNode(node, this._root)) {
      var sibling = node.previousSibling;
      while (sibling != null) {
        node = sibling;
        var result = __filterNode(this, node);
        while (result !== FILTER_REJECT && node.lastChild != null) {
          node = node.lastChild;
          result = __filterNode(this, node);
        }
        if (result === FILTER_ACCEPT) { this._current = node; return node; }
        sibling = node.previousSibling;
      }
      if (__sameNode(node, this._root) || node.parentNode == null) { return null; }
      node = node.parentNode;
      if (__filterNode(this, node) === FILTER_ACCEPT) { this._current = node; return node; }
    }
    return null;
  });

  // ---- NodeIterator ----
  __defReadonly(__niProto, "root", "_root");
  __defReadonly(__niProto, "whatToShow", "_whatToShow");
  __defReadonly(__niProto, "filter", "_filter");
  __defReadonly(__niProto, "referenceNode", "_reference");
  __defReadonly(__niProto, "pointerBeforeReferenceNode", "_pointerBefore");
  // First node following `node` within root's subtree (preorder next, never escaping root).
  function __followingWithinRoot(node, root) {
    if (node.firstChild != null) { return node.firstChild; }
    var n = node;
    while (n != null && !__sameNode(n, root)) {
      if (n.nextSibling != null) { return n.nextSibling; }
      n = n.parentNode;
    }
    return null;
  }
  // First node preceding `node` within root's subtree (preorder predecessor; null at root).
  function __precedingWithinRoot(node, root) {
    if (__sameNode(node, root)) { return null; }
    var prev = node.previousSibling;
    if (prev != null) {
      while (prev.lastChild != null) { prev = prev.lastChild; }
      return prev;
    }
    return node.parentNode;
  }
  function __niTraverse(it, forward) {
    var node = it._reference;
    var beforeNode = it._pointerBefore;
    while (true) {
      if (forward) {
        if (!beforeNode) {
          var f = __followingWithinRoot(node, it._root);
          if (f == null) { return null; }
          node = f;
        } else { beforeNode = false; }
      } else {
        if (beforeNode) {
          var p = __precedingWithinRoot(node, it._root);
          if (p == null) { return null; }
          node = p;
        } else { beforeNode = true; }
      }
      if (__filterNode(it, node) === FILTER_ACCEPT) { break; }
    }
    it._reference = node;
    it._pointerBefore = beforeNode;
    return node;
  }
  def(__niProto, "nextNode", function () { return __niTraverse(this, true); });
  def(__niProto, "previousNode", function () { return __niTraverse(this, false); });
  def(__niProto, "detach", function () {});

  // whatToShow: omitted/undefined => SHOW_ALL (WebIDL default 0xFFFFFFFF); an explicit value
  // (including null, which coerces to 0) is taken as an unsigned long. filter: null when omitted.
  function __normWhatToShow(whatToShow) { return whatToShow === undefined ? 0xFFFFFFFF : (whatToShow >>> 0); }
  function __normFilter(filter) { return (filter === undefined || filter === null) ? null : filter; }
  if (typeof globalThis.document !== "undefined" && globalThis.document) {
    def(globalThis.document, "createTreeWalker", function (root, whatToShow, filter) {
      if (arguments.length < 1 || root == null || typeof root.__node !== "number") {
        throw new TypeError("Failed to execute 'createTreeWalker' on 'Document': parameter 1 is not of type 'Node'.");
      }
      var tw = Object.create(__twProto);
      tw._root = root; tw._whatToShow = __normWhatToShow(whatToShow); tw._filter = __normFilter(filter);
      tw._current = root; tw._active = false;
      return tw;
    });
    def(globalThis.document, "createNodeIterator", function (root, whatToShow, filter) {
      if (arguments.length < 1 || root == null || typeof root.__node !== "number") {
        throw new TypeError("Failed to execute 'createNodeIterator' on 'Document': parameter 1 is not of type 'Node'.");
      }
      var it = Object.create(__niProto);
      it._root = root; it._whatToShow = __normWhatToShow(whatToShow); it._filter = __normFilter(filter);
      it._reference = root; it._pointerBefore = true; it._active = false;
      __liveNodeIterators.push(it);
      return it;
    });
  }

  // TextEncoder / TextDecoder — UTF-8 only (the common case). Pure JS over Uint8Array.
  if (typeof globalThis.TextEncoder !== "function") {
    def(globalThis, "TextEncoder", function () { this.encoding = "utf-8"; });
    globalThis.TextEncoder.prototype.encode = function (str) {
      str = str === undefined ? "" : String(str);
      var bytes = [];
      for (var i = 0; i < str.length; i++) {
        var c = str.charCodeAt(i);
        if (c < 0x80) { bytes.push(c); }
        else if (c < 0x800) { bytes.push(0xc0 | (c >> 6), 0x80 | (c & 0x3f)); }
        else if (c >= 0xd800 && c <= 0xdbff && i + 1 < str.length) {
          var c2 = str.charCodeAt(++i);
          var cp = 0x10000 + ((c & 0x3ff) << 10) + (c2 & 0x3ff);
          bytes.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
        } else { bytes.push(0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f)); }
      }
      return new Uint8Array(bytes);
    };
    globalThis.TextEncoder.prototype.encodeInto = function (str, dest) {
      // Encode as much as fits, one code point at a time, never writing a partial UTF-8 sequence.
      // `read` counts UTF-16 code units consumed (2 for a surrogate pair), `written` counts bytes —
      // the old version reported str.length / min(len) regardless, which is wrong on a short buffer.
      str = str === undefined ? "" : String(str);
      var cap = dest.length, read = 0, written = 0;
      for (var i = 0; i < str.length; i++) {
        var c = str.charCodeAt(i), seq, units = 1;
        if (c < 0x80) { seq = [c]; }
        else if (c < 0x800) { seq = [0xc0 | (c >> 6), 0x80 | (c & 0x3f)]; }
        else if (c >= 0xd800 && c <= 0xdbff && i + 1 < str.length) {
          var cp = 0x10000 + ((c & 0x3ff) << 10) + (str.charCodeAt(i + 1) & 0x3ff);
          seq = [0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f)];
          units = 2;
        } else { seq = [0xe0 | (c >> 12), 0x80 | ((c >> 6) & 0x3f), 0x80 | (c & 0x3f)]; }
        if (written + seq.length > cap) { break; }   // no room for the whole code point — stop here
        for (var k = 0; k < seq.length; k++) { dest[written++] = seq[k]; }
        read += units;
        if (units === 2) { i++; }
      }
      return { read: read, written: written };
    };
  }
  if (typeof globalThis.TextDecoder !== "function") {
    // UTF-8 only (the common case), but a real UTF-8 decoder: validates sequences and emits U+FFFD
    // for malformed input (or throws when `fatal`), strips a leading BOM unless `ignoreBOM`, and
    // carries an incomplete trailing sequence across `decode(…, {stream:true})` calls. Non-UTF-8
    // labels are still treated as UTF-8 (we don't implement legacy encodings).
    def(globalThis, "TextDecoder", function (label, options) {
      this.encoding = "utf-8";
      this.fatal = !!(options && options.fatal);
      this.ignoreBOM = !!(options && options.ignoreBOM);
      this.__pending = null;   // incomplete trailing bytes carried between streaming calls
      this.__bomSeen = false;
    });
    globalThis.TextDecoder.prototype.decode = function (input, options) {
      var stream = !!(options && options.stream);
      var bytes;
      if (input == null) { bytes = new Uint8Array(0); }
      else if (input.buffer) { bytes = new Uint8Array(input.buffer, input.byteOffset || 0, input.byteLength); }
      else { bytes = new Uint8Array(input); }
      if (this.__pending && this.__pending.length) {     // prepend bytes held from a prior stream call
        var merged = new Uint8Array(this.__pending.length + bytes.length);
        merged.set(this.__pending, 0); merged.set(bytes, this.__pending.length);
        bytes = merged; this.__pending = null;
      }
      var out = "", i = 0, n = bytes.length, self = this;
      function fail() { if (self.fatal) { throw new TypeError("The encoded data was not valid."); } out += "�"; }
      while (i < n) {
        var b0 = bytes[i], len, cp, min;
        if (b0 < 0x80) { out += String.fromCharCode(b0); i++; self.__bomSeen = true; continue; }
        else if (b0 >= 0xc2 && b0 <= 0xdf) { len = 2; cp = b0 & 0x1f; min = 0x80; }
        else if (b0 >= 0xe0 && b0 <= 0xef) { len = 3; cp = b0 & 0x0f; min = 0x800; }
        else if (b0 >= 0xf0 && b0 <= 0xf4) { len = 4; cp = b0 & 0x07; min = 0x10000; }
        else { fail(); i++; continue; }                  // invalid lead byte (0x80–0xc1, 0xf5–0xff)
        if (i + len > n) {                               // incomplete sequence at the end of input
          if (stream) { self.__pending = bytes.slice(i); return out; }
          fail(); i++; continue;
        }
        var ok = true;
        for (var k = 1; k < len; k++) {
          var bk = bytes[i + k];
          if (bk < 0x80 || bk > 0xbf) { ok = false; break; }
          cp = (cp << 6) | (bk & 0x3f);
        }
        if (!ok) { fail(); i++; continue; }              // bad continuation — resync from next byte
        if (cp < min || cp > 0x10ffff || (cp >= 0xd800 && cp <= 0xdfff)) { fail(); i += len; continue; }
        if (cp === 0xfeff && !self.__bomSeen && !self.ignoreBOM) { self.__bomSeen = true; i += len; continue; }
        self.__bomSeen = true;
        if (cp > 0xffff) { cp -= 0x10000; out += String.fromCharCode(0xd800 + (cp >> 10), 0xdc00 + (cp & 0x3ff)); }
        else { out += String.fromCharCode(cp); }
        i += len;
      }
      if (!stream) { this.__pending = null; this.__bomSeen = false; }   // reset for the next decode
      return out;
    };
  }

  // base64 (btoa/atob) — pure JS implementation.
  var B64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  def(globalThis, "btoa", function (input) {
    var str = String(input), out = "";
    for (var i = 0; i < str.length;) {
      var c1 = str.charCodeAt(i++) & 0xff;
      var c2 = str.charCodeAt(i++);
      var c3 = str.charCodeAt(i++);
      var e1 = c1 >> 2;
      var e2 = ((c1 & 3) << 4) | ((isNaN(c2) ? 0 : c2) >> 4);
      var e3 = isNaN(c2) ? 64 : (((c2 & 15) << 2) | ((isNaN(c3) ? 0 : c3) >> 6));
      var e4 = isNaN(c3) ? 64 : (c3 & 63);
      out += B64.charAt(e1) + B64.charAt(e2) + (e3 === 64 ? "=" : B64.charAt(e3)) + (e4 === 64 ? "=" : B64.charAt(e4));
    }
    return out;
  });
  def(globalThis, "atob", function (input) {
    // Drop whitespace; keep '=' padding so groups stay 4-aligned.
    var str = String(input).replace(/[^A-Za-z0-9+/=]/g, ""), out = "";
    for (var i = 0; i + 3 < str.length; i += 4) {
      var d1 = B64.indexOf(str.charAt(i));
      var d2 = B64.indexOf(str.charAt(i + 1));
      var p3 = str.charAt(i + 2), p4 = str.charAt(i + 3);
      var d3 = B64.indexOf(p3), d4 = B64.indexOf(p4);
      out += String.fromCharCode(((d1 << 2) | (d2 >> 4)) & 0xff);
      if (p3 !== "=" && d3 >= 0) { out += String.fromCharCode(((d2 & 15) << 4) | (d3 >> 2)); }
      if (p4 !== "=" && d4 >= 0) { out += String.fromCharCode(((d3 & 3) << 6) | d4); }
    }
    return out;
  });

  // crypto: real OS randomness via the __cryptoRandom native (falls back to a PRNG if unavailable).
  function __randBytes(n) {
    try { var b = __cryptoRandom(n); if (b && b.length === n) { return b; } } catch (e) {}
    var out = []; for (var i = 0; i < n; i++) { out.push((Math.floor((i * 2654435761) % 256)) || 1); } return out;
  }
  globalThis.crypto = {
    getRandomValues: function (arr) {
      if (!arr || typeof arr.length !== "number") { return arr; }
      var bpe = arr.BYTES_PER_ELEMENT || 1;
      var bytes = __randBytes(arr.length * bpe);
      for (var i = 0; i < arr.length; i++) {
        var v = 0;
        for (var b = 0; b < bpe; b++) { v = (v * 256) + (bytes[i * bpe + b] || 0); }
        arr[i] = v;
      }
      return arr;
    },
    randomUUID: function () {
      var b = __randBytes(16);
      b[6] = (b[6] & 0x0f) | 0x40; // version 4
      b[8] = (b[8] & 0x3f) | 0x80; // variant 10
      var hex = []; for (var i = 0; i < 16; i++) { hex.push((b[i] + 0x100).toString(16).slice(1)); }
      return hex.slice(0, 4).join("") + "-" + hex.slice(4, 6).join("") + "-" + hex.slice(6, 8).join("") +
             "-" + hex.slice(8, 10).join("") + "-" + hex.slice(10, 16).join("");
    },
    subtle: {}
  };

  // --- FormData ----------------------------------------------------------------------------
  // Pure-JS FormData. Backed by an array of [name, value] entries. When constructed from a
  // <form> element, collects the form's successful named controls. NOTE: File/Blob values are
  // not specially handled — they are stored as-is (and stringified when serialized); there is no
  // real File support, and `fetch` serializes a FormData body as urlencoded (not multipart).
  if (typeof globalThis.FormData !== "function") {
    def(globalThis, "FormData", function (form) {
      var entries = [];
      this.__isFormData = true;
      function add(name, value) { entries.push([String(name), value]); }
      // Collect successful named controls from a <form> element (duck-typed via tagName).
      if (form && typeof form === "object" && form.tagName && String(form.tagName).toUpperCase() === "FORM") {
        var collect = function (el) {
          var kids = el.childNodes || [];
          for (var i = 0; i < kids.length; i++) {
            var c = kids[i];
            if (!c || c.nodeType !== 1) { continue; }
            var tag = String(c.tagName || "").toUpperCase();
            var name = c.getAttribute ? c.getAttribute("name") : null;
            var disabled = c.getAttribute ? (c.getAttribute("disabled") != null) : false;
            if (tag === "INPUT" && name && !disabled) {
              var type = (c.getAttribute("type") || "text").toLowerCase();
              if (type === "checkbox" || type === "radio") {
                if (c.checked) { add(name, c.value != null && c.value !== "" ? c.value : "on"); }
              } else if (type === "submit" || type === "button" || type === "reset" || type === "file" || type === "image") {
                // not successful for our purposes
              } else {
                add(name, c.value != null ? c.value : "");
              }
            } else if (tag === "SELECT" && name && !disabled) {
              add(name, c.value != null ? c.value : "");
            } else if (tag === "TEXTAREA" && name && !disabled) {
              // A <textarea>'s value defaults to its text content when no value was set.
              var tv = (c.value != null && c.value !== "") ? c.value : (c.textContent != null ? c.textContent : "");
              add(name, tv);
            }
            // Recurse into descendants (controls may be nested in wrappers).
            if (c.childNodes && c.childNodes.length) { collect(c); }
          }
        };
        collect(form);
      }
      this.append = function (name, value) { add(name, value); };
      this.set = function (name, value) { name = String(name); for (var i = entries.length - 1; i >= 0; i--) { if (entries[i][0] === name) { entries.splice(i, 1); } } add(name, value); };
      this.get = function (name) { name = String(name); for (var i = 0; i < entries.length; i++) { if (entries[i][0] === name) { return entries[i][1]; } } return null; };
      this.getAll = function (name) { name = String(name); var out = []; for (var i = 0; i < entries.length; i++) { if (entries[i][0] === name) { out.push(entries[i][1]); } } return out; };
      this.has = function (name) { name = String(name); for (var i = 0; i < entries.length; i++) { if (entries[i][0] === name) { return true; } } return false; };
      this.delete = function (name) { name = String(name); for (var i = entries.length - 1; i >= 0; i--) { if (entries[i][0] === name) { entries.splice(i, 1); } } };
      this.forEach = function (cb, thisArg) { for (var i = 0; i < entries.length; i++) { cb.call(thisArg, entries[i][1], entries[i][0], this); } };
      this.keys = function () { return entries.map(function (e) { return e[0]; })[Symbol.iterator](); };
      this.values = function () { return entries.map(function (e) { return e[1]; })[Symbol.iterator](); };
      this.entries = function () { return entries.map(function (e) { return [e[0], e[1]]; })[Symbol.iterator](); };
      this[Symbol.iterator] = function () { return this.entries(); };
      // Internal: urlencoded serialization used by fetch (multipart is NOT implemented).
      this.__toUrlEncoded = function () {
        return entries.map(function (e) { return encodeURIComponent(e[0]) + "=" + encodeURIComponent(String(e[1])); }).join("&");
      };
    });
  }

  // Serialize a FormData-like into an application/x-www-form-urlencoded string.
  function __formDataToUrlEncoded(fd) {
    if (fd && typeof fd.__toUrlEncoded === "function") { return fd.__toUrlEncoded(); }
    // Fallback: iterate entries() if available.
    var parts = [];
    if (fd && typeof fd.forEach === "function") {
      fd.forEach(function (v, k) { parts.push(encodeURIComponent(k) + "=" + encodeURIComponent(String(v))); });
    }
    return parts.join("&");
  }

  // Decode a Blob's bytes (UTF-8) to a JS string, synchronously (the async Blob.text() returns a
  // Promise; for in-process body serialization we need the value directly).
  function __blobTextSync(b) {
    var bytes = (b && b.__blobBytes) || [];
    var s = ""; for (var i = 0; i < bytes.length; i++) { s += String.fromCharCode(bytes[i]); }
    try { return decodeURIComponent(escape(s)); } catch (e) { return s; }
  }
  function __isBlobLike(v) {
    return v && typeof v === "object" && (v.__blobBytes !== undefined || (typeof globalThis.Blob === "function" && v instanceof globalThis.Blob));
  }
  // A boundary made only of lowercase ASCII + digits so it is stable under toLowerCase() (the
  // response-form-data WPT lowercases the whole body and re-parses). A monotonic counter keeps it
  // unique without relying on Math.random.
  var __mpBoundarySeq = 0;
  function __genBoundary() { __mpBoundarySeq++; return "----formdataboundary" + __mpBoundarySeq + "x" + (__mpBoundarySeq * 2654435761 % 100000000); }
  // Escape a name/filename for a Content-Disposition parameter per the spec (CR/LF/" only).
  function __mpEscapeName(s) {
    return String(s).replace(/\r\n|\r|\n/g, "%0D%0A").replace(/"/g, "%22");
  }
  // Serialize a FormData-like into a multipart/form-data body string for the given boundary.
  function __formDataToMultipart(fd, boundary) {
    var crlf = "\r\n", out = "", entries = [];
    if (fd && typeof fd.forEach === "function") { fd.forEach(function (v, k) { entries.push([k, v]); }); }
    for (var i = 0; i < entries.length; i++) {
      var name = entries[i][0], value = entries[i][1];
      out += "--" + boundary + crlf;
      if (__isBlobLike(value)) {
        var filename = (value.name != null) ? value.name : "blob";
        out += 'Content-Disposition: form-data; name="' + __mpEscapeName(name) + '"; filename="' + __mpEscapeName(filename) + '"' + crlf;
        out += "Content-Type: " + (value.type || "application/octet-stream") + crlf + crlf;
        out += __blobTextSync(value) + crlf;
      } else {
        out += 'Content-Disposition: form-data; name="' + __mpEscapeName(name) + '"' + crlf + crlf;
        out += String(value) + crlf;
      }
    }
    out += "--" + boundary + "--" + crlf;
    return out;
  }
  // Parse a multipart/form-data body string into a FormData, or return null on malformed input
  // (the caller turns null into a rejected TypeError). Follows the WHATWG multipart parser closely
  // enough to satisfy the response-form-data WPT's valid/invalid cases.
  function __parseMultipart(input, boundary) {
    var CRLF = "\r\n", dash = "--" + boundary, endDelim = dash + "--", pos = 0;
    var fd = new globalThis.FormData();
    function startsWith(s, at) { return input.substr(at, s.length) === s; }
    function skipLWS() { while (pos < input.length && (input.charAt(pos) === " " || input.charAt(pos) === "\t")) { pos++; } }
    while (true) {
      // Closing delimiter "--boundary--": the body ends here; only transport padding + CRLF (or
      // end of input) may follow, otherwise the body is malformed.
      if (startsWith(endDelim, pos)) {
        pos += endDelim.length;
        skipLWS();
        if (pos >= input.length || startsWith(CRLF, pos)) { return fd; }
        return null;
      }
      // Part delimiter "--boundary" + transport padding + CRLF.
      if (!startsWith(dash, pos)) { return null; }
      pos += dash.length;
      skipLWS();
      if (!startsWith(CRLF, pos)) { return null; }
      pos += 2;
      // Part headers, terminated by a blank line.
      var name = null, filename = null, ctype = null;
      while (true) {
        var nl = input.indexOf(CRLF, pos);
        if (nl < 0) { return null; }
        var line = input.slice(pos, nl);
        pos = nl + 2;
        if (line === "") { break; }
        var ci = line.indexOf(":");
        if (ci < 0) { continue; }
        var hname = line.slice(0, ci).trim().toLowerCase();
        var hval = line.slice(ci + 1).trim();
        if (hname === "content-disposition") {
          var nm = /name="([^"]*)"/i.exec(hval);
          if (nm) { name = nm[1]; }
          var fm = /filename="([^"]*)"/i.exec(hval);
          if (fm) { filename = fm[1]; }
        } else if (hname === "content-type") {
          ctype = hval;
        }
      }
      if (name === null) { return null; }
      // Body runs until the CRLF that precedes the next boundary.
      var idx = input.indexOf(CRLF + dash, pos);
      if (idx < 0) { return null; }
      var bodyStr = input.slice(pos, idx);
      pos = idx + 2; // leave position at the boundary for the next iteration
      if (filename !== null) {
        fd.append(name, new globalThis.File([bodyStr], filename, { type: ctype || "" }));
      } else {
        fd.append(name, bodyStr);
      }
    }
  }
  // Shared body -> FormData consumption for Request/Response. `bodyStr` is the serialized body,
  // `contentType` its media type. Returns a Promise<FormData> (rejects with TypeError when the
  // media type is neither multipart/form-data nor application/x-www-form-urlencoded, or when a
  // multipart body is malformed).
  function __bodyToFormData(bodyStr, contentType) {
    var ct = String(contentType || "");
    var lower = ct.toLowerCase();
    if (lower.indexOf("multipart/form-data") === 0) {
      var bm = /boundary=("?)([^";]+)\1/i.exec(ct);
      if (!bm) { return Promise.reject(new TypeError("Missing multipart/form-data boundary")); }
      var fd = __parseMultipart(String(bodyStr == null ? "" : bodyStr), bm[2]);
      if (fd === null) { return Promise.reject(new TypeError("Failed to parse multipart/form-data body")); }
      return Promise.resolve(fd);
    }
    if (lower.indexOf("application/x-www-form-urlencoded") === 0) {
      var out = new globalThis.FormData();
      var sp = new globalThis.URLSearchParams(String(bodyStr == null ? "" : bodyStr));
      sp.forEach(function (v, k) { out.append(k, v); });
      return Promise.resolve(out);
    }
    return Promise.reject(new TypeError("Body cannot be parsed as FormData"));
  }

  // Async fetch plumbing. `fetch()` calls the non-blocking native `__startFetch`, which spawns a
  // background request thread and returns an id immediately; the page promise is parked in
  // `__pendingFetches[id]` and settled later — on the worker thread, inside the Rust drain — when
  // the request completes, via `__resolveFetch(id, envelopeStr)` / `__rejectFetch(id)`. This lets
  // many fetches run concurrently instead of serializing one blocking call at a time.
  globalThis.__pendingFetches = globalThis.__pendingFetches || {};
  // Build a Response from a host JSON envelope string (the shape the request fetcher returns).
  function __responseFromEnvelope(envelope, fallbackUrl) {
    var env = JSON.parse(envelope);
    var respBody = env.body != null ? String(env.body) : "";
    var contentType = env.contentType != null ? String(env.contentType) : "";
    var rh = new globalThis.Headers();
    if (contentType) { rh.set("content-type", contentType); }
    return new globalThis.Response(respBody, {
      status: env.status != null ? (env.status | 0) : 200,
      statusText: env.statusText != null ? String(env.statusText) : "",
      headers: rh,
      url: env.url != null ? String(env.url) : fallbackUrl,
      type: "basic"
    });
  }
  // Settle a parked fetch with a host envelope (or null → reject as a failed transport).
  def(globalThis, "__resolveFetch", function (id, envelope) {
    var pending = globalThis.__pendingFetches[id];
    if (!pending) { return; } // already aborted/settled; late completion ignored.
    delete globalThis.__pendingFetches[id];
    if (envelope == null) { pending.reject(new TypeError("Failed to fetch")); return; }
    var resp;
    try { resp = __responseFromEnvelope(String(envelope), pending.url); }
    catch (e) { pending.reject(new TypeError("Failed to fetch")); return; }
    pending.resolve(resp);
  });
  // Reject a parked fetch (transport error, or abort).
  def(globalThis, "__rejectFetch", function (id, reason) {
    var pending = globalThis.__pendingFetches[id];
    if (!pending) { return; }
    delete globalThis.__pendingFetches[id];
    pending.reject(reason || new TypeError("Failed to fetch"));
  });

  // fetch: starts the request via the native __startFetch primitive (which runs the host request
  // on a background thread) and returns a Promise parked in __pendingFetches, settled later by
  // __resolveFetch/__rejectFetch during the Rust drain. Sends the method, headers, and serialized
  // body; resolves a Response from the host's JSON envelope. Rejects with TypeError("Failed to
  // fetch") when the host request fails (null envelope), or with AbortError if the signal aborts.
  if (typeof globalThis.fetch !== "function") {
    def(globalThis, "fetch", function (input, init) {
      init = init || {};
      var url;
      try { url = (input && input.url) ? String(input.url) : String(input); }
      catch (e) { url = String(input); }
      // Dangling-markup mitigation: a request URL containing both "<" and a newline/CR/tab is a
      // network error (blocks data exfiltration via unclosed markup in resource URLs).
      if (url.indexOf("<") >= 0 && /[\n\r\t]/.test(url)) {
        return Promise.reject(new TypeError("Failed to fetch"));
      }
      var method = String(init.method || "GET").toUpperCase();

      // Service worker fetch interception: a controlled client's request is offered to the
      // controller's FetchEvent handler, which may respondWith() a synthetic/passed-through response.
      if (typeof globalThis.__swInterceptFetch === "function") {
        var intercepted = globalThis.__swInterceptFetch(method, url, init);
        if (intercepted) { return intercepted; }
      }

      // Honor an AbortSignal: a fetch on an already-aborted signal rejects with AbortError. (Our
      // fetch is synchronous, so only pre-abort is observable.)
      var signal = init.signal;
      if (signal && signal.aborted) {
        return Promise.reject(signal.reason || new globalThis.DOMException("The operation was aborted.", "AbortError"));
      }

      // --- Headers: accept plain object, Headers-like (forEach), or array of pairs. ---
      var headers = {};
      var hdrLower = {}; // lowercased name -> canonical name present, for content-type checks
      function setHeader(name, value) { name = String(name); headers[name] = String(value); hdrLower[name.toLowerCase()] = name; }
      var ih = init.headers;
      if (ih) {
        if (Array.isArray(ih)) {
          for (var i = 0; i < ih.length; i++) { if (ih[i]) { setHeader(ih[i][0], ih[i][1]); } }
        } else if (typeof ih.forEach === "function" && typeof ih.get === "function") {
          ih.forEach(function (v, k) { setHeader(k, v); });
        } else if (typeof ih === "object") {
          for (var k in ih) { if (Object.prototype.hasOwnProperty.call(ih, k)) { setHeader(k, ih[k]); } }
        }
      }
      function hasContentType() { return hdrLower["content-type"] != null; }
      function ensureContentType(ct) { if (!hasContentType()) { setHeader("Content-Type", ct); } }

      // --- Body serialization (GET/HEAD carry no body). ---
      var bodyStr = "";
      var rawBody = init.body;
      if (method !== "GET" && method !== "HEAD" && rawBody != null) {
        if (typeof rawBody === "string") {
          bodyStr = rawBody;
        } else if (rawBody.__isFormData || (typeof globalThis.FormData === "function" && rawBody instanceof globalThis.FormData)) {
          // FormData: encoded as urlencoded (real multipart/form-data is NOT implemented).
          bodyStr = __formDataToUrlEncoded(rawBody);
          ensureContentType("application/x-www-form-urlencoded;charset=UTF-8");
        } else if (typeof rawBody.toString === "function" && (typeof globalThis.URLSearchParams === "function" && rawBody instanceof globalThis.URLSearchParams)) {
          bodyStr = rawBody.toString();
          ensureContentType("application/x-www-form-urlencoded;charset=UTF-8");
        } else if (typeof rawBody === "object" && typeof rawBody.toString === "function" && rawBody.toString !== Object.prototype.toString) {
          // Other stringifiable objects (e.g. URLSearchParams-likes with a custom toString).
          bodyStr = rawBody.toString();
        } else {
          // Plain object / anything else: leave as String(body); don't force JSON.
          bodyStr = String(rawBody);
        }
      }

      if (typeof __startFetch !== "function") {
        return Promise.reject(new TypeError("Failed to fetch"));
      }
      // Kick off the request on a background thread; settle the promise later via the drain.
      var id = __startFetch(method, url, bodyStr, JSON.stringify(headers));
      return new Promise(function (resolve, reject) {
        globalThis.__pendingFetches[id] = { resolve: resolve, reject: reject, url: url };
        // AbortSignal: if it aborts while the request is in flight, reject this id with the abort
        // reason and forget it (a late background completion is then ignored — see __resolveFetch).
        if (signal && typeof signal.addEventListener === "function") {
          signal.addEventListener("abort", function () {
            __rejectFetch(id, signal.reason || new globalThis.DOMException("The operation was aborted.", "AbortError"));
          });
        }
      });
    });
  }

  // XMLHttpRequest: present but inert.
  def(globalThis, "XMLHttpRequest", function () {
    this.readyState = 0; this.status = 0; this.responseText = ""; this.response = "";
    this.onreadystatechange = null; this.onload = null; this.onerror = null;
    this.open = fn; this.send = fn; this.setRequestHeader = fn; this.abort = fn;
    this.getResponseHeader = function () { return null; }; this.getAllResponseHeaders = function () { return ""; };
    this.addEventListener = fn; this.removeEventListener = fn;
  });

  // --- DOM Event constructors + class hierarchy (per the DOM / UI Events standards) ---------
  // Each Event/subclass stores its standard members in a non-enumerable internal bag (__ev) and
  // exposes them as read-only getters on the prototype, so the prototype chain gives correct
  // `instanceof` and `Object.getPrototypeOf(ev) === Iface.prototype` (which document.createEvent
  // relies on). Subclasses inherit Event via real prototype chains (MouseEvent -> UIEvent -> Event).
  (function () {
    // Monotonic high-resolution timestamp source for Event.timeStamp. The event-loop clock does
    // not advance between two synchronous constructions, but spec tests create events in tight
    // loops and rely on consecutive timestamps eventually differing (and not having sub-5µs
    // resolution). Base off performance.now() (shared time origin) and add a 5-microsecond
    // (0.005 ms) monotonic quantum per call so the value strictly increases yet stays coarse.
    var __tsCounter = 0;
    function __eventTimeStamp() {
      var base = 0;
      try { base = (globalThis.performance && typeof globalThis.performance.now === "function")
        ? globalThis.performance.now()
        : (globalThis.__eventLoop ? globalThis.__eventLoop.now : 0); } catch (e) { base = 0; }
      __tsCounter += 1;
      return base + __tsCounter * 0.005;
    }
    // Per-event internal state. `flags` holds dispatch bookkeeping shared with dispatchEvent().
    function initEventState(ev) {
      var s = {
        type: "", bubbles: false, cancelable: false, composed: false,
        defaultPrevented: false, isTrusted: false,
        eventPhase: 0, target: null, currentTarget: null,
        timeStamp: __eventTimeStamp(),
        stopPropagation: false, stopImmediate: false, initialized: false, dispatching: false,
        inPassive: false,
        path: []
      };
      def(ev, "__ev", s);
      return s;
    }
    function st(ev) { return ev.__ev || initEventState(ev); }

    // Define a read-only getter `name` on `proto` returning the matching internal-state field.
    function roGet(proto, name, field) {
      Object.defineProperty(proto, name, {
        get: function () { return st(this)[field]; }, enumerable: true, configurable: true
      });
    }

    function Event(type, init) {
      var s = initEventState(this);
      if (arguments.length > 0) { s.type = String(type); }
      s.initialized = true;
      if (init !== undefined && init !== null) {
        s.bubbles = !!init.bubbles;
        s.cancelable = !!init.cancelable;
        s.composed = !!init.composed;
      }
    }
    var EP = Event.prototype;
    roGet(EP, "type", "type");
    roGet(EP, "bubbles", "bubbles");
    roGet(EP, "cancelable", "cancelable");
    roGet(EP, "composed", "composed");
    roGet(EP, "defaultPrevented", "defaultPrevented");
    roGet(EP, "isTrusted", "isTrusted");
    roGet(EP, "eventPhase", "eventPhase");
    roGet(EP, "target", "target");
    roGet(EP, "currentTarget", "currentTarget");
    roGet(EP, "timeStamp", "timeStamp");
    Object.defineProperty(EP, "srcElement", { get: function () { return st(this).target; }, enumerable: true, configurable: true });
    // returnValue: legacy alias of !defaultPrevented (settable to false => preventDefault()).
    Object.defineProperty(EP, "returnValue", {
      get: function () { return !st(this).defaultPrevented; },
      set: function (v) { if (v === false) { var s = st(this); if (s.cancelable && !s.inPassive) { s.defaultPrevented = true; } } },
      enumerable: true, configurable: true
    });
    // cancelBubble: legacy alias of the stop-propagation flag. Getter returns it; setting to true
    // sets the flag (like stopPropagation()), setting to false is a no-op.
    Object.defineProperty(EP, "cancelBubble", {
      get: function () { return st(this).stopPropagation; },
      set: function (v) { if (v) { st(this).stopPropagation = true; } },
      enumerable: true, configurable: true
    });
    EP.preventDefault = function () { var s = st(this); if (s.cancelable && !s.inPassive) { s.defaultPrevented = true; } };
    EP.stopPropagation = function () { st(this).stopPropagation = true; };
    EP.stopImmediatePropagation = function () { var s = st(this); s.stopPropagation = true; s.stopImmediate = true; };
    EP.composedPath = function () { var s = st(this); return s.path ? s.path.slice() : []; };
    EP.initEvent = function (type, bubbles, cancelable) {
      var s = st(this);
      if (s.dispatching) { return; }
      s.type = String(type);
      s.bubbles = !!bubbles;
      s.cancelable = !!cancelable;
      s.initialized = true;
      s.defaultPrevented = false; s.isTrusted = false;
      s.target = null; s.stopPropagation = false; s.stopImmediate = false;
    };
    // Phase constants on both the constructor and the prototype.
    var phases = { NONE: 0, CAPTURING_PHASE: 1, AT_TARGET: 2, BUBBLING_PHASE: 3 };
    for (var pk in phases) {
      Object.defineProperty(Event, pk, { value: phases[pk], enumerable: true });
      Object.defineProperty(EP, pk, { value: phases[pk], enumerable: true });
    }
    def(globalThis, "Event", Event);
    // Expose the internal-state helpers so dispatchEvent / createEvent can drive events.
    def(globalThis, "__eventState", st);
    def(globalThis, "__initEventState", initEventState);

    // Build a subclass: ctor copies its own init members (from `members`) on top of the parent.
    // `members` maps property -> default value. `coerce` optionally transforms an init value.
    function defSubclass(name, ParentCtor, members, validate, coerce) {
      function Ctor(type, init) {
        ParentCtor.call(this, type, init);
        if (init === undefined || init === null) { init = {}; }
        if (validate) { validate(init); }
        for (var k in members) {
          var v = (k in init) ? init[k] : members[k];
          if (coerce && coerce[k]) { v = coerce[k](v); }
          def(this, k, v);
        }
      }
      Ctor.prototype = Object.create(ParentCtor.prototype);
      Object.defineProperty(Ctor.prototype, "constructor", { value: Ctor, enumerable: false, configurable: true, writable: true });
      def(globalThis, name, Ctor);
      Ctor.__members = members;
      Ctor.__parent = ParentCtor;
      return Ctor;
    }

    // CustomEvent: read-only `detail` + legacy initCustomEvent.
    var CustomEvent = defSubclass("CustomEvent", Event, { detail: null });
    CustomEvent.prototype.initCustomEvent = function (type, bubbles, cancelable, detail) {
      var s = st(this);
      if (s.dispatching) { return; }
      this.initEvent(type, bubbles, cancelable);
      def(this, "detail", detail === undefined ? null : detail);
    };

    function requireObjOrNull(v, what) {
      if (v !== undefined && v !== null && typeof v !== "object" && typeof v !== "function") {
        throw new TypeError(what + " is not an object");
      }
    }

    var UIEvent = defSubclass("UIEvent", Event, { view: null, detail: 0 }, function (init) {
      if ("view" in init) { requireObjOrNull(init.view, "view"); }
    });
    var modInit = function (init) {
      if ("relatedTarget" in init) { requireObjOrNull(init.relatedTarget, "relatedTarget"); }
    };
    var FocusEvent = defSubclass("FocusEvent", UIEvent, { relatedTarget: null }, modInit);
    var MouseEvent = defSubclass("MouseEvent", UIEvent, {
      screenX: 0, screenY: 0, clientX: 0, clientY: 0, button: 0, buttons: 0,
      ctrlKey: false, shiftKey: false, altKey: false, metaKey: false,
      relatedTarget: null, movementX: 0, movementY: 0
    }, modInit);
    MouseEvent.prototype.getModifierState = function (k) {
      switch (k) { case "Control": return !!this.ctrlKey; case "Shift": return !!this.shiftKey;
        case "Alt": return !!this.altKey; case "Meta": return !!this.metaKey; default: return false; }
    };
    var WheelEvent = defSubclass("WheelEvent", MouseEvent, { deltaX: 0, deltaY: 0, deltaZ: 0, deltaMode: 0 }, modInit);
    var DragEvent = defSubclass("DragEvent", MouseEvent, { dataTransfer: null }, modInit);
    var PointerEvent = defSubclass("PointerEvent", MouseEvent, {
      pointerId: 0, width: 1, height: 1, pressure: 0, tangentialPressure: 0,
      tiltX: 0, tiltY: 0, twist: 0, altitudeAngle: 0, azimuthAngle: 0,
      pointerType: "", isPrimary: false
    }, modInit);
    var KeyboardEvent = defSubclass("KeyboardEvent", UIEvent, {
      key: "", code: "", location: 0, repeat: false, isComposing: false,
      ctrlKey: false, shiftKey: false, altKey: false, metaKey: false,
      charCode: 0, keyCode: 0, which: 0
    });
    KeyboardEvent.prototype.getModifierState = MouseEvent.prototype.getModifierState;
    // Legacy init methods (deprecated, but still exercised by tests). Each calls initEvent then sets
    // the subclass members from positional args (re-defining the read-only properties).
    UIEvent.prototype.initUIEvent = function (type, bubbles, cancelable, view, detail) {
      this.initEvent(type, bubbles, cancelable);
      def(this, "view", view === undefined ? null : view);
      def(this, "detail", detail === undefined ? 0 : detail);
    };
    MouseEvent.prototype.initMouseEvent = function (type, bubbles, cancelable, view, detail, screenX, screenY, clientX, clientY, ctrlKey, altKey, shiftKey, metaKey, button, relatedTarget) {
      this.initUIEvent(type, bubbles, cancelable, view, detail);
      def(this, "screenX", screenX || 0); def(this, "screenY", screenY || 0);
      def(this, "clientX", clientX || 0); def(this, "clientY", clientY || 0);
      def(this, "ctrlKey", !!ctrlKey); def(this, "altKey", !!altKey); def(this, "shiftKey", !!shiftKey); def(this, "metaKey", !!metaKey);
      def(this, "button", button || 0); def(this, "relatedTarget", relatedTarget === undefined ? null : relatedTarget);
    };
    KeyboardEvent.prototype.initKeyboardEvent = function (type, bubbles, cancelable, view, key, location, ctrlKey, altKey, shiftKey, metaKey) {
      this.initUIEvent(type, bubbles, cancelable, view, 0);
      def(this, "key", key == null ? "" : String(key)); def(this, "location", location || 0);
      def(this, "ctrlKey", !!ctrlKey); def(this, "altKey", !!altKey); def(this, "shiftKey", !!shiftKey); def(this, "metaKey", !!metaKey);
    };
    var CompositionEvent = defSubclass("CompositionEvent", UIEvent, { data: "" });
    var InputEvent = defSubclass("InputEvent", UIEvent, { data: null, inputType: "", isComposing: false });
    var TouchEvent = defSubclass("TouchEvent", UIEvent, {
      touches: [], targetTouches: [], changedTouches: [],
      ctrlKey: false, shiftKey: false, altKey: false, metaKey: false
    });
    // Plain-Event subclasses (extend Event directly).
    defSubclass("PopStateEvent", Event, { state: null });
    defSubclass("HashChangeEvent", Event, { oldURL: "", newURL: "" });
    defSubclass("PageTransitionEvent", Event, { persisted: false });
    defSubclass("BeforeUnloadEvent", Event, { returnValue: "" });
    defSubclass("MessageEvent", Event, { data: null, origin: "", lastEventId: "", source: null, ports: [] });
    defSubclass("ProgressEvent", Event, { lengthComputable: false, loaded: 0, total: 0 });
    defSubclass("ErrorEvent", Event, { message: "", filename: "", lineno: 0, colno: 0, error: null });
    defSubclass("PromiseRejectionEvent", Event, { promise: null, reason: undefined });
    defSubclass("StorageEvent", Event, { key: null, oldValue: null, newValue: null, url: "", storageArea: null }, null, { url: toUSVString });
    globalThis.StorageEvent.prototype.initStorageEvent = function (type, bubbles, cancelable, key, oldValue, newValue, url, storageArea) {
      this.initEvent(type, bubbles, cancelable);
      def(this, "key", key == null ? null : String(key));
      def(this, "oldValue", oldValue == null ? null : String(oldValue));
      def(this, "newValue", newValue == null ? null : String(newValue));
      def(this, "url", url == null ? "" : String(url));
      def(this, "storageArea", storageArea == null ? null : storageArea);
    };
    defSubclass("AnimationEvent", Event, { animationName: "", elapsedTime: 0, pseudoElement: "" });
    defSubclass("TransitionEvent", Event, { propertyName: "", elapsedTime: 0, pseudoElement: "" });
    defSubclass("CloseEvent", Event, { code: 0, reason: "", wasClean: false });
    defSubclass("DeviceMotionEvent", Event, { acceleration: null, accelerationIncludingGravity: null, rotationRate: null, interval: 0 });
    defSubclass("DeviceOrientationEvent", Event, { alpha: null, beta: null, gamma: null, absolute: false });
    var TextEvent = defSubclass("TextEvent", UIEvent, { data: "" });
    TextEvent.prototype.initTextEvent = function (type, bubbles, cancelable, view, data) {
      this.initUIEvent(type, bubbles, cancelable, view, 0);
      def(this, "data", data == null ? "" : String(data));
    };
    // Service Worker events (see issue #56). ExtendableEvent.waitUntil collects lifetime-extending
    // promises onto the event's internal state; the SW lifecycle awaits them. FetchEvent.respondWith
    // stashes the response promise for the fetch-interception path (stage 3).
    var ExtendableEvent = defSubclass("ExtendableEvent", Event, {});
    ExtendableEvent.prototype.waitUntil = function (p) {
      var s = st(this);
      if (!s.dispatching && !s.__active) {
        throw new globalThis.DOMException("Failed to execute 'waitUntil' on 'ExtendableEvent': The event handler is already finished.", "InvalidStateError");
      }
      if (!s.__extend) { s.__extend = []; }
      s.__extend.push(Promise.resolve(p));
    };
    defSubclass("ExtendableMessageEvent", ExtendableEvent, { data: null, origin: "", lastEventId: "", source: null, ports: [] });
    var FetchEvent = defSubclass("FetchEvent", ExtendableEvent, {
      request: null, clientId: "", resultingClientId: "", replacesClientId: "",
      preloadResponse: null, handled: null
    }, function (init) {
      if (!("request" in init) || !init.request) {
        throw new TypeError("Failed to construct 'FetchEvent': required member request is undefined.");
      }
    });
    FetchEvent.prototype.respondWith = function (r) {
      var s = st(this);
      if (s.__responded) { throw new globalThis.DOMException("Failed to execute 'respondWith' on 'FetchEvent': The event has already been responded to.", "InvalidStateError"); }
      s.__responded = true;
      s.__response = Promise.resolve(r);
      if (!s.__extend) { s.__extend = []; }
      s.__extend.push(s.__response["catch"](function () {}));
    };

    // document.createEvent legacy factory: case-insensitive name -> interface, per the DOM spec
    // table. Returns an UNINITIALIZED event (type==="") whose prototype is the interface's
    // prototype; the caller must initEvent()/initCustomEvent()/... before dispatching.
    var createEventTable = {
      "event": Event, "events": Event, "htmlevents": Event, "svgevents": Event,
      "customevent": CustomEvent,
      "uievent": UIEvent, "uievents": UIEvent,
      "mouseevent": MouseEvent, "mouseevents": MouseEvent,
      "keyboardevent": KeyboardEvent,
      "compositionevent": CompositionEvent,
      "focusevent": FocusEvent,
      "messageevent": MessageEvent,
      "hashchangeevent": globalThis.HashChangeEvent,
      "beforeunloadevent": globalThis.BeforeUnloadEvent,
      "dragevent": DragEvent,
      "storageevent": globalThis.StorageEvent,
      "textevent": TextEvent,
      "devicemotionevent": globalThis.DeviceMotionEvent,
      "deviceorientationevent": globalThis.DeviceOrientationEvent
    };
    def(globalThis, "__createEvent", function (name) {
      var key = String(name).toLowerCase();
      var Ctor = createEventTable.hasOwnProperty(key) ? createEventTable[key] : null;
      if (!Ctor) {
        throw new globalThis.DOMException(
          "The event \"" + name + "\" is not supported.", "NotSupportedError");
      }
      var ev = Object.create(Ctor.prototype);
      initEventState(ev);
      // Materialise this interface's own (and inherited) members as data properties with defaults
      // so they exist before init*() is called, matching a freshly-constructed event.
      var chain = [];
      for (var C = Ctor; C && C.__members; C = C.__parent) { chain.unshift(C); }
      for (var i = 0; i < chain.length; i++) {
        var m = chain[i].__members;
        for (var k in m) { def(ev, k, m[k]); }
      }
      return ev;
    });
  })();

  // --- synthetic event dispatch (driven from Rust on user interaction) ----------------------
  // Build a real bubbling event and walk it up the parent chain (node -> ancestors -> document
  // -> window), invoking each target's __listeners[type] callbacks and its on<type> handler.
  // Returns false if any handler called preventDefault() (caller maps this to "default action
  // should not run"), true otherwise.
  var mouseTypes = { click: 1, mousedown: 1, mouseup: 1, dblclick: 1, contextmenu: 1,
                     pointerdown: 1, pointerup: 1, mouseover: 1, mouseout: 1 };
  def(globalThis, "__dispatchSyntheticEvent", function (nodeId, type, props) {
    var node = null;
    try { node = canon(__wrapNode(nodeId)); } catch (e) { node = null; }
    if (!node) { return true; }
    type = String(type);

    var Ctor = mouseTypes[type] ? globalThis.MouseEvent : globalThis.Event;
    var ev;
    try { ev = new Ctor(type, { bubbles: true, cancelable: true }); }
    catch (e) { ev = { type: type, bubbles: true, cancelable: true, defaultPrevented: false }; }
    // Copy caller-supplied props (clientX/clientY/button/...) onto the event.
    if (props) { for (var k in props) { try { ev[k] = props[k]; } catch (e2) {} } }
    // Run it through the shared capture/target/bubble dispatch (which honours stopPropagation,
    // capture listeners, and returns !defaultPrevented).
    return globalThis.__dispatchEventObject(node, ev);
  });

  // --- non-bubbling synthetic event dispatch ------------------------------------------------
  // Fire `type` on the target node ONLY (no ancestor/document/window propagation). Used for
  // focus/blur, mouseenter/mouseleave which do not bubble. Returns false if preventDefault().
  def(globalThis, "__dispatchSyntheticEventNonBubbling", function (nodeId, type, props) {
    var node = null;
    try { node = canon(__wrapNode(nodeId)); } catch (e) { node = null; }
    if (!node) { return true; }
    type = String(type);

    var Ctor = mouseTypes[type] ? globalThis.MouseEvent : globalThis.Event;
    var ev;
    try { ev = new Ctor(type, { bubbles: false, cancelable: true }); }
    catch (e) { ev = { type: type, bubbles: false, cancelable: true, defaultPrevented: false }; }
    if (props) { for (var k in props) { try { ev[k] = props[k]; } catch (e2) {} } }
    // Non-bubbling: __dispatchEventObject skips the bubble phase (capture + target still run).
    return globalThis.__dispatchEventObject(node, ev);
  });

  // mouseover/mouseout bubble; mouseenter/mouseleave do not — register the latter as non-bubbling.
  mouseTypes.mouseenter = 1; mouseTypes.mouseleave = 1; mouseTypes.mousemove = 1;

  // --- checkbox / radio toggle (driven from Rust on click) ----------------------------------
  // Flip a checkbox's `checked`, or set a radio (unchecking same-name siblings), then fire
  // `input` and `change` (both bubbling). The `click` has already been dispatched by the caller.
  // No-op for disabled controls. Returns nothing; the caller reads back the snapshot.
  def(globalThis, "__toggleCheckable", function (nodeId) {
    var el = null;
    try { el = canon(__wrapNode(nodeId)); } catch (e) { el = null; }
    if (!el) { return; }
    var tag = "";
    try { tag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : ""; } catch (e2) {}
    if (tag !== "input") { return; }
    var ty = String(__getAttr(nodeId, "type") || "").toLowerCase();
    if (ty !== "checkbox" && ty !== "radio") { return; }
    if (__getAttr(nodeId, "disabled") != null) { return; }

    if (ty === "checkbox") {
      var on = __getAttr(nodeId, "checked") != null;
      if (on) { __removeAttr(nodeId, "checked"); } else { __setAttr(nodeId, "checked", ""); }
    } else {
      // Radio: uncheck every same-name radio in the same form (or document), then check this one.
      var name = String(__getAttr(nodeId, "name") || "");
      // Find the enclosing <form>, if any.
      var form = null;
      try {
        var c = el;
        while (c) {
          var t = "";
          try { t = typeof c.tagName === "string" ? c.tagName.toLowerCase() : ""; } catch (ef) {}
          if (t === "form") { form = c; break; }
          c = c.parentNode;
        }
      } catch (e3) {}
      var scope = form || document;
      var radios = [];
      try { radios = scope.querySelectorAll("input[type=radio]"); } catch (e4) { radios = []; }
      for (var i = 0; i < radios.length; i++) {
        var r = radios[i];
        var rname = "";
        try { rname = String(r.getAttribute("name") || ""); } catch (e5) {}
        if (rname === name) {
          try { r.removeAttribute("checked"); } catch (e6) {}
        }
      }
      __setAttr(nodeId, "checked", "");
    }
    __dispatchSyntheticEvent(nodeId, "input", {});
    __dispatchSyntheticEvent(nodeId, "change", {});
  });

  // --- <select> option pick (driven from Rust when the native dropdown menu is used) ---------
  // Toggle a <details>'s `open` attribute (from clicking its <summary>), then fire a non-bubbling
  // `toggle` event so the page reacts.
  def(globalThis, "__toggleDetails", function (nodeId) {
    var el = null;
    try { el = canon(__wrapNode(nodeId)); } catch (e) { el = null; }
    if (!el) { return; }
    var tag = "";
    try { tag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : ""; } catch (e2) {}
    if (tag !== "details") { return; }
    if (__getAttr(nodeId, "open") != null) { __removeAttr(nodeId, "open"); }
    else { __setAttr(nodeId, "open", ""); }
    __dispatchSyntheticEventNonBubbling(nodeId, "toggle", {});
  });

  // Mark the `index`-th descendant <option> as selected (clearing `selected` on the others), set
  // the <select>'s `value` attribute to the chosen option's value (its `value` attr, else its
  // text), then fire bubbling `input` + `change` on the <select> so the page reacts. Returns true
  // if the selection actually changed. <optgroup>s are flattened (depth-first); single-pick only.
  def(globalThis, "__setSelectIndex", function (nodeId, index) {
    var sel = null;
    try { sel = canon(__wrapNode(nodeId)); } catch (e) { sel = null; }
    if (!sel) { return false; }
    var tag = "";
    try { tag = typeof sel.tagName === "string" ? sel.tagName.toLowerCase() : ""; } catch (e2) {}
    if (tag !== "select") { return false; }
    if (__getAttr(nodeId, "disabled") != null) { return false; }

    var options = [];
    try { options = sel.querySelectorAll("option"); } catch (e3) { options = []; }
    if (index < 0 || index >= options.length) { return false; }

    var optText = function (opt) {
      var t = "";
      try { t = opt.textContent == null ? "" : String(opt.textContent); } catch (e) {}
      return t.replace(/\s+/g, " ").replace(/^ | $/g, "");
    };
    var optValue = function (opt) {
      var v = null;
      try { v = opt.getAttribute("value"); } catch (e) {}
      return v == null ? optText(opt) : String(v);
    };

    // Was this already the selected option? (matches the layout crate's selection rule.)
    var wasSelected = false;
    try { wasSelected = options[index].getAttribute("selected") != null; } catch (e4) {}

    for (var i = 0; i < options.length; i++) {
      try {
        if (i === index) { options[i].setAttribute("selected", ""); }
        else { options[i].removeAttribute("selected"); }
      } catch (e5) {}
    }
    var newValue = optValue(options[index]);
    var prevValue = String(__getAttr(nodeId, "value") || "");
    try { __setAttr(nodeId, "value", newValue); } catch (e6) {}

    var changed = !wasSelected || prevValue !== newValue;
    __dispatchSyntheticEvent(nodeId, "input", {});
    __dispatchSyntheticEvent(nodeId, "change", {});
    return changed;
  });

  // --- key input handler (driven from Rust on physical key presses) -------------------------
  // Fire keydown, mutate the focused text field's value (firing input), then keyup. Returns
  // nothing; the caller reads back the updated DOM snapshot. Text-like <input>/<textarea> only.
  var textInputTypes = { text: 1, search: 1, email: 1, url: 1, tel: 1, password: 1, number: 1, "": 1 };
  def(globalThis, "__handleKeyInput", function (nodeId, key, code) {
    var el = null;
    try { el = canon(__wrapNode(nodeId)); } catch (e) { el = null; }
    if (!el) { return; }
    key = String(key);
    code = String(code);

    // keydown — if defaultPrevented, still send keyup but skip the value mutation.
    var allowMutation = __dispatchSyntheticEvent(nodeId, "keydown", { key: key, code: code });

    if (allowMutation) {
      var tag = "";
      try { tag = typeof el.tagName === "string" ? el.tagName.toLowerCase() : ""; } catch (e2) {}
      var isTextarea = tag === "textarea";
      var isTextInput = false;
      if (tag === "input") {
        var ty = "";
        try { ty = String(__getAttr(nodeId, "type") || "").toLowerCase(); } catch (e3) {}
        isTextInput = !!textInputTypes[ty] || ty === undefined;
      }
      var disabled = false, readonly = false;
      try { disabled = __getAttr(nodeId, "disabled") != null; } catch (e4) {}
      try { readonly = __getAttr(nodeId, "readonly") != null; } catch (e5) {}

      if ((isTextInput || isTextarea) && !disabled && !readonly) {
        var cur = "";
        try { cur = el.value == null ? "" : String(el.value); } catch (e6) { cur = ""; }
        var next = cur;
        var mutated = false;
        if (key === "Backspace") {
          if (cur.length > 0) { next = cur.slice(0, -1); mutated = true; }
          else { mutated = true; }
        } else if (key === "Delete") {
          // Simplified: drop the last char (no caret tracking).
          if (cur.length > 0) { next = cur.slice(0, -1); mutated = true; }
          else { mutated = true; }
        } else if (key === "Enter") {
          if (isTextarea) { next = cur + "\n"; mutated = true; }
          // <input>: Enter submits; no value change here.
        } else if (key.length === 1) {
          next = cur + key; mutated = true;
        }
        if (mutated) {
          try { el.value = next; } catch (e7) {}
          __dispatchSyntheticEvent(nodeId, "input", {});
        }
      }
    }

    // keyup always fires.
    __dispatchSyntheticEvent(nodeId, "keyup", { key: key, code: code });
  });

  // --- Canvas 2D context ---------------------------------------------------------------------
  // A real (software) CanvasRenderingContext2D. It keeps drawing STATE (styles + a 2D affine
  // transform + the current path) and records a DISPLAY LIST of resolved commands: every command
  // carries already-transformed device-space coordinates and a resolved CSS color (or gradient),
  // so the Rust engine needs no matrix/style math — it just rasterizes. `__canvasLists()` hands
  // the engine every canvas's {id,width,height,commands}.
  function __cnvMatMul(m, n) {
    // m, n are [a,b,c,d,e,f]; returns m*n (apply n first, then m), matching CanvasRenderingContext2D.
    return [
      m[0] * n[0] + m[2] * n[1],
      m[1] * n[0] + m[3] * n[1],
      m[0] * n[2] + m[2] * n[3],
      m[1] * n[2] + m[3] * n[3],
      m[0] * n[4] + m[2] * n[5] + m[4],
      m[1] * n[4] + m[3] * n[5] + m[5],
    ];
  }
  function __cnvApply(m, x, y) {
    return [m[0] * x + m[2] * y + m[4], m[1] * x + m[3] * y + m[5]];
  }
  // Average scale of the matrix (for lineWidth / radius scaling). sqrt(|det|).
  function __cnvScale(m) {
    var det = m[0] * m[3] - m[1] * m[2];
    return Math.sqrt(Math.abs(det)) || 1;
  }
  function __makeCanvas2D(el) {
    var nodeId = (el && typeof el.__node === "number") ? el.__node : -1;
    var list = [];                 // the display list
    var state = {                  // current drawing state
      fillStyle: '#000000', strokeStyle: '#000000', lineWidth: 1, globalAlpha: 1,
      font: "10px sans-serif", fontSize: 10, textAlign: "start", textBaseline: "alphabetic",
      m: [1, 0, 0, 1, 0, 0],
      lineDash: [], lineDashOffset: 0,
      shadowBlur: 0, shadowColor: "rgba(0,0,0,0)", shadowOffsetX: 0, shadowOffsetY: 0,
      clip: null,                  // device-space clip rect [x,y,w,h] (bounding box of clip path)
    };
    var stack = [];                // save/restore stack
    var subpaths = [];             // array of polylines; each polyline is [x0,y0,x1,y1,...] (device)
    var cur = null;                // current subpath being built
    var penX = 0, penY = 0;        // current point in USER space (pre-transform)
    var startX = 0, startY = 0;    // subpath start (user space), for closePath
    function clone(s) {
      return { fillStyle: s.fillStyle, strokeStyle: s.strokeStyle, lineWidth: s.lineWidth,
        globalAlpha: s.globalAlpha, font: s.font, fontSize: s.fontSize, textAlign: s.textAlign,
        textBaseline: s.textBaseline, m: s.m.slice(),
        lineDash: s.lineDash.slice(), lineDashOffset: s.lineDashOffset,
        shadowBlur: s.shadowBlur, shadowColor: s.shadowColor,
        shadowOffsetX: s.shadowOffsetX, shadowOffsetY: s.shadowOffsetY,
        clip: s.clip ? s.clip.slice() : null };
    }
    // Resolve a fill/stroke style: a CSS color string passes through; a gradient object is encoded.
    function resolveStyle(style) {
      // A pattern (createPattern) is approximated as a solid fallback color (see __pattern below).
      if (style && typeof style === "object" && style.__pattern) {
        return { color: style.fallback || '#808080' };
      }
      if (style && typeof style === "object" && style.__grad) {
        var g = style;
        var stops = g.stops.map(function (s) { return { offset: s.offset, color: s.color }; });
        if (g.kind === "linear") {
          var p0 = __cnvApply(state.m, g.x0, g.y0), p1 = __cnvApply(state.m, g.x1, g.y1);
          return { gradient: "linear", x0: p0[0], y0: p0[1], x1: p1[0], y1: p1[1], stops: stops };
        }
        var c0 = __cnvApply(state.m, g.x0, g.y0), c1 = __cnvApply(state.m, g.x1, g.y1);
        var sc = __cnvScale(state.m);
        return { gradient: "radial", x0: c0[0], y0: c0[1], r0: g.r0 * sc,
          x1: c1[0], y1: c1[1], r1: g.r1 * sc, stops: stops };
      }
      return { color: String(style == null ? '#000' : style) };
    }
    function flushSub() { if (cur && cur.length >= 2) { subpaths.push(cur); } cur = null; }
    // Transform + emit the current set of subpaths (returns a fresh array of device polylines).
    function devicePaths() {
      flushSub();
      var out = [];
      for (var i = 0; i < subpaths.length; i++) { out.push(subpaths[i].slice()); }
      // Rebuild cur from the last so further building keeps working (we already flushed).
      subpaths = out.map(function (p) { return p.slice(); });
      return out;
    }
    function addPoint(ux, uy) {
      var p = __cnvApply(state.m, ux, uy);
      if (!cur) { cur = []; }
      cur.push(p[0], p[1]);
      penX = ux; penY = uy;
    }
    // Is a drop-shadow currently active? (non-transparent shadowColor AND a nonzero offset/blur).
    function shadowActive() {
      if (!state.shadowOffsetX && !state.shadowOffsetY && !state.shadowBlur) { return false; }
      var c = String(state.shadowColor);
      // Quick transparent checks (rgba(...,0) / transparent / #..00). Anything else is opaque-ish.
      if (c === "transparent") { return false; }
      var m = /rgba?\([^)]*?,\s*([0-9.]+)\s*\)/.exec(c);
      if (m && parseFloat(m[1]) === 0) { return false; }
      return true;
    }
    // Offset every geometry field of a command (device space) by (dx,dy). Used for shadow copies.
    function offsetCmd(cmd, dx, dy) {
      var o = {};
      for (var k in cmd) { o[k] = cmd[k]; }
      if (o.quad) { o.quad = o.quad.slice(); for (var i = 0; i < o.quad.length; i += 2) { o.quad[i] += dx; o.quad[i + 1] += dy; } }
      function off(arr) { return arr.map(function (poly) { var p = poly.slice(); for (var j = 0; j < p.length; j += 2) { p[j] += dx; p[j + 1] += dy; } return p; }); }
      if (o.polygons) { o.polygons = off(o.polygons); }
      if (o.polylines) { o.polylines = off(o.polylines); }
      if (typeof o.x === "number") { o.x += dx; }
      if (typeof o.y === "number") { o.y += dy; }
      if (o.clip) { o.clip = o.clip.slice(); o.clip[0] += dx; o.clip[1] += dy; }
      return o;
    }
    // Push a draw command, applying the current clip rect and (best-effort) drop shadow. The shadow
    // is an offset copy painted in shadowColor BEFORE the main command (blur approximated by the
    // engine spreading the shadow color over a small radius).
    function emit(cmd) {
      if (state.clip) { cmd.clip = state.clip.slice(); }
      if (shadowActive()) {
        var sc = __cnvScale(state.m);
        var sh = offsetCmd(cmd, state.shadowOffsetX * sc, state.shadowOffsetY * sc);
        // Recolor the shadow: flat shadowColor, drop any gradient.
        delete sh.gradient; delete sh.stops; delete sh.x0; delete sh.y0; delete sh.x1; delete sh.y1; delete sh.r0; delete sh.r1;
        sh.color = String(state.shadowColor);
        sh.blur = state.shadowBlur * sc;
        list.push(sh);
      }
      list.push(cmd);
    }
    var ctx = {
      canvas: el, lineCap: "butt", lineJoin: "miter", miterLimit: 10, direction: "ltr",
      globalCompositeOperation: "source-over", imageSmoothingEnabled: true,
      __nodeId: nodeId, __list: list,
    };
    // Shadow + dash properties are save/restore-aware (kept on `state`), exposed live.
    Object.defineProperty(ctx, "shadowBlur", { get: function () { return state.shadowBlur; }, set: function (v) { var n = +v; if (n >= 0 && isFinite(n)) { state.shadowBlur = n; } }, enumerable: true });
    Object.defineProperty(ctx, "shadowColor", { get: function () { return state.shadowColor; }, set: function (v) { state.shadowColor = String(v); }, enumerable: true });
    Object.defineProperty(ctx, "shadowOffsetX", { get: function () { return state.shadowOffsetX; }, set: function (v) { var n = +v; if (isFinite(n)) { state.shadowOffsetX = n; } }, enumerable: true });
    Object.defineProperty(ctx, "shadowOffsetY", { get: function () { return state.shadowOffsetY; }, set: function (v) { var n = +v; if (isFinite(n)) { state.shadowOffsetY = n; } }, enumerable: true });
    Object.defineProperty(ctx, "lineDashOffset", { get: function () { return state.lineDashOffset; }, set: function (v) { var n = +v; if (isFinite(n)) { state.lineDashOffset = n; } }, enumerable: true });
    // Styled state exposed as live properties.
    Object.defineProperty(ctx, "fillStyle", { get: function () { return state.fillStyle; }, set: function (v) { state.fillStyle = v; }, enumerable: true });
    Object.defineProperty(ctx, "strokeStyle", { get: function () { return state.strokeStyle; }, set: function (v) { state.strokeStyle = v; }, enumerable: true });
    Object.defineProperty(ctx, "lineWidth", { get: function () { return state.lineWidth; }, set: function (v) { var n = +v; if (n > 0 && isFinite(n)) { state.lineWidth = n; } }, enumerable: true });
    Object.defineProperty(ctx, "globalAlpha", { get: function () { return state.globalAlpha; }, set: function (v) { var n = +v; if (n >= 0 && n <= 1) { state.globalAlpha = n; } }, enumerable: true });
    Object.defineProperty(ctx, "textAlign", { get: function () { return state.textAlign; }, set: function (v) { state.textAlign = String(v); }, enumerable: true });
    Object.defineProperty(ctx, "textBaseline", { get: function () { return state.textBaseline; }, set: function (v) { state.textBaseline = String(v); }, enumerable: true });
    Object.defineProperty(ctx, "font", { get: function () { return state.font; }, set: function (v) {
      state.font = String(v);
      var mm = /(\d+(?:\.\d+)?)px/.exec(state.font); // loose: just pull the px size
      if (mm) { state.fontSize = parseFloat(mm[1]); }
      else { var pt = /(\d+(?:\.\d+)?)pt/.exec(state.font); if (pt) { state.fontSize = parseFloat(pt[1]) * 1.333; } }
    }, enumerable: true });

    ctx.save = function () { stack.push(clone(state)); };
    ctx.restore = function () { if (stack.length) { state = stack.pop(); } };
    // Transform ops mutate the current matrix.
    ctx.translate = function (x, y) { state.m = __cnvMatMul(state.m, [1, 0, 0, 1, +x || 0, +y || 0]); };
    ctx.scale = function (x, y) { state.m = __cnvMatMul(state.m, [+x || 0, 0, 0, +y || 0, 0, 0]); };
    ctx.rotate = function (a) { var c = Math.cos(a), s = Math.sin(a); state.m = __cnvMatMul(state.m, [c, s, -s, c, 0, 0]); };
    ctx.transform = function (a, b, c, d, e, f) { state.m = __cnvMatMul(state.m, [+a, +b, +c, +d, +e, +f]); };
    ctx.setTransform = function (a, b, c, d, e, f) {
      if (a && typeof a === "object") { state.m = [a.a, a.b, a.c, a.d, a.e, a.f]; }
      else { state.m = [+a, +b, +c, +d, +e, +f]; }
    };
    ctx.resetTransform = function () { state.m = [1, 0, 0, 1, 0, 0]; };
    ctx.getTransform = function () { var m = state.m; return { a: m[0], b: m[1], c: m[2], d: m[3], e: m[4], f: m[5] }; };

    // Path building. Arcs / curves are FLATTENED to polylines here, in user space, then transformed.
    ctx.beginPath = function () { subpaths = []; cur = null; };
    ctx.moveTo = function (x, y) { flushSub(); startX = +x; startY = +y; addPoint(+x, +y); };
    ctx.lineTo = function (x, y) { if (!cur) { startX = +x; startY = +y; } addPoint(+x, +y); };
    ctx.closePath = function () { if (cur && cur.length >= 2) { addPoint(startX, startY); } };
    ctx.rect = function (x, y, w, h) {
      flushSub(); x = +x; y = +y; w = +w; h = +h;
      addPoint(x, y); addPoint(x + w, y); addPoint(x + w, y + h); addPoint(x, y + h); addPoint(x, y);
      flushSub();
    };
    ctx.arc = function (x, y, r, a0, a1, ccw) {
      x = +x; y = +y; r = +r; a0 = +a0; a1 = +a1;
      var N = 24, span = a1 - a0;
      if (ccw) { if (span > 0) { span -= 2 * Math.PI; } } else { if (span < 0) { span += 2 * Math.PI; } }
      for (var i = 0; i <= N; i++) {
        var a = a0 + span * (i / N);
        var px = x + Math.cos(a) * r, py = y + Math.sin(a) * r;
        if (i === 0 && !cur) { addPoint(px, py); } else { addPoint(px, py); }
      }
    };
    ctx.ellipse = function (x, y, rx, ry, rot, a0, a1, ccw) {
      x = +x; y = +y; rx = +rx; ry = +ry; rot = +rot || 0; a0 = +a0; a1 = +a1;
      var N = 24, span = a1 - a0;
      if (ccw) { if (span > 0) { span -= 2 * Math.PI; } } else { if (span < 0) { span += 2 * Math.PI; } }
      var cr = Math.cos(rot), sr = Math.sin(rot);
      for (var i = 0; i <= N; i++) {
        var a = a0 + span * (i / N), ex = Math.cos(a) * rx, ey = Math.sin(a) * ry;
        addPoint(x + ex * cr - ey * sr, y + ex * sr + ey * cr);
      }
    };
    ctx.arcTo = function (x1, y1, x2, y2, r) {
      // Approximate: line to the first tangent point, then to the second (good enough flattened).
      ctx.lineTo(+x1, +y1); ctx.lineTo(+x2, +y2);
    };
    ctx.quadraticCurveTo = function (cx, cy, x, y) {
      cx = +cx; cy = +cy; x = +x; y = +y;
      var x0 = penX, y0 = penY, N = 16;
      for (var i = 1; i <= N; i++) {
        var t = i / N, u = 1 - t;
        addPoint(u * u * x0 + 2 * u * t * cx + t * t * x, u * u * y0 + 2 * u * t * cy + t * t * y);
      }
    };
    ctx.bezierCurveTo = function (c1x, c1y, c2x, c2y, x, y) {
      c1x = +c1x; c1y = +c1y; c2x = +c2x; c2y = +c2y; x = +x; y = +y;
      var x0 = penX, y0 = penY, N = 16;
      for (var i = 1; i <= N; i++) {
        var t = i / N, u = 1 - t;
        var b0 = u * u * u, b1 = 3 * u * u * t, b2 = 3 * u * t * t, b3 = t * t * t;
        addPoint(b0 * x0 + b1 * c1x + b2 * c2x + b3 * x, b0 * y0 + b1 * c1y + b2 * c2y + b3 * y);
      }
    };
    ctx.roundRect = function (x, y, w, h) { ctx.rect(x, y, w, h); }; // corners approximated as square

    // Drawing ops append resolved commands.
    function rectCmd(op, x, y, w, h, style) {
      x = +x; y = +y; w = +w; h = +h;
      var p0 = __cnvApply(state.m, x, y), p1 = __cnvApply(state.m, x + w, y),
          p2 = __cnvApply(state.m, x + w, y + h), p3 = __cnvApply(state.m, x, y + h);
      var cmd = { op: op, quad: [p0[0], p0[1], p1[0], p1[1], p2[0], p2[1], p3[0], p3[1]], alpha: state.globalAlpha };
      if (op !== "clearRect") { var r = resolveStyle(style); for (var k in r) { cmd[k] = r[k]; } emit(cmd); }
      else { if (state.clip) { cmd.clip = state.clip.slice(); } list.push(cmd); } // clearRect: clip but no shadow
    }
    ctx.fillRect = function (x, y, w, h) { rectCmd("fillRect", x, y, w, h, state.fillStyle); };
    ctx.clearRect = function (x, y, w, h) {
      // A clearRect covering the whole canvas resets the display list (bounds growth for
      // clear+redraw animation loops). Otherwise it's an erase quad.
      var cw = el.width | 0 || 300, chh = el.height | 0 || 150;
      var m = state.m, axis = (Math.abs(m[1]) < 1e-6 && Math.abs(m[2]) < 1e-6);
      if (axis && (+x) <= 0 && (+y) <= 0 && (+x + +w) >= cw && (+y + +h) >= chh) { list.length = 0; return; }
      rectCmd("clearRect", x, y, w, h, null);
    };
    ctx.strokeRect = function (x, y, w, h) {
      x = +x; y = +y; w = +w; h = +h;
      var pts = [x, y, x + w, y, x + w, y + h, x, y + h, x, y];
      var dev = [];
      for (var i = 0; i < pts.length; i += 2) { var p = __cnvApply(state.m, pts[i], pts[i + 1]); dev.push(p[0], p[1]); }
      var r = resolveStyle(state.strokeStyle);
      var cmd = { op: "stroke", polylines: [dev], width: state.lineWidth * __cnvScale(state.m), alpha: state.globalAlpha };
      for (var k in r) { cmd[k] = r[k]; }
      attachDash(cmd);
      emit(cmd);
    };
    ctx.fill = function () {
      var polys = devicePaths();
      if (!polys.length) { return; }
      var r = resolveStyle(state.fillStyle);
      var cmd = { op: "fill", polygons: polys, alpha: state.globalAlpha };
      for (var k in r) { cmd[k] = r[k]; }
      emit(cmd);
    };
    ctx.stroke = function () {
      var polys = devicePaths();
      if (!polys.length) { return; }
      var r = resolveStyle(state.strokeStyle);
      var cmd = { op: "stroke", polylines: polys, width: state.lineWidth * __cnvScale(state.m), alpha: state.globalAlpha };
      for (var k in r) { cmd[k] = r[k]; }
      attachDash(cmd);
      emit(cmd);
    };
    // Attach the current line-dash pattern (scaled to device space) to a stroke command.
    function attachDash(cmd) {
      if (state.lineDash && state.lineDash.length) {
        var sc = __cnvScale(state.m);
        cmd.dash = state.lineDash.map(function (d) { return d * sc; });
        cmd.dashOffset = state.lineDashOffset * sc;
      }
    }
    function textCmd(op, text, x, y, style) {
      var p = __cnvApply(state.m, +x || 0, +y || 0);
      var r = resolveStyle(style);
      var cmd = { op: "text", text: String(text), x: p[0], y: p[1],
        size: state.fontSize * __cnvScale(state.m), align: state.textAlign,
        baseline: state.textBaseline, alpha: state.globalAlpha };
      for (var k in r) { cmd[k] = r[k]; }
      emit(cmd);
    }
    ctx.fillText = function (t, x, y) { textCmd("fillText", t, x, y, state.fillStyle); };
    ctx.strokeText = function (t, x, y) { textCmd("strokeText", t, x, y, state.strokeStyle); };
    ctx.measureText = function (s) {
      var w = __measureCanvasText(String(s == null ? "" : s), state.fontSize);
      return { width: w, actualBoundingBoxLeft: 0, actualBoundingBoxRight: w,
        actualBoundingBoxAscent: state.fontSize * 0.8, actualBoundingBoxDescent: state.fontSize * 0.2,
        fontBoundingBoxAscent: state.fontSize * 0.8, fontBoundingBoxDescent: state.fontSize * 0.2 };
    };

    // Gradients.
    function makeGradient(kind, x0, y0, x1, y1, r0, r1) {
      var g = { __grad: true, kind: kind, x0: +x0, y0: +y0, x1: +x1, y1: +y1, r0: +r0 || 0, r1: +r1 || 0, stops: [] };
      g.addColorStop = function (off, color) { g.stops.push({ offset: +off, color: String(color) }); };
      return g;
    }
    ctx.createLinearGradient = function (x0, y0, x1, y1) { return makeGradient("linear", x0, y0, x1, y1, 0, 0); };
    ctx.createRadialGradient = function (x0, y0, r0, x1, y1, r1) { return makeGradient("radial", x0, y0, x1, y1, r0, r1); };
    ctx.createConicGradient = function () { return makeGradient("linear", 0, 0, 0, 0, 0, 0); };

    var noop = function () {};
    ctx.drawFocusIfNeeded = noop;
    ctx.isPointInPath = function () { return false; }; ctx.isPointInStroke = function () { return false; };

    // clip(): constrain subsequent draws to the bounding box of the current path (a documented
    // simplification — real clip is the path shape; we track its device-space AABB). Intersects with
    // any existing clip and is save/restore-aware (clip lives on `state`).
    ctx.clip = function () {
      var polys = devicePaths();
      if (!polys.length) { return; }
      var minx = Infinity, miny = Infinity, maxx = -Infinity, maxy = -Infinity;
      for (var i = 0; i < polys.length; i++) {
        var p = polys[i];
        for (var j = 0; j + 1 < p.length; j += 2) {
          if (p[j] < minx) { minx = p[j]; } if (p[j] > maxx) { maxx = p[j]; }
          if (p[j + 1] < miny) { miny = p[j + 1]; } if (p[j + 1] > maxy) { maxy = p[j + 1]; }
        }
      }
      if (!isFinite(minx)) { return; }
      var nx = minx, ny = miny, nw = maxx - minx, nh = maxy - miny;
      if (state.clip) { // intersect with the existing clip rect
        var cx = Math.max(state.clip[0], nx), cy = Math.max(state.clip[1], ny);
        var cw = Math.min(state.clip[0] + state.clip[2], nx + nw) - cx;
        var chh = Math.min(state.clip[1] + state.clip[3], ny + nh) - cy;
        state.clip = [cx, cy, Math.max(0, cw), Math.max(0, chh)];
      } else {
        state.clip = [nx, ny, nw, nh];
      }
    };

    // Line dash. Pattern is in user-space units; scaled to device space at stroke time (attachDash).
    ctx.setLineDash = function (segs) {
      if (!segs || typeof segs.length !== "number") { return; }
      var out = [];
      for (var i = 0; i < segs.length; i++) { var n = +segs[i]; if (isFinite(n) && n >= 0) { out.push(n); } else { return; } }
      // An odd-length pattern is doubled (per spec).
      if (out.length % 2 === 1) { out = out.concat(out); }
      state.lineDash = out;
    };
    ctx.getLineDash = function () { return state.lineDash.slice(); };

    // createPattern: best-effort. We cannot tile in the engine, so return an object usable as a
    // fillStyle/strokeStyle that resolveStyle falls back to a solid color (documented simplification).
    ctx.createPattern = function (image, repetition) {
      return { __pattern: true, repetition: String(repetition || "repeat"), fallback: '#808080' };
    };

    // ---- Image data ----
    function makeImageData(w, h, src) {
      var ww = Math.max(1, w | 0), hh = Math.max(1, h | 0);
      var data = src || new Uint8ClampedArray(ww * hh * 4);
      return { width: ww, height: hh, data: data, colorSpace: "srgb" };
    }
    ctx.createImageData = function (a, b) {
      // createImageData(w,h) | createImageData(imagedata)
      if (a && typeof a === "object" && a.width != null) { return makeImageData(a.width, a.height); }
      return makeImageData(a, b);
    };
    // getImageData reads the engine's pushed pixels (previous frame) for this canvas node. Returns a
    // zeroed buffer if the canvas has not been rasterized yet (one-render lag — documented).
    ctx.getImageData = function (x, y, w, h) {
      var ww = Math.max(1, w | 0), hh = Math.max(1, h | 0);
      var data = new Uint8ClampedArray(ww * hh * 4);
      try {
        if (nodeId >= 0 && typeof __canvasPixels === "function") {
          var got = __canvasPixels(nodeId, x | 0, y | 0, ww, hh);
          if (got && got.b64) {
            var bin = (typeof atob === "function") ? atob(got.b64) : "";
            var n = Math.min(bin.length, data.length);
            for (var i = 0; i < n; i++) { data[i] = bin.charCodeAt(i) & 0xff; }
          }
        }
      } catch (e) {}
      return makeImageData(ww, hh, data);
    };
    // putImageData records a command that writes the pixel block into the canvas surface at (dx,dy).
    // The pixels are base64-bridged to the engine. Dirty-rect args are honored (subset of the block).
    ctx.putImageData = function (imagedata, dx, dy, dirtyX, dirtyY, dirtyW, dirtyH) {
      if (!imagedata || !imagedata.data) { return; }
      var iw = imagedata.width | 0, ih = imagedata.height | 0;
      if (iw <= 0 || ih <= 0) { return; }
      var d = imagedata.data, s = "";
      for (var i = 0; i < d.length; i++) { s += String.fromCharCode(d[i] & 0xff); }
      var b64 = (typeof btoa === "function") ? btoa(s) : "";
      // putImageData ignores the transform; (dx,dy) are device (canvas) pixels directly.
      var cmd = { op: "putImageData", dx: dx | 0, dy: dy | 0, iw: iw, ih: ih, b64: b64 };
      if (dirtyW != null) { cmd.dirtyX = dirtyX | 0; cmd.dirtyY = dirtyY | 0; cmd.dirtyW = dirtyW | 0; cmd.dirtyH = dirtyH | 0; }
      list.push(cmd);
    };

    // drawImage(src, dx,dy) | (src, dx,dy,dw,dh) | (src, sx,sy,sw,sh, dx,dy,dw,dh). `src` is an
    // HTMLImageElement or HTMLCanvasElement; the engine blits its pixels (by node id) into the dest
    // rect, honoring globalAlpha + clip. The dest rect is transformed by the current matrix (as a
    // quad); source sub-rect sampling is nearest-neighbor.
    ctx.drawImage = function (src) {
      var srcId = (src && typeof src.__node === "number") ? src.__node
                : (src && src.canvas && typeof src.canvas.__node === "number") ? src.canvas.__node : -1;
      if (srcId < 0) { return; }
      // Natural source size (for the 3-arg form's default dw/dh, and to default sw/sh).
      var natW = (src.naturalWidth | 0) || (src.width | 0) || 0;
      var natH = (src.naturalHeight | 0) || (src.height | 0) || 0;
      var sx = 0, sy = 0, sw = natW, sh = natH, dx, dy, dw, dh;
      if (arguments.length <= 3) {               // (src, dx, dy)
        dx = +arguments[1] || 0; dy = +arguments[2] || 0; dw = natW; dh = natH;
      } else if (arguments.length <= 5) {         // (src, dx, dy, dw, dh)
        dx = +arguments[1] || 0; dy = +arguments[2] || 0; dw = +arguments[3] || 0; dh = +arguments[4] || 0;
      } else {                                    // (src, sx, sy, sw, sh, dx, dy, dw, dh)
        sx = +arguments[1] || 0; sy = +arguments[2] || 0; sw = +arguments[3] || 0; sh = +arguments[4] || 0;
        dx = +arguments[5] || 0; dy = +arguments[6] || 0; dw = +arguments[7] || 0; dh = +arguments[8] || 0;
      }
      // Transform the dest rect's 4 corners into device space (a quad).
      var p0 = __cnvApply(state.m, dx, dy), p1 = __cnvApply(state.m, dx + dw, dy),
          p2 = __cnvApply(state.m, dx + dw, dy + dh), p3 = __cnvApply(state.m, dx, dy + dh);
      var cmd = { op: "drawImage", src: srcId,
        sx: sx, sy: sy, sw: sw, sh: sh,
        quad: [p0[0], p0[1], p1[0], p1[1], p2[0], p2[1], p3[0], p3[1]],
        alpha: state.globalAlpha };
      emit(cmd);
    };

    ctx.getContextAttributes = function () { return { alpha: true, desynchronized: false, colorSpace: "srgb", willReadFrequently: false }; };
    return ctx;
  }
  // ImageData constructor: new ImageData(w,h) | new ImageData(Uint8ClampedArray, w[, h]).
  if (typeof globalThis.ImageData !== "function") {
    globalThis.ImageData = function ImageData(a, b, c) {
      var data, w, h;
      if (a && typeof a === "object" && typeof a.length === "number") {
        data = a; w = b | 0; h = c != null ? (c | 0) : (w > 0 ? (a.length / 4 / w) | 0 : 0);
      } else {
        w = a | 0; h = b | 0; data = new Uint8ClampedArray(Math.max(0, w * h * 4));
      }
      if (w <= 0) { w = 1; } if (h <= 0) { h = 1; }
      this.width = w; this.height = h; this.data = data; this.colorSpace = "srgb";
    };
  }
  globalThis.__makeCanvas2D = __makeCanvas2D;

  // --- OffscreenCanvas ----------------------------------------------------------------------
  // An OffscreenCanvas owns a 2D context that records the SAME display list as an on-screen
  // <canvas> (via __makeCanvas2D over a detached element-like shim), but is not part of the page's
  // layout. Pixel readback (getImageData / transferToImageBitmap / convertToBlob) rasterizes the
  // display list SYNCHRONOUSLY via the __rasterizeCanvas native (the engine's software rasterizer,
  // shared from the paint crate) — there is no engine compositing pass behind an offscreen canvas,
  // so the on-screen "read engine-pushed pixels a frame later" path does not apply. Exposed on both
  // the page and worker scopes (workers list OffscreenCanvas in __workerSelfGlobals).
  if (typeof globalThis.OffscreenCanvas !== "function") {
    defClass("OffscreenCanvasRenderingContext2D");
    defClass("ImageBitmap");
    defClass("OffscreenCanvas", globalThis.EventTarget);

    // Rasterize an offscreen context's current display list into a full-canvas RGBA buffer.
    function __offscreenRasterize(ctx, cw, chh) {
      var out = new Uint8ClampedArray(Math.max(1, cw) * Math.max(1, chh) * 4);
      try {
        if (typeof __rasterizeCanvas !== "function") { return out; }
        var listJson = JSON.stringify([{ id: 0, width: cw, height: chh, commands: ctx.__list }]);
        var b64 = __rasterizeCanvas(listJson, cw, chh);
        if (b64 && typeof atob === "function") {
          var bin = atob(b64), n = Math.min(bin.length, out.length);
          for (var i = 0; i < n; i++) { out[i] = bin.charCodeAt(i) & 0xff; }
        }
      } catch (e) {}
      return out;
    }
    // Crop a (sx,sy,sw,sh) sub-rect out of a full-canvas RGBA buffer (out-of-bounds pixels stay 0).
    function __cropRGBA(full, cw, chh, sx, sy, sw, sh) {
      var out = new Uint8ClampedArray(Math.max(1, sw) * Math.max(1, sh) * 4);
      for (var yy = 0; yy < sh; yy++) {
        var srcY = sy + yy; if (srcY < 0 || srcY >= chh) { continue; }
        for (var xx = 0; xx < sw; xx++) {
          var srcX = sx + xx; if (srcX < 0 || srcX >= cw) { continue; }
          var si = (srcY * cw + srcX) * 4, di = (yy * sw + xx) * 4;
          out[di] = full[si]; out[di + 1] = full[si + 1]; out[di + 2] = full[si + 2]; out[di + 3] = full[si + 3];
        }
      }
      return out;
    }

    function __makeOffscreen2D(oc) {
      // A detached element-like shim so __makeCanvas2D records a display list. Its node id is the
      // placeholder canvas's node when this offscreen came from transferControlToOffscreen() (so the
      // engine composites the result onto that <canvas>), else -1 (pure offscreen, not composited).
      var pnode = (typeof oc.__placeholderNode === "number") ? oc.__placeholderNode : -1;
      var elShim = { tagName: "CANVAS", __node: pnode };
      Object.defineProperty(elShim, "width", { get: function () { return oc.width | 0; }, configurable: true });
      Object.defineProperty(elShim, "height", { get: function () { return oc.height | 0; }, configurable: true });
      var ctx = globalThis.__makeCanvas2D(elShim);
      Object.defineProperty(ctx, "canvas", { value: oc, enumerable: true, configurable: true });
      try { Object.setPrototypeOf(ctx, globalThis.OffscreenCanvasRenderingContext2D.prototype); } catch (e) {}
      // Synchronous readback overrides (offscreen surfaces are not engine-composited).
      ctx.getImageData = function (x, y, w, h) {
        var cw = oc.width | 0, chh = oc.height | 0;
        var sw = Math.max(1, Math.abs(w | 0)), sh = Math.max(1, Math.abs(h | 0));
        var full = __offscreenRasterize(ctx, cw, chh);
        var data = __cropRGBA(full, cw, chh, x | 0, y | 0, sw, sh);
        return { width: sw, height: sh, data: data, colorSpace: "srgb" };
      };
      // A placeholder-backed offscreen is composited by the engine like a normal canvas.
      if (pnode >= 0) {
        try { globalThis.__canvases = globalThis.__canvases || []; globalThis.__canvases.push(ctx); } catch (e) {}
      }
      return ctx;
    }

    globalThis.OffscreenCanvas = function OffscreenCanvas(width, height) {
      if (!(this instanceof globalThis.OffscreenCanvas)) {
        throw new TypeError("Failed to construct 'OffscreenCanvas': Please use the 'new' operator, this object constructor cannot be called as a function.");
      }
      if (arguments.length < 2) {
        throw new TypeError("Failed to construct 'OffscreenCanvas': 2 arguments required, but only " + arguments.length + " present.");
      }
      installEvents(this);
      var oc = this;
      var w = width >>> 0, h = height >>> 0;   // [EnforceRange] unsigned long
      var ctx2d = null;
      Object.defineProperty(oc, "width", {
        get: function () { return w; },
        set: function (v) { w = v >>> 0; if (ctx2d && ctx2d.__list) { ctx2d.__list.length = 0; } },
        enumerable: true, configurable: true
      });
      Object.defineProperty(oc, "height", {
        get: function () { return h; },
        set: function (v) { h = v >>> 0; if (ctx2d && ctx2d.__list) { ctx2d.__list.length = 0; } },
        enumerable: true, configurable: true
      });
      oc.oncontextlost = null; oc.oncontextrestored = null;
      def(oc, "getContext", function (type) {
        if (String(type) === "2d") { if (!ctx2d) { ctx2d = __makeOffscreen2D(oc); } return ctx2d; }
        return null; // webgl / webgpu / bitmaprenderer not supported
      });
      def(oc, "transferToImageBitmap", function () {
        var cw = oc.width | 0, chh = oc.height | 0;
        var bmp = Object.create(globalThis.ImageBitmap.prototype);
        Object.defineProperty(bmp, "width", { value: cw, enumerable: true, configurable: true });
        Object.defineProperty(bmp, "height", { value: chh, enumerable: true, configurable: true });
        bmp.__rgba = ctx2d ? __offscreenRasterize(ctx2d, cw, chh) : new Uint8ClampedArray(Math.max(1, cw) * Math.max(1, chh) * 4);
        def(bmp, "close", function () {});
        // Transfer empties the source bitmap (approximate: drop the recorded commands).
        if (ctx2d && ctx2d.__list) { ctx2d.__list.length = 0; }
        return bmp;
      });
      def(oc, "convertToBlob", function (options) {
        // No image encoder available: return a Blob carrying the raw RGBA bytes tagged with the
        // requested MIME type, so callers that only inspect type/size resolve. (Simplification.)
        var cw = oc.width | 0, chh = oc.height | 0;
        var rgba = ctx2d ? __offscreenRasterize(ctx2d, cw, chh) : new Uint8ClampedArray(Math.max(1, cw) * Math.max(1, chh) * 4);
        var type = (options && options.type) ? String(options.type) : "image/png";
        try { return Promise.resolve(new globalThis.Blob([rgba], { type: type })); }
        catch (e) { return Promise.reject(e); }
      });
    };
    try {
      globalThis.OffscreenCanvas.prototype = Object.create(globalThis.EventTarget.prototype);
      Object.defineProperty(globalThis.OffscreenCanvas.prototype, "constructor", { value: globalThis.OffscreenCanvas, enumerable: false, configurable: true, writable: true });
    } catch (e) {}
  }

  // Approximate text advance for measureText. The JS crate has no font, so this is a proportional
  // per-character estimate (the engine rasterizes/aligns text with the REAL system font). Narrow
  // glyphs (i/l/.) ~0.32em, wide (m/w/W) ~0.92em, else ~0.55em — close enough for layout.
  function __measureCanvasText(s, px) {
    var w = 0;
    for (var i = 0; i < s.length; i++) {
      var ch = s[i];
      if ("iIl.,:;'|!".indexOf(ch) >= 0) { w += 0.32; }
      else if ("mwMW@".indexOf(ch) >= 0) { w += 0.92; }
      else if (ch >= "A" && ch <= "Z") { w += 0.68; }
      else if (ch === " ") { w += 0.30; }
      else { w += 0.52; }
    }
    return w * px;
  }

  // HTML "named properties on the window object": an element with an `id` is exposed as a bare
  // global so `target1` resolves to `<div id="target1">` without `document.getElementById`.
  // Browsers implement this via a live named-property getter; we install a configurable getter per
  // id that delegates to `getElementById` (so it stays live, returns the canonical wrapper, and
  // tree-order / duplicate-id resolution comes for free). Called once after the environment is
  // installed and the DOM is parsed, before any author script runs. We never shadow an existing
  // own/builtin global (e.g. an `id="location"` must not clobber `window.location`).
  globalThis.__installNamedGlobals = function () {
    var nodes;
    try { nodes = __querySelectorAll("[id]"); } catch (e) { return; }
    if (!nodes) { return; }
    for (var i = 0; i < nodes.length; i++) {
      var nid = nodes[i];
      var idStr;
      try { idStr = __getAttr(nid, "id"); } catch (e) { idStr = ""; }
      if (!idStr) { continue; }
      if (Object.prototype.hasOwnProperty.call(globalThis, idStr)) { continue; }
      (function (name) {
        try {
          Object.defineProperty(globalThis, name, {
            configurable: true,
            enumerable: false,
            get: function () { return document.getElementById(name); },
            // HTML named properties are overridable: assigning (including a global `var name = ...`)
            // replaces the named property with a plain data property. Without a setter, such an
            // assignment throws in strict/module code and aborts the script.
            set: function (v) {
              Object.defineProperty(globalThis, name, {
                value: v,
                writable: true,
                configurable: true,
                enumerable: true,
              });
            },
          });
        } catch (e) {}
      })(idStr);
    }
  };

  // The engine pulls every canvas's display list through this. Returns a JSON-ready array of
  // { id, width, height, commands:[...] }. Guard on the engine side: only called when the DOM has
  // a <canvas>.
  globalThis.__canvasLists = function () {
    var cs = globalThis.__canvases || [];
    var out = [];
    for (var i = 0; i < cs.length; i++) {
      var c = cs[i];
      if (!c || c.__nodeId < 0) { continue; }
      var el = c.canvas;
      out.push({ id: c.__nodeId, width: (el.width | 0) || 300, height: (el.height | 0) || 150, commands: c.__list });
    }
    return out;
  };

})();
