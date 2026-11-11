// Graphics → Playout: the CG operator's surface.
//
// PVW and PGM monitors (the channel's preview/program sources through the
// standard preview ladder), the rundown (click to cue; drag or Alt+↑/↓ to
// reorder), the selected item's fields, and TAKE / CLEAR / NEXT with the
// common CG-app shortcuts: F1 take, F2 clear, Space next. Cut/mix and the
// mix duration (frames) are the channel's transition preset.
import { isBackendUnavailable } from '../../api/operations';
import { useCallback, useEffect, useMemo, useState } from 'react';
import type { DragEvent, JSX, KeyboardEvent as ReactKeyboardEvent } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { ArrowDown, ArrowUp, GripVertical, Play, SkipForward, Square, X } from 'lucide-react';

import {
  clear,
  createChannelSources,
  cue,
  graphicsKeys,
  listChannels,
  listItems,
  listSourceIds,
  next,
  putRundown,
  take,
} from '../../api/graphics';
import type { ChannelView, ItemDoc, TransitionDoc } from '../../api/graphics';
import { Badge } from '../../components/ui/badge';
import { Button } from '../../components/ui/button';
import { Card, CardContent, CardHeader, CardTitle } from '../../components/ui/card';
import { Input } from '../../components/ui/input';
import { Label } from '../../components/ui/label';
import { toast } from '../../components/ui/use-toast';
import { PreviewSurface } from '../../preview/PreviewSurface';
import { usePreviewCapabilities } from '../../preview/capabilities';

function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  const tag = target.tagName;
  return tag === 'INPUT' || tag === 'TEXTAREA' || tag === 'SELECT' || target.isContentEditable;
}

function Monitor({
  label,
  sourceId,
  live,
  onAir,
}: {
  readonly label: string;
  readonly sourceId: string;
  readonly live: boolean;
  readonly onAir: boolean;
}): JSX.Element {
  const capabilities = usePreviewCapabilities();
  return (
    <figure className="space-y-1">
      <figcaption className="flex items-center gap-2 text-sm font-semibold">
        <span
          className={
            onAir
              ? 'rounded bg-red-600 px-2 py-0.5 text-white'
              : 'rounded bg-green-700 px-2 py-0.5 text-white'
          }
        >
          {label}
        </span>
        <code className="text-xs text-muted-foreground">{sourceId}</code>
      </figcaption>
      <div className="aspect-video w-full overflow-hidden rounded-md border bg-[repeating-conic-gradient(#333_0_25%,#222_0_50%)] bg-[length:16px_16px]">
        {live ? (
          <PreviewSurface
            key={sourceId}
            scope="input"
            whepEndpoint={`/api/v1/preview/inputs/${encodeURIComponent(sourceId)}/whep`}
            jpegPath={`/api/v1/preview/inputs/${encodeURIComponent(sourceId)}.jpg`}
            label={label}
            capabilities={capabilities.data}
            className="h-full w-full object-contain"
          />
        ) : (
          <div className="flex h-full items-center justify-center p-4 text-center text-xs text-muted-foreground">
            <Trans>No monitor source yet — use “Create PVW/PGM sources” below.</Trans>
          </div>
        )}
      </div>
    </figure>
  );
}

export function PlayoutView(): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const channels = useQuery({ queryKey: graphicsKeys.channels, queryFn: listChannels, refetchInterval: 10_000 });
  const items = useQuery({ queryKey: graphicsKeys.items, queryFn: listItems });
  const sources = useQuery({ queryKey: ['graphics', 'source-ids'], queryFn: listSourceIds });
  const [channelId, setChannelId] = useState<string>();
  const channel: ChannelView | undefined = useMemo(() => {
    const list = channels.data ?? [];
    return list.find((c) => c.id === channelId) ?? list[0];
  }, [channels.data, channelId]);
  const itemMap = useMemo(() => new Map((items.data ?? []).map((i) => [i.id, i])), [items.data]);
  const [selected, setSelected] = useState<string>();
  const [dragIndex, setDragIndex] = useState<number>();
  const [addItem, setAddItem] = useState('');

  const setChannel = (view: ChannelView): void => {
    queryClient.setQueryData<ChannelView[]>(graphicsKeys.channels, (list) =>
      (list ?? []).map((c) => (c.id === view.id ? view : c)),
    );
  };
  const fail = (title: string) => (error: unknown): void => {
    toast({ title, description: error instanceof Error ? error.message : String(error), variant: 'destructive' });
  };

  const verb = useMutation({
    mutationFn: async (action: { kind: 'take' | 'clear' | 'next' } | { kind: 'cue'; item: string }) => {
      if (channel === undefined) throw new Error(t`No graphics channel`);
      switch (action.kind) {
        case 'take':
          return take(channel.id);
        case 'clear':
          return clear(channel.id);
        case 'next':
          return next(channel.id);
        case 'cue':
          return cue(channel.id, action.item);
      }
    },
    onSuccess: setChannel,
    onError: fail(t`Playout command failed`),
  });

  const saveRundown = useMutation({
    mutationFn: (args: { rundown: string[]; transition?: TransitionDoc }) => {
      if (channel === undefined) throw new Error(t`No graphics channel`);
      return putRundown(channel.id, args.rundown, args.transition);
    },
    onSuccess: setChannel,
    onError: fail(t`Could not save the rundown`),
  });

  const makeSources = useMutation({
    mutationFn: (c: ChannelView) => createChannelSources(c),
    onSuccess: (): void => {
      toast({ title: t`Created the PVW and PGM sources` });
      void queryClient.invalidateQueries({ queryKey: ['graphics', 'source-ids'] });
    },
    onError: fail(t`Could not create sources`),
  });

  const { mutate } = verb;
  const doTake = useCallback((): void => {
    mutate({ kind: 'take' });
  }, [mutate]);
  const doClear = useCallback((): void => {
    mutate({ kind: 'clear' });
  }, [mutate]);
  const doNext = useCallback((): void => {
    mutate({ kind: 'next' });
  }, [mutate]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent): void => {
      if (isTypingTarget(e.target) || e.ctrlKey || e.metaKey || e.altKey) return;
      if (e.key === 'F1') {
        e.preventDefault();
        doTake();
      } else if (e.key === 'F2') {
        e.preventDefault();
        doClear();
      } else if (e.key === ' ' || e.code === 'Space') {
        // Space on a focused button would also click it; NEXT wins only when
        // focus is not on an interactive control.
        if (e.target instanceof HTMLButtonElement) return;
        e.preventDefault();
        doNext();
      }
    };
    window.addEventListener('keydown', onKey);
    return (): void => {
      window.removeEventListener('keydown', onKey);
    };
  }, [doTake, doClear, doNext]);

  if (channels.isLoading) {
    return <p><Trans>Loading…</Trans></p>;
  }
  if (isBackendUnavailable(channels.error)) {
    return <p role="status"><Trans>Graphics are unavailable: this server does not provide them yet (backend not deployed).</Trans></p>;
  }
  if (channel === undefined) {
    return <p><Trans>Could not load graphics channels.</Trans></p>;
  }

  const rundown = channel.rundown;
  const move = (from: number, to: number): void => {
    if (to < 0 || to >= rundown.length || from === to) return;
    const nextOrder = [...rundown];
    const [moved] = nextOrder.splice(from, 1);
    if (moved === undefined) return;
    nextOrder.splice(to, 0, moved);
    saveRundown.mutate({ rundown: nextOrder });
  };
  const transition = channel.transition;
  const sourceIds = new Set(sources.data ?? []);
  const haveSources = sourceIds.has(channel.program_source) && sourceIds.has(channel.preview_source);
  const selectedItem: ItemDoc | undefined = itemMap.get(selected ?? channel.preview ?? '');
  const notInRundown = (items.data ?? []).filter((i) => !rundown.includes(i.id));
  const nameOf = (id: string | null | undefined): string =>
    id === null || id === undefined ? '—' : (itemMap.get(id)?.name ?? id);

  return (
    <div className="space-y-4">
      {(channels.data ?? []).length > 1 ? (
        <div className="flex items-center gap-2">
          <Label htmlFor="gfx-channel"><Trans>Channel</Trans></Label>
          <select
            id="gfx-channel"
            className="h-9 rounded-md border border-input bg-background px-3 text-sm"
            value={channel.id}
            onChange={(e): void => {
              setChannelId(e.target.value);
            }}
          >
            {(channels.data ?? []).map((c) => (
              <option key={c.id} value={c.id}>{c.name}</option>
            ))}
          </select>
        </div>
      ) : null}

      <div className="grid gap-4 md:grid-cols-2">
        <div>
          <Monitor label={t`PVW`} sourceId={channel.preview_source} live={haveSources} onAir={false} />
          <p className="mt-1 text-sm">
            <Trans>Cued:</Trans> <strong data-testid="gfx-pvw-item">{nameOf(channel.preview)}</strong>
          </p>
        </div>
        <div>
          <Monitor label={t`PGM`} sourceId={channel.program_source} live={haveSources} onAir />
          <p className="mt-1 text-sm">
            <Trans>On air:</Trans> <strong data-testid="gfx-pgm-item">{nameOf(channel.program)}</strong>
          </p>
        </div>
      </div>

      {!haveSources ? (
        <div className="flex flex-wrap items-center gap-2 rounded-md border p-3 text-sm">
          <span>
            <Trans>
              The channel is a source the switcher uses: put its PGM source in a layout cell above
              program/video (it is transparent when clear), or use it full-screen.
            </Trans>
          </span>
          <Button size="sm" variant="outline" disabled={makeSources.isPending} onClick={(): void => {
            makeSources.mutate(channel);
          }}>
            <Trans>Create PVW/PGM sources</Trans>
          </Button>
        </div>
      ) : null}

      <div className="flex flex-wrap items-end gap-3">
        <Button
          size="lg"
          className="h-16 min-w-36 bg-red-600 text-lg font-bold text-white hover:bg-red-700"
          disabled={verb.isPending || channel.preview === null || channel.preview === undefined}
          onClick={doTake}
          aria-keyshortcuts="F1"
        >
          <Play aria-hidden="true" className="mr-2 h-5 w-5" />
          <Trans>TAKE</Trans>
          <kbd className="ml-2 text-xs opacity-80">F1</kbd>
        </Button>
        <Button
          size="lg"
          variant="outline"
          className="h-16 min-w-36 text-lg font-bold"
          disabled={verb.isPending}
          onClick={doClear}
          aria-keyshortcuts="F2"
        >
          <Square aria-hidden="true" className="mr-2 h-5 w-5" />
          <Trans>CLEAR</Trans>
          <kbd className="ml-2 text-xs opacity-80">F2</kbd>
        </Button>
        <Button
          size="lg"
          variant="secondary"
          className="h-16 min-w-36 text-lg font-bold"
          disabled={verb.isPending}
          onClick={doNext}
          aria-keyshortcuts="Space"
        >
          <SkipForward aria-hidden="true" className="mr-2 h-5 w-5" />
          <Trans>NEXT</Trans>
          <kbd className="ml-2 text-xs opacity-80">Space</kbd>
        </Button>

        <fieldset className="flex items-end gap-2 rounded-md border px-3 py-2">
          <legend className="px-1 text-xs text-muted-foreground"><Trans>Transition</Trans></legend>
          <div className="flex gap-1" role="radiogroup" aria-label={t`Transition`}>
            {(['cut', 'mix'] as const).map((kind) => (
              <Button
                key={kind}
                size="sm"
                role="radio"
                aria-checked={transition.kind === kind}
                variant={transition.kind === kind ? 'default' : 'outline'}
                onClick={(): void => {
                  saveRundown.mutate({ rundown, transition: { kind, frames: transition.frames } });
                }}
              >
                {kind === 'cut' ? <Trans>CUT</Trans> : <Trans>MIX</Trans>}
              </Button>
            ))}
          </div>
          <div className="space-y-1">
            <Label htmlFor="gfx-frames" className="text-xs"><Trans>Duration (frames)</Trans></Label>
            <Input
              id="gfx-frames"
              type="number"
              min={1}
              max={600}
              className="h-9 w-24"
              defaultValue={transition.frames}
              key={`${channel.id}-${String(transition.frames)}`}
              onBlur={(e): void => {
                const frames = Math.max(1, Math.min(600, Math.round(Number(e.target.value) || 1)));
                if (frames !== transition.frames) {
                  saveRundown.mutate({ rundown, transition: { kind: transition.kind, frames } });
                }
              }}
            />
          </div>
        </fieldset>
      </div>
      <p className="text-xs text-muted-foreground">
        <Trans>
          Shortcuts: F1 = TAKE (preview to air), F2 = CLEAR (off air), Space = NEXT (cue the next
          rundown item). Click a rundown row to cue it. Drag rows, or press Alt+↑ / Alt+↓ on a row,
          to reorder.
        </Trans>
      </p>

      <div className="grid gap-4 md:grid-cols-3">
        <Card className="md:col-span-2">
          <CardHeader>
            <CardTitle><Trans>Rundown</Trans></CardTitle>
          </CardHeader>
          <CardContent>
            {rundown.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                <Trans>The rundown is empty. Add items from the library below.</Trans>
              </p>
            ) : null}
            <ul className="space-y-1" role="listbox" aria-label={t`Rundown`}>
              {rundown.map((id, index) => {
                const onAir = channel.program === id;
                const cued = channel.preview === id;
                return (
                  <li
                    key={id}
                    role="option"
                    draggable
                    tabIndex={0}
                    aria-selected={selected === id}
                    onDragStart={(): void => {
                      setDragIndex(index);
                    }}
                    onDragOver={(e: DragEvent): void => {
                      e.preventDefault();
                    }}
                    onDrop={(): void => {
                      if (dragIndex !== undefined) move(dragIndex, index);
                      setDragIndex(undefined);
                    }}
                    onKeyDown={(e: ReactKeyboardEvent): void => {
                      if (e.altKey && e.key === 'ArrowUp') {
                        e.preventDefault();
                        move(index, index - 1);
                      } else if (e.altKey && e.key === 'ArrowDown') {
                        e.preventDefault();
                        move(index, index + 1);
                      } else if (e.key === 'Enter') {
                        e.preventDefault();
                        setSelected(id);
                        verb.mutate({ kind: 'cue', item: id });
                      }
                    }}
                    onClick={(): void => {
                      setSelected(id);
                      verb.mutate({ kind: 'cue', item: id });
                    }}
                    className={`flex cursor-pointer items-center gap-2 rounded-md border p-2 text-sm ${
                      onAir ? 'border-red-600 bg-red-600/10' : cued ? 'border-green-700 bg-green-700/10' : ''
                    }`}
                  >
                    <GripVertical aria-hidden="true" className="h-4 w-4 text-muted-foreground" />
                    <span className="w-6 text-muted-foreground">{index + 1}</span>
                    <span className="flex-1 font-medium">{nameOf(id)}</span>
                    {onAir ? <Badge variant="destructive"><Trans>PGM</Trans></Badge> : null}
                    {cued ? <Badge><Trans>PVW</Trans></Badge> : null}
                    <Button size="sm" variant="ghost" aria-label={t`Move ${nameOf(id)} up`} onClick={(e): void => { e.stopPropagation(); move(index, index - 1); }}>
                      <ArrowUp aria-hidden="true" className="h-4 w-4" />
                    </Button>
                    <Button size="sm" variant="ghost" aria-label={t`Move ${nameOf(id)} down`} onClick={(e): void => { e.stopPropagation(); move(index, index + 1); }}>
                      <ArrowDown aria-hidden="true" className="h-4 w-4" />
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      aria-label={t`Remove ${nameOf(id)} from the rundown`}
                      onClick={(e): void => {
                        e.stopPropagation();
                        saveRundown.mutate({ rundown: rundown.filter((x) => x !== id) });
                      }}
                    >
                      <X aria-hidden="true" className="h-4 w-4" />
                    </Button>
                  </li>
                );
              })}
            </ul>
            {notInRundown.length > 0 ? (
              <div className="mt-3 flex items-end gap-2">
                <div className="space-y-1">
                  <Label htmlFor="gfx-add"><Trans>Add to rundown</Trans></Label>
                  <select
                    id="gfx-add"
                    className="h-9 rounded-md border border-input bg-background px-3 text-sm"
                    value={addItem}
                    onChange={(e): void => {
                      setAddItem(e.target.value);
                    }}
                  >
                    <option value="">{t`Choose an item…`}</option>
                    {notInRundown.map((i) => (
                      <option key={i.id} value={i.id}>{i.name}</option>
                    ))}
                  </select>
                </div>
                <Button
                  size="sm"
                  disabled={addItem === ''}
                  onClick={(): void => {
                    saveRundown.mutate({ rundown: [...rundown, addItem] });
                    setAddItem('');
                  }}
                >
                  <Trans>Add</Trans>
                </Button>
              </div>
            ) : null}
          </CardContent>
        </Card>

        <Card>
          <CardHeader>
            <CardTitle><Trans>Selected item</Trans></CardTitle>
          </CardHeader>
          <CardContent className="text-sm">
            {selectedItem === undefined ? (
              <p className="text-muted-foreground"><Trans>Select or cue an item to see its fields.</Trans></p>
            ) : (
              <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1">
                <dt className="text-muted-foreground"><Trans>Name</Trans></dt>
                <dd>{selectedItem.name}</dd>
                <dt className="text-muted-foreground"><Trans>Template</Trans></dt>
                <dd><code>{selectedItem.template}</code></dd>
                <dt className="text-muted-foreground"><Trans>Data source</Trans></dt>
                <dd><code>{selectedItem.data_source ?? '—'}</code></dd>
                <dt className="text-muted-foreground"><Trans>Background</Trans></dt>
                <dd>{selectedItem.style?.background ?? 'gradient'}</dd>
                <dt className="text-muted-foreground"><Trans>Accent</Trans></dt>
                <dd>{selectedItem.style?.accent ?? '—'}</dd>
              </dl>
            )}
          </CardContent>
        </Card>
      </div>
    </div>
  );
}
