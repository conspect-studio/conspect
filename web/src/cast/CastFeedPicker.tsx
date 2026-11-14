// What to cast: Program, Preview and Clean (when the run has a switcher),
// every stored layout and every source this principal can read, grouped;
// a search box narrows the list once there are more than 8 choices.
import { useId, useMemo, useState } from 'react';
import type { JSX } from 'react';
import { useLingui } from '@lingui/react/macro';
import { useQuery } from '@tanstack/react-query';

import { createApiClient } from '../api/client';
import { useLayouts } from '../api/queries';
import { getSwitcher } from '../api/switcher';
import { feedFromKey } from '../api/outputRouting';
import type { OutputVideoFeed } from '../api/outputRouting';
import { useSources } from '../resources/queries';
import { Input } from '../components/ui/input';
import { Label } from '../components/ui/label';

export interface FeedOption {
  readonly key: string;
  readonly label: string;
  readonly group: 'buses' | 'layouts' | 'sources';
}

/** The castable feeds, grouped (buses first). */
// eslint-disable-next-line react-refresh/only-export-components -- shared hook beside its picker
export function useFeedOptions(): readonly FeedOption[] {
  const { t } = useLingui();
  const sources = useSources();
  const client = useMemo(() => createApiClient(), []);
  const layouts = useLayouts(client);
  const switcher = useQuery({ queryKey: ['switcher', 'me1'], queryFn: () => getSwitcher(), retry: false });
  const hasSwitcher = switcher.isSuccess;
  return useMemo(() => {
    const options: FeedOption[] = [{ key: 'bus:program', label: t`Program`, group: 'buses' }];
    if (hasSwitcher) {
      options.push({ key: 'bus:preview', label: t`Preview`, group: 'buses' });
      options.push({ key: 'bus:clean', label: t`Clean (Program without labels and tally)`, group: 'buses' });
    }
    for (const layout of layouts.data ?? []) {
      options.push({ key: `aux:${layout.id}`, label: layout.name, group: 'layouts' });
    }
    for (const source of sources.data ?? []) {
      options.push({ key: `src:${source.id}`, label: source.name, group: 'sources' });
    }
    return options;
  }, [hasSwitcher, layouts.data, sources.data, t]);
}

export interface CastFeedPickerProps {
  readonly options: readonly FeedOption[];
  readonly value: string;
  readonly onChange: (key: string, feed: OutputVideoFeed | undefined, label: string) => void;
  readonly label: string;
  readonly disabled?: boolean;
}

/** A grouped, searchable feed select. */
export function CastFeedPicker({ options, value, onChange, label, disabled = false }: CastFeedPickerProps): JSX.Element {
  const { t } = useLingui();
  const id = useId();
  const [query, setQuery] = useState('');
  const searchable = options.length > 8;
  const needle = query.trim().toLowerCase();
  const visible = needle === '' ? options : options.filter((o) => o.label.toLowerCase().includes(needle) || o.key === value);
  const groups: readonly (readonly [FeedOption['group'], string])[] = [
    ['buses', t`Buses`],
    ['layouts', t`Layouts`],
    ['sources', t`Sources`],
  ];
  return (
    <div className="grid gap-1.5">
      <Label htmlFor={`${id}-select`}>{label}</Label>
      {searchable ? (
        <Input
          type="search"
          aria-label={t`Search feeds`}
          placeholder={t`Search feeds…`}
          value={query}
          disabled={disabled}
          onChange={(e): void => {
            setQuery(e.target.value);
          }}
        />
      ) : null}
      <select
        id={`${id}-select`}
        className="h-11 rounded-md border bg-background px-3 text-sm"
        value={value}
        disabled={disabled}
        onChange={(e): void => {
          const key = e.target.value;
          const feed = feedFromKey(key);
          const option = options.find((o) => o.key === key);
          if (option !== undefined) onChange(key, feed, option.label);
        }}
      >
        {groups.map(([group, name]) => {
          const items = visible.filter((o) => o.group === group);
          return items.length === 0 ? null : (
            <optgroup key={group} label={name}>
              {items.map((o) => (
                <option key={o.key} value={o.key}>
                  {o.label}
                </option>
              ))}
            </optgroup>
          );
        })}
      </select>
    </div>
  );
}
