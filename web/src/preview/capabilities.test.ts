// Unit tests for the capabilities helpers (ADR-W023 §2) — the pure
// transport-selection predicate that drives the WHEP→JPEG ladder.
import { describe, expect, it } from 'vitest';

import { programFidelity, whepAvailable } from './capabilities';
import type { PreviewCapabilities } from './capabilities';

function caps(over: Partial<PreviewCapabilities> = {}): PreviewCapabilities {
  return {
    webrtc: true,
    fallback: 'jpeg',
    scopes: {
      program: { whep: true, fidelity: 'real-encoded-output' },
      inputs: { whep: true },
      outputs: { whep: true },
    },
    ...over,
  };
}

describe('whepAvailable', () => {
  it('is false for an undefined document (probe not yet loaded / failed)', () => {
    expect(whepAvailable(undefined, 'program')).toBe(false);
  });

  it('is false on a non-webrtc build regardless of scope flags', () => {
    expect(whepAvailable(caps({ webrtc: false }), 'program')).toBe(false);
    expect(whepAvailable(caps({ webrtc: false }), 'input')).toBe(false);
    expect(whepAvailable(caps({ webrtc: false }), 'output')).toBe(false);
  });

  it('reflects the per-scope whep flag on a webrtc build', () => {
    const document = caps({
      scopes: {
        program: { whep: true, fidelity: 'pre-encode-canvas-approx' },
        inputs: { whep: false },
        outputs: { whep: true },
      },
    });
    expect(whepAvailable(document, 'program')).toBe(true);
    expect(whepAvailable(document, 'input')).toBe(false);
    expect(whepAvailable(document, 'output')).toBe(true);
  });
});

describe('programFidelity', () => {
  it('returns the advertised program fidelity label', () => {
    expect(programFidelity(caps())).toBe('real-encoded-output');
  });

  it('is undefined when absent or the document is missing', () => {
    expect(programFidelity(undefined)).toBeUndefined();
    expect(
      programFidelity(
        caps({
          scopes: {
            program: { whep: true, fidelity: null },
            inputs: { whep: true },
            outputs: { whep: true },
          },
        }),
      ),
    ).toBeUndefined();
  });
});
