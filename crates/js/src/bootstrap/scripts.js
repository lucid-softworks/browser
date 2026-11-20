// Dynamically connected classic scripts. Static scripts are executed by the host;
// scripts created by innerHTML remain inert, including after being moved.
(function () {
  var started = Object.create(null);
  var written = Object.create(null);
  var current = null;
  var insertionPoint = null;
  var parserScripts = [], parserIndex = 0;
  // The host parses ahead before evaluating classic scripts. Keep later script
  // elements out of DOM collections until the parser reaches them, so loaders
  // looking up their own <script> don't select a later, not-yet-executed one.
  globalThis.__scriptPrepareParser = function () {
    walk(document.__node, function (id) {
      var parent = __parent(id), siblings = __children(parent);
      parserScripts.push({id: id, parent: parent, following: siblings.slice(siblings.indexOf(id) + 1)});
    });
    parserScripts.forEach(function (s) { __removeChild(s.parent, s.id); });
  };
  function restoreThrough(id) {
    var end = parserScripts.length;
    if (id != null) {
      end = parserScripts.findIndex(function (s) { return s.id === id; }) + 1;
    }
    while (parserIndex < end) {
      var s = parserScripts[parserIndex++], before = -1;
      for (var i = 0; i < s.following.length; i++) {
        if (__parent(s.following[i]) === s.parent) { before = s.following[i]; break; }
      }
      __insertBefore(s.parent, s.id, before);
    }
  }
  globalThis.__scriptFinishParsing = function () { restoreThrough(null); parserScripts = []; parserIndex = 0; };
  globalThis.__scriptSetCurrent = function (id) {
    if (id != null) { restoreThrough(id); }
    current = id == null ? null : globalThis.__canonNode(__wrapNode(id));
    insertionPoint = current && current.parentNode
      ? { parent: current.parentNode, before: current.nextSibling } : null;
  };
  globalThis.__scriptInsertionPoint = function () { return insertionPoint; };
  Object.defineProperty(document, 'currentScript', {
    get: function () { return current; }, configurable: true
  });
  function walk(id, visit) {
    if (__tag(id) === 'script') { visit(id); }
    var children = __children(id);
    for (var i = 0; i < children.length; i++) { walk(children[i], visit); }
  }
  globalThis.__scriptMarkInert = function (id) {
    walk(id, function (script) { started[script] = true; });
  };
  globalThis.__scriptAllowWritten = function (id) {
    walk(id, function (script) { delete started[script]; written[script] = true; });
  };
  globalThis.__scriptCloneState = function (source, clone) {
    if (started[source]) { started[clone] = true; }
    var a = __children(source), b = __children(clone);
    for (var i = 0; i < a.length && i < b.length; i++) {
      globalThis.__scriptCloneState(a[i], b[i]);
    }
  };
  globalThis.__scriptOnInsert = function (id) {
    var node = globalThis.__canonNode(__wrapNode(id));
    if (!document.documentElement.contains(node)) { return; }
    walk(id, function (scriptId) {
      if (started[scriptId]) { return; }
      var el = globalThis.__canonNode(__wrapNode(scriptId));
      var type = String(el.getAttribute('type') || '').trim().toLowerCase();
      if (type && type !== 'text/javascript' && type !== 'application/javascript') { return; }
      var src = el.getAttribute('src');
      var inline = el.textContent;
      if (!src && !inline) { return; }
      started[scriptId] = true;
      var url = src ? new URL(src, document.baseURI).href : null;
      function run() {
        var previous = current;
        var previousPoint = insertionPoint;
        try {
          var source = url ? __fetch(url) : inline;
          if (source == null) { el.dispatchEvent(new Event('error')); return; }
          globalThis.__scriptSetCurrent(scriptId);
          try { (0, eval)(source); }
          catch (error) { console.error(error && error.stack || error); }
          current = previous;
          insertionPoint = previousPoint;
          if (url) { el.dispatchEvent(new Event('load')); }
        } finally { current = previous; insertionPoint = previousPoint; }
      }
      // document.write participates in the parser's blocking script order.
      // Ordinary dynamically inserted classic scripts load asynchronously.
      if (url && !written[scriptId]) { setTimeout(run, 0); } else { run(); }
    });
  };
  globalThis.__scriptMarkInert(document.__node);
})();
