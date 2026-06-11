// Component tests for the Clock + Timer synthetic source-kind forms
// (SYN-CLOCK-UI, ADR-0047). These are ADDITIVE to SimplePages.test.tsx (the
// WebUI-owner's smoke suite) and cover only the clock/timer kind forms:
//   * the create dialog renders the kind-specific fields after picking the kind,
//   * the edit dialog round-trips a stored clock / timer body onto the form,
//   * the target-type switch reveals the right `at` field + recur toggle,
//   * an invalid IANA timezone surfaces an inline, SR-wired per-field error.
// The pure form<->body mapping is exhaustively covered in
// ../resources/forms.test.ts; this guards the rendered wiring.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { SourcesPage } from './SourcesPage';
import { renderWithProviders } from '../test/render';

const SOURCES = [
  {
    id: 'syd',
    name: 'Sydney',
    body: {
      id: 'syd',
      kind: 'clock',
      face: 'dual',
      twelve_hour: true,
      timezone: 'Australia/Sydney',
      label: 'Sydney',
      show_offset: true,
      show_reference: false,
      numerals: true,
    },
  },
  {
    id: 'onair',
    name: 'On air in',
    body: {
      id: 'onair',
      kind: 'timer',
      target: 'time_of_day',
      at: '14:30:00',
      timezone: 'Australia/Sydney',
      recur_daily: true,
      direction: 'down',
      on_target: 'recur',
      format: 'mm_ss',
      label: 'ON AIR IN',
      overrun_badge: true,
    },
  },
];

const server = setupServer(
  http.get('*/api/v1/sources', () => HttpResponse.json(SOURCES)),
  http.get('*/api/v1/sources/:id', ({ params }) => {
    const found = SOURCES.find((s) => s.id === String(params.id));
    return found
      ? HttpResponse.json(found, { headers: { ETag: '"1"' } })
      : new HttpResponse(null, { status: 404 });
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
});
afterAll(() => {
  server.close();
});

function renderSources(): void {
  renderWithProviders(
    <MemoryRouter>
      <SourcesPage />
    </MemoryRouter>,
  );
}

/** Open the create dialog over the populated table. */
async function openCreateDialog(): Promise<HTMLElement> {
  renderSources();
  expect(await screen.findByText('Sydney')).toBeInTheDocument();
  await userEvent.click(screen.getByRole('button', { name: 'New source' }));
  return screen.findByRole('dialog');
}

/** Pick a kind in the Kind <Select> (Radix listbox). */
async function pickKind(name: string): Promise<void> {
  // The Kind select is the first combobox in the dialog.
  const triggers = screen.getAllByRole('combobox');
  const kindTrigger = triggers[0];
  if (kindTrigger === undefined) {
    throw new Error('no Kind select trigger found');
  }
  await userEvent.click(kindTrigger);
  await userEvent.click(await screen.findByRole('option', { name }));
}

describe('Clock source form (ADR-0047)', () => {
  it('reveals the clock metadata fields when the clock kind is picked', async () => {
    await openCreateDialog();
    await pickKind('clock');
    // The IANA timezone field, the label, and the metadata toggles render.
    expect(await screen.findByLabelText('Timezone (IANA id)')).toBeInTheDocument();
    expect(screen.getByLabelText('Label (optional)')).toBeInTheDocument();
    expect(screen.getByLabelText('Show the UTC offset badge')).toBeInTheDocument();
    expect(
      screen.getByLabelText('Show the reference-lock badge (PTP/NTP/SYS)'),
    ).toBeInTheDocument();
    expect(
      screen.getByLabelText('Draw hour numerals (analogue / dual face)'),
    ).toBeInTheDocument();
  });

  it('hides the fixed-offset field once an IANA zone is entered', async () => {
    await openCreateDialog();
    await pickKind('clock');
    const zone = await screen.findByLabelText('Timezone (IANA id)');
    // With no zone, the fixed-offset minutes field is shown.
    expect(screen.getByLabelText('Timezone offset (minutes from UTC)')).toBeInTheDocument();
    await userEvent.type(zone, 'Australia/Sydney');
    // Entering a zone hides the now-ignored fixed offset.
    expect(
      screen.queryByLabelText('Timezone offset (minutes from UTC)'),
    ).not.toBeInTheDocument();
  });

  it('surfaces an inline error for an unknown IANA timezone on submit', async () => {
    await openCreateDialog();
    await userEvent.type(screen.getByLabelText('Identifier'), 'clk');
    await userEvent.type(screen.getByLabelText('Name'), 'Bad clock');
    await pickKind('clock');
    const zone = await screen.findByLabelText('Timezone (IANA id)');
    await userEvent.type(zone, 'Mars/Olympus');
    await userEvent.click(screen.getByRole('button', { name: 'Create' }));
    expect(await screen.findByText(/valid IANA timezone id/i)).toBeInTheDocument();
    expect(zone).toHaveAttribute('aria-invalid', 'true');
    expect(zone.getAttribute('aria-describedby') ?? '').not.toBe('');
  });

  it('round-trips a stored dual-face clock into the edit dialog', async () => {
    renderSources();
    expect(await screen.findByText('Sydney')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Edit source: Sydney' }));
    expect(await screen.findByRole('dialog')).toBeInTheDocument();
    // The metadata prefills from the stored body (each read by its label so the
    // Name field, which also happens to be "Sydney", is never mistaken for it).
    expect(await screen.findByDisplayValue('Australia/Sydney')).toBeInTheDocument();
    expect(screen.getByLabelText<HTMLInputElement>('Label (optional)').value).toBe('Sydney');
    expect(
      screen.getByLabelText<HTMLInputElement>('Show the UTC offset badge').checked,
    ).toBe(true);
    expect(
      screen.getByLabelText<HTMLInputElement>('Draw hour numerals (analogue / dual face)').checked,
    ).toBe(true);
  });
});

describe('Timer source form (ADR-0047)', () => {
  it('reveals the timer fields and the time-of-day at + recur toggle by default', async () => {
    await openCreateDialog();
    await pickKind('timer');
    expect(await screen.findByLabelText('Target time of day')).toBeInTheDocument();
    expect(screen.getByLabelText('Recur daily (re-arm each day)')).toBeInTheDocument();
    expect(screen.getByLabelText('Overrun prefix (optional)')).toBeInTheDocument();
    expect(
      screen.getByLabelText('Show the overrun badge (OVER / ELAPSED)'),
    ).toBeInTheDocument();
  });

  it('surfaces an inline error for a malformed time-of-day on submit', async () => {
    await openCreateDialog();
    await userEvent.type(screen.getByLabelText('Identifier'), 'tmr');
    await userEvent.type(screen.getByLabelText('Name'), 'Bad timer');
    await pickKind('timer');
    const at = await screen.findByLabelText('Target time of day');
    await userEvent.type(at, '25:00:00');
    await userEvent.click(screen.getByRole('button', { name: 'Create' }));
    expect(await screen.findByText(/24-hour time of day as HH:MM:SS/i)).toBeInTheDocument();
    expect(at).toHaveAttribute('aria-invalid', 'true');
  });

  it('round-trips a stored time-of-day timer into the edit dialog', async () => {
    renderSources();
    expect(await screen.findByText('On air in')).toBeInTheDocument();
    await userEvent.click(screen.getByRole('button', { name: 'Edit source: On air in' }));
    const dialog = await screen.findByRole('dialog');
    // The target time-of-day, zone and label prefill.
    expect(await within(dialog).findByDisplayValue('14:30:00')).toBeInTheDocument();
    expect(within(dialog).getByDisplayValue('ON AIR IN')).toBeInTheDocument();
    // The recur-daily toggle (time-of-day only) is shown and on.
    expect(
      within(dialog).getByLabelText<HTMLInputElement>('Recur daily (re-arm each day)').checked,
    ).toBe(true);
  });
});
