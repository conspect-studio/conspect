import { createApiClient } from '../api/client';
import { MoqUnavailable, type MoqLeaseApi } from './moqPlayback';
import type { PreviewScope } from './capabilities';

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
        // Only the explicit availability code permits a different transport.
        const error: unknown = result.error;
        if (result.response.status === 503 && typeof error === 'object' && error !== null &&
          'code' in error && error.code === 'moq-unavailable') throw new MoqUnavailable('MoQ unavailable');
        throw new Error('MoQ lease request failed');
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
