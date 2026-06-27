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
  // SVG element interface hierarchy. browser-env already defines SVGElement / SVGGraphicsElement /
  // SVGSVGElement; we extend it with the per-element interfaces so `instanceof SVGRectElement` etc.
  // work. Each interface's prototype chains to its parent's prototype.
  function subClass(name, parentName) {
    var parent = globalThis[parentName];
    var fn = globalThis[name];
    if (typeof fn !== "function") { fn = new Function("return function " + name + "(){}")(); globalThis[name] = fn; }
    if (parent && parent.prototype && Object.getPrototypeOf(fn.prototype) !== parent.prototype) {
      Object.setPrototypeOf(fn.prototype, parent.prototype);
      Object.setPrototypeOf(fn, parent);
    }
    return fn;
  }
  // Ensure the browser-env base interfaces chain correctly (SVGSVGElement must reach
  // SVGGraphicsElement so the root <svg> inherits getBBox/transform/SVGTests members).
  subClass("SVGElement", "Element");
  subClass("SVGGraphicsElement", "SVGElement");
  subClass("SVGSVGElement", "SVGGraphicsElement");
  // SVG2 aliases the geometry value types to the CSSOM DOM types; chain our implementations so that
  // `viewBox.baseVal instanceof DOMRect`, `createSVGMatrix() instanceof DOMMatrix`, etc. hold.
  subClass("DOMRect", "DOMRectReadOnly");
  subClass("DOMPoint", "DOMPointReadOnly");
  subClass("DOMMatrix", "DOMMatrixReadOnly");
  subClass("SVGRect", "DOMRect");
  subClass("SVGPoint", "DOMPoint");
  subClass("SVGMatrix", "DOMMatrix");
  subClass("SVGGeometryElement", "SVGGraphicsElement");
  subClass("SVGPathElement", "SVGGeometryElement");
  subClass("SVGRectElement", "SVGGeometryElement");
  subClass("SVGCircleElement", "SVGGeometryElement");
  subClass("SVGEllipseElement", "SVGGeometryElement");
  subClass("SVGLineElement", "SVGGeometryElement");
  subClass("SVGPolylineElement", "SVGGeometryElement");
  subClass("SVGPolygonElement", "SVGGeometryElement");
  subClass("SVGGElement", "SVGGraphicsElement");
  subClass("SVGDefsElement", "SVGGraphicsElement");
  subClass("SVGImageElement", "SVGGraphicsElement");
  subClass("SVGUseElement", "SVGGraphicsElement");
  subClass("SVGSwitchElement", "SVGGraphicsElement");
  subClass("SVGAElement", "SVGGraphicsElement");
  subClass("SVGForeignObjectElement", "SVGGraphicsElement");
  subClass("SVGTextContentElement", "SVGGraphicsElement");
  subClass("SVGTextPositioningElement", "SVGTextContentElement");
  subClass("SVGTextElement", "SVGTextPositioningElement");
  subClass("SVGTSpanElement", "SVGTextPositioningElement");
  subClass("SVGTextPathElement", "SVGTextContentElement");
  subClass("SVGGradientElement", "SVGElement");
  subClass("SVGLinearGradientElement", "SVGGradientElement");
  subClass("SVGRadialGradientElement", "SVGGradientElement");
  subClass("SVGStopElement", "SVGElement");
  subClass("SVGPatternElement", "SVGElement");
  subClass("SVGMarkerElement", "SVGElement");
  (function () {
    var C = { SVG_MARKERUNITS_UNKNOWN: 0, SVG_MARKERUNITS_USERSPACEONUSE: 1, SVG_MARKERUNITS_STROKEWIDTH: 2, SVG_MARKER_ORIENT_UNKNOWN: 0, SVG_MARKER_ORIENT_AUTO: 1, SVG_MARKER_ORIENT_ANGLE: 2, SVG_MARKER_ORIENT_AUTO_START_REVERSE: 3 };
    for (var k in C) { if (C.hasOwnProperty(k)) { globalThis.SVGMarkerElement[k] = C[k]; globalThis.SVGMarkerElement.prototype[k] = C[k]; } }
  })();
  subClass("SVGClipPathElement", "SVGElement");
  subClass("SVGMaskElement", "SVGElement");
  subClass("SVGFilterElement", "SVGElement");
  subClass("SVGSymbolElement", "SVGGraphicsElement");
  subClass("SVGViewElement", "SVGElement");
  subClass("SVGDescElement", "SVGElement");
  subClass("SVGTitleElement", "SVGElement");
  subClass("SVGMetadataElement", "SVGElement");
  subClass("SVGStyleElement", "SVGElement");
  subClass("SVGScriptElement", "SVGElement");
  subClass("SVGAnimationElement", "SVGElement");
  subClass("SVGAnimateElement", "SVGAnimationElement");
  subClass("SVGSetElement", "SVGAnimationElement");
  subClass("SVGAnimateTransformElement", "SVGAnimationElement");
  subClass("SVGAnimateMotionElement", "SVGAnimationElement");
  subClass("SVGAnimateColorElement", "SVGAnimationElement");
  subClass("SVGMPathElement", "SVGElement");
  // feImage etc. interfaces (minimal — for instanceof and ReferenceError avoidance).
  ["SVGFEBlendElement", "SVGFEColorMatrixElement", "SVGFEComponentTransferElement", "SVGFECompositeElement",
   "SVGFEConvolveMatrixElement", "SVGFEDiffuseLightingElement", "SVGFEDisplacementMapElement", "SVGFEDropShadowElement",
   "SVGFEFloodElement", "SVGFEGaussianBlurElement", "SVGFEImageElement", "SVGFEMergeElement", "SVGFEMorphologyElement",
   "SVGFEOffsetElement", "SVGFESpecularLightingElement", "SVGFETileElement", "SVGFETurbulenceElement"].forEach(function (n) { subClass(n, "SVGElement"); });
  // SMIL TimeEvent (: Event), the <use> shadow root (: ShadowRoot) and ShadowAnimation (: Animation).
  subClass("TimeEvent", "Event");
  subClass("SVGUseElementShadowRoot", "ShadowRoot");
  subClass("ShadowAnimation", typeof globalThis.Animation === "function" ? "Animation" : "Object");
  (function () {
    var T = globalThis.TimeEvent.prototype;
    Object.defineProperty(T, "view", { get: function () { return this.__view || null; }, enumerable: true, configurable: true });
    Object.defineProperty(T, "detail", { get: function () { return this.__detail || 0; }, enumerable: true, configurable: true });
    def(T, "initTimeEvent", function (typeArg) { this.__view = arguments[1] || null; this.__detail = arguments[2] | 0; });
    Object.defineProperty(globalThis.ShadowAnimation.prototype, "sourceAnimation", { get: function () { return this.__sourceAnimation || null; }, enumerable: true, configurable: true });
  })();

  // Tag (lowercased local name) -> interface constructor name, used to set each element's prototype.
  var TAG_IFACE = {
    svg: "SVGSVGElement", g: "SVGGElement", defs: "SVGDefsElement", path: "SVGPathElement",
    rect: "SVGRectElement", circle: "SVGCircleElement", ellipse: "SVGEllipseElement",
    line: "SVGLineElement", polyline: "SVGPolylineElement", polygon: "SVGPolygonElement",
    image: "SVGImageElement", use: "SVGUseElement", switch: "SVGSwitchElement", a: "SVGAElement",
    foreignobject: "SVGForeignObjectElement", text: "SVGTextElement", tspan: "SVGTSpanElement",
    textpath: "SVGTextPathElement", lineargradient: "SVGLinearGradientElement",
    radialgradient: "SVGRadialGradientElement", stop: "SVGStopElement", pattern: "SVGPatternElement",
    marker: "SVGMarkerElement", clippath: "SVGClipPathElement", mask: "SVGMaskElement",
    filter: "SVGFilterElement", symbol: "SVGSymbolElement", view: "SVGViewElement",
    desc: "SVGDescElement", title: "SVGTitleElement", metadata: "SVGMetadataElement",
    style: "SVGStyleElement", script: "SVGScriptElement", animate: "SVGAnimateElement",
    set: "SVGSetElement", animatetransform: "SVGAnimateTransformElement",
    animatemotion: "SVGAnimateMotionElement", animatecolor: "SVGAnimateColorElement",
    mpath: "SVGMPathElement", feimage: "SVGFEImageElement", feblend: "SVGFEBlendElement",
    fegaussianblur: "SVGFEGaussianBlurElement", feflood: "SVGFEFloodElement", femerge: "SVGFEMergeElement",
    fecolormatrix: "SVGFEColorMatrixElement", fecomposite: "SVGFECompositeElement",
    feoffset: "SVGFEOffsetElement", feturbulence: "SVGFETurbulenceElement",
    feconvolvematrix: "SVGFEConvolveMatrixElement", femorphology: "SVGFEMorphologyElement",
    fedisplacementmap: "SVGFEDisplacementMapElement", fediffuselighting: "SVGFEDiffuseLightingElement",
    fespecularlighting: "SVGFESpecularLightingElement", fedropshadow: "SVGFEDropShadowElement",
    fetile: "SVGFETileElement", fecomponenttransfer: "SVGFEComponentTransferElement",
    femergenode: "SVGFEMergeNodeElement", fefuncr: "SVGFEFuncRElement", fefuncg: "SVGFEFuncGElement",
    fefuncb: "SVGFEFuncBElement", fefunca: "SVGFEFuncAElement", fepointlight: "SVGFEPointLightElement",
    fespotlight: "SVGFESpotLightElement", fedistantlight: "SVGFEDistantLightElement"
  };

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
    var m = /^([+-]?(?:[0-9]*\.[0-9]+|[0-9]+\.?)(?:[eE][+-]?[0-9]+)?)\s*([a-zA-Z%]*)$/.exec(s);
    if (!m) { return { value: 0, num: 0, unit: "", type: 0, str: s }; }
    var n = parseFloat(m[1]); var u = m[2] || "";
    // Unknown units (e.g. "deg" on an angle attr) keep the numeric value as-is.
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

  // Vectors (arrays of numbers) so the same engine drives scalars (dim 1), rects/viewBox (dim 4),
  // and number/length lists (variable dim).
  function vecParse(str) { return String(str).trim().split(/[\s,]+/).filter(function (x) { return x.length; }).map(function (x) { return parseLen(x).value; }); }
  function vecLerp(a, b, p) { var o = [], n = Math.max(a.length, b.length); for (var i = 0; i < n; i++) { var av = a[i] || 0, bv = b[i] || 0; o.push(av + p * (bv - av)); } return o; }
  function vecDist(a, b) { var s = 0, n = Math.max(a.length, b.length); for (var i = 0; i < n; i++) { var d = (b[i] || 0) - (a[i] || 0); s += d * d; } return Math.sqrt(s); }

  // Interpolate a vector animation function at simple-duration fraction `f` in [0,1].
  // `values` is an array of vectors (each a number[]).
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
        // Distribute by cumulative vector distance between values.
        var dist = [0]; var total = 0;
        for (var k = 1; k < n; k++) { total += vecDist(values[k - 1], values[k]); dist.push(total); }
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
    return vecLerp(values[seg], values[seg + 1], p);
  }

  // Compute one animation element's contribution to an attribute at time t. `baseVec` is the
  // underlying (attribute) value as a vector; `parseFn` turns a from/to/by/values token into a
  // vector (numbers by default; colors when animating a paint property). Returns {value:number[],
  // additive} or null when the animation has no effect at t.
  function animContribution(a, t, baseVec, parseFn) {
    parseFn = parseFn || vecParse;
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
      values = [parseFn(to != null ? to : (vAttr != null ? vAttr : "0"))];
    } else if (vAttr != null) {
      values = splitList(vAttr).map(parseFn);
      if (values.length === 0) { return null; }
    } else if (from != null && to != null) {
      values = [parseFn(from), parseFn(to)];
    } else if (from != null && by != null) {
      var vf = parseFn(from), vb = parseFn(by);
      values = [vf, vf.map(function (x, i) { return x + (vb[i] || 0); })];
    } else if (by != null) {
      var vby = parseFn(by); values = [vby.map(function () { return 0; }), vby]; additive = true; // pure by-animation is additive
    } else if (to != null) {
      values = [baseVec, parseFn(to)]; // to-animation: starts from the underlying value
    } else if (from != null) {
      values = [parseFn(from)];
    } else {
      return null;
    }

    var v = simpleValue(fraction, values, calcMode, keyTimes, keySplines);
    if (accumulate && iteration > 0 && values.length > 0) {
      var last = values[values.length - 1];
      v = v.map(function (x, i) { return x + iteration * (last[i] || 0); });
    }
    return { value: v, additive: additive };
  }

  // The simple-duration fraction (and repeat iteration) of an animation at time `t`, or null when
  // it has no effect. Shared by the scalar/vector and path animation paths.
  function animTiming(a, t) {
    var ga = function (n) { var v = getAttr(a.__node, n); return v == null ? null : v; };
    var begin = parseBegin(ga("begin"));
    var dur = parseClock(ga("dur"));
    if (dur == null || dur <= 0) { dur = Infinity; }
    var rc = ga("repeatCount");
    var reps = rc === "indefinite" ? Infinity : (rc != null ? num(rc) : 1);
    if (!(reps > 0)) { reps = 1; }
    var activeDur = dur === Infinity ? Infinity : dur * reps;
    var repeatDur = parseClock(ga("repeatDur"));
    if (repeatDur != null && repeatDur < activeDur) { activeDur = repeatDur; }
    var fill = ga("fill") || "remove";
    var local = t - begin;
    if (local < 0) { return null; }
    var simpleDur = dur === Infinity ? activeDur : dur;
    var iteration, fraction;
    if (activeDur !== Infinity && local >= activeDur) {
      if (fill !== "freeze") { return null; }
      iteration = simpleDur === Infinity ? 0 : Math.floor(activeDur / simpleDur);
      if (simpleDur !== Infinity && Math.abs(iteration * simpleDur - activeDur) < 1e-9 && iteration > 0) { iteration -= 1; }
      fraction = 1;
    } else {
      iteration = simpleDur === Infinity ? 0 : Math.floor(local / simpleDur);
      fraction = simpleDur === Infinity ? 0 : (local - iteration * simpleDur) / simpleDur;
    }
    return { fraction: fraction, iteration: iteration };
  }

  function isAnimEl(el) {
    var ln = el && el.__localName;
    return ln === "animate" || ln === "set" || ln === "animatecolor" || ln === "animatetransform" || ln === "animatemotion";
  }

  // Collect the animation elements (document order) that target `el`'s attribute `attr`. Scans the
  // whole document so both nested animations and `(xlink:)href`-referenced ones are found.
  function collectAnimations(el, attr) {
    var out = [];
    var doc = el.ownerDocument;
    if (!doc || typeof doc.getElementsByTagName !== "function") { return out; }
    var all = doc.getElementsByTagName("*");
    for (var i = 0; i < all.length; i++) {
      var c = all[i];
      if (c && c.nodeType === 1 && isAnimEl(c) && getAttr(c.__node, "attributeName") === attr && animTargets(c, el)) {
        out.push(c);
      }
    }
    return out;
  }
  function animTargets(a, el) {
    var href = getAttr(a.__node, "href");
    if (href == null) { href = getAttr(a.__node, "xlink:href"); }
    if (href != null && href.charAt(0) === "#") {
      return href.slice(1) === (getAttr(el.__node, "id") || "");
    }
    // No href: targets its parent element.
    try { return a.parentNode && a.parentNode.__node === el.__node; } catch (e) { return false; }
  }

  // The animated value vector of `attr` on `el`, given its base value vector. Returns the base when
  // no animation is active.
  function svgAnimVec(el, attr, baseVec, parseFn) {
    var anims = collectAnimations(el, attr);
    if (!anims.length) { return baseVec; }
    var t = currentTime();
    var result = baseVec; var any = false;
    for (var i = 0; i < anims.length; i++) {
      var c = animContribution(anims[i], t, baseVec, parseFn);
      if (c == null) { continue; }
      any = true;
      if (c.additive && result.length === c.value.length) {
        result = result.map(function (x, j) { return x + c.value[j]; });
      } else {
        result = c.value;
      }
    }
    return any ? result : baseVec;
  }
  globalThis.__svgAnimVec = svgAnimVec;
  // Scalar convenience wrapper.
  function svgAnimNum(el, attr, baseNum, parseFn) { return svgAnimVec(el, attr, [baseNum], parseFn)[0]; }
  globalThis.__svgAnimNum = svgAnimNum;
  function angleVec(s) { return [parseAngle(s).value]; }

  // -------------------------------------------------------------------------------------------
  // SVGLength factory (live, backed by an attribute) and SVGAnimatedLength.
  // -------------------------------------------------------------------------------------------
  function makeLength(getNum, getStr, getType, setNum) {
    var L = Object.create(SVGLength.prototype);
    L.__g = getNum; L.__gs = getStr; L.__gt = getType; L.__s = setNum || null;
    return L;
  }
  // Absolute-unit conversion factors to CSS px (em/ex resolved against `ctxEm`; % is unsupported for a
  // detached length and stays as the raw number).
  var LEN_SUFFIX = { 1: "", 2: "%", 3: "em", 4: "ex", 5: "px", 6: "cm", 7: "mm", 8: "in", 9: "pt", 10: "pc" };
  function lenToPx(n, u, ctxEm) { switch (u) { case 6: return n * 96 / 2.54; case 7: return n * 96 / 25.4; case 8: return n * 96; case 9: return n * 96 / 72; case 10: return n * 16; case 3: return n * ctxEm; case 4: return n * ctxEm / 2; default: return n; } }
  function pxToLen(px, u, ctxEm) { switch (u) { case 6: return px * 2.54 / 96; case 7: return px * 25.4 / 96; case 8: return px / 96; case 9: return px * 72 / 96; case 10: return px / 16; case 3: return px / ctxEm; case 4: return px / (ctxEm / 2); default: return px; } }
  // A standalone, mutable SVGLength (createSVGLength). `ctxEm` is the font-size used to resolve em/ex.
  function makeMutableLength(ctxEm) {
    ctxEm = ctxEm || 16;
    var st = { n: 0, u: 1 }; // valueInSpecifiedUnits, unitType (initial: NUMBER 0)
    var L = Object.create(SVGLength.prototype);
    L.__g = function () { return lenToPx(st.n, st.u, ctxEm); };
    L.__gs = function () { return numStr(st.n) + LEN_SUFFIX[st.u]; };
    L.__gt = function () { return st.u; };
    L.__s = function (px) { st.n = pxToLen(px, st.u, ctxEm); };
    L.__sn = function (n) { st.n = +n; };
    L.__sstr = function (v) { var p = parseLen(v); st.n = p.num; st.u = p.type || 1; };
    L.__st = st; L.__em = ctxEm;
    return L;
  }
  function numStr(n) { return (n === (n | 0)) ? String(n | 0) : String(n); }

  function makeAnimatedLength(el, attr, dflt) {
    var node = el.__node;
    dflt = dflt == null ? "0" : dflt;
    function raw() { var v = getAttr(node, attr); return v == null || v === "" ? dflt : v; }
    var anim = Object.create(SVGAnimatedLength.prototype);
    var baseVal = makeLength(
      function () { return parseLen(raw()).value; },
      raw,
      function () { return parseLen(raw()).type; },
      function (v) { setAttr(node, attr, String(v)); }
    );
    var animVal = makeLength(
      function () { return svgAnimNum(el, attr, parseLen(raw()).value); },
      raw,
      function () { return parseLen(raw()).type; },
      null
    );
    anim.__base = baseVal; anim.__anim = animVal;
    return anim;
  }
  // Spec initial values for length attributes that aren't "0", keyed by "tag.attr".
  var LEN_DEFAULTS = {
    "filter.x": "-10%", "filter.y": "-10%", "filter.width": "120%", "filter.height": "120%",
    "mask.x": "-10%", "mask.y": "-10%", "mask.width": "120%", "mask.height": "120%",
    "lineargradient.x1": "0%", "lineargradient.y1": "0%", "lineargradient.x2": "100%", "lineargradient.y2": "0%",
    "radialgradient.cx": "50%", "radialgradient.cy": "50%", "radialgradient.r": "50%",
    "radialgradient.fx": "50%", "radialgradient.fy": "50%", "radialgradient.fr": "0%",
    "svg.width": "100%", "svg.height": "100%",
    "marker.markerWidth": "3", "marker.markerHeight": "3"
  };

  // SVGRect (live, backed by a 4-number getter) and SVGAnimatedRect (viewBox).
  function makeRect(getVec) {
    var R = Object.create(globalThis.SVGRect.prototype);
    R.__get = getVec;
    return R;
  }
  function makeAnimatedRect(el, attr) {
    var node = el.__node;
    function baseVec() { var s = getAttr(node, attr); return s == null ? [0, 0, 0, 0] : vecParse(s); }
    var anim = Object.create(globalThis.SVGAnimatedRect.prototype);
    anim.__base = makeRect(baseVec);
    anim.__anim = makeRect(function () { return svgAnimVec(el, attr, baseVec()); });
    return anim;
  }

  // SVGNumberList / SVGLengthList (live, read-only items) and their SVGAnimated* wrappers. `isNumber`
  // picks the list/item interfaces, resolved through globalThis so it survives finalizeInterfaces.
  function makeItemList(getVec, isNumber) {
    var L = Object.create(globalThis[isNumber ? "SVGNumberList" : "SVGLengthList"].prototype);
    L.__l = {
      len: function () { return getVec().length; },
      get: function (i) {
        var v = getVec();
        if (isNumber) { var n = Object.create(globalThis.SVGNumber.prototype); n.__v = v[i]; return n; }
        return makeLength(function () { return v[i]; }, function () { return String(v[i]); }, function () { return 1; }, null);
      },
      clear: function () {}, init: function (x) { return x; }, append: function (x) { return x; },
      insert: function (x) { return x; }, remove: function (x) { return x; }, replace: function (x) { return x; }
    };
    return L;
  }
  function makeAnimatedItemList(el, attr, isNumber) {
    var node = el.__node;
    function baseVec() { var s = getAttr(node, attr); return s == null || s === "" ? [] : vecParse(s); }
    var anim = Object.create(globalThis[isNumber ? "SVGAnimatedNumberList" : "SVGAnimatedLengthList"].prototype);
    anim.__base = makeItemList(baseVec, isNumber);
    anim.__anim = makeItemList(function () { return svgAnimVec(el, attr, baseVec()); }, isNumber);
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
    filter: ["x", "y", "width", "height"],
    marker: ["refX", "refY", "markerWidth", "markerHeight"],
    lineargradient: ["x1", "y1", "x2", "y2"],
    radialgradient: ["cx", "cy", "r", "fx", "fy", "fr"],
    textpath: ["startOffset"]
  };

  // SVGAnimatedAngle (marker orient) and SVGAnimatedEnumeration helpers.
  function parseAngle(s) {
    if (s == null) { return { value: 0, type: 1 }; }
    s = String(s).trim();
    var m = /^([+-]?[0-9]*\.?[0-9]+)(deg|grad|rad)?$/.exec(s);
    if (!m) { return { value: 0, type: 0 }; }
    var n = parseFloat(m[1]);
    if (m[2] === "rad") { n = n * 180 / Math.PI; } else if (m[2] === "grad") { n = n * 0.9; }
    return { value: n, type: m[2] === "rad" ? 3 : m[2] === "grad" ? 4 : 2 };
  }
  function makeAngle(getVal) { var A = Object.create(SVGAngle.prototype); A.__g = getVal; A.__gt = function () { return 2; }; A.__s = null; return A; }
  function makeAnimatedAngle(el, attr) {
    var node = el.__node;
    function base() { var o = getAttr(node, attr); if (o == null || o === "auto" || o === "auto-start-reverse") { return 0; } return parseAngle(o).value; }
    var anim = Object.create(globalThis.SVGAnimatedAngle.prototype);
    anim.__base = makeAngle(base);
    anim.__anim = makeAngle(function () { return svgAnimNum(el, attr, base(), angleVec); });
    return anim;
  }
  function makeAnimatedEnum(getBase) {
    var anim = Object.create(globalThis.SVGAnimatedEnumeration.prototype);
    anim.__bget = getBase; anim.__bset = function () {}; anim.__aget = getBase;
    return anim;
  }

  // A writable SVGAnimatedEnumeration backed by an attribute via a keyword<->number map. Setting an
  // out-of-range value throws (per the IDL). `def` is the keyword used when the attribute is absent.
  function makeEnumProp(el, attr, map, def) {
    var node = el.__node;
    var rev = {};
    for (var k in map) { if (map.hasOwnProperty(k)) { rev[map[k]] = k; } }
    function base() { var v = getAttr(node, attr); if (v == null) { return map[def]; } return map[v] != null ? map[v] : 0; }
    var anim = Object.create(globalThis.SVGAnimatedEnumeration.prototype);
    anim.__bget = base;
    anim.__bset = function (n) { n = n | 0; if (n <= 0 || rev[n] == null) { throw new TypeError("invalid enumeration value"); } setAttr(node, attr, rev[n]); };
    anim.__aget = base;
    return anim;
  }
  var ENUM_UNITS = { userSpaceOnUse: 1, objectBoundingBox: 2 };
  // Per-element enumerated properties: [attr, keyword->number map, default keyword].
  var ENUM_PROPS = {
    fecolormatrix: [["type", { matrix: 1, saturate: 2, hueRotate: 3, luminanceToAlpha: 4 }, "matrix"]],
    fecomposite: [["operator", { over: 1, "in": 2, out: 3, atop: 4, xor: 5, arithmetic: 6 }, "over"]],
    feconvolvematrix: [["edgeMode", { duplicate: 1, wrap: 2, none: 3 }, "duplicate"]],
    fedisplacementmap: [["xChannelSelector", { R: 1, G: 2, B: 3, A: 4 }, "A"], ["yChannelSelector", { R: 1, G: 2, B: 3, A: 4 }, "A"]],
    femorphology: [["operator", { erode: 1, dilate: 2 }, "erode"]],
    feturbulence: [["type", { fractalNoise: 1, turbulence: 2 }, "turbulence"], ["stitchTiles", { stitch: 1, noStitch: 2 }, "noStitch"]],
    filter: [["filterUnits", ENUM_UNITS, "objectBoundingBox"], ["primitiveUnits", ENUM_UNITS, "userSpaceOnUse"]],
    clippath: [["clipPathUnits", ENUM_UNITS, "userSpaceOnUse"]],
    mask: [["maskUnits", ENUM_UNITS, "objectBoundingBox"], ["maskContentUnits", ENUM_UNITS, "userSpaceOnUse"]],
    pattern: [["patternUnits", ENUM_UNITS, "objectBoundingBox"], ["patternContentUnits", ENUM_UNITS, "userSpaceOnUse"]],
    fefuncr: [["type", { identity: 1, table: 2, discrete: 3, linear: 4, gamma: 5 }, "identity"]],
    fefuncg: [["type", { identity: 1, table: 2, discrete: 3, linear: 4, gamma: 5 }, "identity"]],
    fefuncb: [["type", { identity: 1, table: 2, discrete: 3, linear: 4, gamma: 5 }, "identity"]],
    fefunca: [["type", { identity: 1, table: 2, discrete: 3, linear: 4, gamma: 5 }, "identity"]],
    textpath: [["method", { align: 1, stretch: 2 }, "align"], ["spacing", { auto: 1, exact: 2 }, "exact"]],
    marker: [["markerUnits", { userSpaceOnUse: 1, strokeWidth: 2 }, "strokeWidth"]]
  };
  var LENGTHADJUST_MAP = { spacing: 1, spacingAndGlyphs: 2 };
  // Interface constants for the enumerated properties.
  var ENUM_CONSTS = {
    SVGFEColorMatrixElement: { SVG_FECOLORMATRIX_TYPE_UNKNOWN: 0, SVG_FECOLORMATRIX_TYPE_MATRIX: 1, SVG_FECOLORMATRIX_TYPE_SATURATE: 2, SVG_FECOLORMATRIX_TYPE_HUEROTATE: 3, SVG_FECOLORMATRIX_TYPE_LUMINANCETOALPHA: 4 },
    SVGFECompositeElement: { SVG_FECOMPOSITE_OPERATOR_UNKNOWN: 0, SVG_FECOMPOSITE_OPERATOR_OVER: 1, SVG_FECOMPOSITE_OPERATOR_IN: 2, SVG_FECOMPOSITE_OPERATOR_OUT: 3, SVG_FECOMPOSITE_OPERATOR_ATOP: 4, SVG_FECOMPOSITE_OPERATOR_XOR: 5, SVG_FECOMPOSITE_OPERATOR_ARITHMETIC: 6 },
    SVGFEConvolveMatrixElement: { SVG_EDGEMODE_UNKNOWN: 0, SVG_EDGEMODE_DUPLICATE: 1, SVG_EDGEMODE_WRAP: 2, SVG_EDGEMODE_NONE: 3 },
    SVGFEDisplacementMapElement: { SVG_CHANNEL_UNKNOWN: 0, SVG_CHANNEL_R: 1, SVG_CHANNEL_G: 2, SVG_CHANNEL_B: 3, SVG_CHANNEL_A: 4 },
    SVGFEMorphologyElement: { SVG_MORPHOLOGY_OPERATOR_UNKNOWN: 0, SVG_MORPHOLOGY_OPERATOR_ERODE: 1, SVG_MORPHOLOGY_OPERATOR_DILATE: 2 },
    SVGFETurbulenceElement: { SVG_TURBULENCE_TYPE_UNKNOWN: 0, SVG_TURBULENCE_TYPE_FRACTALNOISE: 1, SVG_TURBULENCE_TYPE_TURBULENCE: 2, SVG_STITCHTYPE_UNKNOWN: 0, SVG_STITCHTYPE_STITCH: 1, SVG_STITCHTYPE_NOSTITCH: 2 },
    SVGGradientElement: { SVG_SPREADMETHOD_UNKNOWN: 0, SVG_SPREADMETHOD_PAD: 1, SVG_SPREADMETHOD_REFLECT: 2, SVG_SPREADMETHOD_REPEAT: 3 },
    SVGComponentTransferFunctionElement: { SVG_FECOMPONENTTRANSFER_TYPE_UNKNOWN: 0, SVG_FECOMPONENTTRANSFER_TYPE_IDENTITY: 1, SVG_FECOMPONENTTRANSFER_TYPE_TABLE: 2, SVG_FECOMPONENTTRANSFER_TYPE_DISCRETE: 3, SVG_FECOMPONENTTRANSFER_TYPE_LINEAR: 4, SVG_FECOMPONENTTRANSFER_TYPE_GAMMA: 5 },
    SVGTextPathElement: { TEXTPATH_METHODTYPE_UNKNOWN: 0, TEXTPATH_METHODTYPE_ALIGN: 1, TEXTPATH_METHODTYPE_STRETCH: 2, TEXTPATH_SPACINGTYPE_UNKNOWN: 0, TEXTPATH_SPACINGTYPE_AUTO: 1, TEXTPATH_SPACINGTYPE_EXACT: 2 },
    SVGTextContentElement: { LENGTHADJUST_UNKNOWN: 0, LENGTHADJUST_SPACING: 1, LENGTHADJUST_SPACINGANDGLYPHS: 2 }
  };
  (function () {
    for (var iface in ENUM_CONSTS) {
      if (!ENUM_CONSTS.hasOwnProperty(iface)) { continue; }
      var ctor = globalThis[iface];
      if (typeof ctor !== "function") { continue; }
      var cs = ENUM_CONSTS[iface];
      for (var key in cs) { if (cs.hasOwnProperty(key)) { ctor[key] = cs[key]; ctor.prototype[key] = cs[key]; } }
    }
    // Constructors for the fe* light/func and other interfaces referenced by name in tests.
    ["SVGComponentTransferFunctionElement", "SVGFEFuncRElement", "SVGFEFuncGElement", "SVGFEFuncBElement", "SVGFEFuncAElement", "SVGFEPointLightElement", "SVGFESpotLightElement", "SVGFEDistantLightElement", "SVGFEMergeNodeElement", "SVGFETileElement", "SVGFEFloodElement", "SVGFEDropShadowElement"].forEach(function (n) { if (typeof globalThis[n] !== "function") { globalThis[n] = new Function("return function " + n + "(){}")(); } });
    // The component-transfer constants live on SVGComponentTransferFunctionElement (created above).
    (function () { var c = globalThis.SVGComponentTransferFunctionElement, cs = ENUM_CONSTS.SVGComponentTransferFunctionElement; if (c && cs) { for (var k in cs) { if (cs.hasOwnProperty(k)) { c[k] = cs[k]; c.prototype[k] = cs[k]; } } } })();
  })();

  // -------------------------------------------------------------------------------------------
  // Per-element decoration entry point (called by browser_env's enrichElement).
  // -------------------------------------------------------------------------------------------
  function svgEnrich(el) {
    if (!el || el.namespaceURI !== SVG_NS) { return; }
    var ln = "";
    try { ln = (el.localName || el.tagName || "").toLowerCase(); } catch (e) {}
    def(el, "__localName", ln);

    // Set the specific SVG element interface prototype (so `instanceof SVGRectElement` works); its
    // chain ends at SVGElement.prototype set by browser-env's applyNodePrototype. All reflected IDL
    // members live on the interface prototypes (see installSvgProtos), inherited from here.
    var ifaceName = TAG_IFACE[ln];
    if (ifaceName) {
      try { if (Object.getPrototypeOf(el) !== globalThis[ifaceName].prototype) { Object.setPrototypeOf(el, globalThis[ifaceName].prototype); } } catch (e) {}
    }
  }
  // -------------------------------------------------------------------------------------------
  // transform / SVGAnimatedTransformList + animateTransform.
  // -------------------------------------------------------------------------------------------
  function transformMatrix(type, v) {
    switch (type) {
      case "translate": return makeMatrix(1, 0, 0, 1, v[0] || 0, v[1] || 0);
      case "scale": { var sx = v[0] || 0, sy = v.length > 1 ? v[1] : sx; return makeMatrix(sx, 0, 0, sy, 0, 0); }
      case "rotate": { var a = (v[0] || 0) * Math.PI / 180, cx = v[1] || 0, cy = v[2] || 0; var cos = Math.cos(a), sin = Math.sin(a); return makeMatrix(1, 0, 0, 1, cx, cy).multiply(makeMatrix(cos, sin, -sin, cos, 0, 0)).multiply(makeMatrix(1, 0, 0, 1, -cx, -cy)); }
      case "skewx": case "skewX": return makeMatrix(1, 0, Math.tan((v[0] || 0) * Math.PI / 180), 1, 0, 0);
      case "skewy": case "skewY": return makeMatrix(1, Math.tan((v[0] || 0) * Math.PI / 180), 0, 1, 0, 0);
      case "matrix": return makeMatrix(v[0] || 0, v[1] || 0, v[2] || 0, v[3] || 0, v[4] || 0, v[5] || 0);
      default: return makeMatrix(1, 0, 0, 1, 0, 0);
    }
  }
  var TTYPE = { matrix: 1, translate: 2, scale: 3, rotate: 4, skewx: 5, skewX: 5, skewy: 6, skewY: 6 };
  function makeTransform(type, v) {
    var T = Object.create(SVGTransform.prototype);
    var lt = String(type).toLowerCase();
    T.__t = {
      type: TTYPE[lt] || 0,
      angle: (lt === "rotate" || lt === "skewx" || lt === "skewy") ? (v[0] || 0) : 0,
      matrix: transformMatrix(lt, v)
    };
    return T;
  }
  function parseTransformList(str) {
    var out = [];
    var re = /(matrix|translate|scale|rotate|skewX|skewY)\s*\(([^)]*)\)/g, m;
    while ((m = re.exec(str)) !== null) {
      var vals = m[2].split(/[\s,]+/).filter(function (x) { return x.length; }).map(num);
      out.push(makeTransform(m[1], vals));
    }
    return out;
  }
  // A read-only SVGTransformList that re-reads its items live (for baseVal/animVal reflection).
  function makeLiveTransformList(getItems) {
    var L = Object.create(globalThis.SVGTransformList.prototype);
    L.__l = {
      len: function () { return getItems().length; },
      get: function (i) { return getItems()[i]; },
      clear: function () {}, init: function (t) { return t; }, append: function (t) { return t; },
      insert: function (t) { return t; }, remove: function (i) { return getItems()[i]; }, replace: function (t) { return t; },
      consolidate: function () { var items = getItems(); if (!items.length) { return null; } var m = items[0].matrix; for (var k = 1; k < items.length; k++) { m = m.multiply(items[k].matrix); } return makeTransform("matrix", [m.a, m.b, m.c, m.d, m.e, m.f]); }
    };
    return L;
  }
  function makeTransformList(items) {
    var L = Object.create(globalThis.SVGTransformList.prototype);
    L.__l = {
      len: function () { return items.length; },
      get: function (i) { return items[i]; },
      clear: function () { items.length = 0; },
      init: function (t) { items.length = 0; items.push(t); return t; },
      append: function (t) { items.push(t); return t; },
      insert: function (t, i) { items.splice(i, 0, t); return t; },
      remove: function (i) { return items.splice(i, 1)[0]; },
      replace: function (t, i) { items[i] = t; return t; },
      consolidate: function () { if (!items.length) { return null; } var m = items[0].matrix; for (var k = 1; k < items.length; k++) { m = m.multiply(items[k].matrix); } var t = makeTransform("matrix", [m.a, m.b, m.c, m.d, m.e, m.f]); items.length = 0; items.push(t); return t; }
    };
    return L;
  }
  function transformAnimVal(el, attr) {
    var node = el.__node;
    var baseList = parseTransformList(getAttr(node, attr) || "");
    var anims = collectAnimations(el, attr).filter(function (a) { return a.__localName === "animatetransform"; });
    if (!anims.length) { return baseList; }
    var t = currentTime(); var animTransforms = []; var additiveAll = true;
    for (var i = 0; i < anims.length; i++) {
      var c = animContribution(anims[i], t, [0], vecParse);
      if (c == null) { continue; }
      var ty = getAttr(anims[i].__node, "type") || "translate";
      animTransforms.push(makeTransform(ty, c.value));
      if (!c.additive) { additiveAll = false; }
    }
    if (!animTransforms.length) { return baseList; }
    return additiveAll ? baseList.concat(animTransforms) : animTransforms;
  }
  function makeAnimatedTransformListAttr(el, attr) {
    var node = el.__node;
    var anim = Object.create(globalThis.SVGAnimatedTransformList.prototype);
    anim.__base = makeLiveTransformList(function () { return parseTransformList(getAttr(node, attr) || ""); });
    anim.__anim = makeLiveTransformList(function () { return transformAnimVal(el, attr); });
    return anim;
  }
  function makeAnimatedTransformList(el) { return makeAnimatedTransformListAttr(el, "transform"); }

  // SVGAnimatedString (className, href, etc.).
  function makeAnimatedString(el, attr) {
    var node = el.__node;
    var anim = Object.create(globalThis.SVGAnimatedString.prototype);
    anim.__bget = function () { var v = getAttr(node, attr); return v == null ? "" : v; };
    anim.__bset = function (v) { setAttr(node, attr, v == null ? "" : String(v)); };
    anim.__aget = anim.__bget;
    return anim;
  }
  function makeAnimatedNumber(el, attr, dflt) {
    var node = el.__node; dflt = dflt == null ? 0 : dflt;
    function base() { var v = getAttr(node, attr); return v == null || v === "" ? dflt : (parseFloat(v) || 0); }
    var a = Object.create(globalThis.SVGAnimatedNumber.prototype);
    a.__bget = base; a.__bset = function (v) { setAttr(node, attr, String(+v)); }; a.__aget = function () { return svgAnimNum(el, attr, base()); };
    return a;
  }
  function makeAnimatedInteger(el, attr, dflt) {
    var node = el.__node; dflt = dflt == null ? 0 : dflt;
    function base() { var v = getAttr(node, attr); return v == null || v === "" ? dflt : (parseInt(v, 10) || 0); }
    var a = Object.create(globalThis.SVGAnimatedInteger.prototype);
    a.__bget = base; a.__bset = function (v) { setAttr(node, attr, String(v | 0)); }; a.__aget = base;
    return a;
  }
  function makeAnimatedBoolean(el, attr) {
    var node = el.__node;
    function base() { return getAttr(node, attr) === "true"; }
    var a = Object.create(globalThis.SVGAnimatedBoolean.prototype);
    a.__bget = base; a.__bset = function (v) { setAttr(node, attr, v ? "true" : "false"); }; a.__aget = base;
    return a;
  }
  // SVGStringList (requiredExtensions / systemLanguage / requiredFeatures): a live list backed by a
  // whitespace/comma-separated attribute.
  // SVGPointList over the `points` attribute (live; items are SVGPoint).
  function makePointList(el) {
    var node = el.__node;
    function vec() { var s = getAttr(node, "points"); if (s == null || s === "") { return []; } var n = String(s).trim().split(/[\s,]+/).filter(function (x) { return x.length; }).map(parseFloat); var pts = []; for (var i = 0; i + 1 < n.length; i += 2) { pts.push([n[i], n[i + 1]]); } return pts; }
    var L = Object.create(globalThis.SVGPointList.prototype);
    L.__l = {
      len: function () { return vec().length; },
      get: function (i) { var p = vec()[i]; return makePoint(p[0], p[1]); },
      clear: function () { setAttr(node, "points", ""); }, init: function (p) { return p; }, append: function (p) { return p; },
      insert: function (p) { return p; }, remove: function (i) { var p = vec()[i]; return makePoint(p[0], p[1]); }, replace: function (p) { return p; }
    };
    return L;
  }
  function makeStringList(el, attr) {
    var node = el.__node;
    function vec() { var s = getAttr(node, attr); return s == null || s === "" ? [] : String(s).trim().split(/[\s,]+/).filter(function (x) { return x.length; }); }
    function put(v) { setAttr(node, attr, v.join(" ")); }
    var L = Object.create(globalThis.SVGStringList.prototype);
    L.__l = {
      len: function () { return vec().length; },
      get: function (i) { return vec()[i]; },
      clear: function () { setAttr(node, attr, ""); },
      init: function (s) { put([String(s)]); return s; },
      append: function (s) { var v = vec(); v.push(String(s)); put(v); return s; },
      insert: function (s, i) { var v = vec(); v.splice(Math.max(0, Math.min(i, v.length)), 0, String(s)); put(v); return s; },
      replace: function (s, i) { var v = vec(); v[i] = String(s); put(v); return s; },
      remove: function (i) { var v = vec(); var r = v.splice(i, 1)[0]; put(v); return r; }
    };
    return L;
  }
  function makeAnimatedPAR() {
    var par = Object.create(globalThis.SVGAnimatedPreserveAspectRatio.prototype);
    function mk() { var p = Object.create(SVGPreserveAspectRatio.prototype); p.__align = 6; p.__mos = 1; return p; }
    par.__base = mk(); par.__anim = mk();
    return par;
  }
  function svgAncestor(el, names) {
    var p = el.parentNode;
    while (p && p.nodeType === 1) {
      if (p.namespaceURI === SVG_NS && names[p.__localName]) { return p; }
      p = p.parentNode;
    }
    return null;
  }

  // A cached accessor installed on an interface PROTOTYPE: the SVGAnimated* wrapper is built lazily
  // per element and memoized on the instance (stable identity), but the property itself is inherited.
  function accProto(proto, name, factory) {
    Object.defineProperty(proto, name, {
      get: function () {
        var c = this.__svgCache || def(this, "__svgCache", {}) || this.__svgCache;
        if (!(name in c)) { c[name] = factory(this, name); }
        return c[name];
      },
      configurable: true, enumerable: true
    });
  }

  // Install the SVG reflected IDL members on the interface PROTOTYPES (so they are inherited, which is
  // what idlharness verifies — not present as per-instance own properties). Run once at bootstrap.
  function installSvgProtos() {
    var G = globalThis;
    // Scalar length attributes per element interface (reuse the per-tag tables).
    for (var tag in LEN_ATTRS) {
      if (!LEN_ATTRS.hasOwnProperty(tag)) { continue; }
      var ifn = TAG_IFACE[tag]; if (!ifn) { continue; }
      (function (proto, tg) {
        LEN_ATTRS[tg].forEach(function (a) { accProto(proto, a, function (el) { return makeAnimatedLength(el, a, LEN_DEFAULTS[tg + "." + a]); }); });
      })(G[ifn].prototype, tag);
    }
    // Enumerated attributes per element interface.
    for (var etag in ENUM_PROPS) {
      if (!ENUM_PROPS.hasOwnProperty(etag)) { continue; }
      var eifn = TAG_IFACE[etag]; if (!eifn) { continue; }
      (function (proto, specs) {
        specs.forEach(function (s) { accProto(proto, s[0], function (el) { return makeEnumProp(el, s[0], s[1], s[2]); }); });
      })(G[eifn].prototype, ENUM_PROPS[etag]);
    }

    // SVGElement: className / ownerSVGElement / viewportElement.
    accProto(G.SVGElement.prototype, "className", function (el) { return makeAnimatedString(el, "class"); });
    Object.defineProperty(G.SVGElement.prototype, "ownerSVGElement", { get: function () { return svgAncestor(this, { svg: 1 }); }, configurable: true, enumerable: true });
    Object.defineProperty(G.SVGElement.prototype, "viewportElement", { get: function () { return svgAncestor(this, { svg: 1, symbol: 1 }); }, configurable: true, enumerable: true });

    // SVGGraphicsElement: transform + the SVGTests members (requiredExtensions / systemLanguage).
    accProto(G.SVGGraphicsElement.prototype, "transform", function (el) { return makeAnimatedTransformList(el); });
    var TESTS = [G.SVGGraphicsElement, G.SVGAnimationElement, G.SVGClipPathElement, G.SVGPatternElement, G.SVGMaskElement];
    TESTS.forEach(function (C) {
      accProto(C.prototype, "requiredExtensions", function (el) { return makeStringList(el, "requiredExtensions"); });
      accProto(C.prototype, "systemLanguage", function (el) { return makeStringList(el, "systemLanguage"); });
    });

    // SVGURIReference.href (SVGAnimatedString) on the interfaces that include it.
    [G.SVGAElement, G.SVGUseElement, G.SVGImageElement, G.SVGGradientElement, G.SVGPatternElement, G.SVGTextPathElement, G.SVGMPathElement, G.SVGFEImageElement, G.SVGScriptElement].forEach(function (C) {
      accProto(C.prototype, "href", function (el) { return makeAnimatedString(el, getAttr(el.__node, "href") != null ? "href" : "xlink:href"); });
    });

    // SVGFitToViewBox: viewBox + preserveAspectRatio.
    [G.SVGSVGElement, G.SVGSymbolElement, G.SVGMarkerElement, G.SVGPatternElement, G.SVGViewElement].forEach(function (C) {
      accProto(C.prototype, "viewBox", function (el) { return makeAnimatedRect(el, "viewBox"); });
    });
    [G.SVGSVGElement, G.SVGSymbolElement, G.SVGMarkerElement, G.SVGPatternElement, G.SVGViewElement, G.SVGImageElement, G.SVGFEImageElement].forEach(function (C) {
      accProto(C.prototype, "preserveAspectRatio", function (el) { return makeAnimatedPAR(); });
    });

    // SVGGradientElement enumerated units (shared by linear & radial gradients).
    [["gradientUnits", ENUM_UNITS, "objectBoundingBox"], ["spreadMethod", { pad: 1, reflect: 2, repeat: 3 }, "pad"]].forEach(function (s) {
      accProto(G.SVGGradientElement.prototype, s[0], function (el) { return makeEnumProp(el, s[0], s[1], s[2]); });
    });
    // SVGMarkerElement.orient is a plain DOMString reflection (alongside orientType / orientAngle).
    Object.defineProperty(G.SVGMarkerElement.prototype, "orient", { get: function () { var v = getAttr(this.__node, "orient"); return v == null ? "" : v; }, set: function (v) { setAttr(this.__node, "orient", String(v)); }, enumerable: true, configurable: true });
    // Gradient / pattern transforms.
    accProto(G.SVGGradientElement.prototype, "gradientTransform", function (el) { return makeAnimatedTransformListAttr(el, "gradientTransform"); });
    accProto(G.SVGPatternElement.prototype, "patternTransform", function (el) { return makeAnimatedTransformListAttr(el, "patternTransform"); });

    // SVGTextPositioningElement: x/y/dx/dy length-lists + rotate number-list.
    [["x", false], ["y", false], ["dx", false], ["dy", false], ["rotate", true]].forEach(function (spec) {
      accProto(G.SVGTextPositioningElement.prototype, spec[0], function (el) { return makeAnimatedItemList(el, spec[0], spec[1]); });
    });

    // SVGStopElement.offset (SVGAnimatedNumber).
    accProto(G.SVGStopElement.prototype, "offset", function (el) { return makeAnimatedNumber(el, "offset", 0); });

    // SVGAnimatedPoints: points / animatedPoints on polyline & polygon.
    [G.SVGPolylineElement, G.SVGPolygonElement].forEach(function (C) {
      accProto(C.prototype, "points", function (el) { return makePointList(el); });
      accProto(C.prototype, "animatedPoints", function (el) { return makePointList(el); });
    });
    // SVGUseElement instance roots (shadow tree not modelled — expose null).
    Object.defineProperty(G.SVGUseElement.prototype, "instanceRoot", { get: function () { return null; }, enumerable: true, configurable: true });
    Object.defineProperty(G.SVGUseElement.prototype, "animatedInstanceRoot", { get: function () { return null; }, enumerable: true, configurable: true });
    // crossOrigin (SVGImageElement) and disabled (SVGStyleElement).
    Object.defineProperty(G.SVGImageElement.prototype, "crossOrigin", { get: function () { var v = getAttr(this.__node, "crossorigin"); return v == null ? null : v; }, set: function (v) { setAttr(this.__node, "crossorigin", v == null ? "" : String(v)); }, enumerable: true, configurable: true });
    Object.defineProperty(G.SVGStyleElement.prototype, "disabled", { get: function () { return !!this.__disabled; }, set: function (v) { this.__disabled = !!v; }, enumerable: true, configurable: true });
    // DOMString attribute reflections that document.js would otherwise install as own props.
    function reflectStr(proto, prop, attr) { Object.defineProperty(proto, prop, { get: function () { var v = getAttr(this.__node, attr); return v == null ? "" : v; }, set: function (v) { setAttr(this.__node, attr, String(v)); }, enumerable: true, configurable: true }); }
    accProto(G.SVGAElement.prototype, "target", function (el) { return makeAnimatedString(el, "target"); });
    ["download", "ping", "rel", "hreflang", "type", "referrerPolicy"].forEach(function (p) { reflectStr(G.SVGAElement.prototype, p, p === "referrerPolicy" ? "referrerpolicy" : p); });
    Object.defineProperty(G.SVGAElement.prototype, "relList", { get: function () { var tl = globalThis.__makeTokenList(this.__node, "rel"); try { Object.setPrototypeOf(tl, globalThis.DOMTokenList.prototype); } catch (e) {} return tl; }, set: function (v) { setAttr(this.__node, "rel", String(v)); }, enumerable: true, configurable: true });
    // HTMLHyperlinkElementUtils-style URL decomposition over the resolved href.
    var aHrefURL = function (el) { var h = getAttr(el.__node, "href"); if (h == null) { h = getAttr(el.__node, "xlink:href"); } try { return new globalThis.URL(h == null ? "" : h, el.ownerDocument && (el.ownerDocument.baseURI || el.ownerDocument.URL)); } catch (e) { return null; } };
    ["protocol", "username", "password", "host", "hostname", "port", "pathname", "search", "hash"].forEach(function (p) {
      Object.defineProperty(G.SVGAElement.prototype, p, {
        get: function () { var u = aHrefURL(this); return u ? u[p] : ""; },
        set: function (v) { var u = aHrefURL(this); if (u) { try { u[p] = v; setAttr(this.__node, "href", u.href); } catch (e) {} } },
        enumerable: true, configurable: true
      });
    });
    Object.defineProperty(G.SVGAElement.prototype, "origin", { get: function () { var u = aHrefURL(this); return u ? u.origin : ""; }, enumerable: true, configurable: true });
    ["type", "media", "title"].forEach(function (p) { reflectStr(G.SVGStyleElement.prototype, p, p); });
    ["type", "crossOrigin"].forEach(function (p) { reflectStr(G.SVGScriptElement.prototype, p, p === "crossOrigin" ? "crossorigin" : p); });
    // feConvolveMatrix integer / boolean attributes.
    accProto(G.SVGFEConvolveMatrixElement.prototype, "orderX", function (el) { return makeAnimatedInteger(el, "orderX", 0); });
    accProto(G.SVGFEConvolveMatrixElement.prototype, "orderY", function (el) { return makeAnimatedInteger(el, "orderY", 0); });
    accProto(G.SVGFEConvolveMatrixElement.prototype, "targetX", function (el) { return makeAnimatedInteger(el, "targetX", 0); });
    accProto(G.SVGFEConvolveMatrixElement.prototype, "targetY", function (el) { return makeAnimatedInteger(el, "targetY", 0); });
    accProto(G.SVGFEConvolveMatrixElement.prototype, "preserveAlpha", function (el) { return makeAnimatedBoolean(el, "preserveAlpha"); });
    // Document.rootElement is the document's root <svg> (or null).
    var rootElementGet = function () { if (!this || this.nodeType !== 9) { throw new TypeError("Illegal invocation"); } var de = this.documentElement; return de && de.namespaceURI === SVG_NS ? de : null; };
    Object.defineProperty(rootElementGet, "name", { value: "get rootElement", configurable: true });
    Object.defineProperty(G.Document.prototype, "rootElement", { get: rootElementGet, enumerable: true, configurable: true });

    installTextContentProto(G.SVGTextContentElement.prototype);
    installMarkerProto(G.SVGMarkerElement.prototype);
    installSvgRootProto(G.SVGSVGElement.prototype);
    installAnimationProto(G.SVGAnimationElement.prototype);
  }

  function installTextContentProto(proto) {
    accProto(proto, "textLength", function (el) { return makeAnimatedLength(el, "textLength"); });
    accProto(proto, "lengthAdjust", function (el) { return makeEnumProp(el, "lengthAdjust", LENGTHADJUST_MAP, "spacing"); });
    def(proto, "getNumberOfChars", function () { var t = this.textContent; return t == null ? 0 : String(t).length; });
    def(proto, "getComputedTextLength", function () { return bbox(this).width; });
    def(proto, "getSubStringLength", function (i, n) { var len = this.getNumberOfChars(); var total = bbox(this).width; if (!len) { return 0; } return total * Math.max(0, Math.min(n, len - i)) / len; });
    def(proto, "getRotationOfChar", function (i) { var r = getAttr(this.__node, "rotate"); if (r == null || r === "") { return 0; } var list = vecParse(r); if (!list.length) { return 0; } return i < list.length ? list[i] : list[list.length - 1]; });
    def(proto, "getStartPositionOfChar", function (i) { var b = bbox(this); var len = this.getNumberOfChars() || 1; return makePoint(b.x + b.width * i / len, b.y + b.height); });
    def(proto, "getEndPositionOfChar", function (i) { var b = bbox(this); var len = this.getNumberOfChars() || 1; return makePoint(b.x + b.width * (i + 1) / len, b.y + b.height); });
    def(proto, "getExtentOfChar", function (i) { var b = bbox(this); var len = this.getNumberOfChars() || 1; return makeRectObj(b.x + b.width * i / len, b.y, b.width / len, b.height); });
    def(proto, "getCharNumAtPosition", function () { var p = arguments[0]; var b = bbox(this); var len = this.getNumberOfChars() || 1; if (!p || b.width === 0) { return -1; } var idx = Math.floor((p.x - b.x) / (b.width / len)); return idx >= 0 && idx < len ? idx : -1; });
    def(proto, "selectSubString", function (charnum, nchars) {});
  }

  function makeOrientAngle(el) {
    var node = el.__node;
    var isAuto = function () { var o = getAttr(node, "orient"); return o === "auto" || o === "auto-start-reverse"; };
    var angleNum = function () { var o = getAttr(node, "orient"); return (o == null || isAuto()) ? 0 : parseAngle(o).value; };
    var oa = Object.create(SVGAngle.prototype);
    oa.__g = angleNum;
    oa.__gt = function () { var o = getAttr(node, "orient"); return (o == null || isAuto()) ? 1 : parseAngle(o).type; };
    oa.__s = function (v) { setAttr(node, "orient", String(v)); };
    var orientAngle = Object.create(globalThis.SVGAnimatedAngle.prototype);
    orientAngle.__base = oa;
    orientAngle.__anim = makeAngle(function () { return svgAnimNum(el, "orient", angleNum(), angleVec); });
    return orientAngle;
  }
  function makeOrientType(el) {
    var node = el.__node;
    var isAuto = function () { var o = getAttr(node, "orient"); return o === "auto" || o === "auto-start-reverse"; };
    var angleNum = function () { var o = getAttr(node, "orient"); return (o == null || isAuto()) ? 0 : parseAngle(o).value; };
    var val = function () { var o = getAttr(node, "orient"); if (o == null) { return 2; } return isAuto() ? 1 : 2; };
    var orientType = Object.create(globalThis.SVGAnimatedEnumeration.prototype);
    Object.defineProperty(orientType, "baseVal", { get: val, set: function (v) { v = v | 0; if (v === 1) { setAttr(node, "orient", "auto"); } else if (v === 2) { setAttr(node, "orient", String(angleNum())); } }, enumerable: true });
    Object.defineProperty(orientType, "animVal", { get: val, enumerable: true });
    return orientType;
  }
  function installMarkerProto(proto) {
    accProto(proto, "orientAngle", makeOrientAngle);
    accProto(proto, "orientType", makeOrientType);
    def(proto, "setOrientToAuto", function () { setAttr(this.__node, "orient", "auto"); });
    def(proto, "setOrientToAngle", function (a) { setAttr(this.__node, "orient", String(a && a.value != null ? a.value : a)); });
  }

  function installSvgRootProto(proto) {
    def(proto, "pauseAnimations", function () { clock.paused = true; });
    def(proto, "unpauseAnimations", function () { clock.paused = false; });
    def(proto, "animationsPaused", function () { return !!clock.paused; });
    def(proto, "setCurrentTime", function (s) { var v = Number(s); clock.time = isFinite(v) ? v : 0; });
    def(proto, "getCurrentTime", function () { return clock.time; });
    def(proto, "suspendRedraw", function (maxWaitMilliseconds) { return 0; });
    def(proto, "unsuspendRedraw", function (id) {});
    def(proto, "unsuspendRedrawAll", function () {});
    def(proto, "forceRedraw", function () {});
    def(proto, "deselectAll", function () {});
    def(proto, "createSVGLength", function () { return makeMutableLength(); });
    def(proto, "createSVGNumber", function () { var N = Object.create(globalThis.SVGNumber.prototype); N.__v = 0; return N; });
    def(proto, "createSVGPoint", function () { return makePoint(0, 0); });
    def(proto, "createSVGRect", function () { return makeRectObj(0, 0, 0, 0); });
    def(proto, "createSVGMatrix", function () { return makeMatrix(1, 0, 0, 1, 0, 0); });
    def(proto, "createSVGTransform", function () { return makeTransform("matrix", [1, 0, 0, 1, 0, 0]); });
    def(proto, "createSVGAngle", function () { var st = { v: 0, u: 1 }; var A = Object.create(SVGAngle.prototype); A.__g = function () { return st.v; }; A.__gt = function () { return st.u; }; A.__s = function (x) { st.v = +x; }; return A; });
    def(proto, "createSVGTransformFromMatrix", function () { var m = arguments[0]; var T = this.createSVGTransform(); T.setMatrix(m); return T; });
    def(proto, "getElementById", function (id) { return this.ownerDocument.getElementById(id); });
    var elemViewBox = function (e) {
      var b = bbox(e), m = ctmOf(e);
      var xs = [b.x, b.x + b.width], ys = [b.y, b.y + b.height], mnx = Infinity, mny = Infinity, mxx = -Infinity, mxy = -Infinity;
      for (var i = 0; i < 2; i++) { for (var j = 0; j < 2; j++) { var px = m.a * xs[i] + m.c * ys[j] + m.e, py = m.b * xs[i] + m.d * ys[j] + m.f; mnx = Math.min(mnx, px); mny = Math.min(mny, py); mxx = Math.max(mxx, px); mxy = Math.max(mxy, py); } }
      return { x: mnx, y: mny, w: mxx - mnx, h: mxy - mny };
    };
    var rectsOverlap = function (b, r) { return b.x < r.x + r.width && b.x + b.w > r.x && b.y < r.y + r.height && b.y + b.h > r.y; };
    def(proto, "checkIntersection", function (element, rect) { return rectsOverlap(elemViewBox(element), rect); });
    def(proto, "checkEnclosure", function (element, rect) { var b = elemViewBox(element); return b.x >= rect.x && b.y >= rect.y && b.x + b.w <= rect.x + rect.width && b.y + b.h <= rect.y + rect.height; });
    def(proto, "getIntersectionList", function (rect, ref) {
      var root = ref || this, out = [];
      var GRAPHICS = { rect: 1, circle: 1, ellipse: 1, line: 1, polyline: 1, polygon: 1, path: 1, text: 1, image: 1, use: 1 };
      var SKIP = { defs: 1, clippath: 1, mask: 1, symbol: 1, marker: 1, pattern: 1, lineargradient: 1, radialgradient: 1, filter: 1 };
      (function walk(n) {
        var kids = n.childNodes;
        for (var i = 0; kids && i < kids.length; i++) {
          var c = kids[i];
          if (!c || c.nodeType !== 1 || c.namespaceURI !== SVG_NS) { continue; }
          var ln2 = c.__localName;
          if (SKIP[ln2]) { continue; }
          var disp = ""; try { disp = nativeGCS(c).getPropertyValue("display"); } catch (e) {}
          if (disp === "none") { continue; }
          var pe = getAttr(c.__node, "pointer-events"); try { var sp = nativeGCS(c).getPropertyValue("pointer-events"); if (sp) { pe = sp; } } catch (e2) {}
          if (GRAPHICS[ln2] && pe !== "none" && rectsOverlap(elemViewBox(c), rect)) { out.push(c); }
          if (ln2 !== "use") { walk(c); }
        }
      })(root);
      return out;
    });
    def(proto, "getEnclosureList", function (rect, ref) { return []; });
    Object.defineProperty(proto, "currentScale", { get: function () { return this.__curScale || 1; }, set: function (v) { this.__curScale = +v; }, enumerable: true, configurable: true });
    Object.defineProperty(proto, "currentTranslate", { get: function () { return this.__curTrans || (this.__curTrans = makePoint(0, 0)); }, enumerable: true, configurable: true });
  }

  // All SVG interface object names, so the finalize pass can give each the required interface-object
  // semantics (throw when called, locked `prototype`, `prototype.constructor`, parent-interface chain).
  var SVG_IFACE_NAMES = [
    "SVGLength", "SVGAngle", "SVGTransform", "SVGPreserveAspectRatio", "SVGNumber",
    "SVGTransformList", "SVGPointList", "SVGLengthList", "SVGNumberList", "SVGStringList",
    "SVGAnimatedLength", "SVGAnimatedLengthList", "SVGAnimatedNumber", "SVGAnimatedNumberList",
    "SVGAnimatedInteger", "SVGAnimatedEnumeration", "SVGAnimatedBoolean", "SVGAnimatedString",
    "SVGAnimatedRect", "SVGAnimatedAngle", "SVGAnimatedPreserveAspectRatio", "SVGAnimatedTransformList",
    "SVGUnitTypes", "SVGElement", "SVGGraphicsElement", "SVGSVGElement", "SVGGeometryElement",
    "SVGPathElement", "SVGRectElement", "SVGCircleElement", "SVGEllipseElement", "SVGLineElement",
    "SVGPolylineElement", "SVGPolygonElement", "SVGGElement", "SVGDefsElement", "SVGImageElement",
    "SVGUseElement", "SVGSwitchElement", "SVGAElement", "SVGForeignObjectElement", "SVGTextContentElement",
    "SVGTextPositioningElement", "SVGTextElement", "SVGTSpanElement", "SVGTextPathElement",
    "SVGGradientElement", "SVGLinearGradientElement", "SVGRadialGradientElement", "SVGStopElement",
    "SVGPatternElement", "SVGMarkerElement", "SVGClipPathElement", "SVGMaskElement", "SVGFilterElement",
    "SVGSymbolElement", "SVGViewElement", "SVGDescElement", "SVGTitleElement", "SVGMetadataElement",
    "SVGStyleElement", "SVGScriptElement", "SVGAnimationElement", "SVGAnimateElement", "SVGSetElement",
    "SVGAnimateTransformElement", "SVGAnimateMotionElement", "SVGMPathElement",
    "SVGFEBlendElement", "SVGFEColorMatrixElement", "SVGFEComponentTransferElement", "SVGFECompositeElement",
    "SVGFEConvolveMatrixElement", "SVGFEDiffuseLightingElement", "SVGFEDisplacementMapElement",
    "SVGFEDropShadowElement", "SVGFEFloodElement", "SVGFEGaussianBlurElement", "SVGFEImageElement",
    "SVGFEMergeElement", "SVGFEMorphologyElement", "SVGFEOffsetElement", "SVGFESpecularLightingElement",
    "SVGFETileElement", "SVGFETurbulenceElement", "SVGComponentTransferFunctionElement",
    "SVGFEFuncRElement", "SVGFEFuncGElement", "SVGFEFuncBElement", "SVGFEFuncAElement",
    "SVGFEPointLightElement", "SVGFESpotLightElement", "SVGFEDistantLightElement", "SVGFEMergeNodeElement",
    "TimeEvent", "SVGUseElementShadowRoot", "ShadowAnimation"
  ];
  // Members of the SVG value / SVGAnimated* types live on their PROTOTYPES, reading per-instance state
  // from `__`-prefixed backing slots (which idlharness ignores). Run once at bootstrap.
  function installValueProtos() {
    var G = globalThis;
    function A(proto, name, get, set) { var d = { get: get, enumerable: true, configurable: true }; if (set) { d.set = set; } Object.defineProperty(proto, name, d); }
    var RO = function () { throw new G.DOMException("read-only", "NoModificationAllowedError"); };

    A(SVGLength.prototype, "value", function () { return this.__g(); }, function (v) { if (this.__s) { this.__s(v); } else { RO(); } });
    A(SVGLength.prototype, "valueInSpecifiedUnits", function () { return parseLen(this.__gs()).num; }, function (v) { if (this.__sn) { this.__sn(v); } else if (this.__s) { this.__s(v); } else { RO(); } });
    A(SVGLength.prototype, "valueAsString", function () { var s = this.__gs(); return s == null || s === "" ? "0" : s; }, function (v) { if (this.__sstr) { this.__sstr(v); } else if (this.__s) { this.__s(v); } else { RO(); } });
    A(SVGLength.prototype, "unitType", function () { return this.__gt(); });
    def(SVGLength.prototype, "newValueSpecifiedUnits", function (u, v) { if (this.__st) { this.__st.u = u; this.__st.n = v; } else if (this.__s) { this.__s(v); } });
    def(SVGLength.prototype, "convertToSpecifiedUnits", function (u) { if (this.__st) { var px = this.__g(); this.__st.u = u; this.__st.n = pxToLen(px, u, this.__em || 16); } });

    A(G.SVGNumber.prototype, "value", function () { return this.__v || 0; }, function (v) { this.__v = +v; });

    A(SVGAngle.prototype, "value", function () { return this.__g(); }, function (v) { if (this.__s) { this.__s(v); } });
    A(SVGAngle.prototype, "valueInSpecifiedUnits", function () { return this.__g(); }, function (v) { if (this.__s) { this.__s(v); } });
    A(SVGAngle.prototype, "valueAsString", function () { return String(this.__g()); }, function (v) { if (this.__s) { this.__s(v); } });
    A(SVGAngle.prototype, "unitType", function () { return this.__gt ? this.__gt() : 2; });
    def(SVGAngle.prototype, "newValueSpecifiedUnits", function (u, v) { if (this.__s) { this.__s(v); } });
    def(SVGAngle.prototype, "convertToSpecifiedUnits", function (u) {});

    ["x", "y", "width", "height"].forEach(function (k, i) { A(G.SVGRect.prototype, k, function () { return this.__d ? this.__d[i] : (this.__get ? (this.__get()[i] || 0) : 0); }, function (v) { if (this.__d) { this.__d[i] = +v; } }); });
    ["x", "y"].forEach(function (k, i) { A(G.SVGPoint.prototype, k, function () { return this.__d ? this.__d[i] : 0; }, function (v) { if (this.__d) { this.__d[i] = +v; } }); });
    def(G.SVGPoint.prototype, "matrixTransform", function (m) { var x = this.x, y = this.y; return makePoint(m.a * x + m.c * y + m.e, m.b * x + m.d * y + m.f); });

    ["a", "b", "c", "d", "e", "f"].forEach(function (k, i) { A(G.SVGMatrix.prototype, k, function () { return this.__m[i]; }, function (v) { this.__m[i] = +v; }); });
    var mm = G.SVGMatrix.prototype;
    def(mm, "multiply", function (o) { var m = this.__m; return makeMatrix(m[0] * o.a + m[2] * o.b, m[1] * o.a + m[3] * o.b, m[0] * o.c + m[2] * o.d, m[1] * o.c + m[3] * o.d, m[0] * o.e + m[2] * o.f + m[4], m[1] * o.e + m[3] * o.f + m[5]); });
    def(mm, "translate", function (x, y) { return this.multiply(makeMatrix(1, 0, 0, 1, x, y)); });
    def(mm, "scale", function (s) { return this.multiply(makeMatrix(s, 0, 0, s, 0, 0)); });
    def(mm, "scaleNonUniform", function (sx, sy) { return this.multiply(makeMatrix(sx, 0, 0, sy, 0, 0)); });
    def(mm, "rotate", function (deg) { var a = deg * Math.PI / 180, c = Math.cos(a), s = Math.sin(a); return this.multiply(makeMatrix(c, s, -s, c, 0, 0)); });
    def(mm, "rotateFromVector", function (x, y) { var a = Math.atan2(y, x), c = Math.cos(a), s = Math.sin(a); return this.multiply(makeMatrix(c, s, -s, c, 0, 0)); });
    def(mm, "flipX", function () { return this.multiply(makeMatrix(-1, 0, 0, 1, 0, 0)); });
    def(mm, "flipY", function () { return this.multiply(makeMatrix(1, 0, 0, -1, 0, 0)); });
    def(mm, "skewX", function (deg) { return this.multiply(makeMatrix(1, 0, Math.tan(deg * Math.PI / 180), 1, 0, 0)); });
    def(mm, "skewY", function (deg) { return this.multiply(makeMatrix(1, Math.tan(deg * Math.PI / 180), 0, 1, 0, 0)); });
    def(mm, "inverse", function () { var m = this.__m, det = m[0] * m[3] - m[1] * m[2]; if (!det) { throw new G.DOMException("non-invertible", "InvalidStateError"); } var id = 1 / det; return makeMatrix(m[3] * id, -m[1] * id, -m[2] * id, m[0] * id, (m[2] * m[5] - m[3] * m[4]) * id, (m[1] * m[4] - m[0] * m[5]) * id); });

    A(SVGTransform.prototype, "type", function () { return this.__t.type; });
    A(SVGTransform.prototype, "angle", function () { return this.__t.angle; });
    A(SVGTransform.prototype, "matrix", function () { return this.__t.matrix; });
    def(SVGTransform.prototype, "setMatrix", function () { var m = arguments[0]; this.__t.matrix = m; this.__t.type = 1; this.__t.angle = 0; });
    def(SVGTransform.prototype, "setTranslate", function (x, y) { this.__t.matrix = transformMatrix("translate", [x, y]); this.__t.type = 2; this.__t.angle = 0; });
    def(SVGTransform.prototype, "setScale", function (x, y) { this.__t.matrix = transformMatrix("scale", [x, y]); this.__t.type = 3; this.__t.angle = 0; });
    def(SVGTransform.prototype, "setRotate", function (a, cx, cy) { this.__t.matrix = transformMatrix("rotate", [a, cx, cy]); this.__t.type = 4; this.__t.angle = a; });
    def(SVGTransform.prototype, "setSkewX", function (a) { this.__t.matrix = transformMatrix("skewx", [a]); this.__t.type = 5; this.__t.angle = a; });
    def(SVGTransform.prototype, "setSkewY", function (a) { this.__t.matrix = transformMatrix("skewy", [a]); this.__t.type = 6; this.__t.angle = a; });

    A(SVGPreserveAspectRatio.prototype, "align", function () { return this.__align; }, function (v) { this.__align = v | 0; });
    A(SVGPreserveAspectRatio.prototype, "meetOrSlice", function () { return this.__mos; }, function (v) { this.__mos = v | 0; });

    // Lists share a backing interface stored in `__l`: { len, get, clear, init, append, insert, remove, replace }.
    [G.SVGLengthList, G.SVGNumberList, G.SVGStringList, G.SVGPointList, G.SVGTransformList].forEach(function (C) {
      var p = C.prototype;
      A(p, "numberOfItems", function () { return this.__l.len(); });
      A(p, "length", function () { return this.__l.len(); });
      def(p, "getItem", function (i) { var n = this.__l.len(); if (i < 0 || i >= n) { throw new G.DOMException("index", "IndexSizeError"); } return this.__l.get(i); });
      def(p, "clear", function () { this.__l.clear(); });
      def(p, "initialize", function (x) { return this.__l.init(x); });
      def(p, "appendItem", function (x) { return this.__l.append(x); });
      def(p, "insertItemBefore", function (x, i) { return this.__l.insert(x, i); });
      def(p, "removeItem", function (i) { var n = this.__l.len(); if (i < 0 || i >= n) { throw new G.DOMException("index", "IndexSizeError"); } return this.__l.remove(i); });
      def(p, "replaceItem", function (x, i) { var n = this.__l.len(); if (i < 0 || i >= n) { throw new G.DOMException("index", "IndexSizeError"); } return this.__l.replace(x, i); });
    });
    def(G.SVGTransformList.prototype, "consolidate", function () { return this.__l.consolidate ? this.__l.consolidate() : null; });
    def(G.SVGTransformList.prototype, "createSVGTransformFromMatrix", function () { var m = arguments[0]; return makeTransform("matrix", [m.a, m.b, m.c, m.d, m.e, m.f]); });

    // SVGAnimated* wrappers: object-valued (baseVal/animVal are stored objects).
    [G.SVGAnimatedLength, G.SVGAnimatedRect, G.SVGAnimatedAngle, G.SVGAnimatedPreserveAspectRatio, G.SVGAnimatedTransformList, G.SVGAnimatedLengthList, G.SVGAnimatedNumberList].forEach(function (C) {
      A(C.prototype, "baseVal", function () { return this.__base; });
      A(C.prototype, "animVal", function () { return this.__anim; });
    });
    // SVGAnimated* wrappers: primitive-valued (baseVal/animVal via getter/setter closures).
    [G.SVGAnimatedNumber, G.SVGAnimatedInteger, G.SVGAnimatedBoolean, G.SVGAnimatedString, G.SVGAnimatedEnumeration].forEach(function (C) {
      A(C.prototype, "baseVal", function () { return this.__bget(); }, function (v) { this.__bset(v); });
      A(C.prototype, "animVal", function () { return this.__aget(); });
    });
  }

  // Give every SVG interface object the WebIDL interface-object semantics idlharness checks: calling
  // it (with or without `new`) throws TypeError, `prototype` is non-writable/-enumerable/-configurable
  // with `constructor` pointing back, and the object's [[Prototype]] is its parent interface object.
  function finalizeInterfaces() {
    var parentOf = {};
    SVG_IFACE_NAMES.forEach(function (n) { var f = globalThis[n]; if (typeof f === "function") { var p = Object.getPrototypeOf(f); parentOf[n] = (typeof p === "function") ? p.name : null; } });
    var rebuilt = {};
    SVG_IFACE_NAMES.forEach(function (n) {
      var old = globalThis[n]; if (typeof old !== "function") { return; }
      var proto = old.prototype;
      var fn = new Function('return function ' + n + '(){ throw new TypeError("Illegal constructor"); }')();
      // Statics are all WebIDL constants: enumerable, but non-writable / non-configurable, on both the
      // interface object and its prototype.
      Object.getOwnPropertyNames(old).forEach(function (k) {
        if (k === "length" || k === "name" || k === "prototype" || k === "arguments" || k === "caller") { return; }
        var d = { value: old[k], writable: false, enumerable: true, configurable: false };
        try { Object.defineProperty(fn, k, d); } catch (e) {}
        try { Object.defineProperty(proto, k, { value: old[k], writable: false, enumerable: true, configurable: false }); } catch (e) {}
      });
      fn.prototype = proto;
      Object.defineProperty(proto, "constructor", { value: fn, writable: true, enumerable: false, configurable: true });
      Object.defineProperty(globalThis, n, { value: fn, writable: true, enumerable: false, configurable: true });
      rebuilt[n] = fn;
    });
    SVG_IFACE_NAMES.forEach(function (n) {
      var fn = globalThis[n]; if (typeof fn !== "function") { return; }
      var pn = parentOf[n];
      var parent = pn ? (rebuilt[pn] || globalThis[pn]) : null;
      if (parent) { try { Object.setPrototypeOf(fn, parent); } catch (e) {} }
      try { Object.defineProperty(fn, "prototype", { writable: false, enumerable: false, configurable: false }); } catch (e) {}
    });
  }

  // Apply the per-member WebIDL semantics idlharness checks across every SVG interface prototype:
  // members are enumerable; accessor/operation functions carry their proper name and length; and each
  // throws a TypeError when invoked on the bare interface prototype (the brand check). The prototype
  // also gets a Symbol.toStringTag so the class string is "[object <Interface>]".
  function enumerateProtoMembers() {
    function rename(fn, nm, len) {
      try { Object.defineProperty(fn, "name", { value: nm, configurable: true }); } catch (e) {}
      if (len != null) { try { Object.defineProperty(fn, "length", { value: len, configurable: true }); } catch (e2) {} }
    }
    SVG_IFACE_NAMES.forEach(function (n) {
      var C = globalThis[n]; if (typeof C !== "function") { return; }
      var proto = C.prototype;
      try { Object.defineProperty(proto, Symbol.toStringTag, { value: n, writable: false, enumerable: false, configurable: true }); } catch (e) {}
      Object.getOwnPropertyNames(proto).forEach(function (k) {
        if (k === "constructor" || k.indexOf("__") === 0) { return; }
        var d = Object.getOwnPropertyDescriptor(proto, k); if (!d || !d.configurable) { return; }
        // Brand check: a member is only callable on a genuine instance of its interface — i.e. the
        // interface prototype must be in the receiver's chain (false for the bare prototype and for
        // objects of a different interface).
        if (d.get || d.set) {
          if (d.get) { var g0 = d.get; d.get = function () { if (!proto.isPrototypeOf(this)) { throw new TypeError("Illegal invocation"); } return g0.call(this); }; rename(d.get, "get " + k, 0); }
          if (d.set) { var s0 = d.set; d.set = function (v) { if (!proto.isPrototypeOf(this)) { throw new TypeError("Illegal invocation"); } return s0.call(this, v); }; rename(d.set, "set " + k, 1); }
        } else if (typeof d.value === "function") {
          var f0 = d.value, ln = f0.length;
          d.value = function () { if (!proto.isPrototypeOf(this)) { throw new TypeError("Illegal invocation"); } if (arguments.length < ln) { throw new TypeError("Not enough arguments"); } return f0.apply(this, arguments); };
          rename(d.value, k, ln);
        }
        d.enumerable = true;
        try { Object.defineProperty(proto, k, d); } catch (e3) {}
      });
    });
  }

  function installAnimationProto(proto) {
    def(proto, "getStartTime", function () { return parseBegin(getAttr(this.__node, "begin")); });
    def(proto, "getCurrentTime", function () { return clock.time; });
    def(proto, "getSimpleDuration", function () { var d = parseClock(getAttr(this.__node, "dur")); if (d == null) { throw new globalThis.DOMException("no simple duration", "NotSupportedError"); } return d; });
    def(proto, "beginElement", function () {});
    def(proto, "beginElementAt", function (offset) {});
    def(proto, "endElement", function () {});
    def(proto, "endElementAt", function (offset) {});
    Object.defineProperty(proto, "targetElement", {
      get: function () {
        var href = getAttr(this.__node, "href"); if (href == null) { href = getAttr(this.__node, "xlink:href"); }
        if (href && href.charAt(0) === "#") { return this.ownerDocument.getElementById(href.slice(1)); }
        return this.parentNode && this.parentNode.nodeType === 1 ? this.parentNode : null;
      }, configurable: true, enumerable: true
    });
    ["onbegin", "onend", "onrepeat"].forEach(function (h) {
      Object.defineProperty(proto, h, {
        get: function () { return (this.__eh && this.__eh[h]) || null; },
        set: function (v) { (this.__eh || (this.__eh = {}))[h] = typeof v === "function" ? v : null; },
        enumerable: true, configurable: true
      });
    });
  }

  // -------------------------------------------------------------------------------------------
  // Geometry: path/shape outlines, length, point-at-length, bounding box.
  // -------------------------------------------------------------------------------------------
  function gnum(el, attr, dflt) { var v = getAttr(el.__node, attr); if (v == null || v === "") { return dflt || 0; } return parseLen(v).value; }

  // Parse a path `d` string into flattened contours: [{pts:[{x,y}...], closed}]. Curves are
  // sampled; arcs are converted to their center parameterization and sampled.
  function parsePathD(d) {
    var contours = [], cur = null, sx = 0, sy = 0, x = 0, y = 0, px = 0, py = 0, prevCmd = "";
    var toks = String(d).match(/[a-zA-Z]|[-+]?(?:\d*\.\d+|\d+\.?)(?:[eE][-+]?\d+)?/g) || [];
    var i = 0;
    function nextNum() { return parseFloat(toks[i++]); }
    function start(nx, ny) { sx = nx; sy = ny; cur = { pts: [{ x: nx, y: ny }], closed: false }; contours.push(cur); }
    function lineTo(nx, ny) { if (!cur) { start(x, y); } cur.pts.push({ x: nx, y: ny }); }
    function sampleCubic(x0, y0, x1, y1, x2, y2, x3, y3) { var N = 24; for (var k = 1; k <= N; k++) { var t = k / N, u = 1 - t; lineTo(u * u * u * x0 + 3 * u * u * t * x1 + 3 * u * t * t * x2 + t * t * t * x3, u * u * u * y0 + 3 * u * u * t * y1 + 3 * u * t * t * y2 + t * t * t * y3); } }
    function sampleQuad(x0, y0, x1, y1, x2, y2) { var N = 18; for (var k = 1; k <= N; k++) { var t = k / N, u = 1 - t; lineTo(u * u * x0 + 2 * u * t * x1 + t * t * x2, u * u * y0 + 2 * u * t * y1 + t * t * y2); } }
    while (i < toks.length) {
      var cmd = toks[i];
      if (/[a-zA-Z]/.test(cmd)) { i++; } else { cmd = prevCmd === "M" ? "L" : prevCmd === "m" ? "l" : prevCmd; }
      var rel = cmd >= "a";
      switch (cmd.toLowerCase()) {
        case "m": { var nx = nextNum() + (rel ? x : 0), ny = nextNum() + (rel ? y : 0); x = nx; y = ny; start(x, y); break; }
        case "l": { x = nextNum() + (rel ? x : 0); y = nextNum() + (rel ? y : 0); lineTo(x, y); break; }
        case "h": { x = nextNum() + (rel ? x : 0); lineTo(x, y); break; }
        case "v": { y = nextNum() + (rel ? y : 0); lineTo(x, y); break; }
        case "c": { var c1x = nextNum() + (rel ? x : 0), c1y = nextNum() + (rel ? y : 0), c2x = nextNum() + (rel ? x : 0), c2y = nextNum() + (rel ? y : 0), ex = nextNum() + (rel ? x : 0), ey = nextNum() + (rel ? y : 0); sampleCubic(x, y, c1x, c1y, c2x, c2y, ex, ey); px = c2x; py = c2y; x = ex; y = ey; break; }
        case "s": { var sc1x = (prevCmd.toLowerCase() === "c" || prevCmd.toLowerCase() === "s") ? 2 * x - px : x, sc1y = (prevCmd.toLowerCase() === "c" || prevCmd.toLowerCase() === "s") ? 2 * y - py : y, c2x2 = nextNum() + (rel ? x : 0), c2y2 = nextNum() + (rel ? y : 0), ex2 = nextNum() + (rel ? x : 0), ey2 = nextNum() + (rel ? y : 0); sampleCubic(x, y, sc1x, sc1y, c2x2, c2y2, ex2, ey2); px = c2x2; py = c2y2; x = ex2; y = ey2; break; }
        case "q": { var q1x = nextNum() + (rel ? x : 0), q1y = nextNum() + (rel ? y : 0), qex = nextNum() + (rel ? x : 0), qey = nextNum() + (rel ? y : 0); sampleQuad(x, y, q1x, q1y, qex, qey); px = q1x; py = q1y; x = qex; y = qey; break; }
        case "t": { var tq1x = (prevCmd.toLowerCase() === "q" || prevCmd.toLowerCase() === "t") ? 2 * x - px : x, tq1y = (prevCmd.toLowerCase() === "q" || prevCmd.toLowerCase() === "t") ? 2 * y - py : y, tex = nextNum() + (rel ? x : 0), tey = nextNum() + (rel ? y : 0); sampleQuad(x, y, tq1x, tq1y, tex, tey); px = tq1x; py = tq1y; x = tex; y = tey; break; }
        case "a": { var rx = nextNum(), ry = nextNum(), rot = nextNum(), laf = nextNum(), sf = nextNum(), ax = nextNum() + (rel ? x : 0), ay = nextNum() + (rel ? y : 0); sampleArc(x, y, rx, ry, rot, laf, sf, ax, ay, lineTo); x = ax; y = ay; break; }
        case "z": { if (cur) { cur.closed = true; cur.pts.push({ x: sx, y: sy }); } x = sx; y = sy; break; }
        default: i++;
      }
      prevCmd = cmd;
    }
    return contours;
  }
  function sampleArc(x0, y0, rx, ry, rotDeg, laf, sf, x1, y1, lineTo) {
    if (rx === 0 || ry === 0) { lineTo(x1, y1); return; }
    rx = Math.abs(rx); ry = Math.abs(ry);
    var phi = rotDeg * Math.PI / 180, cosp = Math.cos(phi), sinp = Math.sin(phi);
    var dx = (x0 - x1) / 2, dy = (y0 - y1) / 2;
    var x1p = cosp * dx + sinp * dy, y1p = -sinp * dx + cosp * dy;
    var lam = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry);
    if (lam > 1) { var s = Math.sqrt(lam); rx *= s; ry *= s; }
    var sign = laf === sf ? -1 : 1;
    var num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p;
    var den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    var co = sign * Math.sqrt(Math.max(0, num / den));
    var cxp = co * rx * y1p / ry, cyp = -co * ry * x1p / rx;
    var cx = cosp * cxp - sinp * cyp + (x0 + x1) / 2, cy = sinp * cxp + cosp * cyp + (y0 + y1) / 2;
    function ang(ux, uy, vx, vy) { var dot = ux * vx + uy * vy, len = Math.sqrt((ux * ux + uy * uy) * (vx * vx + vy * vy)); var a = Math.acos(Math.max(-1, Math.min(1, dot / len))); if (ux * vy - uy * vx < 0) { a = -a; } return a; }
    var th0 = ang(1, 0, (x1p - cxp) / rx, (y1p - cyp) / ry);
    var dth = ang((x1p - cxp) / rx, (y1p - cyp) / ry, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    if (!sf && dth > 0) { dth -= 2 * Math.PI; } else if (sf && dth < 0) { dth += 2 * Math.PI; }
    var N = Math.max(2, Math.ceil(Math.abs(dth) / (Math.PI / 16)));
    for (var k = 1; k <= N; k++) { var th = th0 + dth * k / N; var ex = cosp * rx * Math.cos(th) - sinp * ry * Math.sin(th) + cx, ey = sinp * rx * Math.cos(th) + cosp * ry * Math.sin(th) + cy; lineTo(ex, ey); }
  }

  function shapeContours(el) {
    var ln = el.__localName;
    if (ln === "path") { return parsePathD(getAttr(el.__node, "d") || ""); }
    if (ln === "rect") { var x = gnum(el, "x"), y = gnum(el, "y"), w = gnum(el, "width"), h = gnum(el, "height"); return [{ pts: [{ x: x, y: y }, { x: x + w, y: y }, { x: x + w, y: y + h }, { x: x, y: y + h }, { x: x, y: y }], closed: true }]; }
    if (ln === "line") { return [{ pts: [{ x: gnum(el, "x1"), y: gnum(el, "y1") }, { x: gnum(el, "x2"), y: gnum(el, "y2") }], closed: false }]; }
    if (ln === "circle" || ln === "ellipse") { var cx = gnum(el, "cx"), cy = gnum(el, "cy"), rx = ln === "circle" ? gnum(el, "r") : gnum(el, "rx"), ry = ln === "circle" ? gnum(el, "r") : gnum(el, "ry"); var pts = []; var M = 256; for (var k = 0; k <= M; k++) { var t = 2 * Math.PI * k / M; pts.push({ x: cx + rx * Math.cos(t), y: cy + ry * Math.sin(t) }); } return [{ pts: pts, closed: true }]; }
    if (ln === "polyline" || ln === "polygon") { var nums = vecParse(getAttr(el.__node, "points") || ""); var p = []; for (var j = 0; j + 1 < nums.length; j += 2) { p.push({ x: nums[j], y: nums[j + 1] }); } if (ln === "polygon" && p.length) { p.push({ x: p[0].x, y: p[0].y }); } return [{ pts: p, closed: ln === "polygon" }]; }
    return [];
  }
  function totalLength(el) {
    var ln = el.__localName;
    if (ln === "rect") { return 2 * (gnum(el, "width") + gnum(el, "height")); }
    if (ln === "circle") { return 2 * Math.PI * gnum(el, "r"); }
    if (ln === "ellipse") { var a = gnum(el, "rx"), b = gnum(el, "ry"); return Math.PI * (3 * (a + b) - Math.sqrt((3 * a + b) * (a + 3 * b))); }
    if (ln === "line") { return Math.hypot(gnum(el, "x2") - gnum(el, "x1"), gnum(el, "y2") - gnum(el, "y1")); }
    var total = 0; var cs = shapeContours(el);
    for (var i = 0; i < cs.length; i++) { var pts = cs[i].pts; for (var k = 1; k < pts.length; k++) { total += Math.hypot(pts[k].x - pts[k - 1].x, pts[k].y - pts[k - 1].y); } }
    return total;
  }
  function pointAtLength(el, len) {
    var cs = shapeContours(el); var segs = [];
    for (var i = 0; i < cs.length; i++) { var pts = cs[i].pts; for (var k = 1; k < pts.length; k++) { segs.push([pts[k - 1], pts[k]]); } }
    if (!segs.length) { return makePoint(0, 0); }
    var tot = totalLength(el);
    if (len < 0) { len = 0; } if (len > tot) { len = tot; }
    var acc = 0;
    for (var s = 0; s < segs.length; s++) { var a = segs[s][0], b = segs[s][1], d = Math.hypot(b.x - a.x, b.y - a.y); if (acc + d >= len || s === segs.length - 1) { var f = d > 0 ? (len - acc) / d : 0; return makePoint(a.x + f * (b.x - a.x), a.y + f * (b.y - a.y)); } acc += d; }
    var last = segs[segs.length - 1][1]; return makePoint(last.x, last.y);
  }
  // The CTM mapping `el`'s user space to its nearest viewport (the enclosing <svg>): the product of
  // ancestor `transform`s, then the viewport's viewBox→viewport transform.
  function ctmOf(el) {
    var m = makeMatrix(1, 0, 0, 1, 0, 0), cur = el;
    while (cur && cur.namespaceURI === SVG_NS && cur.__localName && cur.__localName !== "svg") {
      var tl = parseTransformList(getAttr(cur.__node, "transform") || "");
      for (var i = tl.length - 1; i >= 0; i--) { m = tl[i].matrix.multiply(m); }
      cur = cur.parentNode;
    }
    if (cur && cur.__localName === "svg") {
      var vb = getAttr(cur.__node, "viewBox");
      if (vb) {
        var v = vecParse(vb);
        var r = cur.__node, rect = (typeof __rect === "function") ? __rect(r) : null;
        var vw = rect && rect.width ? rect.width : gnum(cur, "width") || (v[2] || 0);
        var vh = rect && rect.height ? rect.height : gnum(cur, "height") || (v[3] || 0);
        if (v.length === 4 && v[2] > 0 && v[3] > 0 && vw > 0 && vh > 0) {
          var s = Math.min(vw / v[2], vh / v[3]);
          m = makeMatrix(s, 0, 0, s, -v[0] * s, -v[1] * s).multiply(m);
        }
      }
    }
    return m;
  }

  var CONTAINER_TAGS = { g: 1, svg: 1, a: 1, switch: 1, symbol: 1, marker: 1, defs: 0 };
  function bbox(el) {
    var ln = el.__localName;
    if (ln === "use") {
      var href = getAttr(el.__node, "href"); if (href == null) { href = getAttr(el.__node, "xlink:href"); }
      if (href && href.charAt(0) === "#") {
        var tgt = el.ownerDocument.getElementById(href.slice(1));
        if (tgt && tgt.__node !== el.__node) { var tb = bbox(tgt); return makeRectObj(tb.x + gnum(el, "x"), tb.y + gnum(el, "y"), tb.width, tb.height); }
      }
      return makeRectObj(0, 0, 0, 0);
    }
    if (ln === "text" || ln === "tspan" || ln === "tref" || ln === "textpath") {
      // Approximate text extent from the font metrics (exact for the Ahem test font: 1em advance,
      // 0.8em ascent, 0.2em descent).
      var fs = 16;
      try { fs = parseFloat(nativeGCS(el).getPropertyValue("font-size")) || 16; } catch (e) {}
      var tx = gnum(el, "x"), ty = gnum(el, "y");
      var txt = el.textContent == null ? "" : String(el.textContent);
      return makeRectObj(tx, ty - 0.8 * fs, txt.length * fs, fs);
    }
    if (ln === "rect") { return makeRectObj(gnum(el, "x"), gnum(el, "y"), gnum(el, "width"), gnum(el, "height")); }
    if (ln === "circle") { var r = gnum(el, "r"); return makeRectObj(gnum(el, "cx") - r, gnum(el, "cy") - r, 2 * r, 2 * r); }
    if (ln === "ellipse") { var rx = gnum(el, "rx"), ry = gnum(el, "ry"); return makeRectObj(gnum(el, "cx") - rx, gnum(el, "cy") - ry, 2 * rx, 2 * ry); }
    if (ln === "line") { var x1 = gnum(el, "x1"), y1 = gnum(el, "y1"), x2 = gnum(el, "x2"), y2 = gnum(el, "y2"); return makeRectObj(Math.min(x1, x2), Math.min(y1, y2), Math.abs(x2 - x1), Math.abs(y2 - y1)); }
    if (GEOM_TAGS[ln]) {
      var cs = shapeContours(el); var mnx = Infinity, mny = Infinity, mxx = -Infinity, mxy = -Infinity;
      for (var i = 0; i < cs.length; i++) { var pts = cs[i].pts; for (var k = 0; k < pts.length; k++) { mnx = Math.min(mnx, pts[k].x); mny = Math.min(mny, pts[k].y); mxx = Math.max(mxx, pts[k].x); mxy = Math.max(mxy, pts[k].y); } }
      if (!isFinite(mnx)) { return makeRectObj(0, 0, 0, 0); }
      return makeRectObj(mnx, mny, mxx - mnx, mxy - mny);
    }
    // Container: union of children's bboxes, each mapped through the child's own transform.
    if (CONTAINER_TAGS[ln]) {
      var minx = Infinity, miny = Infinity, maxx = -Infinity, maxy = -Infinity;
      var kids = el.childNodes;
      for (var c = 0; c < (kids ? kids.length : 0); c++) {
        var ch = kids[c];
        if (!ch || ch.nodeType !== 1 || ch.namespaceURI !== SVG_NS) { continue; }
        var cln = ch.__localName;
        if (!GEOM_TAGS[cln] && !CONTAINER_TAGS[cln] && cln !== "text" && cln !== "image" && cln !== "use") { continue; }
        var b = bbox(ch);
        if (b.width === 0 && b.height === 0 && !GEOM_TAGS[cln]) { continue; }
        var tl = parseTransformList(getAttr(ch.__node, "transform") || "");
        var m = makeMatrix(1, 0, 0, 1, 0, 0);
        for (var ti = 0; ti < tl.length; ti++) { m = m.multiply(tl[ti].matrix); }
        var corners = [[b.x, b.y], [b.x + b.width, b.y], [b.x, b.y + b.height], [b.x + b.width, b.y + b.height]];
        for (var q = 0; q < 4; q++) { var px = m.a * corners[q][0] + m.c * corners[q][1] + m.e, py = m.b * corners[q][0] + m.d * corners[q][1] + m.f; minx = Math.min(minx, px); miny = Math.min(miny, py); maxx = Math.max(maxx, px); maxy = Math.max(maxy, py); }
      }
      if (!isFinite(minx)) { return makeRectObj(0, 0, 0, 0); }
      return makeRectObj(minx, miny, maxx - minx, maxy - miny);
    }
    return makeRectObj(0, 0, 0, 0);
  }
  function makePoint(x, y) { var P = Object.create(globalThis.SVGPoint.prototype); P.__d = [x, y]; return P; }
  function makeRectObj(x, y, w, h) { var R = Object.create(globalThis.SVGRect.prototype); R.__d = [x, y, w, h]; return R; }

  // Parse a `d` string into SVGPathData-style segments [{type, values}].
  var PATH_ARGS = { m: 2, l: 2, h: 1, v: 1, c: 6, s: 4, q: 4, t: 2, a: 7, z: 0 };
  function parsePathDataStr(d) {
    var out = [];
    var toks = String(d).match(/[a-zA-Z]|[-+]?(?:\d*\.\d+|\d+\.?)(?:[eE][-+]?\d+)?/g) || []; var i = 0, prev = "";
    while (i < toks.length) {
      var t = toks[i]; var cmd;
      if (/[a-zA-Z]/.test(t)) { cmd = t; i++; } else { cmd = (prev === "M") ? "L" : (prev === "m") ? "l" : prev; if (!cmd) { i++; continue; } }
      var n = PATH_ARGS[cmd.toLowerCase()]; if (n == null) { continue; }
      var vals = []; for (var k = 0; k < n; k++) { vals.push(parseFloat(toks[i++])); }
      out.push({ type: cmd, values: vals }); prev = cmd;
    }
    return out;
  }
  function getPathData(el) { return parsePathDataStr(getAttr(el.__node, "d") || ""); }

  // SVGPathSeg-style objects (the deprecated pathSegList API, used by path-animation tests). We do
  // not expose a global SVGPathSeg interface (historical.html requires it stay removed) — just plain
  // objects with `pathSegTypeAsLetter` and the per-command coordinate fields.
  var PATH_FIELDS = {
    M: ["x", "y"], L: ["x", "y"], C: ["x1", "y1", "x2", "y2", "x", "y"], Q: ["x1", "y1", "x", "y"],
    S: ["x2", "y2", "x", "y"], T: ["x", "y"], A: ["r1", "r2", "angle", "largeArcFlag", "sweepFlag", "x", "y"],
    H: ["x"], V: ["y"], Z: []
  };
  function segToObj(seg) {
    var o = { pathSegTypeAsLetter: seg.type };
    var f = PATH_FIELDS[seg.type.toUpperCase()] || [];
    for (var i = 0; i < f.length; i++) { o[f[i]] = seg.values[i]; }
    return o;
  }
  function segListFromString(d) { return parsePathDataStr(d).map(segToObj); }

  // Normalize a segment list to absolute coordinates (uppercase commands), tracking the current
  // point and subpath start. Path `d` interpolation requires both endpoints in the same coordinate
  // mode; browsers normalize to absolute first.
  function normalizeToAbsolute(segs) {
    var out = [], cx = 0, cy = 0, sx = 0, sy = 0;
    for (var i = 0; i < segs.length; i++) {
      var s = segs[i], rel = s.type >= "a", v = s.values, U = s.type.toUpperCase();
      switch (U) {
        case "M": { var x = rel ? cx + v[0] : v[0], y = rel ? cy + v[1] : v[1]; cx = x; cy = y; sx = x; sy = y; out.push({ type: "M", values: [x, y] }); break; }
        case "L": case "T": { var lx = rel ? cx + v[0] : v[0], ly = rel ? cy + v[1] : v[1]; cx = lx; cy = ly; out.push({ type: U, values: [lx, ly] }); break; }
        case "H": { var hx = rel ? cx + v[0] : v[0]; cx = hx; out.push({ type: "H", values: [hx] }); break; }
        case "V": { var vy = rel ? cy + v[0] : v[0]; cy = vy; out.push({ type: "V", values: [vy] }); break; }
        case "C": { var c = [rel ? cx + v[0] : v[0], rel ? cy + v[1] : v[1], rel ? cx + v[2] : v[2], rel ? cy + v[3] : v[3], rel ? cx + v[4] : v[4], rel ? cy + v[5] : v[5]]; cx = c[4]; cy = c[5]; out.push({ type: "C", values: c }); break; }
        case "S": case "Q": { var q = [rel ? cx + v[0] : v[0], rel ? cy + v[1] : v[1], rel ? cx + v[2] : v[2], rel ? cy + v[3] : v[3]]; cx = q[2]; cy = q[3]; out.push({ type: U, values: q }); break; }
        case "A": { var ax = rel ? cx + v[5] : v[5], ay = rel ? cy + v[6] : v[6]; out.push({ type: "A", values: [v[0], v[1], v[2], v[3], v[4], ax, ay] }); cx = ax; cy = ay; break; }
        case "Z": { cx = sx; cy = sy; out.push({ type: "Z", values: [] }); break; }
        default: out.push({ type: U, values: v.slice() });
      }
    }
    return out;
  }

  // Add per-coordinate b*scale onto a (segment lists must share command structure).
  function addScaledSegs(a, b, scale) {
    if (!structureMatches(a, b)) { return a; }
    return a.map(function (s, i) { return { type: s.type, values: s.values.map(function (v, k) { return v + scale * b[i].values[k]; }) }; });
  }
  function lerpSegs(a, b, f) {
    if (!structureMatches(a, b)) { return f < 0.5 ? a : b; }
    return a.map(function (s, i) { return { type: s.type, values: s.values.map(function (v, k) { return v + f * (b[i].values[k] - v); }) }; });
  }
  function structureMatches(a, b) {
    if (a.length !== b.length) { return false; }
    for (var i = 0; i < a.length; i++) { if (a[i].type !== b[i].type) { return false; } }
    return true;
  }
  // The animated `d` segments at the current time (base when no `d` animation is active).
  function pathAnimSegs(el) {
    var node = el.__node;
    var baseSegs = parsePathDataStr(getAttr(node, "d") || "");
    var anims = collectAnimations(el, "d");
    if (!anims.length) { return baseSegs; }
    var t = currentTime(); var segs = baseSegs;
    for (var i = 0; i < anims.length; i++) {
      var a = anims[i]; var tm = animTiming(a, t); if (tm == null) { continue; }
      var ga = function (nm) { var v = getAttr(a.__node, nm); return v == null ? null : v; };
      var from = ga("from"), to = ga("to"), by = ga("by"), values = ga("values");
      var calc = ga("calcMode") || "linear";
      if (a.__localName === "set") { calc = "discrete"; }
      if (values != null) {
        var lists = splitList(values).map(parsePathDataStr);
        if (!lists.length) { continue; }
        segs = interpSegLists(lists, tm.fraction, calc);
      } else if (by != null && from == null) {
        segs = addScaledSegs(baseSegs, parsePathDataStr(by), tm.fraction);
      } else if (from != null && by != null) {
        segs = addScaledSegs(parsePathDataStr(from), parsePathDataStr(by), tm.fraction);
      } else if (from != null && to != null) {
        var fa = normalizeToAbsolute(parsePathDataStr(from)), ta = normalizeToAbsolute(parsePathDataStr(to));
        segs = calc === "discrete" ? (tm.fraction < 1 ? fa : ta) : lerpSegs(fa, ta, tm.fraction);
      } else if (to != null) {
        var ba = normalizeToAbsolute(baseSegs), ta2 = normalizeToAbsolute(parsePathDataStr(to));
        segs = calc === "discrete" ? (tm.fraction < 1 ? ba : ta2) : lerpSegs(ba, ta2, tm.fraction);
      } else if (from != null) {
        segs = parsePathDataStr(from);
      }
    }
    return segs;
  }
  function interpSegLists(lists, f, calc) {
    var n = lists.length;
    if (n === 1) { return lists[0]; }
    if (calc === "discrete") { return lists[Math.min(Math.floor(f * n), n - 1)]; }
    var seg = Math.min(Math.floor(f * (n - 1)), n - 2);
    var local = f * (n - 1) - seg;
    return lerpSegs(lists[seg], lists[seg + 1], local);
  }
  function setPathData(el, segs) {
    var d = (segs || []).map(function (s) { return s.type + (s.values && s.values.length ? " " + s.values.join(" ") : ""); }).join(" ");
    setAttr(el.__node, "d", d);
  }

  var GEOM_TAGS = { path: 1, rect: 1, circle: 1, ellipse: 1, line: 1, polyline: 1, polygon: 1 };
  // Geometry/graphics methods live on the interface PROTOTYPES (so idlharness sees them inherited,
  // not as per-instance own properties). Installed once after the interfaces are defined.
  function installGeometryProtos() {
    var Geo = globalThis.SVGGeometryElement.prototype;
    var Gfx = globalThis.SVGGraphicsElement.prototype;
    var Path = globalThis.SVGPathElement.prototype;
    def(Geo, "getTotalLength", function () { return totalLength(this); });
    def(Geo, "getPointAtLength", function (len) { return pointAtLength(this, Number(len) || 0); });
    def(Geo, "isPointInFill", function () { return false; });
    def(Geo, "isPointInStroke", function () { return false; });
    Object.defineProperty(Geo, "pathLength", {
      get: function () {
        var el = this, c = Object.create(globalThis.SVGAnimatedNumber.prototype);
        Object.defineProperty(c, "baseVal", { get: function () { return gnum(el, "pathLength"); }, set: function (v) { setAttr(el.__node, "pathLength", String(v)); }, enumerable: true });
        Object.defineProperty(c, "animVal", { get: function () { return gnum(el, "pathLength"); }, enumerable: true });
        return c;
      }, configurable: true, enumerable: true
    });
    def(Path, "getPathData", function () { return getPathData(this); });
    def(Path, "setPathData", function (segs) { setPathData(this, segs); });
    Object.defineProperty(Path, "pathSegList", { get: function () { return parsePathDataStr(getAttr(this.__node, "d") || "").map(segToObj); }, configurable: true, enumerable: true });
    Object.defineProperty(Path, "animatedPathSegList", { get: function () { return pathAnimSegs(this).map(segToObj); }, configurable: true, enumerable: true });
    def(Gfx, "getBBox", function () { return bbox(this); });
    def(Gfx, "getCTM", function () { return ctmOf(this); });
    def(Gfx, "getScreenCTM", function () { return ctmOf(this); });
  }

  function makeMatrix(a, b, c, d, e, f) {
    var M = Object.create(globalThis.SVGMatrix.prototype);
    M.__m = [a, b, c, d, e, f];
    return M;
  }
  globalThis.__svgMakeMatrix = makeMatrix;

  // Hit-test SVG content: SVG shapes are rasterized, not laid out as boxes, so the engine's box-tree
  // hit-test lands on the <svg> element. Given a client point, descend the SVG subtree and return the
  // topmost (last-in-document, deepest) shape whose geometry's bbox contains the point — mapping each
  // shape's user-space bbox through its CTM into the <svg>'s local viewport (the same transform
  // checkIntersection uses), then into client coords via the <svg>'s border box. Returns null on miss.
  var SVG_SHAPE_TAGS = { rect: 1, circle: 1, ellipse: 1, line: 1, polyline: 1, polygon: 1, path: 1, text: 1, image: 1, use: 1 };
  globalThis.__svgHitTest = function (svgEl, clientX, clientY) {
    if (!svgEl || svgEl.__localName !== "svg") { return null; }
    var rect; try { rect = svgEl.getBoundingClientRect(); } catch (e) { return null; }
    if (!rect) { return null; }
    var lx = clientX - rect.left, ly = clientY - rect.top;
    function localBox(el) {
      var b = bbox(el), m = ctmOf(el);
      var xs = [b.x, b.x + b.width], ys = [b.y, b.y + b.height];
      var mnx = Infinity, mny = Infinity, mxx = -Infinity, mxy = -Infinity;
      for (var i = 0; i < 2; i++) {
        for (var j = 0; j < 2; j++) {
          var px = m.a * xs[i] + m.c * ys[j] + m.e, py = m.b * xs[i] + m.d * ys[j] + m.f;
          if (px < mnx) { mnx = px; } if (px > mxx) { mxx = px; }
          if (py < mny) { mny = py; } if (py > mxy) { mxy = py; }
        }
      }
      return { l: mnx, t: mny, r: mxx, b: mxy };
    }
    function visit(el) {
      var kids = el.children;
      if (kids) { for (var i = kids.length - 1; i >= 0; i--) { var h = visit(kids[i]); if (h) { return h; } } }
      if (el !== svgEl && el.namespaceURI === SVG_NS && SVG_SHAPE_TAGS[el.__localName]) {
        var box; try { box = localBox(el); } catch (e) { box = null; }
        if (box && lx >= box.l && lx < box.r && ly >= box.t && ly < box.b) { return el; }
      }
      return null;
    }
    return visit(svgEl);
  };

  // -------------------------------------------------------------------------------------------
  // Color resolution + getComputedStyle override for SVG presentation properties.
  // -------------------------------------------------------------------------------------------
  var NAMED = {
    black: [0, 0, 0], silver: [192, 192, 192], gray: [128, 128, 128], grey: [128, 128, 128],
    white: [255, 255, 255], maroon: [128, 0, 0], red: [255, 0, 0], purple: [128, 0, 128],
    fuchsia: [255, 0, 255], magenta: [255, 0, 255], green: [0, 128, 0], lime: [0, 255, 0],
    olive: [128, 128, 0], yellow: [255, 255, 0], navy: [0, 0, 128], blue: [0, 0, 255],
    teal: [0, 128, 128], aqua: [0, 255, 255], cyan: [0, 255, 255], orange: [255, 165, 0],
    pink: [255, 192, 203], brown: [165, 42, 42], gold: [255, 215, 0], indigo: [75, 0, 130],
    violet: [238, 130, 238], crimson: [220, 20, 60], coral: [255, 127, 80], salmon: [250, 128, 114],
    khaki: [240, 230, 140], orchid: [218, 112, 214], plum: [221, 160, 221], tan: [210, 180, 140],
    beige: [245, 245, 220], ivory: [255, 255, 240], lavender: [230, 230, 250], turquoise: [64, 224, 208],
    darkred: [139, 0, 0], darkgreen: [0, 100, 0], darkblue: [0, 0, 139], lightgray: [211, 211, 211],
    lightgrey: [211, 211, 211], darkgray: [169, 169, 169], darkgrey: [169, 169, 169],
    transparent: [0, 0, 0, 0]
  };
  function parseColor(s, el) {
    if (s == null) { return null; }
    s = String(s).trim();
    var lc = s.toLowerCase();
    if (lc === "none") { return null; }
    if (lc === "currentcolor") { return el ? colorOf(el, "color") : [0, 0, 0, 1]; }
    if (NAMED[lc]) { var n = NAMED[lc]; return [n[0], n[1], n[2], n.length > 3 ? n[3] : 1]; }
    var m;
    if ((m = /^#([0-9a-f]{3})$/i.exec(s))) { return [parseInt(m[1][0] + m[1][0], 16), parseInt(m[1][1] + m[1][1], 16), parseInt(m[1][2] + m[1][2], 16), 1]; }
    if ((m = /^#([0-9a-f]{4})$/i.exec(s))) { return [parseInt(m[1][0] + m[1][0], 16), parseInt(m[1][1] + m[1][1], 16), parseInt(m[1][2] + m[1][2], 16), parseInt(m[1][3] + m[1][3], 16) / 255]; }
    if ((m = /^#([0-9a-f]{6})$/i.exec(s))) { return [parseInt(m[1].slice(0, 2), 16), parseInt(m[1].slice(2, 4), 16), parseInt(m[1].slice(4, 6), 16), 1]; }
    if ((m = /^#([0-9a-f]{8})$/i.exec(s))) { return [parseInt(m[1].slice(0, 2), 16), parseInt(m[1].slice(2, 4), 16), parseInt(m[1].slice(4, 6), 16), parseInt(m[1].slice(6, 8), 16) / 255]; }
    if ((m = /^rgba?\(([^)]*)\)$/i.exec(s))) {
      var parts = m[1].split(/[\s,\/]+/).filter(function (x) { return x.length; });
      function ch(x) { x = x.trim(); if (x.indexOf("%") >= 0) { return Math.round(parseFloat(x) * 255 / 100); } return Math.round(parseFloat(x)); }
      return [ch(parts[0]), ch(parts[1]), ch(parts[2]), parts.length > 3 ? parseFloat(parts[3]) : 1];
    }
    return null;
  }
  function fmtColor(c) {
    if (!c) { return "none"; }
    var r = Math.max(0, Math.min(255, Math.round(c[0]))), g = Math.max(0, Math.min(255, Math.round(c[1]))), b = Math.max(0, Math.min(255, Math.round(c[2])));
    var a = c.length > 3 ? c[3] : 1;
    if (a >= 1) { return "rgb(" + r + ", " + g + ", " + b + ")"; }
    return "rgba(" + r + ", " + g + ", " + b + ", " + (Math.round(a * 100) / 100) + ")";
  }
  function svgParent(el) {
    try { var p = el.parentNode; return (p && p.nodeType === 1 && p.namespaceURI === SVG_NS) ? p : null; } catch (e) { return null; }
  }
  function rawStyleOrAttr(el, name) {
    try { var sv = el.style && el.style.getPropertyValue ? el.style.getPropertyValue(name) : ""; if (sv) { return sv; } } catch (e) {}
    var a = getAttr(el.__node, name);
    return a == null || a === "" ? null : a;
  }
  // The computed color of a paint/color property, resolving animation, inheritance and currentColor.
  var PAINT_INIT = { fill: [0, 0, 0, 1], stroke: null, "stop-color": [0, 0, 0, 1], color: [0, 0, 0, 1], "flood-color": [0, 0, 0, 1], "lighting-color": [255, 255, 255, 1] };
  var PAINT_INHERIT = { fill: true, stroke: true, color: true };
  function colorOf(el, name) {
    var anims = collectAnimations(el, name);
    var raw = rawStyleOrAttr(el, name);
    if (anims.length) {
      var base = raw != null ? (parseColor(raw, el) || [0, 0, 0, 1]) : (PAINT_INIT[name] || [0, 0, 0, 1]);
      var pf = function (s) { return parseColor(s, el) || [0, 0, 0, 1]; };
      var v = svgAnimVec(el, name, base, pf);
      return v;
    }
    if (raw == null || raw === "inherit") {
      var p = svgParent(el);
      if (p && (PAINT_INHERIT[name] || raw === "inherit")) { return colorOf(p, name); }
      return PAINT_INIT[name] || [0, 0, 0, 1];
    }
    if (raw.trim().toLowerCase() === "currentcolor") { return colorOf(el, "color"); }
    return parseColor(raw, el) || (PAINT_INIT[name] || [0, 0, 0, 1]);
  }
  // The computed value of a numeric presentation property (opacity etc.).
  var NUM_INIT = { opacity: 1, "fill-opacity": 1, "stroke-opacity": 1, "stop-opacity": 1, "stroke-width": 1 };
  var NUM_INHERIT = { "fill-opacity": true, "stroke-opacity": true, "stroke-width": true };
  function numOf(el, name) {
    var raw = rawStyleOrAttr(el, name);
    var base = raw != null && raw !== "inherit" ? parseLen(raw).value : null;
    if (base == null) {
      var p = svgParent(el);
      if (p && (NUM_INHERIT[name] || raw === "inherit")) { return numOf(p, name); }
      base = NUM_INIT[name] != null ? NUM_INIT[name] : 0;
    }
    return svgAnimNum(el, name, base);
  }

  var SVG_COLOR_PROPS = { color: 1, "stop-color": 1, "flood-color": 1, "lighting-color": 1 };
  var SVG_PAINT_PROPS = { fill: 1, stroke: 1 };
  var SVG_MARKER_PROPS = { "marker-start": 1, "marker-mid": 1, "marker-end": 1 };
  var SVG_NUM_PROPS = { opacity: 1, "fill-opacity": 1, "stroke-opacity": 1, "stop-opacity": 1, "stroke-width": 1 };
  // SVG keyword properties: [initial, inherited].
  var SVG_KEYWORD_PROPS = {
    "text-anchor": ["start", true], "text-decoration-style": ["solid", false], "text-decoration-line": ["none", false],
    "stroke-linecap": ["butt", true], "stroke-linejoin": ["miter", true], "fill-rule": ["nonzero", true],
    "clip-rule": ["nonzero", true], "color-interpolation": ["srgb", true], "color-interpolation-filters": ["linearrgb", true],
    "image-rendering": ["auto", true], "shape-rendering": ["auto", true], "text-rendering": ["auto", true],
    "paint-order": ["normal", true], "pointer-events": ["auto", true]
  };
  // camelCase aliases used for direct property access on the declaration.
  var CAMEL = { fill: "fill", stroke: "stroke", color: "color", opacity: "opacity", stopColor: "stop-color", floodColor: "flood-color", lightingColor: "lighting-color", fillOpacity: "fill-opacity", strokeOpacity: "stroke-opacity", stopOpacity: "stop-opacity", strokeWidth: "stroke-width", visibility: "visibility", textAnchor: "text-anchor", textDecorationLine: "text-decoration-line", textDecorationStyle: "text-decoration-style", textDecorationColor: "text-decoration-color", strokeLinecap: "stroke-linecap", strokeLinejoin: "stroke-linejoin", fillRule: "fill-rule", clipRule: "clip-rule", colorInterpolation: "color-interpolation", colorInterpolationFilters: "color-interpolation-filters", imageRendering: "image-rendering", shapeRendering: "shape-rendering", textRendering: "text-rendering", strokeMiterlimit: "stroke-miterlimit", markerStart: "marker-start", markerMid: "marker-mid", markerEnd: "marker-end", strokeDasharray: "stroke-dasharray", strokeDashoffset: "stroke-dashoffset", paintOrder: "paint-order", clipRule: "clip-rule", pointerEvents: "pointer-events" };
  function svgAbsUrl(u) { try { return new URL(u, document.baseURI).href; } catch (e) { return u; } }
  // fill / stroke <paint>: none | <color> | <url> [none|<color>]? — computed serialization.
  function paintComputed(el, name) {
    var initial = name === "fill" ? "rgb(0, 0, 0)" : "none";
    var raw = rawStyleOrAttr(el, name);
    if (raw == null) { var pp = svgParent(el); return pp ? paintComputed(pp, name) : initial; }
    var r = raw.trim(), lc = r.toLowerCase();
    if (lc === "inherit") { var pi = svgParent(el); return pi ? paintComputed(pi, name) : initial; }
    if (lc === "none") { return "none"; }
    if (lc === "currentcolor") { return fmtColor(nativeColor(el)); }
    if (/^url\(/i.test(r)) {
      var m = /^url\(\s*(?:"([^"]*)"|'([^']*)'|([^)\s]*))\s*\)\s*([\s\S]*)$/i.exec(r);
      if (m) {
        var u = m[1] != null ? m[1] : (m[2] != null ? m[2] : (m[3] || ""));
        var out = 'url("' + svgAbsUrl(u) + '")', fb = (m[4] || "").trim();
        if (fb) { out += " " + (fb.toLowerCase() === "none" ? "none" : (fb.toLowerCase() === "currentcolor" ? fmtColor(nativeColor(el)) : fmtColor(parseColor(fb, el) || [0, 0, 0, 1]))); }
        return out;
      }
    }
    var c = parseColor(r, el); return c ? fmtColor(c) : r;
  }
  // marker-start/mid/end: none | <url> (inherited, initial none).
  function markerComputed(el, name) {
    var raw = rawStyleOrAttr(el, name);
    if (raw == null) { var pm = svgParent(el); return pm ? markerComputed(pm, name) : "none"; }
    var r = raw.trim(), lc = r.toLowerCase();
    if (lc === "inherit") { var pi = svgParent(el); return pi ? markerComputed(pi, name) : "none"; }
    if (lc === "none") { return "none"; }
    var m = /^url\(\s*(?:"([^"]*)"|'([^']*)'|([^)\s]*))\s*\)/i.exec(r);
    if (m) { var u = m[1] != null ? m[1] : (m[2] != null ? m[2] : (m[3] || "")); return 'url("' + svgAbsUrl(u) + '")'; }
    return r;
  }
  function canonDecorationLine(v) {
    v = v.toLowerCase().trim();
    if (v === "none" || v === "spelling-error" || v === "grammar-error") { return v; }
    var order = ["underline", "overline", "line-through", "blink"], toks = v.split(/\s+/);
    var out = order.filter(function (o) { return toks.indexOf(o) >= 0; });
    return out.length ? out.join(" ") : "none";
  }
    // The element's cascaded `color` (includes `<style>`-rule colors that rawStyleOrAttr misses),
    // used to resolve `currentColor`.
  function nativeColor(el) {
    try { var c = nativeGCS(el).getPropertyValue("color"); var pc = parseColor(c, el); if (pc) { return pc; } } catch (e) {}
    return colorOf(el, "color");
  }
  function decoColor(el) {
    var raw = rawStyleOrAttr(el, "text-decoration-color");
    if (raw == null) { return nativeColor(el); } // initial currentColor
    var r = raw.trim().toLowerCase();
    // initial / unset / revert (not inherited) all resolve to the initial currentColor.
    if (r === "currentcolor" || r === "initial" || r === "unset" || r === "revert") { return nativeColor(el); }
    if (r === "inherit") {
      var p = svgParent(el);
      if (p) {
        var praw = rawStyleOrAttr(p, "text-decoration-color");
        var pr = praw == null ? "currentcolor" : praw.trim().toLowerCase();
        // currentColor (or another inherit) resolves against THIS element's color.
        if (pr === "currentcolor" || pr === "inherit") { return nativeColor(el); }
        return parseColor(praw, p) || [0, 0, 0, 1];
      }
      return nativeColor(el);
    }
    return parseColor(raw, el) || [0, 0, 0, 1];
  }
  // Per-property computed initial value (`i`) + whether it inherits (`h`). Used to resolve the
  // CSS-wide keywords initial/inherit/unset/revert.
  var SVG_PROP_META = {
    "fill": { i: "rgb(0, 0, 0)", h: true }, "stroke": { i: "none", h: true },
    "color": { i: "rgb(0, 0, 0)", h: true }, "stop-color": { i: "rgb(0, 0, 0)", h: false },
    "flood-color": { i: "rgb(0, 0, 0)", h: false }, "lighting-color": { i: "rgb(255, 255, 255)", h: false },
    "fill-opacity": { i: "1", h: true }, "stroke-opacity": { i: "1", h: true },
    "stop-opacity": { i: "1", h: false }, "opacity": { i: "1", h: false },
    "stroke-width": { i: "1px", h: true }, "stroke-miterlimit": { i: "4", h: true },
    "marker-start": { i: "none", h: true }, "marker-mid": { i: "none", h: true }, "marker-end": { i: "none", h: true }, "marker": { i: "none", h: true },
    "text-anchor": { i: "start", h: true }, "text-decoration-line": { i: "none", h: false },
    "text-decoration-style": { i: "solid", h: false }, "text-decoration-color": { i: "__cc__", h: false },
    "stroke-linecap": { i: "butt", h: true }, "stroke-linejoin": { i: "miter", h: true },
    "fill-rule": { i: "nonzero", h: true }, "clip-rule": { i: "nonzero", h: true },
    "color-interpolation": { i: "srgb", h: true }, "color-interpolation-filters": { i: "linearrgb", h: true },
    "image-rendering": { i: "auto", h: true }, "shape-rendering": { i: "auto", h: true }, "text-rendering": { i: "auto", h: true },
    "paint-order": { i: "normal", h: true }, "visibility": { i: "visible", h: true },
    "pointer-events": { i: "auto", h: true },
    "stroke-dasharray": { i: "none", h: true }, "stroke-dashoffset": { i: "0px", h: true },
    "x": { i: "0px", h: false }, "y": { i: "0px", h: false }, "cx": { i: "0px", h: false },
    "cy": { i: "0px", h: false }, "r": { i: "0px", h: false }, "rx": { i: "auto", h: false }, "ry": { i: "auto", h: false }
  };
  // Validate + minimally serialize paint-order (an invalid value computes to the initial `normal`).
  function canonPaintOrderJs(v) {
    v = String(v).toLowerCase().trim();
    if (v === "normal" || v === "") { return "normal"; }
    var toks = v.split(/\s+/), ok = { fill: 1, stroke: 1, markers: 1 }, seen = {}, def = ["fill", "stroke", "markers"];
    if (toks.length < 1 || toks.length > 3) { return "normal"; }
    for (var i = 0; i < toks.length; i++) { if (!ok[toks[i]] || seen[toks[i]]) { return "normal"; } seen[toks[i]] = 1; }
    var full = toks.slice();
    for (var d = 0; d < def.length; d++) { if (full.indexOf(def[d]) < 0) { full.push(def[d]); } }
    for (var k = 1; k <= 3; k++) {
      var rb = full.slice(0, k);
      for (var e = 0; e < def.length; e++) { if (rb.indexOf(def[e]) < 0) { rb.push(def[e]); } }
      if (rb.join(" ") === full.join(" ")) { return full.slice(0, k).join(" "); }
    }
    return full.join(" ");
  }
  // Resolve a stroke length token to its computed value (px, "P%", or "calc(P% + Xpx)") using the
  // element's font metrics and the viewport for em/vw/etc. via the shared calc engine.
  function svgLenCtx(el) {
    var fs = 16, rfs = 16;
    try { fs = parseFloat(nativeGCS(el).getPropertyValue("font-size")) || 16; } catch (e) {}
    try { rfs = parseFloat(nativeGCS(el.ownerDocument.documentElement).getPropertyValue("font-size")) || 16; } catch (e2) {}
    return { fs: fs, rfs: rfs, vw: globalThis.innerWidth || 0, vh: globalThis.innerHeight || 0 };
  }
  function computeStrokeLen(el, raw, nonneg) {
    if (!globalThis.__calc) { return /%\s*$/.test(raw) ? raw : (parseLen(raw).value + "px"); }
    var s = /^calc\(/i.test(raw) ? raw : "calc(" + raw + ")";
    var c = globalThis.__calc.compute(s, svgLenCtx(el));
    if (c == null) { return raw; }
    if (nonneg && /^-[0-9.]+px$/.test(c)) { return "0px"; } // non-negative length clamps to 0
    return c;
  }
  function svgInitial(el, name) {
    var m = SVG_PROP_META[name];
    if (!m) { return ""; }
    return m.i === "__cc__" ? fmtColor(nativeColor(el)) : m.i;
  }
  function svgComputed(el, name) {
    // Resolve the CSS-wide keywords (initial | inherit | unset | revert) when set explicitly.
    // (text-decoration-color has its own currentColor-aware resolution in decoColor.)
    var meta = name === "text-decoration-color" ? null : SVG_PROP_META[name];
    if (meta) {
      var rawk = rawStyleOrAttr(el, name);
      if (rawk != null) {
        var lck = rawk.trim().toLowerCase();
        if (lck === "initial") { return svgInitial(el, name); }
        if (lck === "inherit") { var pp = svgParent(el); return pp ? svgComputed(pp, name) : svgInitial(el, name); }
        if (lck === "unset" || lck === "revert") { var pq = svgParent(el); return (meta.h && pq) ? svgComputed(pq, name) : svgInitial(el, name); }
      }
    }
    if (name === "text-decoration-color") { return fmtColor(decoColor(el)); }
    if (SVG_PAINT_PROPS[name]) { return paintComputed(el, name); }
    if (SVG_MARKER_PROPS[name]) { return markerComputed(el, name); }
    // `marker` shorthand: the common longhand value, else "" (per CSSOM shorthand serialization).
    if (name === "marker") {
      var ms = markerComputed(el, "marker-start"), mm = markerComputed(el, "marker-mid"), me = markerComputed(el, "marker-end");
      return ms === mm && mm === me ? ms : "";
    }
    if (SVG_COLOR_PROPS[name]) { var c = colorOf(el, name); return c == null ? "none" : fmtColor(c); }
    if (name === "stroke-width") {
      var rw = rawStyleOrAttr(el, "stroke-width");
      if (rw == null) { var pw = svgParent(el); return pw ? svgComputed(pw, name) : "1px"; }
      return computeStrokeLen(el, rw.trim(), true);
    }
    if (SVG_NUM_PROPS[name]) {
      var nv = numOf(el, name);
      // <alpha-value> properties clamp to [0,1] in the computed value.
      if (name === "opacity" || name === "fill-opacity" || name === "stroke-opacity" || name === "stop-opacity") { nv = Math.max(0, Math.min(1, nv)); }
      return String(nv);
    }
    if (name === "stroke-miterlimit") {
      var rm = rawStyleOrAttr(el, "stroke-miterlimit");
      if (rm == null || rm === "inherit") { var pm = svgParent(el); return pm ? svgComputed(pm, "stroke-miterlimit") : "4"; }
      return String(parseFloat(rm));
    }
    if (name === "paint-order") {
      var rpo = rawStyleOrAttr(el, "paint-order");
      if (rpo == null) { var ppo = svgParent(el); return ppo ? svgComputed(ppo, name) : "normal"; }
      return canonPaintOrderJs(rpo);
    }
    // SVG geometry CSS properties: <length-percentage> resolved to px/% (x/y/cx/cy allow negatives;
    // r non-negative; rx/ry keep the `auto` keyword).
    if (name === "x" || name === "y" || name === "cx" || name === "cy") {
      var rxy = rawStyleOrAttr(el, name);
      return rxy == null ? "0px" : computeStrokeLen(el, rxy.trim(), false);
    }
    if (name === "r") {
      var rrad = rawStyleOrAttr(el, "r");
      return rrad == null ? "0px" : computeStrokeLen(el, rrad.trim(), true);
    }
    if (name === "rx" || name === "ry") {
      var rrx = rawStyleOrAttr(el, name);
      if (rrx == null || rrx.trim().toLowerCase() === "auto") { return "auto"; }
      return computeStrokeLen(el, rrx.trim(), true);
    }
    if (name === "stroke-dashoffset") {
      var rd = rawStyleOrAttr(el, "stroke-dashoffset");
      if (rd == null) { var pd = svgParent(el); return pd ? svgComputed(pd, name) : "0px"; }
      return computeStrokeLen(el, rd.trim());
    }
    if (name === "stroke-dasharray") {
      var ra = rawStyleOrAttr(el, "stroke-dasharray");
      if (ra == null) { var pa = svgParent(el); return pa ? svgComputed(pa, name) : "none"; }
      ra = ra.trim();
      if (ra.toLowerCase() === "none" || ra === "") { return "none"; }
      var dlist = (globalThis.__splitDashList ? globalThis.__splitDashList(ra) : ra.split(/[\s,]+/).filter(Boolean));
      return dlist.map(function (t) { return computeStrokeLen(el, t, true); }).join(", ");
    }
    if (SVG_KEYWORD_PROPS[name]) {
      var spec = SVG_KEYWORD_PROPS[name], raw2 = rawStyleOrAttr(el, name);
      if (raw2 == null || raw2 === "inherit") {
        var p2 = svgParent(el);
        if (p2 && (spec[1] || raw2 === "inherit")) { return svgComputed(p2, name); }
        return spec[0];
      }
      return name === "text-decoration-line" ? canonDecorationLine(raw2) : raw2.toLowerCase();
    }
    if (name === "visibility") {
      var raw3 = rawStyleOrAttr(el, "visibility");
      if (raw3 == null) { var p = svgParent(el); return p ? svgComputed(p, "visibility") : "visible"; }
      return raw3;
    }
    return null;
  }
  function svgHandles(kebab) { return !!SVG_PROP_META[kebab]; }
  var nativeGCS = globalThis.getComputedStyle;
  if (typeof nativeGCS === "function") {
    globalThis.getComputedStyle = function (el, pseudo) {
      var decl = nativeGCS.call(this, el, pseudo);
      if (!el || el.namespaceURI !== SVG_NS) { return decl; }
      return new Proxy(decl, {
        has: function (target, prop) {
          if (typeof prop === "string") {
            if (CAMEL[prop] && svgHandles(CAMEL[prop])) { return true; }
            if (svgHandles(prop)) { return true; }
          }
          return Reflect.has(target, prop);
        },
        get: function (target, prop) {
          if (typeof prop === "string") {
            if (prop === "getPropertyValue") {
              return function (n) { var v = svgComputed(el, String(n).toLowerCase()); return v != null ? v : target.getPropertyValue(n); };
            }
            if (CAMEL[prop]) { var cv = svgComputed(el, CAMEL[prop]); if (cv != null) { return cv; } }
            if (svgHandles(prop)) { var kv = svgComputed(el, prop); if (kv != null) { return kv; } }
          }
          var r = target[prop];
          return typeof r === "function" ? r.bind(target) : r;
        }
      });
    };
  }
  globalThis.__svgColorOf = function (el, name) { return fmtColor(colorOf(el, name)); };

  installValueProtos();
  installGeometryProtos();
  installSvgProtos();
  finalizeInterfaces();
  enumerateProtoMembers();
  // ShadowAnimation has a 2-argument constructor; restore its length after the finalize rebuild.
  try { Object.defineProperty(globalThis.ShadowAnimation, "length", { value: 2, configurable: true }); } catch (e) {}
  globalThis.__svgEnrich = svgEnrich;
})();
