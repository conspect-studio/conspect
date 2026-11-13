/*
 * Conspect Cast receiver (Google Cast custom Web Receiver, CAF v3). Plain script, no build step, ES5 syntax only
 * (Chromecast firmware runs old Chrome). Served by the engine at /cast/receiver/ under a strict CSP (no inline
 * script, no eval).
 *
 * CAF facts this file relies on (read from cast_receiver_framework.js 3.0.0157 and proven in harness/):
 *  - CAF plays HLS with Shaka Player (default 4.15.56, loaded from ajax.googleapis.com) when useShakaForHls is true.
 *    Its Shaka config is {abr.defaultBandwidthEstimate: initialBandwidth || 2e6, streaming.rebufferingGoal:
 *    autoResumeDuration || 10, stallThreshold 5, jumpLargeGaps, retryParameters 4 x 400 ms x2 / 30 s}, then
 *    playbackConfig.shakaConfig is merged over it key by key (our values win).
 *  - A LOAD interceptor may return the request (accept) or an ErrorData of type LOAD_FAILED (reject); CAF then
 *    answers the sender with LOAD_FAILED and fires ERROR 905.
 *  - ERROR events carry detailedErrorCode, severity (1 recoverable, 2 critical), reason and, for Shaka,
 *    error.shakaErrorCode / error.shakaErrorData. Only critical errors end playback (MEDIA_FINISHED, IDLE/ERROR).
 *    A token that starts answering 404 is seen either on the report POST (measured: error screen after 2.3 s,
 *    8 requests) or as a fatal Shaka 1001 once its request retries are used up (7.3 s, 26 requests). A 404 on a
 *    segment alone does not prove the token is gone, so this file asks master.m3u8 before it ends the cast.
 *  - Command bits: PAUSE 1, SEEK 2, STREAM_VOLUME 4, STREAM_MUTE 8. LOAD, PLAY, STOP, GET_STATUS are always on.
 *  - When the last sender disconnects with reason "requested_by_sender" CAF stops the app, unless a handler was
 *    installed with context.setLastSenderDisconnectedHandler (exported, not in the reference docs). Any other
 *    reason leaves the app running. With disableIdleTimeout CAF never closes an idle app: this file does it.
 *  - cast.framework.CastReceiverContext#sendCustomMessage(ns, undefined, obj) broadcasts to every sender.
 *  - At the start of every rebuffer CAF reports BUFFERING, PLAYING (about 0.5 s), BUFFERING: PLAYING alone does
 *    not mean the picture moves (see reallyPlaying).
 *  - After a stall Shaka resumes where it froze, so the player stays later by the length of the stall (see tick).
 *  - CAF loads Shaka from a protocol-relative URL: this page must be served over HTTPS for the CSP to allow it.
 *  - supportedCommands only advertises: CAF still executes a PAUSE or SEEK that a sender sends anyway.
 * Things only a real device can show are marked UNVERIFIED.
 * No secrets here. A token is never logged: only its first 6 characters.
 */
(function () {
  'use strict';

  var NS = 'urn:x-cast:studio.conspect.cast';
  var LOG = '[conspect-cast]';
  var PATH_RE = /^\/cast\/([A-Za-z0-9_-]{20,128})\/master\.m3u8$/;
  var DEBUG_NS = 'urn:x-cast:com.google.cast.debuglogger';
  var DEBUG_SCRIPT = 'https://www.gstatic.com/cast/sdk/libs/devtools/debug_layer/caf_receiver_logger.js';

  var RETRY_DELAYS_MS = [2000, 4000, 8000];   // re-issue the load after a fatal error
  var RETRY_RESET_PLAYING_MS = 30000;         // a retry budget is restored after this much PLAYING
  var MAX_LOAD_AGE_MS = 12 * 3600 * 1000;     // the token's absolute lifetime: no retry after it
  var REPORT_EVERY_MS = 5000;
  var POST_MIN_GAP_MS = 1000;
  var BUFFERING_RELOAD_MS = 20000;            // stuck in BUFFERING with no error: one reload
  var TERMINAL_STOP_MS = 20000;               // on the "ended or expired" screen, then close the app
  var PAUSED_STOP_MS = 20 * 60 * 1000;        // paused this long (TV on another input, or a voice pause): close the app
  var IDLE_STOP_MS = 5 * 60 * 1000;           // nothing loaded (or a final error): close the app (CAF's default)
  var LATENCY_SEEK_S = 25;                    // further behind live than this: seek to the live edge once
  var CATCH_UP_BEHIND_S = 5;                  // this far behind the live position after a stall ...
  var CATCH_UP_AFTER_PLAYING_MS = 120000;     // ... and playing cleanly for 2 minutes: seek to the live edge
  var PROBE_MIN_GAP_MS = 3000;

  // Measured in harness/ on the 2 s / 10-segment reference stream (REPORT.md): the picture moves 0.3 to 1.4 s
  // after LOAD (CAF's default rebufferingGoal of 10 s: 4.3 to 6.5 s). Shaka starts PRESENTATION_DELAY_S behind
  // the end of the newest segment; measured latency was 6.4 to 9.8 s, that is delay - 0.1 to + 3.3 s (where in
  // the 2 s segment cycle the load falls, plus the time to fetch a new segment). That 3.4 s spread only fits the
  // 6 to 10 s target with a delay of 6.5. The buffer swings between 4.2 and 6.7 s; a delay of 6 held 3.5 to 5.5 s
  // and froze for 0.25 s in a 5 s network hiccup, where 6.5 kept 1.1 to 1.5 s and did not.
  var PRESENTATION_DELAY_S = 6.5;
  var RETRY = { maxAttempts: 6, baseDelay: 500, backoffFactor: 1.5, fuzzFactor: 0.5, timeout: 8000 };
  var SHAKA_CONFIG = {
    streaming: {
      bufferingGoal: 12, rebufferingGoal: 4, bufferBehind: 16, stallThreshold: 3, retryParameters: RETRY
    },
    manifest: { retryParameters: RETRY, defaultPresentationDelay: PRESENTATION_DELAY_S }
  };

  var SENTENCE = {
    foreign: 'This receiver only plays Conspect cast streams',
    ended: 'The cast stream has ended or its link expired. Start the cast again.',
    network: 'The TV cannot reach the Conspect server.',
    decode: 'This TV cannot decode the stream',
    generic: 'Playback failed'
  };

  var context = cast.framework.CastReceiverContext.getInstance();
  var playerManager = context.getPlayerManager();
  var EventType = cast.framework.events.EventType;
  var messages = cast.framework.messages;

  var overlay = document.getElementById('overlay');
  var elBusyTitle = document.getElementById('busy-title');
  var elBusyLine = document.getElementById('busy-line');
  var elErrSentence = document.getElementById('error-sentence');
  var elErrCode = document.getElementById('error-code');

  // The last accepted load: {url, base, tag, title, at, media, customData, codecs}. null = nothing to play.
  var current = null;
  var state = 'idle';            // idle | loading | buffering | playing | error
  var lastError = null;          // the error part of a report
  var terminal = false;          // the token is gone: no retry
  var selfLoad = false;          // the LOAD in the interceptor is our own retry
  var retryCount = 0;
  var retryTimer = null;
  var bufferingTimer = null;
  var stopTimer = null;
  var playingSince = 0;
  var playingResetTimer = null;
  var bufferingReloadDone = false;
  var liveSeekAt = 0;
  var rejectedAt = 0;
  var idleTimer = null;
  var pausedTimer = null;
  var lastPostAt = 0;
  var lastProbeAt = 0;
  var stalls = 0;                // BUFFERING entered after the first PLAYING of a load
  var hadPlaying = false;

  function info() {
    var a = Array.prototype.slice.call(arguments);
    a.unshift(LOG);
    try { console.info.apply(console, a); } catch (e) { /* console missing */ }
  }

  /** Text safe for logs and reports: a token in a URL or path is cut to its first 6 characters. */
  function redact(text) {
    return String(text).replace(/\/cast\/([A-Za-z0-9_-]{6})[A-Za-z0-9_-]{14,}/g, '/cast/$1...');
  }

  function now() { return Date.now(); }

  function clear(timer) { if (timer !== null) { clearTimeout(timer); } return null; }

  function mediaElement() {
    var el = document.getElementById('player');
    try {
      if (el && typeof el.getMediaElement === 'function') { return el.getMediaElement(); }
      if (el && el.shadowRoot) { return el.shadowRoot.querySelector('video'); }
    } catch (e) { /* not upgraded yet */ }
    return document.querySelector('video');
  }

  function setState(next, why) {
    if (state !== next) { info('state', state, '->', next, why || ''); }
    state = next;
    overlay.setAttribute('data-state', next);
    var title = current && current.title ? current.title : '';
    elBusyTitle.textContent = title;
    elBusyLine.textContent = next === 'buffering' ? 'Buffering' : (retryCount > 0 ? 'Reconnecting' : 'Connecting');
    if (next === 'error' && lastError) {
      elErrSentence.textContent = lastError.message;
      elErrCode.textContent = codeLine(lastError);
    }
  }

  function codeLine(err) {
    var parts = [];
    if (err.code !== null && err.code !== undefined) { parts.push(String(err.code)); }
    if (err.detailed && err.detailed !== err.code) { parts.push(String(err.detailed)); }
    if (err.shaka_code) { parts.push('shaka ' + err.shaka_code); }
    if (err.http) { parts.push('http ' + err.http); }
    return parts.join(' / ');
  }

  // ---------------------------------------------------------------- stats and reports

  function num(v, digits) {
    if (typeof v !== 'number' || !isFinite(v)) { return null; }
    var m = Math.pow(10, digits || 0);
    return Math.round(v * m) / m;
  }

  function bufferAhead(video) {
    try {
      var b = video.buffered;
      var t = video.currentTime;
      for (var i = 0; i < b.length; i++) {
        if (t >= b.start(i) - 0.25 && t <= b.end(i)) { return b.end(i) - t; }
      }
    } catch (e) { /* no buffer yet */ }
    return 0;
  }

  /**
   * Seconds behind the player's own live position (the end of the seekable range, which Shaka keeps
   * PRESENTATION_DELAY_S behind the newest segment). About 0 in normal play; grows by the length of every stall.
   * Does not depend on the TV's wall clock.
   */
  function behindLive() {
    try {
      var range = playerManager.getLiveSeekableRange();
      if (range && typeof range.end === 'number' && range.end > 0) {
        return Math.max(0, range.end - playerManager.getCurrentTimeSec());
      }
    } catch (e) { /* not live yet */ }
    return null;
  }

  /** Seconds behind live: Shaka's liveLatency (PDT against the TV clock) or, without it, the seekable range. */
  function latency(st) {
    if (st && typeof st.liveLatency === 'number' && isFinite(st.liveLatency) && st.liveLatency > 0) {
      return st.liveLatency;
    }
    var behind = hadPlaying ? behindLive() : null;      // before the first frame the playhead is not placed yet
    return behind === null ? null : behind + PRESENTATION_DELAY_S;
  }

  function buildStats() {
    if (!current) { return null; }
    var st = {};
    try { st = playerManager.getStats() || {}; } catch (e) { st = {}; }
    var video = mediaElement();
    var quality = null;
    try { quality = video && video.getVideoPlaybackQuality ? video.getVideoPlaybackQuality() : null; } catch (e2) { quality = null; }
    return {
      width: num(st.width || (video && video.videoWidth)),
      height: num(st.height || (video && video.videoHeight)),
      bitrate: num(st.streamBandwidth),
      buffer_s: video ? num(bufferAhead(video), 1) : null,
      latency_s: num(latency(st), 1),
      decoded: num(quality ? quality.totalVideoFrames : st.decodedFrames),
      dropped: num(quality ? quality.droppedVideoFrames : st.droppedFrames),
      stalls: stalls,
      current_time: video ? num(video.currentTime, 1) : null
    };
  }

  function shakaVersion() {
    try { return window.shaka && window.shaka.Player ? String(window.shaka.Player.version) : ''; } catch (e) { return ''; }
  }

  function buildReport() {
    return {
      v: 1,
      state: state,
      error: lastError ? {
        code: lastError.code, detailed: lastError.detailed, reason: lastError.reason,
        shaka_code: lastError.shaka_code, shaka_category: lastError.shaka_category,
        severity: lastError.severity, message: lastError.message
      } : null,
      stats: buildStats(),
      shaka: shakaVersion(),
      caf: String(cast.framework.VERSION || ''),
      device: String(navigator.userAgent).slice(0, 200),
      at_ms: now()
    };
  }

  function broadcast(report, senderId) {
    try {
      if (!senderId && context.getSenders().length === 0) { return; }
      context.sendCustomMessage(NS, senderId, report);
    } catch (e) { info('custom message not sent:', redact(e && e.message ? e.message : e)); }
  }

  /** POST the report to <token base>report: at most one per second, dropped (not queued) when too soon. */
  function post(report) {
    if (!current || terminal) { return; }
    var t = now();
    if (t - lastPostAt < POST_MIN_GAP_MS) { return; }
    lastPostAt = t;
    var body = JSON.stringify(report);
    if (body.length > 2000) { report.device = report.device.slice(0, 60); body = JSON.stringify(report); }
    if (body.length > 2000) { return; }
    try {
      var xhr = new XMLHttpRequest();
      xhr.open('POST', current.base + 'report', true);
      xhr.setRequestHeader('Content-Type', 'application/json');
      xhr.timeout = 4000;
      xhr.onload = function () {
        // The token no longer exists on the server: confirm on master.m3u8 and end the cast.
        if (xhr.status === 404 || xhr.status === 403 || xhr.status === 410) { checkToken('report ' + xhr.status); }
      };
      xhr.onerror = function () { /* ignored: reports are best effort */ };
      xhr.ontimeout = xhr.onerror;
      xhr.send(body);
    } catch (e) { /* ignored */ }
  }

  function report(why) {
    var r = buildReport();
    broadcast(r);
    post(r);
    if (why) {
      info('report', why, r.state, r.stats ? JSON.stringify(r.stats) : '', r.error ? codeLine(lastError) : '');
    }
  }

  // ---------------------------------------------------------------- token probe

  /** GET master.m3u8. cb(status, text): status 0 = unreachable. Also learns the H.264 profile/level for messages. */
  function probe(cb) {
    if (!current) { cb(0, ''); return; }
    var target = current;
    var done = false;
    var finish = function (status, text) {
      if (done) { return; }
      done = true;
      if (status === 200) {
        var m = /avc1\.([0-9A-Fa-f]{2})[0-9A-Fa-f]{2}([0-9A-Fa-f]{2})/.exec(text || '');
        if (m) {
          var profiles = { '42': 'Baseline', '4d': 'Main', '58': 'Extended', '64': 'High', '6e': 'High 10' };
          var p = profiles[m[1].toLowerCase()] || ('profile ' + parseInt(m[1], 16));
          target.codec = 'H.264 ' + p + ' level ' + (parseInt(m[2], 16) / 10).toFixed(1);
        }
      }
      cb(status, text || '');
    };
    try {
      var xhr = new XMLHttpRequest();
      xhr.open('GET', target.url, true);
      xhr.timeout = 5000;
      xhr.onload = function () { finish(xhr.status, xhr.responseText); };
      xhr.onerror = function () { finish(0, ''); };
      xhr.ontimeout = xhr.onerror;
      xhr.send();
    } catch (e) { finish(0, ''); }
  }

  function gone(status) { return status === 404 || status === 403 || status === 410 || status === 401; }

  /** Something answered 404/403 under the token: ask master.m3u8 (at most every 3 s) and end the cast if it is gone. */
  function checkToken(why) {
    if (!current || terminal) { return; }
    var t = now();
    if (t - lastProbeAt < PROBE_MIN_GAP_MS) { return; }
    lastProbeAt = t;
    probe(function (status) {
      info('token check after', why, '-> master', status);
      if (gone(status)) { endCast(status); }
    });
  }

  // ---------------------------------------------------------------- errors

  /** Turn a CAF ERROR event into the error part of a report, with an operator sentence. */
  function describe(e) {
    var inner = e && e.error ? e.error : {};
    var data = inner.shakaErrorData;
    var shakaCode = typeof inner.shakaErrorCode === 'number' ? inner.shakaErrorCode : null;
    var category = shakaCode !== null ? Math.floor(shakaCode / 1000) : null;
    var http = null;
    var uri = '';
    if (data && data.length) {
      if (typeof data[0] === 'string') { uri = data[0]; }
      if (shakaCode === 1001 && typeof data[1] === 'number') { http = data[1]; }
    }
    var detailed = typeof e.detailedErrorCode === 'number' ? e.detailedErrorCode : null;
    // CAF's MPL-style codes carry the HTTP status in the tail (3116404 = 311 + closure code 6 + 404).
    if (http === null && detailed !== null && detailed > 1000000) { http = detailed % 1000; }
    var kind = 'generic';
    if (gone(http)) {
      kind = 'ended';
    } else if (shakaCode === 1002 || shakaCode === 1003 || category === 1 || http !== null ||
        (detailed !== null && detailed >= 300 && detailed < 400) || detailed === 103) {
      kind = 'network';
    } else if (category === 3 || shakaCode === 4032 || shakaCode === 4033 ||
        (detailed !== null && (detailed === 102 || detailed === 104 || detailed === 110))) {
      kind = 'decode';
    }
    var message = SENTENCE[kind];
    if (kind === 'decode') { message += ' (' + (current && current.codec ? current.codec : 'H.264') + ').'; }
    var err = {
      code: detailed, detailed: detailed, reason: e.reason || inner.reason || null,
      shaka_code: shakaCode, shaka_category: category,
      severity: typeof e.severity === 'number' ? e.severity : (inner.shakaErrorSeverity || null),
      message: message, kind: kind, http: http, master: /\/master\.m3u8(\?|$)/.test(uri)
    };
    if (kind === 'generic') { err.message = SENTENCE.generic + ' (' + (codeLine(err) || 'unknown') + ').'; }
    return err;
  }

  function showError(err, why) {
    lastError = err;
    setState('error', why);
    report('error');
    armIdleStop();
  }

  /** The token is revoked or expired: final screen, stop the player (no request storm), then close the app. */
  function endCast(status) {
    if (terminal) { return; }
    terminal = true;
    retryTimer = clear(retryTimer);
    bufferingTimer = clear(bufferingTimer);
    var base = lastError || {};
    lastError = {
      code: base.code || 311, detailed: base.detailed || null, reason: base.reason || null,
      shaka_code: base.shaka_code || null, shaka_category: base.shaka_category || null,
      severity: 2, message: SENTENCE.ended, kind: 'ended', http: status
    };
    setState('error', 'token gone (' + status + ')');
    broadcast(buildReport());
    try { playerManager.stop(); } catch (e) { /* nothing loaded */ }
    stopTimer = clear(stopTimer);
    stopTimer = setTimeout(function () { stopApp('stream ended'); }, TERMINAL_STOP_MS);
  }

  function stopApp(why) {
    info('closing the receiver app:', why);
    try { context.stop(); } catch (e) { info('context.stop failed', e && e.message); }
  }

  /** With disableIdleTimeout CAF never closes an idle app, so do it here after 5 minutes without a stream. */
  function armIdleStop() {
    idleTimer = clear(idleTimer);
    idleTimer = setTimeout(function () {
      if (state === 'idle' || state === 'error') { stopApp('idle for 5 minutes'); }
    }, IDLE_STOP_MS);
  }

  // ---------------------------------------------------------------- recovery

  function reload(why) {
    if (!current || terminal) { return; }
    info('reload:', why, 'stream', current.tag);
    var media = new messages.MediaInformation();
    media.contentId = current.url;
    media.contentUrl = current.url;
    media.customData = current.customData;
    media.metadata = current.metadata;
    var request = new messages.LoadRequestData();
    request.media = media;
    request.autoplay = true;
    selfLoad = true;
    try {
      var p = playerManager.load(request);
      if (p && p.catch) { p.catch(function (e) { info('reload rejected', redact(e && (e.reason || e.type || e.message) || e)); }); }
    } catch (e) { info('reload threw', redact(e && e.message)); }
  }

  /** After a fatal error or an unexpected end: probe the token, then load again after 2, 4, 8 s (3 attempts). */
  function scheduleRetry(err, why) {
    if (!current || terminal || retryTimer !== null) { return; }
    if (err) { lastError = err; }
    if (now() - current.at > MAX_LOAD_AGE_MS) { showError(lastError || describe({}), 'load older than 12 h'); return; }
    if (retryCount >= RETRY_DELAYS_MS.length) { showError(lastError || describe({}), 'retries used up'); return; }
    var delay = RETRY_DELAYS_MS[retryCount];
    retryCount += 1;
    info('retry', retryCount, 'of', RETRY_DELAYS_MS.length, 'in', delay, 'ms:', why);
    setState('loading', 'retry');
    report('retry');
    retryTimer = setTimeout(function () {
      retryTimer = null;
      probe(function (status) {
        if (gone(status)) { endCast(status); return; }
        reload('retry ' + retryCount + ' (master ' + status + ')');
      });
    }, delay);
  }

  function onError(e) {
    if (e && e.detailedErrorCode === 905 && now() - rejectedAt < 2000) { return; }  // our own LOAD rejection
    var err = describe(e || {});
    info('error', codeLine(err), 'severity', err.severity, 'kind', err.kind, err.master ? 'on master' : '');
    if (terminal || !current) { return; }
    // After a failed load CAF also fires a bare 905 (LOAD_FAILED) with no cause. The event before it carried the
    // cause (network, decode, ...): keep that one for the screen and the report.
    if (err.code === 905 && err.shaka_code === null && lastError && lastError.kind !== 'generic' &&
        (retryTimer !== null || state === 'error')) { return; }
    if (err.kind === 'ended') {
      lastError = err;
      if (err.master) { endCast(err.http); return; }
      checkToken('http ' + err.http);
    }
    if (err.severity === 1) {
      // Recoverable: the player goes on (Shaka retries live requests by itself). Shown in reports while not playing.
      if (state !== 'playing') { lastError = err; }
      return;
    }
    scheduleRetry(err, 'fatal error');
  }

  function onFinished(e) {
    var reason = e && e.endedReason;
    info('media finished:', reason);
    pausedTimer = clear(pausedTimer);
    bufferingTimer = clear(bufferingTimer);
    playingResetTimer = clear(playingResetTimer);
    if (terminal) { return; }
    var R = cast.framework.events.EndedReason;
    if (reason === R.INTERRUPTED || reason === R.LOAD_CANCELLED || reason === R.PRELOAD_INTERRUPTED) { return; }
    if (current && (reason === R.ERROR || reason === R.END_OF_STREAM)) {
      if (state !== 'error') { scheduleRetry(null, 'ended: ' + reason); }   // 'error': the retries are used up
      return;
    }
    // A sender stopped the stream.
    if (state === 'error' && !current) { return; }   // keep the rejected-LOAD screen
    current = null;
    lastError = null;
    retryTimer = clear(retryTimer);
    setState('idle', 'stopped');
    report('finished');
    armIdleStop();
  }

  // ---------------------------------------------------------------- player events

  function onLoading() {
    idleTimer = clear(idleTimer);
    stopTimer = clear(stopTimer);
    hadPlaying = false;
    if (current) { setState('loading', 'player loading'); report('loading'); }
  }

  function armBufferingWatchdog() {
    if (bufferingTimer !== null) { return; }
    bufferingTimer = setTimeout(function () {
      bufferingTimer = null;
      if (!current || terminal || state === 'playing') { return; }
      probe(function (status) {
        if (gone(status)) { endCast(status); return; }
        if (!bufferingReloadDone) {
          bufferingReloadDone = true;
          reload('buffering for ' + (BUFFERING_RELOAD_MS / 1000) + ' s (master ' + status + ')');
          armBufferingWatchdog();
        } else {
          scheduleRetry(null, 'still buffering after a reload');
        }
      });
    }, BUFFERING_RELOAD_MS);
  }

  function onBuffering(e) {
    if (!current || terminal || retryTimer !== null) { return; }
    if (e && e.isBuffering === false) { onPlaying(); return; }
    if (state === 'buffering') { return; }
    if (hadPlaying && state === 'playing') { stalls += 1; }
    playingResetTimer = clear(playingResetTimer);
    setState('buffering', 'player buffering');
    armBufferingWatchdog();
    report('buffering');
  }

  /**
   * CAF reports PLAYING for about half a second at the start of every rebuffer (BUFFERING, PLAYING, BUFFERING).
   * The picture really moves when the element has data ahead and Shaka has not set its rate to 0 (its way to hold
   * playback while buffering).
   */
  function reallyPlaying() {
    var video = mediaElement();
    return playerManager.getPlayerState() === messages.PlayerState.PLAYING && !!video && !video.paused &&
        video.readyState >= 3 && video.playbackRate > 0;
  }

  /** Twice a second: promote to "playing" once the picture really moves (see reallyPlaying). */
  function sync() {
    if (!current || terminal || retryTimer !== null || state === 'playing' || state === 'error') { return; }
    if (reallyPlaying()) { onPlaying(); }
  }

  function onPlaying() {
    if (!current || terminal || !reallyPlaying()) { return; }
    bufferingTimer = clear(bufferingTimer);
    retryTimer = clear(retryTimer);
    idleTimer = clear(idleTimer);
    pausedTimer = clear(pausedTimer);
    var was = state;
    hadPlaying = true;
    lastError = null;
    setState('playing', 'player playing');
    if (was !== 'playing') {
      playingSince = now();
      playingResetTimer = clear(playingResetTimer);
      playingResetTimer = setTimeout(function () {
        playingResetTimer = null;
        if (retryCount > 0 || bufferingReloadDone) { info('stable for 30 s: retry budget restored'); }
        retryCount = 0;
        bufferingReloadDone = false;
      }, RETRY_RESET_PLAYING_MS);
      report('playing');
    }
  }

  function onPause() {
    // Live only: senders cannot pause (no PAUSE command), but CAF pauses when the TV hides the app.
    if (!current || terminal || retryTimer !== null) { return; }
    if (playerManager.getPlayerState() !== messages.PlayerState.PAUSED) { return; }   // PAUSE also fires on teardown
    playingResetTimer = clear(playingResetTimer);
    setState('buffering', 'paused');
    elBusyLine.textContent = 'Paused';
    report('paused');
    // UNVERIFIED: a paused live player is expected to keep refreshing its playlists, which would keep the token
    // (and the engine's encoder) alive for the full 12 h. Closing after 20 minutes bounds that either way.
    if (pausedTimer === null) {
      pausedTimer = setTimeout(function () {
        pausedTimer = null;
        if (playerManager.getPlayerState() === messages.PlayerState.PAUSED) { stopApp('paused for 20 minutes'); }
      }, PAUSED_STOP_MS);
    }
  }

  /** UNVERIFIED on a device: CAF pauses on visibilitychanged(false); resume when the app is visible again. */
  function onVisibility(e) {
    info('visibility', e && e.isVisible);
    if (e && e.isVisible && current && !terminal &&
        playerManager.getPlayerState() === messages.PlayerState.PAUSED) {
      playerManager.play();
    }
  }

  /** Every 5 s: report, and when far behind the live edge (after a long stall or pause) jump to it once. */
  function tick() {
    if (!current) { return; }
    var r = buildReport();
    broadcast(r);
    post(r);
    var lat = r.stats ? r.stats.latency_s : null;
    var behind = behindLive();
    if (state !== 'playing' || lat === null || behind === null || now() - liveSeekAt < 60000) { return; }
    // (1) Far behind (a long pause): at once. Both figures must agree, because a wrong TV clock inflates `lat`.
    //     With a 20 s playlist window Shaka itself moves the playhead before this can happen (measured).
    // (2) A stall leaves the player that much later for good (measured: 7.5 s -> 16 s after one 12 s stall). The
    //     extra buffer is kept while the network is unsteady and given back after 2 minutes of clean playback.
    var far = lat > LATENCY_SEEK_S && behind > LATENCY_SEEK_S - PRESENTATION_DELAY_S - 4;
    var late = behind > CATCH_UP_BEHIND_S && now() - playingSince > CATCH_UP_AFTER_PLAYING_MS;
    if (far || late) {
      var range = playerManager.getLiveSeekableRange();
      liveSeekAt = now();
      // The range end is already PRESENTATION_DELAY_S behind the newest segment; stay 1 s inside it.
      var target = Math.max(range.start || 0, range.end - 1);
      info('latency', lat, 's,', num(behind, 1), 's behind the live position: seeking to the live edge at', num(target, 1));
      playerManager.seek(target);
    }
  }

  // ---------------------------------------------------------------- LOAD

  /** Accept only http(s)://<our origin>/cast/<token>/master.m3u8. Returns {url, base, tag} or null. */
  function parseStreamUrl(raw) {
    if (typeof raw !== 'string' || raw.length === 0 || raw.length > 512) { return null; }
    var u;
    try { u = new URL(raw); } catch (e) { return null; }
    if (u.protocol !== 'http:' && u.protocol !== 'https:') { return null; }
    if (u.origin !== location.origin || u.username || u.password) { return null; }
    var m = PATH_RE.exec(u.pathname);
    if (!m) { return null; }
    return { url: u.origin + u.pathname, base: u.origin + '/cast/' + m[1] + '/', tag: m[1].slice(0, 6) };
  }

  function loadDebugOverlay() {
    if (window.cast && cast.debug && cast.debug.CastDebugLogger) { enableDebugOverlay(); return; }
    var s = document.createElement('script');
    s.src = DEBUG_SCRIPT;
    s.onload = enableDebugOverlay;
    s.onerror = function () { info('debug overlay script blocked or unreachable'); };
    document.head.appendChild(s);
  }

  function enableDebugOverlay() {
    try {
      var logger = cast.debug.CastDebugLogger.getInstance();
      logger.setEnabled(true);
      logger.showDebugLogs(true);
      info('debug overlay enabled by customData.debug');
    } catch (e) { info('debug overlay failed', e && e.message); }
  }

  function onLoad(request) {
    var mine = selfLoad;
    selfLoad = false;
    var media = request && request.media ? request.media : null;
    var raw = media ? (media.contentUrl || media.contentId || media.entity || '') : '';
    var parsed = parseStreamUrl(raw);
    if (!parsed) {
      info('LOAD rejected: not a Conspect cast stream URL:', redact(String(raw).slice(0, 120)));
      rejectedAt = now();
      current = null;
      terminal = false;
      retryTimer = clear(retryTimer);
      bufferingTimer = clear(bufferingTimer);
      lastError = {
        code: 905, detailed: 905, reason: messages.ErrorReason.INVALID_REQUEST, shaka_code: null,
        shaka_category: null, severity: 2, message: SENTENCE.foreign, kind: 'foreign', http: null
      };
      setState('error', 'load rejected');
      broadcast(buildReport());
      armIdleStop();
      var failure = new messages.ErrorData(messages.ErrorType.LOAD_FAILED);
      failure.reason = messages.ErrorReason.INVALID_REQUEST;
      return failure;
    }
    var custom = media.customData || {};
    var title = typeof custom.title === 'string' ? custom.title :
        (media.metadata && typeof media.metadata.title === 'string' ? media.metadata.title : '');
    if (!mine || !current) {
      retryCount = 0;
      bufferingReloadDone = false;
      stalls = 0;
      liveSeekAt = 0;
      lastError = null;
      current = { at: now(), codec: '' };
    }
    terminal = false;
    retryTimer = clear(retryTimer);
    bufferingTimer = clear(bufferingTimer);
    stopTimer = clear(stopTimer);
    current.url = parsed.url;
    current.base = parsed.base;
    current.tag = parsed.tag;
    current.title = title.slice(0, 80);
    current.customData = custom;

    media.contentUrl = parsed.url;
    media.contentType = 'application/vnd.apple.mpegurl';
    media.streamType = messages.StreamType.LIVE;
    media.hlsSegmentFormat = messages.HlsSegmentFormat.FMP4;
    media.hlsVideoSegmentFormat = messages.HlsVideoSegmentFormat.FMP4;
    media.duration = -1;
    if (!media.metadata) {
      // Senders show this title. It also stops CAF logging "Cannot find section metadata" as an error on every
      // live load (its live programme-guide lookup runs only when media.metadata is absent).
      media.metadata = new messages.GenericMediaMetadata();
      media.metadata.title = current.title || 'Conspect';
    }
    current.metadata = media.metadata;
    request.autoplay = true;
    request.currentTime = undefined;       // always start at the live edge
    info('LOAD accepted: stream', parsed.tag, mine ? '(retry ' + retryCount + ')' : '', 'title:', current.title);
    if (!mine) {
      // One GET of master.m3u8: learns the H.264 profile/level for the decode message and ends at once on a dead token.
      probe(function (status) { if (gone(status)) { endCast(status); } });
    }
    if (custom.debug === true) { loadDebugOverlay(); }
    setState('loading', 'load accepted');
    return request;
  }

  // ---------------------------------------------------------------- start

  playerManager.setMessageInterceptor(messages.MessageType.LOAD, onLoad);
  playerManager.addEventListener(EventType.PLAYER_LOADING, onLoading);
  playerManager.addEventListener(EventType.BUFFERING, onBuffering);
  playerManager.addEventListener(EventType.PLAYING, onPlaying);
  playerManager.addEventListener(EventType.PAUSE, onPause);
  playerManager.addEventListener(EventType.MEDIA_FINISHED, onFinished);
  playerManager.addEventListener(EventType.ERROR, onError);

  context.addCustomMessageListener(NS, function (e) {
    if (e && e.data && e.data.type === 'ping') { broadcast(buildReport(), e.senderId); }
  });
  context.addEventListener(cast.framework.system.EventType.VISIBILITY_CHANGED, onVisibility);
  context.addEventListener(cast.framework.system.EventType.SENDER_DISCONNECTED, function (e) {
    info('sender disconnected:', e && e.reason, 'senders left:', context.getSenders().length);
  });
  // CAF's default closes the app when the LAST sender disconnects explicitly ("requested_by_sender"). A live cast
  // must outlive its sender (a closed tab, a phone leaving), so replace that: a cast ends by the platform STOP
  // (the sender's endSession(true)), by its token ending, or by our idle timer.
  if (typeof context.setLastSenderDisconnectedHandler === 'function') {
    context.setLastSenderDisconnectedHandler(function () {
      if (current && !terminal) { info('last sender left: the stream keeps playing'); return; }
      stopApp('last sender left and nothing is playing');
    });
  }

  var options = new cast.framework.CastReceiverOptions();
  options.useShakaForHls = true;
  options.disableIdleTimeout = true;
  options.customNamespaces = {};
  options.customNamespaces[NS] = cast.framework.system.MessageType.JSON;
  // Google's debug overlay (loaded only for customData.debug) listens here; namespaces cannot be added after start().
  options.customNamespaces[DEBUG_NS] = cast.framework.system.MessageType.JSON;
  options.supportedCommands = messages.Command.STREAM_VOLUME | messages.Command.STREAM_MUTE;
  // The mask is advertised to senders (they hide pause and seek); a PAUSE or SEEK sent anyway (voice) is still
  // executed, and this file then returns to the live edge. enforceSupportedCommands is NOT set: measured, it also
  // refuses this file's own playerManager.seek() and CAF's own pause when the TV hides the app.
  options.playbackConfig = new cast.framework.PlaybackConfig();
  options.playbackConfig.autoResumeDuration = 4;
  options.playbackConfig.initialBandwidth = 6e6;
  options.playbackConfig.shakaConfig = SHAKA_CONFIG;

  setState('idle', 'start');
  context.start(options);
  setInterval(tick, REPORT_EVERY_MS);
  setInterval(sync, 500);
  armIdleStop();
  info('receiver started: CAF', cast.framework.VERSION, 'namespace', NS);
}());
