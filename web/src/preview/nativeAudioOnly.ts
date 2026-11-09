/** Browser distribution deliberately excludes libav WASM and its fallback loader.
 * Unsupported native audio remains unavailable; never fetch a third-party decoder.
 */
export function polyfill(): Promise<boolean> {
  return Promise.resolve(typeof AudioDecoder !== 'undefined' && typeof AudioEncoder !== 'undefined');
}
