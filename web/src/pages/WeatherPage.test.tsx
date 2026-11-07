// WeatherPage — lists BOM locations with status/freshness, shows the latest
// data exactly as served (absent stays "—"), saves via PUT, applies NSW
// presets, and creates weather sources through the sources API.
import { afterAll, afterEach, beforeAll, describe, expect, it } from 'vitest';
import { fireEvent, screen, waitFor, within } from '@testing-library/react';
import { http, HttpResponse } from 'msw';
import { setupServer } from 'msw/node';
import { MemoryRouter } from 'react-router-dom';

import { WeatherPage } from './WeatherPage';
import { renderWithProviders } from '../test/render';

const VIEW = {
  location: {
    id: 'sydney',
    name: 'Sydney',
    forecast_product: 'IDN11060',
    aac: 'NSW_PT131',
    observation_product: 'IDN60920',
    station_wmo: '94768',
  },
  status: 'ok',
  forecast: {
    product: 'IDN11060',
    aac: 'NSW_PT131',
    area_name: 'Sydney',
    issued_utc: '2026-11-05T22:45:31Z',
    issued_local: '9:45 am Fri 6 Nov',
    expires_utc: null,
    days: [
      {
        date: '2026-11-06',
        weekday: 'Fri',
        label: 'Fri 6 Nov',
        icon_code: 1,
        max_c: 25,
        min_c: null,
        precis: 'Sunny.',
        rain_chance: '0%',
        rain_range: null,
      },
    ],
  },
  observation: {
    station_name: 'SYDNEY (OBSERVATORY HILL)',
    wmo_id: '94768',
    time_local: '2026-11-06T10:50:00+11:00',
    time_display: '10:50 am',
    air_temp_c: 22.6,
    apparent_temp_c: 21.2,
    rel_humidity_pct: 56,
    wind_dir: 'ENE',
    wind_kmh: 13,
    gust_kmh: null,
    pressure_hpa: 1018,
    rain_since_9am_mm: 0,
  },
  forecast_freshness: { fetched_at: '2026-11-05T23:00:00Z', attempted_at: null, error: null },
  observation_freshness: { fetched_at: null, attempted_at: null, error: 'ftp: connect timed out' },
  attribution: 'Source: Bureau of Meteorology',
};

const puts: { id: string; body: unknown }[] = [];
const sources: { id: string; body: unknown }[] = [];

const server = setupServer(
  http.get('*/api/v1/weather/locations', () => HttpResponse.json([VIEW])),
  http.put('*/api/v1/weather/locations/:id', async ({ params, request }) => {
    const body: unknown = await request.json();
    puts.push({ id: String(params.id), body });
    return HttpResponse.json({ ...VIEW, location: { ...VIEW.location, id: params.id } }, {
      status: 201,
    });
  }),
  http.post('*/api/v1/sources/:id', async ({ params, request }) => {
    const body: unknown = await request.json();
    sources.push({ id: String(params.id), body });
    return HttpResponse.json({ id: params.id, name: 'x', body: {} }, { status: 201 });
  }),
);

beforeAll(() => {
  server.listen();
});
afterEach(() => {
  server.resetHandlers();
  puts.length = 0;
  sources.length = 0;
});
afterAll(() => {
  server.close();
});

function renderPage(): void {
  renderWithProviders(
    <MemoryRouter>
      <WeatherPage />
    </MemoryRouter>,
  );
}

describe('WeatherPage', () => {
  it('lists a location with status, error and its latest data', async () => {
    renderPage();
    const id = await screen.findByText('sydney');
    const row = within(id.closest('tr') as HTMLElement);
    expect(row.getByText('Current')).toBeInTheDocument();
    expect(row.getByText('ftp: connect timed out')).toBeInTheDocument();
    const forecastTable = screen.getByRole('table', { name: 'Daily forecast for Sydney' });
    const day = within(forecastTable).getByText('Fri 6 Nov').closest('tr') as HTMLElement;
    const cells = within(day).getAllByRole('cell').map((c) => c.textContent);
    expect(cells).toEqual(['Fri 6 Nov', '1', '25°', '—', 'Sunny.', '0%', '—']);
    expect(screen.getAllByText('Source: Bureau of Meteorology').length).toBeGreaterThan(0);
  });

  it('fills the form from an NSW preset and saves with PUT', async () => {
    renderPage();
    await screen.findByText('sydney');
    fireEvent.change(screen.getByLabelText('NSW preset'), { target: { value: 'newcastle' } });
    expect(screen.getByLabelText('Forecast area code (aac)')).toHaveValue('NSW_PT102');
    expect(screen.getByLabelText('Station WMO id (optional)')).toHaveValue('94774');
    fireEvent.click(screen.getByRole('button', { name: 'Add location' }));
    await waitFor(() => {
      expect(puts).toHaveLength(1);
    });
    expect(puts[0]).toEqual({
      id: 'newcastle',
      body: {
        name: 'Newcastle',
        forecast_product: 'IDN11060',
        aac: 'NSW_PT102',
        observation_product: 'IDN60920',
        station_wmo: '94774',
      },
    });
  });

  it('creates a seven-day weather source through the sources API', async () => {
    renderPage();
    await screen.findByText('sydney');
    fireEvent.click(screen.getByRole('button', { name: 'Create 7-day source' }));
    await waitFor(() => {
      expect(sources).toHaveLength(1);
    });
    expect(sources[0]?.id).toBe('weather-sydney-7day');
    expect(sources[0]?.body).toMatchObject({
      body: {
        id: 'weather-sydney-7day',
        kind: 'weather',
        location: 'sydney',
        template: 'seven_day',
        background: 'gradient',
      },
    });
  });

  it('creates a transparent (alpha) source when asked', async () => {
    renderPage();
    await screen.findByText('sydney');
    fireEvent.click(screen.getByLabelText(/transparent background/));
    fireEvent.click(screen.getByRole('button', { name: 'Create today source' }));
    await waitFor(() => {
      expect(sources).toHaveLength(1);
    });
    expect(sources[0]?.id).toBe('weather-sydney-today-alpha');
    expect(sources[0]?.body).toMatchObject({
      body: { template: 'today', background: 'transparent' },
    });
  });
});
