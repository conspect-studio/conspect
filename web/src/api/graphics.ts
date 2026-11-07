// Graphics (CG): `/api/v1/graphics/*`.
//
//   * data sources  — typed providers (`bom_weather` today)
//   * templates     — the built-in template registry (+ sample.jpg)
//   * items         — prepared graphics: template + data binding + style
//   * channels      — playout: rundown, PVW/PGM, cue / take / clear / next
//
// Types come from the generated OpenAPI schema; this module only adds thin
// fetch wrappers in the same style as `weather.ts`.
import { apiUrl, buildHeaders, OperationApiError, readProblem } from './operations';
import type { RequestOptions } from './operations';
import type { components } from './schema';

export type ChannelView = components['schemas']['ChannelView'];
export type ItemDoc = components['schemas']['ItemDoc'];
export type ItemInput = components['schemas']['ItemInput'];
export type TemplateDoc = components['schemas']['TemplateDoc'];
export type DataSourceView = components['schemas']['DataSourceView'];
export type DataSourceInput = components['schemas']['DataSourceInput'];
export type TransitionDoc = components['schemas']['TransitionDoc'];
export type StyleDoc = components['schemas']['StyleDoc'];

const BASE = '/api/v1/graphics';

/** Query keys shared by the Graphics views (and the realtime reducer). */
export const graphicsKeys = {
  all: ['graphics'] as const,
  channels: ['graphics', 'channels'] as const,
  items: ['graphics', 'items'] as const,
  templates: ['graphics', 'templates'] as const,
  dataSources: ['graphics', 'data-sources'] as const,
};

async function call(
  method: string,
  path: string,
  body?: unknown,
  options: RequestOptions = {},
): Promise<unknown> {
  const response = await fetch(apiUrl(options, path), {
    method,
    headers: buildHeaders(options, body !== undefined),
    ...(body !== undefined ? { body: JSON.stringify(body) } : {}),
  });
  if (!response.ok) {
    throw await readProblem(response);
  }
  if (response.status === 204) {
    return undefined;
  }
  const json: unknown = await response.json();
  return json;
}

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null;
}
const hasId = (v: unknown): boolean => isRecord(v) && typeof v.id === 'string';
function isChannel(v: unknown): v is ChannelView {
  return hasId(v) && isRecord(v) && Array.isArray(v.rundown) && isRecord(v.transition);
}
function isItem(v: unknown): v is ItemDoc {
  return hasId(v) && isRecord(v) && typeof v.template === 'string';
}
function isTemplate(v: unknown): v is TemplateDoc {
  return hasId(v) && isRecord(v) && typeof v.family === 'string';
}
function isDataSource(v: unknown): v is DataSourceView {
  return isRecord(v) && hasId(v.source) && typeof v.status === 'string';
}
function one<T>(guard: (v: unknown) => v is T, what: string) {
  return (v: unknown): T => {
    if (!guard(v)) throw new OperationApiError(`The server returned an unexpected ${what}.`);
    return v;
  };
}
function many<T>(guard: (v: unknown) => v is T, what: string) {
  return (v: unknown): T[] => {
    if (!Array.isArray(v) || !v.every(guard)) {
      throw new OperationApiError(`The server returned an unexpected ${what} list.`);
    }
    return v;
  };
}

const enc = encodeURIComponent;

export const listChannels = async (): Promise<ChannelView[]> =>
  many(isChannel, 'channel')(await call('GET', `${BASE}/channels`));
export const listItems = async (): Promise<ItemDoc[]> =>
  many(isItem, 'item')(await call('GET', `${BASE}/items`));
export const listTemplates = async (): Promise<TemplateDoc[]> =>
  many(isTemplate, 'template')(await call('GET', `${BASE}/templates`));
export const listDataSources = async (): Promise<DataSourceView[]> =>
  many(isDataSource, 'data source')(await call('GET', `${BASE}/data-sources`));

export const putItem = async (id: string, input: ItemInput): Promise<ItemDoc> =>
  one(isItem, 'item')(await call('PUT', `${BASE}/items/${enc(id)}`, input));
export const deleteItem = async (id: string): Promise<void> => {
  await call('DELETE', `${BASE}/items/${enc(id)}`);
};
export const putDataSource = async (id: string, input: DataSourceInput): Promise<DataSourceView> =>
  one(isDataSource, 'data source')(await call('PUT', `${BASE}/data-sources/${enc(id)}`, input));
export const deleteDataSource = async (id: string): Promise<void> => {
  await call('DELETE', `${BASE}/data-sources/${enc(id)}`);
};

const channelOf = one(isChannel, 'channel');

export const putRundown = async (
  channel: string,
  rundown: readonly string[],
  transition?: TransitionDoc,
): Promise<ChannelView> =>
  channelOf(
    await call('PUT', `${BASE}/channels/${enc(channel)}/rundown`, {
      rundown,
      ...(transition !== undefined ? { transition } : {}),
    }),
  );

export const cue = async (channel: string, item: string): Promise<ChannelView> =>
  channelOf(await call('POST', `${BASE}/channels/${enc(channel)}/cue`, { item }));
export const take = async (channel: string, transition?: TransitionDoc): Promise<ChannelView> =>
  channelOf(
    await call('POST', `${BASE}/channels/${enc(channel)}/take`, transition ? { transition } : {}),
  );
export const clear = async (channel: string, transition?: TransitionDoc): Promise<ChannelView> =>
  channelOf(
    await call('POST', `${BASE}/channels/${enc(channel)}/clear`, transition ? { transition } : {}),
  );
export const next = async (channel: string): Promise<ChannelView> =>
  channelOf(await call('POST', `${BASE}/channels/${enc(channel)}/next`, {}));

export const itemThumbnailPath = (id: string): string => `${BASE}/items/${enc(id)}/thumbnail.jpg`;
export const templateSamplePath = (id: string): string =>
  `${BASE}/templates/${enc(id)}/sample.jpg`;

/** Create the program + preview sources for a channel through the sources API. */
export async function createChannelSources(channel: ChannelView): Promise<void> {
  const make = async (id: string, bus: 'program' | 'preview', name: string): Promise<void> => {
    const response = await fetch(apiUrl({}, `/api/v1/sources/${enc(id)}`), {
      method: 'POST',
      headers: buildHeaders({}, true),
      body: JSON.stringify({ name, body: { id, kind: 'graphics_channel', channel: channel.id, bus } }),
    });
    // 422 "already exists" is fine: the source is there.
    if (!response.ok && response.status !== 422) {
      throw await readProblem(response);
    }
  };
  await make(channel.program_source, 'program', `${channel.name} (PGM)`);
  await make(channel.preview_source, 'preview', `${channel.name} (PVW)`);
}

/** The ids of every configured source (to tell whether channel sources exist). */
export async function listSourceIds(): Promise<string[]> {
  const body = await call('GET', '/api/v1/sources');
  if (!Array.isArray(body)) return [];
  return body
    .map((s: unknown) =>
      typeof s === 'object' && s !== null && 'id' in s && typeof s.id === 'string' ? s.id : '',
    )
    .filter((id) => id !== '');
}
