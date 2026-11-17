/*
 * Conspect Cast receiver: capability probe, engine clock and tick-to-present latency (ADR-0114 slice 2).
 * Plain script, no build step, ES5 syntax only (Chromecast firmware runs old Chrome: one unknown syntax form stops
 * the whole script). Loaded by index.html BEFORE receiver.js; it only defines window.ConspectProbe and does nothing
 * until receiver.js calls ConspectProbe.start(...). If this file fails to load, receiver.js plays HLS as before.
 *
 * What it does for one load (one attachment; HLS is the only rung of this slice):
 *  - feature tests (wt, mse, vdec, vdec_hw, adec, rtc, rtc_h264, rtc_opus, rvfc, frames), POST /cast/<token>/ladder
 *    with them, and remembers the engine's answer (attach.rung = "hls"; nothing else exists yet);
 *  - the NTP-style clock offset to the engine from each ladder call (t0 sent, t1 received, t2 answered, t3 back),
 *    best of the last 8 by error bound, a clock-only call every 30 s, "unavailable" above 250 ms;
 *  - tick-to-present latency: for each presented frame its engine wall time (Shaka's date-time of the playhead,
 *    moved by the frame's distance from it; the playlist's EXT-X-PROGRAM-DATE-TIME is the engine epoch) against the time it was
 *    shown, moved onto the engine clock. Method "frame" (requestVideoFrameCallback), else "playhead" (currentTime
 *    every 100 ms), else "unavailable". Windows of 5 s from 3 s after the first frame; a sample outside 0..60 s is
 *    invalid; more than 5 invalid in a window make the method "unavailable" for the rest of the load;
 *  - first_frame_ms: from the LOAD to the first presented frame, on the monotonic clock only;
 *  - a frame-counting stall watchdog (rVFC, else getVideoPlaybackQuality): no frame for 2 s while playing is
 *    "stalled". On HLS it only reports; there is no rung below HLS to step down to.
 * Every clock here is monotonic (performance.timeOrigin + performance.now()), never Date.now().
 * No secrets: the token is only ever part of the base URL receiver.js passes in, and is never logged.
 */
(function (root) {
  'use strict';

  var CLOCK_KEEP = 8;                 // samples kept for the offset estimate
  var CLOCK_MAX_ERROR_US = 250000;    // best error above this: clock unavailable
  var CLOCK_EVERY_MS = 30000;         // clock-only ladder call period
  var WARMUP_MS = 3000;               // latency starts this long after the first frame
  var WINDOW_MS = 5000;               // latency window
  var MAX_LATENCY_MS = 60000;         // sanity: a sample is valid in 0..60 s
  var MAX_INVALID = 5;                // more invalid samples than this in a window: unavailable for the load
  var PLAYHEAD_EVERY_MS = 100;
  var STALL_MS = 2000;                // no presented frame for this long: stalled
  var QUALITY_EVERY_MS = 250;
  var MAX_H264 = 8;
  var SKIP_KEY = 'conspect.cast.skip';
  var LOG = '[conspect-cast]';

  var P = {};

  function info() {
    var a = Array.prototype.slice.call(arguments);
    a.unshift(LOG);
    try { console.info.apply(console, a); } catch (e) { /* console missing */ }
  }

  /** Monotonic wall-like clock in microseconds (timeOrigin + now), or null without performance.now. */
  function monoUs(perf) {
    perf = perf || root.performance;
    if (!perf || typeof perf.now !== 'function') { return null; }
    var origin = typeof perf.timeOrigin === 'number' ? perf.timeOrigin :
      (perf.timing && perf.timing.navigationStart) || 0;
    return (origin + perf.now()) * 1000;
  }
  P.monoUs = monoUs;

  // ---------------------------------------------------------------- clock offset (pure)

  /** The offset estimator of ADR-0114 decision 2 ("Receiver clock"). All times in microseconds. */
  function Clock() { this.samples = []; this.generation = null; }
  /**
   * Add one exchange. Returns true when the engine's epoch was stepped since the last sample (a new
   * epoch_generation): the caller then discards its latency samples. Older samples are dropped too, because they
   * describe the old epoch.
   */
  Clock.prototype.add = function (t0, t1, t2, t3, generation) {
    var stepped = this.generation !== null && generation !== this.generation;
    if (stepped) { this.samples = []; }
    this.generation = generation;
    if (![t0, t1, t2, t3].every(function (t) { return typeof t === 'number' && isFinite(t); })) { return stepped; }
    if (t3 < t0) { return stepped; }
    var offset = ((t1 - t0) + (t2 - t3)) / 2;
    var error = ((t3 - t0) - (t2 - t1)) / 2;
    if (error < 0) { error = 0; }
    this.samples.push({ offset: offset, error: error });
    if (this.samples.length > CLOCK_KEEP) { this.samples.shift(); }
    return stepped;
  };
  /** The sample with the smallest error bound of the last 8, or null. */
  Clock.prototype.best = function () {
    var best = null;
    for (var i = 0; i < this.samples.length; i++) {
      if (best === null || this.samples[i].error < best.error) { best = this.samples[i]; }
    }
    return best;
  };
  /** {offset_us, error_us} when usable, else null ("unavailable"). */
  Clock.prototype.usable = function () {
    var best = this.best();
    return best !== null && best.error <= CLOCK_MAX_ERROR_US ? { offset_us: best.offset, error_us: best.error } : null;
  };
  Clock.prototype.reset = function () { this.samples = []; this.generation = null; };
  P.Clock = Clock;

  // ---------------------------------------------------------------- latency windows (pure)

  function percentile(sorted, p) {
    if (sorted.length === 0) { return null; }
    var i = Math.ceil(p * sorted.length) - 1;
    return sorted[Math.max(0, Math.min(sorted.length - 1, i))];
  }
  P.percentile = percentile;

  /** Tick-to-present latency of one attachment. `method`: 'frame' | 'playhead' | 'unavailable'. */
  function Latency(method) {
    this.method = method;
    this.locked = method === 'unavailable';
    this.reset();
    this.last = null;
  }
  /** A new measurement start (first frame, an epoch step): the warm-up and the window restart. */
  Latency.prototype.reset = function (firstFrameUs) {
    this.startUs = typeof firstFrameUs === 'number' ? firstFrameUs + WARMUP_MS * 1000 : null;
    this.windowStartUs = null;
    this.values = [];
    this.invalid = 0;
  };
  /**
   * One frame: shown at `presentUs` (receiver monotonic), engine wall time `frameWallUs`, clock `clock`
   * ({offset_us, error_us} or null). Returns the finished window's stats when this sample closed one.
   */
  Latency.prototype.sample = function (presentUs, frameWallUs, clock) {
    if (this.locked || this.startUs === null || presentUs < this.startUs) { return null; }
    if (this.windowStartUs === null) { this.windowStartUs = presentUs; }
    if (clock && typeof frameWallUs === 'number' && isFinite(frameWallUs)) {
      var ms = (presentUs + clock.offset_us - frameWallUs) / 1000;
      if (ms >= 0 && ms <= MAX_LATENCY_MS) { this.values.push(ms); } else { this.invalid += 1; }
      if (this.invalid > MAX_INVALID) {
        this.locked = true;
        this.method = 'unavailable';
        this.last = null;
        info('latency unavailable for this load:', this.invalid, 'invalid samples in one window');
        return null;
      }
    }
    if (presentUs - this.windowStartUs < WINDOW_MS * 1000) { return null; }
    var done = null;
    if (this.values.length > 0 && clock) {
      var sorted = this.values.slice().sort(function (a, b) { return a - b; });
      done = {
        median: Math.round(percentile(sorted, 0.5)), p95: Math.round(percentile(sorted, 0.95)),
        samples: sorted.length, method: this.method, clock_error_ms: Math.round(clock.error_us / 1000)
      };
    }
    this.last = done;
    this.windowStartUs = presentUs;
    this.values = [];
    this.invalid = 0;
    return done;
  };
  P.Latency = Latency;

  /** The method a device allows: 'frame' with rVFC, else 'playhead', else 'unavailable'. */
  P.methodFor = function (hasRvfc, hasAbsoluteTime) {
    if (!hasAbsoluteTime) { return 'unavailable'; }
    return hasRvfc ? 'frame' : 'playhead';
  };

  // ---------------------------------------------------------------- stall watchdog

  /**
   * Counts presented frames (rVFC callbacks, else getVideoPlaybackQuality() totalVideoFrames - droppedVideoFrames:
   * totalVideoFrames alone also counts dropped frames) and calls onChange(stalled)
   * when no frame was presented for 2 s while the element plays, and again when frames come back. The later rungs
   * use the same helper for their step-down; on HLS it only reports. `getVideo` returns the current element. The
   * count starts from zero for each load (a new watchdog per start()); a value lower than the last one (the element
   * restarted its counts) starts a new count and is not progress.
   * Returns {stop(), stalled(), counter}. `onFrame(now, metadata)` (optional) sees every rVFC callback.
   */
  function FrameWatchdog(getVideo, onChange, onFrame, timers) {
    timers = timers || root;
    var self = this;
    var video = getVideo();
    this.counter = video && typeof video.requestVideoFrameCallback === 'function' ? 'rvfc' :
      (video && typeof video.getVideoPlaybackQuality === 'function' ? 'quality' : null);
    this.isStalled = false;
    this.lastFrameAt = Date.now();
    this.lastCount = -1;
    this.stopped = false;
    var handle = null;
    function frame(nowMs, md) {
      if (self.stopped) { return; }
      self.lastFrameAt = Date.now();
      if (self.isStalled) { self.isStalled = false; onChange(false); }
      if (onFrame) { try { onFrame(nowMs, md); } catch (e) { /* never break the loop */ } }
      arm();
    }
    function arm() {
      var v = getVideo();
      if (!self.stopped && v && typeof v.requestVideoFrameCallback === 'function') {
        try { handle = { v: v, id: v.requestVideoFrameCallback(frame) }; } catch (e) { handle = null; }
      }
    }
    if (this.counter === 'rvfc') { arm(); }
    this.timer = timers.setInterval(function () {
      if (self.stopped) { return; }
      var v = getVideo();
      if (self.counter === 'quality' && v) {
        var q = null;
        try { q = v.getVideoPlaybackQuality(); } catch (e) { q = null; }
        var n = P.presentedFrames(q);
        if (n !== null && n < self.lastCount) { self.lastCount = n; }   // counts restarted: a new count, not progress
        else if (n !== null && n > self.lastCount) {
          var first = self.lastCount < 0;
          self.lastCount = n;
          if (!first) { self.lastFrameAt = Date.now(); if (self.isStalled) { self.isStalled = false; onChange(false); } }
        }
      }
      if (self.counter === 'rvfc' && v && (!handle || handle.v !== v)) { arm(); }   // CAF replaced the element
      var playing = v && !v.paused && v.readyState >= 2;
      if (!playing) { self.lastFrameAt = Date.now(); return; }
      if (!self.isStalled && Date.now() - self.lastFrameAt >= STALL_MS) { self.isStalled = true; onChange(true); }
    }, QUALITY_EVERY_MS);
  }
  /** Presented frames of a VideoPlaybackQuality: totalVideoFrames - droppedVideoFrames, or null. */
  P.presentedFrames = function (q) {
    if (!q || typeof q.totalVideoFrames !== 'number') { return null; }
    var dropped = typeof q.droppedVideoFrames === 'number' ? q.droppedVideoFrames : 0;
    return Math.max(0, q.totalVideoFrames - dropped);
  };

  FrameWatchdog.prototype.stop = function () {
    this.stopped = true;
    try { root.clearInterval(this.timer); } catch (e) { /* none */ }
  };
  FrameWatchdog.prototype.stalled = function () { return this.isStalled; };
  P.FrameWatchdog = FrameWatchdog;

  // ---------------------------------------------------------------- capabilities

  /** Codec strings and picture size from master.m3u8 text: {video, audio, width, height, all}. */
  P.codecsFromMaster = function (text) {
    var out = { video: null, audio: null, width: 0, height: 0, all: [] };
    var m = /CODECS="([^"]+)"/.exec(text || '');
    if (m) {
      var parts = m[1].split(',');
      for (var i = 0; i < parts.length; i++) {
        var c = parts[i].replace(/^\s+|\s+$/g, '');
        if (!c) { continue; }
        out.all.push(c);
        if (/^(avc[13]|hvc1|hev1|av01|vp09)\./.test(c) && !out.video) { out.video = c; }
        if (/^(mp4a|opus|ac-3|ec-3)/.test(c) && !out.audio) { out.audio = c; }
      }
    }
    var r = /RESOLUTION=(\d+)x(\d+)/.exec(text || '');
    if (r) { out.width = parseInt(r[1], 10); out.height = parseInt(r[2], 10); }
    return out;
  };

  /** Rungs in the device's failure memory: {rung: seconds left}, expired entries dropped. */
  P.readSkip = function (storage, nowMs) {
    var out = {};
    try {
      var raw = storage && storage.getItem(SKIP_KEY);
      var map = raw ? JSON.parse(raw) : {};
      for (var k in map) {
        if (Object.prototype.hasOwnProperty.call(map, k) && /^(srt|moq|webrtc)$/.test(k) && typeof map[k] === 'number') {
          var left = Math.round((map[k] - nowMs) / 1000);
          if (left > 0) { out[k] = Math.min(left, 86400); }
        }
      }
    } catch (e) { /* no storage */ }
    return out;
  };

  /** Remember that `rung` failed: skip it for `seconds`. HLS is never remembered. */
  P.rememberFailure = function (storage, rung, seconds, nowMs) {
    if (!/^(srt|moq|webrtc)$/.test(rung)) { return; }
    try {
      var raw = storage.getItem(SKIP_KEY);
      var map = raw ? JSON.parse(raw) : {};
      map[rung] = nowMs + seconds * 1000;
      storage.setItem(SKIP_KEY, JSON.stringify(map));
    } catch (e) { /* no storage */ }
  };

  function settle(promise, cb) {
    var done = false;
    var once = function (v) { if (!done) { done = true; cb(v); } };
    try {
      promise.then(function (r) { once(r && r.supported === true ? 'supported' : 'unsupported'); },
        function () { once('unsupported'); });
    } catch (e) { once('unsupported'); }
    setTimeout(function () { once('unsupported'); }, 2000);
  }

  /** H.264 profile-level-ids RTCRtpReceiver can take (at most 8, lower case, unique). */
  P.h264Profiles = function (env) {
    var out = [];
    try {
      var caps = env.RTCRtpReceiver && env.RTCRtpReceiver.getCapabilities && env.RTCRtpReceiver.getCapabilities('video');
      var codecs = caps && caps.codecs ? caps.codecs : [];
      for (var i = 0; i < codecs.length && out.length < MAX_H264; i++) {
        if (!/h264$/i.test(codecs[i].mimeType || '')) { continue; }
        var m = /profile-level-id=([0-9a-fA-F]{6})/.exec(codecs[i].sdpFmtpLine || '');
        var id = m ? m[1].toLowerCase() : null;
        if (id && out.indexOf(id) < 0) { out.push(id); }
      }
    } catch (e) { /* none */ }
    return out;
  };

  function opusReceive(env) {
    try {
      var caps = env.RTCRtpReceiver && env.RTCRtpReceiver.getCapabilities && env.RTCRtpReceiver.getCapabilities('audio');
      var codecs = caps && caps.codecs ? caps.codecs : [];
      for (var i = 0; i < codecs.length; i++) { if (/opus$/i.test(codecs[i].mimeType || '')) { return true; } }
    } catch (e) { /* none */ }
    return false;
  }

  /**
   * The ES5 feature tests of the probe, for the stream `codecs` (from master.m3u8). Calls cb(caps) once, within
   * about 2 s (decoder checks are promises where they exist).
   */
  P.detectCaps = function (env, codecs, video, cb) {
    var caps = {
      wt: typeof env.WebTransport === 'function',
      mse: false,
      vdec: 'absent', vdec_hw: false, adec: 'absent',
      rtc: typeof env.RTCPeerConnection === 'function',
      rtc_h264: [], rtc_opus: false,
      rvfc: !!(video && typeof video.requestVideoFrameCallback === 'function'),
      frames: video && typeof video.requestVideoFrameCallback === 'function' ? 'rvfc' :
        (video && typeof video.getVideoPlaybackQuality === 'function' ? 'quality' : null),
      srt: null
    };
    try {
      var MS = env.MediaSource;
      if (MS && typeof MS.isTypeSupported === 'function' && codecs.all.length > 0) {
        caps.mse = MS.isTypeSupported('video/mp4; codecs="' + codecs.all.join(',') + '"');
      }
    } catch (e) { caps.mse = false; }
    if (caps.rtc) { caps.rtc_h264 = P.h264Profiles(env); caps.rtc_opus = opusReceive(env); }
    var pending = 0;
    var finished = false;
    function done() { if (!finished && pending === 0) { finished = true; cb(caps); } }
    var size = { codedWidth: codecs.width || 1920, codedHeight: codecs.height || 1080 };
    if (env.VideoDecoder && typeof env.VideoDecoder.isConfigSupported === 'function' && codecs.video) {
      pending += 2;
      settle(env.VideoDecoder.isConfigSupported({ codec: codecs.video, codedWidth: size.codedWidth, codedHeight: size.codedHeight }),
        function (r) { caps.vdec = r; pending -= 1; done(); });
      settle(env.VideoDecoder.isConfigSupported({ codec: codecs.video, codedWidth: size.codedWidth, codedHeight: size.codedHeight,
        hardwareAcceleration: 'prefer-hardware' }), function (r) { caps.vdec_hw = r === 'supported'; pending -= 1; done(); });
    }
    if (env.AudioDecoder && typeof env.AudioDecoder.isConfigSupported === 'function' && codecs.audio) {
      pending += 1;
      settle(env.AudioDecoder.isConfigSupported({ codec: codecs.audio, sampleRate: 48000, numberOfChannels: 2 }),
        function (r) { caps.adec = r; pending -= 1; done(); });
    }
    done();
  };

  // ---------------------------------------------------------------- runtime (one load at a time)

  var run = null;   // the current load's state; replaced on every start()

  function ladderCall(r, body, onAnswer) {
    if (r.stopped) { return; }
    var t0 = monoUs();
    body.clock = { t0: t0 };
    var xhr;
    try {
      xhr = new XMLHttpRequest();
      xhr.open('POST', r.base + 'ladder', true);
      xhr.setRequestHeader('Content-Type', 'application/json');
      xhr.timeout = 5000;
      xhr.onload = function () {
        if (r.stopped) { return; }
        var t3 = monoUs();
        if (xhr.status !== 200) { r.ladderStatus = xhr.status; return; }
        var answer = null;
        try { answer = JSON.parse(xhr.responseText); } catch (e) { answer = null; }
        if (!answer || answer.v !== 1) { return; }
        r.ladderStatus = 200;
        var c = answer.clock;
        if (c && typeof c.t1_unix_us === 'number' && typeof c.t2_unix_us === 'number') {
          var stepped = r.clock.add(t0, c.t1_unix_us, c.t2_unix_us, t3, c.epoch_generation);
          if (stepped) {
            info('engine epoch stepped: latency samples discarded');
            r.latency.reset(r.firstFrameUs);
          }
        } else if (c === null) {
          r.clock.reset();
        }
        if (onAnswer) { onAnswer(answer); }
      };
      xhr.onerror = function () { r.ladderStatus = 0; };
      xhr.ontimeout = xhr.onerror;
      xhr.send(JSON.stringify(body));
    } catch (e) { r.ladderStatus = 0; }
  }

  /**
   * The engine wall time (us) of the frame at element time `mediaTime`: the playlist's EXT-X-PROGRAM-DATE-TIME
   * (the engine epoch) as Shaka maps it for the playhead now (getPlayheadTimeAsDate), moved by the frame's distance
   * from the playhead: wall(frame) = date(playhead) - (currentTime - mediaTime). Measured in the harness: CAF's
   * getAbsoluteTimeForMediaTime and Shaka's getPresentationStartTimeAsDate are NOT this mapping (both about 8 s
   * off the playhead's date-time on a live HLS stream), so neither is used.
   */
  P.wallUsForMediaTime = function (playerManager, video, mediaTime) {
    if (typeof mediaTime !== 'number' || !isFinite(mediaTime) || !video) { return null; }
    try {
      var sp = typeof playerManager.getShakaPlayer === 'function' ? playerManager.getShakaPlayer() : null;
      var date = sp && typeof sp.getPlayheadTimeAsDate === 'function' ? sp.getPlayheadTimeAsDate() : null;
      var ms = date && typeof date.getTime === 'function' ? date.getTime() : NaN;
      var ct = video.currentTime;
      if (!isFinite(ms) || ms <= 0 || typeof ct !== 'number' || !isFinite(ct)) { return null; }
      return (ms / 1000 - (ct - mediaTime)) * 1e6;
    } catch (e) { return null; }
  };

  function absoluteUs(r, mediaTime) { return P.wallUsForMediaTime(r.playerManager, r.video(), mediaTime); }

  function onPresented(r, presentUs, mediaTime) {
    if (r.firstFrameUs === null) {
      r.firstFrameUs = presentUs;
      r.firstFrameMs = Math.max(0, Math.round((presentUs - r.loadUs) / 1000));
      r.latency.reset(presentUs);
    }
    r.latency.sample(presentUs, absoluteUs(r, mediaTime), r.clock.usable());
  }

  /**
   * Start probing for one load. `opts`: {base: '<origin>/cast/<token>/', master: master.m3u8 text,
   * video: function returning the media element, playerManager, storage (optional), env (optional, window)}.
   * Called by receiver.js once master.m3u8 answered 200. Any earlier load's work stops first.
   */
  P.start = function (opts) {
    P.stop();
    var env = opts.env || root;
    var video = opts.video();
    var hasAbs = !!(opts.playerManager && typeof opts.playerManager.getShakaPlayer === 'function');
    var hasRvfc = !!(video && typeof video.requestVideoFrameCallback === 'function');
    var r = {
      base: opts.base, playerManager: opts.playerManager, video: opts.video, stopped: false,
      loadUs: typeof opts.loadUs === 'number' ? opts.loadUs : monoUs(),
      clock: new Clock(), latency: new Latency(P.methodFor(hasRvfc, hasAbs)),
      firstFrameUs: null, firstFrameMs: null, rung: null, ladderStatus: null,
      timers: [], watchdog: null, stalled: false
    };
    run = r;
    r.watchdog = new FrameWatchdog(opts.video, function (stalled) {
      r.stalled = stalled;
      info(stalled ? 'no frame presented for 2 s: stalled' : 'frames presented again');
    }, hasRvfc ? function (now, md) {
      var perf = root.performance;
      var origin = typeof perf.timeOrigin === 'number' ? perf.timeOrigin : 0;
      var shown = md && typeof md.expectedDisplayTime === 'number' ? md.expectedDisplayTime : now;
      onPresented(r, (origin + shown) * 1000, md ? md.mediaTime : NaN);
    } : null);
    if (!hasRvfc) {
      var last = -1;
      r.timers.push(setInterval(function () {
        var v = opts.video();
        if (!v || v.paused || v.readyState < 2) { return; }
        var t = v.currentTime;
        if (t === last) { return; }
        last = t;
        onPresented(r, monoUs(), t);
      }, PLAYHEAD_EVERY_MS));
    }
    var codecs = P.codecsFromMaster(opts.master);
    var storage = opts.storage;
    if (storage === undefined) { try { storage = root.localStorage; } catch (e) { storage = null; } }
    P.detectCaps(env, codecs, video, function (caps) {
      if (r.stopped) { return; }
      r.caps = caps;
      var body = {
        v: 1,
        receiver: { kind: 'web', build: P.BUILD, ua: String(env.navigator ? env.navigator.userAgent : '').slice(0, 200) },
        caps: caps,
        need: { subtitles: false, audio: null },
        skip: P.readSkip(storage, Date.now()),
        failed: null
      };
      ladderCall(r, body, function (answer) {
        r.rung = answer.attach && typeof answer.attach.rung === 'string' ? answer.attach.rung : null;
        info('ladder: attach', r.rung, 'then', JSON.stringify(answer.then || []), 'clock', answer.clock ? 'epoch' : 'none');
      });
    });
    r.timers.push(setInterval(function () { ladderCall(r, { v: 1 }); }, CLOCK_EVERY_MS));
  };

  /** Stop the current load's work (a stop, a new LOAD, a dead token). */
  P.stop = function () {
    if (!run) { return; }
    run.stopped = true;
    if (run.watchdog) { run.watchdog.stop(); }
    for (var i = 0; i < run.timers.length; i++) { clearInterval(run.timers[i]); }
    run = null;
  };

  /** The `transport` part of a receiver report, or null before start(). */
  P.transport = function () {
    if (!run) { return null; }
    var clock = run.clock.usable();
    var method = clock ? run.latency.method : 'unavailable';
    return {
      rung: run.rung,
      first_frame_ms: run.firstFrameMs,
      latency_method: method,
      latency_ms: method === 'unavailable' ? null : run.latency.last,
      clock_error_ms: clock ? Math.round(clock.error_us / 1000) : null,
      resets: { soft: 0, hard: 0, last: null },
      stalled: run.stalled
    };
  };

  P.BUILD = 'probe-1';
  root.ConspectProbe = P;
}(this));
