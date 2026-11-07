// /sources/iptv-org — the IPTV-ORG Source Browser (docs/io/iptv-org.md §4).
//
// Map-first location search + text search + facets, results grouped by channel
// in locality tiers (virtualised), a detail panel with probes, and Add / Bulk add
// that create ordinary linked sources. The map is optional chrome: the place
// search and the result list are the complete keyboard / screen-reader path.
import { lazy, Suspense, useEffect, useMemo, useRef, useState } from 'react';
import type { JSX } from 'react';
import { Link, useNavigate } from 'react-router-dom';
import { Plural, Trans, useLingui } from '@lingui/react/macro';
import { useVirtualizer } from '@tanstack/react-virtual';
import { ChevronDown, ChevronRight, LocateFixed, Plus, Radar } from 'lucide-react';

import type { IndexStream, Place, ProbeResult, StreamFlag } from '../api/catalogue';
import {
  useCatalogueIndex,
  useCataloguePlaces,
  useCatalogueStatus,
  useProbeMutation,
} from '../api/catalogueQueries';
import { PageHeader } from '../components/PageHeader';
import { Badge } from '../components/ui/badge';
import { Button } from '../components/ui/button';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import { SelectField } from '../resources/FormControls';
import { useSources } from '../resources/queries';
import { DataSourcesFooter, FlagBadge, VerdictBadge, useFlagLabel } from './Badges';
import { BulkAddDialog } from './BulkAddDialog';
import { CatalogueLogo } from './CatalogueLogo';
import { FacetGroup, LocationTree, PlaceSearch, placeLabel } from './Controls';
import type { MapFocus, MapPin } from './MapView';
import {
  NO_FILTERS,
  buildEntries,
  buildPlaceIndex,
  buildTextIndex,
  facetCounts,
  flagEmoji,
  flagsOf,
  groupEntries,
  passes,
  pointPlace,
  tierOf,
} from './model';
import type { Entry, FacetKey, Filters, Group, Tier } from './model';
import { ProbeSummary } from './ProbeSummary';
import { observeRectWithFallback } from './virtual';
import { isAddable, toSourceForm } from './toSource';

const MapView = lazy(() => import('./MapView').then((m) => ({ default: m.MapView })));

const ALL_FLAGS: readonly StreamFlag[] = [
  'geo_blocked',
  'not_24_7',
  'needs_headers',
  'http_only',
  'closed',
  'blocklisted',
  'unsupported',
  'no_metadata',
  'already_added',
];
const RADII = ['25', '50', '150', '300', '500'] as const;
type Radius = (typeof RADII)[number];
const PROBE_CAP = 50;
/** At or below this many local (city + radius) streams, say so plainly. */
const SPARSE_LOCAL = 3;

function isLocalTier(tier: Tier | null): boolean {
  return tier === 'city' || tier === 'nearby';
}

type Row =
  | { readonly t: 'tier'; readonly tier: Tier; readonly count: number; readonly open: boolean }
  | { readonly t: 'group'; readonly g: Group; readonly open: boolean }
  | { readonly t: 'stream'; readonly e: Entry };

function toggled<T>(set: ReadonlySet<T>, value: T): Set<T> {
  const next = new Set(set);
  if (next.has(value)) {
    next.delete(value);
  } else {
    next.add(value);
  }
  return next;
}

/** The page. */
export function IptvOrgPage(): JSX.Element {
  const { t } = useLingui();
  const navigate = useNavigate();
  const status = useCatalogueStatus();
  const indexQuery = useCatalogueIndex();
  const placesQuery = useCataloguePlaces();
  const sources = useSources();
  const probe = useProbeMutation();
  const flagLabel = useFlagLabel();

  const [filters, setFilters] = useState<Filters>(NO_FILTERS);
  const [place, setPlace] = useState<Place | null>(null);
  const [radius, setRadius] = useState<Radius>('150');
  const [openTiers, setOpenTiers] = useState<ReadonlySet<Tier>>(
    new Set<Tier>(['city', 'nearby', 'subdivision']),
  );
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(new Set());
  const [selected, setSelected] = useState<ReadonlySet<string>>(new Set());
  const [detailKey, setDetailKey] = useState<string | null>(null);
  const [probes, setProbes] = useState<ReadonlyMap<string, ProbeResult>>(new Map());
  const [probeError, setProbeError] = useState<string | null>(null);
  const [probing, setProbing] = useState<ReadonlySet<string>>(new Set());
  const [bulkOpen, setBulkOpen] = useState(false);
  const [focus, setFocus] = useState<MapFocus | null>(null);

  const placeIndex = useMemo(
    () => buildPlaceIndex(placesQuery.data?.places ?? []),
    [placesQuery.data],
  );
  const streams = indexQuery.data?.streams;
  const entries = useMemo(
    () => (streams === undefined ? [] : buildEntries(streams, placeIndex)),
    [streams, placeIndex],
  );
  const textIndex = useMemo(() => buildTextIndex(streams ?? []), [streams]);

  const added = useMemo(
    () => new Set((sources.data ?? []).map((s) => s.locator).filter((x): x is string => x !== undefined)),
    [sources.data],
  );
  const takenIds = useMemo(() => new Set((sources.data ?? []).map((s) => s.id)), [sources.data]);

  const textHits = useMemo((): ReadonlySet<string> | null => {
    const q = filters.q.trim();
    return q === '' ? null : new Set(textIndex.search(q).map((h) => String(h.id)));
  }, [filters.q, textIndex]);

  const radiusKm = Number(radius);
  // Entries passing every facet + the text query (before the place filter).
  const facetFiltered = useMemo(
    () => entries.filter((e) => passes(e, filters, added, textHits)),
    [entries, filters, added, textHits],
  );
  // Tiered view for the selected place.
  const tiered = useMemo(
    () =>
      facetFiltered
        .map((e) => ({ e, tier: place === null ? null : tierOf(e, place, radiusKm, placeIndex) }))
        .filter((x) => place === null || x.tier !== null),
    [facetFiltered, place, radiusKm, placeIndex],
  );
  const groups = useMemo(() => groupEntries(tiered), [tiered]);
  const localCount = useMemo(() => tiered.filter((x) => isLocalTier(x.tier)).length, [tiered]);

  const inScope = useMemo(
    () =>
      (e: Entry): boolean =>
        place === null || tierOf(e, place, radiusKm, placeIndex) !== null,
    [place, radiusKm, placeIndex],
  );
  const facet = (key: FacetKey): ReturnType<typeof facetCounts> =>
    facetCounts(entries, filters, added, textHits, key, inScope);
  const catOpts = useMemo(() => facet('cats'), [entries, filters, added, textHits, inScope]); // eslint-disable-line react-hooks/exhaustive-deps
  const langOpts = useMemo(() => facet('langs'), [entries, filters, added, textHits, inScope]); // eslint-disable-line react-hooks/exhaustive-deps
  const qualOpts = useMemo(() => facet('quals'), [entries, filters, added, textHits, inScope]); // eslint-disable-line react-hooks/exhaustive-deps
  const protoOpts = useMemo(() => facet('protos'), [entries, filters, added, textHits, inScope]); // eslint-disable-line react-hooks/exhaustive-deps
  const flagOpts = useMemo(() => facet('flags'), [entries, filters, added, textHits, inScope]); // eslint-disable-line react-hooks/exhaustive-deps

  const catName = useMemo(
    () => new Map((indexQuery.data?.categories ?? []).map((c) => [c.id, c.name])),
    [indexQuery.data],
  );
  const langName = useMemo(
    () => new Map((indexQuery.data?.languages ?? []).map((l) => [l.code, l.name])),
    [indexQuery.data],
  );

  // Map data: counts per country / subdivision / city for the facet-filtered set.
  const mapData = useMemo(() => {
    const country = new Map<string, number>();
    const sub = new Map<string, number>();
    const cityCount = new Map<string, number>();
    for (const e of facetFiltered) {
      for (const c of e.allCountries) {
        country.set(c, (country.get(c) ?? 0) + 1);
      }
      for (const s of e.allSubs) {
        sub.set(s, (sub.get(s) ?? 0) + 1);
      }
      for (const c of e.cityIds) {
        cityCount.set(c, (cityCount.get(c) ?? 0) + 1);
      }
    }
    const pins: MapPin[] = [];
    for (const [id, count] of cityCount) {
      const p = placeIndex.byId.get(id);
      if (p?.lat != null && p.lon != null) {
        pins.push({ id, name: p.name, lat: p.lat, lon: p.lon, count });
      }
    }
    return { country, sub, pins };
  }, [facetFiltered, placeIndex]);

  // Flattened, virtualised rows.
  const rows = useMemo((): Row[] => {
    const out: Row[] = [];
    let lastTier: Tier | null | undefined;
    for (const g of groups) {
      if (place !== null && g.tier !== lastTier && g.tier !== null) {
        const count = groups
          .filter((x) => x.tier === g.tier)
          .reduce((n, x) => n + x.streams.length, 0);
        out.push({ t: 'tier', tier: g.tier, count, open: openTiers.has(g.tier) });
      }
      lastTier = g.tier;
      if (g.tier !== null && !openTiers.has(g.tier)) {
        continue;
      }
      const open = expanded.has(g.key);
      out.push({ t: 'group', g, open });
      if (open) {
        for (const e of g.streams) {
          out.push({ t: 'stream', e });
        }
      }
    }
    return out;
  }, [groups, place, openTiers, expanded]);

  const scroller = useRef<HTMLDivElement | null>(null);
  // eslint-disable-next-line react-hooks/incompatible-library -- the virtualizer is used only in this component's render; nothing memoized receives it.
  const virtualizer = useVirtualizer({
    count: rows.length,
    getScrollElement: () => scroller.current,
    estimateSize: () => 64,
    overscan: 8,
    observeElementRect: observeRectWithFallback,
    initialRect: { width: 800, height: 640 },
  });

  // Reset the scroll offset when the result set changes.
  useEffect(() => {
    virtualizer.scrollToOffset(0);
  }, [place, filters, virtualizer]);

  const serverLoc = status.data?.server ?? null;
  const detailGroup = groups.find((g) => g.key === detailKey) ?? null;

  const pickPlace = (p: Place | undefined): void => {
    if (p === undefined) {
      return;
    }
    setPlace(p);
    // With no local streams, open the national tier too instead of an empty view.
    const local = facetFiltered.filter((e) => isLocalTier(tierOf(e, p, radiusKm, placeIndex))).length;
    setOpenTiers(
      new Set<Tier>(local === 0 ? ['city', 'nearby', 'subdivision', 'country'] : ['city', 'nearby', 'subdivision']),
    );
    setFocus({
      id: p.id,
      kind: p.kind,
      lat: p.lat,
      lon: p.lon,
      code: p.kind === 'subdivision' ? p.subdivision : p.country,
      radiusKm,
    });
  };

  const setFacet = (key: FacetKey, value: string): void => {
    setFilters((f) => ({ ...f, [key]: toggled(f[key] as ReadonlySet<string>, value) }));
  };

  const visibleStreams = useMemo(
    () =>
      rows.flatMap((r) =>
        r.t === 'group' ? r.g.streams.map((e) => e.s) : [],
      ),
    [rows],
  );

  const runProbe = (list: readonly IndexStream[]): void => {
    const keys = list.map((s) => s.k);
    setProbeError(null);
    setProbing((p) => new Set([...p, ...keys]));
    probe.mutate(
      { keys },
      {
        onSuccess: (results): void => {
          setProbes((m) => {
            const next = new Map(m);
            results.forEach((r, i) => {
              const k = r.k ?? keys[i];
              if (k !== undefined) {
                next.set(k, r);
              }
            });
            return next;
          });
        },
        onError: (error): void => {
          setProbeError(error.message);
        },
        onSettled: (): void => {
          setProbing((p) => new Set([...p].filter((k) => !keys.includes(k))));
        },
      },
    );
  };

  const addAsSource = (s: IndexStream): void => {
    // One code path: the same form the source dialog edits; the Sources page
    // opens its create dialog prefilled (nothing is saved until confirmed).
    void navigate('/sources', {
      state: { iptvSeed: { ts: Date.now(), form: toSourceForm(s, takenIds, 'notify', new Date(), catName) } },
    });
  };

  const selectedStreams = useMemo(
    () => (streams ?? []).filter((s) => selected.has(s.k)),
    [streams, selected],
  );

  const tierLabel = (tier: Tier): string => {
    switch (tier) {
      case 'city':
        return t`Local (this city)`;
      case 'nearby':
        return t`Nearby (within ${radius} km)`;
      case 'subdivision':
        return t`State / province`;
      case 'country':
        return t`National`;
      case 'region':
        return t`Regional`;
    }
  };

  const loading = indexQuery.isPending || placesQuery.isPending;

  return (
    <>
      <PageHeader
        title={<Trans>Browse IPTV-ORG</Trans>}
        description={
          <Trans>
            Find a public live channel by name, topic or place, check that it plays from this
            server, and add it as a linked source.
          </Trans>
        }
        actions={
          <Button asChild variant="outline">
            <Link to="/sources">
              <Trans>Back to Sources</Trans>
            </Link>
          </Button>
        }
      />

      {status.data?.state === 'disabled' ? (
        <p role="status" className="mb-4 text-sm">
          <Trans>The IPTV-ORG catalogue is disabled on this server.</Trans>
        </p>
      ) : null}
      {status.data !== undefined && status.data.errors.length > 0 ? (
        <p role="status" className="mb-4 text-sm text-muted-foreground">
          <Trans>The last catalogue sync reported an error; showing the previous snapshot.</Trans>
        </p>
      ) : null}

      {loading ? (
        <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
          <Trans>Loading the catalogue…</Trans>
        </p>
      ) : indexQuery.isError ? (
        <p role="alert" className="text-sm text-destructive">
          <Trans>Could not load the catalogue:</Trans> {indexQuery.error.message}
        </p>
      ) : (
        <div className="grid gap-6 lg:grid-cols-[22rem_1fr]">
          <div className="space-y-4">
            <PlaceSearch index={placeIndex} onPick={pickPlace} />
            <div className="flex flex-wrap items-end gap-3">
              <SelectField<Radius>
                label={t`Radius`}
                value={radius}
                options={RADII}
                optionLabel={(r): string => `${r} km`}
                onChange={setRadius}
              />
              {serverLoc !== null ? (
                <Button
                  type="button"
                  variant="outline"
                  onClick={(): void => {
                    pickPlace(pointPlace(serverLoc.name, serverLoc.lat, serverLoc.lon));
                  }}
                >
                  <LocateFixed aria-hidden="true" />
                  <Trans>Near this server</Trans>
                </Button>
              ) : null}
            </div>
            <Suspense
              fallback={
                <p className="text-sm text-muted-foreground" role="status">
                  <Trans>Loading the map…</Trans>
                </p>
              }
            >
              <MapView
                countryCounts={mapData.country}
                subdivisionCounts={mapData.sub}
                pins={mapData.pins}
                focus={focus}
                selectedId={place?.id ?? null}
                onPick={(id): void => {
                  pickPlace(placeIndex.byId.get(id));
                }}
              />
            </Suspense>
            <LocationTree entries={facetFiltered} index={placeIndex} onPick={pickPlace} />
            <FacetGroup
              title={t`Category`}
              options={catOpts}
              selected={filters.cats}
              labelOf={(v): string => catName.get(v) ?? v}
              onToggle={(v): void => {
                setFacet('cats', v);
              }}
            />
            <FacetGroup
              title={t`Language`}
              options={langOpts}
              selected={filters.langs}
              labelOf={(v): string => langName.get(v) ?? v}
              onToggle={(v): void => {
                setFacet('langs', v);
              }}
            />
            <FacetGroup
              title={t`Quality`}
              options={qualOpts}
              selected={filters.quals}
              labelOf={(v): string => v}
              onToggle={(v): void => {
                setFacet('quals', v);
              }}
            />
            <FacetGroup
              title={t`Protocol`}
              options={protoOpts}
              selected={filters.protos}
              labelOf={(v): string => v.toUpperCase()}
              onToggle={(v): void => {
                setFacet('protos', v);
              }}
            />
            <FacetGroup
              title={t`Flags`}
              options={flagOpts}
              selected={filters.flags}
              labelOf={(v): string => {
                const f = ALL_FLAGS.find((x) => x === v);
                return f === undefined ? v : flagLabel(f);
              }}
              onToggle={(v): void => {
                setFacet('flags', v);
              }}
            />
            <p className="text-xs text-muted-foreground">
              <Trans>Blocklisted channels are hidden unless you select the Blocklisted flag.</Trans>
            </p>
          </div>

          <div className="min-w-0 space-y-3">
            <div className="flex flex-wrap items-end gap-3">
              <div className="min-w-48 flex-1 space-y-1">
                <Label htmlFor="iptv-q">
                  <Trans>Search channels</Trans>
                </Label>
                <Input
                  id="iptv-q"
                  type="search"
                  value={filters.q}
                  placeholder={t`Name, network or title`}
                  onChange={(event): void => {
                    setFilters({ ...filters, q: event.target.value });
                  }}
                />
              </div>
              <Button
                type="button"
                variant="outline"
                disabled={visibleStreams.length === 0 || probe.isPending}
                onClick={(): void => {
                  runProbe(visibleStreams.slice(0, PROBE_CAP));
                }}
              >
                <Radar aria-hidden="true" />
                <Trans>Probe all (up to {PROBE_CAP})</Trans>
              </Button>
              <Button
                type="button"
                disabled={selected.size === 0}
                onClick={(): void => {
                  setBulkOpen(true);
                }}
              >
                <Plus aria-hidden="true" />
                <Trans>Add selected ({selected.size})</Trans>
              </Button>
            </div>

            <p role="status" aria-live="polite" className="text-sm text-muted-foreground">
              {place !== null ? (
                <Trans>Near {placeLabel(place, placeIndex)}: </Trans>
              ) : null}
              <Plural value={tiered.length} one="# stream" other="# streams" />{' '}
              <Plural value={groups.length} one="in # channel" other="in # channels" />
              {place !== null ? (
                <>
                  {' '}
                  <button
                    type="button"
                    className="underline"
                    onClick={(): void => {
                      setPlace(null);
                      setFocus(null);
                    }}
                  >
                    <Trans>Clear place</Trans>
                  </button>
                </>
              ) : null}
            </p>

            {place !== null && localCount <= SPARSE_LOCAL ? (
              <p role="status" data-testid="iptv-sparse" className="rounded-md border border-dashed p-3 text-sm">
                {localCount === 0 ? (
                  <Trans>
                    iptv-org lists few local streams here: none in this city or within {radius} km.
                    Showing the state and national streams instead; many real stations are not in
                    the catalogue.
                  </Trans>
                ) : (
                  <Trans>
                    iptv-org lists few local streams here: {localCount} in this city or within{' '}
                    {radius} km. State and national streams are shown below, and a larger radius
                    may find more.
                  </Trans>
                )}
              </p>
            ) : null}

            {probeError !== null ? (
              <p role="alert" className="text-sm text-destructive">
                <Trans>Probe failed:</Trans> {probeError}
              </p>
            ) : null}

            {rows.length === 0 ? (
              <p className="text-sm text-muted-foreground">
                <Trans>No streams match. Loosen a filter or choose another place.</Trans>
              </p>
            ) : (
              <div
                ref={scroller}
                className="h-[36rem] overflow-y-auto rounded-md border"
                data-testid="iptv-results"
                role="list"
                aria-label={t`Results`}
              >
                <div style={{ height: virtualizer.getTotalSize(), position: 'relative' }}>
                  {virtualizer.getVirtualItems().map((item) => {
                    const row = rows[item.index];
                    if (row === undefined) {
                      return null;
                    }
                    return (
                      <div
                        key={item.key}
                        data-index={item.index}
                        ref={virtualizer.measureElement}
                        role="listitem"
                        className="absolute start-0 top-0 w-full border-b"
                        style={{ transform: `translateY(${String(item.start)}px)` }}
                      >
                        {row.t === 'tier' ? (
                          <h3 className="m-0 bg-muted/50 text-sm font-semibold">
                            <button
                              type="button"
                              className="flex w-full items-center gap-2 px-3 py-2 text-start"
                              aria-expanded={row.open}
                              onClick={(): void => {
                                setOpenTiers((s) => toggled(s, row.tier));
                              }}
                            >
                              {row.open ? (
                                <ChevronDown className="size-4 rtl:rotate-0" aria-hidden="true" />
                              ) : (
                                <ChevronRight className="size-4 rtl:rotate-180" aria-hidden="true" />
                              )}
                              {tierLabel(row.tier)}
                              <Badge variant="outline">{row.count}</Badge>
                            </button>
                          </h3>
                        ) : row.t === 'group' ? (
                          <GroupRow
                            g={row.g}
                            open={row.open}
                            added={added}
                            selected={selected}
                            onToggleOpen={(): void => {
                              setExpanded((s) => toggled(s, row.g.key));
                            }}
                            onDetails={(): void => {
                              setDetailKey(row.g.key);
                            }}
                            onSelect={(keys, on): void => {
                              setSelected((s) => {
                                const next = new Set(s);
                                keys.forEach((k) => {
                                  if (on) {
                                    next.add(k);
                                  } else {
                                    next.delete(k);
                                  }
                                });
                                return next;
                              });
                            }}
                            probes={probes}
                          />
                        ) : (
                          <StreamRow
                            e={row.e}
                            added={added}
                            checked={selected.has(row.e.s.k)}
                            probeResult={probes.get(row.e.s.k)}
                            probing={probing.has(row.e.s.k)}
                            onCheck={(on): void => {
                              setSelected((s) => {
                                const next = new Set(s);
                                if (on) {
                                  next.add(row.e.s.k);
                                } else {
                                  next.delete(row.e.s.k);
                                }
                                return next;
                              });
                            }}
                            onProbe={(): void => {
                              runProbe([row.e.s]);
                            }}
                            onAdd={(): void => {
                              addAsSource(row.e.s);
                            }}
                          />
                        )}
                      </div>
                    );
                  })}
                </div>
              </div>
            )}

            {detailGroup !== null ? (
              <DetailPanel
                g={detailGroup}
                added={added}
                probes={probes}
                probing={probing}
                catName={catName}
                langName={langName}
                onProbe={(s): void => {
                  runProbe([s]);
                }}
                onAdd={addAsSource}
                onClose={(): void => {
                  setDetailKey(null);
                }}
              />
            ) : null}
          </div>
        </div>
      )}

      <BulkAddDialog
        open={bulkOpen}
        streams={selectedStreams}
        takenIds={takenIds}
        catNames={catName}
        onClose={(): void => {
          setBulkOpen(false);
        }}
        onDone={(okKeys): void => {
          setSelected((s) => new Set([...s].filter((k) => !okKeys.includes(k))));
        }}
      />
      <DataSourcesFooter />
    </>
  );
}

function bestAddable(g: Group, added: ReadonlySet<string>): Entry | undefined {
  return g.streams.find((e) => isAddable(e.s) && !added.has(e.s.url));
}

function GroupRow({
  g,
  open,
  added,
  selected,
  probes,
  onToggleOpen,
  onDetails,
  onSelect,
}: {
  readonly g: Group;
  readonly open: boolean;
  readonly added: ReadonlySet<string>;
  readonly selected: ReadonlySet<string>;
  readonly probes: ReadonlyMap<string, ProbeResult>;
  readonly onToggleOpen: () => void;
  readonly onDetails: () => void;
  readonly onSelect: (keys: readonly string[], on: boolean) => void;
}): JSX.Element {
  const { t } = useLingui();
  const first = g.streams[0]?.s;
  const best = bestAddable(g, added);
  const flags = new Set(g.streams.flatMap((e) => flagsOf(e.s, added)));
  const anySelected = g.streams.some((e) => selected.has(e.s.k));
  const verdicts = g.streams.map((e) => probes.get(e.s.k)).filter((x) => x !== undefined);
  const name = g.name;
  return (
    <div className="flex items-start gap-3 px-3 py-2">
      <input
        type="checkbox"
        className="mt-1 size-4"
        aria-label={t`Select ${name} for bulk add`}
        disabled={best === undefined}
        checked={anySelected}
        onChange={(event): void => {
          if (event.target.checked) {
            if (best !== undefined) {
              onSelect([best.s.k], true);
            }
          } else {
            onSelect(
              g.streams.map((e) => e.s.k),
              false,
            );
          }
        }}
      />
      {first?.logo != null ? (
        <CatalogueLogo logoKey={first.logo} name={name} className="size-8 shrink-0 object-contain" />
      ) : null}
      <div className="min-w-0 flex-1 space-y-1">
        <p className="flex flex-wrap items-center gap-2">
          <span lang="" dir="auto" className="font-medium">
            {flagEmoji(first?.country ?? null) !== '' ? (
              <span aria-hidden="true">{flagEmoji(first?.country ?? null)} </span>
            ) : null}
            {name}
          </span>
          {g.feedName !== null && g.feedName !== name ? (
            <span className="text-xs text-muted-foreground" lang="" dir="auto">
              {g.feedName}
            </span>
          ) : null}
          {g.channelName !== null ? (
            <span className="text-xs text-muted-foreground" lang="" dir="auto">
              <Trans>on {g.channelName}</Trans>
            </span>
          ) : null}
          {first?.net != null ? (
            <span className="text-xs text-muted-foreground" lang="" dir="auto">
              {first.net}
            </span>
          ) : null}
          <span className="text-xs text-muted-foreground">
            <Plural value={g.streams.length} one="# stream" other="# streams" />
          </span>
          {first?.cats.map((c) => (
            <Badge key={c} variant="secondary">
              {c}
            </Badge>
          ))}
        </p>
        <p className="flex flex-wrap gap-1">
          {[...flags].map((f) => (
            <FlagBadge key={f} flag={f} />
          ))}
          {verdicts.slice(0, 1).map((v) => (
            <VerdictBadge key={v.probed_at} verdict={v.verdict} />
          ))}
        </p>
      </div>
      <div className="flex shrink-0 gap-1">
        <Button
          type="button"
          variant="ghost"
          size="sm"
          aria-expanded={open}
          aria-label={t`Show streams of ${name}`}
          onClick={onToggleOpen}
        >
          {open ? <ChevronDown aria-hidden="true" /> : <ChevronRight className="rtl:rotate-180" aria-hidden="true" />}
          <Trans>Streams</Trans>
        </Button>
        <Button type="button" variant="outline" size="sm" aria-label={t`Details for ${name}`} onClick={onDetails}>
          <Trans>Details</Trans>
        </Button>
      </div>
    </div>
  );
}

function StreamRow({
  e,
  added,
  checked,
  probeResult,
  probing,
  onCheck,
  onProbe,
  onAdd,
}: {
  readonly e: Entry;
  readonly added: ReadonlySet<string>;
  readonly checked: boolean;
  readonly probeResult: ProbeResult | undefined;
  readonly probing: boolean;
  readonly onCheck: (on: boolean) => void;
  readonly onProbe: () => void;
  readonly onAdd: () => void;
}): JSX.Element {
  const { t } = useLingui();
  const s = e.s;
  const flags = flagsOf(s, added);
  const isAdded = flags.includes('already_added');
  const addable = isAddable(s) && !isAdded;
  const title = s.title;
  return (
    <div className="flex items-start gap-3 bg-muted/20 py-2 ps-10 pe-3 text-sm">
      <input
        type="checkbox"
        className="mt-1 size-4"
        aria-label={t`Select stream ${title} for bulk add`}
        disabled={!addable}
        checked={checked}
        onChange={(event): void => {
          onCheck(event.target.checked);
        }}
      />
      <div className="min-w-0 flex-1 space-y-1">
        <p className="flex flex-wrap items-center gap-2">
          <span lang="" dir="auto">
            {title}
          </span>
          {s.q !== null ? <Badge variant="outline">{s.q}</Badge> : null}
          <Badge variant="outline">{s.proto.toUpperCase()}</Badge>
        </p>
        <p className="flex flex-wrap gap-1">
          {flags.map((f) => (
            <FlagBadge key={f} flag={f} />
          ))}
          {probeResult !== undefined ? <VerdictBadge verdict={probeResult.verdict} /> : null}
        </p>
      </div>
      <div className="flex shrink-0 gap-1">
        <Button
          type="button"
          variant="outline"
          size="sm"
          disabled={probing}
          aria-label={t`Probe ${title}`}
          onClick={onProbe}
        >
          {probing ? <Trans>Probing…</Trans> : <Trans>Probe</Trans>}
        </Button>
        <Button
          type="button"
          size="sm"
          disabled={!addable}
          aria-label={t`Add ${title} as source`}
          onClick={onAdd}
        >
          {isAdded ? <Trans>Added</Trans> : <Trans>Add as source</Trans>}
        </Button>
      </div>
    </div>
  );
}

function DetailPanel({
  g,
  added,
  probes,
  probing,
  catName,
  langName,
  onProbe,
  onAdd,
  onClose,
}: {
  readonly g: Group;
  readonly added: ReadonlySet<string>;
  readonly probes: ReadonlyMap<string, ProbeResult>;
  readonly probing: ReadonlySet<string>;
  readonly catName: ReadonlyMap<string, string>;
  readonly langName: ReadonlyMap<string, string>;
  readonly onProbe: (s: IndexStream) => void;
  readonly onAdd: (s: IndexStream) => void;
  readonly onClose: () => void;
}): JSX.Element {
  const { t } = useLingui();
  const first = g.streams[0]?.s;
  const name = g.name;
  return (
    <section
      aria-label={t`Details for ${name}`}
      className="space-y-3 rounded-md border p-4"
      data-testid="iptv-detail"
    >
      <div className="flex items-start justify-between gap-2">
        <h2 className="text-lg font-semibold" lang="" dir="auto">
          {name}
        </h2>
        <Button type="button" variant="ghost" size="sm" onClick={onClose}>
          <Trans>Close details</Trans>
        </Button>
      </div>
      {first !== undefined ? (
        <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
          {first.alt.length > 0 ? (
            <>
              <dt className="text-muted-foreground"><Trans>Also known as</Trans></dt>
              <dd lang="" dir="auto">{first.alt.join(', ')}</dd>
            </>
          ) : null}
          {first.net !== null ? (
            <>
              <dt className="text-muted-foreground"><Trans>Network</Trans></dt>
              <dd lang="" dir="auto">{first.net}</dd>
            </>
          ) : null}
          <dt className="text-muted-foreground"><Trans>Categories</Trans></dt>
          <dd>{first.cats.map((c) => catName.get(c) ?? c).join(', ') || '—'}</dd>
          <dt className="text-muted-foreground"><Trans>Languages</Trans></dt>
          <dd>{first.langs.map((l) => langName.get(l) ?? l).join(', ') || '—'}</dd>
          <dt className="text-muted-foreground"><Trans>Broadcast area</Trans></dt>
          <dd>{first.area.join(', ') || '—'}</dd>
          {first.website !== null ? (
            <>
              <dt className="text-muted-foreground"><Trans>Website</Trans></dt>
              <dd>
                <a className="underline" href={first.website} target="_blank" rel="noreferrer">
                  {first.website}
                </a>
              </dd>
            </>
          ) : null}
        </dl>
      ) : null}
      <ul className="space-y-3">
        {g.streams.map((e) => {
          const s = e.s;
          const flags = flagsOf(s, added);
          const result = probes.get(s.k);
          const addable = isAddable(s) && !flags.includes('already_added');
          const title = s.title;
          return (
            <li key={s.k} className="space-y-2 border-t pt-2">
              <p className="flex flex-wrap items-center gap-2 text-sm">
                <span lang="" dir="auto" className="font-medium">{title}</span>
                {s.feed !== null ? <Badge variant="outline">{s.feed}</Badge> : null}
                {s.q !== null ? <Badge variant="outline">{s.q}</Badge> : null}
              </p>
              <code className="block break-all text-xs text-muted-foreground" lang="" dir="ltr">
                {s.url}
              </code>
              {s.guide != null ? (
                <p className="text-xs" data-testid="iptv-guide">
                  <Badge variant="outline">
                    <Trans>Guide listed: {s.guide.site}</Trans>
                  </Badge>{' '}
                  <span className="text-muted-foreground">
                    <Trans>
                      iptv-org publishes no feed URL for it; supply the XMLTV URL yourself.
                    </Trans>
                  </span>
                </p>
              ) : null}
              <p className="flex flex-wrap gap-1">
                {flags.map((f) => (
                  <FlagBadge key={f} flag={f} />
                ))}
              </p>
              <div className="flex gap-2">
                <Button
                  type="button"
                  variant="outline"
                  size="sm"
                  disabled={probing.has(s.k)}
                  aria-label={t`Probe stream ${title} in details`}
                  onClick={(): void => {
                    onProbe(s);
                  }}
                >
                  <Trans>Probe</Trans>
                </Button>
                <Button
                  type="button"
                  size="sm"
                  disabled={!addable}
                  aria-label={t`Add stream ${title} as source from details`}
                  onClick={(): void => {
                    onAdd(s);
                  }}
                >
                  <Trans>Add as source</Trans>
                </Button>
              </div>
              {result !== undefined ? <ProbeSummary result={result} /> : null}
            </li>
          );
        })}
      </ul>
    </section>
  );
}
