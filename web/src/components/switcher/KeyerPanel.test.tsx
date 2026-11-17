import { describe, expect, it, vi } from 'vitest';
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { messages } from '../../locales/en/messages';
import { KeyerPanel } from './KeyerPanel';
import type { KeyerPanelProps } from './KeyerPanel';
import type { KeyerPlan, KeyerRow } from '../../api/keyers';
import { OperationApiError } from '../../api/operations';

i18n.load('en', messages);
i18n.activate('en');

const BASE_ROW: KeyerRow = {
  id: 'dsk1', family: 'dsk', fill: 'gfx-channel-1', key: null, type: 'linear', clip: 0, gain: 1, invert: false,
  fly: null, mask: null, max_source: null, on_air: false, tie: false, ramp: null, rate: 15,
  loss: 'drop_to_off_air', loss_trigger: 'reconnecting', stale: false, pending: false,
};
/** A non-default on-air luma + inverted key with geometry: what an unrelated edit must never reset. */
const LUMA_ROW: KeyerRow = {
  ...BASE_ROW, key: 'cam-1', type: 'luma', clip: 0.3, gain: 2.5, invert: true, on_air: true,
  fly: { x: 1440, y: 0, w: 480, h: 270 }, mask: { x: 10, y: 20, w: 300, h: 400 }, loss: 'hold_last_good',
};
const SOURCES = [{ id: 'gfx-channel-1', name: 'GFX 1' }, { id: 'cam-1', name: 'Cam 1' }];
const CANVAS = { w: 1920, h: 1080 };
const PASS_PLAN: KeyerPlan = { can_take: true, class: 'class1', keyer: 'main/dsk1', on_air: false, cost: { delta: 0.25, fits: true } };

function panel(props: Partial<KeyerPanelProps> = {}) {
  const all: KeyerPanelProps = {
    row: BASE_ROW, sources: SOURCES, disabledReason: undefined, canvas: CANVAS,
    onPlan: vi.fn().mockResolvedValue(PASS_PLAN), onApply: vi.fn().mockResolvedValue(undefined), ...props,
  };
  const view = render(<I18nProvider i18n={i18n}><KeyerPanel {...all} /></I18nProvider>);
  const rerender = (next: Partial<KeyerPanelProps>) => {
    view.rerender(<I18nProvider i18n={i18n}><KeyerPanel {...all} {...next} /></I18nProvider>);
  };
  return { ...all, rerender };
}
const review = () => screen.getByRole('button', { name: 'Review' });
const apply = () => screen.getByRole('button', { name: 'Apply' });

describe('KeyerPanel', () => {
  it('offers only readable sources for fill/key (no in:<n>, matching useSources())', () => {
    panel();
    const options = Array.from(screen.getByLabelText<HTMLSelectElement>('Fill').options).map((o) => o.value).filter((v) => v !== '');
    expect(options).toEqual(['gfx-channel-1', 'cam-1']);
  });

  it('shows redacted fill/key as text, never editable', () => {
    panel({ row: { ...BASE_ROW, fill: 'redacted', key: 'redacted' } });
    expect(screen.getByTestId('keyer-fill-redacted-dsk1').textContent).toBe('redacted');
    expect(screen.getByTestId('keyer-key-redacted-dsk1').textContent).toBe('redacted');
    expect(screen.queryByLabelText('Fill')).toBeNull();
  });

  it('shows the authoritative committed params, not invented defaults', () => {
    panel({ row: LUMA_ROW });
    expect(screen.getByLabelText<HTMLSelectElement>('Type').value).toBe('luma');
    expect(screen.getByLabelText<HTMLInputElement>('Clip').value).toBe('0.3');
    expect(screen.getByLabelText<HTMLInputElement>('Gain').value).toBe('2.5');
    expect(screen.getByRole('checkbox', { name: 'Invert matte' })).toHaveProperty('checked', true);
    expect(screen.getByLabelText<HTMLSelectElement>('Key').value).toBe('cam-1');
    expect(screen.getByLabelText<HTMLInputElement>('Fly X').value).toBe('1440');
    expect(screen.getByLabelText<HTMLInputElement>('Mask Width').value).toBe('300');
    expect(screen.getByLabelText<HTMLSelectElement>('Loss policy').value).toBe('hold_last_good');
  });

  it('K5-01: an unrelated edit on an on-air luma+invert key sends only that field', async () => {
    const p = panel({ row: LUMA_ROW });
    await userEvent.selectOptions(screen.getByLabelText('Loss policy'), 'drop_to_off_air');
    await userEvent.click(review());
    expect(p.onPlan).toHaveBeenCalledWith({ keyer: 'dsk1', loss: 'drop_to_off_air' });
    await userEvent.click(apply());
    expect(p.onApply).toHaveBeenCalledWith({ keyer: 'dsk1', loss: 'drop_to_off_air' }, false);
  });

  it('K5-01: untouched fields follow a new snapshot; edited fields stay', async () => {
    const p = panel({ row: BASE_ROW });
    await userEvent.click(screen.getByRole('checkbox', { name: 'Invert matte' }));
    p.rerender({ row: { ...BASE_ROW, type: 'luma', clip: 0.4, gain: 3 } });
    expect(screen.getByLabelText<HTMLSelectElement>('Type').value).toBe('luma');
    expect(screen.getByLabelText<HTMLInputElement>('Clip').value).toBe('0.4');
    expect(screen.getByRole('checkbox', { name: 'Invert matte' })).toHaveProperty('checked', true);
    await userEvent.click(review());
    expect(p.onPlan).toHaveBeenCalledWith({ keyer: 'dsk1', invert: true });
  });

  it('K5-02: external key → self key sends key "" (clear); an unchanged key is omitted', async () => {
    const p = panel({ row: { ...BASE_ROW, key: 'cam-1' } });
    await userEvent.click(screen.getByRole('checkbox', { name: 'Invert matte' }));
    await userEvent.click(review());
    expect(p.onPlan).toHaveBeenLastCalledWith({ keyer: 'dsk1', invert: true });
    await userEvent.selectOptions(screen.getByLabelText('Key'), '');
    await userEvent.click(review());
    expect(p.onPlan).toHaveBeenLastCalledWith({ keyer: 'dsk1', key: '', invert: true });
  });

  it('K5-02: a redacted key is never sent', async () => {
    const p = panel({ row: { ...BASE_ROW, key: 'redacted' } });
    await userEvent.click(screen.getByRole('checkbox', { name: 'Invert matte' }));
    await userEvent.click(review());
    expect(p.onPlan).toHaveBeenCalledWith({ keyer: 'dsk1', invert: true });
  });

  it('K5-03: disabling a mask sends mask null; "Full (clear mask)" clears it too', async () => {
    const p = panel({ row: LUMA_ROW });
    await userEvent.click(screen.getByRole('button', { name: 'Full (clear mask)' }));
    expect(screen.queryByLabelText('Mask X')).toBeNull();
    await userEvent.click(review());
    expect(p.onPlan).toHaveBeenCalledWith({ keyer: 'dsk1', mask: null });
  });

  it('K5-03: enabling a mask starts from the real source envelope, not 100×100', async () => {
    panel({ row: { ...BASE_ROW, max_source: { w: 1280, h: 720 } } });
    await userEvent.click(screen.getByRole('checkbox', { name: 'Enable mask' }));
    expect(screen.getByLabelText<HTMLInputElement>('Mask Width').value).toBe('1280');
    expect(screen.getByLabelText<HTMLInputElement>('Mask Height').value).toBe('720');
  });

  it('K5-04: corner → Full round trip sends an explicit fly null', async () => {
    const p = panel({ row: LUMA_ROW });
    expect(screen.getByRole('button', { name: 'Fly to the top right corner' })).toHaveAttribute('aria-pressed', 'true');
    await userEvent.click(screen.getByRole('button', { name: 'Full canvas (no fly)' }));
    expect(screen.queryByLabelText('Fly X')).toBeNull();
    await userEvent.click(review());
    expect(p.onPlan).toHaveBeenCalledWith({ keyer: 'dsk1', fly: null });
  });

  it('K5-04: presets use the API canvas (non-1080p) and x/y are editable', async () => {
    const p = panel({ canvas: { w: 1280, h: 720 } });
    await userEvent.click(screen.getByRole('button', { name: 'Fly to the bottom right corner' }));
    expect(screen.getByLabelText<HTMLInputElement>('Fly X').value).toBe('640');
    expect(screen.getByLabelText<HTMLInputElement>('Fly Y').value).toBe('360');
    await userEvent.clear(screen.getByLabelText('Fly Y'));
    await userEvent.type(screen.getByLabelText('Fly Y'), '300');
    await userEvent.click(review());
    expect(p.onPlan).toHaveBeenCalledWith({ keyer: 'dsk1', fly: { x: 640, y: 300, w: 640, h: 360 } });
  });

  it('K5-07: review shows class and cost; apply is separate and sends the reviewed draft', async () => {
    const p = panel();
    expect(apply()).toHaveProperty('disabled', true);
    await userEvent.click(screen.getByRole('checkbox', { name: 'Invert matte' }));
    await userEvent.click(review());
    expect(p.onApply).not.toHaveBeenCalled();
    expect(screen.getByTestId('keyer-plan-dsk1').textContent).toMatch(/Class 1.*0\.25.*fits the budget/);
    await userEvent.click(apply());
    expect(p.onApply).toHaveBeenCalledWith({ keyer: 'dsk1', invert: true }, false);
  });

  it('K5-07: an edit after review invalidates it (Apply disabled until re-reviewed)', async () => {
    const p = panel();
    await userEvent.click(screen.getByRole('checkbox', { name: 'Invert matte' }));
    await userEvent.click(review());
    expect(apply()).toHaveProperty('disabled', false);
    await userEvent.selectOptions(screen.getByLabelText('Type'), 'luma');
    expect(apply()).toHaveProperty('disabled', true);
    expect(apply()).toHaveAttribute('title', 'Review this change first');
    await userEvent.click(apply());
    expect(p.onApply).not.toHaveBeenCalled();
  });

  it('K5-R2-01: an invalid edit after review makes Apply unavailable until corrected and re-reviewed', async () => {
    const p = panel({ row: LUMA_ROW });
    await userEvent.click(screen.getByRole('checkbox', { name: 'Invert matte' }));
    await userEvent.click(review());
    expect(apply()).toHaveProperty('disabled', false);
    // The invalid clip is omitted from the fields, so the JSON equals the
    // reviewed draft; Apply must still be unavailable.
    await userEvent.clear(screen.getByLabelText('Clip'));
    expect(apply()).toHaveProperty('disabled', true);
    await userEvent.click(apply());
    expect(p.onApply).not.toHaveBeenCalled();
    await userEvent.type(screen.getByLabelText('Clip'), '0.3');
    expect(apply()).toHaveProperty('disabled', true);
    await userEvent.click(review());
    await userEvent.click(apply());
    expect(p.onApply).toHaveBeenCalledWith({ keyer: 'dsk1', invert: false }, false);
  });

  it('K5-07: a plan refusal shows the detail and Apply stays disabled', async () => {
    const plan: KeyerPlan = { can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: false, cost: { delta: 1.5, fits: false }, refusal: { type: 'capacity', detail: 'DSK budget exceeded' } };
    const p = panel({ onPlan: vi.fn().mockResolvedValue(plan) });
    await userEvent.click(screen.getByRole('checkbox', { name: 'Invert matte' }));
    await userEvent.click(review());
    expect(screen.getByTestId('keyer-refusal-dsk1').textContent).toBe('DSK budget exceeded');
    expect(screen.getByTestId('keyer-plan-dsk1').textContent).toContain('exceeds the budget');
    expect(apply()).toHaveProperty('disabled', true);
    expect(p.onApply).not.toHaveBeenCalled();
  });

  it('K5-07: plan passes, command answers 409 on-air-protected → fresh plan + explicit allow', async () => {
    const protectedPlan: KeyerPlan = { can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: true, refusal: { type: 'on-air-protected', detail: 'needs allow' } };
    const onPlan = vi.fn().mockResolvedValueOnce(PASS_PLAN).mockResolvedValueOnce(protectedPlan);
    const onApply = vi.fn()
      .mockRejectedValueOnce(new OperationApiError('Keyer request refused', 409, 'on air', '/problems/on-air-protected'))
      .mockResolvedValueOnce(undefined);
    panel({ onPlan, onApply });
    await userEvent.selectOptions(screen.getByLabelText('Fill'), 'cam-1');
    await userEvent.click(review());
    await userEvent.click(apply());
    const confirm = await screen.findByRole('alertdialog', { name: 'Confirm on-air keyer change' });
    expect(onPlan).toHaveBeenCalledTimes(2);
    expect(onApply).toHaveBeenCalledTimes(1);
    await userEvent.click(within(confirm).getByRole('button', { name: 'Allow on-air change' }));
    expect(onApply).toHaveBeenLastCalledWith({ keyer: 'dsk1', fill: 'cam-1' }, true);
  });

  it('cancelling the on-air confirm applies nothing', async () => {
    const refused: KeyerPlan = { can_take: false, class: 'class1', keyer: 'main/dsk1', on_air: true, refusal: { type: 'on-air-protected', detail: 'needs allow' } };
    const p = panel({ row: { ...BASE_ROW, on_air: true }, onPlan: vi.fn().mockResolvedValue(refused) });
    await userEvent.selectOptions(screen.getByLabelText('Fill'), 'cam-1');
    await userEvent.click(review());
    const confirm = await screen.findByRole('alertdialog');
    await userEvent.click(within(confirm).getByRole('button', { name: 'Cancel' }));
    expect(screen.queryByRole('alertdialog')).toBeNull();
    expect(p.onApply).not.toHaveBeenCalled();
  });

  it('an invalid mask field blocks review with a stated reason', async () => {
    const p = panel();
    await userEvent.click(screen.getByRole('checkbox', { name: 'Enable mask' }));
    await userEvent.clear(screen.getByLabelText('Mask Width'));
    await userEvent.type(screen.getByLabelText('Mask Width'), '-5');
    expect(screen.getByTestId('keyer-invalid-dsk1')).toHaveTextContent(/Mask x\/y/);
    expect(review()).toHaveProperty('disabled', true);
    expect(review().getAttribute('title')).toMatch(/Mask x\/y/);
    await userEvent.click(review());
    expect(p.onPlan).not.toHaveBeenCalled();
  });

  it('K5-09: every disabled control carries the reason as title and aria-describedby', () => {
    panel({ row: LUMA_ROW, disabledReason: 'Switcher unavailable' });
    const section = screen.getByTestId('keyer-panel-dsk1');
    const controls = section.querySelectorAll<HTMLElement>('button, input, select');
    expect(controls.length).toBeGreaterThan(15);
    for (const control of controls) {
      if (control.textContent === 'Cancel') continue;
      expect(control).toHaveProperty('disabled', true);
      expect(control.getAttribute('title')).toBe('Switcher unavailable');
      const describedBy = control.getAttribute('aria-describedby');
      expect(describedBy, control.outerHTML).toBeTruthy();
      expect(document.getElementById(describedBy ?? '')?.textContent).toBe('Switcher unavailable');
    }
  });
});
