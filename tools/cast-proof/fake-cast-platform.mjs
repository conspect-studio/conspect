// Fake Cast platform IPC for the REAL CAF receiver SDK running in a plain browser (no cast.__platform__):
// the SDK opens ws://localhost:8008/v2/ipc and exchanges frames {namespace, senderId, data: <JSON string>}.
// It sends `ready` + `startheartbeat` on the system namespace; this module answers `ready` and `senderconnected`
// and then plays the part of the senders. Listens on BOTH 127.0.0.1 and [::1] (Chrome picks either for localhost).
import { EventEmitter } from 'node:events';
import { WebSocketServer } from 'ws';

export const NS_SYSTEM = 'urn:x-cast:com.google.cast.system';
export const NS_MEDIA = 'urn:x-cast:com.google.cast.media';
export const NS_CONSPECT = 'urn:x-cast:studio.conspect.cast';

export async function startFakePlatform(opts = {}) {
  const port = opts.port || 8008;
  const firstSender = opts.senderId || 'sender-0';
  const bus = new EventEmitter();
  bus.setMaxListeners(100);
  const events = [];            // everything the receiver sent: {at, namespace, senderId, data}
  const state = { socket: null, ready: false, readyAt: 0, closedAt: 0, closeCode: null, requestId: 0, volume: { level: 1, muted: false }, senders: new Set() };

  const servers = [];
  for (const host of ['127.0.0.1', '::1']) {
    const wss = new WebSocketServer({ host, port, path: '/v2/ipc' });
    await new Promise((resolve, reject) => { wss.once('listening', resolve); wss.once('error', (e) => reject(new Error(`fake platform cannot listen on ${host}:${port}: ${e.message}`))); });
    wss.on('connection', (sock) => attach(sock));
    servers.push(wss);
  }

  function send(namespace, senderId, data) {
    if (!state.socket || state.socket.readyState !== 1) throw new Error('receiver IPC is not connected');
    state.socket.send(JSON.stringify({ namespace, senderId, data: typeof data === 'string' ? data : JSON.stringify(data) }));
  }

  function attach(sock) {
    state.socket = sock; state.closedAt = 0;
    sock.on('close', (code) => { state.closedAt = Date.now(); state.closeCode = code; state.ready = false; bus.emit('closed', code); });
    sock.on('message', (raw) => {
      let frame; try { frame = JSON.parse(raw.toString()); } catch { return; }
      let data = frame.data; try { data = JSON.parse(frame.data); } catch { /* string payload */ }
      const ev = { at: Date.now(), namespace: frame.namespace, senderId: frame.senderId, data };
      events.push(ev);
      if (frame.namespace === NS_SYSTEM && data && data.type === 'ready') {
        send(NS_SYSTEM, 'SystemSender', {
          type: 'ready', applicationId: opts.applicationId || 'C0N5PEC7', applicationName: 'Conspect', sessionId: 'session-1',
          launchingSenderId: firstSender, version: '1.56.500000', messagesVersion: '1.0',
          deviceCapabilities: { bluetooth_supported: false, display_supported: true, hi_res_audio_supported: false, is_device_registered: opts.deviceRegistered !== false }
        });
        api.connectSender(firstSender);
        state.ready = true; state.readyAt = Date.now();
        bus.emit('ready');
      }
      // The platform owns the volume: the SDK forwards SET_VOLUME as a system `setvolume`, we confirm it.
      if (frame.namespace === NS_SYSTEM && data && data.type === 'setvolume') {
        if (typeof data.level === 'number') state.volume.level = data.level;
        if (typeof data.muted === 'boolean') state.volume.muted = data.muted;
        send(NS_SYSTEM, 'SystemSender', { type: 'volumechanged', level: state.volume.level, muted: state.volume.muted });
      }
      bus.emit('message', ev);
    });
  }

  /** Resolve with the first received frame (past or future, at or after `since`) matching the predicate. */
  function waitFor(predicate, timeoutMs, what = 'frame', since = 0) {
    const hit = events.find((e) => e.at >= since && predicate(e));
    if (hit) return Promise.resolve(hit);
    return new Promise((resolve, reject) => {
      const onMessage = (e) => { if (e.at >= since && predicate(e)) { done(); resolve(e); } };
      const timer = setTimeout(() => { done(); reject(new Error(`timeout after ${timeoutMs} ms waiting for ${what}`)); }, timeoutMs);
      const done = () => { clearTimeout(timer); bus.off('message', onMessage); };
      bus.on('message', onMessage);
    });
  }

  const isMediaStatus = (e) => e.namespace === NS_MEDIA && e.data && e.data.type === 'MEDIA_STATUS' && Array.isArray(e.data.status);

  const api = {
    events, state, on: (name, fn) => bus.on(name, fn), off: (name, fn) => bus.off(name, fn),
    send, waitFor,
    waitForReceiver(timeoutMs = 15000) {
      if (state.ready) return Promise.resolve();
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error(`timeout after ${timeoutMs} ms waiting for the receiver's system ready`)), timeoutMs);
        bus.once('ready', () => { clearTimeout(timer); resolve(); });
      });
    },
    connectSender(senderId) { state.senders.add(senderId); send(NS_SYSTEM, 'SystemSender', { type: 'senderconnected', senderId, userAgent: 'conspect-harness' }); },
    /** reason as the platform words it: closed_by_peer (explicit), transport_closed, transport_aborted, ... */
    disconnectSender(senderId, reason = 'transport_closed') { state.senders.delete(senderId); send(NS_SYSTEM, 'SystemSender', { type: 'senderdisconnected', senderId, reason }); },
    system(data) { send(NS_SYSTEM, 'SystemSender', data); },
    media(data, senderId = firstSender) { const requestId = ++state.requestId; send(NS_MEDIA, senderId, { requestId, ...data }); return requestId; },
    load(media, extra = {}, senderId = firstSender) { return api.media({ type: 'LOAD', autoplay: true, media, ...extra }, senderId); },
    stop(senderId = firstSender) { return api.media({ type: 'STOP', mediaSessionId: api.mediaSessionId() }, senderId); },
    getStatus(senderId = firstSender) { return api.media({ type: 'GET_STATUS' }, senderId); },
    setVolume(volume, senderId = firstSender) { return api.media({ type: 'SET_VOLUME', mediaSessionId: api.mediaSessionId(), volume }, senderId); },
    sendCustom(namespace, obj, senderId = firstSender) { send(namespace, senderId, obj); },
    mediaStatuses: (since = 0) => events.filter((e) => e.at >= since && isMediaStatus(e)).map((e) => ({ at: e.at, senderId: e.senderId, requestId: e.data.requestId, ...(e.data.status[0] || {}) })),
    mediaSessionId() { const all = api.mediaStatuses(); return all.length ? all[all.length - 1].mediaSessionId : undefined; },
    lastPlayerState() { const all = api.mediaStatuses(); return all.length ? all[all.length - 1].playerState : null; },
    waitForMediaStatus(predicate, timeoutMs, since = 0) {
      return waitFor((e) => isMediaStatus(e) && e.data.status.some((s) => predicate(s, e)), timeoutMs, 'MEDIA_STATUS', since)
        .then((e) => ({ at: e.at, ...e.data.status.find((s) => predicate(s, e)) }));
    },
    custom: (namespace = NS_CONSPECT, since = 0) => events.filter((e) => e.at >= since && e.namespace === namespace),
    waitForClose(timeoutMs) {
      if (state.closedAt) return Promise.resolve(state.closedAt);
      return new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error(`timeout after ${timeoutMs} ms waiting for the receiver to close its IPC`)), timeoutMs);
        bus.once('closed', () => { clearTimeout(timer); resolve(state.closedAt); });
      });
    },
    async close() {
      for (const wss of servers) { for (const c of wss.clients) c.terminate(); await new Promise((resolve) => wss.close(() => resolve())); }
    }
  };
  return api;
}
