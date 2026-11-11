// Graphics → Library: prepared graphics items (Vizrt "pages"). Create one
// from a template, bind it to a data source, set its style, see a thumbnail.
import { useState } from 'react';
import type { JSX, SyntheticEvent } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { Pencil, Plus, Trash2 } from 'lucide-react';

import {
  deleteItem,
  graphicsKeys,
  itemThumbnailPath,
  listDataSources,
  listItems,
  listTemplates,
  putItem,
} from '../../api/graphics';
import type { ItemDoc } from '../../api/graphics';
import { Button } from '../../components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '../../components/ui/card';
import { Input } from '../../components/ui/input';
import { Label } from '../../components/ui/label';
import { toast } from '../../components/ui/use-toast';
import { useJpegPreview } from '../../preview/useJpegPreview';

interface Form {
  id: string;
  name: string;
  template: string;
  data_source: string;
  background: string;
  accent: string;
}

const EMPTY: Form = {
  id: '',
  name: '',
  template: 'weather.seven_day',
  data_source: '',
  background: 'gradient',
  accent: '',
};

const SELECT = 'h-9 min-h-[44px] w-full rounded-md border border-input bg-background px-3 text-sm';

function Thumbnail({ item }: { readonly item: ItemDoc }): JSX.Element {
  // Thumbnails are rendered server-side off the output clock; refresh slowly.
  const url = useJpegPreview(`${itemThumbnailPath(item.id)}?v=${encodeURIComponent(JSON.stringify(item))}`, 60_000);
  return (
    <div className="aspect-video w-full overflow-hidden rounded bg-[repeating-conic-gradient(#333_0_25%,#222_0_50%)] bg-[length:16px_16px]">
      {url !== undefined ? <img src={url} alt={item.name} className="h-full w-full object-contain" /> : null}
    </div>
  );
}

export function LibraryView(): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const items = useQuery({ queryKey: graphicsKeys.items, queryFn: listItems });
  const templates = useQuery({ queryKey: graphicsKeys.templates, queryFn: listTemplates });
  const sources = useQuery({ queryKey: graphicsKeys.dataSources, queryFn: listDataSources });
  const [form, setForm] = useState<Form>(EMPTY);
  const [editing, setEditing] = useState(false);

  const invalidate = (): void => {
    void queryClient.invalidateQueries({ queryKey: graphicsKeys.all });
  };
  const fail = (title: string) => (error: unknown): void => {
    toast({ title, description: error instanceof Error ? error.message : String(error), variant: 'destructive' });
  };
  const save = useMutation({
    mutationFn: (f: Form) =>
      putItem(f.id.trim(), {
        name: f.name.trim(),
        template: f.template,
        data_source: f.data_source === '' ? null : f.data_source,
        style: { background: f.background, ...(f.accent !== '' ? { accent: f.accent } : {}) },
      }),
    onSuccess: (item): void => {
      toast({ title: t`Saved ${item.name}` });
      setForm(EMPTY);
      setEditing(false);
      invalidate();
    },
    onError: fail(t`Could not save the item`),
  });
  const remove = useMutation({
    mutationFn: deleteItem,
    onSuccess: invalidate,
    onError: fail(t`Could not delete the item`),
  });

  const onSubmit = (e: SyntheticEvent): void => {
    e.preventDefault();
    save.mutate(form);
  };
  const set = (key: keyof Form) => (e: { target: { value: string } }): void => {
    setForm({ ...form, [key]: e.target.value });
  };

  return (
    <div className="space-y-6">
      <Card>
        <CardHeader>
          <CardTitle>{editing ? <Trans>Edit item</Trans> : <Trans>New graphics item</Trans>}</CardTitle>
          <CardDescription>
            <Trans>An item is a template bound to data with a style — the thing you cue and take to air.</Trans>
          </CardDescription>
        </CardHeader>
        <CardContent>
          <form className="grid gap-4 md:grid-cols-3" onSubmit={onSubmit}>
            <div className="space-y-1">
              <Label htmlFor="gfx-item-id"><Trans>Item id</Trans></Label>
              <Input id="gfx-item-id" required disabled={editing} value={form.id} onChange={set('id')} />
            </div>
            <div className="space-y-1">
              <Label htmlFor="gfx-item-name"><Trans>Name</Trans></Label>
              <Input id="gfx-item-name" required value={form.name} onChange={set('name')} />
            </div>
            <div className="space-y-1">
              <Label htmlFor="gfx-item-template"><Trans>Template</Trans></Label>
              <select id="gfx-item-template" className={SELECT} value={form.template} onChange={set('template')}>
                {(templates.data ?? []).map((tpl) => (
                  <option key={tpl.id} value={tpl.id}>{tpl.name}</option>
                ))}
              </select>
            </div>
            <div className="space-y-1">
              <Label htmlFor="gfx-item-source"><Trans>Data source</Trans></Label>
              <select id="gfx-item-source" className={SELECT} value={form.data_source} onChange={set('data_source')} required>
                <option value="">{t`Choose a data source…`}</option>
                {(sources.data ?? []).map((s) => (
                  <option key={s.source.id} value={s.source.id}>{s.source.name} ({s.source.kind})</option>
                ))}
              </select>
            </div>
            <div className="space-y-1">
              <Label htmlFor="gfx-item-bg"><Trans>Background</Trans></Label>
              <select id="gfx-item-bg" className={SELECT} value={form.background} onChange={set('background')}>
                <option value="gradient">{t`Gradient (opaque)`}</option>
                <option value="transparent">{t`Transparent (key over video)`}</option>
              </select>
            </div>
            <div className="space-y-1">
              <Label htmlFor="gfx-item-accent"><Trans>Accent colour (#RRGGBB, optional)</Trans></Label>
              <Input id="gfx-item-accent" pattern="#[0-9A-Fa-f]{6}" value={form.accent} onChange={set('accent')} />
            </div>
            <div className="flex items-end gap-2 md:col-span-3">
              <Button type="submit" disabled={save.isPending}>
                <Plus aria-hidden="true" className="mr-1 h-4 w-4" />
                {editing ? <Trans>Save item</Trans> : <Trans>Create item</Trans>}
              </Button>
              {editing ? (
                <Button type="button" variant="outline" onClick={(): void => { setForm(EMPTY); setEditing(false); }}>
                  <Trans>Cancel</Trans>
                </Button>
              ) : null}
            </div>
          </form>
        </CardContent>
      </Card>

      {items.data?.length === 0 ? (
        <p className="text-sm text-muted-foreground"><Trans>No graphics items yet.</Trans></p>
      ) : null}
      <div className="grid gap-4 sm:grid-cols-2 lg:grid-cols-3">
        {(items.data ?? []).map((item) => (
          <Card key={item.id}>
            <CardContent className="space-y-2 pt-4">
              <Thumbnail item={item} />
              <div className="flex items-start justify-between gap-2">
                <div>
                  <div className="font-medium">{item.name}</div>
                  <div className="text-xs text-muted-foreground">
                    <code>{item.template}</code> · <code>{item.data_source ?? '—'}</code>
                  </div>
                </div>
                <div className="flex gap-1">
                  <Button
                    size="sm"
                    variant="ghost"
                    aria-label={t`Edit ${item.name}`}
                    onClick={(): void => {
                      setForm({
                        id: item.id,
                        name: item.name,
                        template: item.template,
                        data_source: item.data_source ?? '',
                        background: item.style?.background ?? 'gradient',
                        accent: item.style?.accent ?? '',
                      });
                      setEditing(true);
                    }}
                  >
                    <Pencil aria-hidden="true" className="h-4 w-4" />
                  </Button>
                  <Button
                    size="sm"
                    variant="ghost"
                    aria-label={t`Delete ${item.name}`}
                    onClick={(): void => {
                      if (window.confirm(t`Delete ${item.name}? If it is on air it is cut off air.`)) remove.mutate(item.id);
                    }}
                  >
                    <Trash2 aria-hidden="true" className="h-4 w-4" />
                  </Button>
                </div>
              </div>
            </CardContent>
          </Card>
        ))}
      </div>
    </div>
  );
}
