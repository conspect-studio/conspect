// Tally — resolved lamp state, a manual override, and tally profiles.
//
//   * Resolved state: `GET /api/v1/tally` lists `{ target, state }` per target;
//     the lamp is shown as colour NAME + glyph + bus, never colour alone.
//   * Override: `PUT /api/v1/tally/override` forces a target's lamp and
//     `DELETE /api/v1/tally/override` clears it; both return `202` (the resolved
//     lamp arrives later on the realtime stream).
//   * Profiles: `GET /api/v1/tally/profiles` lists profiles, and one can be
//     deleted (`DELETE /api/v1/tally/profiles/{id}`, `If-Match`). The bit-colour
//     and index-cell rules of a profile are shown read-only here; editing those
//     mapping tables is done in config-as-code, not this view.
import { useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { CircleOff, Lightbulb, Trash2 } from 'lucide-react';

import {
  useDeleteProfile,
  useTally,
  useTallyOverride,
  useTallyProfiles,
} from '../api/tallyQueries';
import type {
  TallyColor,
  TallyEntry,
  TallyProfile,
  TallyTarget,
} from '../api/tallyQueries';
import { PageHeader } from '../components/PageHeader';
import { TallyLampBadge } from '../components/TallyLampBadge';
import { Button } from '../components/ui/button';
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '../components/ui/dialog';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '../components/ui/select';
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '../components/ui/table';
import { Tabs, TabsContent, TabsList, TabsTrigger } from '../components/ui/tabs';
import { toast } from '../components/ui/use-toast';

const COLORS: readonly TallyColor[] = ['Off', 'Red', 'Green', 'Amber'];

/** A target rendered as plain text (text carries the meaning). */
function targetLabel(target: TallyTarget): string {
  return target.kind === 'tile' ? `tile #${String(target.index)}` : `element ${target.name}`;
}

/** A bus rendered as plain text. */
function busLabel(source: TallyEntry['state']['source']): string {
  switch (source.kind) {
    case 'program':
      return 'program';
    case 'preview':
      return 'preview';
    case 'aux':
      return `aux ${String(source.index)}`;
    case 'iso':
      return `iso ${String(source.index)}`;
  }
}

interface OverrideForm {
  /** Target kind: a tile index or a named element. */
  readonly kind: 'tile' | 'element';
  /** The tile index (when kind === 'tile'). */
  readonly index: string;
  /** The element name (when kind === 'element'). */
  readonly name: string;
  /** The colour to force. */
  readonly color: TallyColor;
}

const EMPTY_FORM: OverrideForm = { kind: 'tile', index: '0', name: '', color: 'Red' };

function asColor(value: string): TallyColor {
  return COLORS.find((c) => c === value) ?? 'Off';
}

function buildTarget(form: OverrideForm): TallyTarget | undefined {
  if (form.kind === 'tile') {
    const index = Number(form.index);
    if (!Number.isInteger(index) || index < 0) {
      return undefined;
    }
    return { kind: 'tile', index };
  }
  const name = form.name.trim();
  if (name === '') {
    return undefined;
  }
  return { kind: 'element', name };
}

/** The tally management page. */
export function TallyPage(): JSX.Element {
  const { t } = useLingui();
  const tally = useTally();
  const profiles = useTallyProfiles();
  const override = useTallyOverride();
  const deleteProfile = useDeleteProfile();

  const [form, setForm] = useState<OverrideForm>(EMPTY_FORM);
  const [pendingDelete, setPendingDelete] = useState<TallyProfile | null>(null);

  const entries = useMemo<TallyEntry[]>(() => tally.data ?? [], [tally.data]);
  const profileList = useMemo<TallyProfile[]>(() => profiles.data ?? [], [profiles.data]);

  const applyOverride = (action: 'set' | 'clear'): void => {
    const target = buildTarget(form);
    if (target === undefined) {
      toast({ title: t`Enter a valid target`, variant: 'destructive' });
      return;
    }
    const vars =
      action === 'set'
        ? ({ action: 'set', target, color: form.color } as const)
        : ({ action: 'clear', target } as const);
    override.mutate(vars, {
      onSuccess: (accepted): void => {
        toast({
          title: action === 'set' ? t`Override accepted` : t`Override cleared`,
          description: `${t`Operation id`}: ${accepted.operation_id}`,
        });
      },
      onError: (error): void => {
        toast({
          title: t`Override failed`,
          description: error.message,
          variant: 'destructive',
        });
      },
    });
  };

  const confirmDelete = (): void => {
    const target = pendingDelete;
    if (target === null) {
      return;
    }
    deleteProfile.mutate(target.id, {
      onSuccess: (): void => {
        toast({ title: t`Profile deleted` });
      },
      onError: (error): void => {
        toast({
          title: t`Could not delete profile`,
          description: error.message,
          variant: 'destructive',
        });
      },
    });
    setPendingDelete(null);
  };

  return (
    <>
      <PageHeader
        title={<Trans>Tally</Trans>}
        description={
          <Trans>
            Resolved tally lamps, a manual override, and the tally profiles that
            map incoming protocol words to lamps.
          </Trans>
        }
      />

      <Tabs defaultValue="state">
        <TabsList>
          <TabsTrigger value="state">
            <Trans>Resolved state</Trans>
          </TabsTrigger>
          <TabsTrigger value="profiles">
            <Trans>Profiles</Trans>
          </TabsTrigger>
        </TabsList>

        <TabsContent value="state">
          <section aria-labelledby="override-heading" className="mb-6 rounded-md border p-4">
            <h2 id="override-heading" className="mb-3 text-base font-semibold">
              <Trans>Manual override</Trans>
            </h2>
            <div className="flex flex-wrap items-end gap-4">
              <div className="grid gap-1.5">
                <Label htmlFor="ovr-kind">
                  <Trans>Target</Trans>
                </Label>
                <Select
                  value={form.kind}
                  onValueChange={(value): void => {
                    setForm({ ...form, kind: value === 'element' ? 'element' : 'tile' });
                  }}
                >
                  <SelectTrigger id="ovr-kind" className="w-36">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    <SelectItem value="tile">{t`Tile index`}</SelectItem>
                    <SelectItem value="element">{t`Element name`}</SelectItem>
                  </SelectContent>
                </Select>
              </div>

              {form.kind === 'tile' ? (
                <div className="grid gap-1.5">
                  <Label htmlFor="ovr-index">
                    <Trans>Index</Trans>
                  </Label>
                  <Input
                    id="ovr-index"
                    inputMode="numeric"
                    className="w-24"
                    value={form.index}
                    onChange={(e): void => {
                      setForm({ ...form, index: e.target.value });
                    }}
                  />
                </div>
              ) : (
                <div className="grid gap-1.5">
                  <Label htmlFor="ovr-name">
                    <Trans>Name</Trans>
                  </Label>
                  <Input
                    id="ovr-name"
                    className="w-44"
                    value={form.name}
                    onChange={(e): void => {
                      setForm({ ...form, name: e.target.value });
                    }}
                  />
                </div>
              )}

              <div className="grid gap-1.5">
                <Label htmlFor="ovr-color">
                  <Trans>Lamp</Trans>
                </Label>
                <Select
                  value={form.color}
                  onValueChange={(value): void => {
                    setForm({ ...form, color: asColor(value) });
                  }}
                >
                  <SelectTrigger id="ovr-color" className="w-32">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {COLORS.map((c) => (
                      <SelectItem key={c} value={c}>
                        {c}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>

              <div className="flex items-center gap-2">
                <Button
                  disabled={override.isPending}
                  onClick={(): void => {
                    applyOverride('set');
                  }}
                >
                  <Lightbulb aria-hidden="true" />
                  <Trans>Force lamp</Trans>
                </Button>
                <Button
                  variant="outline"
                  disabled={override.isPending}
                  onClick={(): void => {
                    applyOverride('clear');
                  }}
                >
                  <CircleOff aria-hidden="true" />
                  <Trans>Clear override</Trans>
                </Button>
              </div>
            </div>
          </section>

          {tally.isPending ? (
            <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
              <Trans>Loading tally state…</Trans>
            </p>
          ) : tally.isError ? (
            <p role="alert" className="text-sm text-destructive">
              <Trans>Could not load tally state:</Trans> {tally.error.message}
            </p>
          ) : entries.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              <Trans>No tally targets are reporting a lamp state.</Trans>
            </p>
          ) : (
            <Table>
              <TableCaption>{t`Resolved tally lamp per target.`}</TableCaption>
              <TableHeader>
                <TableRow>
                  <TableHead>{t`Target`}</TableHead>
                  <TableHead>{t`Lamp`}</TableHead>
                  <TableHead>{t`Brightness`}</TableHead>
                  <TableHead>{t`Bus`}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {entries.map((entry) => (
                  <TableRow key={targetLabel(entry.target)}>
                    <TableCell>
                      <code className="text-xs">{targetLabel(entry.target)}</code>
                    </TableCell>
                    <TableCell>
                      <TallyLampBadge color={entry.state.color} />
                    </TableCell>
                    <TableCell className="tabular-nums">{entry.state.brightness}</TableCell>
                    <TableCell>
                      <code className="text-xs text-muted-foreground">
                        {busLabel(entry.state.source)}
                      </code>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </TabsContent>

        <TabsContent value="profiles">
          {profiles.isPending ? (
            <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
              <Trans>Loading profiles…</Trans>
            </p>
          ) : profiles.isError ? (
            <p role="alert" className="text-sm text-destructive">
              <Trans>Could not load profiles:</Trans> {profiles.error.message}
            </p>
          ) : profileList.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              <Trans>No tally profiles are defined.</Trans>
            </p>
          ) : (
            <Table>
              <TableCaption>{t`Tally profiles and their rule counts.`}</TableCaption>
              <TableHeader>
                <TableRow>
                  <TableHead>{t`Identifier`}</TableHead>
                  <TableHead>{t`Bit→colour rules`}</TableHead>
                  <TableHead>{t`Index→cell rules`}</TableHead>
                  <TableHead>{t`Actions`}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {profileList.map((profile) => (
                  <TableRow key={profile.id}>
                    <TableCell>
                      <code className="text-xs">{profile.id}</code>
                    </TableCell>
                    <TableCell className="tabular-nums">
                      {profile.bit_colors?.length ?? 0}
                    </TableCell>
                    <TableCell className="tabular-nums">
                      {profile.index_cells?.length ?? 0}
                    </TableCell>
                    <TableCell>
                      <Button
                        variant="ghost"
                        size="sm"
                        aria-label={`${t`Delete profile`}: ${profile.id}`}
                        onClick={(): void => {
                          setPendingDelete(profile);
                        }}
                      >
                        <Trash2 aria-hidden="true" />
                        <Trans>Delete</Trans>
                      </Button>
                    </TableCell>
                  </TableRow>
                ))}
              </TableBody>
            </Table>
          )}
        </TabsContent>
      </Tabs>

      <Dialog
        open={pendingDelete !== null}
        onOpenChange={(open): void => {
          if (!open) {
            setPendingDelete(null);
          }
        }}
      >
        <DialogContent>
          <DialogHeader>
            <DialogTitle>
              <Trans>Delete tally profile?</Trans>
            </DialogTitle>
            <DialogDescription>
              <Trans>This permanently removes the profile's mapping rules.</Trans>
            </DialogDescription>
          </DialogHeader>
          {pendingDelete !== null ? (
            <p className="text-sm">
              <code>{pendingDelete.id}</code>
            </p>
          ) : null}
          <DialogFooter>
            <Button
              variant="outline"
              onClick={(): void => {
                setPendingDelete(null);
              }}
            >
              <Trans>Cancel</Trans>
            </Button>
            <Button variant="destructive" onClick={confirmDelete}>
              <Trans>Delete</Trans>
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
