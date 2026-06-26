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
    var C = { SVG_MARKERUNITS_UNKNOWN: 0, SVG_MARKERUNITS_USERSPACEONUSE: 1, SVG_MARKERUNITS_STROKEWIDTH: 2, SVG_MARKER_ORIENT_UNKNOWN: 0, SVG_MARKER_ORIENT_AUTO: 1, SVG_MARKER_ORIENT_ANGLE: 2 };
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
    feoffset: "SVGFEOffsetElement", feturbulence: "SVGFETurbulenceElement"
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
    Object.defineProperty(anim, "baseVal", { value: baseVal, enumerable: true });
    Object.defineProperty(anim, "animVal", { value: animVal, enumerable: true });
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

  // SVGRect (live, backed by a 4-number attribute) and SVGAnimatedRect (viewBox).
  function makeRect(getVec) {
    var R = Object.create(globalThis.SVGRect.prototype);
    ["x", "y", "width", "height"].forEach(function (k, i) {
      Object.defineProperty(R, k, { get: function () { return getVec()[i] || 0; }, set: function () {}, enumerable: true, configurable: true });
    });
    return R;
  }
  function makeAnimatedRect(el, attr) {
    var node = el.__node;
    function baseVec() { var s = getAttr(node, attr); return s == null ? [0, 0, 0, 0] : vecParse(s); }
    var anim = Object.create(globalThis.SVGAnimatedRect.prototype);
    Object.defineProperty(anim, "baseVal", { value: makeRect(baseVec), enumerable: true });
    Object.defineProperty(anim, "animVal", { value: makeRect(function () { return svgAnimVec(el, attr, baseVec()); }), enumerable: true });
    return anim;
  }

  // SVGNumberList / SVGLengthList (live) and their SVGAnimated* wrappers (x/y/dx/dy/rotate on text).
  function makeItemList(getVec, listProto, itemProto) {
    var L = Object.create(listProto.prototype);
    Object.defineProperty(L, "numberOfItems", { get: function () { return getVec().length; }, enumerable: true, configurable: true });
    Object.defineProperty(L, "length", { get: function () { return getVec().length; }, enumerable: true, configurable: true });
    def(L, "getItem", function (i) {
      var v = getVec(); if (i < 0 || i >= v.length) { throw new globalThis.DOMException("index", "IndexSizeError"); }
      var it = Object.create(itemProto.prototype); it.value = v[i]; it.valueInSpecifiedUnits = v[i]; it.valueAsString = String(v[i]); it.unitType = 1; return it;
    });
    def(L, "initialize", function () {}); def(L, "clear", function () {}); def(L, "appendItem", function (x) { return x; });
    return L;
  }
  function makeAnimatedItemList(el, attr, listProto, itemProto) {
    var node = el.__node;
    function baseVec() { var s = getAttr(node, attr); return s == null || s === "" ? [] : vecParse(s); }
    var anim = Object.create((listProto === globalThis.SVGNumberList ? globalThis.SVGAnimatedNumberList : globalThis.SVGAnimatedLengthList).prototype);
    Object.defineProperty(anim, "baseVal", { value: makeItemList(baseVec, listProto, itemProto), enumerable: true });
    Object.defineProperty(anim, "animVal", { value: makeItemList(function () { return svgAnimVec(el, attr, baseVec()); }, listProto, itemProto), enumerable: true });
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
  function makeAngle(getVal) { var A = Object.create(SVGAngle.prototype); Object.defineProperty(A, "value", { get: getVal, enumerable: true }); Object.defineProperty(A, "valueInSpecifiedUnits", { get: getVal, enumerable: true }); Object.defineProperty(A, "valueAsString", { get: function () { return String(getVal()); }, enumerable: true }); A.unitType = 2; return A; }
  function makeAnimatedAngle(el, attr) {
    var node = el.__node;
    function base() { var o = getAttr(node, attr); if (o == null || o === "auto" || o === "auto-start-reverse") { return 0; } return parseAngle(o).value; }
    var anim = Object.create(globalThis.SVGAnimatedAngle.prototype);
    Object.defineProperty(anim, "baseVal", { value: makeAngle(base), enumerable: true });
    Object.defineProperty(anim, "animVal", { value: makeAngle(function () { return svgAnimNum(el, attr, base(), angleVec); }), enumerable: true });
    return anim;
  }
  function makeAnimatedEnum(getBase) {
    var anim = Object.create(globalThis.SVGAnimatedEnumeration.prototype);
    Object.defineProperty(anim, "baseVal", { get: getBase, set: function () {}, enumerable: true });
    Object.defineProperty(anim, "animVal", { get: getBase, enumerable: true });
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
    Object.defineProperty(anim, "baseVal", {
      get: base,
      set: function (n) { n = n | 0; if (n <= 0 || rev[n] == null) { throw new TypeError("invalid enumeration value"); } setAttr(node, attr, rev[n]); },
      enumerable: true
    });
    Object.defineProperty(anim, "animVal", { get: base, enumerable: true });
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
    lineargradient: [["gradientUnits", ENUM_UNITS, "objectBoundingBox"], ["spreadMethod", { pad: 1, reflect: 2, repeat: 3 }, "pad"]],
    radialgradient: [["gradientUnits", ENUM_UNITS, "objectBoundingBox"], ["spreadMethod", { pad: 1, reflect: 2, repeat: 3 }, "pad"]],
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
    SVGTextPathElement: { SVG_TEXTPATH_METHODTYPE_UNKNOWN: 0, SVG_TEXTPATH_METHODTYPE_ALIGN: 1, SVG_TEXTPATH_METHODTYPE_STRETCH: 2, SVG_TEXTPATH_SPACINGTYPE_UNKNOWN: 0, SVG_TEXTPATH_SPACINGTYPE_AUTO: 1, SVG_TEXTPATH_SPACINGTYPE_EXACT: 2 },
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

    // Enumerated presentation properties (SVGAnimatedEnumeration: type/operator/gradientUnits/…).
    if (ENUM_PROPS[ln]) {
      ENUM_PROPS[ln].forEach(function (s) { def(el, s[0], makeEnumProp(el, s[0], s[1], s[2])); });
    }

    // Set the specific SVG element interface prototype (so `instanceof SVGRectElement` works); its
    // chain ends at SVGElement.prototype set by browser-env's applyNodePrototype.
    var ifaceName = TAG_IFACE[ln];
    if (ifaceName && globalThis[ifaceName] && globalThis[ifaceName].prototype) {
      try { if (Object.getPrototypeOf(el) !== globalThis[ifaceName].prototype) { Object.setPrototypeOf(el, globalThis[ifaceName].prototype); } } catch (e) {}
    }

    // Scalar length attributes -> SVGAnimatedLength (cached per element+attr).
    var attrs = LEN_ATTRS[ln];
    if (attrs) {
      var cache = {};
      def(el, "__svgLenCache", cache);
      for (var i = 0; i < attrs.length; i++) {
        (function (a) {
          var dflt = LEN_DEFAULTS[ln + "." + a];
          Object.defineProperty(el, a, {
            get: function () { return cache[a] || (cache[a] = makeAnimatedLength(el, a, dflt)); },
            configurable: true, enumerable: true
          });
        })(attrs[i]);
      }
    }

    // SVGTextContentElement methods (text / tspan / textPath / tref).
    if (ln === "text" || ln === "tspan" || ln === "tref" || ln === "textpath" || ln === "altglyph") {
      def(el, "getNumberOfChars", function () { var t = this.textContent; return t == null ? 0 : String(t).length; });
      def(el, "getComputedTextLength", function () { return bbox(this).width; });
      def(el, "getSubStringLength", function (i, n) { var len = this.getNumberOfChars(); var total = bbox(this).width; if (!len) { return 0; } return total * Math.max(0, Math.min(n, len - i)) / len; });
      def(el, "getRotationOfChar", function (i) {
        var r = getAttr(this.__node, "rotate"); if (r == null || r === "") { return 0; }
        var list = vecParse(r); if (!list.length) { return 0; }
        return i < list.length ? list[i] : list[list.length - 1];
      });
      def(el, "getStartPositionOfChar", function (i) { var b = bbox(this); var len = this.getNumberOfChars() || 1; return makePoint(b.x + b.width * i / len, b.y + b.height); });
      def(el, "getEndPositionOfChar", function (i) { var b = bbox(this); var len = this.getNumberOfChars() || 1; return makePoint(b.x + b.width * (i + 1) / len, b.y + b.height); });
      def(el, "getExtentOfChar", function (i) { var b = bbox(this); var len = this.getNumberOfChars() || 1; return makeRectObj(b.x + b.width * i / len, b.y, b.width / len, b.height); });
      def(el, "getCharNumAtPosition", function (p) { var b = bbox(this); var len = this.getNumberOfChars() || 1; if (!p || b.width === 0) { return -1; } var idx = Math.floor((p.x - b.x) / (b.width / len)); return idx >= 0 && idx < len ? idx : -1; });
      def(el, "selectSubString", function () {});
      if (!("textLength" in el)) { (function () { var tc = null; Object.defineProperty(el, "textLength", { get: function () { if (!tc) { tc = makeAnimatedLength(el, "textLength"); } return tc; }, configurable: true, enumerable: true }); })(); }
      def(el, "lengthAdjust", makeEnumProp(el, "lengthAdjust", LENGTHADJUST_MAP, "spacing"));
    }
    // Text-positioning elements: x/y/dx/dy are length lists, rotate is a number list.
    if (ln === "text" || ln === "tspan" || ln === "tref" || ln === "textpath" || ln === "altglyph") {
      var listCache = {};
      [["x", globalThis.SVGLengthList, SVGLength], ["y", globalThis.SVGLengthList, SVGLength],
       ["dx", globalThis.SVGLengthList, SVGLength], ["dy", globalThis.SVGLengthList, SVGLength],
       ["rotate", globalThis.SVGNumberList, globalThis.SVGNumber]].forEach(function (spec) {
        Object.defineProperty(el, spec[0], {
          get: function () { return listCache[spec[0]] || (listCache[spec[0]] = makeAnimatedItemList(el, spec[0], spec[1], spec[2])); },
          configurable: true, enumerable: true
        });
      });
    }

    // viewBox -> SVGAnimatedRect (on the elements that take one).
    if (ln === "svg" || ln === "symbol" || ln === "marker" || ln === "pattern" || ln === "view") {
      (function () {
        var vbCache = null;
        Object.defineProperty(el, "viewBox", {
          get: function () { return vbCache || (vbCache = makeAnimatedRect(el, "viewBox")); },
          configurable: true, enumerable: true
        });
      })();
    }
    // preserveAspectRatio: a minimal SVGAnimatedPreserveAspectRatio (the default xMidYMid meet).
    if (ln === "svg" || ln === "symbol" || ln === "marker" || ln === "pattern" || ln === "view" || ln === "image" || ln === "feimage") {
      if (!("preserveAspectRatio" in el)) {
        var par = Object.create(globalThis.SVGAnimatedPreserveAspectRatio.prototype);
        function makePar() { var p = Object.create(SVGPreserveAspectRatio.prototype); p.align = 6; p.meetOrSlice = 1; return p; }
        Object.defineProperty(par, "baseVal", { value: makePar(), enumerable: true });
        Object.defineProperty(par, "animVal", { value: makePar(), enumerable: true });
        def(el, "preserveAspectRatio", par);
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
      // Geometry queries: an element intersects `rect` (in this svg's user space) when its CTM-mapped
      // bounding box overlaps the rect.
      var elemViewBox = function (e) {
        var b = bbox(e), m = ctmOf(e);
        var xs = [b.x, b.x + b.width], ys = [b.y, b.y + b.height], mnx = Infinity, mny = Infinity, mxx = -Infinity, mxy = -Infinity;
        for (var i = 0; i < 2; i++) { for (var j = 0; j < 2; j++) { var px = m.a * xs[i] + m.c * ys[j] + m.e, py = m.b * xs[i] + m.d * ys[j] + m.f; mnx = Math.min(mnx, px); mny = Math.min(mny, py); mxx = Math.max(mxx, px); mxy = Math.max(mxy, py); } }
        return { x: mnx, y: mny, w: mxx - mnx, h: mxy - mny };
      };
      var rectsOverlap = function (b, r) { return b.x < r.x + r.width && b.x + b.w > r.x && b.y < r.y + r.height && b.y + b.h > r.y; };
      def(el, "checkIntersection", function (element, rect) { return rectsOverlap(elemViewBox(element), rect); });
      def(el, "checkEnclosure", function (element, rect) { var b = elemViewBox(element); return b.x >= rect.x && b.y >= rect.y && b.x + b.w <= rect.x + rect.width && b.y + b.h <= rect.y + rect.height; });
      def(el, "getIntersectionList", function (rect, ref) {
        var root = ref || el, out = [];
        var GRAPHICS = { rect: 1, circle: 1, ellipse: 1, line: 1, polyline: 1, polygon: 1, path: 1, text: 1, image: 1, use: 1 };
        var SKIP = { defs: 1, clippath: 1, mask: 1, symbol: 1, marker: 1, pattern: 1, lineargradient: 1, radialgradient: 1, filter: 1 };
        (function walk(n) {
          var kids = n.childNodes;
          for (var i = 0; kids && i < kids.length; i++) {
            var c = kids[i];
            if (!c || c.nodeType !== 1 || c.namespaceURI !== SVG_NS) { continue; }
            var ln2 = c.__localName;
            if (SKIP[ln2]) { continue; } // non-rendered containers (and their subtrees)
            var disp = ""; try { disp = nativeGCS(c).getPropertyValue("display"); } catch (e) {}
            if (disp === "none") { continue; }
            var pe = getAttr(c.__node, "pointer-events"); try { var sp = nativeGCS(c).getPropertyValue("pointer-events"); if (sp) { pe = sp; } } catch (e2) {}
            if (GRAPHICS[ln2] && pe !== "none" && rectsOverlap(elemViewBox(c), rect)) { out.push(c); }
            if (ln2 !== "use") { walk(c); } // <use> renders a clone, not its DOM children
          }
        })(root);
        return out;
      });
      def(el, "getEnclosureList", function (rect, ref) { return []; });
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

    // Marker: orientAngle / orientType / markerUnits.
    if (ln === "marker") {
      var node = el.__node;
      var isAutoOrient = function () { var o = getAttr(node, "orient"); return o === "auto" || o === "auto-start-reverse"; };
      var angleNum = function () { var o = getAttr(node, "orient"); return (o == null || isAutoOrient()) ? 0 : parseAngle(o).value; };
      // orientAngle (SVGAnimatedAngle) — baseVal writes sync to the `orient` attribute.
      var oa = Object.create(SVGAngle.prototype);
      Object.defineProperty(oa, "value", { get: angleNum, set: function (v) { setAttr(node, "orient", String(v)); }, enumerable: true });
      Object.defineProperty(oa, "valueInSpecifiedUnits", { get: angleNum, set: function (v) { setAttr(node, "orient", String(v)); }, enumerable: true });
      Object.defineProperty(oa, "valueAsString", { get: function () { return String(angleNum()); }, set: function (v) { setAttr(node, "orient", String(v)); }, enumerable: true });
      Object.defineProperty(oa, "unitType", { get: function () { var o = getAttr(node, "orient"); return (o == null || isAutoOrient()) ? 1 : parseAngle(o).type; }, enumerable: true });
      def(oa, "newValueSpecifiedUnits", function (u, v) { setAttr(node, "orient", String(v)); });
      def(oa, "convertToSpecifiedUnits", function () { setAttr(node, "orient", String(angleNum())); });
      var orientAngle = Object.create(globalThis.SVGAnimatedAngle.prototype);
      Object.defineProperty(orientAngle, "baseVal", { value: oa, enumerable: true });
      Object.defineProperty(orientAngle, "animVal", { value: makeAngle(function () { return svgAnimNum(el, "orient", angleNum(), angleVec); }), enumerable: true });
      def(el, "orientAngle", orientAngle);
      // orientType (SVGAnimatedEnumeration) — baseVal writes sync to the `orient` attribute.
      var orientTypeVal = function () { var o = getAttr(node, "orient"); if (o == null) { return 2; } return isAutoOrient() ? 1 : 2; };
      var orientType = Object.create(globalThis.SVGAnimatedEnumeration.prototype);
      Object.defineProperty(orientType, "baseVal", { get: orientTypeVal, set: function (v) { v = v | 0; if (v === 1) { setAttr(node, "orient", "auto"); } else if (v === 2) { setAttr(node, "orient", String(angleNum())); } }, enumerable: true });
      Object.defineProperty(orientType, "animVal", { get: orientTypeVal, enumerable: true });
      def(el, "orientType", orientType);
      def(el, "setOrientToAuto", function () { setAttr(node, "orient", "auto"); });
      def(el, "setOrientToAngle", function (a) { setAttr(node, "orient", String(a && a.value != null ? a.value : a)); });
      def(el, "SVG_MARKER_ORIENT_UNKNOWN", 0); def(el, "SVG_MARKER_ORIENT_AUTO", 1); def(el, "SVG_MARKER_ORIENT_ANGLE", 2);
    }

    // transform -> SVGAnimatedTransformList (cached); className -> SVGAnimatedString.
    (function () {
      var tCache = null;
      Object.defineProperty(el, "transform", { get: function () { return tCache || (tCache = makeAnimatedTransformList(el)); }, configurable: true, enumerable: true });
    })();
    // gradientTransform / patternTransform on gradient and pattern elements.
    if (ln === "lineargradient" || ln === "radialgradient") {
      var gtCache = null;
      Object.defineProperty(el, "gradientTransform", { get: function () { return gtCache || (gtCache = makeAnimatedTransformListAttr(el, "gradientTransform")); }, configurable: true, enumerable: true });
    }
    if (ln === "pattern") {
      var ptCache = null;
      Object.defineProperty(el, "patternTransform", { get: function () { return ptCache || (ptCache = makeAnimatedTransformListAttr(el, "patternTransform")); }, configurable: true, enumerable: true });
    }
    (function () {
      var cCache = null;
      Object.defineProperty(el, "className", { get: function () { return cCache || (cCache = makeAnimatedString(el, "class")); }, configurable: true, enumerable: true });
    })();
    // href is an SVGAnimatedString on the elements that take one (a, use, image, gradients, …).
    if (ln === "a" || ln === "use" || ln === "image" || ln === "lineargradient" || ln === "radialgradient" || ln === "pattern" || ln === "textpath" || ln === "mpath" || ln === "feimage" || ln === "tref") {
      if (!("href" in el) || typeof el.href !== "object") {
        var hCache = null;
        Object.defineProperty(el, "href", { get: function () { return hCache || (hCache = makeAnimatedString(el, getAttr(el.__node, "href") != null ? "href" : "xlink:href")); }, configurable: true, enumerable: true });
      }
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
    T.type = TTYPE[lt] || 0;
    T.angle = (lt === "rotate" || lt === "skewx" || lt === "skewy") ? (v[0] || 0) : 0;
    T.matrix = transformMatrix(lt, v);
    def(T, "setMatrix", function (m) { T.matrix = m; T.type = 1; T.angle = 0; });
    def(T, "setTranslate", function (x, y) { T.matrix = transformMatrix("translate", [x, y]); T.type = 2; T.angle = 0; });
    def(T, "setScale", function (x, y) { T.matrix = transformMatrix("scale", [x, y]); T.type = 3; T.angle = 0; });
    def(T, "setRotate", function (a, cx, cy) { T.matrix = transformMatrix("rotate", [a, cx, cy]); T.type = 4; T.angle = a; });
    def(T, "setSkewX", function (a) { T.matrix = transformMatrix("skewx", [a]); T.type = 5; T.angle = a; });
    def(T, "setSkewY", function (a) { T.matrix = transformMatrix("skewy", [a]); T.type = 6; T.angle = a; });
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
  function makeTransformList(items) {
    var L = Object.create(globalThis.SVGTransformList.prototype);
    Object.defineProperty(L, "numberOfItems", { get: function () { return items.length; }, enumerable: true });
    Object.defineProperty(L, "length", { get: function () { return items.length; }, enumerable: true });
    def(L, "getItem", function (i) { if (i < 0 || i >= items.length) { throw new globalThis.DOMException("index", "IndexSizeError"); } return items[i]; });
    def(L, "clear", function () { items.length = 0; });
    def(L, "initialize", function (t) { items.length = 0; items.push(t); return t; });
    def(L, "appendItem", function (t) { items.push(t); return t; });
    def(L, "insertItemBefore", function (t, i) { items.splice(i, 0, t); return t; });
    def(L, "removeItem", function (i) { return items.splice(i, 1)[0]; });
    def(L, "replaceItem", function (t, i) { items[i] = t; return t; });
    def(L, "consolidate", function () { if (!items.length) { return null; } var m = items[0].matrix; for (var k = 1; k < items.length; k++) { m = m.multiply(items[k].matrix); } var t = makeTransform("matrix", [m.a, m.b, m.c, m.d, m.e, m.f]); items.length = 0; items.push(t); return t; });
    def(L, "createSVGTransformFromMatrix", function (m) { return makeTransform("matrix", [m.a, m.b, m.c, m.d, m.e, m.f]); });
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
    Object.defineProperty(anim, "baseVal", { get: function () { return makeTransformList(parseTransformList(getAttr(node, attr) || "")); }, enumerable: true });
    Object.defineProperty(anim, "animVal", { get: function () { return makeTransformList(transformAnimVal(el, attr)); }, enumerable: true });
    return anim;
  }
  function makeAnimatedTransformList(el) { return makeAnimatedTransformListAttr(el, "transform"); }

  // SVGAnimatedString (className, href, etc.).
  function makeAnimatedString(el, attr) {
    var node = el.__node;
    var anim = Object.create(globalThis.SVGAnimatedString.prototype);
    Object.defineProperty(anim, "baseVal", { get: function () { var v = getAttr(node, attr); return v == null ? "" : v; }, set: function (v) { setAttr(node, attr, v == null ? "" : String(v)); }, enumerable: true });
    Object.defineProperty(anim, "animVal", { get: function () { var v = getAttr(node, attr); return v == null ? "" : v; }, enumerable: true });
    return anim;
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
  function makePoint(x, y) { var P = Object.create(globalThis.SVGPoint.prototype); P.x = x; P.y = y; def(P, "matrixTransform", function (m) { return makePoint(m.a * x + m.c * y + m.e, m.b * x + m.d * y + m.f); }); return P; }
  function makeRectObj(x, y, w, h) { var R = Object.create(globalThis.SVGRect.prototype); R.x = x; R.y = y; R.width = w; R.height = h; return R; }

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
    def(Geo, "isPointInFill", function (point) { return false; });
    def(Geo, "isPointInStroke", function (point) { return false; });
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
    M.a = a; M.b = b; M.c = c; M.d = d; M.e = e; M.f = f;
    def(M, "multiply", function (o) { return makeMatrix(M.a * o.a + M.c * o.b, M.b * o.a + M.d * o.b, M.a * o.c + M.c * o.d, M.b * o.c + M.d * o.d, M.a * o.e + M.c * o.f + M.e, M.b * o.e + M.d * o.f + M.f); });
    def(M, "translate", function (x, y) { return M.multiply(makeMatrix(1, 0, 0, 1, x, y)); });
    def(M, "scale", function (s) { return M.multiply(makeMatrix(s, 0, 0, s, 0, 0)); });
    def(M, "inverse", function () { var det = M.a * M.d - M.b * M.c; if (!det) { throw new globalThis.DOMException("not invertible", "InvalidStateError"); } return makeMatrix(M.d / det, -M.b / det, -M.c / det, M.a / det, (M.c * M.f - M.d * M.e) / det, (M.b * M.e - M.a * M.f) / det); });
    return M;
  }
  globalThis.__svgMakeMatrix = makeMatrix;

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

  installGeometryProtos();
  globalThis.__svgEnrich = svgEnrich;
})();
