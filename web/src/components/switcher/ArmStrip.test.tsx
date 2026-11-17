import { describe, expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { i18n } from '@lingui/core';
import { I18nProvider } from '@lingui/react';
import { messages } from '../../locales/en/messages';
import { ArmStrip } from './ArmStrip';
import type { ArmStripProps } from './ArmStrip';
import type { KeyerRow } from '../../api/keyers';

i18n.load('en', messages);
i18n.activate('en');

const USK: KeyerRow = {
  id: 'usk1', family: 'usk', armed: false, fill: 'cam-1', key: null, type: 'linear', clip: 0, gain: 1, invert: false,
  fly: null, mask: null, max_source: null, on_air: false, tie: false, ramp: null, rate: 15,
  loss: 'drop_to_off_air', loss_trigger: 'reconnecting', stale: false, pending: false,
};
const USK2: KeyerRow = { ...USK, id: 'usk2', on_air: true };

function strip(props: Partial<ArmStripProps> = {}) {
  const all: ArmStripProps = { armed: { background: true, keys: [] }, keys: [USK, USK2], disabledReason: undefined, onArm: vi.fn(), ...props };
  render(<I18nProvider i18n={i18n}><ArmStrip {...all} /></I18nProvider>);
  return all;
}

describe('ArmStrip', () => {
  it('lights BKGD and each KEY from the armed view only', () => {
    strip({ armed: { background: false, keys: ['usk2'] } });
    expect(screen.getByTestId('keyer-arm-background')).toHaveAttribute('aria-pressed', 'false');
    expect(screen.getByTestId('keyer-arm-usk1')).toHaveAttribute('aria-pressed', 'false');
    expect(screen.getByTestId('keyer-arm-usk2')).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByRole('button', { name: 'KEY 1 (usk1)' })).toBeTruthy();
    expect(screen.getByRole('button', { name: 'KEY 2 (usk2)' })).toBeTruthy();
  });

  it('shows an ON AIR lamp per KEY', () => {
    strip();
    expect(screen.getByTestId('keyer-arm-lamp-usk1')).toHaveAttribute('data-on-air', 'false');
    expect(screen.getByTestId('keyer-arm-lamp-usk2')).toHaveAttribute('data-on-air', 'true');
    expect(screen.getByRole('status', { name: 'KEY 2 on air' })).toHaveTextContent('ON AIR');
  });

  it('a toggle hands the next full set to onArm', async () => {
    const { onArm } = strip({ armed: { background: true, keys: ['usk1'] } });
    await userEvent.click(screen.getByTestId('keyer-arm-usk2'));
    expect(onArm).toHaveBeenLastCalledWith({ background: true, keys: ['usk1', 'usk2'] });
    await userEvent.click(screen.getByTestId('keyer-arm-usk1'));
    expect(onArm).toHaveBeenLastCalledWith({ background: true, keys: [] });
    await userEvent.click(screen.getByTestId('keyer-arm-background'));
    expect(onArm).toHaveBeenLastCalledWith({ background: false, keys: ['usk1'] });
  });

  it('the last lit toggle cannot clear the set (an empty arm is refused)', () => {
    strip({ armed: { background: true, keys: [] } });
    const bkgd = screen.getByTestId('keyer-arm-background');
    expect(bkgd).toHaveProperty('disabled', true);
    expect(bkgd).toHaveAttribute('title', 'At least one of BKGD or a key must stay armed');
    expect(document.getElementById(bkgd.getAttribute('aria-describedby') ?? '')).toHaveTextContent('At least one of BKGD');
    expect(screen.getByTestId('keyer-arm-usk1')).toHaveProperty('disabled', false);
  });

  it('a disabled reason (in flight) disables every toggle with title and aria-describedby', async () => {
    const { onArm } = strip({ armed: { background: true, keys: ['usk1'] }, disabledReason: 'A switcher request is in flight' });
    for (const id of ['keyer-arm-background', 'keyer-arm-usk1', 'keyer-arm-usk2']) {
      const button = screen.getByTestId(id);
      expect(button).toHaveProperty('disabled', true);
      expect(button).toHaveAttribute('title', 'A switcher request is in flight');
      const described = document.getElementById(button.getAttribute('aria-describedby') ?? '');
      expect(described).toHaveTextContent('A switcher request is in flight');
    }
    await userEvent.click(screen.getByTestId('keyer-arm-usk2'));
    expect(onArm).not.toHaveBeenCalled();
  });

  it('a key with hidden sources is shown with its lamp but not toggleable', () => {
    strip({ keys: [{ ...USK, fill: 'redacted' }, USK2] });
    const key = screen.getByTestId('keyer-arm-usk1');
    expect(key).toHaveProperty('disabled', true);
    expect(key).toHaveAttribute('title', "This key's sources are hidden from you, so you cannot arm it");
    expect(screen.getByTestId('keyer-arm-lamp-usk1')).toBeTruthy();
    expect(screen.getByTestId('keyer-arm-usk2')).toHaveProperty('disabled', false);
  });

  it('an armed key with hidden sources blocks the whole set (arm replaces it)', () => {
    strip({ armed: { background: true, keys: ['usk1'] }, keys: [{ ...USK, key: 'redacted' }, USK2] });
    expect(screen.getByTestId('keyer-arm-background')).toHaveProperty('disabled', true);
    expect(screen.getByTestId('keyer-arm-usk2')).toHaveAttribute('title', "An armed key's sources are hidden from you, so the arm set cannot be changed here");
  });

  it('shows only readable keys, at most four, and BKGD always', () => {
    const many = ['a', 'b', 'c', 'd', 'e'].map((id) => ({ ...USK, id }));
    strip({ keys: many, armed: { background: true, keys: [] } });
    expect(screen.getByTestId('keyer-arm-background')).toBeTruthy();
    expect(screen.queryByTestId('keyer-arm-e')).toBeNull();
    expect(screen.getAllByRole('button')).toHaveLength(5);
  });
});
