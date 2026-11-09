// TranscriptRangeEditor tests — the controlled word-range contract:
//   * the approved tap flow: tap anchors → second tap selects the RANGE
//     BETWEEN → a third tap re-anchors; Shift always extends;
//   * 48px IN/OUT endpoint steppers step the range ends one WORD at a
//     time and disable at the edges and before crossing;
//   * keyboard: arrows/Home/End move focus (plain = re-anchor, Shift =
//     extend); Enter activates the focused word;
//   * the selection readout shows EXACT grid boundaries — NDF timecode
//     plus the integer index and exact rational pair, never float
//     seconds; unaligned words render the honest fallback, never NaN;
//   * fully controlled: firing any intent changes NOTHING until the
//     caller re-renders (aria-selected/readouts are prop-driven);
//   * readonly mode renders static words with the caller's capability
//     explanation — never a fake editable transcript.
import { describe, expect, it, vi } from 'vitest';
import { fireEvent, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';

import { renderWithProviders } from '../../../test/render';
import { TranscriptRangeEditor } from './TranscriptRangeEditor';
import type {
  TranscriptSelection,
  TranscriptWord,
} from './types';

function word(
  id: string,
  text: string,
  startFrame: number,
  segmentId: string | null,
  speakerLabel: string | null,
  aligned = true,
): TranscriptWord {
  return {
    id,
    text,
    startBoundary: aligned
      ? { kind: 'frames', index: startFrame, numerator: 25, denominator: 1 }
      : null,
    endBoundary: aligned
      ? { kind: 'frames', index: startFrame + 25, numerator: 25, denominator: 1 }
      : null,
    confidence: 0.93,
    speakerLabel,
    segmentId,
  };
}

const WORDS: readonly TranscriptWord[] = [
  word('w0', 'the', 0, 's1', 'SPEAKER 1'),
  word('w1', 'wide', 25, 's1', 'SPEAKER 1'),
  word('w2', 'match', 50, 's1', 'SPEAKER 1'),
  word('w3', 'tonight', 75, 's1', 'SPEAKER 1'),
  word('w4', 'starts', 100, 's2', 'SPEAKER 2'),
  word('w5', 'right', 125, 's2', 'SPEAKER 2'),
  word('w6', 'after', 150, 's2', 'SPEAKER 2'),
  word('w7', 'this', 175, 's2', 'SPEAKER 2'),
];

function renderEditor(overrides: {
  words?: readonly TranscriptWord[];
  selection?: TranscriptSelection | null;
  readOnly?: boolean;
  readOnlyReason?: string;
} = {}) {
  const onSelectionChange = vi.fn();
  const view = renderWithProviders(
    <TranscriptRangeEditor
      transcriptRevision="rev-1" alignedRevision="rev-1"
      words={overrides.words ?? WORDS}
      selection={overrides.selection ?? null}
      onSelectionChange={onSelectionChange}
      readOnly={overrides.readOnly ?? false}
      readOnlyReason={overrides.readOnlyReason}
    />,
  );
  // NOTE: the spread comes FIRST so the custom rerender below is not
  // shadowed by the RenderResult's own rerender.
  const rerender = (selection: TranscriptSelection | null): void => {
    view.rerender(
      <TranscriptRangeEditor
        transcriptRevision="rev-1" alignedRevision="rev-1"
      words={overrides.words ?? WORDS}
        selection={selection}
        onSelectionChange={onSelectionChange}
        readOnly={overrides.readOnly ?? false}
        readOnlyReason={overrides.readOnlyReason}
      />,
    );
  };
  return { ...view, onSelectionChange, rerender };
}

/** The option button for a word id (accessible names are the word TEXT). */
const option = (id: string): HTMLElement =>
  screen.getByRole('option', {
    name: (_accessibleName: string, element: Element): boolean =>
      element instanceof HTMLElement && element.dataset.wordId === id,
  });

describe('TranscriptRangeEditor — the approved tap flow', () => {
  it('tap anchors, second tap selects the range between, third tap re-anchors', async () => {
    const user = userEvent.setup();
    const helpers = renderEditor();
    await user.click(option('w2'));
    expect(helpers.onSelectionChange).toHaveBeenCalledWith({
      startWordId: 'w2',
      endWordId: null,
    });
    // No silent mutation: the DOM still shows the prop's (empty) selection.
    expect(option('w2')).toHaveAttribute('aria-selected', 'false');

    // The caller applies the anchor; now the DOM reflects it.
    helpers.rerender({ startWordId: 'w2', endWordId: null });
    expect(option('w2')).toHaveAttribute('aria-selected', 'true');

    await user.click(option('w5'));
    expect(helpers.onSelectionChange).toHaveBeenCalledWith({
      startWordId: 'w2',
      endWordId: 'w5',
    });

    // A third tap INSIDE the range re-anchors (prototype semantics).
    helpers.rerender({ startWordId: 'w2', endWordId: 'w5' });
    await user.click(option('w3'));
    expect(helpers.onSelectionChange).toHaveBeenCalledWith({
      startWordId: 'w3',
      endWordId: null,
    });
  });

  it('the selection renders from props once the caller re-renders', () => {
    renderEditor({ selection: { startWordId: 'w2', endWordId: 'w5' } });
    const listbox = screen.getByRole('listbox', { name: /Transcript words/ });
    const selected = within(listbox)
      .getAllByRole('option')
      .filter((node) => node.getAttribute('aria-selected') === 'true')
      .map((node) => node.dataset.wordId);
    expect(selected).toEqual(['w2', 'w3', 'w4', 'w5']);
  });

  it('Shift+click always extends from the anchor', () => {
    const helpers = renderEditor({
      selection: { startWordId: 'w2', endWordId: 'w5' },
    });
    fireEvent.click(option('w6'), { shiftKey: true });
    expect(helpers.onSelectionChange).toHaveBeenCalledWith({
      startWordId: 'w2',
      endWordId: 'w6',
    });
  });
});

describe('TranscriptRangeEditor — keyboard selection', () => {
  it('arrows move focus; plain arrows re-anchor, Shift+arrow extends', async () => {
    const user = userEvent.setup();
    const helpers = renderEditor();
    await user.click(option('w0')); // focus + anchor
    await user.keyboard('{ArrowRight}');
    expect(helpers.onSelectionChange).toHaveBeenLastCalledWith({
      startWordId: 'w1',
      endWordId: null,
    });
    // Controlled caller: feed the anchor back before extending.
    helpers.rerender({ startWordId: 'w1', endWordId: null });
    await user.keyboard('{Shift>}{ArrowRight}{/Shift}');
    expect(helpers.onSelectionChange).toHaveBeenLastCalledWith({
      startWordId: 'w1',
      endWordId: 'w2',
    });
    // Focus is on w2 after the shift-extend; plain ArrowLeft moves to w1
    // and re-anchors there.
    helpers.rerender({ startWordId: 'w1', endWordId: 'w2' });
    await user.keyboard('{ArrowLeft}');
    expect(helpers.onSelectionChange).toHaveBeenLastCalledWith({
      startWordId: 'w1',
      endWordId: null,
    });
  });

  it('Home/End jump to the first/last word', async () => {
    const user = userEvent.setup();
    const helpers = renderEditor();
    await user.click(option('w3'));
    await user.keyboard('{Home}');
    expect(helpers.onSelectionChange).toHaveBeenLastCalledWith({
      startWordId: 'w0',
      endWordId: null,
    });
    await user.keyboard('{End}');
    expect(helpers.onSelectionChange).toHaveBeenLastCalledWith({
      startWordId: 'w7',
      endWordId: null,
    });
  });

  it('Enter activates the focused word with the tap semantics', async () => {
    const user = userEvent.setup();
    const helpers = renderEditor({
      selection: { startWordId: 'w2', endWordId: null },
    });
    // Clicking the anchor itself starts the range there (anchor → {w2,w2}).
    await user.click(option('w2'));
    expect(helpers.onSelectionChange).toHaveBeenLastCalledWith({
      startWordId: 'w2',
      endWordId: 'w2',
    });
    helpers.rerender({ startWordId: 'w2', endWordId: 'w2' });
    await user.keyboard('{ArrowRight}');
    expect(helpers.onSelectionChange).toHaveBeenLastCalledWith({
      startWordId: 'w3',
      endWordId: null,
    });
    helpers.rerender({ startWordId: 'w3', endWordId: null });
    // Enter activates the focused word (w3): anchor → range {w3,w3}.
    await user.keyboard('{Enter}');
    expect(helpers.onSelectionChange).toHaveBeenLastCalledWith({
      startWordId: 'w3',
      endWordId: 'w3',
    });
  });

  it('keeps a single roving tab stop derived from focus/anchor', () => {
    renderEditor({ selection: { startWordId: 'w4', endWordId: null } });
    const tabbables = screen
      .getAllByRole('option')
      .filter((node) => node.tabIndex === 0);
    expect(tabbables).toHaveLength(1);
    expect(tabbables[0]?.dataset.wordId).toBe('w4');
  });
});

describe('TranscriptRangeEditor — 48px endpoint steppers', () => {
  it('steps IN/OUT one word at a time, as intents', () => {
    const helpers = renderEditor({
      selection: { startWordId: 'w2', endWordId: 'w5' },
    });
    const inBack = screen.getByRole('button', {
      name: /Step IN back one word/,
    });
    expect(inBack.className).toMatch(/\bh-12\b/);
    fireEvent.click(inBack);
    expect(helpers.onSelectionChange).toHaveBeenCalledWith({
      startWordId: 'w1',
      endWordId: 'w5',
    });
    const outForward = screen.getByRole('button', {
      name: /Step OUT forward one word/,
    });
    fireEvent.click(outForward);
    expect(helpers.onSelectionChange).toHaveBeenCalledWith({
      startWordId: 'w2',
      endWordId: 'w6',
    });
  });

  it('disables the edges: list bounds and the single-word range', () => {
    const fullRange = renderEditor({
      selection: { startWordId: 'w0', endWordId: 'w7' },
    });
    const fullScope = within(fullRange.container);
    expect(
      fullScope.getByRole('button', { name: /Step IN back one word/ }),
    ).toBeDisabled();
    expect(
      fullScope.getByRole('button', { name: /Step OUT forward one word/ }),
    ).toBeDisabled();

    const single = renderEditor({
      selection: { startWordId: 'w3', endWordId: 'w3' },
    });
    const singleScope = within(single.container);
    expect(
      singleScope.getByRole('button', { name: /Step IN forward one word/ }),
    ).toBeDisabled();
    expect(
      singleScope.getByRole('button', { name: /Step OUT back one word/ }),
    ).toBeDisabled();
  });
});

describe('TranscriptRangeEditor — exact boundary readout', () => {
  it('shows NDF timecodes plus the exact index/rational unit lines', () => {
    renderEditor({
      selection: { startWordId: 'w2', endWordId: 'w5' },
    });
    const readout = screen.getByTestId('transcript-selection-readout');
    expect(readout).toHaveTextContent(/Selected: match tonight starts right/);
    // 25 fps exact grid: w2 starts at FR 50 (00:00:02:00); w5 ends at
    // FR 150 (00:00:06:00). Integer index + exact pair, never a float.
    expect(readout).toHaveTextContent(/00:00:02:00 → 00:00:06:00/);
    expect(readout).toHaveTextContent(/FR 50 · 25\/1/);
    expect(readout).toHaveTextContent(/FR 150 · 25\/1/);
    expect(readout.textContent).not.toMatch(/NaN|undefined/);
  });

  it('renders the honest not-aligned fallback for unaligned words', () => {
    const unaligned = WORDS.map((w, i) =>
      i === 2 ? word('w2', 'match', 50, 's1', 'SPEAKER 1', false) : w,
    );
    renderEditor({
      words: unaligned,
      selection: { startWordId: 'w2', endWordId: 'w5' },
    });
    const readout = screen.getByTestId('transcript-selection-readout');
    expect(readout).toHaveTextContent(/Timecodes not aligned/);
    expect(readout.textContent).not.toMatch(/NaN|undefined/);
  });

  it('renders segment headers with speaker labels once per group', () => {
    renderEditor();
    const headers = screen.getAllByTestId('transcript-segment-header');
    expect(headers).toHaveLength(2);
    expect(headers[0]).toHaveTextContent(/SPEAKER 1/);
    expect(headers[0]).toHaveTextContent(/00:00:00:00/);
    expect(headers[1]).toHaveTextContent(/SPEAKER 2/);
    expect(headers[1]).toHaveTextContent(/00:00:04:00/);
  });
});

describe('TranscriptRangeEditor — readonly and empty honesty', () => {
  it('readonly renders static words with the capability explanation, no interactions', () => {
    const helpers = renderEditor({
      readOnly: true,
      readOnlyReason: 'No STT backend exists yet — transcript jobs are a concept.',
    });
    expect(screen.queryByRole('listbox')).toBeNull();
    expect(screen.queryByRole('option')).toBeNull();
    expect(screen.queryByTestId('transcript-selection-readout')).toBeNull();
    const explanation = screen.getByTestId('transcript-readonly-explanation');
    expect(explanation).toHaveTextContent(
      /Transcript is read-only — selection is unavailable\./,
    );
    expect(explanation).toHaveTextContent(
      /No STT backend exists yet — transcript jobs are a concept\./,
    );
    // Clicking static text can never fire a selection intent.
    fireEvent.click(screen.getByText('match'));
    expect(helpers.onSelectionChange).not.toHaveBeenCalled();
  });

  it('an empty transcript is an honest empty state with the reason', () => {
    renderEditor({
      words: [],
      readOnlyReason: 'No STT backend exists yet.',
    });
    const editor = screen.getByTestId('transcript-range-editor');
    expect(editor.dataset.state).toBe('empty');
    expect(editor).toHaveTextContent(/No transcript available for this asset\./);
    expect(editor).toHaveTextContent(/No STT backend exists yet\./);
  });

  it('words keep the 48px touch floor', () => {
    renderEditor();
    const match = option('w2');
    expect(match.className).toMatch(/\bmin-h-12\b/);
    expect(match.className).toMatch(/\bmin-w-12\b/);
  });
});

describe('TranscriptRangeEditor — precision readiness', () => {
  it('keeps unaligned interior words text-only and blocks all precision steppers', () => {
    const words = [WORDS[0]!, { ...WORDS[1]!, startBoundary: null }, WORDS[2]!];
    const onSelectionChange = vi.fn();
    renderWithProviders(<TranscriptRangeEditor words={words} selection={{ startWordId: 'w0', endWordId: 'w2' }} transcriptRevision="rev-1" alignedRevision="rev-1" onSelectionChange={onSelectionChange} />);
    expect(screen.getByTestId('transcript-selection-readout')).toHaveTextContent(/Text selection only/);
    for (const button of within(screen.getByTestId('transcript-selection-readout')).getAllByRole('button')) expect(button).toBeDisabled();
  });
  it('blocks clip precision on mixed grids, overlaps, zero intervals and stale revision', () => {
    const candidates = [
      { words: [WORDS[0]!, { ...WORDS[1]!, startBoundary: { kind: 'frames' as const, index: 25, numerator: 30000, denominator: 1001 } }, WORDS[2]!], revision: 'r1' },
      { words: [WORDS[0]!, { ...WORDS[1]!, startBoundary: { kind: 'frames' as const, index: 20, numerator: 25, denominator: 1 } }, WORDS[2]!], revision: 'r1' },
      { words: [WORDS[0]!, { ...WORDS[1]!, endBoundary: { kind: 'frames' as const, index: 25, numerator: 25, denominator: 1 } }, WORDS[2]!], revision: 'r1' },
      { words: [WORDS[0]!, WORDS[1]!, WORDS[2]!], revision: 'stale' },
    ];
    for (const item of candidates) {
      const view = renderWithProviders(<TranscriptRangeEditor words={item.words} selection={{ startWordId: 'w0', endWordId: 'w2' }} transcriptRevision={item.revision} alignedRevision="r1" onSelectionChange={vi.fn()} />);
      expect(within(view.container).getByTestId('transcript-clip-readiness')).toHaveTextContent(/Text selection only/);
      for (const button of within(view.container).getAllByRole('button', { name: /Step (IN|OUT)/ })) expect(button).toBeDisabled();
      view.unmount();
    }
  });
  it('restores a tab stop after words change and readonly toggles', () => {
    const onSelectionChange = vi.fn();
    const view = renderWithProviders(<TranscriptRangeEditor words={WORDS} selection={null} onSelectionChange={onSelectionChange} />);
    fireEvent.focus(option('w2'));
    view.rerender(<TranscriptRangeEditor words={[WORDS[3]!, WORDS[4]!]} selection={null} onSelectionChange={onSelectionChange} readOnly />);
    view.rerender(<TranscriptRangeEditor words={[WORDS[3]!, WORDS[4]!]} selection={null} onSelectionChange={onSelectionChange} />);
    expect(option('w3')).toHaveAttribute('tabindex', '0');
  });
});
