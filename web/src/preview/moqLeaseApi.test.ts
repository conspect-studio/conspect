import { afterEach, describe, expect, it, vi } from 'vitest';
const mocks = vi.hoisted(() => ({ POST:vi.fn(), DELETE:vi.fn() }));
vi.mock('../api/client', () => ({createApiClient: () => mocks}));
import {createMoqLeaseApi} from './moqLeaseApi';
import {MoqLeaseRefused, type MoqLease} from './moqPlayback';
afterEach(() => vi.resetAllMocks());
describe('MoQ typed lease adapter', () => {
 it.each(['moq-capacity','forbidden','moq-codec','moq-unavailable'])('reports the typed refusal %s from the legacy code field', async (code) => {
  mocks.POST.mockResolvedValue({error:{code},response:{status:503}});
  const error = await createMoqLeaseApi('program','/api/v1/preview/program/whep').acquire(new AbortController().signal).catch((e: unknown) => e);
  expect(error).toBeInstanceOf(MoqLeaseRefused); expect((error as MoqLeaseRefused).reason).toBe(code);
 });
 it('reads the reason from the RFC 9457 problem type the server actually sends', async () => {
  // Body observed on the test server: {"type":"/problems/moq-unavailable","title":"MoQ lease request rejected","status":503}
  mocks.POST.mockResolvedValue({error:{type:'/problems/moq-unavailable',title:'MoQ lease request rejected',status:503},response:{status:503}});
  const error = await createMoqLeaseApi('program','/api/v1/preview/program/whep').acquire(new AbortController().signal).catch((e: unknown) => e);
  expect(error).toBeInstanceOf(MoqLeaseRefused); expect((error as MoqLeaseRefused).reason).toBe('moq-unavailable'); expect((error as MoqLeaseRefused).status).toBe(503);
 });
 it('names an unparseable refusal instead of throwing an untyped error', async () => {
  mocks.POST.mockResolvedValue({error:'<html>',response:{status:502}});
  const error = await createMoqLeaseApi('program','/api/v1/preview/program/whep').acquire(new AbortController().signal).catch((e: unknown) => e);
  expect(error).toBeInstanceOf(MoqLeaseRefused); expect((error as MoqLeaseRefused).reason).toBe('unknown');
 });
 it.each([
  ['a long code', {code:'x'.repeat(41)}],
  ['free text in the code', {code:'Access denied for <b>user</b>'}],
  ['an odd last segment of the type', {type:'/problems/Moq Capacity!'}],
 ])('never passes server text through as the reason: %s', async (_name, body) => {
  mocks.POST.mockResolvedValue({error:body,response:{status:503}});
  const error = await createMoqLeaseApi('program','/api/v1/preview/program/whep').acquire(new AbortController().signal).catch((e: unknown) => e);
  expect(error).toBeInstanceOf(MoqLeaseRefused); expect((error as MoqLeaseRefused).reason).toBe('unknown');
 });
 it('scopes release and checks generation acknowledgement', async () => {
  const lease: MoqLease = {lease_id:'lease',generation:7,protocol:'moq-transport-18',expires_at:'',expires_in_ms:60000,url:'',broadcast:'',audio:[],video_alternatives:[],video:{codec:'vp8',coded_width:320,coded_height:180}};
  mocks.DELETE.mockResolvedValue({data:{lease_id:'lease',generation:6,revoked:true}});
  await expect(createMoqLeaseApi('input','/api/v1/preview/inputs/camera/whep').release(lease)).rejects.toThrow('not acknowledged');
  expect(mocks.DELETE).toHaveBeenCalledWith('/api/v1/preview/inputs/{id}/moq/{lease_id}',expect.objectContaining({params:{path:{id:'camera',lease_id:'lease'},query:{generation:7}}}));
 });
});
