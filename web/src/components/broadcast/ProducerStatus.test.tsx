// ProducerStatus tests — the truth-separation contract from the correction-4
// safety review (§S2 and criteria 7): the OUTPUT PRODUCER state is derived
// from observed ticks only. A management connection — however healthy — never
// feeds the producer verdict; a missing tick is UNKNOWN, an old tick is
// FROZEN / STALE at an exact threshold, and a future timestamp cannot verify
// freshness, so it is UNKNOWN too. Age is measured against the caller's
// clock, never against an internal one.
import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/react';

import { TestProviders } from '../../test/render';
import { deriveProducerDisplay } from './producerDisplay';
import { ProducerStatus } from './ProducerStatus';

const NOW = 1_000_000;

describe('deriveProducerDisplay — pure derivation', () => {
  it('derives producing from a fresh tick and the producing state', () => {
    expect(deriveProducerDisplay('producing', NOW - 2400, NOW, 3000)).toEqual({
      display: 'producing',
      ageMs: 2400,
      reason: null,
    });
  });

  it('crosses the stale threshold at exactly staleAfterMs', () => {
    expect(deriveProducerDisplay('producing', NOW - 2999, NOW, 3000).display).toBe('producing');
    const at = deriveProducerDisplay('producing', NOW - 3000, NOW, 3000);
    expect(at.display).toBe('stale');
    expect(at.ageMs).toBe(3000);
  });

  it('treats a missing tick as unknown even when the state enum claims producing', () => {
    expect(deriveProducerDisplay('producing', null, NOW, 3000)).toEqual({
      display: 'unknown',
      ageMs: null,
      reason: 'no-tick',
    });
  });

  it('treats a future timestamp as unverifiable, never fresh', () => {
    expect(deriveProducerDisplay('producing', NOW + 5000, NOW, 3000)).toEqual({
      display: 'unknown',
      ageMs: null,
      reason: 'future-tick',
    });
  });

  it('lets failed and stopped win over tick freshness', () => {
    expect(deriveProducerDisplay('failed', NOW - 100, NOW, 3000).display).toBe('failed');
    expect(deriveProducerDisplay('stopped', NOW - 100, NOW, 3000).display).toBe('stopped');
  });

  it('keeps a fresh tick with an unreported state as unknown', () => {
    expect(deriveProducerDisplay('unknown', NOW - 100, NOW, 3000)).toEqual({
      display: 'unknown',
      ageMs: 100,
      reason: 'state-unknown',
    });
  });
});

describe('ProducerStatus — rendering', () => {
  it('shows producing with the real tick age and source', () => {
    render(
      <TestProviders>
        <ProducerStatus
          state="producing"
          lastTickAt={NOW - 2400}
          tickSource="output engine tick"
          now={() => NOW}
        />
      </TestProviders>,
    );
    expect(screen.getByText('Producing')).toBeInTheDocument();
    expect(screen.getByText(/last tick 2\.4s ago/)).toBeInTheDocument();
    expect(screen.getByText('output engine tick')).toBeInTheDocument();
  });

  it('shows frozen / stale with the age at and past the threshold', () => {
    render(
      <TestProviders>
        <ProducerStatus state="producing" lastTickAt={NOW - 3000} now={() => NOW} />
      </TestProviders>,
    );
    expect(screen.getByText('Frozen / stale')).toBeInTheDocument();
    expect(screen.getByText(/last tick 3\.0s ago/)).toBeInTheDocument();
  });

  it('shows unknown with no output tick observed when the tick is missing', () => {
    render(
      <TestProviders>
        <ProducerStatus state="producing" lastTickAt={null} now={() => NOW} />
      </TestProviders>,
    );
    expect(screen.getByText('Unknown')).toBeInTheDocument();
    expect(screen.getByText('No output tick observed')).toBeInTheDocument();
    expect(screen.queryByText('Producing')).toBeNull();
  });

  it('shows unknown with a clock-skew note when the tick is in the future', () => {
    render(
      <TestProviders>
        <ProducerStatus state="producing" lastTickAt={NOW + 5000} now={() => NOW} />
      </TestProviders>,
    );
    expect(screen.getByText('Unknown')).toBeInTheDocument();
    expect(
      screen.getByText(/Tick timestamp is ahead of the clock/),
    ).toBeInTheDocument();
    expect(screen.queryByText('Producing')).toBeNull();
  });

  it('shows failed even with a fresh tick, and stopped plainly', () => {
    render(
      <TestProviders>
        <ProducerStatus state="failed" lastTickAt={NOW - 100} now={() => NOW} />
      </TestProviders>,
    );
    expect(screen.getByText('Failed')).toBeInTheDocument();
    expect(screen.queryByText('Producing')).toBeNull();
  });

  it('keeps a fresh tick with an unreported state honest', () => {
    render(
      <TestProviders>
        <ProducerStatus state="unknown" lastTickAt={NOW - 400} now={() => NOW} />
      </TestProviders>,
    );
    expect(screen.getByText('Unknown')).toBeInTheDocument();
    expect(screen.getByText('Producer state not reported')).toBeInTheDocument();
  });

  it('ages against the shared ticking clock when no clock is injected', () => {
    // The impure Date.now() read lives inside the shared store interval, so
    // this render path stays pure; the age is real, not a frozen snapshot.
    render(
      <TestProviders>
        <ProducerStatus state="producing" lastTickAt={Date.now() - 500} />
      </TestProviders>,
    );
    expect(screen.getByText('Producing')).toBeInTheDocument();
    expect(screen.getByText(/last tick \d+\.\ds ago/)).toBeInTheDocument();
  });

  it('wraps instead of clipping the status line at narrow widths', () => {
    const { container } = render(
      <TestProviders>
        <ProducerStatus state="producing" lastTickAt={NOW - 100} now={() => NOW} />
      </TestProviders>,
    );
    const root = container.firstElementChild as HTMLElement;
    expect(root.className).toMatch(/\bmin-w-0\b/);
    expect(root.className).toMatch(/break-words/);
    expect(root.className).not.toMatch(/whitespace-nowrap/);
    expect(root.className).not.toMatch(/overflow-hidden/);
    expect(root.className).not.toMatch(/truncate/);
  });
});

describe('ProducerStatus — management is a separate fact', () => {
  it('renders the management chip separately and never infers producer health from it', () => {
    render(
      <TestProviders>
        <ProducerStatus
          state="stopped"
          lastTickAt={NOW - 100}
          managementLabel="Connected"
          now={() => NOW}
        />
      </TestProviders>,
    );
    expect(screen.getByText(/Management · Connected/)).toBeInTheDocument();
    // The producer verdict stays honest: a connected management plane does
    // not make a stopped producer produce.
    expect(screen.getByText('Stopped')).toBeInTheDocument();
    expect(screen.queryByText('Producing')).toBeNull();
  });

  it('keeps the producer verdict identical with and without a management label', () => {
    const base = { state: 'producing' as const, lastTickAt: NOW - 2400, now: () => NOW };
    const { container: withoutChip } = render(
      <TestProviders>
        <ProducerStatus {...base} />
      </TestProviders>,
    );
    const { container: withChip } = render(
      <TestProviders>
        <ProducerStatus {...base} managementLabel="Connected" />
      </TestProviders>,
    );
    const badgeOf = (c: HTMLElement): string | null =>
      c.querySelector('[data-producer-badge]')?.textContent ?? null;
    expect(badgeOf(withChip)).toBe(badgeOf(withoutChip));
    expect(badgeOf(withoutChip)).toBe('Producing');
  });
});
