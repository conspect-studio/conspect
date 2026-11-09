// TranscriptRangeEditor — the word-range selection surface for one asset's
// transcript. The prototype's verified touch flow carries forward:
//   * tap one word to anchor; tap another to select the RANGE BETWEEN;
//     a third tap re-anchors (Shift+click/Shift+arrow extends) — the
//     prototype's exact semantics, controlled through
//     `TranscriptSelection { startWordId, endWordId | null }`;
//   * 48px IN/OUT endpoint steppers step the range ends one WORD at a
//     time — the precision alternative to tapping a small word next to
//     its neighbour (round-4 touch blocker);
//   * keyboard: Arrow/Home/End move focus (plain = re-anchor on the
//     focused word, Shift = extend the range), Enter/Space activate.
//   * the selection readout shows EXACT grid boundaries — the first
//     selected word's start and the last word's end as NDF timecode /
//     elapsed clock plus the integer index and exact rational pair —
//     never float seconds (audit §5); an unaligned word renders an honest
//     not-aligned fallback, never NaN.
//   * `readOnly` renders the words static with the caller's capability
//     explanation (e.g. "no STT backend exists today") — the transcript is
//     never silently editable, and never silently presented as analysis
//     that exists.
// FULLY CONTROLLED: selection (and focus) changes only emit intents; the
// DOM reflects new state only when the caller re-renders.
import {
  useCallback,
  useMemo,
  useRef,
  useState,
  type KeyboardEvent,
  type JSX,
} from 'react';
import { useLingui } from '@lingui/react/macro';

import { cn } from '../../../lib/utils';
import { TouchAction } from '../TouchAction';
import {
  describeBoundaryUnit,
  formatBoundaryLabel,
} from './boundaries';
import type { RationalBoundary } from '../duration';
import { isSameGrid } from './boundaries';
import type {
  TranscriptSelection,
  TranscriptWord,
} from './types';

/** Props for {@link TranscriptRangeEditor}. */
export interface TranscriptRangeEditorProps {
  /** The asset's ordered words (caller-owned; corrections are overlays there). */
  readonly words: readonly TranscriptWord[];
  /** The controlled selection; `null` = none. */
  readonly selection: TranscriptSelection | null;
  /**
   * Intent: the selection changed (anchor/extend/re-anchor/step). The
   * caller owns the value; nothing applies inside the component.
   */
  readonly onSelectionChange: (selection: TranscriptSelection | null) => void;
  /**
   * Render readonly: words become static text, steppers and interactions
   * disappear, and `readOnlyReason` explains why (capability honesty).
   */
  readonly readOnly?: boolean | undefined;
  /** Localized capability explanation shown in readonly mode. */
  readonly readOnlyReason?: string | undefined;
  /** Overrides the empty-state line (already localized by the caller). */
  readonly emptyLabel?: string | undefined;
  /** Transcript revision selected by the caller. */
  readonly transcriptRevision?: string | null | undefined;
  /** Revision whose alignment has been validated against this asset. */
  readonly alignedRevision?: string | null | undefined;
}

/** The normalized inclusive range indexes of a selection, or null. */
function rangeOf(
  words: readonly TranscriptWord[],
  selection: TranscriptSelection | null,
): { min: number; max: number } | null {
  if (selection === null) {
    return null;
  }
  const startIndex = words.findIndex((word) => word.id === selection.startWordId);
  const endIndex =
    selection.endWordId === null
      ? startIndex
      : words.findIndex((word) => word.id === selection.endWordId);
  if (startIndex < 0 || endIndex < 0) {
    return null;
  }
  return { min: Math.min(startIndex, endIndex), max: Math.max(startIndex, endIndex) };
}

/** Validate every selected interval, not only the two visible endpoints. */
function clipRangeReady(words: readonly TranscriptWord[], range: { min: number; max: number }, transcriptRevision?: string | null, alignedRevision?: string | null): boolean {
  if (!transcriptRevision || transcriptRevision !== alignedRevision) return false;
  const selected = words.slice(range.min, range.max + 1);
  let previous: RationalBoundary | null = null;
  const first = selected[0]?.startBoundary;
  if (!first || formatBoundaryLabel(first) === null) return false;
  for (const word of selected) {
    const start = word.startBoundary;
    const end = word.endBoundary;
    if (!start || !end || formatBoundaryLabel(start) === null || formatBoundaryLabel(end) === null || !isSameGrid(first, start) || !isSameGrid(first, end) || end.index <= start.index || (previous !== null && start.index < previous.index)) return false;
    previous = end;
  }
  return previous !== null && previous.index > first.index;
}

/**
 * The word-range selection editor: controlled words + selection, exact
 * boundary readouts, 48px endpoint steppers, keyboard support, and an
 * honest readonly capability explanation.
 */
export function TranscriptRangeEditor({
  words,
  selection,
  onSelectionChange,
  readOnly = false,
  readOnlyReason,
  emptyLabel,
  transcriptRevision,
  alignedRevision,
}: TranscriptRangeEditorProps): JSX.Element {
  const { t } = useLingui();
  const wordRefs = useRef(new Map<string, HTMLButtonElement | HTMLSpanElement>());
  const [focusedWordId, setFocusedWordId] = useState<string | null>(null);

  const range = useMemo(() => rangeOf(words, selection), [words, selection]);
  const clipReady = range !== null && clipRangeReady(words, range, transcriptRevision, alignedRevision);

  // Consecutive groups sharing a segmentId; ungrouped words (null) each
  // form their own group without a header.
  const segmentGroups = useMemo(() => {
    const groups: {
      key: string;
      segmentId: string | null;
      speakerLabel: string | null;
      words: { word: TranscriptWord; index: number }[];
    }[] = [];
    words.forEach((word, index) => {
      const lastGroup = groups.at(-1) ?? null;
      const sameGroup =
        lastGroup !== null &&
        lastGroup.segmentId === word.segmentId &&
        (word.segmentId !== null || lastGroup.speakerLabel === word.speakerLabel);
      if (sameGroup) {
        lastGroup.words.push({ word, index });
        return;
      }
      groups.push({
        key: word.segmentId ?? `ungrouped-${word.id}`,
        segmentId: word.segmentId,
        speakerLabel: word.speakerLabel,
        words: [{ word, index }],
      });
    });
    return groups;
  }, [words]);

  const focusWord = useCallback((wordId: string): void => {
    setFocusedWordId(wordId);
    wordRefs.current.get(wordId)?.focus();
  }, []);

  // The roving tab stop: the focused word, else the anchor, else word 0.
  const tabStopId =
    [focusedWordId, selection?.startWordId, words.at(0)?.id].find((id) => id !== null && id !== undefined && words.some((word) => word.id === id)) ?? null;

  const activate = useCallback(
    (wordId: string, extend: boolean): void => {
      if (readOnly) {
        return;
      }
      if (selection === null) {
        onSelectionChange({ startWordId: wordId, endWordId: null });
        return;
      }
      const anchor = selection.startWordId;
      const r = rangeOf(words, selection);
      const index = words.findIndex((word) => word.id === wordId);
      const inside = r !== null && index >= r.min && index <= r.max;
      if (extend) {
        // Shift always extends from the anchor.
        onSelectionChange({ startWordId: anchor, endWordId: wordId });
        return;
      }
      if (inside && selection.endWordId !== null) {
        // A third tap inside a range re-anchors (prototype semantics).
        onSelectionChange({ startWordId: wordId, endWordId: null });
        return;
      }
      if (selection.endWordId === null) {
        // Second tap: anchor → range between.
        onSelectionChange({ startWordId: anchor, endWordId: wordId });
        return;
      }
      // Outside a range: re-anchor (start a new selection).
      onSelectionChange({ startWordId: wordId, endWordId: null });
    },
    [onSelectionChange, readOnly, selection, words],
  );

  const handleKeyDown = useCallback(
    (event: KeyboardEvent<HTMLElement>): void => {
      if (readOnly || words.length === 0 || tabStopId === null) {
        return;
      }
      const focusedIndex = words.findIndex((word) => word.id === tabStopId);
      if (focusedIndex < 0) {
        return;
      }
      let nextIndex: number;
      switch (event.key) {
        case 'ArrowRight':
          nextIndex = focusedIndex + 1;
          break;
        case 'ArrowLeft':
          nextIndex = focusedIndex - 1;
          break;
        case 'Home':
          nextIndex = 0;
          break;
        case 'End':
          nextIndex = words.length - 1;
          break;
        case 'Enter':
        case ' ': {
          const focusedWord = words.at(focusedIndex);
          if (focusedWord === undefined) {
            return;
          }
          event.preventDefault();
          activate(focusedWord.id, event.shiftKey);
          return;
        }
        default:
          return;
      }
      if (nextIndex < 0 || nextIndex >= words.length || nextIndex === focusedIndex) {
        event.preventDefault();
        return;
      }
      event.preventDefault();
      const nextWord = words.at(nextIndex);
      if (nextWord === undefined) {
        return;
      }
      focusWord(nextWord.id);
      if (event.shiftKey) {
        activate(nextWord.id, true);
      } else {
        // Plain arrows follow focus: re-anchor on the focused word.
        onSelectionChange({ startWordId: nextWord.id, endWordId: null });
      }
    },
    [
      activate,
      focusWord,
      onSelectionChange,
      readOnly,
      words,
      tabStopId,
    ],
  );

  // Steppers step one word at a time, never past each other.
  const stepRange = (bound: 'in' | 'out', direction: -1 | 1): void => {
    if (range === null || !clipReady) {
      return;
    }
    let { min, max } = range;
    if (bound === 'in') {
      min += direction;
    } else {
      max += direction;
    }
    if (min < 0 || max >= words.length || min > max) {
      return;
    }
    const inWord = words.at(min);
    const outWord = words.at(max);
    if (inWord === undefined || outWord === undefined) {
      return;
    }
    if (!clipRangeReady(words, { min, max }, transcriptRevision, alignedRevision)) return;
    onSelectionChange({
      startWordId: inWord.id,
      endWordId: outWord.id,
    });
  };

  const inBackwardDisabled = range === null || !clipReady || range.min === 0;
  const inForwardDisabled = range === null || !clipReady || range.min >= range.max;
  const outBackwardDisabled = range === null || !clipReady || range.max <= range.min;
  const outForwardDisabled = range === null || !clipReady || range.max === words.length - 1;

  if (words.length === 0) {
    return (
      <section
        data-testid="transcript-range-editor"
        data-state="empty"
        aria-label={t`Transcript`}
        className="rounded-lg border border-dashed p-4 text-sm"
      >
        <p className="min-w-0 break-words">
          {emptyLabel ?? t`No transcript available for this asset.`}
        </p>
        {readOnlyReason !== undefined ? (
          <p className="min-w-0 break-words text-xs text-muted-foreground">
            {readOnlyReason}
          </p>
        ) : null}
      </section>
    );
  }

  return (
    <section
      data-testid="transcript-range-editor"
      data-state={readOnly ? 'readonly' : 'interactive'}
      aria-label={t`Transcript`}
      className="flex min-w-0 flex-col gap-3"
    >
      {readOnly ? (
        <div
          data-testid="transcript-readonly-explanation"
          className="rounded-lg border border-dashed p-3 text-sm"
        >
          <p className="min-w-0 break-words">
            {t`Transcript is read-only — selection is unavailable.`}
          </p>
          {readOnlyReason !== undefined ? (
            <p className="min-w-0 break-words text-xs text-muted-foreground">
              {readOnlyReason}
            </p>
          ) : null}
        </div>
      ) : (
        <p className="min-w-0 break-words text-xs text-muted-foreground">
          {t`Tap one word to select it; tap another to select the range between — or step the IN/OUT endpoints.`}
        </p>
      )}

      {range !== null && !readOnly ? (
        <div
          data-testid="transcript-selection-readout"
          className="flex min-w-0 flex-col gap-2 rounded-lg border p-3"
        >
          <SelectionReadout words={words} range={range} clipReady={clipReady} />
          <div className="flex min-w-0 flex-wrap items-start gap-2">
            <EndpointStepper
              label={t`IN`}
              stepBackwardDisabled={inBackwardDisabled}
              stepForwardDisabled={inForwardDisabled}
              onStepBackward={(): void => {
                stepRange('in', -1);
              }}
              onStepForward={(): void => {
                stepRange('in', 1);
              }}
            />
            <EndpointStepper
              label={t`OUT`}
              stepBackwardDisabled={outBackwardDisabled}
              stepForwardDisabled={outForwardDisabled}
              onStepBackward={(): void => {
                stepRange('out', -1);
              }}
              onStepForward={(): void => {
                stepRange('out', 1);
              }}
            />
          </div>
        </div>
      ) : null}

      <div
        role={readOnly ? undefined : 'listbox'}
        aria-multiselectable={readOnly ? undefined : 'true'}
        aria-label={readOnly ? undefined : t`Transcript words`}
        onKeyDown={handleKeyDown}
        className="flex min-w-0 flex-col gap-3"
      >
        {segmentGroups.map((group) => (
          <div key={group.key} className="flex min-w-0 flex-col gap-1">
            {group.segmentId !== null ? (
              <p
                data-testid="transcript-segment-header"
                className="flex min-w-0 flex-wrap items-baseline gap-x-3 gap-y-1 text-xs text-muted-foreground"
              >
                <span className="break-words font-medium">
                  {group.speakerLabel ?? t`Unlabeled speaker`}
                </span>
                <span className="font-mono">
                  {formatBoundaryLabel(group.words.at(0)?.word.startBoundary) ??
                    t`timecodes not aligned`}
                </span>
              </p>
            ) : null}
            <div className="flex min-w-0 flex-wrap gap-1">
              {group.words.map(({ word, index }) => {
                const selected =
                  range !== null && index >= range.min && index <= range.max;
                const refCallback = (
                  node: HTMLButtonElement | HTMLSpanElement | null,
                ): void => {
                  if (node === null) {
                    wordRefs.current.delete(word.id);
                  } else {
                    wordRefs.current.set(word.id, node);
                  }
                };
                if (readOnly) {
                  return (
                    <span
                      key={word.id}
                      ref={refCallback}
                      data-word-id={word.id}
                      data-word-selected={selected ? 'true' : 'false'}
                      className="inline-flex min-h-12 min-w-12 items-center rounded-md px-2 py-1 text-sm"
                    >
                      {word.text}
                    </span>
                  );
                }
                return (
                  <button
                    key={word.id}
                    ref={refCallback}
                    data-word-id={word.id}
                    type="button"
                    role="option"
                    aria-selected={selected}
                    tabIndex={word.id === tabStopId ? 0 : -1}
                    onFocus={(): void => {
                      setFocusedWordId(word.id);
                    }}
                    onClick={(event): void => {
                      activate(word.id, event.shiftKey);
                    }}
                    className={cn(
                      'inline-flex min-h-12 min-w-12 items-center rounded-md border px-2 py-1 text-sm transition-colors focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring',
                      selected
                        ? 'border-primary bg-primary text-primary-foreground'
                        : 'border-input bg-transparent hover:bg-accent',
                    )}
                  >
                    {word.text}
                  </button>
                );
              })}
            </div>
          </div>
        ))}
      </div>
    </section>
  );
}

/** The selected words + their exact boundary readout. */
function SelectionReadout({
  words,
  range,
  clipReady,
}: {
  readonly clipReady: boolean;
  readonly words: readonly TranscriptWord[];
  readonly range: { min: number; max: number };
}): JSX.Element {
  const { t } = useLingui();
  const selectedWords = words.slice(range.min, range.max + 1);
  const first = selectedWords.at(0);
  const last = selectedWords.at(-1);
  if (first === undefined || last === undefined) {
    return <div className="min-w-0 break-words text-sm">{t`Selection unavailable`}</div>;
  }
  const startLabel = formatBoundaryLabel(first.startBoundary);
  const endLabel = formatBoundaryLabel(last.endBoundary);
  const startUnit = describeBoundaryUnit(first.startBoundary);
  const endUnit = describeBoundaryUnit(last.endBoundary);
  return (
    <div className="flex min-w-0 flex-col gap-1">
      <p className="min-w-0 break-words text-sm">
        {t`Selected:`}{' '}
        <span className="font-medium">
          {selectedWords.map((word) => word.text).join(' ')}
        </span>
      </p>
      <p className="min-w-0 break-words font-mono text-xs text-muted-foreground">
        {startLabel !== null && endLabel !== null
          ? `${startLabel} → ${endLabel}`
          : t`Timecodes not aligned`}
      </p>
      <p data-testid="transcript-clip-readiness" className="text-xs text-muted-foreground">{clipReady ? t`Clip-ready selection` : t`Text selection only — alignment, asset grid, interval order or transcript revision is unavailable.`}</p>
      {startUnit !== null && endUnit !== null ? (
        <p className="min-w-0 break-words text-xs text-muted-foreground">
          {`${startUnit} · ${endUnit}`}
        </p>
      ) : null}
    </div>
  );
}

/** One endpoint (IN/OUT) with its 48px step buttons. */
function EndpointStepper({
  label,
  stepBackwardDisabled,
  stepForwardDisabled,
  onStepBackward,
  onStepForward,
}: {
  readonly label: string;
  readonly stepBackwardDisabled: boolean;
  readonly stepForwardDisabled: boolean;
  readonly onStepBackward: () => void;
  readonly onStepForward: () => void;
}): JSX.Element {
  const { t } = useLingui();
  return (
    <span className="flex min-w-0 items-center gap-1">
      <span className="text-xs font-medium text-muted-foreground">{label}</span>
      <TouchAction
        variant="outline"
        disabled={stepBackwardDisabled}
        onPress={onStepBackward}
        aria-label={t`Step ${label} back one word`}
      >
        <span aria-hidden="true">◀</span>
      </TouchAction>
      <TouchAction
        variant="outline"
        disabled={stepForwardDisabled}
        onPress={onStepForward}
        aria-label={t`Step ${label} forward one word`}
      >
        <span aria-hidden="true">▶</span>
      </TouchAction>
    </span>
  );
}
