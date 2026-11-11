import {afterEach,describe,it,expect,vi} from 'vitest';
import {act,fireEvent} from '@testing-library/react';
import {renderWithProviders} from '../test/render';
const REASONS=['unsupported','moq-unavailable','moq-capacity','moq-codec','forbidden','session-failed','release-unacknowledged'];
vi.mock('./MoqPlayer',()=>({MoqPlayer:({onUnavailable}:{onUnavailable:(reason:string)=>void})=><div data-testid="moq">{REASONS.map((reason)=><button key={reason} onClick={()=>{onUnavailable(reason);}}>{reason}</button>)}</div>}));
vi.mock('./WhepPlayer',()=>({WhepPlayer:()=> <div data-testid="whep"/>}));
vi.mock('./useJpegPreview',()=>({useJpegStill:()=>({src:undefined,state:'loading'}),useJpegPreview:()=>undefined}));
import {MOQ_UNACKNOWLEDGED_HOLD_MS,PreviewSurface} from './PreviewSurface';
const both={moq:true,webrtc:true,fallback:'jpeg',scopes:{program:{whep:true},inputs:{whep:true},outputs:{whep:true}}};
const moqOnly={moq:true,webrtc:false,fallback:'jpeg',scopes:{program:{whep:false},inputs:{whep:false},outputs:{whep:false}}};
const seen:{transport:string;reason:string|undefined}[]=[];
const record=(transport:string,reason:string|undefined):void=>{seen.push({transport,reason});};
const surface=(capabilities:typeof both)=>{seen.length=0;return renderWithProviders(<PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/jpeg" label="preview" capabilities={capabilities} onTransport={record}/>);};
const last=()=>seen[seen.length-1];
afterEach(()=>{vi.useRealTimers();});
describe('transport ladder: MoQ, then WebRTC, then stills',()=>{
 it('opens MoQ first when the server offers it',()=>{
  const view=surface(both);
  expect(view.getByTestId('moq')).toBeInTheDocument(); expect(view.queryByTestId('whep')).toBeNull();
  expect(last()).toEqual({transport:'moq',reason:undefined});
 });
 it.each(['unsupported','moq-unavailable','moq-capacity','moq-codec','forbidden','session-failed'])('falls back to WebRTC when MoQ ends with %s, and records why',(reason)=>{
  const view=surface(both);
  fireEvent.click(view.getByText(reason));
  expect(view.getByTestId('whep')).toBeInTheDocument(); expect(view.queryByTestId('moq')).toBeNull();
  expect(last()).toEqual({transport:'webrtc',reason});
 });
 it.each(['moq-unavailable','session-failed'])('falls back to stills when MoQ ends with %s and the server has no WebRTC',(reason)=>{
  const view=surface(moqOnly);
  fireEvent.click(view.getByText(reason));
  expect(view.queryByTestId('whep')).toBeNull(); expect(view.getByTestId('jpeg-rung')).toBeInTheDocument();
  expect(last()).toEqual({transport:'jpeg',reason});
 });
 it('shows the user that MoQ was refused for authorization, on the WebRTC rung and on the stills rung',()=>{
  const live=surface(both);
  fireEvent.click(live.getByText('forbidden'));
  expect(live.getByTestId('whep')).toBeInTheDocument();
  expect(live.getByTestId('moq-fallback-note')).toHaveTextContent('this account has no access');
  live.unmount();
  const stills=surface(moqOnly);
  fireEvent.click(stills.getByText('forbidden'));
  expect(stills.getByTestId('jpeg-rung')).toBeInTheDocument();
  expect(stills.getByTestId('moq-fallback-note')).toHaveAttribute('data-reason','forbidden');
 });
 it.each(['moq-capacity','moq-codec','session-failed'])('names the reason %s to the user',(reason)=>{
  const view=surface(both);
  fireEvent.click(view.getByText(reason));
  expect(view.getByTestId('moq-fallback-note')).toHaveTextContent(reason);
 });
 it.each(['unsupported','moq-unavailable'])('adds no note for the normal state %s',(reason)=>{
  const view=surface(both);
  fireEvent.click(view.getByText(reason));
  expect(view.getByTestId('whep')).toBeInTheDocument(); expect(view.queryByTestId('moq-fallback-note')).toBeNull();
 });
 it('does not open WebRTC while an unacknowledged MoQ lease can still be held, then opens it',()=>{
  vi.useFakeTimers();
  const onLeaseState=vi.fn();
  seen.length=0;
  const view=renderWithProviders(<PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/jpeg" label="preview" capabilities={both} onTransport={record} onLeaseState={onLeaseState}/>);
  fireEvent.click(view.getByText('release-unacknowledged'));
  expect(view.queryByTestId('whep')).toBeNull(); expect(view.getByTestId('jpeg-rung')).toBeInTheDocument();
  expect(view.getByTestId('fallback-badge')).toBeInTheDocument();
  expect(last()).toEqual({transport:'jpeg',reason:'release-unacknowledged'});
  act(()=>{vi.advanceTimersByTime(MOQ_UNACKNOWLEDGED_HOLD_MS-1);});
  expect(view.queryByTestId('whep')).toBeNull(); expect(onLeaseState).not.toHaveBeenCalledWith('released');
  act(()=>{vi.advanceTimersByTime(1);});
  expect(view.getByTestId('whep')).toBeInTheDocument();
  expect(onLeaseState).toHaveBeenCalledWith('released');
  expect(last()).toEqual({transport:'webrtc',reason:'release-unacknowledged'});
 });
 it('starts at MoQ again, with no stale reason, when the surface is reused for another source',()=>{
  seen.length=0;
  const at=(id:string)=> <PreviewSurface scope="input" whepEndpoint={`/api/v1/preview/inputs/${id}/whep`} jpegPath="/jpeg" label="preview" capabilities={both} onTransport={record}/>;
  const view=renderWithProviders(at('a'));
  fireEvent.click(view.getByText('session-failed'));
  expect(view.getByTestId('whep')).toBeInTheDocument();
  view.rerender(at('b'));
  expect(view.getByTestId('moq')).toBeInTheDocument(); expect(view.queryByTestId('whep')).toBeNull();
  expect(view.queryByTestId('moq-fallback-note')).toBeNull();
  expect(last()).toEqual({transport:'moq',reason:undefined});
 });
 it('tells the monitor once that MoQ is unavailable, so a still fallback is labelled honestly',()=>{
  const onTransportUnavailable=vi.fn();
  const view=renderWithProviders(<PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/jpeg" label="preview" capabilities={moqOnly} onTransportUnavailable={onTransportUnavailable}/>);
  fireEvent.click(view.getByText('moq-unavailable'));
  expect(onTransportUnavailable).toHaveBeenCalledOnce();
  expect(view.getByTestId('jpeg-rung')).toBeInTheDocument();
 });
});
