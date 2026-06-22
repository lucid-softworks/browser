(function () {
  function def(obj, name, value) {
    Object.defineProperty(obj, name, { value: value, enumerable: false, configurable: true, writable: true });
  }

  // Web IDL collection objects. A resolver either recomputes the current nodes (live collection)
  // or closes over a snapshot (static NodeList). Proxy traps provide indexed and named properties
  // without making the platform object inherit from Array.
  var __collectionState = new WeakMap();
  function NodeList() { throw new TypeError("Illegal constructor"); }
  function HTMLCollection() { throw new TypeError("Illegal constructor"); }
  function collectionItems(value) {
    var resolver = __collectionState.get(value);
    if (!resolver) { throw new TypeError("Illegal invocation"); }
    return resolver();
  }
  function arrayIndex(prop) {
    if (typeof prop !== "string" || !/^(0|[1-9][0-9]*)$/.test(prop)) { return -1; }
    var n = Number(prop);
    return n >= 0 && n < 4294967295 ? n : -1;
  }
  function collectionNamedItems(items) {
    var names = Object.create(null);
    for (var i = 0; i < items.length; i++) {
      var item = items[i];
      if (!item) { continue; }
      var id = item.id == null ? "" : String(item.id);
      if (id && !(id in names)) { names[id] = item; }
      var name = item.getAttribute && item.namespaceURI === "http://www.w3.org/1999/xhtml"
        ? item.getAttribute("name") : null;
      if (name && !(name in names)) { names[String(name)] = item; }
    }
    return names;
  }
  function makeCollection(Ctor, resolver, live) {
    var snapshot = live ? null : resolver().slice();
    var resolve = live ? resolver : function () { return snapshot; };
    var target = Object.create(Ctor.prototype);
    var proxy = new Proxy(target, {
      get: function (obj, prop, receiver) {
        var index = arrayIndex(prop);
        if (index >= 0) { return resolve()[index]; }
        if (Ctor === HTMLCollection && typeof prop === "string" && !(prop in obj)) {
          var named = collectionNamedItems(resolve());
          if (prop in named) { return named[prop]; }
        }
        return Reflect.get(obj, prop, receiver);
      },
      set: function (obj, prop, value, receiver) {
        if (arrayIndex(prop) >= 0) { return false; }
        if (Ctor === HTMLCollection && typeof prop === "string" && prop in collectionNamedItems(resolve())) { return false; }
        return Reflect.set(obj, prop, value, receiver);
      },
      has: function (obj, prop) {
        var index = arrayIndex(prop);
        if (index >= 0) { return index < resolve().length; }
        if (Ctor === HTMLCollection && typeof prop === "string" && prop in collectionNamedItems(resolve())) { return true; }
        return Reflect.has(obj, prop);
      },
      ownKeys: function (obj) {
        var items = resolve(), keys = [];
        for (var i = 0; i < items.length; i++) { keys.push(String(i)); }
        if (Ctor === HTMLCollection) {
          var named = collectionNamedItems(items);
          for (var name in named) { if (keys.indexOf(name) < 0) { keys.push(name); } }
        }
        var own = Reflect.ownKeys(obj);
        for (var j = 0; j < own.length; j++) { if (keys.indexOf(own[j]) < 0) { keys.push(own[j]); } }
        return keys;
      },
      getOwnPropertyDescriptor: function (obj, prop) {
        var index = arrayIndex(prop), items = resolve();
        if (index >= 0 && index < items.length) {
          return { value: items[index], writable: false, enumerable: true, configurable: true };
        }
        if (Ctor === HTMLCollection && typeof prop === "string") {
          var named = collectionNamedItems(items);
          if (prop in named) {
            return { value: named[prop], writable: false, enumerable: false, configurable: true };
          }
        }
        return Reflect.getOwnPropertyDescriptor(obj, prop);
      },
      defineProperty: function (obj, prop, descriptor) {
        if (arrayIndex(prop) >= 0) { return false; }
        return Reflect.defineProperty(obj, prop, descriptor);
      }
    });
    __collectionState.set(proxy, resolve);
    return proxy;
  }
  Object.defineProperty(NodeList.prototype, Symbol.toStringTag,
    { value: "NodeList", configurable: true });
  Object.defineProperty(HTMLCollection.prototype, Symbol.toStringTag,
    { value: "HTMLCollection", configurable: true });
  Object.defineProperty(NodeList.prototype, "length", {
    get: function () { return collectionItems(this).length; }, enumerable: true, configurable: true
  });
  Object.defineProperty(HTMLCollection.prototype, "length", {
    get: function () { return collectionItems(this).length; }, enumerable: true, configurable: true
  });
  def(NodeList.prototype, "item", function (index) {
    var item = collectionItems(this)[Number(index) >>> 0];
    return item === undefined ? null : item;
  });
  def(HTMLCollection.prototype, "item", NodeList.prototype.item);
  def(HTMLCollection.prototype, "namedItem", function (name) {
    var key = String(name);
    if (!key) { return null; }
    var item = collectionNamedItems(collectionItems(this))[key];
    return item === undefined ? null : item;
  });
  def(NodeList.prototype, "forEach", function (callback, thisArg) {
    var items = collectionItems(this);
    for (var i = 0; i < items.length; i++) { callback.call(thisArg, items[i], i, this); }
  });
  function collectionIterator(kind) {
    return function () {
      var collection = this, index = 0;
      var iterator = { next: function () {
        var items = collectionItems(collection);
        if (index >= items.length) { return { value: undefined, done: true }; }
        var value = kind === "keys" ? index : (kind === "entries" ? [index, items[index]] : items[index]);
        index++;
        return { value: value, done: false };
      } };
      iterator[Symbol.iterator] = function () { return this; };
      return iterator;
    };
  }
  def(NodeList.prototype, "entries", collectionIterator("entries"));
  def(NodeList.prototype, "keys", collectionIterator("keys"));
  def(NodeList.prototype, "values", collectionIterator("values"));
  def(NodeList.prototype, Symbol.iterator, NodeList.prototype.values);
  def(HTMLCollection.prototype, Symbol.iterator, collectionIterator("values"));
  def(globalThis, "NodeList", NodeList);
  def(globalThis, "HTMLCollection", HTMLCollection);
  def(globalThis, "__makeNodeList", function (resolver, live) {
    return makeCollection(NodeList, resolver, !!live);
  });
  def(globalThis, "__makeHTMLCollection", function (resolver) {
    return makeCollection(HTMLCollection, resolver, true);
  });

  // window / self aliases (globalThis already exists).
  globalThis.window = globalThis;
  globalThis.self = globalThis;
  // Top-level browsing context: parent/top/frames are self-referential, there's no opener, and
  // zero child frames. Real code (testharness.js, framebusters, analytics) walks `window.parent` /
  // `window.top` and crashes if they're undefined.
  globalThis.parent = globalThis;
  globalThis.top = globalThis;
  globalThis.frames = globalThis;
  globalThis.opener = null;
  try { globalThis.length = 0; } catch (e) {}
  // Minimal location stub (overwritten by the browser-env bootstrap).
  globalThis.location = { href: "" };

  var NODE = "__node";

  // --- DOM namespace / case metadata ----------------------------------------------------------
  // The Rust arena stores only a lowercased `tag` string per element, which loses the namespace,
  // the prefix, and the original case that `createElementNS` must remember. We keep that extra
  // metadata in a JS-side map keyed by the node id (mirroring how other per-node JS state is kept).
  // Elements parsed from HTML source (no entry here) default to the HTML namespace, a lowercased
  // localName, an uppercase tagName, and a null prefix.
  var HTML_NS = "http://www.w3.org/1999/xhtml";
  var XML_NS = "http://www.w3.org/XML/1998/namespace";
  var XMLNS_NS = "http://www.w3.org/2000/xmlns/";
  var __nsMeta = {}; // id -> { namespaceURI, prefix, localName, qualifiedName, isHTML }

  function asciiLower(s) {
    return String(s).replace(/[A-Z]/g, function (c) { return c.toLowerCase(); });
  }
  function asciiUpper(s) {
    return String(s).replace(/[a-z]/g, function (c) { return c.toUpperCase(); });
  }

  // XML `Name` / `QName` validation. Matches the behaviour browsers (and the WPT suite) actually
  // implement, which is more lenient than the strict XML grammar: a NameStartChar is an ASCII
  // letter / underscore or any non-ASCII codepoint (>= U+0080); a NameChar additionally allows
  // digits, '-' and '.', and in fact any character that is not whitespace or '>' (so '<', '}', and
  // lone surrogates are accepted mid-name). The ':' separates a prefix from a local name.
  function isNameStartChar(cc) {
    return (cc >= 0x41 && cc <= 0x5A) || (cc >= 0x61 && cc <= 0x7A) || cc === 0x5F || cc >= 0x80;
  }
  function isNameChar(cc) {
    if (isNameStartChar(cc)) { return true; }
    if (cc >= 0x30 && cc <= 0x39) { return true; }      // 0-9
    if (cc === 0x2D || cc === 0x2E) { return true; }    // - .
    // Lenient: any non-whitespace, non-'>' character is accepted mid-name.
    if (cc === 0x3E) { return false; }                  // '>'
    if (cc === 0x20 || cc === 0x09 || cc === 0x0A || cc === 0x0C || cc === 0x0D) { return false; }
    return true;
  }
  // A valid "Name" (colons permitted as NameChar when allowColon): NameStartChar NameChar*.
  function isValidNameImpl(s, allowColon) {
    if (s.length === 0) { return false; }
    if (!isNameStartChar(s.charCodeAt(0))) { return false; }
    for (var i = 1; i < s.length; i++) {
      var cc = s.charCodeAt(i);
      if (cc === 0x3A) { if (!allowColon) { return false; } continue; }
      if (!isNameChar(cc)) { return false; }
    }
    return true;
  }
  function isValidName(s) { return isValidNameImpl(s, false); }
  // The "validate and extract" QName split is more lenient than the strict XML QName grammar (and
  // matches what browsers/WPT implement): once a colon is found, the prefix and the local part are
  // checked on their own. A prefix only needs to be a non-empty run of NameChars — its first
  // character need NOT be a NameStartChar (so "0:a" is accepted). A local part must start with a
  // NameStartChar OR a colon (so "f::oo" splits to the local part ":oo", which is accepted — the
  // colon there is treated as part of the local name), then NameChars (colons included).
  function isValidPrefix(p) {
    if (p.length === 0) { return false; }
    for (var i = 0; i < p.length; i++) { if (!isNameChar(p.charCodeAt(i))) { return false; } }
    return true;
  }
  function isValidLocalPart(s) {
    if (s.length === 0) { return false; }
    var c0 = s.charCodeAt(0);
    if (!(isNameStartChar(c0) || c0 === 0x3A)) { return false; }
    for (var i = 1; i < s.length; i++) { if (!isNameChar(s.charCodeAt(i))) { return false; } }
    return true;
  }

  function invalidCharacterError() {
    throw new globalThis.DOMException("The string contains invalid characters.", "InvalidCharacterError");
  }
  function namespaceError() {
    throw new globalThis.DOMException("The namespace is not valid.", "NamespaceError");
  }

  // "validate and extract" (DOM standard): given a namespace + qualifiedName, validate the QName and
  // split it into [namespace, prefix, localName], enforcing the xml/xmlns special cases.
  function validateAndExtract(ns, qualifiedName) {
    ns = (ns === undefined || ns === null || ns === "") ? null : String(ns);
    var qname = String(qualifiedName);
    var prefix = null;
    var localName = qname;
    var ci = qname.indexOf(":");
    if (ci >= 0) {
      prefix = qname.slice(0, ci);
      localName = qname.slice(ci + 1);
      // Prefix: a non-empty run of NameChars (first char need not be a NameStartChar). Local part:
      // starts with a NameStartChar or a colon, then NameChars (further colons permitted).
      if (!isValidPrefix(prefix)) { invalidCharacterError(); }
      if (!isValidLocalPart(localName)) { invalidCharacterError(); }
    } else {
      if (!isValidNameImpl(qname, false)) { invalidCharacterError(); }
    }
    if (prefix !== null && ns === null) { namespaceError(); }
    if (prefix === "xml" && ns !== XML_NS) { namespaceError(); }
    if ((qname === "xmlns" || prefix === "xmlns") && ns !== XMLNS_NS) { namespaceError(); }
    if (ns === XMLNS_NS && qname !== "xmlns" && prefix !== "xmlns") { namespaceError(); }
    return { namespace: ns, prefix: prefix, localName: localName };
  }

  // The qualified name of an element id (prefix:localName, or localName), honouring metadata.
  function elQualifiedName(eid) {
    var m = __nsMeta[eid];
    if (m) { return m.qualifiedName; }
    return __tag(eid); // parsed HTML: arena tag is the lowercased qualified name
  }
  function elNamespace(eid) {
    var m = __nsMeta[eid];
    if (m) { return m.namespaceURI; }
    return __nodeType(eid) === 1 ? __namespaceUri(eid) : null;
  }
  function elLocalName(eid) {
    var m = __nsMeta[eid];
    if (m) { return m.localName; }
    return __tag(eid);
  }
  // getElementsByTagName matcher, per the DOM standard (HTML document branch). qualifiedName "*"
  // matches all; HTML-namespace elements match their lowercased qualified name; other namespaces
  // match the qualified name exactly.
  function matchesTagName(eid, qualifiedName) {
    if (qualifiedName === "*") { return true; }
    var qn = elQualifiedName(eid);
    // HTML-namespace elements compare their (as-stored) qualified name against the lowercased
    // search string; other namespaces compare the qualified name exactly.
    if (elNamespace(eid) === HTML_NS) {
      return qn === asciiLower(qualifiedName);
    }
    return qn === qualifiedName;
  }
  function matchesTagNameNS(eid, ns, localName) {
    if (ns !== "*" && elNamespace(eid) !== (ns === "" ? null : ns)) { return false; }
    if (localName !== "*" && elLocalName(eid) !== localName) { return false; }
    return true;
  }
  // Collect element-node descendants of `rootId` (excluding root) in tree order, matched by `pred`.
  function collectDescendants(rootId, pred) {
    var out = [];
    function visit(nid, isRoot) {
      if (!isRoot && __nodeType(nid) === 1 && pred(nid)) { out.push(wrap(nid)); }
      var kids = __children(nid);
      for (var i = 0; i < kids.length; i++) { visit(kids[i], false); }
    }
    visit(rootId, true);
    return out;
  }

  // --- Namespace lookup (DOM standard §node tree) ---------------------------------------------
  // These operate on raw node ids so they can be shared by Element / Document / Attr / DocumentType
  // wrappers. xmlns declarations live as ordinary attributes in the arena ("xmlns" / "xmlns:prefix").
  // "locate a namespace prefix" for an element id given a namespace.
  function locateNamespacePrefix(eid, ns) {
    if (elNamespace(eid) === ns && elMetaPrefix(eid) !== null) { return elMetaPrefix(eid); }
    var names = __attrNames(eid);
    for (var i = 0; i < names.length; i++) {
      var k = names[i];
      if (k === "xmlns") { continue; }
      if (k.indexOf("xmlns:") === 0 && __getAttr(eid, k) === ns) { return k.slice(6); }
    }
    var p = __parent(eid);
    if (p >= 0 && __nodeType(p) === 1) { return locateNamespacePrefix(p, ns); }
    return null;
  }
  // "locate a namespace" for a node id given a prefix (null/"" => default namespace).
  function locateNamespace(nid, prefix) {
    if (nid < 0) { return null; }
    var t = __nodeType(nid);
    if (t === 1) { // element
      // The `xml` / `xmlns` prefixes are bound to fixed namespaces (per browser behaviour). These
      // only resolve in an element context; for a bare DocumentFragment they stay null.
      if (prefix === "xml") { return XML_NS; }
      if (prefix === "xmlns") { return XMLNS_NS; }
      var elNs = elNamespace(nid);
      if (elNs != null && elMetaPrefix(nid) === (prefix == null ? null : prefix)) { return elNs; }
      var names = __attrNames(nid);
      for (var i = 0; i < names.length; i++) {
        var k = names[i];
        if (prefix != null && k === ("xmlns:" + prefix)) { var v = __getAttr(nid, k); return v === "" ? null : v; }
        if (prefix == null && k === "xmlns") { var v2 = __getAttr(nid, k); return v2 === "" ? null : v2; }
      }
      var p = __parent(nid);
      if (p >= 0 && __nodeType(p) === 1) { return locateNamespace(p, prefix); }
      return null;
    }
    if (t === 9) { // document → documentElement
      var de = __documentElementId();
      return de >= 0 ? locateNamespace(de, prefix) : null;
    }
    if (t === 10 || t === 7) { return null; } // DocumentType / PI
    // Text/Comment/Fragment: defer to the parent element.
    var pp = __parent(nid);
    if (pp >= 0 && __nodeType(pp) === 1) { return locateNamespace(pp, prefix); }
    return null;
  }
  function elMetaPrefix(eid) {
    var m = __nsMeta[eid];
    return m ? m.prefix : null;
  }
  function nodeLookupNamespaceURI(nid, prefix) {
    var p = (prefix === undefined || prefix === null || prefix === "") ? null : String(prefix);
    return locateNamespace(nid, p);
  }
  function nodeLookupPrefix(nid, ns) {
    if (ns == null || ns === "") { return null; }
    var t = __nodeType(nid);
    var startEl = -1;
    if (t === 1) { startEl = nid; }
    else if (t === 9) { startEl = __documentElementId(); }
    else { var p = __parent(nid); if (p >= 0 && __nodeType(p) === 1) { startEl = p; } }
    if (startEl < 0) { return null; }
    return locateNamespacePrefix(startEl, String(ns));
  }
  function nodeIsDefaultNamespace(nid, ns) {
    var want = (ns == null || ns === "") ? null : String(ns);
    var def = locateNamespace(nid, null);
    return def === want;
  }
  def(globalThis, "__nodeLookupNamespaceURI", nodeLookupNamespaceURI);
  def(globalThis, "__nodeLookupPrefix", nodeLookupPrefix);
  def(globalThis, "__nodeIsDefaultNamespace", nodeIsDefaultNamespace);

  // --- DOM mutation shared helpers (used across the ChildNode/ParentNode mixins) --------------
  function hierarchyRequestError(msg) {
    throw new globalThis.DOMException(msg || "The operation would yield an incorrect node tree.", "HierarchyRequestError");
  }
  function notFoundError(msg) {
    throw new globalThis.DOMException(msg || "The object can not be found here.", "NotFoundError");
  }
  // The node id of an argument: real DOM-arena nodes carry `__node`; strings/anything else => -1.
  function nodeIdOf(x) { return (x && typeof x.__node === "number") ? x.__node : -1; }
  // WebIDL: a non-nullable `Node` parameter throws a TypeError (not a DOMException) when the value
  // isn't a Node. Returns the node id on success.
  function requireNodeArg(x, methodName) {
    var nid = nodeIdOf(x);
    if (nid < 0) {
      throw new TypeError("Failed to execute '" + methodName + "': parameter is not of type 'Node'.");
    }
    return nid;
  }

  // "convert nodes into a node" (DOM standard): a list of (Node | string) becomes a single node.
  // Strings become Text nodes. A single node is returned as-is; multiple nodes (or zero) are
  // collected into a DocumentFragment. Returns a node id (or -1 if the result is an empty fragment
  // that the caller may still insert as a no-op).
  function convertNodesIntoNode(args) {
    var ids = [];
    for (var i = 0; i < args.length; i++) {
      var a = args[i];
      var nid = nodeIdOf(a);
      if (nid >= 0) { ids.push(nid); }
      // Non-Node args are DOMStrings: null -> "null", undefined -> "undefined" (WebIDL stringify).
      else { ids.push(__createText(String(a))); }
    }
    if (ids.length === 1) { return ids[0]; }
    var frag = __createDocumentFragment();
    for (var j = 0; j < ids.length; j++) { __appendChild(frag, ids[j]); }
    return frag;
  }

  // True if `ancestorId` is an inclusive ancestor of `nodeId` (would create a cycle on insert).
  function isInclusiveAncestor(ancestorId, nodeId) {
    var cur = nodeId;
    while (cur >= 0) { if (cur === ancestorId) { return true; } cur = __parent(cur); }
    return false;
  }

  // Pre-insertion validity (subset relevant here): parent must be a Document/Fragment/Element, the
  // node must not be an inclusive ancestor of parent, and `ref` (if given) must be a child of parent.
  function ensurePreInsertValid(parentId, nodeId, refId) {
    var pt = __nodeType(parentId);
    if (pt !== 1 && pt !== 9 && pt !== 11) {
      hierarchyRequestError("Cannot insert into a node that is not a Document, DocumentFragment, or Element.");
    }
    if (nodeId >= 0 && isInclusiveAncestor(nodeId, parentId)) {
      hierarchyRequestError("The new child element contains the parent.");
    }
    var nt = nodeId >= 0 ? __nodeType(nodeId) : -1;
    if (nt === 9) { hierarchyRequestError("Nodes of type Document may not be inserted."); }
    if (nodeId >= 0 && (nt === 1 || nt === 3 || nt === 8 || nt === 11) && (pt === 9)) {
      // Documents have additional constraints, but our tree is HTML-shaped; allow elements/fragments.
    }
    if (refId >= 0 && __parent(refId) !== parentId) {
      notFoundError("The reference child is not a child of this node.");
    }
  }

  // Insert `nodeId` (possibly a DocumentFragment, whose children are moved) into `parentId` before
  // `refId` (-1 = append). Returns the inserted node id. Validity must be checked by the caller.
  function insertNode(parentId, nodeId, refId) {
    if (nodeId < 0) { return nodeId; }
    if (__nodeType(nodeId) === 11) {
      var moving = __children(nodeId).slice();
      for (var i = 0; i < moving.length; i++) { __insertBefore(parentId, moving[i], refId); }
      if (globalThis.__ceOnInsert) { for (var k = 0; k < moving.length; k++) { try { globalThis.__ceOnInsert(moving[k]); } catch (e) {} } }
      if (globalThis.__adoptOnInsert) { for (var m = 0; m < moving.length; m++) { try { globalThis.__adoptOnInsert(moving[m]); } catch (e) {} } }
      return nodeId;
    }
    __insertBefore(parentId, nodeId, refId);
    // Custom Elements: a newly-connected element (and its subtree) may need upgrading + connectedCallback.
    if (globalThis.__ceOnInsert) { try { globalThis.__ceOnInsert(nodeId); } catch (e) {} }
    // Cross-document adoption: clear adoptedStyleSheets of shadow roots moved into a frame document.
    if (globalThis.__adoptOnInsert) { try { globalThis.__adoptOnInsert(nodeId); } catch (e) {} }
    return nodeId;
  }

  // The set of arg node-ids (used to skip them when picking a viable reference sibling).
  function argNodeIdSet(args) {
    var set = {};
    for (var i = 0; i < args.length; i++) { var n = nodeIdOf(args[i]); if (n >= 0) { set[n] = true; } }
    return set;
  }

  // ChildNode.before/after: insert `args` among this node's siblings. No-op if no parent. The
  // reference child is computed BEFORE the nodes are converted/moved, skipping any sibling that's
  // itself one of the arguments (DOM standard's "viable previous/next sibling").
  function childBefore(id, args) {
    var parent = __parent(id);
    if (parent < 0) { return; }
    var set = argNodeIdSet(args);
    var sibs = __children(parent);
    var idx = sibs.indexOf(id);
    // viablePreviousSibling: first preceding sibling not in args (it survives the conversion since
    // it isn't an arg), or null. Captured BEFORE conversion; resolved to a reference AFTER, per spec.
    var viablePrev = -1;
    for (var i = idx - 1; i >= 0; i--) { if (!set[sibs[i]]) { viablePrev = sibs[i]; break; } }
    var node = convertNodesIntoNode(args);
    var ref;
    if (viablePrev < 0) { var k = __children(parent); ref = k.length ? k[0] : -1; }
    else { var nk = __children(parent); var pi = nk.indexOf(viablePrev); ref = (pi >= 0 && pi + 1 < nk.length) ? nk[pi + 1] : -1; }
    insertNode(parent, node, ref);
  }
  function childAfter(id, args) {
    var parent = __parent(id);
    if (parent < 0) { return; }
    var set = argNodeIdSet(args);
    var sibs = __children(parent);
    var idx = sibs.indexOf(id);
    // viableNextSibling: first following sibling not in args, else null (append).
    var ref = -1;
    for (var i = idx + 1; i < sibs.length; i++) { if (!set[sibs[i]]) { ref = sibs[i]; break; } }
    var node = convertNodesIntoNode(args);
    insertNode(parent, node, ref);
  }
  function childReplaceWith(id, args) {
    var parent = __parent(id);
    if (parent < 0) { return; }
    var set = argNodeIdSet(args);
    var sibs = __children(parent);
    var idx = sibs.indexOf(id);
    var ref = -1;
    for (var i = idx + 1; i < sibs.length; i++) { if (!set[sibs[i]]) { ref = sibs[i]; break; } }
    var node = convertNodesIntoNode(args);
    // Spec: if this node still has the same parent (it wasn't moved into the fragment), replace it;
    // otherwise just insert before the viable next sibling. We always remove `id` then insert.
    if (__parent(id) === parent) { __removeChild(parent, id); }
    insertNode(parent, node, ref);
  }
  // Validate that no argument node is a host-including inclusive ancestor of `parentId`, and that
  // parentId is a valid insertion parent (Document/Fragment/Element). Run before any conversion.
  function ensureParentNodeArgsValid(parentId, args) {
    var pt = __nodeType(parentId);
    if (pt !== 1 && pt !== 9 && pt !== 11) {
      hierarchyRequestError("Cannot insert into a node that is not a Document, DocumentFragment, or Element.");
    }
    for (var i = 0; i < args.length; i++) {
      var n = nodeIdOf(args[i]);
      if (n >= 0 && isInclusiveAncestor(n, parentId)) {
        hierarchyRequestError("The new child element contains the parent.");
      }
    }
  }
  // ParentNode.prepend/append/replaceChildren on `id`.
  function parentPrepend(id, args) {
    ensureParentNodeArgsValid(id, args);
    var node = convertNodesIntoNode(args);
    var kids = __children(id);
    insertNode(id, node, kids.length ? kids[0] : -1);
  }
  function parentAppend(id, args) {
    ensureParentNodeArgsValid(id, args);
    var node = convertNodesIntoNode(args);
    insertNode(id, node, -1);
  }
  function parentReplaceChildren(id, args) {
    ensureParentNodeArgsValid(id, args);
    var node = convertNodesIntoNode(args);
    var old = __children(id).slice();
    for (var i = 0; i < old.length; i++) { __removeChild(id, old[i]); }
    insertNode(id, node, -1);
  }
  def(globalThis, "__convertNodesIntoNode", convertNodesIntoNode);
  def(globalThis, "__insertNode", insertNode);

  // Mirror the JS-side `__nsMeta` (namespace metadata for createElementNS elements) from a source
  // subtree onto a freshly-cloned subtree. The Rust clone preserves child order, so we walk both
  // trees in lockstep. Attributes themselves are copied arena-side; only namespace info lives in JS.
  function copyNsMetaDeep(srcId, dstId) {
    var m = __nsMeta[srcId];
    if (m) {
      __nsMeta[dstId] = { namespaceURI: m.namespaceURI, prefix: m.prefix, localName: m.localName,
                          qualifiedName: m.qualifiedName, isHTML: m.isHTML };
    }
    var sk = __children(srcId), dk = __children(dstId);
    var n = Math.min(sk.length, dk.length);
    for (var i = 0; i < n; i++) { copyNsMetaDeep(sk[i], dk[i]); }
  }

  // ---- Tree-position primitives (shared by compareDocumentPosition + Range boundary points) ----
  // The root (furthest ancestor) of a node id.
  function __rootId(id) { var c = id; while (true) { var p = __parent(c); if (p < 0) { return c; } c = p; } }
  // A boundary point (node, offset) as a root-relative key: the indices of each ancestor within its
  // parent (root downward), then the offset. Lexicographic comparison of two such keys (shorter key
  // is "before" when it is a prefix) reproduces the DOM "position of boundary point relative to
  // boundary point" algorithm exactly — and, with offset omitted, plain node tree order.
  function __pathKey(id, offset) {
    var path = []; var c = id;
    while (true) { var p = __parent(c); if (p < 0) { break; } path.push(__children(p).indexOf(c)); c = p; }
    path.reverse();
    if (offset !== undefined) { path.push(offset); }
    return path;
  }
  function __cmpKey(a, b) {
    var n = a.length < b.length ? a.length : b.length;
    for (var i = 0; i < n; i++) { if (a[i] < b[i]) { return -1; } if (a[i] > b[i]) { return 1; } }
    if (a.length < b.length) { return -1; }
    if (a.length > b.length) { return 1; }
    return 0;
  }
  def(globalThis, "__rootId", __rootId);
  def(globalThis, "__pathKey", __pathKey);
  def(globalThis, "__cmpKey", __cmpKey);
  // compareDocumentPosition bitmask for otherId relative to thisId (node1 = other, node2 = this).
  var DOCPOS = { DISCONNECTED: 1, PRECEDING: 2, FOLLOWING: 4, CONTAINS: 8, CONTAINED_BY: 16, IMPLEMENTATION_SPECIFIC: 32 };
  function __cmpDocPos(thisId, otherId) {
    if (otherId === thisId) { return 0; }
    if (otherId < 0 || thisId < 0) {
      return DOCPOS.DISCONNECTED | DOCPOS.IMPLEMENTATION_SPECIFIC | (otherId < thisId ? DOCPOS.PRECEDING : DOCPOS.FOLLOWING);
    }
    if (__rootId(otherId) !== __rootId(thisId)) {
      // Disconnected: pick a consistent (implementation-specific) ordering by id.
      return DOCPOS.DISCONNECTED | DOCPOS.IMPLEMENTATION_SPECIFIC | (otherId < thisId ? DOCPOS.PRECEDING : DOCPOS.FOLLOWING);
    }
    var ko = __pathKey(otherId), kt = __pathKey(thisId);
    var n = ko.length < kt.length ? ko.length : kt.length;
    for (var i = 0; i < n; i++) {
      if (ko[i] !== kt[i]) { return ko[i] < kt[i] ? DOCPOS.PRECEDING : DOCPOS.FOLLOWING; }
    }
    // One path is a prefix of the other → ancestor/descendant.
    if (ko.length < kt.length) { return DOCPOS.CONTAINS | DOCPOS.PRECEDING; }   // other is ancestor of this
    return DOCPOS.CONTAINED_BY | DOCPOS.FOLLOWING;                              // other is descendant of this
  }
  def(globalThis, "__cmpDocPos", __cmpDocPos);

  // Build a fresh element wrapper object for a node id. Carries `__node` plus accessors/methods
  // that delegate to the native primitives. Returns null for id === -1.
  function wrap(id) {
    if (typeof id !== "number" || id < 0) { return null; }
    var el = {};
    def(el, NODE, id);

    function uc(s) { return String(s == null ? "" : s).toUpperCase(); }

    // tagName resolution, honouring createElementNS metadata. HTML-namespace elements uppercase
    // their tagName; other namespaces preserve the qualifiedName exactly as given. Parsed elements
    // (no metadata) are HTML by default → uppercase of the lowercased arena tag.
    function elTagName() {
      var m = __nsMeta[id];
      if (m) {
        if (m.isHTML) { return asciiUpper(m.qualifiedName); }
        return m.qualifiedName;
      }
      return uc(__tag(id));
    }
    Object.defineProperty(el, "tagName", { get: elTagName, enumerable: true, configurable: true });
    Object.defineProperty(el, "nodeName", { get: function () {
      var t = __nodeType(id);
      if (t === 3) { return "#text"; }
      if (t === 8) { return "#comment"; }
      if (t === 9) { return "#document"; }
      if (t === 11) { return "#document-fragment"; }
      if (t === 10) { var di = __doctypeInfo(id); return di ? di.name : ""; }   // DocumentType: nodeName === name
      if (t === 7) { return __piTarget(id); }                                   // PI: nodeName === target
      return elTagName();
    }, enumerable: true, configurable: true });
    // DocumentType reflection (name / publicId / systemId) and ProcessingInstruction.target.
    if (__nodeType(id) === 10) {
      Object.defineProperty(el, "name", { get: function () { var d = __doctypeInfo(id); return d ? d.name : ""; }, enumerable: true, configurable: true });
      Object.defineProperty(el, "publicId", { get: function () { var d = __doctypeInfo(id); return d ? d.publicId : ""; }, enumerable: true, configurable: true });
      Object.defineProperty(el, "systemId", { get: function () { var d = __doctypeInfo(id); return d ? d.systemId : ""; }, enumerable: true, configurable: true });
    }
    if (__nodeType(id) === 7) {
      Object.defineProperty(el, "target", { get: function () { return __piTarget(id); }, enumerable: true, configurable: true });
    }
    Object.defineProperty(el, "namespaceURI", { get: function () {
      var m = __nsMeta[id];
      if (m) { return m.namespaceURI; }
      return __nodeType(id) === 1 ? __namespaceUri(id) : null;
    }, enumerable: true, configurable: true });
    Object.defineProperty(el, "prefix", { get: function () {
      var m = __nsMeta[id];
      return m ? m.prefix : null;
    }, enumerable: true, configurable: true });
    Object.defineProperty(el, "localName", { get: function () {
      var m = __nsMeta[id];
      if (m) { return m.localName; }
      return __nodeType(id) === 1 ? __tag(id) : null;
    }, enumerable: true, configurable: true });
    Object.defineProperty(el, "nodeType", { get: function () { return __nodeType(id); }, enumerable: true, configurable: true });

    Object.defineProperty(el, "textContent", {
      // Per spec: null for Document (9) and DocumentType (10); for everything else (Element,
      // DocumentFragment, Text, Comment, PI) the concatenation / data computed natively.
      get: function () { var t = __nodeType(id); return (t === 9 || t === 10) ? null : __textContent(id); },
      // Setter is a no-op on Document/DocumentType (textContent is null there).
      set: function (v) { var t = __nodeType(id); if (t === 9 || t === 10) { return; } __setTextContent(id, v == null ? "" : String(v)); },
      enumerable: true, configurable: true
    });
    // `data` mirrors textContent — used by Vue when patching text/comment anchors. This is a
    // CharacterData property, so only install it on Text/Comment/ProcessingInstruction nodes; on
    // element nodes `data` is a reflected content attribute (e.g. <object>.data is a URL), so leave
    // it free for the reflection layer.
    if (__nodeType(id) !== 1) {
      // `data` is a [LegacyNullToEmptyString] DOMString: only `null` becomes "" (undefined -> the
      // string "undefined", 0 -> "0", etc.).
      Object.defineProperty(el, "data", {
        get: function () { return __textContent(id); },
        set: function (v) { __setTextContent(id, v === null ? "" : String(v)); },
        enumerable: true, configurable: true
      });
      // CharacterData.length: the number of UTF-16 code units in `data`.
      Object.defineProperty(el, "length", {
        get: function () { return __textContent(id).length; },
        enumerable: true, configurable: true
      });
      // The CharacterData mutation methods all reduce to "replace data": offset/count are WebIDL
      // `unsigned long` (ToUint32, i.e. `>>> 0`), an out-of-range offset throws IndexSizeError, and a
      // count running past the end is clamped. Operations are in UTF-16 code units (JS string units).
      var __cdReplace = function (offset, count, insert) {
        var d = __textContent(id);
        var len = d.length;
        if (offset > len) { throw new globalThis.DOMException("The index is not in the allowed range.", "IndexSizeError"); }
        if (offset + count > len) { count = len - offset; }
        __setTextContent(id, d.slice(0, offset) + insert + d.slice(offset + count));
      };
      def(el, "substringData", function (offset, count) {
        if (arguments.length < 2) { throw new TypeError("Failed to execute 'substringData': 2 arguments required."); }
        var d = __textContent(id), len = d.length;
        offset = offset >>> 0; count = count >>> 0;
        if (offset > len) { throw new globalThis.DOMException("The index is not in the allowed range.", "IndexSizeError"); }
        var end = offset + count; if (end > len) { end = len; }
        return d.slice(offset, end);
      });
      def(el, "appendData", function (data) {
        if (arguments.length < 1) { throw new TypeError("Failed to execute 'appendData': 1 argument required."); }
        __setTextContent(id, __textContent(id) + String(data));
      });
      def(el, "insertData", function (offset, data) {
        if (arguments.length < 2) { throw new TypeError("Failed to execute 'insertData': 2 arguments required."); }
        __cdReplace(offset >>> 0, 0, String(data));
      });
      def(el, "deleteData", function (offset, count) {
        if (arguments.length < 2) { throw new TypeError("Failed to execute 'deleteData': 2 arguments required."); }
        __cdReplace(offset >>> 0, count >>> 0, "");
      });
      def(el, "replaceData", function (offset, count, data) {
        if (arguments.length < 3) { throw new TypeError("Failed to execute 'replaceData': 3 arguments required."); }
        __cdReplace(offset >>> 0, count >>> 0, String(data));
      });
    }
    Object.defineProperty(el, "nodeValue", {
      // nodeValue is the data for the CharacterData kinds (Text=3, CDATASection=4, PI=7, Comment=8);
      // null for everything else.
      get: function () { var t = __nodeType(id); return (t === 3 || t === 4 || t === 7 || t === 8) ? __textContent(id) : null; },
      set: function (v) { __setTextContent(id, v == null ? "" : String(v)); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(el, "innerHTML", {
      get: function () { return __innerHTML(id); },
      set: function (v) { __setInnerHTML(id, v == null ? "" : String(v)); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(el, "outerHTML", {
      get: function () { try { return __innerHTML(__parent(id) >= 0 ? id : id); } catch (e) { return ""; } },
      enumerable: true, configurable: true
    });
    // innerText / outerText are HTMLElement-only (not on SVG/MathML elements). The getter is the
    // rendered-text algorithm; the setters build a fragment (text runs + <br>s). innerText replaces
    // children; outerText replaces the element and throws NoModificationAllowedError when detached.
    // Both attributes are [LegacyNullToEmptyString]: null -> "", undefined -> the string "undefined".
    // The HTMLElement-only gate lives in the native primitives (which see the parsed namespace):
    // on SVG/MathML the getter yields `undefined` and the setters are no-ops.
    Object.defineProperty(el, "innerText", {
      get: function () { return __innerText(id); },
      set: function (v) { __setInnerText(id, v === null ? "" : String(v)); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(el, "outerText", {
      get: function () { return __innerText(id); },
      set: function (v) {
        if (!__setOuterText(id, v === null ? "" : String(v))) {
          throw new globalThis.DOMException("The object can not be modified.", "NoModificationAllowedError");
        }
      },
      enumerable: true, configurable: true
    });
    // setHTMLUnsafe(html): parse `html` and replace this element's children, like innerHTML but
    // without sanitization (we do not sanitize anyway). The `template`/shadowroot semantics of the
    // real algorithm are not modeled; a plain reparse covers the WPT callers that use it as a
    // convenience to install markup. getHTML() serializes back (≈ innerHTML).
    def(el, "setHTMLUnsafe", function (html) { __setInnerHTML(id, html == null ? "" : String(html)); });
    def(el, "getHTML", function () { return __innerHTML(id); });

    // id / className are DOMString reflections: null/undefined stringify to "null"/"undefined".
    Object.defineProperty(el, "id", {
      get: function () { var v = __getAttr(id, "id"); return v == null ? "" : v; },
      set: function (v) { __setAttr(id, "id", String(v)); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(el, "className", {
      get: function () { var v = __getAttr(id, "class"); return v == null ? "" : v; },
      set: function (v) { __setAttr(id, "class", String(v)); },
      enumerable: true, configurable: true
    });

    // Per-element attribute namespace metadata: keyed by the qualified-name storage key, holds
    // { namespaceURI, prefix, localName } so getAttributeNS / Attr.localName reflect correctly.
    function elIsHtml() {
      var m = __nsMeta[id];
      return m ? m.isHTML : (__nodeType(id) === 1);
    }
    def(el, "getAttribute", function (name) {
      // HTML elements ASCII-lowercase the qualified name before matching (stored lowercased).
      var nm = String(name);
      if (elIsHtml()) { nm = asciiLower(nm); }
      return __getAttr(id, nm);
    });
    def(el, "setAttribute", function (name, value) {
      var nm = String(name);
      // "validate" the qualified name: reject the empty string and any name containing whitespace
      // or '>' (matching the lenient Name production browsers/WPT accept).
      if (nm.length === 0) { invalidCharacterError(); }
      for (var vi = 0; vi < nm.length; vi++) {
        var vc = nm.charCodeAt(vi);
        if (vc === 0x3E || vc === 0x20 || vc === 0x09 || vc === 0x0A || vc === 0x0C || vc === 0x0D) { invalidCharacterError(); }
      }
      // HTML elements ASCII-lowercase the attribute's qualified name.
      if (elIsHtml()) { nm = asciiLower(nm); }
      // `value` is a non-nullable DOMString in WebIDL: undefined -> "undefined", null -> "null".
      __setAttr(id, nm, String(value));
    });
    def(el, "removeAttribute", function (name) {
      var nm = String(name);
      if (elIsHtml()) { nm = asciiLower(nm); }
      __detachCachedAttr(nm);
      __removeAttr(id, nm); delete __attrNs[nm]; delete __attrNodeCache[nm];
    });
    def(el, "hasAttribute", function (name) {
      var nm = String(name);
      if (elIsHtml()) { nm = asciiLower(nm); }
      return __getAttr(id, nm) != null;
    });
    def(el, "getAttributeNames", function () { return __attrNames(id); });

    // Namespaced attribute accessors. The arena keys attrs by their qualified name; we keep the
    // namespace/prefix/localName split in __attrNs so getAttributeNS and Attr reflection work.
    var __attrNs = {};
    def(el, "setAttributeNS", function (ns, qualifiedName, value) {
      var ex = validateAndExtract(ns, qualifiedName);
      var key = String(qualifiedName);
      __setAttr(id, key, value == null ? "" : String(value));
      __attrNs[key] = { namespaceURI: ex.namespace, prefix: ex.prefix, localName: ex.localName };
    });
    def(el, "getAttributeNS", function (ns, localName) {
      var want = (ns === undefined || ns === null || ns === "") ? null : String(ns);
      var ln = String(localName);
      var names = __attrNames(id);
      for (var i = 0; i < names.length; i++) {
        var k = names[i];
        var meta = __attrNs[k];
        var kNs = meta ? meta.namespaceURI : null;
        var kLocal = meta ? meta.localName : k;
        if (kNs === want && kLocal === ln) { return __getAttr(id, k); }
      }
      return null;
    });
    def(el, "hasAttributeNS", function (ns, localName) {
      return el.getAttributeNS(ns, localName) != null;
    });
    def(el, "removeAttributeNS", function (ns, localName) {
      var want = (ns === undefined || ns === null || ns === "") ? null : String(ns);
      var ln = String(localName);
      var names = __attrNames(id);
      for (var i = 0; i < names.length; i++) {
        var k = names[i];
        var meta = __attrNs[k];
        var kNs = meta ? meta.namespaceURI : null;
        var kLocal = meta ? meta.localName : k;
        if (kNs === want && kLocal === ln) { __detachCachedAttr(k); __removeAttr(id, k); delete __attrNs[k]; delete __attrNodeCache[k]; return; }
      }
    });

    // A LIVE NamedNodeMap: React (and others) do `for (var a = el.attributes; a.length;)
    // el.removeAttributeNode(a[0])`, capturing the map once and relying on removals shrinking it —
    // so length/index must re-query the node each access (a static snapshot would infinite-loop).
    // A *bound* Attr node, keyed by its qualified-name storage key. `ownerElement` is live (becomes
    // null once the attribute is removed), and value get/set reads/writes the live arena attribute.
    // Cached per storage key so `el.attributes[0] === el.getAttributeNode(name)` (object identity).
    var __attrNodeCache = {};
    var makeAttr = function (attrName) {
      if (__attrNodeCache[attrName]) { return __attrNodeCache[attrName]; }
      var meta = __attrNs[attrName];
      var attr = { nodeName: attrName, name: attrName, nodeType: 2,
               namespaceURI: meta ? meta.namespaceURI : null,
               prefix: meta ? meta.prefix : null,
               localName: meta ? meta.localName : attrName,
               specified: true };
      Object.defineProperty(attr, "ownerElement", {
        get: function () { return __getAttr(id, attrName) == null ? null : el; },
        enumerable: true, configurable: true
      });
      var setVal = function (v) { __setAttr(id, attrName, v == null ? "" : String(v)); };
      var getVal = function () { var v = __getAttr(id, attrName); return v == null ? "" : v; };
      Object.defineProperty(attr, "value", { get: getVal, set: setVal, enumerable: true, configurable: true });
      Object.defineProperty(attr, "nodeValue", { get: getVal, set: setVal, enumerable: true, configurable: true });
      Object.defineProperty(attr, "textContent", { get: getVal, set: setVal, enumerable: true, configurable: true });
      def(attr, "lookupNamespaceURI", function (prefix) { return nodeLookupNamespaceURI(id, prefix); });
      def(attr, "lookupPrefix", function (ns) { return nodeLookupPrefix(id, ns); });
      def(attr, "isDefaultNamespace", function (ns) { return nodeIsDefaultNamespace(id, ns); });
      try { if (globalThis.Attr && globalThis.Attr.prototype) { Object.setPrototypeOf(attr, globalThis.Attr.prototype); } } catch (e) {}
      __attrNodeCache[attrName] = attr;
      return attr;
    };
    // If a cached Attr node exists for `key`, snapshot its current value into a standalone closure
    // and null its ownerElement. Call BEFORE removing the arena attribute so the detached node keeps
    // the value it had while connected (per spec, an Attr retains its value after removal).
    function __detachCachedAttr(key) {
      var a = __attrNodeCache[key];
      if (!a) { return; }
      var stored = __getAttr(id, key); if (stored == null) { stored = ""; }
      try {
        var dget = function () { return stored; };
        var dset = function (v) { stored = v == null ? "" : String(v); };
        Object.defineProperty(a, "value", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(a, "nodeValue", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(a, "textContent", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(a, "ownerElement", { value: null, configurable: true, enumerable: true, writable: true });
      } catch (e) {}
    }
    // Find the storage key of an attribute by (namespace, localName); null if absent.
    function attrKeyByNs(want, ln) {
      var names = __attrNames(id);
      for (var i = 0; i < names.length; i++) {
        var k = names[i], meta = __attrNs[k];
        var kNs = meta ? meta.namespaceURI : null;
        var kLocal = meta ? meta.localName : k;
        if (kNs === want && kLocal === ln) { return k; }
      }
      return null;
    }
    var attrMap = new Proxy({}, {
      get: function (t, prop) {
        if (prop === "length") { return __attrNames(id).length; }
        if (prop === "item") { return function (i) { var n = __attrNames(id)[i >>> 0]; return n == null ? null : makeAttr(n); }; }
        if (prop === "getNamedItem") { return function (nm) { return el.getAttributeNode(nm); }; }
        if (prop === "getNamedItemNS") { return function (ns, ln) { return el.getAttributeNodeNS(ns, ln); }; }
        if (prop === "setNamedItem" || prop === "setNamedItemNS") { return function (attr) { return el.setAttributeNode(attr); }; }
        if (prop === "removeNamedItem") { return function (nm) {
          var a = el.getAttributeNode(nm);
          if (a == null) { notFoundError("No attribute named '" + nm + "'."); }
          return el.removeAttributeNode(a);
        }; }
        if (prop === "removeNamedItemNS") { return function (ns, ln) {
          var a = el.getAttributeNodeNS(ns, ln);
          if (a == null) { notFoundError("No such attribute."); }
          return el.removeAttributeNode(a);
        }; }
        if (prop === Symbol.iterator) { return function () { return __attrNames(id).map(makeAttr)[Symbol.iterator](); }; }
        if (typeof prop === "string" && /^\d+$/.test(prop)) { var n = __attrNames(id)[+prop]; return n == null ? undefined : makeAttr(n); }
        // Named property access: getNamedItem(prop).
        if (typeof prop === "string" && __getAttr(id, prop) != null) { return makeAttr(prop); }
        return t[prop];
      },
      has: function (t, prop) {
        if (prop === "length" || prop === "item" || prop === "getNamedItem" || prop === "getNamedItemNS" ||
            prop === "setNamedItem" || prop === "setNamedItemNS" || prop === "removeNamedItem" || prop === "removeNamedItemNS") { return true; }
        if (typeof prop === "string" && /^\d+$/.test(prop)) { return +prop < __attrNames(id).length; }
        return prop in t;
      },
      // Own-property enumeration: getOwnPropertyNames(attrs) === [indices..., qualifiedNames...].
      // Indices are enumerable; the named (qualified-name) keys are non-enumerable own properties.
      ownKeys: function () {
        var names = __attrNames(id), keys = [];
        for (var i = 0; i < names.length; i++) { keys.push(String(i)); }
        var seen = Object.create(null);
        for (var j = 0; j < names.length; j++) { if (!seen[names[j]]) { seen[names[j]] = 1; keys.push(names[j]); } }
        return keys;
      },
      getOwnPropertyDescriptor: function (t, prop) {
        if (typeof prop === "string" && /^\d+$/.test(prop)) {
          var nm = __attrNames(id)[+prop];
          if (nm != null) { return { value: makeAttr(nm), writable: false, enumerable: true, configurable: true }; }
          return undefined;
        }
        if (prop === "length") { return { value: __attrNames(id).length, writable: false, enumerable: false, configurable: true }; }
        if (typeof prop === "string" && __getAttr(id, prop) != null) {
          // A named (qualified-name) own property: non-enumerable, holds the Attr.
          return { value: makeAttr(prop), writable: false, enumerable: false, configurable: true };
        }
        return Object.getOwnPropertyDescriptor(t, prop);
      }
    });
    Object.defineProperty(el, "attributes", { get: function () { return attrMap; }, configurable: true });
    def(el, "removeAttributeNode", function (attr) {
      // Spec: if attr's element isn't this element, throw NotFoundError. Then remove it and detach
      // the SAME Attr object (it keeps its last value/name; ownerElement becomes null).
      if (!attr || attr.nodeType !== 2) { throw new TypeError("parameter is not an Attr."); }
      var key = (attr.__attrKey != null) ? attr.__attrKey : String(attr.name);
      if (__getAttr(id, key) == null) { notFoundError("The attribute is not part of this element."); }
      var finalVal = __getAttr(id, key);
      __removeAttr(id, key); delete __attrNs[key]; delete __attrNodeCache[key];
      // Re-bind the node's value/ownerElement to a standalone (detached) state.
      try {
        var stored = finalVal == null ? "" : String(finalVal);
        var dget = function () { return stored; };
        var dset = function (v) { stored = v == null ? "" : String(v); };
        Object.defineProperty(attr, "value", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "nodeValue", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "textContent", { get: dget, set: dset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "ownerElement", { value: null, configurable: true, enumerable: true, writable: true });
      } catch (e) {}
      return attr;
    });
    def(el, "getAttributeNode", function (name) {
      var nm = String(name);
      if (elIsHtml()) { nm = asciiLower(nm); }
      return __getAttr(id, nm) == null ? null : makeAttr(nm);
    });
    def(el, "getAttributeNodeNS", function (ns, localName) {
      var want = (ns === undefined || ns === null || ns === "") ? null : String(ns);
      var key = attrKeyByNs(want, String(localName));
      return key == null ? null : makeAttr(key);
    });
    // setAttributeNode(attr): set/replace the attribute named by attr; per spec, throw
    // InUseAttributeError if attr is already owned by a *different* element. Returns the previously
    // set Attr (or null). For a same-name replacement, returns the old attr value.
    function setAttrNodeImpl(attr) {
      if (!attr || attr.nodeType !== 2) { throw new TypeError("parameter is not an Attr."); }
      var owner = attr.ownerElement;
      if (owner != null && owner !== el) {
        throw new globalThis.DOMException("The attribute is in use by another element.", "InUseAttributeError");
      }
      var ns = attr.namespaceURI || null;
      var ln = attr.localName != null ? String(attr.localName) : String(attr.name);
      var key = String(attr.name);
      // Existing attribute with the same namespace + localName?
      var oldKey = attrKeyByNs(ns, ln);
      var oldAttr = null;
      if (oldKey != null) {
        oldAttr = makeAttr(oldKey);
        if (oldKey !== key) { __removeAttr(id, oldKey); delete __attrNs[oldKey]; delete __attrNodeCache[oldKey]; }
      }
      var newVal = attr.value == null ? "" : String(attr.value);
      __setAttr(id, key, newVal);
      __attrNs[key] = { namespaceURI: ns, prefix: attr.prefix || null, localName: ln };
      // Adopt the SAME Attr object: re-bind its value / ownerElement getters to this element's live
      // arena attribute, and register it as the canonical cached node so getAttributeNode /
      // el.attributes[i] return the identical object (per spec the node is moved, not copied).
      try { def(attr, "__attrKey", key); } catch (e) {}
      try {
        var bget = function () { var v = __getAttr(id, key); return v == null ? "" : v; };
        var bset = function (v) { __setAttr(id, key, v == null ? "" : String(v)); };
        Object.defineProperty(attr, "value", { get: bget, set: bset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "nodeValue", { get: bget, set: bset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "textContent", { get: bget, set: bset, configurable: true, enumerable: true });
        Object.defineProperty(attr, "ownerElement", { get: function () { return __getAttr(id, key) == null ? null : el; }, configurable: true, enumerable: true });
      } catch (e) {}
      __attrNodeCache[key] = attr;
      return oldAttr;
    }
    def(el, "setAttributeNode", setAttrNodeImpl);
    def(el, "setAttributeNodeNS", setAttrNodeImpl);
    def(el, "toggleAttribute", function (name, force) {
      var qn = String(name);
      // "validate" only rejects names that don't match the (lenient) Name production: the empty
      // string and any name containing whitespace or '>' (matching setAttribute's behaviour).
      if (qn.length === 0) { invalidCharacterError(); }
      for (var vi = 0; vi < qn.length; vi++) {
        var vc = qn.charCodeAt(vi);
        if (vc === 0x3E || vc === 0x20 || vc === 0x09 || vc === 0x0A || vc === 0x0C || vc === 0x0D) { invalidCharacterError(); }
      }
      if (elIsHtml()) { qn = asciiLower(qn); }
      var present = __getAttr(id, qn) != null;
      if (!present) {
        if (force === undefined || force === true) { __setAttr(id, qn, ""); return true; }
        return false;
      }
      if (force === undefined || force === false) { __removeAttr(id, qn); delete __attrNs[qn]; return false; }
      return true;
    });

    def(el, "appendChild", function (child) {
      var cid = requireNodeArg(child, "appendChild");
      ensurePreInsertValid(id, cid, -1);
      insertNode(id, cid, -1);
      return child;
    });
    def(el, "removeChild", function (child) {
      var cid = requireNodeArg(child, "removeChild");
      if (__parent(cid) !== id) { notFoundError("The node to be removed is not a child of this node."); }
      __removeChild(id, cid);
      return child;
    });
    def(el, "insertBefore", function (newNode, refNode) {
      var cid = requireNodeArg(newNode, "insertBefore");
      var refId = (refNode == null) ? -1 : nodeIdOf(refNode);
      if (refNode != null && refId < 0) { notFoundError("The reference child is not a child of this node."); }
      ensurePreInsertValid(id, cid, refId);
      insertNode(id, cid, refId);
      return newNode;
    });
    def(el, "replaceChild", function (newNode, oldNode) {
      var nid = requireNodeArg(newNode, "replaceChild"), oid = requireNodeArg(oldNode, "replaceChild");
      if (__parent(oid) !== id) { notFoundError("The node to be replaced is not a child of this node."); }
      if (isInclusiveAncestor(nid, id)) { hierarchyRequestError("The new child element contains the parent."); }
      // Reference child = oldNode's next sibling, unless that's newNode itself (then newNode's next).
      var sibs = __children(id); var idx = sibs.indexOf(oid);
      var ref = (idx >= 0 && idx + 1 < sibs.length) ? sibs[idx + 1] : -1;
      if (ref === nid) {
        var ni = sibs.indexOf(nid);
        ref = (ni >= 0 && ni + 1 < sibs.length) ? sibs[ni + 1] : -1;
      }
      __removeChild(id, oid);
      insertNode(id, nid, ref);
      return oldNode;
    });
    def(el, "remove", function () { var p = __parent(id); if (p >= 0) { __removeChild(p, id); } });
    def(el, "append", function () { parentAppend(id, arguments); });
    def(el, "prepend", function () { parentPrepend(id, arguments); });
    def(el, "replaceChildren", function () { parentReplaceChildren(id, arguments); });
    def(el, "before", function () { childBefore(id, arguments); });
    def(el, "after", function () { childAfter(id, arguments); });
    def(el, "replaceWith", function () { childReplaceWith(id, arguments); });
    def(el, "cloneNode", function (deep) {
      var nid = __cloneNode(id, !!deep);
      if (nid < 0) { return null; }
      copyNsMetaDeep(id, nid);
      var w = wrap(nid);
      // Route through the canonical-wrapper cache (when the browser-env layer is present) so the
      // clone has a stable identity and full enrichment (style/classList/childNodes === checks).
      return (typeof globalThis.__canonNode === "function") ? globalThis.__canonNode(w) : w;
    });

    def(el, "insertAdjacentElement", function (position, node) {
      var pos = String(position == null ? "" : position).toLowerCase();
      if (!node || typeof node.__node !== "number") { return null; }
      var nid = node.__node;
      var p;
      if (pos === "beforebegin") { p = __parent(id); if (p >= 0) { __insertBefore(p, nid, id); } }
      else if (pos === "afterbegin") { var k = __children(id); __insertBefore(id, nid, k.length ? k[0] : -1); }
      else if (pos === "beforeend") { __appendChild(id, nid); }
      else if (pos === "afterend") { p = __parent(id); if (p >= 0) { var sibs = __children(p); var idx = sibs.indexOf(id); var ref = (idx >= 0 && idx + 1 < sibs.length) ? sibs[idx + 1] : -1; __insertBefore(p, nid, ref); } }
      else { throw new globalThis.DOMException("Failed to execute 'insertAdjacentElement': '" + position + "' is not a valid value.", "SyntaxError"); }
      return node;
    });

    def(el, "insertAdjacentHTML", function (position, html) {
      var pos = String(position == null ? "" : position).toLowerCase();
      if (pos !== "beforebegin" && pos !== "afterbegin" && pos !== "beforeend" && pos !== "afterend") {
        throw new globalThis.DOMException("Failed to execute 'insertAdjacentHTML': '" + position + "' is not a valid value.", "SyntaxError");
      }
      // Parse the HTML fragment into real nodes via a temp container, then move them.
      var tmp = __createElement("template");
      __setInnerHTML(tmp, html == null ? "" : String(html));
      var parsed = __children(tmp).slice();
      if (pos === "beforebegin") {
        var p = __parent(id);
        if (p < 0 || __nodeType(p) === 9) { throw new globalThis.DOMException("Cannot insert adjacent to a node with no parent element.", "NoModificationAllowedError"); }
        for (var i = 0; i < parsed.length; i++) { __insertBefore(p, parsed[i], id); }
      } else if (pos === "afterbegin") {
        var k = __children(id); var ref = k.length ? k[0] : -1;
        for (var i = 0; i < parsed.length; i++) { __insertBefore(id, parsed[i], ref); }
      } else if (pos === "beforeend") {
        for (var i = 0; i < parsed.length; i++) { __appendChild(id, parsed[i]); }
      } else { // afterend
        var p2 = __parent(id);
        if (p2 < 0 || __nodeType(p2) === 9) { throw new globalThis.DOMException("Cannot insert adjacent to a node with no parent element.", "NoModificationAllowedError"); }
        var sibs = __children(p2); var idx = sibs.indexOf(id);
        var ref2 = (idx >= 0 && idx + 1 < sibs.length) ? sibs[idx + 1] : -1;
        for (var i = 0; i < parsed.length; i++) { __insertBefore(p2, parsed[i], ref2); }
      }
    });

    def(el, "insertAdjacentText", function (position, text) {
      var t = document.createTextNode(text == null ? "" : String(text));
      return el.insertAdjacentElement(position, t);
    });

    def(el, "contains", function (other) {
      if (!other || typeof other.__node !== "number") { return false; }
      var cur = other.__node;
      while (cur >= 0) { if (cur === id) { return true; } cur = __parent(cur); }
      return false;
    });

    def(el, "compareDocumentPosition", function (other) {
      if (!other || typeof other.__node !== "number") {
        throw new TypeError("Failed to execute 'compareDocumentPosition' on 'Node': parameter 1 is not of type 'Node'.");
      }
      return __cmpDocPos(id, other.__node);
    });

    def(el, "querySelector", function (sel) { var r = __querySelectorAllWithin(id, String(sel)); return r.length ? wrap(r[0]) : null; });
    def(el, "querySelectorAll", function (sel) {
      var ids = __querySelectorAllWithin(id, String(sel));
      return globalThis.__makeNodeList(function () { return ids.map(wrap); }, false);
    });
    def(el, "getElementsByTagName", function (tag) {
      var qn = String(tag);
      return globalThis.__makeHTMLCollection(function () {
        return collectDescendants(id, function (eid) { return matchesTagName(eid, qn); });
      });
    });
    def(el, "getElementsByTagNameNS", function (ns, localName) {
      var n = ns === "*" ? "*" : (ns == null || ns === "" ? null : String(ns));
      var ln = (localName === "*" || localName == null) ? "*" : String(localName);
      return globalThis.__makeHTMLCollection(function () {
        return collectDescendants(id, function (eid) { return matchesTagNameNS(eid, n, ln); });
      });
    });
    def(el, "getElementsByClassName", function (cls) {
      // Scope getElementsByClassName by filtering the global result to descendants of `id`.
      var classNames = String(cls);
      return globalThis.__makeHTMLCollection(function () {
        var all = __getElementsByClassName(classNames);
        var out = [];
        for (var i = 0; i < all.length; i++) {
          var cur = __parent(all[i]); var isDesc = false;
          while (cur >= 0) { if (cur === id) { isDesc = true; break; } cur = __parent(cur); }
          if (isDesc) { out.push(wrap(all[i])); }
        }
        return out;
      });
    });

    def(el, "matches", function (sel) {
      // An element matches `sel` if it appears in the document-wide result set.
      var r = __querySelectorAll(String(sel));
      for (var i = 0; i < r.length; i++) { if (r[i] === id) { return true; } }
      return false;
    });
    def(el, "closest", function (sel) {
      var cur = id;
      while (cur >= 0) {
        var w = wrap(cur);
        if (w && w.matches(sel)) { return w; }
        cur = __parent(cur);
      }
      return null;
    });

    // Navigation accessors. Identity-stable lookup for related nodes: __nodeFor returns the CANONICAL (cached) wrapper,
    // so node.parentNode / childNodes[i] / firstChild / siblings are === across repeated accesses
    // and === the wrapper other code holds for the same node. Plain wrap() mints a fresh object
    // each call, which breaks identity comparisons — e.g. the WPT idiom
    // `while (node.parentNode.childNodes[i] != node) i++` never matches and spins forever.
    function nf(x) { if (typeof x !== "number" || x < 0) { return null; } return globalThis.__nodeFor ? globalThis.__nodeFor(x) : wrap(x); }
    function childList(elementsOnly) {
      var kids = __children(id); var out = [];
      for (var i = 0; i < kids.length; i++) {
        if (!elementsOnly || __nodeType(kids[i]) === 1) { out.push(nf(kids[i])); }
      }
      return out;
    }
    var childrenCollection = null, childNodesList = null;
    Object.defineProperty(el, "children", { get: function () {
      if (!childrenCollection) { childrenCollection = globalThis.__makeHTMLCollection(function () { return childList(true); }); }
      return childrenCollection;
    }, enumerable: true, configurable: true });
    Object.defineProperty(el, "childNodes", { get: function () {
      if (!childNodesList) { childNodesList = globalThis.__makeNodeList(function () { return childList(false); }, true); }
      return childNodesList;
    }, enumerable: true, configurable: true });
    Object.defineProperty(el, "parentNode", { get: function () { return nf(__parent(id)); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "parentElement", { get: function () { var p = __parent(id); return p >= 0 ? nf(p) : null; }, enumerable: true, configurable: true });
    Object.defineProperty(el, "firstChild", { get: function () { var k = __children(id); return k.length ? nf(k[0]) : null; }, enumerable: true, configurable: true });
    Object.defineProperty(el, "lastChild", { get: function () { var k = __children(id); return k.length ? nf(k[k.length - 1]) : null; }, enumerable: true, configurable: true });
    Object.defineProperty(el, "firstElementChild", { get: function () { var c = childList(true); return c.length ? c[0] : null; }, enumerable: true, configurable: true });
    Object.defineProperty(el, "lastElementChild", { get: function () { var c = childList(true); return c.length ? c[c.length - 1] : null; }, enumerable: true, configurable: true });
    Object.defineProperty(el, "childElementCount", { get: function () { var k = __children(id), n = 0; for (var i = 0; i < k.length; i++) { if (__nodeType(k[i]) === 1) { n++; } } return n; }, enumerable: true, configurable: true });

    function sibling(next, elementOnly) {
      var p = __parent(id); if (p < 0) { return null; }
      var sibs = __children(p);
      var idx = sibs.indexOf(id); if (idx < 0) { return null; }
      var i = idx;
      while (true) {
        if (next) { i++; if (i >= sibs.length) { return null; } }
        else { i--; if (i < 0) { return null; } }
        if (!elementOnly || __nodeType(sibs[i]) === 1) { return nf(sibs[i]); }
      }
    }
    Object.defineProperty(el, "nextSibling", { get: function () { return sibling(true, false); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "previousSibling", { get: function () { return sibling(false, false); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "nextElementSibling", { get: function () { return sibling(true, true); }, enumerable: true, configurable: true });
    Object.defineProperty(el, "previousElementSibling", { get: function () { return sibling(false, true); }, enumerable: true, configurable: true });

    // Namespace lookup mixin (Node). DocumentType/PI/DocumentFragment wrappers also get these.
    def(el, "lookupNamespaceURI", function (prefix) { return nodeLookupNamespaceURI(id, prefix); });
    def(el, "lookupPrefix", function (ns) { return nodeLookupPrefix(id, ns); });
    def(el, "isDefaultNamespace", function (ns) { return nodeIsDefaultNamespace(id, ns); });

    // Return the CANONICAL wrapper for this node so identity is stable: every wrap(id) — whether
    // from createElement, a traversal getter, or __nodeFor — yields the same object for one node.
    // Without this each call mints a distinct object, breaking `===` and the WPT identity loops
    // (`while (node.parentNode.childNodes[i] != node) i++`). canon caches before enriching, so the
    // re-entrant lookup during enrichment is safe. Guarded because __canonNode (and its cache) is
    // installed after wrap is defined; the few wraps before then are re-canonicalized on next access.
    return globalThis.__canonNode ? globalThis.__canonNode(el) : el;
  }
  def(globalThis, "__wrapNode", wrap);

  // --- document --------------------------------------------------------------------------------
  var document = {};
  def(document, "getElementById", function (idStr) { var n = __getElementById(String(idStr)); return n >= 0 ? wrap(n) : null; });
  def(document, "getElementsByTagName", function (tag) {
    var qn = String(tag);
    return globalThis.__makeHTMLCollection(function () {
      return collectDescendants(0, function (eid) { return matchesTagName(eid, qn); });
    });
  });
  def(document, "getElementsByClassName", function (cls) {
    var classNames = String(cls);
    return globalThis.__makeHTMLCollection(function () { return __getElementsByClassName(classNames).map(wrap); });
  });
  def(document, "querySelector", function (sel) { var r = __querySelectorAll(String(sel)); return r.length ? wrap(r[0]) : null; });
  def(document, "querySelectorAll", function (sel) {
    var ids = __querySelectorAll(String(sel));
    return globalThis.__makeNodeList(function () { return ids.map(wrap); }, false);
  });
  function documentCollection(predicate) {
    return globalThis.__makeHTMLCollection(function () { return collectDescendants(0, predicate); });
  }
  function collectionProperty(name, collection) {
    Object.defineProperty(document, name, {
      get: function () { return collection; }, enumerable: true, configurable: true
    });
  }
  collectionProperty("images", documentCollection(function (id) { return __tag(id) === "img"; }));
  var embedsCollection = documentCollection(function (id) { return __tag(id) === "embed"; });
  collectionProperty("embeds", embedsCollection);
  collectionProperty("plugins", embedsCollection);
  collectionProperty("links", documentCollection(function (id) {
    var tag = __tag(id);
    return (tag === "a" || tag === "area") && __getAttr(id, "href") !== null;
  }));
  collectionProperty("forms", documentCollection(function (id) { return __tag(id) === "form"; }));
  collectionProperty("scripts", documentCollection(function (id) { return __tag(id) === "script"; }));
  collectionProperty("anchors", documentCollection(function (id) {
    return __tag(id) === "a" && __getAttr(id, "name") !== null;
  }));
  collectionProperty("applets", documentCollection(function (id) { return __tag(id) === "applet"; }));
  // document.write / writeln. We run scripts after the full parse (there is no live insertion point),
  // so the written markup is parsed and appended to <body> (or the documentElement) — enough for the
  // common case of a script writing extra elements (e.g. a <link>/<script>) into the page.
  def(document, "write", function () {
    var html = "";
    for (var i = 0; i < arguments.length; i++) { html += String(arguments[i]); }
    var target = document.body || document.documentElement;
    if (!target) { return; }
    var tmp = document.createElement("div");
    tmp.innerHTML = html;
    var kids = [];
    var cn = tmp.childNodes;
    for (var k = 0; k < cn.length; k++) { kids.push(cn[k]); }
    for (var j = 0; j < kids.length; j++) { try { target.appendChild(kids[j]); } catch (e) {} }
  });
  def(document, "writeln", function () {
    var a = Array.prototype.slice.call(arguments);
    a.push("\n");
    document.write.apply(document, a);
  });
  def(document, "createElement", function (tag) {
    // HTML document: validate the name as an XML Name, then ASCII-lowercase it. namespaceURI is the
    // HTML namespace, prefix null, localName the lowercased name, tagName the uppercased localName.
    var name = String(tag);
    // createElement validates the name as an XML Name (colons permitted), without splitting it
    // into prefix/localName. HTML documents then ASCII-lowercase the whole name.
    if (!isValidNameImpl(name, true)) { invalidCharacterError(); }
    var local = asciiLower(name);
    var id = __createElement(local);
    __nsMeta[id] = { namespaceURI: HTML_NS, prefix: null, localName: local, qualifiedName: local, isHTML: true };
    return wrap(id);
  });
  def(document, "createElementNS", function (ns, qualifiedName) {
    var ex = validateAndExtract(ns, qualifiedName);
    var isHTML = ex.namespace === HTML_NS;
    // The arena tag is the local name (lowercased only when HTML, to match parser behaviour).
    var arenaTag = isHTML ? asciiLower(ex.localName) : ex.localName;
    var id = __createElement(arenaTag);
    __nsMeta[id] = {
      namespaceURI: ex.namespace,
      prefix: ex.prefix,
      localName: ex.localName,
      qualifiedName: String(qualifiedName),
      isHTML: isHTML
    };
    return wrap(id);
  });
  // createAttribute / createAttributeNS return an Attr node (not arena-backed) with the correct
  // name/localName/namespaceURI/prefix/value reflection.
  function makeAttrNode(namespaceURI, prefix, localName, qualifiedName, initialValue) {
    var value = initialValue == null ? "" : String(initialValue);
    var attr = {
      nodeType: 2,
      namespaceURI: namespaceURI,
      prefix: prefix,
      localName: localName,
      name: qualifiedName,
      nodeName: qualifiedName,
      specified: true,
      ownerElement: null
    };
    Object.defineProperty(attr, "value", {
      get: function () { return value; },
      set: function (v) { value = v == null ? "" : String(v); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(attr, "nodeValue", {
      get: function () { return value; },
      set: function (v) { value = v == null ? "" : String(v); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(attr, "textContent", {
      get: function () { return value; },
      set: function (v) { value = v == null ? "" : String(v); },
      enumerable: true, configurable: true
    });
    // Attr namespace lookup delegates to the owner element (null when disconnected).
    def(attr, "lookupNamespaceURI", function (prefix) {
      var oe = this.ownerElement; return oe && oe.lookupNamespaceURI ? oe.lookupNamespaceURI(prefix) : null;
    });
    def(attr, "lookupPrefix", function (ns) {
      var oe = this.ownerElement; return oe && oe.lookupPrefix ? oe.lookupPrefix(ns) : null;
    });
    def(attr, "isDefaultNamespace", function (ns) {
      var oe = this.ownerElement; return oe && oe.isDefaultNamespace ? oe.isDefaultNamespace(ns) : (ns == null || ns === "");
    });
    try { if (globalThis.Attr && globalThis.Attr.prototype) { Object.setPrototypeOf(attr, globalThis.Attr.prototype); } } catch (e) {}
    return attr;
  }
  def(document, "createAttribute", function (localName) {
    // HTML document: validate (only the empty name is rejected here, matching browser behaviour)
    // then ASCII-lowercase. namespaceURI/prefix null.
    var name = String(localName);
    if (name.length === 0) { invalidCharacterError(); }
    var local = asciiLower(name);
    return makeAttrNode(null, null, local, local);
  });
  def(document, "createAttributeNS", function (ns, qualifiedName) {
    var ex = validateAndExtract(ns, qualifiedName);
    return makeAttrNode(ex.namespace, ex.prefix, ex.localName, String(qualifiedName));
  });
  // Expose the Attr factory + the validation helpers so the off-document (XML) document objects
  // built by document.implementation.createDocument can offer case-preserving createAttribute.
  def(globalThis, "__makeAttrNode", makeAttrNode);
  // createDocumentType: validate the qualified name (QName), then build a real DocumentType arena
  // node. Per spec a bad name is an InvalidCharacterError; a bad prefix split is a NamespaceError.
  def(globalThis, "__createDocumentTypeNode", function (qualifiedName, publicId, systemId) {
    var qn = String(qualifiedName);
    // createDocumentType's "validate" step only checks the QName matches the (lenient) Name
    // production. Per the behaviour browsers/WPT implement, every codepoint must be a NameChar:
    // any non-whitespace, non-'>' character is accepted mid-name (colons included), and the empty
    // string is allowed. A '>' or ASCII whitespace anywhere => InvalidCharacterError.
    for (var i = 0; i < qn.length; i++) {
      var cc = qn.charCodeAt(i);
      if (cc === 0x3E || cc === 0x20 || cc === 0x09 || cc === 0x0A || cc === 0x0C || cc === 0x0D) {
        invalidCharacterError();
      }
    }
    var nid = __createDocumentType(qn, publicId == null ? "" : String(publicId), systemId == null ? "" : String(systemId));
    return wrap(nid);
  });
  def(globalThis, "__validateAndExtractName", validateAndExtract);
  def(globalThis, "__invalidCharacterError", invalidCharacterError);
  // Create an element carrying explicit namespace metadata (used by XML-flavoured documents from
  // document.implementation.createDocument, whose createElement does NOT lowercase or assign the
  // HTML namespace). htmlNs => HTML-namespace semantics (lowercase + uppercase tagName).
  def(globalThis, "__createElementWithNs", function (namespaceURI, name) {
    var nm = String(name);
    if (!isValidNameImpl(nm, true)) { invalidCharacterError(); }
    var isHtml = namespaceURI === HTML_NS;
    var local = isHtml ? asciiLower(nm) : nm;
    var id = __createElement(local);
    __nsMeta[id] = {
      namespaceURI: (namespaceURI === undefined || namespaceURI === null || namespaceURI === "") ? null : String(namespaceURI),
      prefix: null, localName: local, qualifiedName: local, isHTML: isHtml
    };
    return wrap(id);
  });
  // Like __createElementWithNs, but NEVER ASCII-lowercases the name — the createElement steps for an
  // *XML* document (e.g. one from document.implementation.createDocument) preserve case even when the
  // element lands in the HTML namespace (an application/xhtml+xml document). namespaceURI is set as
  // given (the empty string and null both map to the null namespace).
  def(globalThis, "__createElementCasePreserving", function (namespaceURI, name) {
    var nm = String(name);
    if (!isValidNameImpl(nm, true)) { invalidCharacterError(); }
    var ns = (namespaceURI === undefined || namespaceURI === null || namespaceURI === "") ? null : String(namespaceURI);
    var id = __createElement(nm);
    __nsMeta[id] = {
      namespaceURI: ns, prefix: null, localName: nm, qualifiedName: nm, isHTML: ns === HTML_NS
    };
    return wrap(id);
  });
  // getElementsByTagNameNS(namespace, localName): live descendant collection matching namespace
  // (or "*") and localName (or "*").
  def(document, "getElementsByTagNameNS", function (namespace, localName) {
    var ns = namespace === "*" ? "*" : (namespace == null || namespace === "" ? null : String(namespace));
    var ln = (localName === "*" || localName == null) ? "*" : String(localName);
    return globalThis.__makeHTMLCollection(function () {
      return collectDescendants(0, function (eid) { return matchesTagNameNS(eid, ns, ln); });
    });
  });
  // Node-id-keyed attribute helpers the browser-env bootstrap uses for style/classList/dataset.
  def(document, "__getAttr", function (node, name) { return __getAttr(node, String(name)); });
  def(document, "__setAttr", function (node, name, value) { __setAttr(node, String(name), value == null ? "" : String(value)); });
  def(document, "__removeAttr", function (node, name) { __removeAttr(node, String(name)); });

  Object.defineProperty(document, "title", {
    get: function () { return __titleText(); },
    set: function (v) {
      var head = __headId();
      var t = -1;
      var all = __getElementsByTagName("title");
      if (all.length) { t = all[0]; }
      if (t < 0) {
        t = __createElement("title");
        var parent = head >= 0 ? head : __documentElementId();
        if (parent >= 0) { __appendChild(parent, t); }
      }
      if (t >= 0) { __setTextContent(t, v == null ? "" : String(v)); }
    },
    enumerable: true, configurable: true
  });
  // Document namespace lookup delegates to the document element (node id 0 is the Document root).
  def(document, "lookupNamespaceURI", function (prefix) { return nodeLookupNamespaceURI(0, prefix); });
  def(document, "lookupPrefix", function (ns) { return nodeLookupPrefix(0, ns); });
  def(document, "isDefaultNamespace", function (ns) { return nodeIsDefaultNamespace(0, ns); });
  // document.doctype: the first DocumentType child of the document root (node id 0), or null.
  Object.defineProperty(document, "doctype", {
    get: function () {
      var kids = __children(0);
      // Return the CANONICAL wrapper, not a fresh wrap(): otherwise document.doctype mints a new
      // object on every access, so it never === the doctype in document.childNodes and its
      // .parentNode is unstable. Identity-sensitive callers then loop forever (e.g. WPT common.js
      // `indexOf`: `while (node != node.parentNode.childNodes[i]) i++`).
      for (var i = 0; i < kids.length; i++) {
        if (__nodeType(kids[i]) === 10) {
          var dt = wrap(kids[i]);
          try { dt = globalThis.__canonNode(dt); } catch (e) {}
          return dt;
        }
      }
      return null;
    },
    enumerable: true, configurable: true
  });
  def(document, "createProcessingInstruction", function (target, data) {
    var t = String(target);
    if (!isValidNameImpl(t, true)) { invalidCharacterError(); }
    if (String(data).indexOf("?>") >= 0) {
      throw new globalThis.DOMException("The data must not contain '?>'.", "InvalidCharacterError");
    }
    var __pi = wrap(__createProcessingInstruction(t, String(data)));
    // Canonicalize (cache the wrapper) so navigation preserves node identity, and graft on methods.
    try { __pi = globalThis.__canonNode(__pi); } catch (e) {}
    try { globalThis.__addPartialMethods(__pi); } catch (e) {}
    return __pi;
  });
  def(document, "createDocumentType", function (qualifiedName, publicId, systemId) {
    return globalThis.__createDocumentTypeNode(String(qualifiedName),
      publicId == null ? "" : String(publicId), systemId == null ? "" : String(systemId));
  });
  Object.defineProperty(document, "body", { get: function () { var n = __bodyId(); return n >= 0 ? wrap(n) : null; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "documentElement", { get: function () { var n = __documentElementId(); return n >= 0 ? wrap(n) : null; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "head", { get: function () { var n = __headId(); return n >= 0 ? wrap(n) : null; }, enumerable: true, configurable: true });
  // Live child accessors over the arena document node (id 0). Canonicalize via __nodeFor so identity
  // checks (e.g. WPT common.js `indexOf`: `while (node != node.parentNode.childNodes[i]) i++`) hold.
  function __docKidFor(cid) { return (typeof globalThis.__nodeFor === "function") ? globalThis.__nodeFor(cid) : wrap(cid); }
  Object.defineProperty(document, "childNodes", { get: function () { var ids = __children(0), a = []; for (var i = 0; i < ids.length; i++) { a.push(__docKidFor(ids[i])); } return a; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "firstChild", { get: function () { var ids = __children(0); return ids.length ? __docKidFor(ids[0]) : null; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "lastChild", { get: function () { var ids = __children(0); return ids.length ? __docKidFor(ids[ids.length - 1]) : null; }, enumerable: true, configurable: true });
  // A Document has no parent, siblings, or owner document (all null, never undefined — WPT helpers
  // compare strictly against null, e.g. `if (node.parentNode === null)`).
  Object.defineProperty(document, "parentNode", { get: function () { return null; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "parentElement", { get: function () { return null; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "previousSibling", { get: function () { return null; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "nextSibling", { get: function () { return null; }, enumerable: true, configurable: true });
  Object.defineProperty(document, "ownerDocument", { get: function () { return null; }, enumerable: true, configurable: true });
  def(document, "nodeType", 9);
  // A Document's textContent / nodeValue are null (it's not CharacterData or an Element).
  Object.defineProperty(document, "textContent", { get: function () { return null; }, set: function () {}, enumerable: true, configurable: true });
  Object.defineProperty(document, "nodeValue", { get: function () { return null; }, set: function () {}, enumerable: true, configurable: true });

  // document.styleSheets: a StyleSheetList of the CSSStyleSheet objects for each <style> and
  // <link rel=stylesheet> element, in document order. Each entry is the element's own `.sheet`
  // (SameObject), so `styleSheets[i] === el.sheet`.
  // The current document.styleSheets entries: each <style>/<link rel=stylesheet>'s `.sheet`, in
  // tree order (excluding the adopted-sheets mirror and disabled links).
  function __collectDocSheets() {
    var els = document.querySelectorAll("style, link");
    var sheets = [];
    for (var i = 0; i < els.length; i++) {
      var el = els[i];
      var tag = (el.tagName || "").toLowerCase();
      if (el.getAttribute && el.getAttribute("data-adopted-stylesheets") != null) { continue; }
      if (tag === "link") {
        var rel = (el.getAttribute && el.getAttribute("rel") || "").toLowerCase();
        if (rel.split(/\s+/).indexOf("stylesheet") < 0) { continue; }
      }
      if (el.__sheetDisabled || (el.getAttribute && el.getAttribute("disabled") != null && tag === "link")) { continue; }
      try { var s = el.sheet; if (s) { sheets.push(s); } } catch (e) {}
    }
    return sheets;
  }
  // document.styleSheets is a LIVE StyleSheetList: a single object whose length / indexing / item /
  // iteration all re-read the DOM, so a captured reference reflects added/removed sheets (CSSOM).
  var __docSheetList = Object.create((globalThis.StyleSheetList && globalThis.StyleSheetList.prototype) || Object.prototype);
  Object.defineProperty(__docSheetList, "length", { get: function () { return __collectDocSheets().length; }, enumerable: false, configurable: true });
  __docSheetList.item = function (n) { var s = __collectDocSheets(); n = n >>> 0; return n < s.length ? s[n] : null; };
  try {
    __docSheetList[Symbol.iterator] = function () {
      var s = __collectDocSheets(), i = 0;
      var it = { next: function () { return i < s.length ? { value: s[i++], done: false } : { value: undefined, done: true }; } };
      it[Symbol.iterator] = function () { return this; };
      return it;
    };
  } catch (e) {}
  var __docSheetListProxy = new Proxy(__docSheetList, {
    get: function (t, p) {
      // Indexed getter: out-of-range returns `undefined` (WebIDL), unlike item() which returns null.
      if (typeof p === "string" && /^[0-9]+$/.test(p)) { var s = __collectDocSheets(); var n = Number(p); return n < s.length ? s[n] : undefined; }
      return t[p];
    },
    has: function (t, p) {
      if (typeof p === "string" && /^[0-9]+$/.test(p)) { return Number(p) < t.length; }
      return p in t;
    }
  });
  Object.defineProperty(document, "styleSheets", {
    get: function () { return __docSheetListProxy; },
    enumerable: true, configurable: true
  });

  globalThis.document = document;
})();
