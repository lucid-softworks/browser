// Iframe overlay: runs once in a frame's OWN context after the browser env is installed. The frame
// already has its own window/document/self/location/performance from browser_env over its parsed
// document; this wires the cross-frame bridge (parent/top messaging, page->frame delivery). The host
// <iframe> element's node id was seeded as __frameNodeId by the native that created this context.
(function () {
  "use strict";
  var g = globalThis;
  var nodeId = g.__frameNodeId;
  var isAux = !!g.__frameIsAuxWindow;

  // A messaging facade onto the page (the opener for a window.open target, or the parent for an
  // iframe). A real browser exposes the other Window object; cross-realm property access is limited
  // here to postMessage, which is what cross-context tests use.
  var pageRef = {
    postMessage: function (data, targetOrigin, transfer) { g.__framePostToParent(nodeId, data); },
    closed: false
  };

  if (isAux) {
    // A window.open() target is its own top-level browsing context: parent/top are itself, there is
    // no frameElement, and `opener` is the page that opened it.
    Object.defineProperty(g, "parent", { value: g, writable: true, configurable: true });
    Object.defineProperty(g, "top", { value: g, writable: true, configurable: true });
    Object.defineProperty(g, "frameElement", { value: null, writable: true, configurable: true });
    Object.defineProperty(g, "opener", { value: pageRef, writable: true, configurable: true });
  } else {
    // parent / top: the messaging facade onto the page context.
    Object.defineProperty(g, "parent", { value: pageRef, writable: true, configurable: true });
    Object.defineProperty(g, "top", { value: pageRef, writable: true, configurable: true });
    Object.defineProperty(g, "frameElement", { value: null, writable: true, configurable: true });
  }

  // page -> frame: the native bridge calls this with the page's value; localise with the frame's
  // own structuredClone, then deliver a `message` event on a fresh task. `source` is the page facade
  // (opener/parent) so handlers that reply via `event.source.postMessage(...)` reach the page.
  g.__frameAccept = function (data) {
    var cloned = g.structuredClone(data);
    setTimeout(function () {
      g.dispatchEvent(new g.MessageEvent("message", { data: cloned, origin: "", lastEventId: "", source: pageRef, ports: [] }));
    }, 0);
  };
})();
