// Routing — classify and apply a crosspoint take.
//
// An operator routes one source elementary stream onto a destination. The form
// builds a `RouteTakeRequest` (source input + stream selector → a kind-specific
// target). "Plan" is a dry run that classifies the take (Class-1 hot vs
// reset-lite vs Class-2 migration) WITHOUT applying it (invariant #11), shown in
// a banner. "Take" applies it: a hot take resolves immediately; a Class-2
// migration is accepted with a 202 operation id whose outcome lands on the
// realtime stream. The take carries a fresh Idempotency-Key per attempt.
import { useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { Play, Route as RouteIcon } from 'lucide-react';

import { usePlanRoute, useTakeRoute } from '../api/routingQueries';
import type {
  RouteClass,
  RouteKind,
  RoutePlan,
  RouteTakeRequest,
  RouteTarget,
  StreamRef,
} from '../api/routingQueries';
import { ROUTE_KINDS } from '../api/routingQueries';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import type { BadgeProps } from '../components/ui/badge';
import { Button } from '../components/ui/button';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '../components/ui/select';
import { toast } from '../components/ui/use-toast';

/** A stream selector strategy (mirrors `StreamSelectorDoc.by`). */
type SelectorBy = 'best' | 'index' | 'language' | 'stream_id';

const SELECTOR_BYS: readonly SelectorBy[] = ['best', 'index', 'language', 'stream_id'];

/** Narrow a Select string value to a known take kind (falls back to `video`). */
function asRouteKind(value: string): RouteKind {
  return ROUTE_KINDS.find((kind) => kind === value) ?? 'video';
}

/** Narrow a Select string value to a known selector strategy (falls back to `best`). */
function asSelectorBy(value: string): SelectorBy {
  return SELECTOR_BYS.find((by) => by === value) ?? 'best';
}

/** The local form state the page edits before composing the request. */
interface RouteForm {
  readonly kind: RouteKind;
  readonly inputId: string;
  readonly selectorBy: SelectorBy;
  readonly selectorValue: string;
  /** The destination identifier (cell / channel / track / layer, per kind). */
  readonly target: string;
  /** Discrete-track pinned channel count (audio_discrete_track only). */
  readonly pinnedChannels: string;
  /** Operator-confirmed down/up-mix to a pinned layout. */
  readonly coerce: boolean;
}

const EMPTY_FORM: RouteForm = {
  kind: 'video',
  inputId: '',
  selectorBy: 'best',
  selectorValue: '',
  target: '',
  pinnedChannels: '',
  coerce: false,
};

/** A stream selector, narrowed to the non-undefined variants of the wire type. */
type StreamSelector = NonNullable<StreamRef['selector']>;

/** Compose the source stream reference from the form. */
function buildSource(form: RouteForm): StreamRef {
  const selector = ((): StreamSelector => {
    switch (form.selectorBy) {
      case 'index': {
        const index = Number.parseInt(form.selectorValue, 10);
        return Number.isFinite(index) ? { by: 'index', index } : { by: 'best' };
      }
      case 'language':
        return { by: 'language', language: form.selectorValue.trim() };
      case 'stream_id':
        return { by: 'stream_id', id: form.selectorValue.trim() };
      case 'best':
        return { by: 'best' };
    }
  })();
  return {
    input_id: form.inputId.trim(),
    kind: { kind: form.kind },
    selector,
  };
}

/** Compose the kind-specific destination target from the form. */
function buildTarget(form: RouteForm): RouteTarget {
  const id = form.target.trim();
  switch (form.kind) {
    case 'video':
      return { kind: 'video_cell', cell: id };
    case 'subtitle':
      return { kind: 'subtitle_layer', layer: id };
    case 'audio': {
      const pinned = Number.parseInt(form.pinnedChannels, 10);
      // A discrete-track destination is chosen when a channel count is given;
      // otherwise the audio routes onto the named program-bus channel.
      if (form.pinnedChannels.trim() !== '' && Number.isFinite(pinned)) {
        return { kind: 'audio_discrete_track', track: id, pinned_channels: pinned };
      }
      return { kind: 'audio_program_bus', channel: id };
    }
  }
}

/** Compose the full take request from the form. */
function buildRequest(form: RouteForm): RouteTakeRequest {
  return {
    source: buildSource(form),
    target: buildTarget(form),
    ...(form.coerce ? { coerce: true } : {}),
  };
}

/** Map a route class to a banner badge hue + label. */
function classBadge(routeClass: RouteClass): {
  variant: BadgeProps['variant'];
  label: JSX.Element;
} {
  switch (routeClass) {
    case 'class1':
      return { variant: 'live', label: <Trans>Class-1 — hot, seamless at a frame boundary</Trans> };
    case 'reset_lite':
      return { variant: 'reconnecting', label: <Trans>Reset-lite — a brief controlled reset</Trans> };
    case 'class2':
      return {
        variant: 'stale',
        label: <Trans>Class-2 — make-before-break migration (asynchronous)</Trans>,
      };
  }
}

/** The crosspoint routing page. */
export function RoutingPage(): JSX.Element {
  const { t } = useLingui();
  const [form, setForm] = useState<RouteForm>(EMPTY_FORM);
  const [plan, setPlan] = useState<RoutePlan | null>(null);

  const planRoute = usePlanRoute();
  const takeRoute = useTakeRoute();

  // The take is only valid once a source input id and a destination are given.
  const valid = useMemo(
    () => form.inputId.trim() !== '' && form.target.trim() !== '',
    [form.inputId, form.target],
  );

  const needsSelectorValue = form.selectorBy !== 'best';

  const onPlan = (): void => {
    setPlan(null);
    planRoute.mutate(buildRequest(form), {
      onSuccess: (result): void => {
        setPlan(result);
      },
      onError: (error): void => {
        toast({
          title: t`Could not plan the take`,
          description: error.message,
          variant: 'destructive',
        });
      },
    });
  };

  const onTake = (): void => {
    takeRoute.mutate(
      { kind: form.kind, request: buildRequest(form) },
      {
        onSuccess: (outcome): void => {
          if (outcome.status === 'applied') {
            toast({
              title: t`Take applied`,
              description: t`Applied as ${outcome.applied.class}. Operation ${outcome.applied.operation_id}.`,
            });
          } else {
            toast({
              title: t`Migration accepted`,
              description: t`Operation ${outcome.accepted.operation_id}; the outcome arrives on the realtime stream.`,
            });
          }
        },
        onError: (error): void => {
          toast({
            title: t`Could not take`,
            description: error.message,
            variant: 'destructive',
          });
        },
      },
    );
  };

  const targetLabel = ((): string => {
    switch (form.kind) {
      case 'video':
        return t`Target video cell`;
      case 'audio':
        return t`Target program-bus channel or discrete track`;
      case 'subtitle':
        return t`Target subtitle layer`;
    }
  })();

  return (
    <>
      <PageHeader
        title={<Trans>Routing</Trans>}
        description={
          <Trans>
            Route a source elementary stream onto a destination. Plan classifies
            the take without applying it; Take applies it — a Class-2 migration
            is accepted asynchronously and its outcome arrives on the realtime
            stream.
          </Trans>
        }
      />

      <form
        className="grid max-w-2xl gap-4"
        onSubmit={(e): void => {
          e.preventDefault();
          if (valid) {
            onTake();
          }
        }}
      >
        <div className="grid gap-1.5">
          <Label htmlFor="route-kind">
            <Trans>Take kind</Trans>
          </Label>
          <Select
            value={form.kind}
            onValueChange={(value): void => {
              // Changing the kind invalidates the prior plan + target.
              setPlan(null);
              setForm({ ...form, kind: asRouteKind(value), target: '', pinnedChannels: '' });
            }}
          >
            <SelectTrigger id="route-kind" className="w-56">
              <SelectValue />
            </SelectTrigger>
            <SelectContent>
              {ROUTE_KINDS.map((option) => (
                <SelectItem key={option} value={option}>
                  {option}
                </SelectItem>
              ))}
            </SelectContent>
          </Select>
        </div>

        <div className="grid gap-1.5">
          <Label htmlFor="route-input">
            <Trans>Source input id</Trans>
          </Label>
          <Input
            id="route-input"
            className="w-72"
            value={form.inputId}
            placeholder={t`e.g. cam-north`}
            onChange={(e): void => {
              setPlan(null);
              setForm({ ...form, inputId: e.target.value });
            }}
          />
        </div>

        <div className="flex flex-wrap items-end gap-3">
          <div className="grid gap-1.5">
            <Label htmlFor="route-selector-by">
              <Trans>Stream selector</Trans>
            </Label>
            <Select
              value={form.selectorBy}
              onValueChange={(value): void => {
                setPlan(null);
                setForm({ ...form, selectorBy: asSelectorBy(value), selectorValue: '' });
              }}
            >
              <SelectTrigger id="route-selector-by" className="w-44">
                <SelectValue />
              </SelectTrigger>
              <SelectContent>
                {SELECTOR_BYS.map((option) => (
                  <SelectItem key={option} value={option}>
                    {option}
                  </SelectItem>
                ))}
              </SelectContent>
            </Select>
          </div>
          {needsSelectorValue ? (
            <div className="grid gap-1.5">
              <Label htmlFor="route-selector-value">
                {form.selectorBy === 'index' ? (
                  <Trans>Stream index</Trans>
                ) : form.selectorBy === 'language' ? (
                  <Trans>Language tag</Trans>
                ) : (
                  <Trans>Stream id</Trans>
                )}
              </Label>
              <Input
                id="route-selector-value"
                className="w-44"
                type={form.selectorBy === 'index' ? 'number' : 'text'}
                value={form.selectorValue}
                placeholder={
                  form.selectorBy === 'language'
                    ? t`e.g. eng`
                    : form.selectorBy === 'stream_id'
                      ? t`e.g. v/pid:256`
                      : t`e.g. 0`
                }
                onChange={(e): void => {
                  setPlan(null);
                  setForm({ ...form, selectorValue: e.target.value });
                }}
              />
            </div>
          ) : null}
        </div>

        <div className="grid gap-1.5">
          <Label htmlFor="route-target">{targetLabel}</Label>
          <Input
            id="route-target"
            className="w-72"
            value={form.target}
            placeholder={form.kind === 'video' ? t`e.g. cell-a` : t`destination id`}
            onChange={(e): void => {
              setPlan(null);
              setForm({ ...form, target: e.target.value });
            }}
          />
        </div>

        {form.kind === 'audio' ? (
          <div className="grid gap-1.5">
            <Label htmlFor="route-pinned-channels">
              <Trans>Discrete-track channel count (optional)</Trans>
            </Label>
            <Input
              id="route-pinned-channels"
              className="w-72"
              type="number"
              value={form.pinnedChannels}
              placeholder={t`leave blank to route onto a program-bus channel`}
              onChange={(e): void => {
                setPlan(null);
                setForm({ ...form, pinnedChannels: e.target.value });
              }}
            />
            <p className="text-xs text-muted-foreground">
              <Trans>
                Set a channel count to route onto a discrete track; leave it
                blank to route onto the named program-bus channel.
              </Trans>
            </p>
          </div>
        ) : null}

        <div className="flex items-center gap-2">
          <input
            id="route-coerce"
            type="checkbox"
            className="size-4"
            checked={form.coerce}
            onChange={(e): void => {
              setPlan(null);
              setForm({ ...form, coerce: e.target.checked });
            }}
          />
          <Label htmlFor="route-coerce">
            <Trans>Allow a confirmed down/up-mix to the pinned layout</Trans>
          </Label>
        </div>

        {plan !== null ? (
          <div
            role="status"
            aria-live="polite"
            className="rounded-md border p-3 text-sm"
          >
            <div className="flex flex-wrap items-center gap-2">
              <Badge variant={classBadge(plan.class).variant}>{plan.class}</Badge>
              <span>{classBadge(plan.class).label}</span>
            </div>
            {plan.coerced ? (
              <p className="mt-2 text-xs text-muted-foreground">
                <Trans>
                  This take would be coerced to Class-1 with degradation (a
                  pinned-layout mix).
                </Trans>
              </p>
            ) : null}
          </div>
        ) : null}

        <div className="flex items-center gap-2">
          <Button
            type="button"
            variant="outline"
            disabled={!valid || planRoute.isPending}
            onClick={onPlan}
          >
            <RouteIcon aria-hidden="true" />
            <Trans>Plan (dry run)</Trans>
          </Button>
          <Button type="submit" disabled={!valid || takeRoute.isPending}>
            <Play aria-hidden="true" />
            <Trans>Take</Trans>
          </Button>
        </div>
      </form>
    </>
  );
}
