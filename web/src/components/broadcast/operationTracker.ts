// QueryClient-scoped bridge from the ONE shell-owned WebSocket to command callers.
// The server, never this client, supplies the operation id in an actual HTTP 202.
// No POST is replayed, and no query, localStorage journal or polling is implied.
import type { QueryClient } from '@tanstack/react-query';
import { OperationApiError } from '../../api/operations';
import type { AcceptedBody } from '../../api/operations';
import { getStoredToken } from '../../api/token';
import { parseMediaPlayerEvent } from '../../realtime/envelope';
import type { Envelope } from '../../realtime/envelope';
import type { MediaPlayerState, OutputRunState } from '../../realtime/generated-types';
import {
  admitOperationOutcome, applyTerminalEvidence, createOperationOutcomeState, reserveOperationOutcome,
  expireOperationDeadlines, markOperationResponseUnknown, OperationOutcomeError,
  releaseOperationReservation,
} from './operationOutcomes';
import type { OperationOutcomeState, OperationRecord, TerminalEvidence } from './operationOutcomes';

export const OPERATION_TRACKER_QUERY_KEY = ['broadcast', 'operationTracker'] as const;
const EARLY_EVENTS_MAX = 64;
export const DEFAULT_OPERATION_DEADLINE_MS = 30_000;

type PlayerState = MediaPlayerState['kind'];
/** Callers must supply the *command's* expected terminal state, not any state change. */
export type OperationExpectation =
  | { readonly topic: 'switcher'; readonly resource: string; readonly event: 'media.player_state';
      readonly terminal: { readonly kind: 'player-state'; readonly state: PlayerState;
        readonly exitArmed?: boolean } }
  | { readonly topic: 'outputs'; readonly resource: string; readonly event: 'output.status';
      readonly terminal: { readonly kind: 'output-state'; readonly state: OutputRunState } };

interface ParsedEvent { readonly corr: string; readonly topic: string; readonly resource: string;
  readonly event: 'media.player_state' | 'output.status'; readonly state: PlayerState | OutputRunState;
  readonly at: number; readonly exitArmed?: boolean }
export interface SubmissionResult { readonly kind: 'tracked' | 'accepted-unobserved'; readonly operationId: string }

function parseEvent(envelope: Envelope): ParsedEvent | undefined {
  if (!envelope.corr || !envelope.id) return undefined;
  if (envelope.t === 'media.player_state' && envelope.topic === 'switcher') {
    const data = parseMediaPlayerEvent(envelope.data);
    if (data?.player !== envelope.id) return undefined;
    return { corr: envelope.corr, topic: envelope.topic, resource: envelope.id,
      event: envelope.t, state: data.state.kind, at: Date.now(),
      ...(data.state.kind === 'vamping' ? { exitArmed: data.state.exit_armed } : {}) };
  }
  if (envelope.t === 'output.status' && envelope.topic === 'outputs') {
    if (typeof envelope.data !== 'object' || envelope.data === null || !('state' in envelope.data)) return undefined;
    const state = envelope.data.state;
    if (state !== 'idle' && state !== 'starting' && state !== 'running' && state !== 'migrating' && state !== 'error') return undefined;
    return { corr: envelope.corr, topic: envelope.topic, resource: envelope.id,
      event: envelope.t, state, at: Date.now() };
  }
  return undefined;
}
function matching(event: ParsedEvent, expectation: OperationExpectation, corr: string): boolean {
  return event.corr === corr && event.topic === expectation.topic &&
    event.resource === expectation.resource && event.event === expectation.event;
}
function verdict(event: ParsedEvent, expectation: OperationExpectation): 'applied' | 'failed' | undefined {
  if (expectation.terminal.kind === 'player-state' && event.state === 'vamping' &&
      expectation.terminal.exitArmed !== undefined && event.exitArmed !== expectation.terminal.exitArmed) return undefined;
  return event.state === expectation.terminal.state ? 'applied' : undefined;
}

export class OperationTracker {
  private state: OperationOutcomeState = createOperationOutcomeState();
  private readonly expectations = new Map<string, OperationExpectation>();
  private early: ParsedEvent[] = [];
  private identity: string | undefined;
  private deadlineTimer: ReturnType<typeof setTimeout> | undefined;
  private generation = 0;

  constructor(private readonly client: QueryClient) {
    this.identity = getStoredToken();
  }
  /** Identity is scoped to the operator credential, not persisted on disk. */
  resetIdentity(identity: string | undefined): void {
    if (this.identity === identity) return;
    this.identity = identity;
    this.clear();
  }
  private checkIdentity(): void {
    this.resetIdentity(getStoredToken());
  }
  private clear(): void {
    if (this.deadlineTimer !== undefined) clearTimeout(this.deadlineTimer);
    this.deadlineTimer = undefined;
    this.generation += 1;
    this.state = createOperationOutcomeState();
    this.expectations.clear();
    this.early = [];
    this.publish();
  }
  snapshot(): OperationOutcomeState {
    this.checkIdentity();
    return this.state;
  }
  private publish(): void {
    this.client.setQueryData(OPERATION_TRACKER_QUERY_KEY, this.state);
    this.scheduleDeadline();
  }
  private scheduleDeadline(): void {
    if (this.deadlineTimer !== undefined) clearTimeout(this.deadlineTimer);
    this.deadlineTimer = undefined;
    const deadlines = [...this.state.records.values()]
      .flatMap((item) => item.phase === 'accepted' && item.deadlineAt !== undefined ? [item.deadlineAt] : []);
    if (deadlines.length === 0) return;
    const next = Math.min(...deadlines);
    this.deadlineTimer = setTimeout(() => {
      this.deadlineTimer = undefined;
      const updated = expireOperationDeadlines(this.state, Date.now());
      if (updated !== this.state) { this.state = updated; this.publish(); }
      else this.scheduleDeadline();
    }, Math.max(0, next - Date.now()));
  }
  /** Gap/disconnect invalidates pending observation, but late exact events may reconcile. */
  onGap(): void {
    this.checkIdentity();
    // A frame observed before admission may still be the exact terminal
    // evidence; do not discard it merely because another sequence was lost.
    const now = Date.now();
    this.state = expireOperationDeadlines({ ...this.state,
      records: new Map([...this.state.records].map(([id, record]): [string, OperationRecord] =>
        [id, record.phase === 'accepted' ? { ...record, deadlineAt: now } : record])) }, now);
    this.publish();
  }
  onEnvelope(envelope: Envelope): void {
    this.checkIdentity();
    const event = parseEvent(envelope);
    if (event === undefined) return;
    const expectation = this.expectations.get(event.corr);
    if (expectation !== undefined) {
      if (!matching(event, expectation, event.corr)) return;
      const outcome = verdict(event, expectation);
      if (outcome !== undefined) this.acceptEvidence(event, outcome);
      return;
    }
    // Known admitted operations must not re-enter the early-event cache after
    // history eviction or a mismatched resource/topic.
    if ([...this.state.records.values()].some((record) => record.corr === event.corr)) return;
    // Retain typed, validated event payload until the actual 202 arrives. A
    // wrong topic/resource/intermediate state cannot occupy a terminal verdict.
    this.early.push(event);
    if (this.early.length > EARLY_EVENTS_MAX) this.early.shift();
  }
  private acceptEvidence(event: ParsedEvent, result: 'applied' | 'failed'): void {
    const evidence: TerminalEvidence = { corr: event.corr, topic: event.topic, resource: event.resource,
      event: event.event, verdict: result, at: event.at };
    const next = applyTerminalEvidence(this.state, evidence);
    if (next !== this.state) { this.state = next; this.publish(); }
  }
  /**
   * Reserve synchronously BEFORE calling the existing caller-supplied async POST.
   * A 4xx/5xx OperationApiError is definitive; any other throw (including a
   * malformed response or abort after send) is ambiguous and retains its seat.
   * Unsupported command correlation is accepted-unobserved, never fabricated.
   */
  async submit(
    send: () => Promise<AcceptedBody>, expectation: OperationExpectation | undefined,
    options: { readonly deadlineMs?: number } = {},
  ): Promise<SubmissionResult> {
    this.checkIdentity();
    const generation = this.generation;
    const { state, token } = reserveOperationOutcome(this.state);
    this.state = state;
    this.publish();
    let accepted: AcceptedBody;
    try { accepted = await send(); }
    catch (error) {
      this.checkIdentity();
      if (generation === this.generation && this.state.provisional.has(token)) {
        this.state = error instanceof OperationApiError && error.status !== undefined && error.status >= 400 && error.status < 600
          ? releaseOperationReservation(this.state, token, 'definitive-http-rejection')
          : markOperationResponseUnknown(this.state, token, 'transport-unknown');
        this.publish();
      }
      throw error;
    }
    // A change of principal while the POST was in flight must not publish its
    // result under another operator's QueryClient identity.
    this.checkIdentity();
    if (generation !== this.generation || !this.state.provisional.has(token))
      return { kind: 'accepted-unobserved', operationId: accepted.operation_id };
    if (!expectation?.resource || !accepted.operation_id) {
      // 202 is authoritative admission even without observable correlation.
      // Preserve the anonymous capacity seat: unsupported is not rejection.
      this.state = markOperationResponseUnknown(this.state, token, 'transport-unknown');
      this.publish();
      return { kind: 'accepted-unobserved', operationId: accepted.operation_id };
    }
    const deadlineMs = options.deadlineMs ?? DEFAULT_OPERATION_DEADLINE_MS;
    if (!Number.isFinite(deadlineMs) || deadlineMs < 0) {
      this.state = markOperationResponseUnknown(this.state, token, 'transport-unknown');
      this.publish();
      return { kind: 'accepted-unobserved', operationId: accepted.operation_id };
    }
    try {
      this.state = admitOperationOutcome(this.state, token, {
        operationId: accepted.operation_id, corr: accepted.operation_id, topic: expectation.topic,
        resource: expectation.resource, event: expectation.event, kind: accepted.kind,
        acceptedAt: Date.now(), deadlineAt: Date.now() + deadlineMs,
      });
      this.expectations.set(accepted.operation_id, expectation);
      this.publish();
      // An earlier loading frame must not mask a later playing frame. Keep
      // searching all bounded candidates for exact command terminal evidence.
      const index = this.early.findIndex((item) => matching(item, expectation, accepted.operation_id) && verdict(item, expectation) !== undefined);
      if (index >= 0) {
        const [item] = this.early.splice(index, 1);
        if (item !== undefined) {
          const outcome = verdict(item, expectation);
          if (outcome !== undefined) this.acceptEvidence(item, outcome);
        }
      }
      return { kind: 'tracked', operationId: accepted.operation_id };
    } catch (error) {
      // The server accepted a command: a client-side identity/schema failure
      // cannot release its reserved seat or turn the command into a rejection.
      this.state = markOperationResponseUnknown(this.state, token, 'transport-unknown');
      this.publish();
      if (!(error instanceof OperationOutcomeError)) throw error;
      return { kind: 'accepted-unobserved', operationId: accepted.operation_id };
    }
  }
}
const trackers = new WeakMap<QueryClient, OperationTracker>();
export function getOperationTracker(client: QueryClient): OperationTracker {
  let tracker = trackers.get(client);
  if (tracker === undefined) { tracker = new OperationTracker(client); trackers.set(client, tracker); }
  return tracker;
}
