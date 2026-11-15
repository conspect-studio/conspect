// QueryClient-scoped bridge from the ONE shell-owned WebSocket to command callers.
// The server, never this client, supplies the operation id in an actual HTTP 202.
// No POST is replayed, and no query, localStorage journal or polling is implied.
import type { QueryClient } from '@tanstack/react-query';
import { OperationApiError } from '../../api/operations';
import type { AcceptedBody } from '../../api/operations';
import { getStoredToken } from '../../api/token';
import { isCorrelatedKind } from '../../realtime/correlatedCommands';
import { parseMediaPlayerEvent } from '../../realtime/envelope';
import { switcherOperationEvidence } from '../../realtime/operationEvidence';
import type { Envelope } from '../../realtime/envelope';
import type { MediaPlayerState } from '../../realtime/generated-types';
import type { OutputRunState } from '../../realtime/pending/events.pending';
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
      readonly terminal: { readonly kind: 'output-state'; readonly state: OutputRunState } }
  | { readonly topic: 'switcher'; readonly resource: string; readonly event: 'switcher.operation';
      readonly terminal: { readonly kind: 'switcher-operation'; readonly me: string } };

type ParsedEvent =
  | { readonly corr: string; readonly topic: 'switcher'; readonly resource: string;
      readonly event: 'media.player_state'; readonly state: PlayerState;
      readonly at: number; readonly exitArmed?: boolean }
  | { readonly corr: string; readonly topic: 'outputs'; readonly resource: string;
      readonly event: 'output.status'; readonly state: OutputRunState; readonly at: number }
  | { readonly corr: string; readonly topic: 'switcher'; readonly resource: string;
      readonly event: 'switcher.operation'; readonly me: string;
      readonly result: 'applied' | 'failed'; readonly at: number };
export interface SubmissionResult { readonly kind: 'tracked' | 'accepted-unobserved'; readonly operationId: string }
/** Server-issued identity on an unresolved 202; absent fields are never fabricated. */
export interface UnknownSeatDetail {
  readonly operationId?: string;
  readonly kind?: string;
  readonly reason: 'uncorrelated-kind' | 'unsupported-expectation' | 'invalid-deadline';
}
export type TrackerSnapshot = OperationOutcomeState & {
  readonly unknownSeatDetails: ReadonlyMap<number, UnknownSeatDetail>;
};

function parseEvent(envelope: Envelope): ParsedEvent | undefined {
  const switcher = switcherOperationEvidence(envelope);
  if (switcher !== undefined) return { corr: switcher.corr, topic: switcher.topic,
    resource: switcher.resource, event: switcher.event, me: switcher.me,
    result: switcher.verdict, at: Date.now() };
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
// Correlation alone is insufficient: a caller cannot turn a play into a pause,
// or claim a switcher cut from a player/output state. The switcher 202 uses
// the exact verb string and its validated output-tick outcome is independent
// of the CorrKey::for_command registry in realtime.rs.
function matchesCommandKind(kind: string, expectation: OperationExpectation): boolean {
  if (!isCorrelatedKind(kind) || typeof expectation.resource !== 'string' || !expectation.resource.trim()) return false;
  if (expectation.event === 'switcher.operation')
    return expectation.terminal.me === expectation.resource &&
      (kind === 'select_program' || kind === 'select_preview' || kind === 'cut' || kind === 'auto' || kind === 'set_transition');
  if (expectation.event === 'output.status') {
    return (kind === 'start' && expectation.terminal.state === 'running') ||
      (kind === 'stop' && expectation.terminal.state === 'idle');
  }
  const { state, exitArmed } = expectation.terminal;
  if (kind === 'arm_media_exit' || kind === 'take_media_exit' || kind === 'cancel_media_exit')
    return state === 'vamping' && exitArmed === (kind !== 'cancel_media_exit');
  if (exitArmed !== undefined) return false;
  return (kind === 'media_play' && state === 'playing') ||
    (kind === 'media_pause' && state === 'paused') ||
    (kind === 'media_stop' && state === 'stopped') ||
    (kind === 'media_cue' && state === 'cued');
}
function matching(event: ParsedEvent, expectation: OperationExpectation, corr: string): boolean {
  return event.corr === corr && event.topic === expectation.topic &&
    event.resource === expectation.resource && event.event === expectation.event;
}
function verdict(event: ParsedEvent, expectation: OperationExpectation): 'applied' | 'failed' | undefined {
  if (event.event === 'switcher.operation' && expectation.event === 'switcher.operation')
    return event.me === expectation.terminal.me ? event.result : undefined;
  if (event.event === 'media.player_state' && expectation.event === 'media.player_state') {
    if (event.state === 'vamping' && expectation.terminal.exitArmed !== undefined &&
        event.exitArmed !== expectation.terminal.exitArmed) return undefined;
    return event.state === expectation.terminal.state ? 'applied' : undefined;
  }
  if (event.event === 'output.status' && expectation.event === 'output.status')
    return event.state === expectation.terminal.state ? 'applied' : undefined;
  return undefined;
}

const EMPTY_SNAPSHOT: TrackerSnapshot = {
  ...createOperationOutcomeState(), unknownSeatDetails: new Map(),
};

export class OperationTracker {
  private state: OperationOutcomeState = createOperationOutcomeState();
  private currentSnapshot: TrackerSnapshot = { ...this.state, unknownSeatDetails: new Map() };
  private readonly expectations = new Map<string, OperationExpectation>();
  private readonly unknownSeatDetails = new Map<number, UnknownSeatDetail>();
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
    this.unknownSeatDetails.clear();
    this.early = [];
    this.publish();
  }
  /** Pure, referentially stable render read. Never expose another credential's evidence. */
  snapshot(): TrackerSnapshot {
    return this.identity === getStoredToken() ? this.currentSnapshot : EMPTY_SNAPSHOT;
  }
  private publish(): void {
    this.currentSnapshot = { ...this.state, unknownSeatDetails: new Map(this.unknownSeatDetails) };
    this.client.setQueryData(OPERATION_TRACKER_QUERY_KEY, this.currentSnapshot);
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
    // API generics describe the expected body; a malformed 202 still crosses
    // this boundary at runtime and must never become a tracked operation.
    const body: unknown = accepted;
    const operationId = body !== null && typeof body === 'object' && 'operation_id' in body &&
      typeof body.operation_id === 'string' ? body.operation_id : '';
    const kind = body !== null && typeof body === 'object' && 'kind' in body &&
      typeof body.kind === 'string' ? body.kind : undefined;
    if (generation !== this.generation || !this.state.provisional.has(token))
      return { kind: 'accepted-unobserved', operationId };
    const unknown = (reason: UnknownSeatDetail['reason']): SubmissionResult => {
      this.state = markOperationResponseUnknown(this.state, token, 'transport-unknown');
      this.unknownSeatDetails.set(token, {
        ...(operationId.trim() ? { operationId } : {}),
        ...(kind?.trim() ? { kind } : {}), reason,
      });
      this.publish();
      return { kind: 'accepted-unobserved', operationId };
    };
    if (!operationId.trim() || !kind || !expectation || !matchesCommandKind(kind, expectation)) {
      // The actual 202 kind, never a caller's claimed expectation, determines
      // whether a frame can prove application. Replays hide their original kind.
      // Preserve the capacity seat: unsupported is not rejection.
      return unknown(!kind || !isCorrelatedKind(kind) ? 'uncorrelated-kind' : 'unsupported-expectation');
    }
    const deadlineMs = options.deadlineMs ?? DEFAULT_OPERATION_DEADLINE_MS;
    if (!Number.isFinite(deadlineMs) || deadlineMs < 0) return unknown('invalid-deadline');
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
      return { kind: 'tracked', operationId };
    } catch (error) {
      // The server accepted a command: a client-side identity/schema failure
      // cannot release its reserved seat or turn the command into a rejection.
      const result = unknown('unsupported-expectation');
      if (!(error instanceof OperationOutcomeError)) throw error;
      return result;
    }
  }
}
const trackers = new WeakMap<QueryClient, OperationTracker>();
export function getOperationTracker(client: QueryClient): OperationTracker {
  let tracker = trackers.get(client);
  if (tracker === undefined) { tracker = new OperationTracker(client); trackers.set(client, tracker); }
  return tracker;
}
