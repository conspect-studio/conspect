// Graphics → Data: data sources. Today the only provider is BOM weather
// (the former Weather page, moved here unchanged in behaviour).
//
// Lists the configured locations (`GET /api/v1/weather/locations`) with their
// fetch status and freshness, an add/edit form (NSW presets verified against
// IDN11060/IDN60920; anything else entered manually), a "create weather
// source" action per location (seven-day or today graphic, via the ordinary
// sources API), and the latest data in a table. Values are shown exactly as
// BOM published them; absent values render as an em dash.
import { isBackendUnavailable } from '../../api/operations';
import { useMemo, useState } from 'react';
import type { JSX, SyntheticEvent } from 'react';
import { Trans, useLingui } from '@lingui/react/macro';
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { CloudSun, Pencil, Plus, Trash2 } from 'lucide-react';

import {
  createWeatherSource,
  deleteWeatherLocation,
  forecastOf,
  listWeatherLocations,
  NSW_FORECAST_PRODUCT,
  NSW_OBSERVATION_PRODUCT,
  NSW_PRESETS,
  observationOf,
  putWeatherLocation,
  weatherSourceBody,
} from '../../api/weather';
import type {
  OperationApiError,
  WeatherLocationInput,
  WeatherLocationView,
  WeatherTemplate,
} from '../../api/weather';
import { Badge } from '../../components/ui/badge';
import type { BadgeProps } from '../../components/ui/badge';
import { Button } from '../../components/ui/button';
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '../../components/ui/card';
import { Input } from '../../components/ui/input';
import { Label } from '../../components/ui/label';
import {
  Table,
  TableBody,
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '../../components/ui/table';
import { toast } from '../../components/ui/use-toast';

const weatherKeys = { list: ['weather-locations'] as const };

interface FormState {
  id: string;
  name: string;
  forecast_product: string;
  aac: string;
  observation_product: string;
  station_wmo: string;
}

const EMPTY: FormState = {
  id: '',
  name: '',
  forecast_product: NSW_FORECAST_PRODUCT,
  aac: '',
  observation_product: '',
  station_wmo: '',
};

function fromView(view: WeatherLocationView): FormState {
  const l = view.location;
  return {
    id: l.id,
    name: l.name,
    forecast_product: l.forecast_product,
    aac: l.aac,
    observation_product: l.observation_product ?? '',
    station_wmo: l.station_wmo ?? '',
  };
}

function toInput(form: FormState): WeatherLocationInput {
  const obs = form.observation_product.trim();
  const wmo = form.station_wmo.trim();
  return {
    name: form.name.trim(),
    forecast_product: form.forecast_product.trim(),
    aac: form.aac.trim(),
    observation_product: obs === '' ? null : obs,
    station_wmo: wmo === '' ? null : wmo,
  };
}

function statusVariant(status: string): BadgeProps['variant'] {
  switch (status) {
    case 'ok':
      return 'live';
    case 'pending':
      return 'reconnecting';
    case 'expired':
      return 'stale';
    default:
      return 'offline';
  }
}

function deg(v: number | null | undefined): string {
  return v === null || v === undefined ? '—' : `${String(v)}°`;
}

function when(iso: string | null | undefined): string {
  if (iso === null || iso === undefined) {
    return '—';
  }
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString();
}

/** The weather locations page. */
export function DataView(): JSX.Element {
  const { t } = useLingui();
  const queryClient = useQueryClient();
  const locations = useQuery<WeatherLocationView[], OperationApiError>({
    queryKey: weatherKeys.list,
    queryFn: () => listWeatherLocations(),
    refetchInterval: 30_000,
  });
  const data = useMemo(() => locations.data ?? [], [locations.data]);
  const [form, setForm] = useState<FormState>(EMPTY);
  const [editing, setEditing] = useState(false);
  const [preset, setPreset] = useState('');
  const [transparent, setTransparent] = useState(false);

  const statusLabel = (status: string): string => {
    switch (status) {
      case 'ok':
        return t`Current`;
      case 'pending':
        return t`Waiting for first fetch`;
      case 'expired':
        return t`Expired`;
      default:
        return t`Fetch error`;
    }
  };

  const invalidate = (): void => {
    void queryClient.invalidateQueries({ queryKey: weatherKeys.list });
  };
  const fail = (title: string) => (error: unknown): void => {
    toast({
      title,
      description: error instanceof Error ? error.message : String(error),
      variant: 'destructive',
    });
  };

  const save = useMutation({
    mutationFn: (f: FormState) => putWeatherLocation(f.id.trim(), toInput(f)),
    onSuccess: (view): void => {
      toast({ title: t`Saved ${view.location.name}` });
      setForm(EMPTY);
      setEditing(false);
      setPreset('');
      invalidate();
    },
    onError: fail(t`Could not save location`),
  });

  const remove = useMutation({
    mutationFn: (id: string) => deleteWeatherLocation(id),
    onSuccess: invalidate,
    onError: fail(t`Could not delete location`),
  });

  const createSource = (view: WeatherLocationView, template: WeatherTemplate): void => {
    const id = `weather-${view.location.id}-${template === 'seven_day' ? '7day' : 'today'}${
      transparent ? '-alpha' : ''
    }`;
    const name =
      template === 'seven_day'
        ? t`${view.location.name} 7-day weather`
        : t`${view.location.name} today weather`;
    createWeatherSource(id, name, weatherSourceBody(id, view.location.id, template, transparent ? 'transparent' : 'gradient'))
      .then((): void => {
        toast({
          title: t`Created source ${id}`,
          description: t`Bind it to a cell on the Layouts page, or preview it on Sources.`,
        });
      })
      .catch(fail(t`Could not create source`));
  };

  const applyPreset = (id: string): void => {
    setPreset(id);
    const p = NSW_PRESETS.find((x) => x.id === id);
    if (p !== undefined) {
      setForm({
        id: editing ? form.id : p.id,
        name: p.name,
        forecast_product: NSW_FORECAST_PRODUCT,
        aac: p.aac,
        observation_product: NSW_OBSERVATION_PRODUCT,
        station_wmo: p.station_wmo,
      });
    }
  };

  const onSubmit = (e: SyntheticEvent): void => {
    e.preventDefault();
    save.mutate(form);
  };

  const field = (key: keyof FormState) => ({
    value: form[key],
    onChange: (e: { target: { value: string } }): void => {
      setForm({ ...form, [key]: e.target.value });
    },
  });

  return (
    <div>
      <p className="mb-4 text-sm text-muted-foreground">
        <Trans>
          Data sources feed graphics templates. Bureau of Meteorology weather (bom_weather) is
          fetched by the server at most every 10 minutes per product; every weather graphic shows
          “Source: Bureau of Meteorology”. More providers (RSS news) plug in here later.
        </Trans>
      </p>

      <Card className="mb-6">
        <CardHeader>
          <CardTitle>
            {editing ? <Trans>Edit location</Trans> : <Trans>Add location</Trans>}
          </CardTitle>
          <CardDescription>
            <Trans>
              Pick an NSW preset, or enter any BOM location manually: the state forecast product
              (for example IDV10753 for Victoria) and its area code (aac, for example VIC_PT042),
              plus optionally the state observations product (for example IDV60920) and the
              station WMO id. Codes are listed in the product XML on ftp://ftp.bom.gov.au/anon/gen/fwo/.
            </Trans>
          </CardDescription>
        </CardHeader>
        <CardContent>
          <form className="grid gap-4 md:grid-cols-3" onSubmit={onSubmit}>
            <div className="space-y-1">
              <Label htmlFor="weather-preset">
                <Trans>NSW preset</Trans>
              </Label>
              <select
                id="weather-preset"
                className="h-9 min-h-[44px] w-full rounded-md border border-input bg-background px-3 text-sm"
                value={preset}
                onChange={(e): void => {
                  applyPreset(e.target.value);
                }}
              >
                <option value="">{t`Manual entry`}</option>
                {NSW_PRESETS.map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name}
                  </option>
                ))}
              </select>
            </div>
            <div className="space-y-1">
              <Label htmlFor="weather-id">
                <Trans>Location id</Trans>
              </Label>
              <Input id="weather-id" required disabled={editing} {...field('id')} />
            </div>
            <div className="space-y-1">
              <Label htmlFor="weather-name">
                <Trans>Display name</Trans>
              </Label>
              <Input id="weather-name" required {...field('name')} />
            </div>
            <div className="space-y-1">
              <Label htmlFor="weather-forecast-product">
                <Trans>Forecast product</Trans>
              </Label>
              <Input id="weather-forecast-product" required {...field('forecast_product')} />
            </div>
            <div className="space-y-1">
              <Label htmlFor="weather-aac">
                <Trans>Forecast area code (aac)</Trans>
              </Label>
              <Input id="weather-aac" required {...field('aac')} />
            </div>
            <div className="space-y-1">
              <Label htmlFor="weather-obs-product">
                <Trans>Observation product (optional)</Trans>
              </Label>
              <Input id="weather-obs-product" {...field('observation_product')} />
            </div>
            <div className="space-y-1">
              <Label htmlFor="weather-wmo">
                <Trans>Station WMO id (optional)</Trans>
              </Label>
              <Input id="weather-wmo" {...field('station_wmo')} />
            </div>
            <div className="flex items-end gap-2 md:col-span-2">
              <Button type="submit" disabled={save.isPending}>
                <Plus aria-hidden="true" className="mr-1 h-4 w-4" />
                {editing ? <Trans>Save location</Trans> : <Trans>Add location</Trans>}
              </Button>
              {editing ? (
                <Button
                  type="button"
                  variant="outline"
                  onClick={(): void => {
                    setForm(EMPTY);
                    setEditing(false);
                    setPreset('');
                  }}
                >
                  <Trans>Cancel</Trans>
                </Button>
              ) : null}
            </div>
          </form>
        </CardContent>
      </Card>

      <div className="mb-3 flex items-center gap-2">
        <input
          id="weather-transparent"
          type="checkbox"
          className="h-4 w-4"
          checked={transparent}
          onChange={(e): void => {
            setTransparent(e.target.checked);
          }}
        />
        <Label htmlFor="weather-transparent">
          <Trans>Create sources with a transparent background (for layering over video)</Trans>
        </Label>
      </div>

      <Table>
        <TableCaption>
          <Trans>Configured weather locations</Trans>
        </TableCaption>
        <TableHeader>
          <TableRow>
            <TableHead>
              <Trans>Location</Trans>
            </TableHead>
            <TableHead>
              <Trans>Products</Trans>
            </TableHead>
            <TableHead>
              <Trans>Status</Trans>
            </TableHead>
            <TableHead>
              <Trans>Issued</Trans>
            </TableHead>
            <TableHead>
              <Trans>Last fetched</Trans>
            </TableHead>
            <TableHead>
              <Trans>Actions</Trans>
            </TableHead>
          </TableRow>
        </TableHeader>
        <TableBody>
          {locations.isLoading ? (
            <TableRow>
              <TableCell colSpan={6}>
                <Trans>Loading…</Trans>
              </TableCell>
            </TableRow>
          ) : null}
          {locations.isError ? (
            <TableRow>
              <TableCell colSpan={6}>
                {isBackendUnavailable(locations.error) ? (
                  <Trans>Weather is unavailable: this server does not provide it yet (backend not deployed).</Trans>
                ) : (
                  <Trans>Could not load weather locations.</Trans>
                )}
              </TableCell>
            </TableRow>
          ) : null}
          {!locations.isLoading && !locations.isError && data.length === 0 ? (
            <TableRow>
              <TableCell colSpan={6}>
                <Trans>No locations yet. Nothing is fetched until one is added.</Trans>
              </TableCell>
            </TableRow>
          ) : null}
          {data.map((view) => {
            const l = view.location;
            const f = forecastOf(view);
            const error =
              view.forecast_freshness.error ?? view.observation_freshness?.error ?? null;
            return (
              <TableRow key={l.id}>
                <TableCell>
                  <div className="font-medium">{l.name}</div>
                  <code className="text-xs text-muted-foreground">{l.id}</code>
                </TableCell>
                <TableCell className="text-xs">
                  <div>
                    {l.forecast_product} · {l.aac}
                  </div>
                  {l.observation_product !== undefined && l.observation_product !== null ? (
                    <div>
                      {l.observation_product} · {l.station_wmo}
                    </div>
                  ) : null}
                </TableCell>
                <TableCell>
                  <Badge variant={statusVariant(view.status)}>{statusLabel(view.status)}</Badge>
                  {error !== null ? (
                    <div className="mt-1 max-w-xs text-xs text-muted-foreground">{error}</div>
                  ) : null}
                </TableCell>
                <TableCell className="text-xs">{f?.issued_local ?? '—'}</TableCell>
                <TableCell className="text-xs">{when(view.forecast_freshness.fetched_at)}</TableCell>
                <TableCell>
                  <div className="flex flex-wrap gap-1">
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={(): void => {
                        createSource(view, 'seven_day');
                      }}
                    >
                      <CloudSun aria-hidden="true" className="mr-1 h-4 w-4" />
                      <Trans>Create 7-day source</Trans>
                    </Button>
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={(): void => {
                        createSource(view, 'today');
                      }}
                    >
                      <CloudSun aria-hidden="true" className="mr-1 h-4 w-4" />
                      <Trans>Create today source</Trans>
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      aria-label={t`Edit ${l.name}`}
                      onClick={(): void => {
                        setForm(fromView(view));
                        setEditing(true);
                        setPreset('');
                      }}
                    >
                      <Pencil aria-hidden="true" className="h-4 w-4" />
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      aria-label={t`Delete ${l.name}`}
                      onClick={(): void => {
                        remove.mutate(l.id);
                      }}
                    >
                      <Trash2 aria-hidden="true" className="h-4 w-4" />
                    </Button>
                  </div>
                </TableCell>
              </TableRow>
            );
          })}
        </TableBody>
      </Table>

      {data.map((view) => (
        <LatestData key={view.location.id} view={view} />
      ))}

      <Card className="mt-6">
        <CardHeader>
          <CardTitle>
            <Trans>Layering over a looped video background</Trans>
          </CardTitle>
        </CardHeader>
        <CardContent className="space-y-2 text-sm text-muted-foreground">
          <p>
            <Trans>
              “Create … source” makes an opaque graphic with a dark gradient. For a video
              background, tick “transparent background” before creating the source: its panels and
              text then carry a real alpha channel (NV12 + alpha) and everything else is clear.
            </Trans>
          </p>
          <p>
            <Trans>
              In the layout, place a media player (looping clip) cell and the weather cell over the
              same area, with the weather cell on a higher z. Set the weather cell’s opacity (for
              example 0.9) to taste. Transparent graphics composite on the CPU path; opaque ones
              also run on the GPU.
            </Trans>
          </p>
          <p>
            <Trans>
              BOM daily forecasts carry seven days and no hourly or morning/afternoon breakdown, so
              the “today” graphic shows the day’s forecast with the latest observation, never
              invented values.
            </Trans>
          </p>
        </CardContent>
      </Card>
    </div>
  );
}

function LatestData({ view }: { view: WeatherLocationView }): JSX.Element | null {
  const f = forecastOf(view);
  const o = observationOf(view);
  if (f === undefined && o === undefined) {
    return null;
  }
  return (
    <Card className="mt-6">
      <CardHeader>
        <CardTitle>{view.location.name}</CardTitle>
        <CardDescription>
          {f !== undefined ? (
            <Trans>
              {f.area_name} · {f.product} issued {f.issued_local ?? f.issued_utc}
            </Trans>
          ) : null}
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {f !== undefined ? (
          <Table>
            <TableCaption>
              <Trans>Daily forecast for {view.location.name}</Trans>
            </TableCaption>
            <TableHeader>
              <TableRow>
                <TableHead>
                  <Trans>Day</Trans>
                </TableHead>
                <TableHead>
                  <Trans>Icon code</Trans>
                </TableHead>
                <TableHead>
                  <Trans>Max</Trans>
                </TableHead>
                <TableHead>
                  <Trans>Min</Trans>
                </TableHead>
                <TableHead>
                  <Trans>Précis</Trans>
                </TableHead>
                <TableHead>
                  <Trans>Rain chance</Trans>
                </TableHead>
                <TableHead>
                  <Trans>Rain amount</Trans>
                </TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {f.days.map((d) => (
                <TableRow key={d.date}>
                  <TableCell>{d.label ?? `${d.weekday} ${d.date}`}</TableCell>
                  <TableCell>{d.icon_code ?? '—'}</TableCell>
                  <TableCell>{deg(d.max_c)}</TableCell>
                  <TableCell>{deg(d.min_c)}</TableCell>
                  <TableCell>{d.precis ?? '—'}</TableCell>
                  <TableCell>{d.rain_chance ?? '—'}</TableCell>
                  <TableCell>{d.rain_range ?? '—'}</TableCell>
                </TableRow>
              ))}
            </TableBody>
          </Table>
        ) : null}
        {o !== undefined ? (
          <p className="text-sm">
            <Trans>
              Observed {o.time_display} at {o.station_name}: {deg(o.air_temp_c)} (feels like{' '}
              {deg(o.apparent_temp_c)}), humidity{' '}
              {o.rel_humidity_pct === null ? '—' : `${String(o.rel_humidity_pct)}%`}, wind{' '}
              {o.wind_dir ?? '—'} {o.wind_kmh === null ? '' : `${String(o.wind_kmh)} km/h`},
              pressure {o.pressure_hpa === null ? '—' : `${String(o.pressure_hpa)} hPa`}.
            </Trans>
          </p>
        ) : null}
        <p className="text-xs text-muted-foreground">{view.attribution}</p>
      </CardContent>
    </Card>
  );
}
