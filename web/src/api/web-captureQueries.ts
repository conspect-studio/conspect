// React Query bindings for the web-capture (VT fill) surface: capability
// discovery, submitting a capture job, and polling it to a terminal state.
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import type {
  UseMutationResult,
  UseQueryResult,
} from '@tanstack/react-query';

import {
  cancelCapture,
  createCapture,
  deleteFillRefresh,
  getCaptureCapabilities,
  getCaptureJob,
  getFillRefresh,
  putFillRefresh,
  isTerminalCaptureState,
  OperationApiError,
} from './web-capture';
import type {
  CaptureBody,
  CaptureCapabilities,
  CreateCaptureAccepted,
  FillRefreshStatus,
  JobView,
  RefreshScheduleBody,
  RequestOptions,
} from './web-capture';

export type {
  CaptureBody,
  CaptureCapabilities,
  CaptureTerms,
  CaptureView,
  CreateCaptureAccepted,
  FillRefreshStatus,
  JobView,
  RefreshScheduleBody,
} from './web-capture';
export {
  MAX_REFRESH_MINUTES,
  MIN_REFRESH_MINUTES,
  OperationApiError,
  WINDY_TERMS_VERSION,
  isTerminalCaptureState,
  windyFillId,
} from './web-capture';

/** Connection options threaded into the web-capture hooks. */
export interface WebCaptureContext {
  /** Base URL (defaults to same-origin). */
  readonly baseUrl?: string;
  /** Optional bearer token (defaults to the operator's stored token). */
  readonly token?: string;
}

function options(context: WebCaptureContext): RequestOptions {
  return {
    ...(context.baseUrl !== undefined ? { baseUrl: context.baseUrl } : {}),
    ...(context.token !== undefined ? { token: context.token } : {}),
  };
}

/** Stable React Query key for the capture-capabilities read. */
export const captureCapabilitiesKey = ['media-captures', 'capabilities'] as const;

/** Stable React Query key for one capture job's polled status. */
export function captureJobKey(jobId: number): readonly unknown[] {
  return ['media-captures', 'job', jobId] as const;
}

/**
 * `GET /api/v1/media/captures/capabilities` — whether this server is
 * provisioned to run captures, and whether Premium/URL-fetch are available.
 * Read this before offering the fill dialog so an unprovisioned server shows
 * a clear disabled message instead of letting the operator try and fail.
 */
export function useCaptureCapabilities(
  context: WebCaptureContext = {},
): UseQueryResult<CaptureCapabilities, OperationApiError> {
  return useQuery<CaptureCapabilities, OperationApiError>({
    queryKey: captureCapabilitiesKey,
    queryFn: (): Promise<CaptureCapabilities> => getCaptureCapabilities(options(context)),
  });
}

/** Submit one bounded capture job (`POST /api/v1/media/captures`). */
export function useCreateCapture(
  context: WebCaptureContext = {},
): UseMutationResult<
  CreateCaptureAccepted,
  OperationApiError,
  { readonly body: CaptureBody; readonly idempotencyKey?: string }
> {
  return useMutation({
    mutationFn: (vars): Promise<CreateCaptureAccepted> =>
      createCapture(vars.body, {
        ...options(context),
        ...(vars.idempotencyKey !== undefined ? { idempotencyKey: vars.idempotencyKey } : {}),
      }),
  });
}

/**
 * Poll one capture job's status (`GET /api/v1/media/captures/{id}`) on a
 * ~2s interval, stopping automatically once the state is terminal
 * (`succeeded`/`failed`/`cancelled`/`uncertain`).
 */
export function useCaptureJob(
  jobId: number | undefined,
  context: WebCaptureContext = {},
): UseQueryResult<JobView, OperationApiError> {
  return useQuery<JobView, OperationApiError>({
    queryKey: captureJobKey(jobId ?? -1),
    queryFn: (): Promise<JobView> => {
      if (jobId === undefined) {
        return Promise.reject(new OperationApiError('No capture job id to poll.'));
      }
      return getCaptureJob(jobId, options(context));
    },
    enabled: jobId !== undefined,
    refetchInterval: (query): number | false => {
      const state = query.state.data?.state;
      if (state !== undefined && isTerminalCaptureState(state)) {
        return false;
      }
      return 2000;
    },
  });
}

/** Cancel a queued/running capture job (`POST .../cancel`). */
export function useCancelCapture(
  context: WebCaptureContext = {},
): UseMutationResult<
  void,
  OperationApiError,
  { readonly jobId: number; readonly idempotencyKey?: string }
> {
  return useMutation({
    mutationFn: (vars): Promise<void> =>
      cancelCapture(vars.jobId, {
        ...options(context),
        ...(vars.idempotencyKey !== undefined ? { idempotencyKey: vars.idempotencyKey } : {}),
      }),
  });
}

/** Stable React Query key for one fill's scheduled refresh status. */
export function fillRefreshKey(fillId: string): readonly unknown[] {
  return ['media-fills', 'refresh', fillId] as const;
}

/**
 * Poll one fill's scheduled refresh status (`null` = no schedule): every ~5s
 * while a run is in flight, otherwise every ~30s (the scheduler's own tick).
 */
export function useFillRefresh(
  fillId: string | undefined,
  context: WebCaptureContext = {},
): UseQueryResult<FillRefreshStatus | null, OperationApiError> {
  return useQuery<FillRefreshStatus | null, OperationApiError>({
    queryKey: fillRefreshKey(fillId ?? ''),
    queryFn: (): Promise<FillRefreshStatus | null> => {
      if (fillId === undefined) return Promise.resolve(null);
      return getFillRefresh(fillId, options(context));
    },
    enabled: fillId !== undefined,
    refetchInterval: (query): number => (query.state.data?.in_flight === true ? 5000 : 30000),
  });
}

/** Install or replace a fill's scheduled refresh (`PUT .../refresh`). */
export function usePutFillRefresh(
  context: WebCaptureContext = {},
): UseMutationResult<
  FillRefreshStatus,
  OperationApiError,
  { readonly fillId: string; readonly body: RefreshScheduleBody }
> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (vars): Promise<FillRefreshStatus> => putFillRefresh(vars.fillId, vars.body, options(context)),
    onSuccess: (status, vars): void => {
      client.setQueryData(fillRefreshKey(vars.fillId), status);
    },
  });
}

/** Stop a fill's scheduled refresh (`DELETE .../refresh`). */
export function useDeleteFillRefresh(
  context: WebCaptureContext = {},
): UseMutationResult<void, OperationApiError, { readonly fillId: string }> {
  const client = useQueryClient();
  return useMutation({
    mutationFn: (vars): Promise<void> => deleteFillRefresh(vars.fillId, options(context)),
    onSuccess: (_void, vars): void => {
      client.setQueryData(fillRefreshKey(vars.fillId), null);
    },
  });
}
