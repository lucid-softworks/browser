(function () {
  // Two-phase clock: VIRTUAL at load (fast-forward to fire pending one-shots so the first paint is
  // complete; intervals may spin, bounded by the cap), and the REAL wall clock once the page is
  // live (driven by Engine::tick) so setInterval/setTimeout/rAF fire on actual elapsed time.
  var loop = { timers: [], micro: [], nextId: 1, now: 0, realBase: 0, realtime: false, firedThisDrain: Object.create(null) };
  // Largest virtual-time jump allowed while fast-forwarding the load-time clock. Generous enough to
  // run any realistic deferred load work (setTimeout(0) chains, rAF, init delays) but well under the
  // multi-second testharness timeout, so test timeouts are deferred to the real clock instead of
  // being fast-forwarded and fired during load.
  var LOAD_FASTFORWARD_CAP_MS = 5000;
  Object.defineProperty(globalThis, "__eventLoop", { value: loop, enumerable: false, configurable: true, writable: true });
  Object.defineProperty(globalThis, "__timerErrors", { value: [], enumerable: false, configurable: true, writable: true });
  function nowMs() { try { return Date.now(); } catch (e) { return 0; } }
  function currentTime() { return loop.realtime ? (loop.now + (nowMs() - loop.realBase)) : loop.now; }

  function schedule(fn, delay, args, repeat) {
    if (typeof fn !== "function") { return 0; }
    var d = Number(delay) || 0;
    if (d < 0 || d !== d) { d = 0; }
    var id = loop.nextId++;
    loop.timers.push({ id: id, fn: fn, delay: d, args: args, when: currentTime() + d, repeat: repeat });
    return id;
  }

  function define(name, fn) {
    Object.defineProperty(globalThis, name, { value: fn, enumerable: false, configurable: true, writable: true });
  }

  define("setTimeout", function (fn, delay) {
    var args = Array.prototype.slice.call(arguments, 2);
    return schedule(fn, delay, args, false);
  });
  define("setInterval", function (fn, delay) {
    var args = Array.prototype.slice.call(arguments, 2);
    return schedule(fn, delay, args, true);
  });
  define("clearTimeout", function (id) {
    if (id == null) { return; }
    for (var i = 0; i < loop.timers.length; i++) {
      if (loop.timers[i].id === id) { loop.timers.splice(i, 1); return; }
    }
  });
  define("clearInterval", globalThis.clearTimeout);

  define("queueMicrotask", function (fn) {
    if (typeof fn !== "function") { throw new TypeError("queueMicrotask: argument is not a function"); }
    loop.micro.push(fn);
  });

  define("requestAnimationFrame", function (fn) {
    // No real frames; schedule ~16ms out (one 60fps frame) so rAF runs after 0ms timers. The
    // callback receives a DOMHighResTimeStamp read at FIRE time — performance.now() (the frame time),
    // matching document.timeline.currentTime so animation-timeline tests line up.
    return schedule(function () {
      var ts = (globalThis.performance && typeof globalThis.performance.now === "function")
        ? globalThis.performance.now() : currentTime();
      // Freeze the frame time so document.timeline.currentTime reads the SAME value as the rAF
      // timestamp during this frame (callbacks + their sync continuations).
      globalThis.__frameTime = ts;
      fn(ts);
    }, 16, [], false);
  });
  define("cancelAnimationFrame", globalThis.clearTimeout);

  // Reset the per-drain "already fired" set (Rust calls at each drain start) so an interval can't
  // spin within a single realtime tick.
  define("__beginDrain", function () { loop.firedThisDrain = Object.create(null); });
  // Switch from the load-time virtual clock to the real wall clock (Rust calls once the page is
  // live); re-arm surviving repeating timers to fire `delay` ms from now (real time).
  define("__enterRealtime", function () {
    if (loop.realtime) { return; }
    loop.realtime = true;
    loop.realBase = nowMs();
    for (var i = 0; i < loop.timers.length; i++) { if (loop.timers[i].repeat) { loop.timers[i].when = loop.now + loop.timers[i].delay; } }
  });

  // Driver called from Rust. Returns true if it ran a task (microtask or timer), false if nothing
  // is currently runnable. One throwing task does not kill the loop: errors are collected.
  define("__runDueTimers", function () {
    // 1. Drain ALL microtasks first (FIFO), including ones queued while draining.
    var ranSomething = false;
    while (loop.micro.length > 0) {
      var m = loop.micro.shift();
      ranSomething = true;
      try { m(); } catch (e) { globalThis.__timerErrors.push((e&&e.stack||String(e))); }
    }
    if (ranSomething) { return true; }

    // 2. Pick the smallest-`when` timer (skipping a repeat already fired this realtime tick).
    if (loop.timers.length === 0) { return false; }
    // A repeating timer fires at most once per drain (load OR tick) so an interval can't spin to
    // the cap — its callback runs once at load, then once per real-time tick thereafter.
    var bestIdx = -1, best = null;
    for (var i = 0; i < loop.timers.length; i++) {
      var t = loop.timers[i];
      if (t.repeat && loop.firedThisDrain[t.id]) { continue; }
      if (bestIdx < 0 || t.when < best.when || (t.when === best.when && t.id < best.id)) { bestIdx = i; best = t; }
    }
    if (bestIdx < 0) { return false; }
    var timer = loop.timers[bestIdx];
    if (loop.realtime) {
      // Real clock: fire only once the scheduled instant has actually elapsed.
      if (timer.when > currentTime()) { return false; }
      if (timer.repeat) { timer.when = timer.when + timer.delay; loop.firedThisDrain[timer.id] = true; }
      else { loop.timers.splice(bestIdx, 1); }
    } else {
      // Load-time: fast-forward virtual time to this timer and fire it (one-shots and rAF chains
      // run freely; a repeating timer fires once and is parked for the real-time ticks). But do NOT
      // fast-forward across a far-future timer: long timeouts (notably testharness's multi-second
      // per-test/harness timeout) must be measured on the REAL clock, not skipped to instantly. If
      // the page is awaiting external input that only arrives after load — e.g. a testdriver
      // `test_driver.Actions().send()` — virtual time would otherwise jump straight to the test
      // timeout and fire it ("Test timed out") before the input is ever delivered. Parking such
      // timers ends the load drain; they then fire in real time once `__enterRealtime` is active.
      if (timer.when - loop.now > LOAD_FASTFORWARD_CAP_MS) { return false; }
      if (timer.when > loop.now) { loop.now = timer.when; }
      if (timer.repeat) { timer.when = loop.now + timer.delay; loop.firedThisDrain[timer.id] = true; }
      else { loop.timers.splice(bestIdx, 1); }
    }
    try { timer.fn.apply(undefined, timer.args); }
    catch (e) { globalThis.__timerErrors.push((e&&e.stack||String(e))); }
    return true;
  });
})();
