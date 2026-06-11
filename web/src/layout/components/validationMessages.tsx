// Maps a machine ValidationCode to a localized, human-readable message.
//
// The codes are closed unions (see model.ts / gridModel.ts), so these switches
// are exhaustive and the compiler flags a missing case if a code is added.
import type { JSX } from 'react';
import { Trans } from '@lingui/react/macro';

import type { ValidationCode } from '../model';
import type { GridValidationCode } from '../gridModel';

/** A localized message element for a validation code. */
export function ValidationMessage({
  code,
}: {
  readonly code: ValidationCode;
}): JSX.Element {
  switch (code) {
    case 'name-empty':
      return <Trans>A layout name is required.</Trans>;
    case 'canvas-dim':
      return <Trans>Canvas width and height must be positive whole numbers.</Trans>;
    case 'fps-format':
      return (
        <Trans>
          Frame rate must be an exact rational like 30000/1001 — not a decimal.
        </Trans>
      );
    case 'cell-id-empty':
      return <Trans>Every cell needs a non-empty identifier.</Trans>;
    case 'cell-id-duplicate':
      return <Trans>Cell identifiers must be unique within a layout.</Trans>;
    case 'rect-bounds':
      return <Trans>The cell extends outside the canvas.</Trans>;
    case 'rect-extent':
      return <Trans>The cell is too small.</Trans>;
    case 'rotation-range':
      return <Trans>Rotation must be between 0 and 359 degrees.</Trans>;
    case 'no-cells':
      return <Trans>Add at least one cell to the layout.</Trans>;
    case 'opacity-range':
      return <Trans>Opacity must be between 0 and 1.</Trans>;
    case 'corner-radius-invalid':
      return <Trans>Corner radius must be a whole number of pixels (0 or more).</Trans>;
    case 'border-width-invalid':
      return <Trans>Border width must be a whole number of pixels (0 or more).</Trans>;
    case 'border-color-hex':
      return <Trans>Border colour must be a hex value like #101014 or #abc.</Trans>;
    case 'qos-priority-int':
      return <Trans>Priority must be a whole number (higher is shed last).</Trans>;
  }
}

/** A localized message element for a grid validation code. */
export function GridValidationMessage({
  code,
}: {
  readonly code: GridValidationCode;
}): JSX.Element {
  switch (code) {
    case 'tracks-empty':
      return <Trans>Each axis needs at least one track (e.g. 1fr).</Trans>;
    case 'track-format':
      return (
        <Trans>Tracks must be a number with a unit: 1fr, 200px or 25%.</Trans>
      );
    case 'gap-invalid':
      return <Trans>Gaps must be whole pixel counts (0 or more).</Trans>;
    case 'area-not-rectangle':
      return <Trans>Every named area must form a contiguous rectangle.</Trans>;
    case 'grid-overflow':
      return (
        <Trans>
          The tracks overflow the canvas (fixed sizes or percentages exceed it)
          — an area would land outside the output frame.
        </Trans>
      );
    case 'areas-normalized':
      return (
        <Trans>
          The stored area map was ragged and has been normalized for editing;
          saving will persist the repaired map.
        </Trans>
      );
    case 'cell-area-and-rect':
      return (
        <Trans>
          This cell declares both an area and a rect — keep exactly one (the
          engine rejects the pair).
        </Trans>
      );
    case 'cell-area-unknown':
      return <Trans>This cell references an area that is not in the grid.</Trans>;
    case 'area-no-cell':
      return <Trans>This area has no cell, so nothing will render in it.</Trans>;
    default:
      return <ValidationMessage code={code} />;
  }
}
