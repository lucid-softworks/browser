// SVG DOM support: the SVG* IDL interface constructors, animated-attribute reflection
// (SVGAnimatedLength.baseVal/animVal tied to presentation attributes), and a SMIL animation
// engine that computes animVal at the document's current animation time.
//
// This bootstrap runs after <browser-env>. The per-element decoration in browser_env.js calls
// globalThis.__svgEnrich(el) for every element it wraps; here we act only on elements in the SVG
// namespace, layering on the length reflections, the animation timeline controls (on the <svg>
// root), and the animation-element APIs.
(function () {
  "use strict";

  var SVG_NS = "http://www.w3.org/2000/svg";
  var XLINK_NS = "http://www.w3.org/1999/xlink";

  var getAttr = globalThis.__getAttr;
  var setAttr = globalThis.__setAttr;
  function def(o, n, v) { try { Object.defineProperty(o, n, { value: v, writable: true, configurable: true, enumerable: false }); } catch (e) {} }

  // ---- The animation timeline (one per document; tests pause then setCurrentTime to sample). ----
  var clock = { time: 0, paused: false };
  function currentTime() { return clock.time; }
  globalThis.__svgClock = clock;

  // -------------------------------------------------------------------------------------------
  // SVG* interface constructors. Minimal but real prototypes so `instanceof` and the interface
  // constants work; instances are produced via Object.create(Ctor.prototype) by the factories below.
  // -------------------------------------------------------------------------------------------
  function ctor(name, statics) {
    var fn = globalThis[name];
    if (typeof fn !== "function") {
      fn = new Function("return function " + name + "(){}")();
      globalThis[name] = fn;
    }
    if (statics) { for (var k in statics) { if (statics.hasOwnProperty(k)) { fn[k] = statics[k]; fn.prototype[k] = statics[k]; } } }
    return fn;
  }

  var SVGLength = ctor("SVGLength", {
    SVG_LENGTHTYPE_UNKNOWN: 0, SVG_LENGTHTYPE_NUMBER: 1, SVG_LENGTHTYPE_PERCENTAGE: 2,
    SVG_LENGTHTYPE_EMS: 3, SVG_LENGTHTYPE_EXS: 4, SVG_LENGTHTYPE_PX: 5, SVG_LENGTHTYPE_CM: 6,
    SVG_LENGTHTYPE_MM: 7, SVG_LENGTHTYPE_IN: 8, SVG_LENGTHTYPE_PT: 9, SVG_LENGTHTYPE_PC: 10
  });
  var SVGAngle = ctor("SVGAngle", {
    SVG_ANGLETYPE_UNKNOWN: 0, SVG_ANGLETYPE_UNSPECIFIED: 1, SVG_ANGLETYPE_DEG: 2,
    SVG_ANGLETYPE_RAD: 3, SVG_ANGLETYPE_GRAD: 4
  });
  var SVGTransform = ctor("SVGTransform", {
    SVG_TRANSFORM_UNKNOWN: 0, SVG_TRANSFORM_MATRIX: 1, SVG_TRANSFORM_TRANSLATE: 2,
    SVG_TRANSFORM_SCALE: 3, SVG_TRANSFORM_ROTATE: 4, SVG_TRANSFORM_SKEWX: 5, SVG_TRANSFORM_SKEWY: 6
  });
  var SVGPreserveAspectRatio = ctor("SVGPreserveAspectRatio", {
    SVG_PRESERVEASPECTRATIO_UNKNOWN: 0, SVG_PRESERVEASPECTRATIO_NONE: 1,
    SVG_PRESERVEASPECTRATIO_XMINYMIN: 2, SVG_PRESERVEASPECTRATIO_XMIDYMIN: 3,
    SVG_PRESERVEASPECTRATIO_XMAXYMIN: 4, SVG_PRESERVEASPECTRATIO_XMINYMID: 5,
    SVG_PRESERVEASPECTRATIO_XMIDYMID: 6, SVG_PRESERVEASPECTRATIO_XMAXYMID: 7,
    SVG_PRESERVEASPECTRATIO_XMINYMAX: 8, SVG_PRESERVEASPECTRATIO_XMIDYMAX: 9,
    SVG_PRESERVEASPECTRATIO_XMAXYMAX: 10, SVG_MEETORSLICE_UNKNOWN: 0,
    SVG_MEETORSLICE_MEET: 1, SVG_MEETORSLICE_SLICE: 2
  });
  ctor("SVGNumber"); ctor("SVGRect"); ctor("SVGPoint"); ctor("SVGMatrix");
  ctor("SVGTransformList"); ctor("SVGPointList"); ctor("SVGLengthList"); ctor("SVGNumberList");
  ctor("SVGStringList");
  ctor("SVGAnimatedLength"); ctor("SVGAnimatedLengthList"); ctor("SVGAnimatedNumber");
  ctor("SVGAnimatedNumberList"); ctor("SVGAnimatedInteger"); ctor("SVGAnimatedEnumeration");
  ctor("SVGAnimatedBoolean"); ctor("SVGAnimatedString"); ctor("SVGAnimatedRect");
  ctor("SVGAnimatedAngle"); ctor("SVGAnimatedPreserveAspectRatio");
  ctor("SVGAnimatedTransformList");
  var SVGUnitTypes = ctor("SVGUnitTypes", {
    SVG_UNIT_TYPE_UNKNOWN: 0, SVG_UNIT_TYPE_USERSPACEONUSE: 1, SVG_UNIT_TYPE_OBJECTBOUNDINGBOX: 2
  });
  globalThis.SVGUnitTypes = SVGUnitTypes;

  // -------------------------------------------------------------------------------------------
  // Length parsing. Returns the value in user units (px), the specified number, and the unit type.
  // -------------------------------------------------------------------------------------------
  var UNIT_TYPE = { "": 1, "px": 5, "%": 2, "em": 3, "ex": 4, "cm": 6, "mm": 7, "in": 8, "pt": 9, "pc": 10 };
  function unitToPx(n, u) {
    switch (u) {
      case "pt": return n * 96 / 72;
      case "pc": return n * 16;
      case "cm": return n * 96 / 2.54;
      case "mm": return n * 96 / 25.4;
      case "in": return n * 96;
      default: return n; // px, unitless, %, em, ex (approx: caller rarely reads % in user units)
    }
  }
  function parseLen(s) {
    if (s == null) { return { value: 0, num: 0, unit: "", type: 1, str: "" }; }
    s = String(s).trim();
    var m = /^([+-]?(?:[0-9]*\.[0-9]+|[0-9]+\.?)(?:[eE][+-]?[0-9]+)?)\s*(px|pt|pc|cm|mm|in|em|ex|%)?$/.exec(s);
    if (!m) { return { value: 0, num: 0, unit: "", type: 0, str: s }; }
    var n = parseFloat(m[1]); var u = m[2] || "";
    return { value: unitToPx(n, u), num: n, unit: u, type: UNIT_TYPE[u] || 0, str: s };
  }
  function num(v) { v = parseFloat(v); return isFinite(v) ? v : 0; }

  // -------------------------------------------------------------------------------------------
  // SMIL value computation.
  // -------------------------------------------------------------------------------------------
  // Parse a SMIL clock-value (e.g. "4s", "1.5s", "250ms", "0:0:4", "indefinite", "2") to seconds.
  function parseClock(s) {
    if (s == null) { return null; }
    s = String(s).trim();
    if (s === "" ) { return null; }
    if (s === "indefinite") { return Infinity; }
    var m;
    if ((m = /^([0-9]+):([0-9]{2}):([0-9]{2}(?:\.[0-9]+)?)$/.exec(s))) { return (+m[1]) * 3600 + (+m[2]) * 60 + (+m[3]); }
    if ((m = /^([0-9]+):([0-9]{2}(?:\.[0-9]+)?)$/.exec(s))) { return (+m[1]) * 60 + (+m[2]); }
    if ((m = /^([0-9]*\.?[0-9]+)(h|min|s|ms)?$/.exec(s))) {
      var v = parseFloat(m[1]);
      switch (m[2]) { case "h": return v * 3600; case "min": return v * 60; case "ms": return v / 1000; default: return v; }
    }
    return null;
  }
  // The first numeric offset of a begin/end list (we don't model event/syncbase timing yet).
  function parseBegin(s) {
    if (s == null || s === "") { return 0; }
    var first = String(s).split(";")[0].trim();
    var c = parseClock(first);
    return c == null ? 0 : c;
  }
  function splitList(s) {
    if (s == null) { return []; }
    return String(s).split(";").map(function (x) { return x.trim(); }).filter(function (x) { return x.length > 0; });
  }

  // cubic-bezier(x1,y1,x2,y2) easing: given x in [0,1], solve for parameter then return y.
  function bezierEase(x, x1, y1, x2, y2) {
    function curve(t, a, b) { var c = 1 - t; return 3 * c * c * t * a + 3 * c * t * t * b + t * t * t; }
    if (x <= 0) { return 0; } if (x >= 1) { return 1; }
    var lo = 0, hi = 1, t = x;
    for (var i = 0; i < 40; i++) {
      var xc = curve(t, x1, x2);
      if (Math.abs(xc - x) < 1e-6) { break; }
      if (xc < x) { lo = t; } else { hi = t; }
      t = (lo + hi) / 2;
    }
    return curve(t, y1, y2);
  }

  // Interpolate a numeric animation function at simple-duration fraction `f` in [0,1].
  function simpleValue(f, values, calcMode, keyTimes, keySplines) {
    var n = values.length;
    if (n === 1) { return values[0]; }
    if (calcMode === "discrete") {
      var idx;
      if (keyTimes && keyTimes.length === n) {
        idx = 0;
        for (var i = 0; i < n; i++) { if (keyTimes[i] <= f) { idx = i; } else { break; } }
      } else {
        idx = Math.min(Math.floor(f * n), n - 1);
      }
      return values[idx];
    }
    // linear / paced / spline: locate the segment.
    var times = keyTimes;
    if (!times || times.length !== n) {
      if (calcMode === "paced") {
        // Distribute by cumulative absolute distance between values.
        var dist = [0]; var total = 0;
        for (var k = 1; k < n; k++) { total += Math.abs(values[k] - values[k - 1]); dist.push(total); }
        times = dist.map(function (d) { return total === 0 ? 0 : d / total; });
      } else {
        times = []; for (var j = 0; j < n; j++) { times.push(j / (n - 1)); }
      }
    }
    var seg = n - 2;
    for (var s = 0; s < n - 1; s++) { if (f >= times[s] && f <= times[s + 1]) { seg = s; break; } if (f < times[s]) { seg = Math.max(0, s - 1); break; } }
    var span = times[seg + 1] - times[seg];
    var p = span > 0 ? (f - times[seg]) / span : 0;
    if (p < 0) { p = 0; } if (p > 1) { p = 1; }
    if (calcMode === "spline" && keySplines && keySplines[seg]) {
      var ks = keySplines[seg];
      p = bezierEase(p, ks[0], ks[1], ks[2], ks[3]);
    }
    return values[seg] + p * (values[seg + 1] - values[seg]);
  }

  // Compute one animation element's contribution to a scalar attribute at time t.
  // Returns {value, additive} or null when the animation has no effect at t.
  function animContribution(a, t, baseNum) {
    var ga = function (n) { var v = getAttr(a.__node, n); return v == null ? null : v; };
    var begin = parseBegin(ga("begin"));
    var durRaw = ga("dur");
    var dur = parseClock(durRaw);
    if (dur == null || dur <= 0) { dur = Infinity; }
    var repeatCount = ga("repeatCount");
    var reps = repeatCount === "indefinite" ? Infinity : (repeatCount != null ? num(repeatCount) : 1);
    if (!(reps > 0)) { reps = 1; }
    var activeDur = dur === Infinity ? Infinity : dur * reps;
    var repeatDur = parseClock(ga("repeatDur"));
    if (repeatDur != null && repeatDur < activeDur) { activeDur = repeatDur; }
    var fill = (ga("fill") || "remove");

    var local = t - begin;
    var simpleDur = dur === Infinity ? activeDur : dur;
    var iteration, fraction;
    if (local < 0) { return null; }
    if (activeDur !== Infinity && local >= activeDur) {
      if (fill !== "freeze") { return null; }
      iteration = simpleDur === Infinity ? 0 : Math.floor(activeDur / simpleDur);
      if (simpleDur !== Infinity && Math.abs(iteration * simpleDur - activeDur) < 1e-9 && iteration > 0) { iteration -= 1; }
      fraction = 1;
    } else {
      iteration = simpleDur === Infinity ? 0 : Math.floor(local / simpleDur);
      fraction = simpleDur === Infinity ? 0 : (local - iteration * simpleDur) / simpleDur;
    }

    // Build the values list and additivity from from/to/by/values.
    var calcMode = ga("calcMode");
    if (a.__localName === "set") { calcMode = "discrete"; }
    if (!calcMode) { calcMode = "linear"; }
    var additive = ga("additive") === "sum";
    var accumulate = ga("accumulate") === "sum";

    var values, keyTimes = null, keySplines = null;
    var kt = ga("keyTimes");
    if (kt != null) { keyTimes = splitList(kt).map(num); }
    var ks = ga("keySplines");
    if (ks != null) {
      keySplines = String(ks).split(";").map(function (g) { return g.trim(); }).filter(function (g) { return g.length; })
        .map(function (g) { return g.split(/[\s,]+/).map(num); });
    }

    var vAttr = ga("values");
    var from = ga("from"), to = ga("to"), by = ga("by");
    if (a.__localName === "set") {
      values = [num(to != null ? to : (vAttr != null ? vAttr : 0))];
    } else if (vAttr != null) {
      values = splitList(vAttr).map(num);
      if (values.length === 0) { return null; }
    } else if (from != null && to != null) {
      values = [parseLen(from).value, parseLen(to).value];
    } else if (from != null && by != null) {
      values = [parseLen(from).value, parseLen(from).value + parseLen(by).value];
    } else if (by != null) {
      values = [0, parseLen(by).value]; additive = true; // pure by-animation is additive
    } else if (to != null) {
      values = [baseNum, parseLen(to).value]; // to-animation: starts from the underlying value
    } else if (from != null) {
      values = [parseLen(from).value];
    } else {
      return null;
    }

    var v = simpleValue(fraction, values, calcMode, keyTimes, keySplines);
    if (accumulate && iteration > 0 && values.length > 0) {
      v += iteration * values[values.length - 1];
    }
    return { value: v, additive: additive };
  }

  function isAnimEl(el) {
    var ln = el && el.__localName;
    return ln === "animate" || ln === "set" || ln === "animateColor" || ln === "animateTransform" || ln === "animateMotion";
  }

  // Collect the animation elements (document order) that target `el`'s attribute `attr`.
  function collectAnimations(el, attr) {
    var out = [];
    var kids = el.childNodes;
    if (kids) {
      for (var i = 0; i < kids.length; i++) {
        var c = kids[i];
        if (c && c.nodeType === 1 && isAnimEl(c) && getAttr(c.__node, "attributeName") === attr && animTargets(c, el)) {
          out.push(c);
        }
      }
    }
    return out;
  }
  function animTargets(a, el) {
    var href = getAttr(a.__node, "href");
    if (href == null) { href = getAttr(a.__node, "xlink:href"); }
    if (href == null || href === "") { return true; } // targets its parent
    return ("#" + (getAttr(el.__node, "id") || "")) === href;
  }

  // The animated numeric value of `attr` on `el`, given its base (attribute) value.
  function svgAnimNum(el, attr, baseNum) {
    var anims = collectAnimations(el, attr);
    if (!anims.length) { return baseNum; }
    var t = currentTime();
    var result = baseNum; var any = false;
    for (var i = 0; i < anims.length; i++) {
      var c = animContribution(anims[i], t, baseNum);
      if (c == null) { continue; }
      any = true;
      result = c.additive ? result + c.value : c.value;
    }
    return any ? result : baseNum;
  }
  globalThis.__svgAnimNum = svgAnimNum;

  // -------------------------------------------------------------------------------------------
  // SVGLength factory (live, backed by an attribute) and SVGAnimatedLength.
  // -------------------------------------------------------------------------------------------
  function makeLength(getNum, getStr, getType, setNum) {
    var L = Object.create(SVGLength.prototype);
    Object.defineProperty(L, "value", {
      get: getNum,
      set: setNum || function () { throw new globalThis.DOMException("read-only", "NoModificationAllowedError"); },
      enumerable: true, configurable: true
    });
    Object.defineProperty(L, "valueInSpecifiedUnits", { get: function () { return parseLen(getStr()).num; }, enumerable: true, configurable: true });
    Object.defineProperty(L, "valueAsString", {
      get: function () { var s = getStr(); return s == null || s === "" ? "0" : s; },
      set: setNum ? function (v) { setStrRaw(v); } : undefined, enumerable: true, configurable: true
    });
    Object.defineProperty(L, "unitType", { get: getType, enumerable: true, configurable: true });
    var setStrRaw = setNum;
    def(L, "newValueSpecifiedUnits", function (unit, v) { if (setNum) { setNum(v); } });
    def(L, "convertToSpecifiedUnits", function () {});
    return L;
  }

  function makeAnimatedLength(el, attr) {
    var node = el.__node;
    var anim = Object.create(SVGAnimatedLength.prototype);
    var baseVal = makeLength(
      function () { return parseLen(getAttr(node, attr)).value; },
      function () { return getAttr(node, attr); },
      function () { return parseLen(getAttr(node, attr)).type; },
      function (v) { setAttr(node, attr, String(v)); }
    );
    var animVal = makeLength(
      function () { return svgAnimNum(el, attr, parseLen(getAttr(node, attr)).value); },
      function () { return getAttr(node, attr); },
      function () { return parseLen(getAttr(node, attr)).type; },
      null
    );
    Object.defineProperty(anim, "baseVal", { value: baseVal, enumerable: true });
    Object.defineProperty(anim, "animVal", { value: animVal, enumerable: true });
    return anim;
  }

  // Per-tag scalar length-valued attributes (each exposed as an SVGAnimatedLength property).
  var LEN_ATTRS = {
    rect: ["x", "y", "width", "height", "rx", "ry"],
    circle: ["cx", "cy", "r"],
    ellipse: ["cx", "cy", "rx", "ry"],
    line: ["x1", "y1", "x2", "y2"],
    image: ["x", "y", "width", "height"],
    use: ["x", "y", "width", "height"],
    svg: ["x", "y", "width", "height"],
    foreignobject: ["x", "y", "width", "height"],
    pattern: ["x", "y", "width", "height"],
    mask: ["x", "y", "width", "height"],
    filter: ["x", "y", "width", "height"]
  };

  // -------------------------------------------------------------------------------------------
  // Per-element decoration entry point (called by browser_env's enrichElement).
  // -------------------------------------------------------------------------------------------
  function svgEnrich(el) {
    if (!el || el.namespaceURI !== SVG_NS) { return; }
    var ln = "";
    try { ln = (el.localName || el.tagName || "").toLowerCase(); } catch (e) {}
    def(el, "__localName", ln);

    // Scalar length attributes -> SVGAnimatedLength (cached per element+attr).
    var attrs = LEN_ATTRS[ln];
    if (attrs) {
      var cache = {};
      def(el, "__svgLenCache", cache);
      for (var i = 0; i < attrs.length; i++) {
        (function (a) {
          Object.defineProperty(el, a, {
            get: function () { return cache[a] || (cache[a] = makeAnimatedLength(el, a)); },
            configurable: true, enumerable: true
          });
        })(attrs[i]);
      }
    }

    // The <svg> root: the animation timeline controls + create* factories.
    if (ln === "svg") {
      def(el, "pauseAnimations", function () { clock.paused = true; });
      def(el, "unpauseAnimations", function () { clock.paused = false; });
      def(el, "setCurrentTime", function (s) { var v = Number(s); clock.time = isFinite(v) ? v : 0; });
      def(el, "getCurrentTime", function () { return clock.time; });
      def(el, "suspendRedraw", function () { return 0; });
      def(el, "unsuspendRedraw", function () {});
      def(el, "unsuspendRedrawAll", function () {});
      def(el, "forceRedraw", function () {});
      def(el, "createSVGLength", function () { var L = Object.create(SVGLength.prototype); L.value = 0; L.valueInSpecifiedUnits = 0; L.valueAsString = "0"; L.unitType = 1; def(L, "newValueSpecifiedUnits", function (u, v) { this.value = v; this.valueInSpecifiedUnits = v; }); def(L, "convertToSpecifiedUnits", function () {}); return L; });
      def(el, "createSVGNumber", function () { var N = Object.create(globalThis.SVGNumber.prototype); N.value = 0; return N; });
      def(el, "createSVGPoint", function () { var P = Object.create(globalThis.SVGPoint.prototype); P.x = 0; P.y = 0; def(P, "matrixTransform", function (m) { return P; }); return P; });
      def(el, "createSVGRect", function () { var R = Object.create(globalThis.SVGRect.prototype); R.x = 0; R.y = 0; R.width = 0; R.height = 0; return R; });
      def(el, "createSVGMatrix", function () { return makeMatrix(1, 0, 0, 1, 0, 0); });
      def(el, "createSVGTransform", function () { var T = Object.create(SVGTransform.prototype); T.type = 0; T.angle = 0; T.matrix = makeMatrix(1, 0, 0, 1, 0, 0); def(T, "setMatrix", function (m) { this.matrix = m; this.type = 1; }); def(T, "setTranslate", function (x, y) { this.matrix = makeMatrix(1, 0, 0, 1, x, y); this.type = 2; }); def(T, "setScale", function (sx, sy) { this.matrix = makeMatrix(sx, 0, 0, sy, 0, 0); this.type = 3; }); def(T, "setRotate", function (ang) { this.angle = ang; this.type = 4; }); return T; });
      def(el, "createSVGAngle", function () { var A = Object.create(SVGAngle.prototype); A.value = 0; A.unitType = 1; return A; });
      def(el, "createSVGTransformFromMatrix", function (m) { var T = el.createSVGTransform(); T.setMatrix(m); return T; });
      def(el, "getElementById", function (id) { return el.ownerDocument.getElementById(id); });
    }

    // Animation elements: the timeline-query API used by some tests.
    if (isAnimEl(el)) {
      if (typeof el.getStartTime !== "function") {
        def(el, "getStartTime", function () { return parseBegin(getAttr(el.__node, "begin")); });
      }
      if (typeof el.getCurrentTime !== "function") { def(el, "getCurrentTime", function () { return clock.time; }); }
      if (typeof el.getSimpleDuration !== "function") {
        def(el, "getSimpleDuration", function () { var d = parseClock(getAttr(el.__node, "dur")); if (d == null) { throw new globalThis.DOMException("no simple duration", "NotSupportedError"); } return d; });
      }
      Object.defineProperty(el, "targetElement", {
        get: function () {
          var href = getAttr(el.__node, "href"); if (href == null) { href = getAttr(el.__node, "xlink:href"); }
          if (href && href.charAt(0) === "#") { return el.ownerDocument.getElementById(href.slice(1)); }
          return el.parentNode && el.parentNode.nodeType === 1 ? el.parentNode : null;
        }, configurable: true, enumerable: true
      });
    }
  }

  function makeMatrix(a, b, c, d, e, f) {
    var M = Object.create(globalThis.SVGMatrix.prototype);
    M.a = a; M.b = b; M.c = c; M.d = d; M.e = e; M.f = f;
    def(M, "multiply", function (o) { return makeMatrix(M.a * o.a + M.c * o.b, M.b * o.a + M.d * o.b, M.a * o.c + M.c * o.d, M.b * o.c + M.d * o.d, M.a * o.e + M.c * o.f + M.e, M.b * o.e + M.d * o.f + M.f); });
    def(M, "translate", function (x, y) { return M.multiply(makeMatrix(1, 0, 0, 1, x, y)); });
    def(M, "scale", function (s) { return M.multiply(makeMatrix(s, 0, 0, s, 0, 0)); });
    def(M, "inverse", function () { var det = M.a * M.d - M.b * M.c; if (!det) { throw new globalThis.DOMException("not invertible", "InvalidStateError"); } return makeMatrix(M.d / det, -M.b / det, -M.c / det, M.a / det, (M.c * M.f - M.d * M.e) / det, (M.b * M.e - M.a * M.f) / det); });
    return M;
  }
  globalThis.__svgMakeMatrix = makeMatrix;

  globalThis.__svgEnrich = svgEnrich;
})();
