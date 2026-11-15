'use strict';
// Versioned site selectors. Kept separate from process and artifact ownership.
const TERMS_VERSION = 'windy-2023-09-01';
// Chrome hidden in every capture. Children are listed too (`sel *`): Windy sets an
// explicit `visibility:visible` on some descendants (e.g. the hamburger bars), which
// beats an inherited `hidden` on the parent and leaks the icon back into the frame.
const CHROME_SELECTORS = ['#search', '#logo-wrapper', '#logo', '.rhpane', '#plugin-radar-plus', '#bottom-credits', '#contrib',
  '.rhpane__bottom-messages', '.radsat-logos', '.rhitem', '.avatar-wrapper', '.rhpane__top-icons',
  '#plugin-rhpane-top', '#plugin-rhbottom', '.leaflet-control-container', '.maplibregl-ctrl',
  '.maplibregl-ctrl-attrib', '.hide-on-picker-drag', '.rhbottom__legend', '#plugins-bottom-desktop',
  '#plugin-progress-bar', '#note-message', '.timecode', '.play-pause',
  '#plugin-search-input', '#plugin-startup-weather', '.rhitem__hamburger', '[class*="hamburger"]',
  '.mobile-ui', '.mobile-ui__icon', '.mobile-ui__hamburger-icon', '#mobile-avatar'];
// transition/animation off: Windy's hamburger bars carry `transition: 0.3s ease-in-out 0.5s`, so a
// bare visibility:hidden only lands after the delay and the first step captured the icon (live, mda001).
const CLEAN = `${CHROME_SELECTORS.join(',')},${CHROME_SELECTORS.map(x => x + ' *').join(',')}{visibility:hidden!important;transition:none!important;animation:none!important}`;
const STYLE_ID = 'mv-clean-style';
// Idempotent: (re)creates the style if Windy replaced head/style, and keeps it last.
const ENSURE_CLEAN_EXPRESSION = `(()=>{let s=document.getElementById(${JSON.stringify(STYLE_ID)});
  if(!s||s.textContent!==${JSON.stringify(CLEAN)}){s?.remove();s=document.createElement('style');s.id=${JSON.stringify(STYLE_ID)};
    s.textContent=${JSON.stringify(CLEAN)};}
  if(!s.isConnected||document.head.lastElementChild!==s)(document.head||document.documentElement).appendChild(s);return true;})()`;
// Returns '' when clean, else the offending selectors (comma-joined). One evaluate per step.
const CHROME_VISIBLE_EXPRESSION = `(()=>{const bad=[];const W=innerWidth,H=innerHeight;
  for(const sel of ${JSON.stringify(CHROME_SELECTORS)}){let hit=false;
    for(const e of document.querySelectorAll(sel+','+sel+' *')){
      const r=e.getBoundingClientRect();
      if(r.width<=0||r.height<=0||r.right<=0||r.bottom<=0||r.left>=W||r.top>=H)continue;
      const c=getComputedStyle(e);
      if(c.visibility==='visible'&&Number(c.opacity)>0&&c.display!=='none'){hit=true;break;}}
    if(hit)bad.push(sel);}
  return bad.join(',');})()`;
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
module.exports = {TERMS_VERSION, CLEAN, CHROME_SELECTORS, ENSURE_CLEAN_EXPRESSION, CHROME_VISIBLE_EXPRESSION, validate, url, statusExpression, CALENDAR_EXPRESSION, WEBGL_EXPRESSION, setTimestampExpression};
