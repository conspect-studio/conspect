import { afterEach, describe, expect, it, vi } from 'vitest';
const mocks = vi.hoisted(() => ({ POST:vi.fn(), DELETE:vi.fn() }));
vi.mock('../api/client', () => ({createApiClient: () => mocks}));
import {createMoqLeaseApi} from './moqLeaseApi';
import {MoqUnavailable, type MoqLease} from './moqPlayback';
afterEach(() => vi.resetAllMocks());
describe('MoQ typed lease adapter', () => {
 it.each(['moq-capacity','forbidden','moq-codec'])('never falls back for %s', async (code) => {
  mocks.POST.mockResolvedValue({error:{code},response:{status:503}});
  const error = await createMoqLeaseApi('program','/api/v1/preview/program/whep').acquire(new AbortController().signal).catch((e: unknown) => e);
  expect(error).toBeInstanceOf(Error); expect(error).not.toBeInstanceOf(MoqUnavailable);
 });
 it('allows only explicit unavailable', async () => {
  mocks.POST.mockResolvedValue({error:{code:'moq-unavailable'},response:{status:503}});
  await expect(createMoqLeaseApi('program','/api/v1/preview/program/whep').acquire(new AbortController().signal)).rejects.toBeInstanceOf(MoqUnavailable);
 });
 it('scopes release and checks generation acknowledgement', async () => {
  const lease: MoqLease = {lease_id:'lease',generation:7,protocol:'moq-transport-18',expires_at:'',url:'',broadcast:'',audio:[],video:{codec:'vp8',coded_width:320,coded_height:180}};
  mocks.DELETE.mockResolvedValue({data:{lease_id:'lease',generation:6,revoked:true}});
  await expect(createMoqLeaseApi('input','/api/v1/preview/inputs/camera/whep').release(lease)).rejects.toThrow('not acknowledged');
  expect(mocks.DELETE).toHaveBeenCalledWith('/api/v1/preview/inputs/{id}/moq/{lease_id}',expect.objectContaining({params:{path:{id:'camera',lease_id:'lease'},query:{generation:7}}}));
 });
});
