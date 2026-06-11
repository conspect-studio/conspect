// HealthBanner: renders active health warnings with severity (text + glyph),
// message, code, and the actionable remediation — and renders NOTHING when clean
// (no false alarm on a healthy host). The colour is never the sole signal (WCAG
// 1.4.1): the severity label text + the glyph carry the meaning.
import { describe, expect, it, vi } from 'vitest';
import { screen } from '@testing-library/react';

import { renderWithProviders } from '../test/render';
import { HealthBanner } from './HealthBanner';
import type * as useHealthModule from '../realtime/useHealth';
import type { HealthState, HealthWarning } from '../realtime/useHealth';

// Drive the banner off a stubbed hook so the test does not open a WebSocket.
const mockHealth = vi.hoisted(() => {
  const value: { current: HealthState } = {
    current: { warnings: [], status: 'open' },
  };
  return value;
});
vi.mock('../realtime/useHealth', async (importOriginal) => {
  const actual = await importOriginal<typeof useHealthModule>();
  return { ...actual, useHealth: (): HealthState => mockHealth.current };
});

function warning(over: Partial<HealthWarning> = {}): HealthWarning {
  return {
    code: 'gpu-present-no-vulkan-adapter',
    severity: 'warning',
    subsystem: 'compositor',
    message: 'GPU RTX 4060 detected but GPU compositing is UNAVAILABLE; fell back to CPU.',
    remediation: 'Set NVIDIA_DRIVER_CAPABILITIES to include `graphics` and install libvulkan1.',
    since: 1_700_000_000_000_000_000,
    active: true,
    ...over,
  };
}

describe('HealthBanner', () => {
  it('renders nothing when there are no active warnings', () => {
    mockHealth.current = { warnings: [], status: 'open' };
    const { container } = renderWithProviders(<HealthBanner />);
    expect(container).toBeEmptyDOMElement();
  });

  it('renders the message, code, and remediation when warned', () => {
    mockHealth.current = { warnings: [warning()], status: 'open' };
    renderWithProviders(<HealthBanner />);
    // The alert region exists and is announced.
    const alert = screen.getByRole('alert');
    expect(alert).toBeInTheDocument();
    // The message and the actionable remediation are visible.
    expect(screen.getByText(/GPU compositing is UNAVAILABLE/)).toBeInTheDocument();
    expect(screen.getByText(/NVIDIA_DRIVER_CAPABILITIES/)).toBeInTheDocument();
    expect(screen.getByText(/libvulkan1/)).toBeInTheDocument();
    // The stable code is shown (operator can search/reference it).
    expect(screen.getByText('gpu-present-no-vulkan-adapter')).toBeInTheDocument();
    // Severity is conveyed as TEXT (not colour alone): the label is present.
    expect(screen.getByText('Warning')).toBeInTheDocument();
  });

  it('renders a row per active warning, severity label per row', () => {
    mockHealth.current = {
      warnings: [
        warning(),
        warning({ code: 'another-code', severity: 'critical', message: 'Second issue.' }),
      ],
      status: 'open',
    };
    renderWithProviders(<HealthBanner />);
    expect(screen.getByText('Warning')).toBeInTheDocument();
    expect(screen.getByText('Critical')).toBeInTheDocument();
    expect(screen.getByText('Second issue.')).toBeInTheDocument();
  });
});
