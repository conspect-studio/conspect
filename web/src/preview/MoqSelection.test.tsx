import {describe,it,expect,vi} from 'vitest';
import {fireEvent} from '@testing-library/react';
import {renderWithProviders} from '../test/render';
vi.mock('./MoqPlayer',()=>({MoqPlayer:({onUnavailable}:{onUnavailable:(reason:string)=>void})=><div data-testid="moq">{['unsupported','moq-unavailable','moq-capacity','moq-codec','forbidden','session-failed'].map((reason)=><button key={reason} onClick={()=>{onUnavailable(reason);}}>{reason}</button>)}</div>}));
vi.mock('./WhepPlayer',()=>({WhepPlayer:()=> <div data-testid="whep"/>}));
vi.mock('./useJpegPreview',()=>({useJpegStill:()=>({src:undefined,state:'loading'}),useJpegPreview:()=>undefined}));
import {PreviewSurface} from './PreviewSurface';
const both={moq:true,webrtc:true,fallback:'jpeg',scopes:{program:{whep:true},inputs:{whep:true},outputs:{whep:true}}};
const moqOnly={moq:true,webrtc:false,fallback:'jpeg',scopes:{program:{whep:false},inputs:{whep:false},outputs:{whep:false}}};
const seen:{transport:string;reason:string|undefined}[]=[];
const surface=(capabilities:typeof both)=>{seen.length=0;return renderWithProviders(<PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/jpeg" label="preview" capabilities={capabilities} onTransport={(transport,reason)=>{seen.push({transport,reason});}}/>);};
const last=()=>seen[seen.length-1];
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
 it('falls back to stills when MoQ fails and the server has no WebRTC',()=>{
  const view=surface(moqOnly);
  fireEvent.click(view.getByText('moq-unavailable'));
  expect(view.queryByTestId('whep')).toBeNull(); expect(view.getByTestId('jpeg-rung')).toBeInTheDocument();
  expect(last()).toEqual({transport:'jpeg',reason:'moq-unavailable'});
 });
 it('never leaves a dead view: no state shows only an alert',()=>{
  const view=surface(both);
  fireEvent.click(view.getByText('session-failed'));
  expect(view.queryByText('Live input preview failed.')).toBeNull();
 });
 it('tells the monitor that MoQ is unavailable, so a still fallback is labelled honestly',()=>{
  const onTransportUnavailable=vi.fn();
  const view=renderWithProviders(<PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/jpeg" label="preview" capabilities={moqOnly} onTransportUnavailable={onTransportUnavailable}/>);
  fireEvent.click(view.getByText('moq-unavailable'));
  expect(onTransportUnavailable).toHaveBeenCalledOnce();
  expect(view.getByTestId('jpeg-rung')).toBeInTheDocument();
 });
});
