// The shared per-cell properties panel — ONE implementation of the full config
// `Cell` property surface (failover slate, appearance, degradation), mounted by
// BOTH editors: the absolute editor's CellsForm rows and the grid editor's
// per-area panel. All edits go through the pure `CellProperties` model
// (./cellProps), so unrendered fields are never lost.
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';

import {
  DEGRADATION_MODES,
  FAILOVER_SLATES,
  onLossOf,
  SCALER_MODES,
} from '../cellProps';
import type {
  BorderModel,
  CellProperties,
  FailoverSlate,
  QosModel,
} from '../cellProps';
import { AdvancedSection, FormField, SelectField } from '../../resources/FormControls';

/** The sentinel option for "key absent — engine default applies". */
const DEFAULT_OPTION = '__default__';

/** Props for {@link CellPropertiesPanel}. */
export interface CellPropertiesPanelProps {
  /** A unique id prefix for this mount (e.g. the cell id). */
  readonly idPrefix: string;
  /** The current properties. */
  readonly value: CellProperties;
  /** Called with the full next properties on every edit. */
  readonly onChange: (next: CellProperties) => void;
}

/** Parse a number field, mapping an empty/invalid entry to "key absent". */
function numberOrAbsent(raw: string): number | undefined {
  if (raw.trim() === '') {
    return undefined;
  }
  const parsed = Number(raw);
  return Number.isFinite(parsed) ? parsed : undefined;
}

/** Render a number back to its field string ('' when the key is absent). */
function fieldOf(value: number | undefined): string {
  return value === undefined ? '' : String(value);
}

const EMPTY_BORDER: BorderModel = {
  widthPx: undefined,
  color: undefined,
  style: undefined,
  extra: {},
};

const EMPTY_QOS: QosModel = {
  priority: undefined,
  degradation: undefined,
  extra: {},
};

function isEmptyBorder(border: BorderModel): boolean {
  return (
    border.widthPx === undefined &&
    border.color === undefined &&
    border.style === undefined &&
    Object.keys(border.extra).length === 0
  );
}

function isEmptyQos(qos: QosModel): boolean {
  return (
    qos.priority === undefined &&
    qos.degradation === undefined &&
    Object.keys(qos.extra).length === 0
  );
}

/** The tri-state options for an optional boolean (absent / true / false). */
type TriState = typeof DEFAULT_OPTION | 'on' | 'off';

function triOf(value: boolean | undefined): TriState {
  if (value === undefined) {
    return DEFAULT_OPTION;
  }
  return value ? 'on' : 'off';
}

function triToBool(value: TriState): boolean | undefined {
  if (value === DEFAULT_OPTION) {
    return undefined;
  }
  return value === 'on';
}

/** The full Cell property editor (on_loss + appearance + degradation). */
export function CellPropertiesPanel({
  idPrefix,
  value,
  onChange,
}: CellPropertiesPanelProps): JSX.Element {
  const { t } = useLingui();

  const slateValue = value.onLoss === undefined ? DEFAULT_OPTION : value.onLoss.slate;
  const knownSlate = FAILOVER_SLATES.find((slate) => slate === slateValue);
  // An unknown future slate is shown (and preserved); picking it back is a no-op.
  const slateOptions: readonly string[] = [
    DEFAULT_OPTION,
    ...FAILOVER_SLATES,
    ...(slateValue !== DEFAULT_OPTION && knownSlate === undefined ? [slateValue] : []),
  ];
  const slateLabel = (option: string): string => {
    switch (option) {
      case DEFAULT_OPTION:
        return t`Default (colour bars)`;
      case 'bars':
        return t`Colour bars (line-up signal)`;
      case 'no_signal':
        return t`"No signal" card`;
      case 'black':
        return t`Black`;
      default:
        return t`${option} (custom — preserved)`;
    }
  };

  // Unknown stored tokens (a future scaler/strategy) are SHOWN and preserved
  // — never displayed as "Default" and never silently replaced (the same
  // custom-option pattern as the failover slate above).
  const scalerValue =
    value.scaler === undefined || value.scaler === '' ? DEFAULT_OPTION : value.scaler;
  const scalerOptions: readonly string[] = [
    DEFAULT_OPTION,
    ...SCALER_MODES,
    ...(scalerValue !== DEFAULT_OPTION && !SCALER_MODES.some((m) => m === scalerValue)
      ? [scalerValue]
      : []),
  ];
  const storedDegradation = value.qos?.degradation;
  const degradationValue =
    storedDegradation === undefined || storedDegradation === ''
      ? DEFAULT_OPTION
      : storedDegradation;
  const degradationOptions: readonly string[] = [
    DEFAULT_OPTION,
    ...DEGRADATION_MODES,
    ...(degradationValue !== DEFAULT_OPTION &&
    !DEGRADATION_MODES.some((m) => m === degradationValue)
      ? [degradationValue]
      : []),
  ];

  const border = value.border ?? EMPTY_BORDER;
  const setBorder = (next: BorderModel): void => {
    onChange({ ...value, border: isEmptyBorder(next) ? undefined : next });
  };
  const qos = value.qos ?? EMPTY_QOS;
  const setQos = (next: QosModel): void => {
    onChange({ ...value, qos: isEmptyQos(next) ? undefined : next });
  };

  return (
    <div className="flex flex-col gap-3" data-testid={`cell-props-${idPrefix}`}>
      <SelectField
        label={t`On signal loss`}
        value={slateValue}
        options={slateOptions}
        optionLabel={slateLabel}
        onChange={(next): void => {
          if (next === DEFAULT_OPTION) {
            onChange({ ...value, onLoss: undefined });
            return;
          }
          const slate = FAILOVER_SLATES.find((s): s is FailoverSlate => s === next);
          if (slate !== undefined) {
            onChange({ ...value, onLoss: onLossOf(slate) });
          }
        }}
      />

      <AdvancedSection summary={t`Appearance`}>
        <div className="grid grid-cols-2 gap-3">
          <FormField
            id={`${idPrefix}-opacity`}
            label={t`Opacity (0–1)`}
            type="number"
            value={fieldOf(value.opacity)}
            onChange={(raw): void => {
              onChange({ ...value, opacity: numberOrAbsent(raw) });
            }}
          />
          <FormField
            id={`${idPrefix}-corner-radius`}
            label={t`Corner radius (px)`}
            type="number"
            value={fieldOf(value.cornerRadius)}
            onChange={(raw): void => {
              onChange({ ...value, cornerRadius: numberOrAbsent(raw) });
            }}
          />
          <FormField
            id={`${idPrefix}-align`}
            label={t`Align`}
            value={value.align ?? ''}
            placeholder={t`e.g. center or top_left`}
            onChange={(raw): void => {
              onChange({ ...value, align: raw.trim() === '' ? undefined : raw });
            }}
          />
          <SelectField
            label={t`Scaler`}
            value={scalerValue}
            options={scalerOptions}
            optionLabel={(option): string => {
              if (option === DEFAULT_OPTION) {
                return t`Default (auto)`;
              }
              return SCALER_MODES.some((mode) => mode === option)
                ? option
                : t`${option} (custom — preserved)`;
            }}
            onChange={(next): void => {
              onChange({
                ...value,
                scaler: next === DEFAULT_OPTION ? undefined : next,
              });
            }}
          />
          <SelectField
            label={t`Visibility`}
            value={triOf(value.visible)}
            options={[DEFAULT_OPTION, 'on', 'off'] as const}
            optionLabel={(option): string => {
              if (option === DEFAULT_OPTION) {
                return t`Default (visible)`;
              }
              return option === 'on' ? t`Visible` : t`Hidden (decode-skip)`;
            }}
            onChange={(next): void => {
              onChange({ ...value, visible: triToBool(next) });
            }}
          />
          <SelectField
            label={t`Static source hint`}
            value={triOf(value.staticFriendly)}
            options={[DEFAULT_OPTION, 'on', 'off'] as const}
            optionLabel={(option): string => {
              if (option === DEFAULT_OPTION) {
                return t`Default (off)`;
              }
              return option === 'on' ? t`Mostly static` : t`Not static`;
            }}
            onChange={(next): void => {
              onChange({ ...value, staticFriendly: triToBool(next) });
            }}
          />
        </div>
        <fieldset className="flex flex-col gap-3">
          <legend className="text-xs font-medium">
            <Trans>Border</Trans>
          </legend>
          <div className="grid grid-cols-3 gap-3">
            <FormField
              id={`${idPrefix}-border-width`}
              label={t`Width (px)`}
              type="number"
              value={fieldOf(border.widthPx)}
              onChange={(raw): void => {
                setBorder({ ...border, widthPx: numberOrAbsent(raw) });
              }}
            />
            <FormField
              id={`${idPrefix}-border-color`}
              label={t`Colour (hex)`}
              value={border.color ?? ''}
              placeholder="#101014"
              onChange={(raw): void => {
                setBorder({ ...border, color: raw.trim() === '' ? undefined : raw });
              }}
            />
            <FormField
              id={`${idPrefix}-border-style`}
              label={t`Style`}
              value={border.style ?? ''}
              placeholder={t`e.g. solid`}
              onChange={(raw): void => {
                setBorder({ ...border, style: raw.trim() === '' ? undefined : raw });
              }}
            />
          </div>
        </fieldset>
      </AdvancedSection>

      <AdvancedSection summary={t`Degradation`}>
        <div className="grid grid-cols-2 gap-3">
          <FormField
            id={`${idPrefix}-qos-priority`}
            label={t`Priority`}
            type="number"
            hint={<Trans>Higher priority is shed last under load.</Trans>}
            value={fieldOf(qos.priority)}
            onChange={(raw): void => {
              setQos({ ...qos, priority: numberOrAbsent(raw) });
            }}
          />
          <SelectField
            label={t`Strategy`}
            value={degradationValue}
            options={degradationOptions}
            optionLabel={(option): string => {
              if (option === DEFAULT_OPTION) {
                return t`Default (balanced)`;
              }
              return DEGRADATION_MODES.some((mode) => mode === option)
                ? option
                : t`${option} (custom — preserved)`;
            }}
            onChange={(next): void => {
              setQos({
                ...qos,
                degradation: next === DEFAULT_OPTION ? undefined : next,
              });
            }}
          />
        </div>
      </AdvancedSection>
    </div>
  );
}
