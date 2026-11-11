import {describe,it,expect,vi} from 'vitest';
import {fireEvent} from '@testing-library/react';
import {renderWithProviders} from '../test/render';
vi.mock('./MoqPlayer',()=>({MoqPlayer:({onUnavailable,onFatal}:{onUnavailable:()=>void;onFatal:()=>void})=><div data-testid="moq"><button onClick={onUnavailable}>unavailable</button><button onClick={onFatal}>forbidden</button></div>}));
vi.mock('./WhepPlayer',()=>({WhepPlayer:()=> <div data-testid="whep"/>}));
import {PreviewSurface} from './PreviewSurface';
const capabilities={moq:true,webrtc:true,fallback:'jpeg',scopes:{program:{whep:true},inputs:{whep:true},outputs:{whep:true}}};
describe('MoQ-first selection',()=>{
 it('opens MoQ before WHEP and only falls back on unavailable',()=>{
  const view=renderWithProviders(<PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/jpeg" label="preview" capabilities={capabilities}/>);
  expect(view.getByTestId('moq')).toBeInTheDocument(); expect(view.queryByTestId('whep')).toBeNull();
  fireEvent.click(view.getByText('unavailable')); expect(view.getByTestId('whep')).toBeInTheDocument();
 });
 it('reports typed MoQ unavailability to the monitor when JPEG is the fallback',()=>{
  const onTransportUnavailable=vi.fn();
  const noWhep={...capabilities,webrtc:false,scopes:{...capabilities.scopes,program:{whep:false}}};
  const view=renderWithProviders(<PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/jpeg" label="preview" capabilities={noWhep} onTransportUnavailable={onTransportUnavailable}/>);
  fireEvent.click(view.getByText('unavailable'));
  expect(onTransportUnavailable).toHaveBeenCalledOnce();
  expect(view.getByTestId('jpeg-rung')).toBeInTheDocument();
 });
 it('keeps authorization/capacity failure visible without WHEP',()=>{
  const view=renderWithProviders(<PreviewSurface scope="program" whepEndpoint="/api/v1/preview/program/whep" jpegPath="/jpeg" label="preview" capabilities={capabilities}/>);
  fireEvent.click(view.getByText('forbidden')); expect(view.getByRole('alert')).toBeInTheDocument(); expect(view.queryByTestId('whep')).toBeNull();
 });
});
