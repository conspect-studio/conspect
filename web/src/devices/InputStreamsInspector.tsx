// A read-only inspector for an input's elementary-stream inventory.
//
// `GET /api/v1/inputs/{id}/streams` returns the cached `StreamInventoryDoc` for a
// configured input (a source). On a device-detail page the device's bindable
// candidates suggest input ids to inspect — picking one shows its elementary
// streams (codec / kind / language / stable id). It is the off-engine cached
// snapshot, so it is honestly empty until that input has been configured and
// probed. READ-ONLY: there is no PID-selection override (that PATCH does not
// exist) — this only reads the inventory.
import { useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';

import { useInputStreams } from '../api/input-streamsQueries';
import type { StreamDescriptor } from '../api/input-streamsQueries';
import { Badge } from '../components/ui/badge';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';

/** Render a descriptor's codec + key detail as a short, honest line. */
function streamSummary(stream: StreamDescriptor): string {
  const parts: string[] = [stream.codec];
  if (stream.detail.detail === 'video') {
    parts.push(`${String(stream.detail.params.width)}×${String(stream.detail.params.height)}`);
  } else if (stream.detail.detail === 'audio') {
    parts.push(
      `${String(stream.detail.params.channels)}ch ${String(stream.detail.params.sample_rate)} Hz`,
    );
  }
  return parts.join(' · ');
}

/** Props for {@link InputStreamsInspector}. */
export interface InputStreamsInspectorProps {
  /** Input ids to offer as suggestions (e.g. a device's bindable candidates). */
  readonly suggestions?: readonly string[];
}

/**
 * Inspect an input's elementary-stream inventory, read-only. Defaults to no
 * input selected (the query stays disabled until an id is chosen/entered).
 */
export function InputStreamsInspector({
  suggestions = [],
}: InputStreamsInspectorProps): JSX.Element {
  const { t } = useLingui();
  const [inputId, setInputId] = useState<string>('');
  const streams = useInputStreams(inputId === '' ? undefined : inputId);
  const inventory = streams.data;

  return (
    <div className="flex flex-col gap-3">
      <div className="flex flex-wrap items-end gap-2">
        <div className="grid gap-1.5">
          <Label htmlFor="input-streams-id">
            <Trans>Input id</Trans>
          </Label>
          <Input
            id="input-streams-id"
            className="w-64"
            value={inputId}
            placeholder={t`e.g. cam-north`}
            list="input-streams-suggestions"
            onChange={(e): void => {
              setInputId(e.target.value);
            }}
          />
          {suggestions.length > 0 ? (
            <datalist id="input-streams-suggestions">
              {suggestions.map((id) => (
                <option key={id} value={id} />
              ))}
            </datalist>
          ) : null}
        </div>
      </div>

      {inputId === '' ? (
        <p className="text-sm text-muted-foreground">
          <Trans>
            Enter a configured input id to inspect its elementary streams. The
            inventory is cached off-engine, so it appears once the input has been
            probed.
          </Trans>
        </p>
      ) : streams.isPending ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading the stream inventory…</Trans>
        </p>
      ) : streams.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load the stream inventory:</Trans> {streams.error.message}
        </p>
      ) : inventory === undefined || inventory.streams.length === 0 ? (
        <p className="text-sm text-muted-foreground">
          <Trans>
            No streams cached for this input yet — it appears once the input has
            been configured and probed.
          </Trans>
        </p>
      ) : (
        <ul className="flex flex-col gap-2">
          {inventory.streams.map((stream) => (
            <li
              key={`${stream.id.kind_scope}/${stream.id.key}`}
              className="flex flex-wrap items-center gap-2 rounded-md border p-2 text-sm"
            >
              <Badge variant="outline">{stream.kind}</Badge>
              <span className="text-xs text-muted-foreground">{streamSummary(stream)}</span>
              {stream.language !== undefined && stream.language !== null ? (
                <Badge variant="secondary">{stream.language}</Badge>
              ) : null}
              {stream.default ? (
                <Badge variant="secondary">
                  <Trans>default</Trans>
                </Badge>
              ) : null}
              <code className="ml-auto text-xs text-muted-foreground">{stream.id.key}</code>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
