// Iframe overlay: runs once in a frame's OWN context after the browser env is installed. The frame
// already has its own window/document/self/location/performance from browser_env over its parsed
// document; this wires the cross-frame bridge (parent/top messaging, page->frame delivery). The host
// <iframe> element's node id was seeded as __frameNodeId by the native that created this context.
(function () {
  "use strict";
  var g = globalThis;
  var nodeId = g.__frameNodeId;

  // parent / top: a messaging facade onto the page context. (A real browser exposes the parent
  // Window object; cross-realm property access is limited here to postMessage, which is what
  // cross-frame tests use.)
  var parentRef = {
    postMessage: function (data, targetOrigin, transfer) {
      if (typeof g.__framePostToParent === "function") { g.__framePostToParent(nodeId, data); }
    },
    closed: false
  };
  try { Object.defineProperty(g, "parent", { value: parentRef, writable: true, configurable: true }); } catch (e) {}
  try { Object.defineProperty(g, "top", { value: parentRef, writable: true, configurable: true }); } catch (e) {}
  try { Object.defineProperty(g, "frameElement", { value: null, writable: true, configurable: true }); } catch (e) {}

  // page -> frame: the native bridge calls this with the parent's value; localise with the frame's
  // own structuredClone, then deliver a `message` event on a fresh task.
  g.__frameAccept = function (data) {
    var cloned; try { cloned = g.structuredClone(data); } catch (e) { cloned = data; }
    setTimeout(function () {
      var ev;
      try { ev = new g.MessageEvent("message", { data: cloned, origin: "", lastEventId: "", source: parentRef, ports: [] }); }
      catch (e2) { ev = { type: "message", data: cloned }; }
      try { g.dispatchEvent(ev); } catch (e3) {}
    }, 0);
  };
})();
