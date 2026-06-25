// Worker overlay: turn a freshly browser-env'd V8 context into a DedicatedWorkerGlobalScope, then
// fetch + run the worker's top-level script. Runs (once) in the worker's OWN context, where
// `self === globalThis` already holds (browser_env aliases them), so top-level declarations in the
// worker script and every importScripts'd file become real worker-global properties — exactly the
// semantics canvas-tests.js et al. rely on. The worker id and script URL were seeded as globals
// (__workerId / __workerScriptURL) by the native that created this context.
// testharness selects its environment with `'document' in self` FIRST (→ Window), only then
// `self instanceof DedicatedWorkerGlobalScope`. Our worker context reuses the full browser env, so
// `document`/`window` exist as own globals. We delete them as OWN properties (so `'document' in
// self` is false and the worker environment is chosen) but rebind them as global LEXICAL bindings —
// `let` at top-level script scope is NOT a property of globalThis, yet browser_env's ~293 bare
// `document` references still resolve to it through the scope chain. This runs at top level (NOT
// inside the IIFE below) so the `let`s land in the global lexical environment.
var __wDocStub = globalThis.document;
var __wWinStub = globalThis;
try { delete globalThis.document; } catch (e) {}
try { delete globalThis.window; } catch (e) {}
// `webkitURL` is a legacy *Window* alias for URL; it must not exist in a worker scope.
try { delete globalThis.webkitURL; } catch (e) {}
let document = __wDocStub;
let window = __wWinStub;

(function () {
  "use strict";
  var g = globalThis;
  var id = g.__workerId;
  var href = String(g.__workerScriptURL || "");

  // --- DedicatedWorkerGlobalScope identity --------------------------------------------------
  // testharness picks the worker test environment via
  //   'DedicatedWorkerGlobalScope' in self && self instanceof DedicatedWorkerGlobalScope
  // We keep the real context global (a fully featured object with all its methods) and make
  // `instanceof` recognise it via Symbol.hasInstance — far safer than reparenting the global.
  function defScopeClass(name) {
    var C = function () {};
    try { Object.defineProperty(C, Symbol.hasInstance, { value: function (x) { return x === g; } }); } catch (e) {}
    try { Object.defineProperty(g, name, { value: C, writable: true, configurable: true, enumerable: false }); } catch (e) {}
    return C;
  }
  defScopeClass("WorkerGlobalScope");
  defScopeClass("DedicatedWorkerGlobalScope");
  g.self = g;

  // WorkerLocation: the script URL.
  try { Object.defineProperty(g, "location", { value: new g.URL(href), writable: true, configurable: true }); } catch (e) {}
  if (typeof g.name !== "string") { try { g.name = ""; } catch (e) {} }
  g.onmessage = null; g.onmessageerror = null; g.onerror = null;

  var closed = false;

  // worker -> parent: hand the value to the native bridge, which re-enters the page context.
  g.postMessage = function (data, transfer) {
    if (closed) { return; }
    if (typeof g.__workerPostToParent === "function") { g.__workerPostToParent(id, data); }
  };
  g.close = function () { closed = true; };

  // parent -> worker: invoked from the native bridge with the parent's value. Localise via our own
  // structuredClone, then deliver a `message` event on a fresh task (so listeners attached right
  // after `new Worker()` aren't missed).
  g.__workerAccept = function (data) {
    if (closed) { return; }
    var cloned; try { cloned = g.structuredClone(data); } catch (e) { cloned = data; }
    setTimeout(function () {
      if (closed) { return; }
      var ev;
      try { ev = new g.MessageEvent("message", { data: cloned, origin: "", lastEventId: "", source: null, ports: [] }); }
      catch (e2) { ev = { type: "message", data: cloned }; }
      try { g.dispatchEvent(ev); } catch (e3) {}
    }, 0);
  };

  // importScripts: fetch synchronously and run each as a TOP-LEVEL script (so declarations land on
  // the worker global and are visible to later scripts — matching real worker semantics).
  g.importScripts = function () {
    for (var i = 0; i < arguments.length; i++) {
      var su = (new g.URL(String(arguments[i]), href)).href;
      var env = g.__request("GET", su, "", "{}");
      if (!env) { throw new g.DOMException("Failed to execute 'importScripts' on 'WorkerGlobalScope': could not load " + su, "NetworkError"); }
      var p; try { p = JSON.parse(env); } catch (e) { throw new g.DOMException("Failed to execute 'importScripts' on 'WorkerGlobalScope': bad response for " + su, "NetworkError"); }
      if (!p.ok) { throw new g.DOMException("Failed to execute 'importScripts' on 'WorkerGlobalScope': HTTP " + p.status + " for " + su, "NetworkError"); }
      g.__runWorkerScript(p.body || "", su);
    }
  };

  // Run the top-level worker script now. Its declarations land on the worker global. An inline
  // source (decoded data:/blob: worker, seeded by the native) is used directly; otherwise fetch it.
  try {
    if (typeof g.__workerInlineSource === "string") {
      g.__runWorkerScript(g.__workerInlineSource, href);
    } else {
      var topEnv = g.__request("GET", href, "", "{}");
      if (topEnv) {
        var tp = JSON.parse(topEnv);
        if (tp && tp.ok) { g.__runWorkerScript(tp.body || "", href); }
      }
    }
  } catch (e) {}
})();
