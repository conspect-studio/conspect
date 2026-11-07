// BOM weather locations: `/api/v1/weather/locations`.
//
//   * `GET    /api/v1/weather/locations`       every in-scope location + latest
//     normalized data (forecast days, latest observation) and freshness.
//   * `GET    /api/v1/weather/locations/{id}`  one location.
//   * `PUT    /api/v1/weather/locations/{id}`  create (201) or replace (200).
//   * `DELETE /api/v1/weather/locations/{id}`  remove (204).
//
// A weather *source* (the on-screen graphic) is an ordinary source of kind
// `weather` created through the sources API (`POST /api/v1/sources/{id}`).
import { apiUrl, buildHeaders, OperationApiError, readProblem } from './operations';
import type { RequestOptions } from './operations';
import type { components } from './schema';

export type WeatherLocationView = components['schemas']['WeatherLocationView'];
export type WeatherLocationInput = components['schemas']['WeatherLocationInput'];
export type WeatherLocationDoc = components['schemas']['WeatherLocationDoc'];
export { OperationApiError } from './operations';
export type { RequestOptions } from './operations';

/** One normalized forecast day as served under `forecast.days[]`. */
export interface WeatherDay {
  readonly date: string;
  readonly weekday: string;
  readonly label?: string;
  readonly icon_code: number | null;
  readonly max_c: number | null;
  readonly min_c: number | null;
  readonly precis: string | null;
  readonly rain_chance: string | null;
  readonly rain_range: string | null;
}

/** The normalized forecast object. */
export interface WeatherForecast {
  readonly product: string;
  readonly aac: string;
  readonly area_name: string;
  readonly issued_utc: string;
  readonly issued_local: string | null;
  readonly expires_utc: string | null;
  readonly days: readonly WeatherDay[];
}

/** The latest observation object. */
export interface WeatherObservation {
  readonly station_name: string;
  readonly wmo_id: string;
  readonly time_local: string;
  readonly time_display: string;
  readonly air_temp_c: number | null;
  readonly apparent_temp_c: number | null;
  readonly rel_humidity_pct: number | null;
  readonly wind_dir: string | null;
  readonly wind_kmh: number | null;
  readonly gust_kmh: number | null;
  readonly pressure_hpa: number | null;
  readonly rain_since_9am_mm: number | null;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null;
}

function isView(value: unknown): value is WeatherLocationView {
  return (
    isRecord(value) &&
    isRecord(value.location) &&
    typeof value.location.id === 'string' &&
    typeof value.status === 'string'
  );
}

function isForecast(value: unknown): value is WeatherForecast {
  return isRecord(value) && typeof value.product === 'string' && Array.isArray(value.days);
}

function isObservation(value: unknown): value is WeatherObservation {
  return isRecord(value) && typeof value.station_name === 'string';
}

/** Narrow the untyped `forecast` object. */
export function forecastOf(view: WeatherLocationView): WeatherForecast | undefined {
  const f: unknown = view.forecast;
  return isForecast(f) ? f : undefined;
}

/** Narrow the untyped `observation` object. */
export function observationOf(view: WeatherLocationView): WeatherObservation | undefined {
  const o: unknown = view.observation;
  return isObservation(o) ? o : undefined;
}

const COLLECTION = '/api/v1/weather/locations';

function itemPath(id: string): string {
  return `${COLLECTION}/${encodeURIComponent(id)}`;
}

/** List every in-scope location with its latest data. */
export async function listWeatherLocations(
  options: RequestOptions = {},
): Promise<WeatherLocationView[]> {
  const response = await fetch(apiUrl(options, COLLECTION), {
    method: 'GET',
    headers: buildHeaders(options, false),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!Array.isArray(body) || !body.every(isView)) {
    throw new OperationApiError('The server returned an unexpected weather list.');
  }
  return body;
}

/** Create or replace a location. */
export async function putWeatherLocation(
  id: string,
  input: WeatherLocationInput,
  options: RequestOptions = {},
): Promise<WeatherLocationView> {
  const response = await fetch(apiUrl(options, itemPath(id)), {
    method: 'PUT',
    headers: buildHeaders(options, true),
    body: JSON.stringify(input),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  const body: unknown = await response.json();
  if (!isView(body)) {
    throw new OperationApiError('The server returned an unexpected weather location.');
  }
  return body;
}

/** Delete a location (404 counts as already gone). */
export async function deleteWeatherLocation(
  id: string,
  options: RequestOptions = {},
): Promise<void> {
  const response = await fetch(apiUrl(options, itemPath(id)), {
    method: 'DELETE',
    headers: buildHeaders(options, false),
  });
  if (!response.ok && response.status !== 404) {
    throw await readProblem(response);
  }
}

/** The two on-screen graphics a weather source can draw. */
export type WeatherTemplate = 'seven_day' | 'today';

/** The source document for a weather graphic. */
export function weatherSourceBody(
  sourceId: string,
  location: string,
  template: WeatherTemplate,
  background: 'gradient' | 'transparent',
): Record<string, unknown> {
  return { id: sourceId, kind: 'weather', location, template, background };
}

/** Create a weather source through the ordinary sources API. */
export async function createWeatherSource(
  sourceId: string,
  name: string,
  body: Record<string, unknown>,
  options: RequestOptions = {},
): Promise<void> {
  const response = await fetch(
    apiUrl(options, `/api/v1/sources/${encodeURIComponent(sourceId)}`),
    {
      method: 'POST',
      headers: buildHeaders(options, true),
      body: JSON.stringify({ name, body }),
    },
  );
  if (!response.ok) {
    throw await readProblem(response);
  }
}

/**
 * Presets verified against the public NSW products IDN11060 (forecast area
 * codes) and IDN60920 (observation station WMO ids). Other states/locations
 * are entered manually.
 */
export interface WeatherPreset {
  readonly id: string;
  readonly name: string;
  readonly aac: string;
  readonly station_wmo: string;
}

export const NSW_FORECAST_PRODUCT = 'IDN11060';
export const NSW_OBSERVATION_PRODUCT = 'IDN60920';

export const NSW_PRESETS: readonly WeatherPreset[] = [
  { id: 'sydney', name: 'Sydney', aac: 'NSW_PT131', station_wmo: '94768' },
  { id: 'parramatta', name: 'Parramatta', aac: 'NSW_PT111', station_wmo: '94764' },
  { id: 'penrith', name: 'Penrith', aac: 'NSW_PT114', station_wmo: '94763' },
  { id: 'richmond', name: 'Richmond', aac: 'NSW_PT123', station_wmo: '95753' },
  { id: 'campbelltown', name: 'Campbelltown', aac: 'NSW_PT026', station_wmo: '94757' },
  { id: 'gosford', name: 'Gosford', aac: 'NSW_PT054', station_wmo: '94782' },
  { id: 'newcastle', name: 'Newcastle', aac: 'NSW_PT102', station_wmo: '94774' },
  { id: 'katoomba', name: 'Katoomba', aac: 'NSW_PT072', station_wmo: '94744' },
  { id: 'nowra', name: 'Nowra', aac: 'NSW_PT105', station_wmo: '94750' },
  { id: 'canberra', name: 'Canberra', aac: 'NSW_PT027', station_wmo: '94926' },
  { id: 'coffs-harbour', name: 'Coffs Harbour', aac: 'NSW_PT033', station_wmo: '95729' },
  { id: 'port-macquarie', name: 'Port Macquarie', aac: 'NSW_PT119', station_wmo: '94799' },
  { id: 'tamworth', name: 'Tamworth', aac: 'NSW_PT134', station_wmo: '95762' },
  { id: 'dubbo', name: 'Dubbo', aac: 'NSW_PT047', station_wmo: '95719' },
  { id: 'wagga-wagga', name: 'Wagga Wagga', aac: 'NSW_PT150', station_wmo: '94910' },
  { id: 'albury', name: 'Albury', aac: 'NSW_PT206', station_wmo: '95896' },
];
