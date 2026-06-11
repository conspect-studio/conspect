// Config-as-code export (ADR-W015 §3).
//
// `GET /api/v1/config/export` composes the current stores (sources / outputs /
// overlays / layouts + the seeded canvas) into a `MultiviewConfig` and returns
// it as TOML. This closes the management loop honestly TODAY: edit in the UI →
// export → persist as the config file → restart picks it up.
//
// The route may not be deployed yet on an older control plane: 404/501 raise
// the typed `ConfigExportUnsupportedError` so the UI can explain rather than
// fail opaquely.
import { getStoredToken } from '../api/token';

/** The exported document plus the filename to save it under. */
export interface ConfigExport {
  /** The TOML config document. */
  readonly toml: string;
  /** The download filename. */
  readonly filename: string;
}

/** Connection options for the export call. */
export interface ConfigExportOptions {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Bearer token; falls back to the operator's stored token. */
  readonly token?: string;
}

/** The export failed for a reason other than "route not deployed". */
export class ConfigExportError extends Error {
  /** The HTTP status code, when one was returned. */
  readonly status: number | undefined;

  constructor(message: string, status?: number) {
    super(message);
    this.name = 'ConfigExportError';
    this.status = status;
  }
}

/** The backend does not serve `/api/v1/config/export` (404/501). */
export class ConfigExportUnsupportedError extends ConfigExportError {
  constructor(status: number) {
    super('The control plane does not serve config export yet.', status);
    this.name = 'ConfigExportUnsupportedError';
  }
}

/** Fetch the composed config as TOML (`GET /api/v1/config/export`). */
export async function fetchConfigExport(
  options: ConfigExportOptions = {},
): Promise<ConfigExport> {
  const headers = new Headers();
  const token = options.token ?? getStoredToken();
  if (token !== undefined && token !== '') {
    headers.set('Authorization', `Bearer ${token}`);
  }
  const response = await fetch(`${options.baseUrl ?? ''}/api/v1/config/export`, {
    method: 'GET',
    headers,
  });
  if (response.status === 404 || response.status === 501) {
    throw new ConfigExportUnsupportedError(response.status);
  }
  if (!response.ok) {
    throw new ConfigExportError(
      `Config export failed (${String(response.status)})`,
      response.status,
    );
  }
  const toml = await response.text();
  return { toml, filename: 'multiview.toml' };
}

/**
 * Trigger a browser download of the exported TOML via a blob + `a[download]`.
 * Split from the fetch so the network call stays unit-testable without a DOM.
 */
export function downloadConfigExport(result: ConfigExport): void {
  const blob = new Blob([result.toml], { type: 'application/toml' });
  const url = URL.createObjectURL(blob);
  const anchor = document.createElement('a');
  anchor.href = url;
  anchor.download = result.filename;
  document.body.append(anchor);
  anchor.click();
  anchor.remove();
  URL.revokeObjectURL(url);
}
