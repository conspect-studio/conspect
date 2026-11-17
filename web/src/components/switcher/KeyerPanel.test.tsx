import { describe, expect, it, vi } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { messages } from '../../locales/en/messages';
import { KeyerPanel } from './KeyerPanel';
import type { KeyerPlan, KeyerRow } from '../../api/keyers';

i18n.load('en', messages);
i18n.activate('en');

const BASE_ROW: KeyerRow = {
  id: 'dsk1', family: 'dsk', fill: 'gfx-channel-1', key: null, type: 'linear',
  fly: null, mask: null, on_air: false, tie: false, ramp: null, rate: 15,
  loss: 'drop_to_off_air', stale: false, pending: false,
};
const SOURCES = [{ id: 'gfx-channel-1', name: 'GFX 1' }, { id: 'cam-1', name: 'Cam 1' }];
const CANVAS = { w: 1920, h: 1080 };
const PASS_PLAN: KeyerPlan = { can_take: true, class: 'class1', keyer: 'main/dsk1', on_air: false };

function withI18n(node: React.ReactNode) {
  return render(<I18nProvider i18n={i18n}>{node}</I18nProvider>);
}

describe('KeyerPanel', () => {
  it('offers only readable sources for fill/key (no in:<n>, matching useSources())', () => {
    withI18n(<KeyerPanel row={BASE_ROW} sources={SOURCES} disabledReason={undefined} canvas={CANVAS} onPlan={vi.fn()} onApply={vi.fn()} />);
    const fill = screen.getByLabelText<HTMLSelectElement>('Fill');
    const options = Array.from(fill.options).map((o) => o.value).filter((v) => v !== '');
    expect(options).toEqual(['gfx-channel-1', 'cam-1']);
    expect(options.some((id) => id.startsWith('in:'))).toBe(false);
  });

  it('shows a redacted fill as text, not editable', () => {
    const row = { ...BASE_ROW, fill: 'redacted' };
    withI18n(<KeyerPanel row={row} sources={SOURCES} disabledReason={undefined} canvas={CANVAS} onPlan={vi.fn()} onApply={vi.fn()} />);
    expect(screen.getByTestId('keyer-fill-redacted-dsk1').textContent).toBe('redacted');
    expect(screen.queryByLabelText('Fill')).toBeNull();
  });

  it('shows a redacted key as text, not editable', () => {
    const row = { ...BASE_ROW, key: 'redacted' };
    withI18n(<KeyerPanel row={row} sources={SOURCES} disabledReason={undefined} canvas={CANVAS} onPlan={vi.fn()} onApply={vi.fn()} />);
    expect(screen.getByTestId('keyer-key-redacted-dsk1').textContent).toBe('redacted');
    expect(screen.queryByLabelText('Key')).toBeNull();
  });

  it('every control states the disabled reason as title and aria-describedby', () => {
    withI18n(<KeyerPanel row={BASE_ROW} sources={SOURCES} disabledReason="Switcher unavailable" canvas={CANVAS} onPlan={vi.fn()} onApply={vi.fn()} />);
    const review = screen.getByRole('button', { name: 'Review & apply' });
    expect(review).toHaveProperty('disabled', true);
    expect(review).toHaveAttribute('title', 'Switcher unavailable');
    const describedBy = review.getAttribute('aria-describedby');
    expect(describedBy).toBeTruthy();
    expect(document.getElementById(describedBy!)?.textContent).toBe('Switcher unavailable');
    expect(screen.getByLabelText('Fill')).toHaveProperty('disabled', true);
  });

  it('plans then applies directly when the plan passes with no on-air protection', async () => {
    const onPlan = vi.fn().mockResolvedValue(PASS_PLAN);
    const onApply = vi.fn().mockResolvedValue(undefined);
    withI18n(<KeyerPanel row={BASE_ROW} sources={SOURCES} disabledReason={undefined} canvas={CANVAS} onPlan={onPlan} onApply={onApply} />);
    await userEvent.click(screen.getByRole('button', { name: 'Review & apply' }));
    expect(onPlan).toHaveBeenCalledTimes(1);
    expect(onApply).toHaveBeenCalledTimes(1);
    expect(onApply).toHaveBeenCalledWith(expect.anything(), false);
    expect(screen.queryByRole('alertdialog')).toBeNull();
  });

  it('shows the capacity refusal and cost delta without applying', async () => {
    const plan: KeyerPlan = { can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: false, cost: { delta: 1.5, fits: false }, refusal: { type: 'capacity', detail: 'DSK budget exceeded' } };
    const onPlan = vi.fn().mockResolvedValue(plan);
    const onApply = vi.fn();
    withI18n(<KeyerPanel row={BASE_ROW} sources={SOURCES} disabledReason={undefined} canvas={CANVAS} onPlan={onPlan} onApply={onApply} />);
    await userEvent.click(screen.getByRole('button', { name: 'Review & apply' }));
    expect(screen.getByTestId('keyer-refusal-dsk1').textContent).toBe('DSK budget exceeded');
    expect(screen.getByTestId('keyer-cost-dsk1').textContent).toContain('exceeds the budget');
    expect(onApply).not.toHaveBeenCalled();
  });

  it('on-air-protected refusal for a fill/key change asks to confirm allow before applying', async () => {
    const onAirRow = { ...BASE_ROW, on_air: true };
    const refusedPlan: KeyerPlan = { can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: true, refusal: { type: 'on-air-protected', detail: 'fill/key change needs on_air allow' } };
    const onPlan = vi.fn().mockResolvedValue(refusedPlan);
    const onApply = vi.fn().mockResolvedValue(undefined);
    withI18n(<KeyerPanel row={onAirRow} sources={SOURCES} disabledReason={undefined} canvas={CANVAS} onPlan={onPlan} onApply={onApply} />);
    await userEvent.selectOptions(screen.getByLabelText('Fill'), 'cam-1');
    await userEvent.click(screen.getByRole('button', { name: 'Review & apply' }));
    const confirm = await screen.findByRole('alertdialog', { name: 'Confirm on-air keyer change' });
    expect(onApply).not.toHaveBeenCalled();
    await userEvent.click(within(confirm).getByRole('button', { name: 'Allow on-air change' }));
    expect(onApply).toHaveBeenCalledWith(expect.anything(), true);
  });

  it('cancelling the on-air confirm applies nothing', async () => {
    const onAirRow = { ...BASE_ROW, on_air: true };
    const refusedPlan: KeyerPlan = { can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: true, refusal: { type: 'on-air-protected', detail: 'needs allow' } };
    const onPlan = vi.fn().mockResolvedValue(refusedPlan);
    const onApply = vi.fn();
    withI18n(<KeyerPanel row={onAirRow} sources={SOURCES} disabledReason={undefined} canvas={CANVAS} onPlan={onPlan} onApply={onApply} />);
    await userEvent.selectOptions(screen.getByLabelText('Fill'), 'cam-1');
    await userEvent.click(screen.getByRole('button', { name: 'Review & apply' }));
    const confirm = await screen.findByRole('alertdialog');
    await userEvent.click(within(confirm).getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('alertdialog')).toBeNull();
    expect(onApply).not.toHaveBeenCalled();
  });

  it('a fly preset other than full shows width/height box fields', async () => {
    withI18n(<KeyerPanel row={BASE_ROW} sources={SOURCES} disabledReason={undefined} canvas={CANVAS} onPlan={vi.fn()} onApply={vi.fn()} />);
    expect(screen.queryByLabelText('Width')).toBeNull();
    await userEvent.click(screen.getByRole('button', { name: 'TL' }));
    expect(screen.getByLabelText('Width')).toBeTruthy();
    expect(screen.getByLabelText('Height')).toBeTruthy();
  });

  it('shows luma clip/gain only for the luma type', async () => {
    withI18n(<KeyerPanel row={BASE_ROW} sources={SOURCES} disabledReason={undefined} canvas={CANVAS} onPlan={vi.fn()} onApply={vi.fn()} />);
    expect(screen.queryByLabelText('Clip')).toBeNull();
    await userEvent.selectOptions(screen.getByLabelText('Type'), 'luma');
    expect(screen.getByLabelText('Clip')).toBeTruthy();
    expect(screen.getByLabelText('Gain')).toBeTruthy();
  });
});
