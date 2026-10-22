// "From a managed device" — the device-projection sections in the Sources and
// Outputs create dialogs (ADR-M009).
//
// Picking an enumerated stream/decode-slot PREFILLS the ordinary transport
// form and stamps `device_ref` into the body's preserved extras: the result
// is a normal managed Source/Output bound to the device — the engine's
// ingest/serve paths are untouched. The binding is visible and clearable
// before saving. Unverified candidates (vendor-undocumented mounts) are
// labelled and never silently guessed.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Cable, X } from 'lucide-react';

import { useDevices, useOutputTargets, useSourceCandidates } from './queries';
import type { DeviceView } from './types';
import { withSourceKind } from '../resources/forms';
import type { OutputFormState, SourceFormState } from '../resources/forms';
import type { SourceFormKind } from '../resources/forms';
import type { OutputKind } from '../resources/types';
import { SelectField } from '../resources/FormControls';
import { Badge } from '../components/ui/badge';
import { Button } from '../components/ui/button';

/** Strip the device binding from a form's preserved extras. */
function withoutDeviceRef(
  extra: Readonly<Record<string, unknown>>,
): Readonly<Record<string, unknown>> {
  return Object.fromEntries(
    Object.entries(extra).filter(([key]) => key !== 'device_ref'),
  );
}

/** The visible, clearable binding chip shown once a device stream is picked. */
function BindingChip({
  deviceRef,
  onClear,
}: {
  readonly deviceRef: string;
  readonly onClear: () => void;
}): JSX.Element {
  const { t } = useLingui();
  return (
    <p className="flex flex-wrap items-center gap-2 text-sm">
      <Badge variant="outline">
        <Cable className="size-3.5" aria-hidden="true" />
        <span>
          <Trans>Bound to device</Trans>
        </span>
      </Badge>
      <code className="text-xs">{deviceRef}</code>
      <Button
        type="button"
        variant="ghost"
        size="sm"
        aria-label={t`Clear device binding`}
        onClick={onClear}
      >
        <X aria-hidden="true" />
        <Trans>Clear</Trans>
      </Button>
    </p>
  );
}

/** Shared section frame + device picker. */
function SectionFrame({
  devices,
  selected,
  onSelect,
  children,
}: {
  readonly devices: readonly DeviceView[];
  readonly selected: string;
  readonly onSelect: (id: string) => void;
  readonly children: JSX.Element;
}): JSX.Element {
  const { t } = useLingui();
  return (
    <fieldset className="flex flex-col gap-2 rounded-md border p-3">
      <legend className="px-1 text-sm font-medium">
        <Trans>From a managed device</Trans>
      </legend>
      <SelectField<string>
        label={t`Device`}
        value={selected}
        options={devices.map((device) => device.id)}
        optionLabel={(id): string => {
          const device = devices.find((d) => d.id === id);
          return device !== undefined ? `${device.name} (${device.id})` : id;
        }}
        testId="from-device-select"
        onChange={onSelect}
      />
      {children}
    </fieldset>
  );
}

/** The Sources create-dialog section: enumerated streams → prefilled form. */
export function SourceFromDeviceSection({
  form,
  setForm,
}: {
  readonly form: SourceFormState;
  readonly setForm: (next: SourceFormState) => void;
}): JSX.Element | null {
  const { t } = useLingui();
  const devices = useDevices();
  const [picked, setPicked] = useState<string | undefined>(undefined);
  const list = devices.data ?? [];
  // With exactly one adopted device there is nothing to choose: use it.
  const selected = picked ?? (list.length === 1 ? list.at(0)?.id : undefined);
  const candidates = useSourceCandidates(selected);

  const deviceRef = form.extra.device_ref;
  const chip =
    typeof deviceRef === 'string' ? (
      <BindingChip
        deviceRef={deviceRef}
        onClear={(): void => {
          setForm({ ...form, extra: withoutDeviceRef(form.extra) });
        }}
      />
    ) : null;

  if (list.length === 0) {
    // No adopted devices: nothing to offer (the chip still shows on edit).
    return chip;
  }

  return (
    <>
      <SectionFrame
        devices={list}
        selected={selected ?? ''}
        onSelect={setPicked}
      >
        <>
          {selected === undefined ? (
            <p className="text-xs text-muted-foreground">
              <Trans>Pick a device to list the streams it serves.</Trans>
            </p>
          ) : (candidates.data ?? []).length === 0 ? (
            <p className="text-xs text-muted-foreground">
              <Trans>
                Nothing enumerated yet — streams appear once the device's
                driver has probed it.
              </Trans>
            </p>
          ) : (
            <ul className="flex flex-col gap-1">
              {(candidates.data ?? []).map((candidate) => (
                <li key={candidate.id} className="flex flex-wrap items-center gap-2">
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    aria-label={`${t`Use stream`}: ${candidate.id}`}
                    onClick={(): void => {
                      const kind: SourceFormKind =
                        candidate.kind === 'srt'
                          ? 'srt'
                          : candidate.kind === 'ndi'
                            ? 'ndi'
                            : 'rtsp';
                      const base = withSourceKind(form, kind);
                      setForm({
                        ...base,
                        url: kind === 'ndi' ? base.url : (candidate.url ?? ''),
                        ndiName: kind === 'ndi' ? (candidate.url ?? '') : base.ndiName,
                        extra: { ...base.extra, device_ref: selected },
                      });
                    }}
                  >
                    {candidate.id}
                    <Badge variant="outline">{candidate.kind}</Badge>
                  </Button>
                  {candidate.unverified ? (
                    <span className="text-xs text-muted-foreground">
                      <Trans>unverified — supply the URL yourself</Trans>
                    </span>
                  ) : null}
                </li>
              ))}
            </ul>
          )}
        </>
      </SectionFrame>
      {chip}
    </>
  );
}

/** The Outputs create-dialog section: decode targets → device_ref binding. */
export function OutputFromDeviceSection({
  form,
  setForm,
}: {
  readonly form: OutputFormState;
  readonly setForm: (next: OutputFormState) => void;
}): JSX.Element | null {
  const { t } = useLingui();
  const devices = useDevices();
  const [picked, setPicked] = useState<string | undefined>(undefined);
  const list = devices.data ?? [];
  const selected = picked ?? (list.length === 1 ? list.at(0)?.id : undefined);
  const targets = useOutputTargets(selected);

  const deviceRef = form.extra.device_ref;
  const chip =
    typeof deviceRef === 'string' ? (
      <BindingChip
        deviceRef={deviceRef}
        onClear={(): void => {
          setForm({ ...form, extra: withoutDeviceRef(form.extra) });
        }}
      />
    ) : null;

  if (list.length === 0) {
    return chip;
  }

  return (
    <>
      <SectionFrame devices={list} selected={selected ?? ''} onSelect={setPicked}>
        <>
          {selected === undefined ? (
            <p className="text-xs text-muted-foreground">
              <Trans>Pick a device to list its decode slots.</Trans>
            </p>
          ) : (targets.data ?? []).length === 0 ? (
            <p className="text-xs text-muted-foreground">
              <Trans>
                Nothing enumerated yet — decode slots appear once the driver
                reads the device's decode table.
              </Trans>
            </p>
          ) : (
            <ul className="flex flex-col gap-1">
              {(targets.data ?? []).map((target) => {
                const label = target.label ?? target.id;
                return (
                  <li key={target.id}>
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      aria-label={`${t`Use decode target`}: ${label}`}
                      onClick={(): void => {
                        const kind: OutputKind =
                          target.kind === 'srt'
                            ? 'srt'
                            : target.kind === 'rtmp'
                              ? 'rtmp'
                              : target.kind === 'ndi'
                                ? 'ndi'
                                : 'rtsp';
                        setForm({
                          ...form,
                          kind,
                          extra: { ...form.extra, device_ref: selected },
                        });
                      }}
                    >
                      {label}
                      <Badge variant="outline">{target.kind}</Badge>
                    </Button>
                  </li>
                );
              })}
            </ul>
          )}
        </>
      </SectionFrame>
      {chip}
    </>
  );
}
