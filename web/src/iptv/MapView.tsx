// The offline map: MapLibre GL JS over bundled Natural Earth admin-0/admin-1
// boundaries (public domain) — NO external tiles, fonts or style URLs. Shading
// is a step expression on a per-feature stream count; city pins are clustered.
// The map is decorative-plus: everything it does is also reachable through the
// place search + result list (keyboard / screen-reader path), and the panel
// beside it lists the same counts as text.
import { useEffect, useRef, useState } from 'react';
import type { JSX } from 'react';
import { useLingui } from '@lingui/react/macro';
import type {
  GeoJSONSource,
  Map as MapLibreMap,
  MapGeoJSONFeature,
  StyleSpecification,
  ExpressionSpecification,
} from 'maplibre-gl';

/** A map location request (fly-to). */
export interface MapFocus {
  readonly id: string;
  readonly kind: 'city' | 'subdivision' | 'country' | 'region';
  readonly lat: number | null;
  readonly lon: number | null;
  /** Country / subdivision code used to find the polygon bounds. */
  readonly code: string | null;
  readonly radiusKm: number | null;
}

/** A city pin. */
export interface MapPin {
  readonly id: string;
  readonly name: string;
  readonly lat: number;
  readonly lon: number;
  readonly count: number;
}

/** Props for {@link MapView}. */
export interface MapViewProps {
  /** Stream counts keyed by ISO alpha-2. */
  readonly countryCounts: ReadonlyMap<string, number>;
  /** Stream counts keyed by subdivision code (`US-KS`). */
  readonly subdivisionCounts: ReadonlyMap<string, number>;
  readonly pins: readonly MapPin[];
  readonly focus: MapFocus | null;
  /** The selected place id (highlighted outline). */
  readonly selectedId: string | null;
  /** Called with a place id (`c/US`, `s/US-KS`, `ct/USICT`). */
  readonly onPick: (placeId: string) => void;
  /** Where the bundled boundary files live (default `<base>maps/`). */
  readonly mapsBase?: string;
}

interface Geometry {
  readonly type: string;
  readonly coordinates?: unknown;
}
interface GeoFeature {
  readonly type: 'Feature';
  readonly geometry: Geometry;
  readonly properties: Record<string, unknown>;
}
interface Geo {
  readonly type: 'FeatureCollection';
  readonly features: readonly GeoFeature[];
}

const EMPTY: Geo = { type: 'FeatureCollection', features: [] };

const STYLE: StyleSpecification = {
  version: 8,
  sources: {},
  layers: [{ id: 'bg', type: 'background', paint: { 'background-color': '#dbe7f0' } }],
};

// Step ramp on the stream count; the colours never carry meaning alone (the
// side panel prints the counts).
const SHADE = [
  'step',
  ['coalesce', ['get', 'n'], 0],
  '#f1efe9',
  1,
  '#cfe3c8',
  10,
  '#9ccb94',
  50,
  '#5fa86a',
  250,
  '#2f7d4a',
  1000,
  '#14532d',
] as ExpressionSpecification;

function bboxOf(geometry: Geometry): [number, number, number, number] | null {
  let minX = 180;
  let minY = 90;
  let maxX = -180;
  let maxY = -90;
  let any = false;
  const walk = (c: unknown): void => {
    if (Array.isArray(c) && typeof c[0] === 'number' && typeof c[1] === 'number') {
      any = true;
      minX = Math.min(minX, c[0]);
      maxX = Math.max(maxX, c[0]);
      minY = Math.min(minY, c[1]);
      maxY = Math.max(maxY, c[1]);
    } else if (Array.isArray(c)) {
      c.forEach(walk);
    }
  };
  walk(geometry.coordinates);
  return (any as boolean) ? [minX, minY, maxX, maxY] : null;
}

function isGeo(value: unknown): value is Geo {
  return (
    typeof value === 'object' &&
    value !== null &&
    'features' in value &&
    Array.isArray(value.features)
  );
}

function text(value: unknown): string {
  return typeof value === 'string' || typeof value === 'number' ? String(value) : '';
}

async function loadGeo(url: string): Promise<Geo> {
  try {
    const response = await fetch(url);
    if (!response.ok) {
      return EMPTY;
    }
    const body: unknown = await response.json();
    return isGeo(body) ? body : EMPTY;
  } catch {
    return EMPTY;
  }
}

function withCounts(geo: Geo, key: string, counts: ReadonlyMap<string, number>): Geo {
  return {
    ...geo,
    features: geo.features.map((f) => ({
      ...f,
      properties: { ...f.properties, n: counts.get(text(f.properties[key])) ?? 0 },
    })),
  };
}

/** The MapLibre map. Fails soft: if WebGL is unavailable the page still works. */
export function MapView(props: MapViewProps): JSX.Element {
  const { t } = useLingui();
  const container = useRef<HTMLDivElement | null>(null);
  const mapRef = useRef<MapLibreMap | null>(null);
  const sourceClass = useRef<typeof GeoJSONSource | null>(null);
  const geoRef = useRef<{ countries: Geo; subs: Geo }>({ countries: EMPTY, subs: EMPTY });
  const propsRef = useRef(props);
  const [ready, setReady] = useState(false);
  const [failed, setFailed] = useState(false);

  useEffect(() => {
    propsRef.current = props;
  });

  // Create the map once (lazy-load maplibre-gl + its CSS).
  useEffect(() => {
    const life = { cancelled: false };
    const el = container.current;
    if (el === null) {
      return undefined;
    }
    void (async (): Promise<void> => {
      try {
        const [maplibregl, worker] = await Promise.all([
          import('maplibre-gl'),
          // MapLibre 6 resolves its worker next to its own module; a Vite bundle
          // does not emit that file, so name the worker asset explicitly.
          import('maplibre-gl/dist/maplibre-gl-worker.mjs?url'),
          import('maplibre-gl/dist/maplibre-gl.css'),
        ]);
        maplibregl.setWorkerUrl(worker.default);
        if (life.cancelled) {
          return;
        }
        const base =
          propsRef.current.mapsBase ?? `${import.meta.env.BASE_URL}maps/`;
        const [countries, subs] = await Promise.all([
          loadGeo(`${base}countries.geojson`),
          loadGeo(`${base}subdivisions.geojson`),
        ]);
        if (life.cancelled as boolean) {
          return;
        }
        geoRef.current = { countries, subs };
        sourceClass.current = maplibregl.GeoJSONSource;
        const map = new maplibregl.Map({
          container: el,
          style: STYLE,
          center: [0, 25],
          zoom: 1.2,
          attributionControl: false,
          renderWorldCopies: false,
          dragRotate: false,
          cooperativeGestures: false,
        });
        map.touchZoomRotate.disableRotation();
        map.addControl(new maplibregl.NavigationControl({ showCompass: false }), 'top-right');
        map.addControl(
          new maplibregl.AttributionControl({
            compact: true,
            customAttribution:
              'Natural Earth (public domain) · iptv-org (Unlicense) · MapLibre',
          }),
        );
        const popup = new maplibregl.Popup({ closeButton: false, closeOnClick: false });
        map.on('load', () => {
          map.addSource('countries', { type: 'geojson', data: EMPTY });
          map.addSource('subs', { type: 'geojson', data: EMPTY });
          map.addSource('pins', {
            type: 'geojson',
            data: EMPTY,
            cluster: true,
            clusterRadius: 38,
            clusterMaxZoom: 7,
            clusterProperties: { sum: ['+', ['get', 'n']] },
          });
          map.addLayer({
            id: 'countries-fill',
            type: 'fill',
            source: 'countries',
            paint: { 'fill-color': SHADE, 'fill-opacity': 0.95 },
          });
          map.addLayer({
            id: 'countries-line',
            type: 'line',
            source: 'countries',
            paint: { 'line-color': '#6b7280', 'line-width': 0.6 },
          });
          map.addLayer({
            id: 'subs-fill',
            type: 'fill',
            source: 'subs',
            minzoom: 3,
            paint: { 'fill-color': SHADE, 'fill-opacity': 0.95 },
          });
          map.addLayer({
            id: 'subs-line',
            type: 'line',
            source: 'subs',
            minzoom: 3,
            paint: { 'line-color': '#4b5563', 'line-width': 0.5, 'line-dasharray': [2, 2] },
          });
          map.addLayer({
            id: 'selected-line',
            type: 'line',
            source: 'subs',
            filter: ['==', ['get', 'code'], ''],
            paint: { 'line-color': '#b91c1c', 'line-width': 2.5 },
          });
          map.addLayer({
            id: 'clusters',
            type: 'circle',
            source: 'pins',
            filter: ['has', 'point_count'],
            paint: {
              'circle-color': '#1d4ed8',
              'circle-opacity': 0.85,
              'circle-stroke-color': '#ffffff',
              'circle-stroke-width': 2,
              'circle-radius': ['step', ['get', 'sum'], 10, 20, 14, 100, 19, 500, 25],
            },
          });
          map.addLayer({
            id: 'pin-dots',
            type: 'circle',
            source: 'pins',
            filter: ['!', ['has', 'point_count']],
            paint: {
              'circle-color': '#1d4ed8',
              'circle-stroke-color': '#ffffff',
              'circle-stroke-width': 2,
              'circle-radius': ['step', ['get', 'n'], 6, 5, 8, 25, 11],
            },
          });
          const featureId = (f: MapGeoJSONFeature, layer: string): string | undefined => {
            const p = f.properties as Record<string, unknown>;
            if (layer === 'pin-dots') {
              return typeof p.id === 'string' ? p.id : undefined;
            }
            if (layer === 'subs-fill') {
              return typeof p.code === 'string' ? `s/${p.code}` : undefined;
            }
            return typeof p.cc === 'string' ? `c/${p.cc}` : undefined;
          };
          for (const layer of ['pin-dots', 'subs-fill', 'countries-fill']) {
            map.on('click', layer, (e) => {
              const f = e.features?.[0];
              // Topmost layer wins: a pin beats a polygon.
              if (f === undefined || (layer !== 'pin-dots' && map.queryRenderedFeatures(e.point, { layers: ['pin-dots', 'clusters'] }).length > 0)) {
                return;
              }
              if (layer === 'countries-fill' && map.getZoom() >= 3 && map.queryRenderedFeatures(e.point, { layers: ['subs-fill'] }).length > 0) {
                return;
              }
              const id = featureId(f, layer);
              if (id !== undefined) {
                propsRef.current.onPick(id);
              }
            });
            map.on('mousemove', layer, (e) => {
              const f = e.features?.[0];
              if (f === undefined) {
                return;
              }
              map.getCanvas().style.cursor = 'pointer';
              const p = f.properties as Record<string, unknown>;
              popup
                .setLngLat(e.lngLat)
                .setText(`${text(p.name)} · ${text(p.n)}`)
                .addTo(map);
            });
            map.on('mouseleave', layer, () => {
              map.getCanvas().style.cursor = '';
              popup.remove();
            });
          }
          map.on('click', 'clusters', (e) => {
            const f = e.features?.[0];
            const props: Record<string, unknown> = f?.properties ?? {};
            const id = props.cluster_id;
            const src = map.getSource('pins');
            if (f === undefined || typeof id !== 'number' || !(src instanceof maplibregl.GeoJSONSource)) {
              return;
            }
            void src.getClusterExpansionZoom(id).then((zoom) => {
              const g: unknown = f.geometry;
              const c: unknown =
                typeof g === 'object' && g !== null && 'coordinates' in g ? g.coordinates : undefined;
              if ( Array.isArray(c) && typeof c[0] === 'number' && typeof c[1] === 'number') {
                map.easeTo({ center: [c[0], c[1]], zoom });
              }
            });
          });
          mapRef.current = map;
          setReady(true);
        });
      } catch {
        // No WebGL (or maplibre failed): the search + list path remains complete.
        setFailed(true);
      }
    })();
    return (): void => {
      life.cancelled = true;
      mapRef.current?.remove();
      mapRef.current = null;
      setReady(false);
    };
  }, []);

  // Push counts + pins into the sources whenever they change.
  const { countryCounts, subdivisionCounts, pins } = props;
  useEffect(() => {
    const map = mapRef.current;
    if (map === null || !ready) {
      return;
    }
    const setData = (id: string, data: unknown): void => {
      const src = map.getSource(id);
      const ctor = sourceClass.current;
      if (ctor !== null && src instanceof ctor) {
        // maplibre's `GeoJSON.GeoJSON` parameter type; our data is valid GeoJSON.
        // eslint-disable-next-line @typescript-eslint/no-unsafe-type-assertion
        void src.setData(data as Parameters<GeoJSONSource['setData']>[0]);
      }
    };
    setData('countries', withCounts(geoRef.current.countries, 'cc', countryCounts));
    setData('subs', withCounts(geoRef.current.subs, 'code', subdivisionCounts));
    setData('pins', {
      type: 'FeatureCollection',
      features: pins.map((p) => ({
        type: 'Feature',
        geometry: { type: 'Point', coordinates: [p.lon, p.lat] },
        properties: { id: p.id, name: p.name, n: p.count },
      })),
    });
  }, [ready, countryCounts, subdivisionCounts, pins]);

  // Highlight the selected subdivision.
  const { selectedId, focus } = props;
  useEffect(() => {
    const map = mapRef.current;
    if (map === null || !ready) {
      return;
    }
    const code = selectedId?.startsWith('s/') === true ? selectedId.slice(2) : '';
    map.setFilter('selected-line', ['==', ['get', 'code'], code]);
  }, [ready, selectedId]);

  // Fly to the focused place.
  useEffect(() => {
    const map = mapRef.current;
    if (map === null || !ready || focus === null) {
      return;
    }
    if (focus.lat !== null && focus.lon !== null) {
      const radius = focus.radiusKm ?? 50;
      // ~ zoom that fits the radius in a typical panel.
      const zoom = Math.max(3, Math.min(10, Math.log2(40000 / Math.max(radius, 10)) - 0.5));
      map.flyTo({ center: [focus.lon, focus.lat], zoom, essential: false });
      return;
    }
    const geo = focus.kind === 'subdivision' ? geoRef.current.subs : geoRef.current.countries;
    const key = focus.kind === 'subdivision' ? 'code' : 'cc';
    const feature = geo.features.find((f) => f.properties[key] === focus.code);
    const box = feature ? bboxOf(feature.geometry) : null;
    if (box !== null) {
      map.fitBounds(
        [
          [box[0], box[1]],
          [box[2], box[3]],
        ],
        { padding: 24, maxZoom: 7 },
      );
    }
  }, [ready, focus]);

  return (
    <div
      className="relative h-72 w-full overflow-hidden rounded-md border bg-muted lg:h-96"
      data-testid="iptv-map"
      data-ready={ready ? 'true' : 'false'}
    >
      {/* MapLibre's CSS sets `position: relative` on its container, which overrides
          `absolute inset-0` and collapses it to 0 px tall; size it explicitly. */}
      <div ref={container} className="h-full w-full" />
      {failed ? (
        <p className="absolute inset-0 grid place-items-center p-4 text-center text-sm text-muted-foreground">
          {t`The map could not start on this device. Use the place search and the list instead — they offer everything the map does.`}
        </p>
      ) : null}
    </div>
  );
}
