'use strict';
// Versioned site selectors. Kept separate from process and artifact ownership.
const TERMS_VERSION = 'windy-2023-09-01';
const CLEAN = `#search,#logo-wrapper,#logo,.rhpane,#plugin-radar-plus,#bottom-credits,#contrib,
.rhpane__bottom-messages,.radsat-logos,.rhitem,.avatar-wrapper,.rhpane__top-icons,
#plugin-rhpane-top,#plugin-rhbottom,.leaflet-control-container,.maplibregl-ctrl,
.maplibregl-ctrl-attrib,.hide-on-picker-drag,.rhbottom__legend,#plugins-bottom-desktop,
#plugin-progress-bar,#note-message,.timecode,.play-pause{visibility:hidden!important}`;
function validate(view) {
  if (!view || !Number.isFinite(view.lat) || view.lat < -85 || view.lat > 85
      || !Number.isFinite(view.lon) || view.lon < -180 || view.lon > 180
      || !Number.isInteger(view.zoom) || view.zoom < 1 || view.zoom > 18
      || !['radar', 'wind'].includes(view.overlay)) throw new Error('invalid Windy view');
  return view;
}
function url(view) {
  validate(view);
  const prefix = view.overlay === 'radar' ? '-Weather-radar-radar?radar,' : '?';
  return `https://www.windy.com/${prefix}${view.lat},${view.lon},${view.zoom}`;
}
function statusExpression(overlay) {
  if (!['radar', 'wind'].includes(overlay)) throw new Error('unsupported overlay');
  return `JSON.stringify((()=>{
    const active=window.W?.store?.get?.('overlay');
    const text=document.body?.innerText||'';
    return {overlay:active||null,timestamp:window.W?.store?.get?.('timestamp')??null,noData:/No data!/i.test(text),
      loginVisible:!!document.querySelector('.rhpane__top-icons__login'),
      subscription:(()=>{try{return localStorage.getItem('settings_subscription');}catch{return null;}})(),
      time:document.querySelector('.timecode .box,.main-timecode .box')?.textContent||null,
      canvases:document.querySelectorAll('canvas').length,
      tilesLoaded:document.querySelectorAll('.leaflet-tile-loaded').length,
      tilesPending:[...document.querySelectorAll('img.leaflet-tile')].filter(i=>!i.complete||i.naturalWidth===0).length};
  })())`;
}
// Radar step timeline (spike-proven): the store's radarCalendar lists step timestamps.
const CALENDAR_EXPRESSION = `JSON.stringify((()=>{const c=window.W?.store?.get?.('radarCalendar');
  return Array.isArray(c?.timestamps)?c.timestamps.slice(-200):null;})())`;
// One-shot probe (never per poll: extra contexts can evict the map's own context).
const WEBGL_EXPRESSION = `(()=>{try{const c=document.createElement('canvas');const g=c.getContext('webgl2')||c.getContext('webgl');
  if(!g)return false;g.getExtension('WEBGL_lose_context')?.loseContext();return true;}catch{return false;}})()`;
function setTimestampExpression(t) {
  if (!Number.isSafeInteger(t) || t <= 0) throw new Error('invalid step');
  return `window.W.store.set('timestamp',${t})`;
}
module.exports = {TERMS_VERSION, CLEAN, validate, url, statusExpression, CALENDAR_EXPRESSION, WEBGL_EXPRESSION, setTimestampExpression};
