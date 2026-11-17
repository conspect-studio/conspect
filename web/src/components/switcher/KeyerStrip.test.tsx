import { describe, expect, it, vi } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { messages } from '../../locales/en/messages';
import { KeyerStrip } from './KeyerStrip';
import type { KeyerRow } from '../../api/keyers';

i18n.load('en', messages);
i18n.activate('en');

const BASE_ROW: KeyerRow = {
  id: 'dsk1', family: 'dsk', armed: false, fill: 'gfx-channel-1', key: null, type: 'linear', clip: 0, gain: 1, invert: false,
  fly: null, mask: null, max_source: null, on_air: false, tie: false, ramp: null, rate: 15,
  loss: 'drop_to_off_air', loss_trigger: 'reconnecting', stale: false, pending: false,
};
function withRamp(row: KeyerRow, ramp: NonNullable<KeyerRow['ramp']>): KeyerRow {
  return { ...row, ramp };
}

function withI18n(node: React.ReactNode) {
  return render(<I18nProvider i18n={i18n}>{node}</I18nProvider>);
}

describe('KeyerStrip', () => {
  it('shows OFF AIR idle state with enabled controls', () => {
    withI18n(<KeyerStrip row={BASE_ROW} tick={100} disabledReason={undefined} onTie={vi.fn()} onCut={vi.fn()} onAuto={vi.fn()} />);
    const lamp = screen.getByTestId('keyer-lamp-dsk1');
    expect(lamp).toHaveAttribute('data-on-air', 'false');
    expect(screen.getByRole('button', { name: 'Cut' })).toHaveProperty('disabled', false);
    expect(screen.getByRole('button', { name: 'Auto' })).toHaveProperty('disabled', false);
  });

  it('shows ON AIR lamp lit', () => {
    withI18n(<KeyerStrip row={{ ...BASE_ROW, on_air: true }} tick={100} disabledReason={undefined} onTie={vi.fn()} onCut={vi.fn()} onAuto={vi.fn()} />);
    expect(screen.getByTestId('keyer-lamp-dsk1')).toHaveAttribute('data-on-air', 'true');
    expect(screen.getByText('ON AIR')).toBeTruthy();
  });

  it('shows ramp progress while ramping', () => {
    const ramping = withRamp({ ...BASE_ROW, on_air: true }, { start_tick: 90, rate: 20, to_on: true });
    withI18n(<KeyerStrip row={ramping} tick={100} disabledReason={undefined} onTie={vi.fn()} onCut={vi.fn()} onAuto={vi.fn()} />);
    const lamp = screen.getByTestId('keyer-lamp-dsk1');
    expect(lamp).toHaveAttribute('data-ramping', 'true');
    // (100 - 90) / 20 = 50%.
    expect(within(lamp).getByTestId('keyer-ramp-dsk1').textContent).toContain('50%');
  });

  it('reflects tie state and toggles it', async () => {
    const onTie = vi.fn();
    withI18n(<KeyerStrip row={{ ...BASE_ROW, tie: true }} tick={100} disabledReason={undefined} onTie={onTie} onCut={vi.fn()} onAuto={vi.fn()} />);
    const checkbox = screen.getByRole('checkbox', { name: 'Tie' });
    expect(checkbox).toHaveProperty('checked', true);
    await userEvent.click(checkbox);
    expect(onTie).toHaveBeenCalledWith(false);
  });

  it('disables every control with the reason as title and aria-describedby', () => {
    withI18n(<KeyerStrip row={BASE_ROW} tick={100} disabledReason="Switcher unavailable" onTie={vi.fn()} onCut={vi.fn()} onAuto={vi.fn()} />);
    const cut = screen.getByRole('button', { name: 'Cut' });
    expect(cut).toHaveProperty('disabled', true);
    expect(cut).toHaveAttribute('title', 'Switcher unavailable');
    const describedBy = cut.getAttribute('aria-describedby');
    expect(describedBy).toBeTruthy();
    expect(document.getElementById(describedBy!)?.textContent).toBe('Switcher unavailable');
  });

  it('shows stale and pending badges', () => {
    withI18n(<KeyerStrip row={{ ...BASE_ROW, stale: true, pending: true }} tick={100} disabledReason={undefined} onTie={vi.fn()} onCut={vi.fn()} onAuto={vi.fn()} />);
    expect(screen.getByTestId('keyer-stale-dsk1')).toBeTruthy();
    expect(screen.getByTestId('keyer-pending-dsk1')).toBeTruthy();
  });

  it('CUT and AUTO call back with the current rate', async () => {
    const onCut = vi.fn();
    const onAuto = vi.fn();
    withI18n(<KeyerStrip row={BASE_ROW} tick={100} disabledReason={undefined} onTie={vi.fn()} onCut={onCut} onAuto={onAuto} />);
    await userEvent.click(screen.getByRole('button', { name: 'Cut' }));
    expect(onCut).toHaveBeenCalledTimes(1);
    const rateInput = screen.getByLabelText('Rate in frames for dsk1');
    await userEvent.clear(rateInput);
    await userEvent.type(rateInput, '30');
    await userEvent.click(screen.getByRole('button', { name: 'Auto' }));
    expect(onAuto).toHaveBeenCalledWith(30);
  });

  it('disables AUTO (but not CUT) for an out-of-range rate, with a reason', async () => {
    withI18n(<KeyerStrip row={BASE_ROW} tick={100} disabledReason={undefined} onTie={vi.fn()} onCut={vi.fn()} onAuto={vi.fn()} />);
    const rateInput = screen.getByLabelText('Rate in frames for dsk1');
    await userEvent.clear(rateInput);
    await userEvent.type(rateInput, '301');
    const auto = screen.getByRole('button', { name: 'Auto' });
    expect(auto).toHaveProperty('disabled', true);
    expect(auto.getAttribute('title')).toMatch(/1–300/);
    expect(screen.getByRole('button', { name: 'Cut' })).toHaveProperty('disabled', false);
  });

  it('K5-09: every disabled strip control (incl. the rate input) carries the reason', () => {
    withI18n(<KeyerStrip row={BASE_ROW} tick={100} disabledReason="A switcher request is in flight" onTie={vi.fn()} onCut={vi.fn()} onAuto={vi.fn()} />);
    const controls = screen.getByTestId('keyer-strip-dsk1').querySelectorAll<HTMLElement>('button, input');
    expect(controls.length).toBe(4);
    for (const control of controls) {
      expect(control).toHaveProperty('disabled', true);
      expect(control.getAttribute('title')).toBe('A switcher request is in flight');
      expect(document.getElementById(control.getAttribute('aria-describedby') ?? '')?.textContent).toBe('A switcher request is in flight');
    }
  });

  it('the rate follows the committed rate until edited', () => {
    const view = withI18n(<KeyerStrip row={BASE_ROW} tick={100} disabledReason={undefined} onTie={vi.fn()} onCut={vi.fn()} onAuto={vi.fn()} />);
    view.rerender(<I18nProvider i18n={i18n}><KeyerStrip row={{ ...BASE_ROW, rate: 40 }} tick={100} disabledReason={undefined} onTie={vi.fn()} onCut={vi.fn()} onAuto={vi.fn()} /></I18nProvider>);
    expect(screen.getByLabelText<HTMLInputElement>('Rate in frames for dsk1').value).toBe('40');
  });
});
