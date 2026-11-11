// Redacted protocol instrumentation for the ABR probe (t42).
//
// The question this answers: after the selector moves to another rendition, does
// the new track's data arrive at all, does it arrive without a decodable
// keyframe, or does it arrive and fail to decode?
//
// Three layers are observed, all of them at boundaries the page already owns:
//
//  1. WebTransport request streams (`createBidirectionalStream`) - one per MoQ
//     request. Counts open/close, bytes written and bytes read per stream, so a
//     SUBSCRIBE or UNSUBSCRIBE is visible as a request stream opening, being
//     written to, and ending. Only numeric request and alias identifiers are
//     recorded; namespace strings and credentials are never exported.
//  2. Incoming unidirectional streams - MoQ object streams, i.e. the media
//     itself. Counts how many open, when, how many bytes each carries and when
//     it ends. "No media arrived" and "media arrived" are different failures and
//     this separates them.
//  3. VideoDecoder inputs - every encoded chunk handed to WebCodecs, with its
//     `type` (`key` or `delta`), byte length, timestamp and queue depth, plus
//     configure calls and decode errors. This is the RAP question: a stream that
//     arrives as deltas only cannot be decoded until a key chunk follows.
//
// Everything is timestamped against the probe start so the timelines can be read
// against the selector's rendition changes. Nothing secret, no tokens, no
// request ids, no packet contents: the URL path, wire request ids and payloads
// are never recorded.
const MAX_ENTRIES = 40_000;


// ---- redacted MoQ control-message decoding --------------------------------
//
// draft-16 multiplexes every request over one control stream as
// [typeId varint][size u16 BE][body] (see the pinned client's
// ietf/adapter.js #messageSize). Decoding just the type and the first body
// varint answers the question this task exists for: was the rendition change
// actually requested, with which request id, and did the peer answer. Only the
// type id, the declared body length and the first varint are recorded; no
// payload, no URL and no token is ever copied out.
const CONTROL_TYPES = {
  0x02: 'REQUEST_UPDATE(SUBSCRIBE_UPDATE)',
  0x03: 'SUBSCRIBE',
  0x04: 'SUBSCRIBE_OK',
  0x05: 'SUBSCRIBE_ERROR/REQUEST_ERROR',
  0x06: 'PUBLISH_NAMESPACE',
  0x07: 'REQUEST_OK/NAMESPACE_OK',
  0x09: 'PUBLISH_NAMESPACE_DONE',
  0x0a: 'UNSUBSCRIBE',
  0x0b: 'PUBLISH_DONE',
  0x0d: 'TRACK_STATUS_REQUEST',
  0x11: 'SUBSCRIBE_NAMESPACE',
  0x12: 'SUBSCRIBE_NAMESPACE_OK',
  0x13: 'SUBSCRIBE_NAMESPACE_ERROR',
  0x15: 'MAX_REQUEST_ID',
  0x16: 'FETCH',
  0x17: 'FETCH_CANCEL',
  0x18: 'FETCH_OK',
  0x19: 'FETCH_ERROR',
  0x1a: 'REQUESTS_BLOCKED',
  0x1d: 'PUBLISH',
  0x1e: 'PUBLISH_OK',
  0x1f: 'PUBLISH_ERROR',
  0x20: 'CLIENT_SETUP',
  0x21: 'SERVER_SETUP',
};

function readVarint(bytes, offset) {
  if (offset >= bytes.length) return null;
  const length = 1 << (bytes[offset] >> 6);
  if (offset + length > bytes.length) return null;
  let value = BigInt(bytes[offset] & 0x3f);
  for (let index = 1; index < length; index += 1) {
    value = (value << 8n) | BigInt(bytes[offset + index]);
  }
  return { value, next: offset + length };
}

/** Decode the framed messages in `buffer`; returns messages and leftover bytes. */
export function decodeControlMessages(buffer) {
  const messages = [];
  let offset = 0;
  for (;;) {
    const type = readVarint(buffer, offset);
    if (!type) break;
    if (buffer.length < type.next + 2) break;
    const size = (buffer[type.next] << 8) | buffer[type.next + 1];
    const bodyStart = type.next + 2;
    if (buffer.length < bodyStart + size) break;
    const body = buffer.subarray(bodyStart, bodyStart + size);
    const first = readVarint(body, 0);
    const entry = {
      type: Number(type.value),
      typeName: CONTROL_TYPES[Number(type.value)] ?? 'unknown',
      size,
      firstVarint: first ? Number(first.value) : null,
      bodyBytes: size,
    };
    if (entry.type === 0x04 && first) {
      const alias = readVarint(body, first.next);
      if (alias) entry.trackAlias = Number(alias.value);
    }
    // draft-16 SETUP bodies are a count-prefixed option list with delta-encoded
    // ids: even ids carry a varint value, odd ids carry a length-prefixed byte
    // value (pinned client: ietf/parameters.js SetupOptions.encode).
    if (entry.type === 0x20 || entry.type === 0x21) {
      const count = readVarint(body, 0);
      entry.setupOptions = [];
      let cursor = count ? count.next : 0;
      let previousId = 0;
      for (let index = 0; count && index < count.value; index += 1) {
        const delta = readVarint(body, cursor);
        if (!delta) break;
        const id = index === 0 ? Number(delta.value) : previousId + Number(delta.value);
        previousId = id;
        cursor = delta.next;
        if (id % 2 === 0) {
          const value = readVarint(body, cursor);
          if (!value) break;
          cursor = value.next;
          entry.setupOptions.push({ id, value: Number(value.value) });
          if (id === 2) entry.maxRequestId = Number(value.value);
        } else {
          const length = readVarint(body, cursor);
          if (!length) break;
          cursor = length.next + Number(length.value);
          entry.setupOptions.push({ id, length: Number(length.value) });
        }
      }
    }
    messages.push(entry);
    offset = bodyStart + size;
  }
  return { messages, rest: buffer.subarray(offset) };
}

export function installProtocolInstrumentation(startedAt) {
  const at = () => Math.round(performance.now() - startedAt);
  const log = {
    requests: [],
    objects: [],
    decodeInputs: [],
    errors: [],
    configures: [],
    // draft-16 multiplexes every request (SUBSCRIBE, UNSUBSCRIBE, ...) over one
    // long-lived control stream as virtual streams, so individual requests are
    // not visible as new QUIC streams. Their timing is: a write on the control
    // stream, and the peer's answer as a read.
    writes: [],
    reads: [],
    controlMessages: [],
  };
  // Per-stream, per-direction byte assembly for control-message decoding.
  const assemblers = new Map();
  const feedControl = (index, direction, chunk) => {
    if (!chunk || typeof chunk.byteLength !== 'number') return;
    const key = `${index}:${direction}`;
    const previous = assemblers.get(key) ?? new Uint8Array(0);
    const merged = new Uint8Array(previous.length + chunk.byteLength);
    merged.set(previous, 0);
    merged.set(new Uint8Array(chunk.buffer ?? chunk, chunk.byteOffset ?? 0, chunk.byteLength), previous.length);
    const { messages, rest } = decodeControlMessages(merged);
    assemblers.set(key, rest);
    for (const message of messages) {
      push(log.controlMessages, {
        tMs: at(),
        stream: index,
        direction,
        ...message,
      });
    }
  };
  const push = (list, entry) => {
    if (list.length < MAX_ENTRIES) list.push(entry);
  };

  const WebTransportClass = globalThis.WebTransport;
  if (WebTransportClass?.prototype?.createBidirectionalStream) {
    const originalCreate = WebTransportClass.prototype.createBidirectionalStream;
    WebTransportClass.prototype.createBidirectionalStream = function (...args) {
      const entry = {
        index: log.requests.length,
        openedMs: at(),
        writtenBytes: 0,
        writes: 0,
        readBytes: 0,
        reads: 0,
        ended: false,
      };
      log.requests.push(entry);
      // Chromium returns a promise here, so the stream is wrapped once it
      // resolves; the caller gets the same promise back.
      const instrument = (stream) => {
        try {
          const writer = stream.writable.getWriter();
          const wrappedWritable = new WritableStream({
            write(chunk) {
              entry.writtenBytes += chunk?.byteLength ?? 0;
              entry.writes += 1;
              push(log.writes, {
                tMs: at(),
                stream: entry.index,
                bytes: chunk?.byteLength ?? 0,
              });
              feedControl(entry.index, 'out', chunk);
              return writer.write(chunk);
            },
            close() {
              entry.ended = true;
              entry.endedMs = at();
              entry.endedHow = 'close';
              return writer.close();
            },
            abort(reason) {
              entry.ended = true;
              entry.endedMs = at();
              entry.endedHow = 'abort';
              entry.abortReason = String(reason?.message ?? reason ?? '');
              return writer.abort(reason);
            },
          });
          Object.defineProperty(stream, 'writable', {
            value: wrappedWritable,
            configurable: true,
          });
        } catch (error) {
          entry.wrapError = String(error?.message ?? error);
        }
        try {
          const reader = stream.readable.getReader();
          const wrappedReadable = new ReadableStream({
            async pull(controller) {
              const { value, done } = await reader.read();
              if (done) {
                entry.readEndedMs = at();
                controller.close();
                return;
              }
              entry.readBytes += value?.byteLength ?? 0;
              entry.reads += 1;
              push(log.reads, {
                tMs: at(),
                stream: entry.index,
                bytes: value?.byteLength ?? 0,
              });
              feedControl(entry.index, 'in', value);
              controller.enqueue(value);
            },
            cancel(reason) {
              return reader.cancel(reason);
            },
          });
          Object.defineProperty(stream, 'readable', {
            value: wrappedReadable,
            configurable: true,
          });
        } catch (error) {
          entry.readWrapError = String(error?.message ?? error);
        }
        entry.instrumentedMs = at();
        return stream;
      };
      const result = originalCreate.apply(this, args);
      if (result && typeof result.then === 'function') {
        result.then(
          (stream) => instrument(stream),
          (error) => {
            entry.failed = String(error?.message ?? error);
          },
        );
        return result;
      }
      return instrument(result);
    };
  }

  if (WebTransportClass?.prototype) {
    const descriptor = Object.getOwnPropertyDescriptor(
      WebTransportClass.prototype,
      'incomingUnidirectionalStreams',
    );
    if (descriptor?.get) {
      Object.defineProperty(WebTransportClass.prototype, 'incomingUnidirectionalStreams', {
        configurable: true,
        enumerable: descriptor.enumerable,
        get() {
          const source = descriptor.get.call(this);
          return source.pipeThrough(
            new TransformStream({
              transform(stream, controller) {
                const entry = { index: log.objects.length, openedMs: at(), bytes: 0, reads: 0 };
                // Retain at most three varints transiently, never payload bytes.
                // draft16 subgroup header: type, track alias, group id.
                let header = new Uint8Array(0);
                let headerDone = false;
                push(log.objects, entry);
                controller.enqueue(
                  stream.pipeThrough(
                    new TransformStream({
                      transform(chunk, innerController) {
                        entry.bytes += chunk?.byteLength ?? 0;
                        entry.reads += 1;
                        entry.lastMs = at();
                        if (!headerDone) {
                          const bytes = new Uint8Array(chunk.buffer ?? chunk, chunk.byteOffset ?? 0, chunk.byteLength);
                          const merged = new Uint8Array(Math.min(24, header.length + bytes.length));
                          merged.set(header.subarray(0, merged.length));
                          merged.set(bytes.subarray(0, merged.length - header.length), header.length);
                          header = merged;
                          const type = readVarint(header, 0);
                          const alias = type && readVarint(header, type.next);
                          const group = alias && readVarint(header, alias.next);
                          if (type && alias && group) {
                            const kind = Number(type.value);
                            if ((kind >= 0x10 && kind <= 0x1f) || (kind >= 0x30 && kind <= 0x3f)) {
                              entry.streamType = kind;
                              entry.trackAlias = Number(alias.value);
                              entry.groupId = Number(group.value);
                            }
                            headerDone = true;
                            header = new Uint8Array(0);
                          }
                        }
                        innerController.enqueue(chunk);
                      },
                      flush() {
                        entry.closedMs = at();
                      },
                    }),
                  ),
                );
              },
            }),
          );
        },
      });
    }
  }

  const DecoderClass = globalThis.VideoDecoder;
  if (DecoderClass?.prototype?.decode) {
    const originalDecode = DecoderClass.prototype.decode;
    DecoderClass.prototype.decode = function (chunk) {
      push(log.decodeInputs, {
        tMs: at(),
        type: chunk?.type,
        bytes: chunk?.byteLength,
        timestamp: Number(chunk?.timestamp ?? 0),
        queue: this.decodeQueueSize,
      });
      return originalDecode.call(this, chunk);
    };
    const originalConfigure = DecoderClass.prototype.configure;
    DecoderClass.prototype.configure = function (config) {
      push(log.configures, {
        tMs: at(),
        codec: config?.codec,
        codedWidth: config?.codedWidth,
        codedHeight: config?.codedHeight,
        descriptionBytes: config?.description?.byteLength ?? 0,
      });
      return originalConfigure.call(this, config);
    };
    const onError = Object.getOwnPropertyDescriptor(DecoderClass.prototype, 'onerror');
    if (onError?.set && onError?.get) {
      Object.defineProperty(DecoderClass.prototype, 'onerror', {
        configurable: true,
        get: onError.get,
        set(handler) {
          onError.set.call(this, (event) => {
            push(log.errors, {
              tMs: at(),
              message: String(event?.message ?? event?.error?.message ?? event?.type ?? 'error'),
            });
            return handler?.call(this, event);
          });
        },
      });
    }
  }

  return {
    log,
    snapshot() {
      const keyChunks = log.decodeInputs.filter((entry) => entry.type === 'key').length;
      return {
        requestStreams: log.requests,
        controlWrites: log.writes,
        controlReads: log.reads,
        controlMessages: log.controlMessages,
        incomingObjectStreams: log.objects,
        decodeInputs: log.decodeInputs,
        decodeKeyChunks: keyChunks,
        decodeDeltaChunks: log.decodeInputs.length - keyChunks,
        configureCalls: log.configures,
        decodeErrors: log.errors,
        totals: {
          requests: log.requests.length,
          requestsWithWrites: log.requests.filter((entry) => entry.writes > 0).length,
          objectStreams: log.objects.length,
          objectBytes: log.objects.reduce((sum, entry) => sum + entry.bytes, 0),
          decodeInputs: log.decodeInputs.length,
          decodeKeyChunks: keyChunks,
          decodeErrors: log.errors.length,
          controlWrites: log.writes.length,
          controlWriteBytes: log.writes.reduce((sum, entry) => sum + entry.bytes, 0),
          controlReads: log.reads.length,
          controlMessages: log.controlMessages.length,
          setupOptions: log.controlMessages
            .filter((entry) => entry.setupOptions)
            .map((entry) => ({
              tMs: entry.tMs,
              direction: entry.direction,
              type: entry.type,
              maxRequestId: entry.maxRequestId ?? null,
              options: entry.setupOptions,
            })),
          messageTypeCounts: log.controlMessages.reduce((counts, entry) => {
            const key = `${entry.direction}:${entry.typeName}`;
            counts[key] = (counts[key] ?? 0) + 1;
            return counts;
          }, {}),
        },
      };
    },
  };
}
