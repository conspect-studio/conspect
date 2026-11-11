/* eslint-disable react-refresh/only-export-components -- label hooks/helpers live beside the components that use them */
// Place search (typo-tolerant, offline), facet groups and the location tree.
import { useId, useMemo, useState } from 'react';
import type { JSX } from 'react';
import { Plural, Trans, useLingui } from '@lingui/react/macro';
import { MapPin, Search } from 'lucide-react';

import type { Place } from '../api/catalogue';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';
import { flagEmoji, searchPlaces } from './model';
import type { Entry, FacetOption, PlaceIndex } from './model';

/** “Wichita, Kansas, United States” — a place with its containing places. */
export function placeLabel(place: Place, index: PlaceIndex): string {
  const parts = [place.name];
  if (place.kind === 'city') {
    const sub = place.subdivision;
    const subPlace = sub !== null ? index.byId.get(`s/${sub}`) : undefined;
    if (subPlace !== undefined) {
      parts.push(subPlace.name);
    }
  }
  if (place.kind === 'city' || place.kind === 'subdivision') {
    const c = place.country !== null ? index.byId.get(`c/${place.country}`) : undefined;
    if (c !== undefined) {
      parts.push(c.name);
    }
  }
  return parts.join(', ');
}

/** Place search: a labelled input with a result list reachable by keyboard. */
export function PlaceSearch({
  index,
  onPick,
}: {
  readonly index: PlaceIndex;
  readonly onPick: (place: Place) => void;
}): JSX.Element {
  const { t } = useLingui();
  const [query, setQuery] = useState('');
  const inputId = useId();
  const listId = useId();
  const hits = useMemo(() => searchPlaces(index, query), [index, query]);
  const kindLabel = (kind: Place['kind']): string => {
    switch (kind) {
      case 'city':
        return t`City`;
      case 'subdivision':
        return t`State / province`;
      case 'country':
        return t`Country`;
      case 'region':
        return t`Region`;
    }
  };
  return (
    <div className="space-y-1">
      <Label htmlFor={inputId}>
        <Trans>Where is it happening?</Trans>
      </Label>
      <div className="relative">
        <Search
          className="pointer-events-none absolute start-2.5 top-2.5 size-4 text-muted-foreground"
          aria-hidden="true"
        />
        <Input
          id={inputId}
          type="search"
          className="ps-8"
          value={query}
          placeholder={t`City, state, country or region`}
          autoComplete="off"
          aria-controls={listId}
          onChange={(event): void => {
            setQuery(event.target.value);
          }}
        />
      </div>
      <div id={listId} aria-live="polite">
        {query.trim() !== '' ? (
          hits.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              <Trans>No matching place.</Trans>
            </p>
          ) : (
            <>
              <p className="sr-only">
                <Plural value={hits.length} one="# place found" other="# places found" />
              </p>
              <ul className="divide-y rounded-md border" aria-label={t`Matching places`}>
                {hits.map((place) => (
                  <li key={place.id}>
                    <button
                      type="button"
                      className="flex w-full items-center gap-2 px-3 py-1.5 text-start text-sm hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
                      onClick={(): void => {
                        onPick(place);
                        setQuery('');
                      }}
                    >
                      <MapPin className="size-4 shrink-0" aria-hidden="true" />
                      <span lang="" dir="auto" className="flex-1">
                        {placeLabel(place, index)}
                      </span>
                      <span className="text-xs text-muted-foreground">{kindLabel(place.kind)}</span>
                    </button>
                  </li>
                ))}
              </ul>
            </>
          )
        ) : null}
      </div>
    </div>
  );
}

/** One multi-select facet with live counts. */
export function FacetGroup({
  title,
  options,
  selected,
  onToggle,
  labelOf,
  limit = 8,
}: {
  readonly title: string;
  readonly options: readonly FacetOption[];
  readonly selected: ReadonlySet<string>;
  readonly onToggle: (value: string) => void;
  readonly labelOf: (value: string) => string;
  readonly limit?: number;
}): JSX.Element | null {
  const [all, setAll] = useState(false);
  if (options.length === 0) {
    return null;
  }
  const shown = all ? options : options.slice(0, limit);
  return (
    <fieldset className="space-y-1">
      <legend className="text-sm font-medium">{title}</legend>
      <ul className="space-y-0.5">
        {shown.map((o) => {
          const id = `facet-${title}-${o.value}`.replace(/\s+/g, '-');
          return (
            <li key={o.value} className="flex items-center gap-2 text-sm">
              <input
                id={id}
                type="checkbox"
                className="size-4"
                checked={selected.has(o.value)}
                onChange={(): void => {
                  onToggle(o.value);
                }}
              />
              <label htmlFor={id} className="flex-1" lang="" dir="auto">
                {labelOf(o.value)}
              </label>
              <span className="text-xs tabular-nums text-muted-foreground">{o.count}</span>
            </li>
          );
        })}
      </ul>
      {options.length > limit ? (
        <button
          type="button"
          className="text-xs underline"
          aria-expanded={all}
          onClick={(): void => {
            setAll(!all);
          }}
        >
          {all ? <Trans>Show fewer</Trans> : <Trans>Show all {options.length}</Trans>}
        </button>
      ) : null}
    </fieldset>
  );
}

interface TreeNode {
  readonly place: Place;
  count: number;
  readonly children: Map<string, TreeNode>;
}

/** Location facet: region → country → subdivision → city, with counts. */
export function LocationTree({
  entries,
  index,
  onPick,
}: {
  readonly entries: readonly Entry[];
  readonly index: PlaceIndex;
  readonly onPick: (place: Place) => void;
}): JSX.Element | null {
  const { t } = useLingui();
  const roots = useMemo(() => {
    const regions = new Map<string, TreeNode>();
    const regionsOf = (country: string): string[] =>
      [...index.byId.values()]
        .filter((p) => p.kind === 'region' && p.members.includes(country))
        .map((p) => p.id);
    const node = (m: Map<string, TreeNode>, place: Place): TreeNode => {
      let n = m.get(place.id);
      if (n === undefined) {
        n = { place, count: 0, children: new Map() };
        m.set(place.id, n);
      }
      return n;
    };
    for (const e of entries) {
      const country = e.s.country;
      const cPlace = country !== null ? index.byId.get(`c/${country}`) : undefined;
      if (cPlace === undefined || country === null) {
        continue;
      }
      const regionId = regionsOf(country)[0];
      const rPlace = regionId !== undefined ? index.byId.get(regionId) : undefined;
      if (rPlace === undefined) {
        continue;
      }
      const r = node(regions, rPlace);
      r.count += 1;
      const c = node(r.children, cPlace);
      c.count += 1;
      for (const sub of e.allSubs) {
        const sPlace = index.byId.get(`s/${sub}`);
        if (sPlace === undefined) {
          continue;
        }
        const s = node(c.children, sPlace);
        s.count += 1;
        for (const cid of e.cityIds) {
          const ctPlace = index.byId.get(cid);
          if (ctPlace?.subdivision === sub) {
            node(s.children, ctPlace).count += 1;
          }
        }
      }
    }
    return [...regions.values()].sort((a, b) => b.count - a.count);
  }, [entries, index]);
  if (roots.length === 0) {
    return null;
  }
  const render = (n: TreeNode): JSX.Element => {
    const kids = [...n.children.values()].sort((a, b) => b.count - a.count);
    const label = (
      <button
        type="button"
        className="text-start underline-offset-2 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring"
        onClick={(): void => {
          onPick(n.place);
        }}
      >
        {n.place.kind === 'country' ? `${flagEmoji(n.place.country)} ` : ''}
        {n.place.name} <span className="text-xs text-muted-foreground">({n.count})</span>
      </button>
    );
    return kids.length === 0 ? (
      <li key={n.place.id}>{label}</li>
    ) : (
      <li key={n.place.id}>
        <details>
          <summary className="cursor-pointer">{label}</summary>
          <ul className="ms-4 space-y-0.5">{kids.map(render)}</ul>
        </details>
      </li>
    );
  };
  return (
    <fieldset className="space-y-1">
      <legend className="text-sm font-medium">{t`Location`}</legend>
      <ul className="max-h-56 space-y-0.5 overflow-y-auto text-sm">{roots.map(render)}</ul>
    </fieldset>
  );
}
