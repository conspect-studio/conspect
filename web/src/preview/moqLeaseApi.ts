import { createApiClient } from '../api/client';
import { MoqLeaseRefused, type MoqLeaseApi } from './moqPlayback';
import type { PreviewScope } from './capabilities';

/** The typed refusal reason of a MoQ lease problem body (`unknown` when absent). */
export function moqRefusalReason(error: unknown): string {
  if (typeof error !== 'object' || error === null) return 'unknown';
  const record = error as { readonly code?: unknown; readonly type?: unknown };
  if (typeof record.code === 'string' && record.code !== '') return record.code;
  if (typeof record.type === 'string') {
    const last = record.type.split('/').pop();
    if (last !== undefined && last !== '') return last;
  }
  return 'unknown';
}

export function createMoqLeaseApi(scope: PreviewScope, whepEndpoint: string): MoqLeaseApi {
  const id = /\/preview\/(?:inputs|outputs)\/([^/]+)\/whep$/.exec(whepEndpoint)?.[1];
  const resourceId = id ? decodeURIComponent(id) : '';
  return {
    async acquire(signal) {
      const client = createApiClient();
      const result = scope === 'program'
        ? await client.POST('/api/v1/preview/program/moq', { signal })
        : scope === 'input'
          ? await client.POST('/api/v1/preview/inputs/{id}/moq', { params: { path: { id: resourceId } }, signal })
          : await client.POST('/api/v1/preview/outputs/{id}/moq', { params: { path: { id: resourceId } }, signal });
      if (!result.data) {
        // The server answers RFC 9457 problems: the reason is the last segment of
        // `type` (`/problems/moq-unavailable`); `code` is the legacy field. Every
        // refusal is typed so the surface can name it AND try the next transport.
        throw new MoqLeaseRefused(moqRefusalReason(result.error), result.response.status);
      }
      return result.data;
    },
    async release(lease) {
      const client = createApiClient();
      const signal = AbortSignal.timeout(5000);
      const query = { generation: lease.generation };
      const result = scope === 'program'
        ? await client.DELETE('/api/v1/preview/program/moq/{lease_id}', {params:{path:{lease_id:lease.lease_id}, query},signal})
        : scope === 'input'
          ? await client.DELETE('/api/v1/preview/inputs/{id}/moq/{lease_id}', {params:{path:{id:resourceId,lease_id:lease.lease_id},query},signal})
          : await client.DELETE('/api/v1/preview/outputs/{id}/moq/{lease_id}', {params:{path:{id:resourceId,lease_id:lease.lease_id},query},signal});
      if (!result.data?.revoked || result.data.lease_id !== lease.lease_id || result.data.generation !== lease.generation) {
        throw new Error('MoQ lease revocation not acknowledged');
      }
    },
  };
}
