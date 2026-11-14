// React Query bindings for the web-capture (VT fill) surface: capability
// discovery, submitting a capture job, and polling it to a terminal state.
import { useMutation, useQuery } from '@tanstack/react-query';
import type {
  UseMutationResult,
  UseQueryResult,
} from '@tanstack/react-query';

import {
  cancelCapture,
  createCapture,
  getCaptureCapabilities,
  getCaptureJob,
  isTerminalCaptureState,
  OperationApiError,
} from './web-capture';
import type {
  CaptureBody,
  CaptureCapabilities,
  CreateCaptureAccepted,
  JobView,
  RequestOptions,
} from './web-capture';

export type {
  CaptureBody,
  CaptureCapabilities,
  CaptureTerms,
  CaptureView,
  CreateCaptureAccepted,
  JobView,
} from './web-capture';
export { OperationApiError, WINDY_TERMS_VERSION, isTerminalCaptureState, windyFillId } from './web-capture';

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
