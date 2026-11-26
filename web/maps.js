// GPS route maps (gps_track.py, /api/tracks, /api/logs/<id>/track). One file for both pages, in its own scope:
//  library: ① 목록 | 지도 (every drive the filters show, one colour each), ② the route on each card (an SVG, no
//           tiles) that opens 주행 지도: the drive full screen, its map above (or beside) a sheet of its
//           segment rows;
//  replay:  ③ 주행 상황 | 지도 (the drive, this log's segment solid, a car marker following playback, tap to seek) and
//           ④ the small map in the video's top-right corner, under the speed and the wheel
//           (겹쳐 보기 설정 › 지도).
// Leaflet (vendor/, BSD-2) draws the routes, markers and controls; the basemap under them is OpenFreeMap's vector
// tiles (OpenStreetMap data, no API key) drawn by MapLibre GL JS (vendor/, BSD-3) through maplibre-gl-leaflet
// (vendor/, ISC). All three and their stylesheets load only when a map is first shown. Style: Liberty (light) or
// Dark with the app's theme; 회색 지도 takes Positron or Dark without the busy layers and 글자 숨기기 hides the names and
// symbols; the attribution stays one tap away (an ⓘ in the corner, compactCredit).
(()=>{
 const library=!!document.getElementById('panel-logs'),replay=!!document.getElementById('roadView');
 if(!library&&!replay)return;
 const STYLES={light:'https://tiles.openfreemap.org/styles/liberty',dark:'https://tiles.openfreemap.org/styles/dark'};
 // Two buttons under the zoom buttons, each kept on this browser and off by default, for every map on the page but the
 // small map over the video (its own two options): 회색 지도, Positron (light) or Dark without 3D buildings, points of
 // interest, road shields, one-way arrows and house numbers; 글자 숨기기, the style's own colours (the yellow main roads
 // stay) without its symbol layers (place and road names, icons, arrows). Both on: the calm style without any text.
 const SIMPLE_STYLES={light:'https://tiles.openfreemap.org/styles/positron',dark:'https://tiles.openfreemap.org/styles/dark'};
 const SIMPLE_KEY='roadviewer-map-simple',BUSY_LAYER=/poi|shield|housenumber|transit|airport|one_?way/i;
 const BARE_KEY='roadviewer-map-nolabels';
 let simple=false,bare=false;try{simple=localStorage.getItem(SIMPLE_KEY)==='1';bare=localStorage.getItem(BARE_KEY)==='1'}catch{}
 const simpleButtons=new Set();
 // 지도 테마 (the same popover): 자동 follows the app's theme, 라이트 and 다크 fix the basemap for every map on the page
 // but the small map over the video (its own 색상 setting); kept on this browser.
 const TONE_KEY='roadviewer-map-tone';let mapTone='auto';try{const saved=localStorage.getItem(TONE_KEY);if(saved==='light'||saved==='dark')mapTone=saved}catch{}
 let redrawMiniCar=()=>{};
 // The same vehicle silhouette as the road view (12 × 22, with a front windshield).
 const CAR_KEY='roadviewer-map-car-icon';let mapCar='vehicle';try{if(localStorage.getItem(CAR_KEY)==='arrow')mapCar='arrow'}catch{}
 const CAR_COLORS={auto:'자동',white:'흰색',mint:'민트',blue:'파랑',cyan:'하늘',violet:'보라',rose:'분홍',amber:'주황',red:'빨강'};
 const CAR_COLOR_KEY='roadviewer-map-car-color';let mapCarColor='auto';try{const saved=localStorage.getItem(CAR_COLOR_KEY);if(Object.hasOwn(CAR_COLORS,saved))mapCarColor=saved}catch{}
 const carColorHex=()=>mapCarColor==='white'?'#fff':PASTEL[mapCarColor];
 const carColorStyle=()=>mapCarColor==='auto'?'':`--map-car-color:${carColorHex()};`;
 const carColorControl=()=>'<span>차량 아이콘 색상</span><div class="map-car-colors" role="group" aria-label="차량 아이콘 색상">'+Object.entries(CAR_COLORS).map(([key,name])=>`<button type="button" data-map-color="${key}" aria-pressed="false" title="${name}"${key==='auto'?'':` style="--swatch:${key==='white'?'#fff':PASTEL[key]}"`}><i aria-hidden="true"></i><span>${name}</span></button>`).join('')+'</div>';
 function syncCarChoices(){for(const wrap of simpleButtons)showCarChoice(wrap.rvPop);for(const id of ['mapCarIcon','mapCarColors']){const root=document.getElementById(id);if(root)showCarChoice(root)}}
 function setMapCarColor(color){
  if(!Object.hasOwn(CAR_COLORS,color)||color===mapCarColor)return;mapCarColor=color;try{localStorage.setItem(CAR_COLOR_KEY,color)}catch{}
  for(const body of document.querySelectorAll('.map-car-body')){body.dataset.carColor=color;if(color==='auto')body.style.removeProperty('--map-car-color');else body.style.setProperty('--map-car-color',carColorHex())}
  syncCarChoices();redrawMiniCar();
 }
 const carArt=kind=>kind==='arrow'?'<svg viewBox="0 0 24 24" width="22" height="22" aria-hidden="true"><path d="M12 3 19 20 12 16 5 20Z"/></svg>':'<svg viewBox="0 0 24 24" width="44" height="44" aria-hidden="true"><rect class="map-vehicle-shell" x="6" y="1" width="12" height="22" rx="3"/><rect class="map-vehicle-glass" x="8" y="4" width="8" height="3" rx="1"/></svg>';
 function showCarChoice(root){for(const choice of root.querySelectorAll('button[data-map-car]'))choice.setAttribute('aria-pressed',String(choice.dataset.mapCar===mapCar));for(const choice of root.querySelectorAll('button[data-map-color]'))choice.setAttribute('aria-pressed',String(choice.dataset.mapColor===mapCarColor))}
 function setMapCar(kind){
  if(!['vehicle','arrow'].includes(kind)||kind===mapCar)return;mapCar=kind;try{localStorage.setItem(CAR_KEY,kind)}catch{}
  for(const body of document.querySelectorAll('.map-car-body')){body.dataset.carIcon=kind;body.innerHTML=carArt(kind)}
  syncCarChoices();redrawMiniCar();
 }
 const link=(href,text)=>`<a href="${href}" target="_blank" rel="noopener">${text}</a>`;
 const ATTRIBUTION=`${link('https://openfreemap.org','OpenFreeMap')} ${link('https://www.openmaptiles.org/','© OpenMapTiles')} Data from ${link('https://www.openstreetmap.org/copyright','OpenStreetMap')}`;
 const SHORT_ATTRIBUTION=`${link('https://openfreemap.org','OpenFreeMap')} ${link('https://www.openmaptiles.org/','© OpenMapTiles')} ${link('https://www.openstreetmap.org/copyright','© OSM')}`;
 const PALETTE=['mint','blue','amber','violet','rose','cyan','lime','coral','indigo','yellow','red','slate'];
 const reduced=()=>matchMedia('(prefers-reduced-motion: reduce)').matches;
 const theme=()=>window.RoadViewerTheme?.theme?.()==='light'?'light':'dark';
 const css=name=>getComputedStyle(document.documentElement).getPropertyValue(name).trim();
 // Drive lines on the all-drives map: the tag colour picker's pastel tones in both themes, each over a thin darker
 // casing so a light pastel still reads on the light basemap.
 const PASTEL={mint:'#65dfb9',blue:'#8cbcff',amber:'#f2c16f',violet:'#c2a1ff',rose:'#f2a4bd',cyan:'#75d9ef',lime:'#b9df78',coral:'#ffad92',indigo:'#9faaff',yellow:'#f5df7a',red:'#ff8c91',slate:'#acbbcc'};
 const tagColor=i=>PASTEL[PALETTE[i%PALETTE.length]];
 const casingColor=()=>theme()==='light'?'rgba(31,41,55,.42)':'rgba(0,0,0,.55)';
 // The stylesheet this page was served with names the asset folder and its version.
 const styleLink=document.head.querySelector('link[href*="style.css"]'),assetBase=styleLink?new URL('./',styleLink.href):new URL(replay?'../../assets/':'assets/',location.href),version=styleLink?new URL(styleLink.href).search:'';
 // Session-scoped memory cache kept across SPA page transitions (pageScope shadows `window`, not `globalThis`).
 const sessionCache=globalThis.__rvTrackSessionCache??={tracksTag:null,tracksText:null,tracks:null,tracksSignature:null,logTracks:new Map(),maxLogTracks:30};
 function getCachedLogTrack(logId){
  const entry=sessionCache.logTracks.get(logId);if(!entry)return null;
  sessionCache.logTracks.delete(logId);sessionCache.logTracks.set(logId,entry);return entry;
 }
 function setCachedLogTrack(logId,entry){
  const keys=new Set([logId]);
  for(const seg of entry.value?.segments||[])if(seg?.id)keys.add(seg.id);
  for(const k of keys){sessionCache.logTracks.delete(k);sessionCache.logTracks.set(k,entry)}
  while(sessionCache.logTracks.size>sessionCache.maxLogTracks){
   const oldest=sessionCache.logTracks.keys().next().value;if(oldest===undefined)break;
   sessionCache.logTracks.delete(oldest);
  }
 }
 function evictLogTrack(logId){
  const entry=sessionCache.logTracks.get(logId);if(!entry)return;
  for(const [k,v] of [...sessionCache.logTracks])if(v===entry)sessionCache.logTracks.delete(k);
 }
 // Load the vendor assets once, including overlapping replay initialization.
 const mapAssets=globalThis.__rvMapAssets??=new Map();
 function loadMapAsset(id,name,tag){
  if(mapAssets.has(id))return mapAssets.get(id);
  const url=new URL('vendor/'+name+version,assetBase).href;
  const pending=new Promise((resolve,reject)=>{
   const el=document.createElement(tag);el.id=id;
   if(tag==='script')el.src=url;else{el.rel='stylesheet';el.href=url}
   el.onload=()=>resolve();el.onerror=()=>{el.remove();reject(Error('지도를 불러오지 못했습니다.'))};
   document.head.append(el);
  }).catch(error=>{mapAssets.delete(id);throw error});
  mapAssets.set(id,pending);return pending;
 }
 const loadMapLibre=()=>Promise.all([
  loadMapAsset('rvMaplibreCss','maplibre-gl.css','link'),
  window.maplibregl?.Map||loadMapAsset('rvMaplibreJs','maplibre-gl.js','script'),
 ]).then(()=>window.maplibregl);
 let leaflet=null;
 function loadLeaflet(){
  return leaflet??=Promise.all([
   loadMapLibre(),loadMapAsset('rvLeafletCss','leaflet.css','link'),
   window.L?.map||loadMapAsset('rvLeafletJs','leaflet.js','script'),
  ]).then(()=>window.L.maplibreGL||loadMapAsset('rvMaplibreLeafletJs','leaflet-maplibre-gl.js','script'))
   .then(()=>window.L).catch(error=>{leaflet=null;throw error});
 }
 // A map with this app's look: no Leaflet prefix, the OpenFreeMap style of the current theme, zoom buttons only where
 // asked. At most two exist on a page (library: the list's map and 주행 지도; replay: the 지도 tab and the small map);
 // MapLibre draws only when its Leaflet map moves or its style changes, so a hidden one costs no frames.
 const maps=new Set();
 // Asked once; the probe's context is released right away (phones allow only a few).
 let hasWebgl=null;
 const webgl=()=>hasWebgl??=(()=>{try{const canvas=document.createElement('canvas'),gl=canvas.getContext('webgl2')||canvas.getContext('webgl');gl?.getExtension('WEBGL_lose_context')?.loseContext();return !!gl}catch{return false}})();
 const GL_SIDE=4000;  // the drawn basemap canvas's longest side, in device pixels, for the margins below
 // tone: a function giving 'light' or 'dark' to override the app theme; labels: a function, false to hide the style's
 // place and road names (its symbol layers); grey: a function, true for the grey style (tone, labels, grey: the small
 // map's own choices; other maps follow the page's 회색 지도 and 글자 숨기기 buttons). margin: the share of the map's
 // size drawn beyond each edge, by the basemap (the plugin's padding) and the route lines (the SVG renderer's padding),
 // for the maps that follow the car by scrolling what is drawn (the replay's); the others keep the defaults (0.1).
 // A tap beside a route line counts as a tap on it: the line nearest the tap within a finger's reach (24px; 8px with a
 // mouse). The lines are 4-5px wide, and on a phone a tap had to land exactly on one, or it cleared the selection.
 const nearestLine=(lines,event)=>{
  const reach=event.originalEvent?.pointerType==='mouse'?8:24,lp=event.layerPoint;let best=null;
  for(const line of lines){
   if(!line._map)continue;
   const b=line._pxBounds;
   if(b&&b.isValid()){
    const pad=best?Math.min(reach,best.distance):reach;
    if(lp.x<b.min.x-pad||lp.x>b.max.x+pad||lp.y<b.min.y-pad||lp.y>b.max.y+pad)continue;
   }
   const at=line.closestLayerPoint(lp);
   if(at&&at.distance<=reach&&(!best||at.distance<best.distance))best={distance:at.distance,line};
  }
  return best?.line||null;
 };
 function attachMapToPage(L,map,container,{zoom=true,tone=null,labels=null,grey=null}={}){
  const tiles=()=>tone?tone()||theme():mapTone==='auto'?theme():mapTone,plain=()=>labels?!labels():bare,isGrey=()=>grey?grey():simple,tileKey=()=>tiles()+(isGrey()?'-grey':'')+(plain()?'-nolabels':'');
  map.rvTiles=tiles;map.rvTileKey=tileKey;map.rvPlain=plain;map.rvGrey=isGrey;map.rvOwn=!!(labels||grey);
  map.rvShowLabels=()=>showLabels(map,false);
  maps.add(map);
  if(zoom){map.rvSimpleControl=simpleControl(L).addTo(map)}
  map.rvCreditCleanup=compactCredit(L,map,container);
  new ResizeObserver(()=>{if(container.isConnected)map.invalidateSize({animate:false})}).observe(container);
  applyTiles(map);
 }
 function detachMapFromPage(map){
  maps.delete(map);
  if(map.rvSimpleControl){try{map.removeControl(map.rvSimpleControl)}catch{}map.rvSimpleControl=null}
  if(map.rvCreditCleanup){try{map.rvCreditCleanup()}catch{}map.rvCreditCleanup=null}
  map.rvShowLabels=null;map.rvTiles=null;map.rvTileKey=null;map.rvPlain=null;map.rvGrey=null;map.rvHold=null;map.rvOnGlReady=null;
 }
 function createMap(L,container,{zoom=true,interactive=true,attribution=ATTRIBUTION,tone=null,labels=null,grey=null,margin=null,warmup=false,autoDispose=true}={}){
  const startWarmup=!!(margin&&warmup&&webgl()),initialPad=startWarmup?.1:margin;
  const map=L.map(container,{zoomControl:false,attributionControl:true,dragging:interactive,touchZoom:interactive,scrollWheelZoom:interactive,doubleClickZoom:interactive,boxZoom:interactive,keyboard:interactive,tap:interactive,zoomSnap:.25,maxZoom:19,fadeAnimation:!reduced(),zoomAnimation:!reduced(),markerZoomAnimation:!reduced(),...(margin?{renderer:L.svg({padding:initialPad})}:{})});
  map.rvMargin=initialPad||0;map.rvMarginMax=margin||0;map.rvWarmup=startWarmup;
  // Leaflet listens on the window and MapLibre keeps a WebGL context and workers: a map left standing when its page
  // goes kept the whole page alive.
  if(autoDispose&&typeof onPageDispose==='function')onPageDispose(()=>map.remove());  // not there on a page loaded by itself
  // margin is the most asked for: each side gets as much of it as keeps the drawn canvas within GL_SIDE device pixels on
  // its longer side (MapLibre and phone GPUs stop at 4096; beyond, the map blurs or is cut), never under the default 0.1.
  // A phone's 지도 tab gets the full 0.5; a tablet's or a desktop's large map, or the small map enlarged on a big
  // screen, a share that fits. Set again on every resize (Leaflet's resize runs this before the plugin redraws).
  // map.rvNarrow (the 지도 tab during a hand zoom) and map.rvWarmup (first paint before idle): the default 0.1.
  if(margin){
   const fit=()=>{const size=map.getSize(),side=Math.max(size.x,size.y)*Math.min(window.devicePixelRatio||1,2);
    const share=(map.rvNarrow||map.rvWarmup)?.1:side>0?Math.max(.1,Math.min(margin,(GL_SIDE/side-1)/2)):margin;map.rvMargin=share;
    if(map.options.renderer)map.options.renderer.options.padding=share;if(map.rvGlLayer)map.rvGlLayer.options.padding=share;};
   map.rvFitMargin=fit;map.on('resize',()=>{fit();map.options.renderer?._update?.()});
  }
  // Leaflet 1.9.4 without zoom animation (reduced motion) moves the zoom along with the fingers during a pinch, so when
  // the pinch ends its final view (_resetView) sees no zoom change and fires no zoomend after the zoomstart: the basemap
  // plugin, waiting for it, stayed frozen out of place (and the 지도 tab's narrow margin with it). A move that ends with a
  // zoom still open, no zoom animation and no pinch in progress closes it.
  let zoomOpen=false;
  map.on('zoomstart',()=>{zoomOpen=true}).on('zoomend',()=>{zoomOpen=false})
   .on('moveend',()=>{if(zoomOpen&&!map._animatingZoom&&!map.touchZoom?._zooming)map.fire('zoomend')});
  map.attributionControl.setPrefix(false);
  const tiles=()=>tone?tone()||theme():mapTone==='auto'?theme():mapTone,plain=()=>labels?!labels():bare,isGrey=()=>grey?grey():simple,tileKey=()=>tiles()+(isGrey()?'-grey':'')+(plain()?'-nolabels':'');
  container.classList.add('rv-map');container.dataset.tiles=tiles();container.dataset.tileKey=tileKey();map.rvTiles=tiles;map.rvTileKey=tileKey;map.rvPlain=plain;map.rvGrey=isGrey;map.rvOwn=!!(labels||grey);map.rvShowLabels=()=>showLabels(map,false);maps.add(map);
  // The style is set after the map exists (not as an option) so that every load, the first and each theme change,
  // goes through transformStyle: it hides the symbol layers right in the style while 글자 숨기기 is on. Leaflet adds
  // the layer (and MapLibre's map) once the map has a view, so this waits for it. No WebGL (rare): the routes are
  // still drawn, on the plain background.
  map.rvGl=null;map.rvSymbols=[];
  if(webgl()){
   const layer=L.maplibreGL({attributionControl:{customAttribution:attribution},pixelRatio:Math.min(window.devicePixelRatio||1,2),fadeDuration:reduced()?0:300,renderWorldCopies:false,...(margin?{padding:initialPad}:{})});
   // The plugin redraws on every Leaflet move (throttled); map.rvHold (a hand drag of the 지도 tab while its margin
   // still covers) skips that: what is drawn moves with the map pane, re-centred by the caller.
   layer._throttledUpdate=L.Util.throttle(function(e){if(!map.rvHold?.())this._update(e)},layer.options.updateInterval,layer);
   map.rvGlLayer=layer;layer.once('add',()=>{map.rvGl=layer.getMaplibreMap();map.rvGl.on('styledata',()=>map.rvShowLabels?.());setStyle(map);map.rvOnGlReady?.(map.rvGl)});
   layer.addTo(map);
  }
  map.rvFitMargin?.();  // the basemap layer exists now: its padding too
  if(zoom){L.control.zoom({position:'topright',zoomInTitle:'확대',zoomOutTitle:'축소'}).addTo(map);map.rvSimpleControl=simpleControl(L).addTo(map)}
  map.rvCreditCleanup=compactCredit(L,map,container);
  new ResizeObserver(()=>{if(container.isConnected)map.invalidateSize({animate:false})}).observe(container);
  return map;
 }
 // The map credit (OpenFreeMap, OpenMapTiles, OpenStreetMap) folds into an ⓘ button in the bottom-right corner on every
 // map (phones, tablets and the small map over the video alike), as MapLibre's compact attribution does: folded at
 // first, the button shows or hides it.
 function compactCredit(L,map,container){
  const corner=map.attributionControl.getContainer().parentNode,toggle=L.DomUtil.create('button','rv-credit-toggle leaflet-control',corner);
  toggle.type='button';toggle.setAttribute('aria-label','지도 출처');toggle.title='지도 출처';
  toggle.innerHTML='<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><circle cx="12" cy="12" r="9"/><path d="M12 11v5.5"/><path d="M12 7.5h.01" stroke-width="2.6"/></svg>';
  L.DomEvent.disableClickPropagation(toggle);
  let open=false;
  const sync=()=>{container.classList.add('is-credit-compact');container.classList.toggle('is-credit-open',open);toggle.setAttribute('aria-expanded',String(open))};
  toggle.addEventListener('click',e=>{e.preventDefault();open=!open;sync()});
  toggle.addEventListener('keydown',e=>e.stopPropagation());
  sync();
  return ()=>{toggle.remove()};
 }
 function simpleControl(L){
  const control=L.control({position:'topright'});
  // One layers button under the zoom buttons opens 지도 표시, a small glass popover like the road view's 레이어 menu,
  // with the two switches; it opens towards the middle of the map and closes on Escape or a tap elsewhere.
  control.onAdd=()=>{
   const wrap=L.DomUtil.create('div','rv-map-simple'),bar=L.DomUtil.create('div','leaflet-bar',wrap),button=L.DomUtil.create('a','rv-map-options-button',bar);
   button.href='#';button.setAttribute('role','button');button.setAttribute('aria-haspopup','true');button.setAttribute('aria-expanded','false');button.title='지도 표시';button.setAttribute('aria-label','지도 표시');
   button.innerHTML='<svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round" aria-hidden="true"><path d="m12 3.5 8.5 4.5-8.5 4.5L3.5 8Z"/><path d="m3.5 12.5 8.5 4.5 8.5-4.5" stroke-linecap="round"/><path d="m3.5 16.5 8.5 4.5 8.5-4.5" stroke-linecap="round" opacity=".55"/></svg>';
   const pop=L.DomUtil.create('div','rv-map-options',wrap);pop.hidden=true;pop.setAttribute('role','group');pop.setAttribute('aria-label','지도 표시');
   pop.innerHTML='<p class="rv-map-options-title">지도 표시</p><div class="rv-map-tone"><span>지도 테마</span><div class="view-switch view-switch-3" role="group" aria-label="지도 테마">'
    +[['auto','자동'],['light','라이트'],['dark','다크']].map(([tone,text])=>`<button type="button" data-tone="${tone}" aria-pressed="false">${text}</button>`).join('')+'</div></div>'+[['labels','글자 숨기기','지명·도로 이름·아이콘·화살표를 숨깁니다. 지도 색은 그대로입니다.'],['grey','회색 지도','차분한 회색 지도로 바꾸고 3D 건물·아이콘을 뺍니다.']]
    .map(([kind,title,note])=>`<label><input type="checkbox" data-kind="${kind}"><span><b>${title}</b><small>${note}</small></span></label>`).join('')+ '<div class="rv-map-tone rv-map-car-choice"><span>차량 아이콘</span><div class="view-switch" role="group" aria-label="차량 아이콘"><button type="button" data-map-car="vehicle" aria-pressed="false">내 차량</button><button type="button" data-map-car="arrow" aria-pressed="false">방향 화살표</button></div></div>';
   const colors=L.DomUtil.create('div','map-car-color-setting',pop);colors.innerHTML=carColorControl();
   for(const choice of colors.querySelectorAll('[data-map-color]'))choice.addEventListener('click',()=>setMapCarColor(choice.dataset.mapColor));
   L.DomEvent.disableClickPropagation(wrap);L.DomEvent.disableScrollPropagation(pop);wrap.rvPop=pop;
   // Open, the popover leaves the map for the page (the open dialog, or the body) at a fixed place beside the button:
   // inside the map it was drawn under whatever lies over the map (주행 지도's segment cards on a phone). Closed, it
   // goes back into the control.
   const place=()=>{
    const host=wrap.closest('dialog[open]')||document.body;if(pop.parentNode!==host)host.append(pop);
    const map=wrap.closest('.leaflet-container')?.getBoundingClientRect(),at=button.getBoundingClientRect(),left=!!map&&at.left+at.width/2>map.left+map.width/2;
    pop.classList.toggle('to-left',left);
    const width=pop.offsetWidth,height=pop.offsetHeight;
    pop.style.left=Math.round(Math.max(8,Math.min(window.innerWidth-width-8,left?at.left-8-width:at.right+8)))+'px';
    pop.style.top=Math.round(Math.max(8,Math.min(window.innerHeight-height-8,at.top)))+'px';
   };
   const resized=()=>setOpen(false);
   wrap.rvClose=()=>{setOpen(false);pop.remove()};
   const outside=event=>{if(!wrap.contains(event.target)&&!pop.contains(event.target))setOpen(false)},key=event=>{if(event.key==='Escape'){event.stopPropagation();event.preventDefault();setOpen(false);button.focus()}};  // only the popover, not the drive map under it
   const setOpen=open=>{
    pop.hidden=!open;wrap.classList.toggle('is-open',open);button.setAttribute('aria-expanded',String(open));
    if(open)place();else if(pop.parentNode!==wrap)wrap.append(pop);
    if(open){document.addEventListener('pointerdown',outside,true);document.addEventListener('keydown',key,true);window.addEventListener('resize',resized)}else{document.removeEventListener('pointerdown',outside,true);document.removeEventListener('keydown',key,true);window.removeEventListener('resize',resized)}
   };
   L.DomEvent.on(button,'click',event=>{L.DomEvent.preventDefault(event);setOpen(pop.hidden)});
   for(const box of pop.querySelectorAll('input'))box.addEventListener('change',()=>setSimple(box.dataset.kind,box.checked));
   for(const choice of pop.querySelectorAll('button[data-tone]'))choice.addEventListener('click',()=>setMapTone(choice.dataset.tone));
   for(const choice of pop.querySelectorAll('button[data-map-car]'))choice.addEventListener('click',()=>setMapCar(choice.dataset.mapCar));
   simpleButtons.add(wrap);showSimple(wrap);return wrap;
  };
  control.onRemove=()=>{control.getContainer()?.rvClose?.();for(const button of simpleButtons)if(!button.isConnected)simpleButtons.delete(button)};
  return control;
 }
 function showSimple(wrap){
  const pop=wrap.rvPop;showCarChoice(pop);
  for(const box of pop.querySelectorAll('input[data-kind]'))box.checked=box.dataset.kind==='grey'?simple:bare;
  for(const choice of pop.querySelectorAll('button[data-tone]'))choice.setAttribute('aria-pressed',String(choice.dataset.tone===mapTone));
  wrap.querySelector('.rv-map-options-button').classList.toggle('is-on',simple||bare||mapTone!=='auto');
 }
 // 지도 테마 fixed against the app's theme: the root says so (data-map-contrast: the map's tone), and the glass over
 // the maps turns nearly solid in the app's own theme (style.css). See-through, a dark card over a light map read as a
 // light card with its light text lost: only the map is to change.
 const markContrast=()=>{const root=document.documentElement;if(mapTone!=='auto'&&mapTone!==theme())root.dataset.mapContrast=mapTone;else delete root.dataset.mapContrast};
 markContrast();
 const syncSimple=()=>{
  markContrast();
  for(const button of simpleButtons){if(!button.isConnected){simpleButtons.delete(button);continue}showSimple(button)}
  for(const map of maps){if(!map.getContainer().isConnected){maps.delete(map);continue}if(!map.rvOwn)applyTiles(map)}
 };
 function setMapTone(tone){if(tone===mapTone)return;mapTone=tone;try{localStorage.setItem(TONE_KEY,tone)}catch{}syncSimple()}
 // Only the basemap's style changes: the view, the route lines and any selection stay as they are.
 function setSimple(kind,on){
  if(kind==='grey')simple=on;else bare=on;try{localStorage.setItem(kind==='grey'?SIMPLE_KEY:BARE_KEY,on?'1':'0')}catch{}
  syncSimple();
 }
 function setStyle(map){
  // 회색 지도: the calm style without the busy layers; 글자 숨기기: below, every symbol layer hidden.
  const grey=map.rvGrey(),box=map.getContainer();box.dataset.simple=String(grey);box.dataset.grey=String(grey);
  map.rvGl.setStyle((grey?SIMPLE_STYLES:STYLES)[map.rvTiles()],{diff:false,transformStyle:(previous,next)=>{
   const busy=grey?next.layers.filter(layer=>layer.type==='fill-extrusion'||BUSY_LAYER.test(layer.id)).map(layer=>layer.id):[];
   map.getContainer().dataset.removed=busy.join(' ');
   if(busy.length)next={...next,layers:next.layers.filter(layer=>!busy.includes(layer.id))};
   // The symbol layers the style itself shows: the ones 글자 숨기기 hides and shows again.
   map.rvSymbols=next.layers.filter(layer=>layer.type==='symbol'&&layer.layout?.visibility!=='none').map(layer=>layer.id);
   const plain=map.rvPlain();box.dataset.labels=plain?'hidden':'shown';
   if(!plain)return next;
   const hidden=new Set(map.rvSymbols);
   return {...next,layers:next.layers.map(layer=>hidden.has(layer.id)?{...layer,layout:{...layer.layout,visibility:'none'}}:layer)};
  }});
 }
 // apply: set the symbol layers' visibility (a loaded style); always: data-labels tells what the map shows.
 function showLabels(map,apply=true){
  const gl=map.rvGl,visible=map.rvPlain()?'none':'visible';if(!gl)return;
  if(apply)try{for(const id of map.rvSymbols)if(gl.getLayer(id))gl.setLayoutProperty(id,'visibility',visible)}catch{}  // Still loading: transformStyle applies it.
  const layers=map.rvSymbols.map(id=>gl.getLayer(id)).filter(Boolean);
  if(layers.length)map.getContainer().dataset.labels=layers.some(layer=>layer.getLayoutProperty('visibility')!=='none')?'shown':'hidden';
 }
 window.addEventListener('rv:themechange',()=>{markContrast();for(const map of maps){const box=map.getContainer();if(!box.isConnected){maps.delete(map);continue}applyTiles(map)}redrawColours()});
 // The theme (or the small map's tone) and 회색 지도 load another style; 글자 숨기기 only switches the symbol layers.
 function applyTiles(map){
  const box=map.getContainer(),key=map.rvTileKey(),tiles=map.rvTiles(),styleChanged=box.dataset.tiles!==tiles||box.dataset.grey!==String(map.rvGrey());box.dataset.tiles=tiles;
  if(box.dataset.tileKey===key)return;box.dataset.tileKey=key;
  if(!map.rvGl)return;if(styleChanged)setStyle(map);else showLabels(map);
 }
 let redrawColours=()=>{};
 const latlngs=points=>points.map(p=>[p[1],p[2]]);
 const kmText=m=>Number.isFinite(m)?(m>=1000?(m/1000).toFixed(m>=100000?0:1)+' km':Math.round(m)+' m'):'—';
 // A route's shape as an SVG fitted to w×h (latitude-scaled, aspect kept, pad: the margin's share of each side): a soft
 // halo under a line that fades from the start colour to the accent, a hollow start dot and a filled end dot with a
 // halo; grid adds faint map lines behind it. The colours are theme tokens (library.css --route-*).
 let shapeId=0;
 function shapeSvg(coords,w,h,{grid=false,pad=.18,stroke=2.6}={}){
  const mid=coords.reduce((s,c)=>s+c[0],0)/coords.length,k=Math.cos(mid*Math.PI/180);
  const xs=coords.map(c=>c[1]*k),ys=coords.map(c=>-c[0]),minX=Math.min(...xs),maxX=Math.max(...xs),minY=Math.min(...ys),maxY=Math.max(...ys);
  const iw=w*(1-2*pad),ih=h*(1-2*pad),scale=Math.min(iw/Math.max(maxX-minX,1e-9),ih/Math.max(maxY-minY,1e-9)),ox=(w-(maxX-minX)*scale)/2,oy=(h-(maxY-minY)*scale)/2;
  const pts=xs.map((x,i)=>[+((x-minX)*scale+ox).toFixed(1),+((ys[i]-minY)*scale+oy).toFixed(1)]),d=pts.map(p=>p.join(',')).join(' ');
  const [a,b]=[pts[0],pts.at(-1)],id='rvShape'+(++shapeId);let lines='';
  if(grid){for(let i=1;i<4;i++)lines+=`<line x1="${(w*i/4).toFixed(1)}" y1="0" x2="${(w*i/4).toFixed(1)}" y2="${h}"/>`;for(let i=1;i<3;i++)lines+=`<line x1="0" y1="${(h*i/3).toFixed(1)}" x2="${w}" y2="${(h*i/3).toFixed(1)}"/>`;lines=`<g class="shape-grid">${lines}</g>`}
  return `<svg class="route-svg" viewBox="0 0 ${w} ${h}" width="${w}" height="${h}" aria-hidden="true" focusable="false"><defs><linearGradient id="${id}" gradientUnits="userSpaceOnUse" x1="${a[0]}" y1="${a[1]}" x2="${b[0]===a[0]&&b[1]===a[1]?b[0]+.1:b[0]}" y2="${b[1]}"><stop offset="0" class="shape-from"/><stop offset="1" class="shape-to"/></linearGradient></defs>${lines}`+
   `<polyline class="shape-halo" points="${d}" stroke-width="${stroke+4}"/><polyline class="shape-line" points="${d}" stroke="url(#${id})" stroke-width="${stroke}"/>`+
   `<circle class="shape-start" cx="${a[0]}" cy="${a[1]}" r="${(stroke*.95).toFixed(2)}" stroke-width="${(stroke*.6).toFixed(2)}"/><circle class="shape-end-halo" cx="${b[0]}" cy="${b[1]}" r="${(stroke*2.2).toFixed(2)}"/><circle class="shape-end" cx="${b[0]}" cy="${b[1]}" r="${(stroke*1.15).toFixed(2)}"/></svg>`;
 }

 if(library){
  // ---- ① 전체 주행 경로, ② the card's route shape and 주행 지도.
  let tracks=sessionCache.tracks,trackTag=sessionCache.tracksTag,trackText=sessionCache.tracksText,trackSignature=sessionCache.tracksSignature,trackRequest=null,mode='list',overview=null,overviewLines=new Map(),selected=null;
  const switcher=document.getElementById('logView'),panel=document.getElementById('logMap'),canvas=panel.querySelector('.log-map-canvas'),card=document.getElementById('logMapCard'),empty=panel.querySelector('.log-map-empty');
  const routeOf=new Map();  // log id -> route of /api/tracks
  if(tracks)for(const route of tracks)for(const id of route.ids)routeOf.set(id,route);
  // The glass pills at the bottom of the maps (the selected drive, the marked segment) rise in (library.css
  // rv-map-card-in) and sink out (.is-leaving, rv-map-card-out) before they are hidden; shown again meanwhile, they
  // come straight back. done runs once the pill is hidden (at once without motion).
  // wait (ms): a pill that shows because the cards were just put away rises in only once they have gone (listWait);
  // rising in at once, it crossed the cards that were still sliding out.
  function showPill(pill,on,done=null,wait=0){
   const ticket=pill.rvPill=(pill.rvPill||0)+1,plain=()=>{pill.style.animationDelay='';pill.style.animationFillMode=''};
   if(on){
    if(pill.hidden||pill.classList.contains('is-leaving')){plain();if(wait>0&&!reduced()){pill.style.animationDelay=Math.round(wait)+'ms';pill.style.animationFillMode='backwards'}}
    pill.classList.remove('is-leaving');pill.hidden=false;return;
   }
   const finish=()=>{if(pill.rvPill!==ticket)return;pill.classList.remove('is-leaving');plain();pill.hidden=true;done?.()};
   if(pill.hidden||reduced()||!pill.getClientRects().length){finish();return}
   plain();pill.classList.add('is-leaving');
   pill.addEventListener('animationend',e=>{if(e.target===pill)finish()},{once:true});setTimeout(finish,320);
  }
  // Tells how long the cards of a list still need to leave: called with whether they are shown, it returns the rest of
  // LIST_OUT_MS since that last turned (0 once they are gone, or when nothing changed).
  const LIST_OUT_MS=300;
  function listWait(){let last=null,at=-1e9;return shown=>{if(shown!==last){if(last!==null)at=performance.now();last=shown}return Math.max(0,at+LIST_OUT_MS-performance.now())}}
  // Scrolls a list so that the row sits in its middle (as far as the list can scroll).
  // The browser's scroll anchoring is off meanwhile: it moved the list for a card that had just shrunk above the row,
  // after the target was worked out, and the row ended at the top instead of the middle.
  // `from`: where the list stood before the rows changed. At the end of the list, the row that loses its buttons shortens
  // it for a moment (a layout falls in before the tapped row grows) and the browser pulls the position up in one step;
  // the scroll starts from `from` again so the list glides instead of jumping.
  function centreRow(list,row,from=null){
   list.style.overflowAnchor='none';clearTimeout(list.rvAnchor);list.rvAnchor=setTimeout(()=>{list.style.overflowAnchor=''},700);
   if(from!==null&&!reduced())list.scrollTop=from;
   const ticket=list.rvCentre=(list.rvCentre||0)+1;
   const target=()=>{const box=row.getBoundingClientRect(),area=list.getBoundingClientRect();return list.scrollTop+box.top-area.top-Math.max(0,(area.height-box.height)/2)};
   const go=()=>list.scrollTo({top:target(),behavior:reduced()?'auto':'smooth'});
   go();
   // The rows out of sight hold a placeholder's height (content-visibility), which their drawn height need not match:
   // the rows the scroll brings into view then move the row away from the middle under way (with the anchoring off,
   // nothing makes up for it). Once the list has come to rest the row is centred again, three times at most, unless the
   // list was touched meanwhile or another row was asked for.
   let touched=false,left=3;const touch=()=>{touched=true};
   for(const type of ['pointerdown','wheel','touchstart','keydown'])list.addEventListener(type,touch,{once:true,passive:true});
   const rest=()=>{
    let last=list.scrollTop,still=0;
    const frame=()=>{
     if(list.rvCentre!==ticket||touched||!row.isConnected)return;
     if(Math.abs(list.scrollTop-last)<.5)still++;else{still=0;last=list.scrollTop}
     if(still<8){requestAnimationFrame(frame);return}
     const top=Math.max(0,Math.min(list.scrollHeight-list.clientHeight,target()));
     if(Math.abs(top-list.scrollTop)>=2&&left-->0){go();rest()}
    };
    requestAnimationFrame(frame);
   };
   rest();
  }
  const groupRoute=group=>{for(const log of group.logs){const route=routeOf.get(log.id);if(route)return route}return null};
  // tracksPending: the routes are on their way. wantDrive: a 지도 button pressed meanwhile; its map opens when they arrive.
  let tracksPending=false,wantDrive=null;
  async function fetchTracks(){
   trackRequest?.abort();const request=trackRequest=new AbortController();
   tracksPending=true;syncCards();
   try{
    const response=await fetch('api/tracks',{cache:'no-store',signal:request.signal,headers:trackTag?{'If-None-Match':trackTag}:{}});
    let text;if(response.status===304&&trackText)text=trackText;else{if(!response.ok)return;text=await response.text();trackTag=response.headers.get('ETag');trackText=text}
    tracks=JSON.parse(text).routes||[];sessionCache.tracksTag=trackTag;sessionCache.tracksText=trackText;sessionCache.tracks=tracks;
    routeOf.clear();for(const route of tracks)for(const id of route.ids)routeOf.set(id,route);
    tracksPending=false;syncCards();if(mode==='map')drawOverview(false);
    if(wantDrive){const {group,button}=wantDrive;wantDrive=null;if(button.isConnected&&logGroups.get(group.route.key)===group&&groupRoute(group))openDrive(group,{opener:button})}
   }catch{}
   finally{if(trackRequest===request&&tracksPending){tracksPending=false;wantDrive=null;syncCards()}}
  }
  // After every list: the routes are fetched again only when the logs with GPS (or their conversions) changed.
  function listed(logs){
   const withTrack=logs.filter(log=>log.has_track);switcher.hidden=!withTrack.length&&mode==='list';
   const signature=JSON.stringify(withTrack.map(log=>[log.id,log.conversion_revision??0,log.name]));
   if(signature!==trackSignature||(!tracks&&withTrack.length)){
    trackSignature=sessionCache.tracksSignature=signature;sessionCache.logTracks.clear();
    if(withTrack.length)fetchTracks();else{tracks=sessionCache.tracks=[];trackTag=sessionCache.tracksTag=null;trackText=sessionCache.tracksText=null;routeOf.clear();syncCards()}
   }else syncCards();
   // The open drive map follows the list; it closes when its drive is gone.
   if(drive.open){if(driveGroup&&logGroups.get(driveGroup.route.key)===driveGroup&&groupRoute(driveGroup))syncDrive();else closeDrive()}
  }
  // ② The route on each card with GPS (library.css shows one per layout): a glass inset in the video thumbnail's corner
  // and the map strip that grows out beside it on hover or focus (mouse screens), or a 지도 button left of 재생 (phones
  // and touch screens). The strip and the button open the drive's map (주행 지도); the inset is only a picture.
  // SVGs are rendered lazily: initial viewport cards synchronously (no flash), cards scrolled near the viewport via
  // IntersectionObserver, and remaining off-screen cards in short idle slices so large lists never block the main thread.
  const SYNC_SHAPE_IMMEDIATE=6,pendingShapes=new Map();
  let shapeIdle=0,shapeIdleKind=0;
  function renderCardShape(group,route,key){
   if(!group.shape)return;
   const [inset,strip,map]=group.shape;
   if(inset.dataset.key===key)return;
   inset.dataset.key=key;const points=route.points;
   inset.innerHTML=shapeSvg(points,46,46,{pad:.2,stroke:2.6});
   strip.innerHTML=shapeSvg(points,99,99,{grid:true,pad:.16,stroke:3});
   map.firstChild.innerHTML=shapeSvg(points,34,32,{pad:.2,stroke:2.2});
  }
  function flushCardShape(element){
   const item=pendingShapes.get(element);if(!item)return;
   pendingShapes.delete(element);shapeObserver?.unobserve(element);
   renderCardShape(item.group,item.route,item.key);
  }
  const shapeObserver=typeof IntersectionObserver==='function'?new IntersectionObserver(entries=>{
   for(const entry of entries)if(entry.isIntersecting)flushCardShape(entry.target);
  },{rootMargin:'240px 0px'}):null;
  function cancelShapeIdle(){
   if(!shapeIdle)return;
   if(shapeIdleKind===1)window.cancelIdleCallback?.(shapeIdle);else clearTimeout(shapeIdle);
   shapeIdle=0;shapeIdleKind=0;
  }
  if(typeof onPageDispose==='function')onPageDispose(()=>{cancelShapeIdle();pendingShapes.clear()});
  function scheduleShapes(){
   if(shapeIdle||!pendingShapes.size)return;
   const run=deadline=>{
    shapeIdle=0;shapeIdleKind=0;let count=0;
    for(const [el,item] of pendingShapes){
     pendingShapes.delete(el);shapeObserver?.unobserve(el);
     if(el.isConnected)renderCardShape(item.group,item.route,item.key);
     if(++count>=4&&(count>=12||(deadline&&deadline.timeRemaining()<=2)))break;
    }
    if(pendingShapes.size)scheduleShapes();
   };
   if(typeof window.requestIdleCallback==='function'){shapeIdleKind=1;shapeIdle=window.requestIdleCallback(run,{timeout:200})}
   else{shapeIdleKind=2;shapeIdle=setTimeout(()=>run(null),16)}
  }
  function syncCards(){
   let immediate=0;
   for(const group of logGroups.values()){
    const route=groupRoute(group),summary=group.element.querySelector(':scope>summary');
    // While the routes are still on their way, a card whose logs have GPS already shows its 지도 button (without the
    // route's shape yet): it used to pop in beside 재생 seconds after the card.
    const pending=!route&&tracksPending&&group.logs.some(log=>log.has_track);
    if(!pending&&(!route||route.points.length<2)){
     if(pendingShapes.delete(group.element))shapeObserver?.unobserve(group.element);
     for(const part of group.shape||[])part.remove();group.shape=null;continue;
    }
    if(!group.shape){
     const open=(button)=>event=>{event.preventDefault();event.stopPropagation();if(groupRoute(group))openDrive(group,{opener:button});else wantDrive={group,button}};
     const inset=document.createElement('span');inset.className='route-inset';inset.setAttribute('aria-hidden','true');
     const strip=document.createElement('button');strip.type='button';strip.className='route-strip';strip.dataset.noRipple='';
     strip.setAttribute('aria-label','주행 지도 열기');strip.setAttribute('aria-haspopup','dialog');strip.title='주행 지도';strip.onclick=open(strip);
     const map=document.createElement('button');map.type='button';map.className='route-map-button';map.dataset.noRipple='';
     map.setAttribute('aria-label','주행 지도 열기');map.setAttribute('aria-haspopup','dialog');map.innerHTML='<span class="route-glyph"></span><span>지도</span>';map.onclick=open(map);
     const hydrate=()=>flushCardShape(group.element);
     summary?.addEventListener('pointerenter',hydrate,{passive:true});summary?.addEventListener('focusin',hydrate,{passive:true});
     group.shape=[inset,strip,map];
    }
    const [inset,strip,map]=group.shape;
    if(pending){if(map.parentNode!==group.actions)group.actions.prepend(map);continue}
    if(inset.parentNode!==group.thumb)group.thumb.append(inset);
    if(strip.parentNode!==summary)group.thumb.after(strip);
    if(map.parentNode!==group.actions)group.actions.prepend(map);
    const key=route.points.length+':'+route.points[0]+':'+route.points.at(-1);
    if(inset.dataset.key!==key){
     if(immediate<SYNC_SHAPE_IMMEDIATE||!shapeObserver){
      immediate++;
      if(pendingShapes.delete(group.element))shapeObserver?.unobserve(group.element);
      renderCardShape(group,route,key);
     }else{
      const had=pendingShapes.has(group.element);
      pendingShapes.set(group.element,{group,route,key});
      if(!had)shapeObserver.observe(group.element);
     }
    }
   }
   if(pendingShapes.size)scheduleShapes();
   if(restoreDrive)restoreDrive();
  }
  const routeFacts=(group,showDriveNumber=false)=>{
   const route=groupRoute(group),date=logDate(group.firstLog),duration=group.logs.reduce((sum,log)=>sum+(Number.isFinite(log.duration)?log.duration:0),0);
   const dateTitle=date?`${localeDate(date)} ${clockText(date)}`:null;
   const title=dateTitle&&showDriveNumber&&group.route.segment!==null?`${group.route.title} · ${dateTitle}`:dateTitle||group.route.title;
   return {title,facts:[kmText(route?.distance_m),duration?durationText(duration):null,`${group.logs.length}개 구간`].filter(Boolean).join(' · ')};
  };
  // The fold button of a list card over a map (wide screens): folds the card away and back, kept on this browser.
  // Marks the cards of a list that are in sight (.is-seen): only those fade and slide when the list is put away or
  // brought back (library.css). Going away, that is what the column shows now; coming back, what it will show at
  // `height` px (the column has no height yet then, but its cards keep their places). Marked without motion
  // (.is-marking): a card marked while the list is away takes its hidden look at once, so it can then slide in.
  function markSeen(column,height=null){
   const area=column.getBoundingClientRect(),end=area.top+(height??area.height);
   column.classList.add('is-marking');
   for(const row of column.children){const box=row.getBoundingClientRect();row.classList.toggle('is-seen',box.bottom>area.top-80&&box.top<end+80)}  // a little beyond: a card just past the edge slides too
   void column.offsetWidth;column.classList.remove('is-marking');void column.offsetWidth;
  }
  function foldable(button,box,key,changed,column){
   let folded=false;try{folded=localStorage.getItem(key)==='1'}catch{}
   const show=()=>{box.classList.toggle('is-folded',folded);button.setAttribute('aria-expanded',String(!folded));const label=folded?'주행 목록 펼치기':'주행 목록 접기';button.setAttribute('aria-label',label);button.title=label};
   show();
   button.addEventListener('click',()=>{markSeen(column);folded=!folded;show();try{localStorage.setItem(key,folded?'1':'0')}catch{}changed()});
   return ()=>folded;
  }
  // ---- 주행 지도: one drive full screen. The map stays on top (phones) or on the left (wide screens); the drive's
  // segment rows, as in the list, scroll on their own in a sheet below it (or beside it). A row tap marks its segment
  // on the map and fits the map to it; a tap on the map marks the row and scrolls the sheet to it. It is a history
  // entry (#drive-map): the system back, the header's back button and Escape close it.
  const drive=document.getElementById('driveMap'),driveBody=drive.querySelector('.drive-map-body'),driveCanvas=drive.querySelector('.drive-map-canvas'),sheet=drive.querySelector('.drive-map-sheet'),handle=drive.querySelector('.drive-map-handle'),sheetList=drive.querySelector('.drive-map-list');
  // The card columns over the maps fade out at an edge with more cards beyond it (none at the top while scrolled to the
  // top, none at the end). Each card near an edge is masked itself, in the column's coordinates: a mask on the column
  // would make it the cards' backdrop root and take away their blur of the map. As on the replay's segment strip
  // (app.js segmentStripFades), each fade widens with the distance scrolled from that end (full after FADE/2 px) instead
  // of switching on at once. The mask reaches SHADOW px past the card on every side and is not clipped to it
  // (mask-clip:no-clip), so a masked card keeps its drop shadow: it fades with the card instead of vanishing.
  const FADE=28,SHADOW=28;
  const MASK_PROPS=[['maskSize',`calc(100% + ${2*SHADOW}px) calc(100% + ${2*SHADOW}px)`],['maskPosition',`${-SHADOW}px ${-SHADOW}px`],['maskRepeat','no-repeat'],['maskOrigin','border-box'],['maskClip','no-clip']];
  function setMask(card,mask){
   if(card.style.maskImage===mask)return;
   for(const [name,value] of MASK_PROPS){const v=mask?value:'';card.style['webkit'+name[0].toUpperCase()+name.slice(1)]=v;card.style[name]=v}
   card.style.webkitMaskImage=mask;card.style.maskImage=mask;
  }
  function fadeEdges(column){
   let frame=0,watched=new Set(),masked=new Set(),childrenDirty=true;
   const syncSizes=()=>{
    childrenDirty=false;
    const now=new Set(column.children);
    for(const card of watched)if(!now.has(card))sizes.unobserve(card);
    for(const card of now)if(!watched.has(card))sizes.observe(card);
    watched=now;
   };
   const paint=()=>{
    frame=0;
    if(childrenDirty)syncSizes();
    const top=Math.max(0,column.scrollTop),h=column.clientHeight,end=Math.max(0,column.scrollHeight-h-top),origin=column.getBoundingClientRect().top+column.clientTop;
    const topFade=Math.min(FADE,top*2),endFade=Math.min(FADE,end*2);
    column.classList.toggle('fade-top',top>1);column.classList.toggle('fade-bottom',end>1);
    column.style.setProperty('--fade-top',topFade.toFixed(1)+'px');column.style.setProperty('--fade-bottom',endFade.toFixed(1)+'px');
    const cards=column.children,n=cards.length,nextMasked=new Set(),px=v=>(v+SHADOW).toFixed(1)+'px';
    // Only measure cards near the visible top/bottom fade edges (binary search on the vertical stack) instead of
    // calling getBoundingClientRect() on every child. Individual cards stay masked (never the column) to preserve
    // backdrop-filter blur of the map behind them.
    if(h>0&&n>0&&(topFade>.5||endFade>.5)){
     const findFirst=threshold=>{
      let lo=0,hi=n;
      while(lo<hi){const mid=(lo+hi)>>1;if(cards[mid].getBoundingClientRect().bottom<=threshold)lo=mid+1;else hi=mid}
      return lo;
     };
     const candidates=new Map();
     const collect=(start,limit)=>{
      for(let i=start;i<n;i++){
       const card=cards[i],box=card.getBoundingClientRect();
       if(box.top>=limit)break;
       candidates.set(card,box);
      }
     };
     if(topFade>.5)collect(findFirst(origin),origin+topFade);
     if(endFade>.5)collect(findFirst(origin+h-endFade),origin+h);
     for(const [card,box] of candidates){
      // A card with its ⋯ menu open is not masked: the mask would cut off the menu where it leaves the card.
      const y=box.top-origin,still=!card.querySelector('.recording-more[open]'),fadeTop=still&&topFade>.5&&y<topFade,fadeBottom=still&&endFade>.5&&y+box.height>h-endFade;
      if(fadeTop||fadeBottom){
       nextMasked.add(card);
       setMask(card,`linear-gradient(to bottom,${fadeTop?`transparent ${px(-y)},#000 ${px(topFade-y)}`:'#000 0'},${fadeBottom?`#000 ${px(h-endFade-y)},transparent ${px(h-y)}`:'#000 100%'})`);
      }
     }
    }
    for(const card of masked)if(!nextMasked.has(card))setMask(card,'');
    masked=nextMasked;
   };
   const later=()=>{if(!frame)frame=requestAnimationFrame(paint)},sizes=new ResizeObserver(later);
   column.addEventListener('scroll',later,{passive:true});
   new ResizeObserver(later).observe(column);
   new MutationObserver(records=>{if(records.some(r=>r.type==='childList'))childrenDirty=true;later()}).observe(column,{childList:true,subtree:true,attributes:true,attributeFilter:['class','hidden','open']});
   return later;
  }
  const DRIVE_HASH='#drive-map',SNAPS=[.36,.54,.7],wide=matchMedia('(min-width:1024px),(min-width:600px) and (orientation:landscape)');
  fadeEdges(sheetList);fadeEdges(document.getElementById('logMapRows'));
  let driveGroup=null,driveMap=null,driveLayer=null,driveLines=new Map(),driveRows=new Map(),driveMarked=null,driveTicket=0,driveOpener=null,restoreDrive=null,escapeTaken=false;
  function openDrive(group,{segment=null,push=true,opener=null}={}){
   const same=drive.open&&driveGroup===group;driveGroup=group;
   if(!same){
    driveMarked=segment;driveRows.clear();sheetList.replaceChildren();syncDrive();sheetList.scrollTop=0;
    if(!drive.open){driveOpener=opener||document.activeElement;drive.showModal();document.body.style.overflow='hidden';driveSheet.set(1,{fit:true})}
    if(push){
     const url=new URL(window.location.href);if(!url.hash||url.hash===DRIVE_HASH){url.hash=libraryTab||'logs';history.replaceState(history.state,'',url)}
     url.hash=DRIVE_HASH;history.pushState({...(history.state||{}),rvScroll:scrollY,rvDrive:group.route.key,rvDriveSegment:segment},'',url);
    }
    drawDrive(segment);
   }else if(segment)markDrive(segment,{fit:true,scroll:true});
  }
  // The header and rows follow the list (polling): only changed rows are rebuilt.
  function syncDrive(){
   const group=driveGroup;if(!group)return;
   const {title,facts}=routeFacts(group);document.getElementById('driveMapTitle').textContent=title;document.getElementById('driveMapFacts').textContent=facts;
   handle.querySelector('.drive-map-count').textContent=`${group.logs.length}개 구간`;
   const logs=[...group.logs].sort((a,b)=>(logRoute(a).segment??0)-(logRoute(b).segment??0)),next=new Map();
   const rows=logs.map(log=>{
    const signature=JSON.stringify(log);let cached=driveRows.get(log.id);
    if(!cached||cached.signature!==signature){const row=renderRow(log,null).row;if(cached)row.classList.add('is-update');cached={row,signature}}
    cached.row.classList.toggle('is-map-marked',log.id===driveMarked);next.set(log.id,cached);return cached.row;
   });
   driveRows=next;reconcileLogs(sheetList,rows);
  }
  async function drawDrive(segment){
   const ticket=++driveTicket,group=driveGroup,firstId=group.firstLog.id;
   const driveSig=JSON.stringify(group.logs.map(l=>[l.id,l.status,l.has_track?1:0,l.conversion_revision??0]));
   const cached=getCachedLogTrack(firstId);
   const loadTrack=cached&&cached.driveSig===driveSig?Promise.resolve(cached.value):fetch(`api/logs/${firstId}/track`,{cache:'no-cache',headers:cached?.etag?{'If-None-Match':cached.etag}:{}}).then(async r=>{
    if(r.status===304&&cached){cached.driveSig=driveSig;return cached.value}
    if(!r.ok)return {segments:[]};
    const value=await r.json();setCachedLogTrack(firstId,{etag:r.headers.get('ETag'),value,driveSig});return value;
   });
   let L,track;
   try{[L,track]=await Promise.all([loadLeaflet(),loadTrack])}
   catch(e){if(ticket===driveTicket)error(e.message);return}
   if(ticket!==driveTicket||!drive.open)return;
   if(!driveMap){driveMap=createMap(L,driveCanvas);driveLayer=L.layerGroup().addTo(driveMap);driveMap.on('click',event=>{const line=nearestLine(driveLines.values(),event);if(line)markDrive(line.rvId,{scroll:true});else markDrive(null,{fit:true})})}
   driveLayer.clearLayers();driveLines.clear();
   const colour=css('--accent')||'#3DCB92';
   for(const part of track.segments){
    if(!part.points.length)continue;
    const path=latlngs(part.points),line=L.polyline(path,{color:colour,weight:5,opacity:.9,lineCap:'round',lineJoin:'round'}).addTo(driveLayer);
    line.bindTooltip(part.segment===null?'구간':'구간 '+part.segment,{sticky:true,direction:'top'});
    line.on('click',e=>{L.DomEvent.stopPropagation(e);markDrive(part.id,{scroll:true})});
    // Segment boundaries: a small ring where each segment starts.
    L.circleMarker(path[0],{radius:4,color:colour,weight:2,fillColor:css('--panel')||'#fff',fillOpacity:1,interactive:false}).addTo(driveLayer);
    line.rvId=part.id;driveLines.set(part.id,line);
   }
   driveMap.invalidateSize({animate:false});
   if(segment&&driveLines.has(segment))markDrive(segment,{fit:true,scroll:true,animate:false});else{driveMarked=null;paintDrive();fitDrive(false)}
  }
  // The zoom and 단순 지도 buttons' column (top right on phones, moved to the left on wide screens): fits keep clear of it.
  function controlsSide(map){
   const area=map.getContainer().getBoundingClientRect(),box=map.getContainer().querySelector('.leaflet-top.leaflet-right')?.getBoundingClientRect();
   if(!box?.width)return {left:0,right:0};
   return box.left>area.left+area.width/2?{left:0,right:Math.max(0,area.right-box.left)}:{left:Math.max(0,box.right-area.left),right:0};
  }
  function fitDrive(animate=!reduced()){
   if(!driveMap||!window.L)return;const line=driveMarked&&driveLines.get(driveMarked);
   const bounds=line?line.getBounds():[...driveLines.values()].reduce((b,l)=>b.extend(l.getBounds()),window.L.latLngBounds([]));
   // The segments' glass floats over the map (a bottom sheet, or a card on the right on wide screens): fit beside it.
   const area=driveCanvas.getBoundingClientRect(),cover=sheet.getBoundingClientRect();
   const right=wide.matches?(driveFolded()?0:Math.max(0,area.right-cover.left)):0,bottom=wide.matches?0:Math.max(0,area.bottom-cover.top);
   const side=controlsSide(driveMap);
   if(bounds.isValid())driveMap.fitBounds(bounds,{paddingTopLeft:[28+side.left,28],paddingBottomRight:[28+Math.max(right,side.right),28+bottom],maxZoom:line?16:17,animate});
  }
  function paintDrive(){
   for(const [id,line] of driveLines){const on=!driveMarked||id===driveMarked;line.setStyle({opacity:on?.95:.3,weight:id===driveMarked?7:5});if(id===driveMarked)line.bringToFront()}
   for(const [id,cached] of driveRows)cached.row.classList.toggle('is-map-marked',id===driveMarked);
  }
  // The marked segment's glass pill at the bottom of the drive map, shown while its card is out of sight: the list folded
  // away (wide screens) or the sheet hidden (phones). 재생 opens that segment; ✕ clears the mark.
  const segmentCard=document.createElement('div');segmentCard.className='log-map-card drive-map-card';segmentCard.setAttribute('role','status');segmentCard.hidden=true;
  segmentCard.innerHTML='<div class="map-card-text"><b class="map-card-title"></b><span class="map-card-facts"></span></div><div class="map-card-actions"><button type="button" class="map-card-close icon-button" aria-label="선택 해제" title="선택 해제"><svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" aria-hidden="true"><path d="M6 6l12 12M18 6 6 18"/></svg></button></div>';
  driveBody.append(segmentCard);
  segmentCard.querySelector('.map-card-close').onclick=()=>markDrive(null);
  const driveWait=listWait();
  function syncSegmentCard(){
   const log=driveMarked&&driveGroup?.logs.find(l=>l.id===driveMarked);
   let folded=false;try{folded=driveFolded()}catch{}  // Not yet defined while the screen is being set up.
   // Phones: the pill shows while the cards are away; the class (not the snap) also follows a drag in progress.
   const out=wide.matches?folded:driveBody.classList.contains('is-sheet-hidden'),show=!!log&&drive.open&&out;
   showPill(segmentCard,show,null,driveWait(!out));if(!show)return;
   const n=logRoute(log).segment,date=logDate(log),end=date&&Number.isFinite(log.duration)?new Date(date.getTime()+log.duration*1000):null;
   segmentCard.querySelector('.map-card-title').textContent=`구간 ${n??'—'}`+(date?` · ${shortClock(date)}${end?'–'+shortClock(end):''}`:'');
   segmentCard.querySelector('.map-card-facts').textContent=[Number.isFinite(log.duration)?durationText(log.duration):null,log.status==='ready'?'재생 가능':null].filter(Boolean).join(' · ');
   segmentCard.querySelector('.map-card-play')?.remove();
   if(log.status==='ready'){const play=document.createElement('a');play.className='replay map-card-play';play.dataset.glow='';play.href=`view/${log.id}/`;play.innerHTML=PLAY_ICON;play.append('재생');segmentCard.querySelector('.map-card-actions').prepend(play)}
  }
  function markDrive(id,{fit=false,scroll=false,animate=!reduced()}={}){
   driveMarked=id;paintDrive();if(fit)fitDrive(animate);syncSegmentCard();
   const row=id&&driveRows.get(id)?.row;
   if(scroll&&row)centreRow(sheetList,row);
  }
  // A tap on a row (outside its play button, ⋯ menu and other controls) marks its segment instead of playing it.
  sheetList.addEventListener('click',event=>{
   const row=event.target.closest('.log-row'),control=row&&event.target.closest('a,button,summary,details,input,label');
   if(!row||control&&row.contains(control))return;
   event.stopPropagation();sheetList.querySelectorAll('.recording-more[open]').forEach(more=>{more.open=false});
   markDrive(row.dataset.id,{fit:true,scroll:true});
  });
  // A phone bottom sheet over a map (the drive map's segments, the all-drives list): its handle drags it between the snap
  // heights (a tap or the arrow keys step through them). Dragged (or stepped) below the lowest height, the cards slide
  // away down and only the handle stays (snap -1); a tap brings them back. body carries is-sheet-hidden, is-dragging and
  // --sheet-h; changed() runs whenever is-sheet-hidden may have changed (mid-drag too), settled(fit) once the sheet has
  // moved (only while ready()). Wide screens leave the handle alone.
  function bottomSheet(body,sheet,handle,{changed,settled,ready,column}){
   let snap=1,refit=0,dragged=false;
   const handleOnly=()=>Math.ceil(handle.getBoundingClientRect().height+18)+'px';
   // Put away from a shown height, the sheet keeps that height and slides down until only the handle shows
   // (is-sheet-down, library.css: no layout per frame). --sheet-top is where its top edge ends up (the pill rides on it).
   // After a drag that hid the cards the sheet is as short as the finger left it: it shrinks to the handle and rest()
   // then swaps it, unseen, for a full sheet slid down, so the next tap slides it up as well.
   const down=()=>body.classList.contains('is-sheet-down'),top=value=>body.style.setProperty('--sheet-top',value);
   let resting=0;
   function rest(){
    if(!body.classList.contains('is-sheet-hidden')||down()||body.classList.contains('is-dragging'))return;
    body.classList.add('is-settling');body.style.setProperty('--sheet-h',SNAPS[1]*100+'%');body.classList.add('is-sheet-down');void sheet.offsetWidth;body.classList.remove('is-settling');
   }
   function set(i,{fit=false}={}){
    snap=Math.max(-1,Math.min(SNAPS.length-1,i));const hidden=snap<0,was=body.classList.contains('is-sheet-hidden');
    clearTimeout(resting);
    if(!wide.matches&&hidden!==was)markSeen(column,hidden||down()?null:SNAPS[snap]*body.clientHeight);
    body.style.setProperty('--sheet-peek',handleOnly());
    if(!hidden){body.classList.remove('is-sheet-down');body.style.setProperty('--sheet-h',SNAPS[snap]*100+'%')}
    else if(!was)body.classList.add('is-sheet-down');
    else if(!down()){body.style.setProperty('--sheet-h',handleOnly());resting=setTimeout(rest,reduced()?0:320)}
    body.classList.toggle('is-sheet-hidden',hidden);top(hidden?handleOnly():SNAPS[snap]*100+'%');
    handle.setAttribute('aria-valuenow',String(hidden?0:Math.round(SNAPS[snap]*100)));changed();handle.setAttribute('aria-valuetext',hidden?'숨김':Math.round(SNAPS[snap]*100)+'%');
    // Raising or lowering the sheet keeps the map where it is: only a tap on a card moves it. Opening the screen (fit)
    // still fits once the sheet has risen, beside its final height.
    clearTimeout(refit);if(ready())refit=setTimeout(()=>settled(fit),reduced()?0:300);
   }
   handle.addEventListener('pointerdown',event=>{
    if(wide.matches||event.button>0)return;
    event.preventDefault();dragged=false;try{handle.setPointerCapture(event.pointerId)}catch{}
    const startY=event.clientY,startH=down()?parseFloat(handleOnly()):sheet.getBoundingClientRect().height,total=body.clientHeight;let moved=false;
    body.classList.add('is-dragging');
    const floor=parseFloat(handleOnly());
    const move=e=>{const dy=e.clientY-startY;if(Math.abs(dy)>4)moved=true;if(!moved)return;const h=Math.max(floor,Math.min(SNAPS.at(-1)*total,startH-dy));
     clearTimeout(resting);body.classList.remove('is-sheet-down');  // dragged, the sheet follows the finger by its height
     body.style.setProperty('--sheet-h',h+'px');top(h+'px');
     // The cards coming back (or going away) mid-drag: the pill leaves (or returns) right then, not on release.
     const hide=h<SNAPS[0]*total*.6;if(hide!==body.classList.contains('is-sheet-hidden')){markSeen(column,hide?null:SNAPS.at(-1)*total);body.classList.toggle('is-sheet-hidden',hide);changed()}};
    const end=()=>{
     handle.removeEventListener('pointermove',move);handle.removeEventListener('pointerup',end);handle.removeEventListener('pointercancel',end);
     body.classList.remove('is-dragging');
     if(!moved)return;  // a tap: the click that follows toggles (see below)
     dragged=true;
     const share=sheet.getBoundingClientRect().height/total;if(share<SNAPS[0]*.6){set(-1);return}
     let best=0;SNAPS.forEach((value,i)=>{if(Math.abs(value-share)<Math.abs(SNAPS[best]-share))best=i});set(best);
    };
    handle.addEventListener('pointermove',move);handle.addEventListener('pointerup',end);handle.addEventListener('pointercancel',end);
   });
   // A tap (or Enter/Space) folds and unfolds like a disclosure: hidden -> the middle height, shown at any height -> hidden.
   // Dragging still settles on any of the three heights; the arrow keys still step through them.
   // The tap acts on its click, not on the finger's release: toggled on release, the sheet moved away first and the click
   // then landed on the map under the finger, which cleared the selection. The click that ends a drag is ignored.
   function toggle(){set(snap<0?1:-1)}
   handle.addEventListener('click',()=>{if(dragged){dragged=false;return}if(!wide.matches)toggle()});
   handle.addEventListener('keydown',e=>{const step={ArrowUp:1,ArrowDown:-1}[e.key];if(step&&!wide.matches){e.preventDefault();set(snap+step)}});
   return {set};
  }
  const driveSheet=bottomSheet(driveBody,sheet,handle,{column:sheetList,changed:()=>syncSegmentCard(),ready:()=>!!driveMap,
   settled:fit=>{if(drive.open){driveMap.invalidateSize({animate:false});if(fit)fitDrive(false)}}});
  const driveFolded=foldable(driveBody.querySelector('.drive-map-fold'),driveBody,'roadviewer-drive-list-folded',()=>syncSegmentCard(),sheetList);  // Folding keeps the map where it is.
  // Closing goes through history, so the system back, the header's back button and Escape all end the same way.
  function closeDrive(){if(!drive.open)return;if(window.location.hash===DRIVE_HASH&&history.state?.rvDrive)history.back();else finishDrive()}
  // Focus goes back to what opened it right away (not on the later close event, which could take it from a field).
  function finishDrive(){driveTicket++;if(!drive.open)return;const opener=driveOpener;driveOpener=null;drive.close();if(opener?.isConnected)opener.focus({preventScroll:true})}
  document.getElementById('driveMapBack').onclick=closeDrive;
  drive.addEventListener('keydown',e=>{if(e.key==='Escape')escapeTaken=!!(drive.querySelector('.recording-more[open]')||document.querySelector('.tag-popover'))},true);
  drive.addEventListener('cancel',e=>{e.preventDefault();if(!escapeTaken)closeDrive();escapeTaken=false});
  drive.addEventListener('close',()=>{
   document.body.style.overflow='';driveGroup=null;driveMarked=null;
   if(window.location.hash===DRIVE_HASH){const url=new URL(window.location.href);url.hash=libraryTab||'logs';history.replaceState({...history.state,rvDrive:null},'',url)}
   driveOpener=null;
  });
  window.addEventListener('popstate',()=>{
   if(drive.open&&window.location.hash!==DRIVE_HASH)finishDrive();
   else if(!drive.open&&window.location.hash===DRIVE_HASH)restore();
  });
  // Reloaded or come back (from a replay) to #drive-map: the drive opens again once the list knows it.
  function restore(){
   const key=history.state?.rvDrive,group=key&&logGroups.get(key);
   if(group&&groupRoute(group)){restoreDrive=null;selectTab('logs',{hash:false});openDrive(group,{segment:history.state.rvDriveSegment,push:false});return}
   if(tracks||!key){restoreDrive=null;const url=new URL(window.location.href);url.hash=libraryTab||'logs';history.replaceState({...history.state,rvDrive:null},'',url)}
  }
  if(window.location.hash===DRIVE_HASH)restoreDrive=restore;
  // ⋯ › 지도에서 보기: the drive's map with this segment marked.
  function showSegment(id){
   const group=[...logGroups.values()].find(group=>group.logs.some(log=>log.id===id));if(!group)return;
   openDrive(group,{segment:id});
  }
  // ① 목록 | 지도
  // Phones: the map reaches from where it starts (the page scrolled to the top) down to just above the bottom tab bar,
  // whatever sits above it (the Home Assistant app's own header, the search, the filter chips); a fixed allowance left a
  // tall device (a Fold) with a short map and empty space under it.
  // A short screen (a Fold's inner one, 670px tall) left only the 320px minimum under the title, search and chips: there the
  // map is as tall as the screen allows with the filter chips kept above it: scrolled until the chips reach the app bar,
  // it ends just above the tab bar. The page is not scrolled for the user. The chips stay because they filter the map
  // and are where a finger can still scroll the page: a map covering the whole screen takes every drag for itself.
  function fitPanelHeight(){
   if(panel.hidden||wide.matches){panel.style.removeProperty('--map-room');return}
   const tabs=document.querySelector('.lib-tabs'),bar=tabs?.getBoundingClientRect(),bottom=bar&&getComputedStyle(tabs).position==='fixed'&&bar.top>innerHeight/2?bar.top:innerHeight;
   const top=panel.getBoundingClientRect().top+window.scrollY,room=bottom-top-12;
   const chips=document.getElementById('logChips'),appbar=document.querySelector('.lib-appbar');
   const head=appbar&&['sticky','fixed'].includes(getComputedStyle(appbar).position)?appbar.getBoundingClientRect().height:0;
   const lift=room<440&&chips?Math.max(0,Math.round(chips.getBoundingClientRect().top+window.scrollY-head-8)):0;
   panel.style.setProperty('--map-room',Math.round(Math.max(320,room+lift))+'px');
  }
  addEventListener('resize',()=>{if(mode==='map')fitPanelHeight()});
  function setMode(next){
   mode=next;for(const button of switcher.querySelectorAll('button'))button.setAttribute('aria-pressed',String(button.dataset.view===mode));
   document.getElementById('list').hidden=mode==='map';panel.hidden=mode!=='map';
   if(mode==='map')fitPanelHeight();
   if(mode==='map'){document.getElementById('empty').hidden=true;listSheet.set(1);drawOverview(true)}else{card.rvPill=(card.rvPill||0)+1;card.classList.remove('is-leaving');card.hidden=true;selected=null;applyFilters()}
   try{sessionStorage.setItem('roadviewer-log-view',mode)}catch{}
  }
  switcher.addEventListener('click',e=>{const button=e.target.closest('button[data-view]');if(button&&button.dataset.view!==mode)setMode(button.dataset.view)});
  const visibleGroups=()=>[...logGroups.values()].filter(group=>!group.element.hidden&&group.element.closest('.log-day')&&!group.element.closest('.log-day').hidden);
  let overviewFit=true,hovered=null,rowsKey='';
  const mapList=document.getElementById('logMapList'),mapRows=document.getElementById('logMapRows'),mapRow=new Map();  // route key -> row
  const listFolded=foldable(panel.querySelector('.map-fold'),panel,'roadviewer-map-list-folded',()=>{select(selected,{fit:false})},mapRows);  // Folding keeps the map where it is.
  // Phones: the cards are a bottom sheet like the drive map's (its handle shows the count); moving it keeps the map put.
  const listHandle=mapList.querySelector('.log-map-handle');
  const listSheet=bottomSheet(panel,mapList,listHandle,{column:mapRows,changed:()=>{if(mode==='map')select(selected)},ready:()=>!!overview,
   settled:()=>{if(mode==='map')overview.invalidateSize({animate:false})}});
  // The cards are in sight: the list card unfolded (wide screens) or the sheet up (phones; the class follows a drag too).
  const listShown=()=>wide.matches?!listFolded():!panel.classList.contains('is-sheet-hidden');
  // Fits beside the list card while it is shown (wide screens), above the sheet (phones).
  function overviewPadding(){
   const area=canvas.getBoundingClientRect(),cover=mapList.getBoundingClientRect();
   const right=wide.matches&&listShown()?Math.max(0,area.right-cover.left):0,bottom=wide.matches?0:Math.max(0,area.bottom-cover.top);
   const side=overview?controlsSide(overview):{left:0,right:0};
   return {paddingTopLeft:[28+side.left,28],paddingBottomRight:[28+Math.max(right,side.right),28+bottom]};
  }
  function fitOverview(group,animate=!reduced()){
   const line=overviewLines.get(groupRoute(group)?.key);
   if(line&&overview)overview.fitBounds(line.getBounds(),{...overviewPadding(),maxZoom:16,animate});
  }
  // The rows are plain buttons, rebuilt only when the drives on the map change (content-visibility skips the ones
  // out of view, so hundreds stay cheap).
  function drawRows(shown){
   const key=shown.map(item=>item.route.key).join('|');
   mapList.querySelector('.log-map-list-count').textContent=`${shown.length}개`;listHandle.querySelector('.drive-map-count').textContent=`주행 목록 ${shown.length}개`;
   if(key===rowsKey)return;rowsKey=key;mapRow.clear();
   mapRows.replaceChildren(...shown.map(({group,route},i)=>{
    const row=document.createElement('div'),pick=document.createElement('button'),title=document.createElement('b'),facts=document.createElement('span'),info=routeFacts(group,true);
    row.className='map-list-row';row.setAttribute('role','listitem');row.dataset.color=PALETTE[i%PALETTE.length];row.rvGroup=group;
    pick.type='button';pick.className='map-list-pick';pick.dataset.noRipple='';title.textContent=info.title;facts.textContent=info.facts;pick.append(title,facts);
    row.append(pick);mapRow.set(route.key,row);return row;
   }));
  }
  mapRows.addEventListener('click',event=>{const row=event.target.closest('.map-list-pick')?.parentElement;if(row)select(row.rvGroup,{fit:true,scroll:true})});
  mapRows.addEventListener('pointerover',event=>{if(event.pointerType!=='mouse')return;const group=event.target.closest('.map-list-row')?.rvGroup||null;if(group!==hovered){hovered=group;paintOverview()}});
  mapRows.addEventListener('pointerleave',()=>{if(hovered){hovered=null;paintOverview()}});
  wide.addEventListener('change',()=>{if(mode==='map')select(selected)});
  wide.addEventListener('change',()=>syncSegmentCard());
  let overviewCasingRenderer=null,overviewFocusCasing=null,overviewGroupLine=new Map(),overviewPainted=false,overviewLastFocus=null,overviewLastTheme=null;
  async function drawOverview(fit){
   if(fit)overviewFit=true;
   if(mode==='map')loadLeaflet().catch(()=>{});
   if(!tracks){empty.hidden=true;return}
   let L;try{L=await loadLeaflet()}catch(e){error(e.message);return}
   if(mode!=='map')return;
   if(!overview){
    overview=createMap(L,canvas);
    overviewCasingRenderer=L.canvas({padding:.1});
    overview.on('click',event=>{const line=nearestLine(overviewLines.values(),event);if(line)select(line.rvGroup,{scroll:true});else select(null)});
   }
   const shown=visibleGroups().map(group=>({group,route:groupRoute(group)})).filter(item=>item.route&&item.route.points.length>1);
   const keys=new Set(shown.map(item=>item.route.key));
   let changed=false;
   for(const [key,line] of overviewLines)if(!keys.has(key)){line.rvCasing?.remove();line.remove();overviewLines.delete(key);changed=true}
   const bounds=overviewFit?L.latLngBounds([]):null;
   overviewGroupLine.clear();
   shown.forEach(({group,route},i)=>{
    let line=overviewLines.get(route.key);
    if(!line){
     const casing=L.polyline(route.points,{renderer:overviewCasingRenderer,weight:7,opacity:.5,lineCap:'round',lineJoin:'round',interactive:false}).addTo(overview);
     line=L.polyline(route.points,{weight:4,opacity:.9,lineCap:'round',lineJoin:'round'}).addTo(overview);
     line.rvCasing=casing;line.on('click',e=>{L.DomEvent.stopPropagation(e);select(line.rvGroup,{scroll:true})});overviewLines.set(route.key,line);changed=true;
    }else if(line.rvPoints!==route.points){line.setLatLngs(route.points);line.rvCasing?.setLatLngs(route.points);changed=true}
    if(line.rvIndex!==i)changed=true;
    line.rvPoints=route.points;line.rvGroup=group;line.rvRoute=route;line.rvIndex=i;overviewGroupLine.set(group,line);
    if(bounds)bounds.extend(route.points);
   });
   if(changed)overviewPainted=false;
   drawRows(shown);
   if(selected&&!shown.some(item=>item.group===selected))select(null);else select(selected);
   empty.hidden=!!shown.length;
   if(overviewFit&&bounds?.isValid()){overviewFit=false;overview.invalidateSize({animate:false});overview.fitBounds(bounds,{...overviewPadding(),animate:false,maxZoom:15})}
  }
  // A hovered row's drive (wide screens) shows over the selected one.
  // Diffing per-line style keys avoids O(N) DOM attribute writes on unchanged polls, and switching focus between two
  // drives updates only those two lines (O(1)). Unfocused casings live on a single Canvas layer; only the focused
  // drive uses a single reusable SVG casing so it stays above dimmed lines.
  function paintOverview(){
   const focus=hovered||selected,cc=casingColor(),themeNow=theme();
   let focusLine=null;
   const applyLine=line=>{
    const isFocus=line.rvGroup===focus,on=!focus||isFocus,w=isFocus?6:4,col=tagColor(line.rvIndex),op=on?.95:.22,cop=on?.55:.12;
    const key=`${col}|${op}|${w}|${cc}|${cop}`;
    if(line.rvStyle!==key){
     line.rvStyle=key;
     line.setStyle({color:col,opacity:op,weight:w});
     line.rvCasing?.setStyle({color:cc,opacity:cop,weight:w+3});
    }
    if(isFocus)focusLine=line;
   };
   if(overviewPainted&&overviewLastTheme===themeNow&&overviewLastFocus&&focus&&overviewLastFocus!==focus){
    const prevLine=overviewGroupLine.get(overviewLastFocus),nextLine=overviewGroupLine.get(focus);
    if(prevLine)applyLine(prevLine);
    if(nextLine)applyLine(nextLine);
   }else{
    for(const line of overviewLines.values())applyLine(line);
   }
   overviewPainted=true;overviewLastFocus=focus;overviewLastTheme=themeNow;
   if(focusLine&&overview&&window.L){
    if(!overviewFocusCasing)overviewFocusCasing=window.L.polyline(focusLine.rvPoints,{weight:9,opacity:.55,color:cc,lineCap:'round',lineJoin:'round',interactive:false});
    else if(overviewFocusCasing.rvPoints!==focusLine.rvPoints)overviewFocusCasing.setLatLngs(focusLine.rvPoints);
    overviewFocusCasing.rvPoints=focusLine.rvPoints;
    if(!overview.hasLayer(overviewFocusCasing))overviewFocusCasing.addTo(overview);
    overviewFocusCasing.setStyle({color:cc,opacity:.55,weight:9});
    overviewFocusCasing.bringToFront();focusLine.bringToFront();
   }else if(overviewFocusCasing&&overview?.hasLayer(overviewFocusCasing)){
    overviewFocusCasing.remove();
   }
  }
  redrawColours=()=>{if(overview)paintOverview();const colour=css('--accent');for(const line of driveLines.values())line.setStyle({color:colour})};
  // The selected drive's actions: in its row while the cards are in sight (the list card, the phone sheet up), else in
  // the bottom card.
  const overviewWait=listWait();
  function select(group,{fit=false,scroll=false}={}){
   const from=mapRows.scrollTop;
   selected=group;paintOverview();
   const wait=overviewWait(listShown()),inline=!!group&&listShown(),row=group&&mapRow.get(groupRoute(group)?.key);
   // A redraw (the list polls) keeps the selected row's buttons, and with them the focus.
   // The buttons that leave are taken out of the layout while the new one is placed and centred (the list's final
   // shape), then shown again to fold away (morphRows).
   const leaving=[];let entering=null;
   // Meanwhile the list keeps its height (a bottom padding as tall as the buttons taken out): a list scrolled to its end
   // would otherwise be pulled up for that moment.
   const pad=mapRows.style.paddingBottom;let held=0;
   for(const other of mapRows.querySelectorAll('.map-list-row.is-selected,.map-list-row:has(>.map-card-actions:not(.is-folding))'))if(other!==row||!inline){const box=other.querySelector(':scope>.map-card-actions:not(.is-folding)');if(box){const s=getComputedStyle(box);box.rvFold={h:box.offsetHeight,height:s.height,pad:s.paddingBottom};held+=box.rvFold.h;leaving.push(box);const r=other.getBoundingClientRect(),a=mapRows.getBoundingClientRect();if(r.bottom>a.top&&r.top<a.bottom){box.rvFold.drawn=other;other.style.contentVisibility='visible'}}other.classList.remove('is-selected')}
   if(held){mapRows.style.paddingBottom=parseFloat(getComputedStyle(mapRows).paddingBottom)+held+'px';for(const box of leaving)box.style.display='none'}
   showPill(card,!!group&&!inline,()=>{if(card.hidden)card.querySelector('.map-card-play')?.remove()},wait);
   if(fit&&group)fitOverview(group);
   if(row){
    row.classList.add('is-selected');
    if(inline&&!row.querySelector(':scope>.map-card-actions:not(.is-folding)')){entering=rowActions(group);row.append(entering)}
    if(scroll)centreRow(mapRows,row,from);
   }
   for(const box of leaving)box.style.display='';if(held)mapRows.style.paddingBottom=pad;
   morphRows(leaving,entering,row);
   if(!group||inline){if(card.hidden)card.querySelector('.map-card-play')?.remove();return}  // else once it has sunk out
   const route=groupRoute(group),date=logDate(group.firstLog),duration=group.logs.reduce((sum,log)=>sum+(Number.isFinite(log.duration)?log.duration:0),0);
   card.querySelector('.map-card-title').textContent=date?`${localeDate(date)} ${clockText(date)}`:group.route.title;
   card.querySelector('.map-card-facts').textContent=[kmText(route?.distance_m),duration?durationText(duration):null,`${group.logs.length}개 구간`].filter(Boolean).join(' · ');
   // 재생 exists only while a drive is shown (the list counts its .replay links).
   card.querySelector('.map-card-play')?.remove();const ready=firstReady(group);
   if(ready){const play=document.createElement('a');play.className='replay map-card-play';play.dataset.glow='';play.href=`view/${ready.id}/`;play.innerHTML=PLAY_ICON;play.append('재생');card.querySelector('.map-card-actions').prepend(play)}
   card.dataset.color=PALETTE[(overviewLines.get(route?.key)?.rvIndex||0)%PALETTE.length];
  }
  // Selecting a drive morphs the list: the selected row's buttons unfold under its title (the row grows, the buttons
  // fade in) while the row that loses them folds back, and the rows below follow both heights, so the list grows and
  // shrinks smoothly instead of jumping. Both heights move over the same time and curve, so when one row's buttons
  // replace another's the list keeps its total height throughout. The selected row's rim fades in. Reduced motion: the
  // old jump.
  const MORPH={duration:300,easing:'cubic-bezier(.2,0,0,1)'};
  function morphRows(leaving,entering,row){
   const still=reduced();
   for(const box of leaving){
    // Measured before its row was unselected: that row now skips its layout until the browser next decides it is in
    // sight (content-visibility), and read now the buttons would measure 0 and vanish at once. A row in sight is kept
    // drawn until they have folded (for a frame it would otherwise take its placeholder height and the list would jump);
    // a row out of sight keeps its placeholder height, as it would without the fold.
    const was=box.rvFold,done=()=>{box.remove();if(was?.drawn)was.drawn.style.contentVisibility=''};
    if(still||!was?.h){done();continue}
    box.inert=true;box.classList.add('is-folding');box.style.overflow='hidden';
    const fold=box.animate([{height:was.height,paddingBottom:was.pad},{height:'0px',paddingBottom:'0px'}],MORPH);
    box.animate([{opacity:1},{opacity:0}],{duration:160,easing:'cubic-bezier(.4,0,1,1)',fill:'forwards'});
    fold.onfinish=fold.oncancel=done;
   }
   if(still)return;
   if(entering&&entering.offsetHeight){
    const s=getComputedStyle(entering);entering.style.overflow='hidden';
    const grow=entering.animate([{height:'0px',paddingBottom:'0px'},{height:s.height,paddingBottom:s.paddingBottom}],MORPH);
    grow.onfinish=grow.oncancel=()=>{entering.style.overflow=''};
    entering.animate([{opacity:0},{opacity:1}],{duration:240,delay:80,easing:'cubic-bezier(0,0,.2,1)',fill:'backwards'});
   }
   if(entering&&row)try{row.animate([{opacity:0},{opacity:1}],{duration:280,easing:'cubic-bezier(0,0,.2,1)',pseudoElement:'::after'})}catch{}
  }
  const playLink=group=>{const ready=firstReady(group);if(!ready)return null;const play=document.createElement('a');play.className='replay map-card-play';play.dataset.glow='';play.href=`view/${ready.id}/`;play.innerHTML=PLAY_ICON;play.append('재생');return play};
  function rowActions(group){
   const box=document.createElement('div'),button=(text,run)=>{const b=document.createElement('button');b.type='button';b.textContent=text;b.onclick=run;return b};
   box.className='map-card-actions';const play=playLink(group);if(play)box.append(play);
   const driveButton=button('주행 지도',event=>openDrive(group,{opener:event.currentTarget}));driveButton.setAttribute('aria-haspopup','dialog');
   box.append(driveButton,button('목록에서 보기',()=>showInList(group)));return box;
  }
  card.querySelector('.map-card-list').onclick=()=>showInList(selected);
  function showInList(group){
   setMode('list');if(!group)return;
   // Centred, or its top near the top of the screen when the card is taller than the screen (an open drive). The list is
   // drawn again as it comes back and rows above can still change height (thumbnails, route shapes), so the drive is
   // brought back twice more if its top ended up off screen, unless the user has scrolled since.
   const target=group.element;let touched=false;const stop=()=>{touched=true};
   for(const type of ['wheel','touchstart','keydown','pointerdown'])addEventListener(type,stop,{once:true,passive:true,capture:true});
   const block=()=>target.getBoundingClientRect().height>innerHeight*.8?'start':'center';
   const again=()=>{if(touched||!target.isConnected)return;const r=target.getBoundingClientRect();if(r.top<0||r.top>=innerHeight)target.scrollIntoView({block:block()})};
   requestAnimationFrame(()=>{target.scrollIntoView({block:block(),behavior:reduced()?'auto':'smooth'});target.classList.remove('is-located');void target.offsetWidth;target.classList.add('is-located');
    setTimeout(again,reduced()?150:450);setTimeout(()=>{again();for(const type of ['wheel','touchstart','keydown','pointerdown'])removeEventListener(type,stop,{capture:true})},reduced()?600:1000)});
  }
  card.querySelector('.map-card-close').onclick=()=>select(null);
  card.querySelector('.map-card-drive').onclick=event=>{if(selected)openDrive(selected,{opener:event.currentTarget})};
  window.rvMaps={listed,filtered:()=>{if(mode==='map')drawOverview(false)},showSegment,mode:()=>mode};
  {let saved=null;try{saved=sessionStorage.getItem('roadviewer-log-view')}catch{}if(saved==='map')setMode('map')}
 }

 if(replay){
  // ---- ③ 주행 상황 | 지도 and ④ the small map over the video.
  const id=location.pathname.split('/').filter(Boolean).at(-1);
  const mapTab=document.getElementById('mapTab'),view=document.getElementById('routeMapView'),follow=document.getElementById('mapFollow'),mini=document.getElementById('videoMap'),miniToggle=document.getElementById('videoMiniMap');
  const routeControls=view.querySelector('.route-map-controls'),overviewButton=document.getElementById('mapOverview');
  const FOLLOW_ZOOM=16;
  const savedBookmarks=window.rvBookmarks?.();
  let bookmarks=savedBookmarks?.id===id?savedBookmarks.items:[];
  // Park both replay maps during SPA swaps within one drive.
  const replayPool=globalThis.__rvReplayMapPool??={active:new Map(),parked:new Map()};
  function destroyParked(slot){
   const entry=replayPool.parked.get(slot);if(!entry)return;
   replayPool.parked.delete(slot);globalThis.clearTimeout(entry.timer);
   try{entry.map.remove()}catch{}
  }
  function destroyAllParked(){for(const slot of [...replayPool.parked.keys()])destroyParked(slot)}
  let route=null,unconverted=[],driveKey='',current=-1,big=null,small=null,bigPending=false,smallPending=false,lastKey='',following=true,mode='road';
  let disposed=false;
  const sameDrive=entry=>entry.driveKey===driveKey||route?.some(segment=>entry.ids.includes(segment.id));
  function parkTarget(slot,target){
   clearTimeout(target.widen);clearTimeout(target.warmupTimer);target.abort?.abort();
   for(const [ev,fn] of target.handlers||[])target.map.off(ev,fn);
   target.layerGroup?.clearLayers();target.layerGroup?.remove();
   const map=target.map;
   if(slot==='small'){
    map.stop();
   }else{
    const pane=map.getPane('mapPane'),glBox=map.rvGlLayer?.getContainer?.();
    if(pane)pane.style.translate='';if(glBox)glBox.style.translate='';
    map.rvNarrow=false;detachMapFromPage(map);
   }
   destroyParked(slot);
   const timer=globalThis.setTimeout(()=>destroyParked(slot),2500);
   replayPool.parked.set(slot,{driveKey,ids:[...route,...unconverted].map(segment=>segment.id),map,container:map.getContainer(),timer,
    styleKey:target.appliedStyle,basemaps:target.basemaps,pinned:slot==='small'&&pinned&&large});
  }
  function claimParked(slot){
   const entry=replayPool.parked.get(slot);if(!entry)return null;
   if(!sameDrive(entry)){destroyParked(slot);return null}
   replayPool.parked.delete(slot);globalThis.clearTimeout(entry.timer);return entry;
  }
  const waitSwap=slot=>document.body.hidden&&replayPool.active.has(slot)?new Promise(resolve=>addEventListener('pageshow',resolve,{once:true})):Promise.resolve();
  if(typeof onPageDispose==='function')onPageDispose(()=>{
   disposed=true;
   for(const [slot,target] of [['big',big],['small',small]]){
    if(!target)continue;
    replayPool.active.delete(slot);
    if(globalThis.__rvNavNext==='replay'&&driveKey)parkTarget(slot,target);
    else{clearTimeout(target.widen);clearTimeout(target.warmupTimer);target.abort?.abort();try{target.map.remove()}catch{}}
   }
   if(globalThis.__rvNavNext!=='replay')destroyAllParked();
   big=null;small=null;
  });
  // A seek asked from another segment's map: applied once this segment's replay is ready.
  try{
   const seek=JSON.parse(sessionStorage.getItem('rv-map-seek')||'null');
   if(seek?.id===id&&Number.isFinite(seek.t)){
    sessionStorage.removeItem('rv-map-seek');const started=performance.now();
    const wait=setInterval(()=>{if(typeof data!=='undefined'&&data&&!loading){clearInterval(wait);setTime(seek.t)}else if(performance.now()-started>30000)clearInterval(wait)},100);
   }
  }catch{}
  // Eagerly preload Leaflet+MapLibre when the user already has the map tab or mini-map enabled.
  {let savedTab=null;try{savedTab=localStorage.getItem('roadviewer-analysis-tab')}catch{}
   if(savedTab==='map')loadLeaflet().catch(()=>{});else if(miniToggle?.checked)loadMapLibre().catch(()=>{})}
  function applyTrack(value){
   if(!value?.segments?.length)return;
   const nextRoute=value.segments.filter(segment=>segment.points.length),nextCurrent=nextRoute.findIndex(segment=>segment.id===id);
   if(nextCurrent<0){destroyAllParked();return}  // This segment has no GPS: no map (its drive's other segments are not this replay).
   route=nextRoute;unconverted=value.unconverted||[];current=nextCurrent;driveKey=route.map(s=>s.id).join(',');
   for(const [slot,entry] of replayPool.parked)if(!sameDrive(entry))destroyParked(slot);
   if(big){
    big.layerGroup.clearLayers();
    big.lines=layers(window.L,big.layerGroup);
    big.layerGroup.addLayer(big.bookmarks);
    drawRouteBookmarks();
   }
   if(small)drawMiniRoute();
   lastKey='';
   // The 지도 tab appears once this segment has a track; a saved 지도 tab (telemetry.js fell back to 주행 상황) returns.
   mapTab.hidden=false;let saved=null;try{saved=localStorage.getItem('roadviewer-analysis-tab')}catch{}
   if(saved==='map')window.selectAnalysisTab?.('map',false);syncMini();
  }
  let asked=false,trackLoading=false,trackRequest=0;
  function fetchReplayTrack(force=false,driveSig=null){
   if(!force&&asked)return;
   const cached=getCachedLogTrack(id);
   if(!force&&cached&&(!driveSig||!cached.driveSig||cached.driveSig===driveSig)){
    asked=true;applyTrack(cached.value);return;
   }
   asked=true;trackLoading=true;
   const request=++trackRequest;
   const headers=cached?.etag?{'If-None-Match':cached.etag}:{};
   return fetch(`../../api/logs/${id}/track`,{cache:'no-cache',headers}).then(async r=>{
    if(request!==trackRequest)return null;
    if(r.status===304&&cached){if(driveSig)cached.driveSig=driveSig;return route?undefined:cached.value}
    if(!r.ok)return null;
    const value=await r.json();
    if(request===trackRequest&&!disposed)setCachedLogTrack(id,{etag:r.headers.get('ETag'),value,driveSig:driveSig??cached?.driveSig});
    return value;
   }).then(value=>{if(request!==trackRequest||disposed)return;trackLoading=false;if(value)applyTrack(value);else if(value===null)asked=false})
    .catch(()=>{if(request===trackRequest){trackLoading=false;asked=false}});
  }
  // Start fetching the track immediately on replay entry (deferred to end of init so all const/let bindings exist).
  window.rvMaps={replayLog(log,logs){
   if(!log||log.id!==id)return;
   if(!log.has_track){evictLogTrack(id);destroyAllParked();return}
   const routeKey=typeof logSegmentInfo==='function'?logSegmentInfo(log.name)?.route:null;
   const sameRoute=Array.isArray(logs)&&routeKey?logs.filter(l=>logSegmentInfo(l.name)?.route===routeKey):[log];
   const driveSig=JSON.stringify(sameRoute.map(l=>[l.id,l.status,l.has_track?1:0,l.conversion_revision??0]));
   const cached=getCachedLogTrack(id);
   if(cached&&cached.driveSig&&cached.driveSig!==driveSig){evictLogTrack(id);fetchReplayTrack(true,driveSig);return}
   if(cached&&!cached.driveSig)cached.driveSig=driveSig;
   if(!asked&&!trackLoading)fetchReplayTrack(false,driveSig);
  }};
  function setMode(next){
   mode=next;
   if(mode==='map'){lastKey='';if(route)ensureBig();else loadLeaflet().catch(()=>{})}else render?.();
   syncMini();
  }
  window.addEventListener('rv:analysistab',e=>{const next=e.detail==='map'?'map':'road';if(next!==mode)setMode(next)});
  // Position at replay time t: between this segment's 1 Hz points. A held sample (the receiver repeating its last fix
  // once while the car moves) is passed over, so the car does not stop for a second and then jump twice as far. The
  // heading turns between the two samples' bearings the short way round (each from the receiver, or the path itself
  // where it gave none).
  const same=(p,q)=>!!p&&!!q&&p[1]===q[1]&&p[2]===q[2];
  const held=(points,j)=>j>0&&j<points.length-1&&same(points[j-1],points[j])&&!same(points[j-2],points[j-1])&&!same(points[j],points[j+1])&&!(Number.isFinite(points[j][3])&&points[j][3]<.5);
  function legBearing(from,to){
   if(!from||!to||same(from,to))return null;
   const radians=Math.PI/180,longitude=((to[2]-from[2])%360+540)%360-180;
   return Math.atan2(longitude*Math.cos(from[1]*radians),to[1]-from[1])/radians;
  }
  function heading(points,j){
   const p=points[j];if(Number.isFinite(p[4]))return p[4];
   // A stopped start may repeat one fix for seconds: orient before playback, using the first distinct fix.
   let next=j+1,previous=j-1;
   while(next<points.length&&same(p,points[next]))next++;
   while(previous>=0&&same(p,points[previous]))previous--;
   const [from,to]=next<points.length?[p,points[next]]:previous>=0?[points[previous],p]:[p,p];
   return legBearing(from,to);
  }
  function position(time,points=route[current].points){
   let i=0;
   while(i<points.length-1&&points[i+1][0]<=time)i++;
   let from=i,to=Math.min(i+1,points.length-1);
   if(held(points,to))to++;else if(held(points,from))from--;
   const a=points[from],b=points[to],span=b[0]-a[0],k=span>0?Math.max(0,Math.min(1,(time-a[0])/span)):0;
   const lat=a[1]+(b[1]-a[1])*k,lon=a[2]+(b[2]-a[2])*k,ha=heading(points,from),hb=heading(points,to);
   const bearing=ha!==null&&hb!==null?ha+(((hb-ha)%360+540)%360-180)*k:ha??hb??0;
   const course=legBearing(a,b)??legBearing(points[from-1],a)??bearing;
   return {latlng:[lat,lon],course,bearing:(bearing%360+360)%360,speed:(a[3]??0)+((b[3]??0)-(a[3]??0))*k,index:i,k,from,to};
  }
  const carIcon=(L,bearing)=>L.divIcon({className:'map-car',html:`<span class="map-car-body" data-car-icon="${mapCar}" data-car-color="${mapCarColor}" style="${carColorStyle()}transform:rotate(${bearing.toFixed(2)}deg)">${carArt(mapCar)}</span>`,iconSize:[44,44],iconAnchor:[22,22]});
  // The drive's lines: this log's segment solid, the drive's other segments faded. Only the large map seeks on a tap
  // (interactive).
  function layers(L,group){
   const shown=css('--accent')||'#3DCB92',rest=css('--muted')||'#9AA0A9',lines={others:[]};
   route.forEach((segment,i)=>{
    if(i===current)return;
    const line=L.polyline(latlngs(segment.points),{color:rest,weight:5,opacity:.45,lineCap:'round',lineJoin:'round'}).addTo(group);
    line.on('click',e=>seekTo(e.latlng));lines.others.push(line);
   });
   // The current segment, drawn whole once (draw), on top of the others. Kept unclipped and unsimplified, so the drawn
   // points are the samples and never change under the car.
   lines.current=L.polyline([],{noClip:true,smoothFactor:0,lineCap:'round',lineJoin:'round',color:shown,weight:6,opacity:.95,className:'map-current'}).addTo(group);
   lines.current.on('click',e=>seekTo(e.latlng));
   const initial=position(typeof t==='number'?t:0);
   lines.car=L.marker(initial.latlng,{icon:carIcon(L,initial.bearing),interactive:false,keyboard:false}).addTo(group);
   return lines;
  }
  const bindMap=(target,ev,fn)=>{target.map.on(ev,fn);target.handlers.push([ev,fn])};
  // Two-stage canvas warmup: starts at 0.1 margin for a fast first paint, then expands to full margin after MapLibre's
  // first idle (or once visible and not mid-gesture).
  function scheduleWarmupExpand(target){
   const {map}=target;if(!map.rvWarmup)return;
   let done=false;
   const expand=()=>{
    if(done||!map.rvWarmup)return;
    if(!map.getContainer().isConnected||map.getSize().x<=0){target.warmupTimer=setTimeout(expand,120);return}
    if(target.zooming||map._animatingZoom||target.held||target.touches>=2||map.rvNarrow){target.warmupTimer=setTimeout(expand,200);return}
    done=true;clearTimeout(target.warmupTimer);target.warmupTimer=0;
    map.rvWarmup=false;setMargin(target);
   };
   const hook=gl=>{if(gl.loaded?.())expand();else gl.once('idle',expand)};
   if(map.rvGl)hook(map.rvGl);else map.rvOnGlReady=hook;
   target.warmupTimer=setTimeout(expand,1200);
  }
  async function ensureBig(){
   if(big||bigPending||!route)return;
   bigPending=true;
   try{
    const L=await loadLeaflet().catch(e=>{showError(e.message);return null});
    await waitSwap('big');
    if(!L||disposed||big||!route)return;
    const slot=view.querySelector('.route-map-canvas');if(!slot)return;
    const reused=claimParked('big');
    let map;
    if(reused){
     slot.replaceWith(reused.container);map=reused.map;
     attachMapToPage(L,map,reused.container,{zoom:true});
    }else{
     map=createMap(L,slot,{margin:BIG_MARGIN,warmup:true,autoDispose:false});
    }
    if(reused)map.invalidateSize({animate:false});
    map.setView(position(typeof t==='number'?t:0).latlng,FOLLOW_ZOOM,{animate:false});
    const layerGroup=L.layerGroup().addTo(map),abort=new AbortController();
    big={map,layerGroup,lines:layers(L,layerGroup),handlers:[],abort};
    big.bookmarks=L.layerGroup().addTo(layerGroup);
    drawRouteBookmarks();
    routeControls.hidden=false;
    replayPool.active.set('big',driveKey);
    bindMap(big,'dragstart',()=>{setFollow(false);big.held=true;big.anchor=dragAnchor(map)});
    bindMap(big,'move',()=>dragMove(big));
    bindMap(big,'moveend',()=>{if(big.held){big.held=false;recentre(big)}});
    bindMap(big,'zoomstart',()=>{if(performance.now()<(big.eager||0))narrow(big)});
    bindMap(big,'zoomstart zoom zoomend',()=>{if(map.rvNarrow)narrow(big)});
    watchView(big);lastKey='';update(true);
    map.rvHold=()=>!!big?.held;
    scheduleWarmupExpand(big);
    // A touch, click, wheel or key on the map first folds the scroll into Leaflet's own view (one pan, the translate
    // back under a pixel), and for a moment after it the map pans by every whole pixel again: a drag, a zoom (buttons,
    // pinch, wheel, keys) and a tap's place (seeking on a line) then work on what is shown.
    const box=map.getContainer();
    // A zoom by hand (two fingers, the wheel, the buttons, +/-, a box zoom; a double tap or click through zoomstart)
    // narrows the margin first, before Leaflet's own handlers.
    const opts={capture:true,passive:true,signal:abort.signal};
    for(const type of ['pointerdown','touchstart','mousedown','wheel','keydown','dblclick','click'])box.addEventListener(type,e=>{
     if(e.touches)big.touches=e.touches.length;
     if(zoomInput(e)){narrow(big);setFollow(false)}
     else if(e.type==='keydown'&&e.key.startsWith('Arrow'))setFollow(false);
     big.eager=performance.now()+800;settle(big);
    },opts);
    for(const type of ['touchmove','touchend','touchcancel'])box.addEventListener(type,e=>{big.touches=e.touches.length;if(type==='touchmove'&&e.touches.length>=2&&map.rvNarrow)narrow(big)},opts);
   }finally{bigPending=false}
  }
  // Keep the basemap aligned during Leaflet's own movement, including free exploration.
  function watchView(target){
   bindMap(target,'zoomstart',()=>{target.zooming=true});
   bindMap(target,'zoomend',()=>{target.zooming=false;lastKey=''});
   bindMap(target,'resize',()=>{target.eager=performance.now()+300;lastKey=''});
   bindMap(target,'move zoomend resize',()=>alignBasemap(target));
  }
  // The analysis tab keeps its compositor-based following and Leaflet pan behavior.
  const BIG_MARGIN=1,RECENTRE=.65;
  // eager (and for a moment after an input or a resize): pan by every whole pixel, Leaflet's own view kept on what is shown.
  function followCar(target,latlng,share,eager=performance.now()<(target.eager||0)){
   const {map}=target,pane=map.getPane('mapPane'),size=map.getSize(),reach=eager?0:RECENTRE*map.rvMargin;
   const at=()=>map.project(latlng,map.getZoom())._subtract(map.getPixelOrigin())._add(window.L.DomUtil.getPosition(pane));
   let p=at(),dx=p.x-size.x/2,dy=p.y-size.y*share;target.follow={latlng,share};
   if(Math.abs(dx)>Math.max(.5,reach*size.x)||Math.abs(dy)>Math.max(.5,reach*size.y)){
    map.panBy([Math.round(dx),Math.round(dy)],{animate:false});redrawBasemap(map);p=at();dx=p.x-size.x/2;dy=p.y-size.y*share;
   }
   shiftPane(target,`${(-dx).toFixed(3)}px ${(-dy).toFixed(3)}px`);alignBasemap(target);
  }
  // The basemap exactly under the lines, by a sub-pixel translate of its canvas: the plugin puts the canvas on a whole
  // pixel, though its margin (a share of the size) can end in a half, and centres MapLibre on getCenter(), which until
  // the first pan after a setView or a zoom is the centre asked for, not the one Leaflet drew from (its pixel origin is
  // rounded). With a pan on every pixel neither showed; scrolling for seconds between pans, both would.
  function alignBasemap(target,force=false){
   const {map}=target,box=map.rvGlLayer?.getContainer?.();if(!box||(target.held&&!force))return;  // held: what is drawn moves with the pane
   const size=map.getSize(),zoom=map.getZoom(),p=map.containerPointToLayerPoint([0,0]).subtract(size.multiplyBy(map.rvGlLayer.options.padding));
   const c=map.project(map.getCenter(),zoom).subtract(map.getPixelOrigin()).subtract(map.containerPointToLayerPoint(size.divideBy(2)));
   const value=`${(p.x-Math.round(p.x)+c.x).toFixed(3)}px ${(p.y-Math.round(p.y)+c.y).toFixed(3)}px`;
   if(target.align!==value){target.align=value;box.style.translate=value}
  }
  // Leaflet's view brought to what is shown (the translate back under a pixel).
  function settle(target){if(target.follow&&target.shift&&!target.zooming&&following)followCar(target,target.follow.latlng,target.follow.share,true)}
  function shiftPane(target,value){if(target.shift===value)return;target.shift=value;target.map.getPane('mapPane').style.translate=value}
  function redrawBasemap(map,force=false){  // Still loading: MapLibre draws on its own frame.
   const layer=map.rvGlLayer,zooming=layer?._zooming;
   try{if(force&&layer)layer._zooming=false;layer?._update?.();map.rvGl?.redraw?.()}catch{}finally{if(layer)layer._zooming=zooming}
  }
  // A hand zoom on the 지도 tab (pinch, wheel, double tap, +/-, the buttons, a box zoom) redraws the whole basemap canvas
  // every frame, its cost following the canvas's area (a full margin each side: 9x the shown area, ~70 ms a frame on a
  // phone). From the gesture's start until ZOOM_HOLD after its last input (and Leaflet's zoom animation) the margin is
  // the default 0.1, then the full margin again: each a single resize and redraw, the shown area unmoved.
  const ZOOM_HOLD=1000,ZOOM_KEYS=new Set(['+','=','-','_']);
  const zoomInput=e=>e.type==='wheel'||e.type==='dblclick'||(e.type==='touchstart'&&e.touches.length>=2)||(e.type==='keydown'&&ZOOM_KEYS.has(e.key))||
   (e.type==='mousedown'&&e.shiftKey)||((e.type==='pointerdown'||e.type==='click'&&e.detail===0)&&!!e.target?.closest?.('.leaflet-control-zoom'));
  function narrow(target){
   const {map}=target;clearTimeout(target.widen);target.widen=setTimeout(()=>widen(target),ZOOM_HOLD);
   if(map.rvNarrow||map._animatingZoom)return;map.rvNarrow=true;setMargin(target);
  }
  function widen(target){
   const {map}=target;
   if(target.zooming||map._animatingZoom||target.held||target.touches>=2){target.widen=setTimeout(()=>widen(target),ZOOM_HOLD/4);return}
   if(map.rvNarrow){map.rvNarrow=false;setMargin(target)}
  }
  // The basemap's and the lines' margin set again (createMap's fit), the canvas resized and drawn once around the view.
  function setMargin(target){
   const {map}=target,layer=map.rvGlLayer;map.rvFitMargin?.();
   try{if(layer?._container&&layer._glMap){layer._resizeContainer();layer._glMap.resize()}}catch{}
   recentre(target,true);
  }
  // A hand drag (and its inertia) moves the map pane, and with it what is drawn: the basemap is re-centred (and the
  // lines clipped again) only once the pane has moved RECENTRE of the margin since, and when the map comes to rest.
  function dragAnchor(map){const o=map.rvGlLayer?._offset;return o?window.L.point(-o.x,-o.y):window.L.DomUtil.getPosition(map.getPane('mapPane'))}
  function dragMove(target){
   if(!target.held||target.zooming)return;
   const {map}=target,p=window.L.DomUtil.getPosition(map.getPane('mapPane')),a=target.anchor,size=map.getSize(),reach=RECENTRE*map.rvMargin;
   if(!a||Math.abs(p.x-a.x)>reach*size.x||Math.abs(p.y-a.y)>reach*size.y)recentre(target);
  }
  function recentre(target,force=false){
   const {map}=target;if(target.zooming&&!force)return;
   redrawBasemap(map,force);map.options.renderer?._update?.();
   target.anchor=window.L.DomUtil.getPosition(map.getPane('mapPane'));alignBasemap(target,true);
  }
  function setFollow(on){
   if(on===following||!big)return;
   if(!on)settle(big);
   following=on;
   follow.setAttribute('aria-pressed',String(on));
   shiftPane(big,'');
   if(on){big.map.setView(position(t).latlng,FOLLOW_ZOOM,{animate:false});update(true)}
  }
  follow.onclick=()=>setFollow(!following);
  overviewButton.onclick=()=>{
   setFollow(false);
   const bounds=window.L.latLngBounds(route.flatMap(segment=>latlngs(segment.points)));
   big.map.fitBounds(bounds,{padding:[24,routeControls.offsetHeight+20],animate:false,maxZoom:17});
  };
  function draw(target,pos,force){
   const {lines,map}=target,points=route[current].points,now=pos.latlng;
   // The current segment is drawn once; as the car moves only the marker moves, so the line is never projected,
   // rounded or simplified again under the car.
   if(force||lines.points!==points){lines.points=points;lines.current.setLatLngs(latlngs(points))}
   // The marker sits on Leaflet's whole pixel; its body takes the remaining fraction and the heading.
   lines.car.setLatLng(now);const body=lines.car.getElement()?.querySelector('.map-car-body');
   if(body){const p=map.project(now,map.getZoom()),transform=`translate(${(p.x-Math.round(p.x)).toFixed(3)}px,${(p.y-Math.round(p.y)).toFixed(3)}px) rotate(${pos.bearing.toFixed(2)}deg)`;if(lines.transform!==transform){lines.transform=transform;body.style.transform=transform}}
   if(!target.zooming&&following)followCar(target,now,.5);
  }
  function update(force=false){
   if(!route||current<0||typeof t==='undefined')return;
   const pos=position(t),key=pos.index+':'+pos.k.toFixed(4),moved=force||key!==lastKey;lastKey=key;
   if(big&&mode==='map'&&moved)draw(big,pos,force);
   if(small&&!mini.hidden&&(moved||small.smoothing))followMini(pos);
  }
  // Follows playback: one cheap check per frame while a map is shown.
  const loop=()=>{if(route&&current>=0&&hasVideo()!==miniVideo)syncMini();if((big&&mode==='map')||(small&&!mini.hidden))update();requestAnimationFrame(loop)};requestAnimationFrame(loop);
  // Tapping the route seeks there; a point of another segment of the drive opens that segment at that time.
  function seekTo(latlng){
   let best=null,bestD=Infinity;const k=Math.cos(latlng.lat*Math.PI/180);
   route.forEach((segment,s)=>segment.points.forEach(p=>{const d=(p[1]-latlng.lat)**2+((p[2]-latlng.lng)*k)**2;if(d<bestD){bestD=d;best={s,t:p[0]}}}));
   if(!best)return;
   seekReplay(route[best.s].id,best.t);
  }
  function seekReplay(target,time){
   if(target===id){if(!loading)setTime(Math.max(0,Math.min(time,data?.duration??time)))}
   else{try{sessionStorage.setItem('rv-map-seek',JSON.stringify({id:target,t:time}))}catch{}goToLog({id:target})}
  }
  const carChoices=document.getElementById('mapCarIcon');
  if(carChoices){
   showCarChoice(carChoices);for(const choice of carChoices.querySelectorAll('button[data-map-car]'))choice.addEventListener('click',()=>setMapCar(choice.dataset.mapCar));
   const colors=document.createElement('div');colors.id='mapCarColors';colors.className='map-car-color-setting sub';colors.innerHTML=carColorControl();carChoices.parentElement.after(colors);showCarChoice(colors);
   for(const choice of colors.querySelectorAll('[data-map-color]'))choice.addEventListener('click',()=>setMapCarColor(choice.dataset.mapColor));
  }
  // ④ The video stage owns the overlay geometry. The canvas is native MapLibre and has its own lifetime.
  const stage=document.getElementById('videoStage'),hud=stage.querySelector('.video-hud');
  const openMap=mini.querySelector('.video-map-open'),closeMap=mini.querySelector('.video-map-close');
  const controls=mini.querySelector('.video-map-controls'),pin=mini.querySelector('.video-map-pin');
  const followMiniButton=mini.querySelector('.video-map-follow'),overviewMiniButton=mini.querySelector('.video-map-overview'),preview=mini.querySelector('.video-map-preview');
  const previewButton=preview.querySelector('button'),previewStatus=preview.querySelector('small');
  const credit=mini.querySelector('.video-map-credit');
  credit.querySelector('div').innerHTML=SHORT_ATTRIBUTION;
  const toneSwitch=document.getElementById('videoMiniMapTone'),miniPlain=document.getElementById('videoMiniMapPlain');
  const miniGrey=document.getElementById('videoMiniMapGrey'),TONE_KEY='roadviewer-minimap-tone';
  let miniTone='dark';
  try{const saved=localStorage.getItem(TONE_KEY);if(['auto','light','dark'].includes(saved))miniTone=saved}catch{}
  const showTone=()=>{for(const button of toneSwitch.querySelectorAll('button'))button.setAttribute('aria-pressed',String(button.dataset.tone===miniTone))};
  showTone();
  toneSwitch.addEventListener('click',event=>{
   const button=event.target.closest('button[data-tone]');if(!button)return;
   miniTone=button.dataset.tone;showTone();
   try{localStorage.setItem(TONE_KEY,miniTone)}catch{}
   applyMiniStyle();
  });
  for(const box of [miniPlain,miniGrey])box.addEventListener('change',applyMiniStyle);

  const MINT='#3DCB92',AMBER='#F5C451',BASE='#0E1219',FADED='#7C96B8';
  const sourceNames=['route','bridges','bookmarks','car','snap'];
  const collection=(features=[])=>({type:'FeatureCollection',features});
  const feature=(type,coordinates,properties={})=>({type:'Feature',geometry:{type,coordinates},properties});
  const coords=point=>[point[2],point[1]],lngLat=pos=>[pos.latlng[1],pos.latlng[0]];
  const line=(id,source,paint,filter)=>({id,type:'line',source,paint,...(filter?{filter}:{}),layout:{'line-cap':'round','line-join':'round'}});
  const circle=(id,color,radius,stroke,width)=>({id,type:'circle',source:id,paint:{
   'circle-color':color,'circle-radius':radius,'circle-stroke-color':stroke,'circle-stroke-width':width,
  }});
  const currentLine=['==',['get','current'],true];
  const overlayLayers=[
   line('rv-bridges','rv-bridges',{'line-color':'#8B9AB0','line-width':4,'line-opacity':.65,'line-dasharray':[2,2]}),
   line('rv-route','rv-route',{'line-color':FADED,'line-width':5},['!',currentLine]),
   line('rv-route-casing','rv-route',{'line-color':BASE,'line-width':9},currentLine),
   line('rv-route-current','rv-route',{'line-color':MINT,'line-width':6},currentLine),
   circle('rv-bookmarks',AMBER,6,BASE,2),
   circle('rv-snap','rgba(61,203,146,.25)',9,MINT,2),
   {id:'rv-car',type:'symbol',source:'rv-car',layout:{
    'icon-image':'rv-vehicle','icon-rotation-alignment':'viewport','icon-pitch-alignment':'viewport',
    'icon-rotate':['get','bearing'],'icon-allow-overlap':true,'icon-ignore-placement':true,
   }},
  ];
  function miniStyle(base,dark){
   const layers=(base?.layers||[{type:'background',id:'background',paint:{'background-color':dark?BASE:'#F0EEEA'}}])
    .filter(layer=>layer.type!=='fill-extrusion'&&!BUSY_LAYER.test(layer.id)&&!(miniPlain.checked&&layer.type==='symbol'))
    .map(layer=>{
     if(!dark||miniGrey.checked)return layer;
     let paint=layer.paint;
     if(layer.type==='background')paint={...paint,'background-color':BASE};
     if(layer.type==='line'&&(/road|street|transportation|highway/i.test(layer.id)||layer['source-layer']==='transportation')){
      paint={...paint,'line-color':/motorway|trunk|primary|major/i.test(layer.id)?'#2A3342':'#1E2532'};
     }
     if(layer.type==='symbol')paint={...paint,'text-halo-width':0,'text-color':'#A2AEBF'};
     return {...layer,paint};
    });
   const sources={...base?.sources};
   for(const name of sourceNames)sources['rv-'+name]={type:'geojson',data:collection(),tolerance:0};
   return {...base,version:8,sources,layers:[...layers,...overlayLayers]};
  }
  async function applyMiniStyle(){
   const target=small;if(!target)return;
   const tone=miniTone==='auto'?(mini.closest(':fullscreen')?'dark':theme()):miniTone;
   const url=(miniGrey.checked?SIMPLE_STYLES:STYLES)[tone],key=[tone,miniGrey.checked,miniPlain.checked].join(':');
   if(target.styleKey===key)return;
   target.styleKey=key;
   const revision=target.styleRevision=(target.styleRevision||0)+1;
   try{
    let base=target.basemaps.get(url);
    if(!base){
     const response=await fetch(url,{signal:AbortSignal.timeout(10000)});
     if(!response.ok)throw Error('basemap unavailable');
     base=await response.json();target.basemaps.set(url,base);
    }
    if(target!==small||target.styleRevision!==revision)return;
    target.map.setStyle(miniStyle(base,tone==='dark'),{diff:false});target.appliedStyle=key;
   }catch{
    if(target!==small||target.styleRevision!==revision)return;
    target.styleKey=null;target.appliedStyle=null;
    target.map.setStyle(miniStyle(null,tone==='dark'),{diff:false});
   }
  }
  const miniData=(name,features)=>small?.map.getSource('rv-'+name)?.setData(collection(features));
  let routeGeometry=[],bridgeGeometry=[],bounds=null,screenRoute=null;
  let selection=null,miniVideo=null,large=false,pinned=false,miniFollowing=true,resizing=false,resizeRevision=0,returnTimer=0;
  const conversions=new Map();
  let conversionTimer=0;

  function drawMiniRoute(){
   bounds=new window.maplibregl.LngLatBounds();
   routeGeometry=route.map(segment=>{
    const coordinates=segment.points.map(coords),distances=[0];
    coordinates.forEach((point,i)=>{
     bounds.extend(point);
     if(i)distances.push(distances[i-1]+new window.maplibregl.LngLat(...coordinates[i-1]).distanceTo(new window.maplibregl.LngLat(...point)));
    });
    return {segment,coordinates,distances};
   });
   bridgeGeometry=[];
   for(let i=1;i<routeGeometry.length;i++){
    const before=routeGeometry[i-1],after=routeGeometry[i],start=before.segment.segment,end=after.segment.segment;
    if(!Number.isInteger(start)||!Number.isInteger(end)||end<=start+1)continue;
    const pending=unconverted.filter(segment=>Number.isInteger(segment.segment)&&segment.segment>start&&segment.segment<end);
    if(!pending.length)continue;
    const a=before.coordinates.at(-1),b=after.coordinates[0];
    const at=k=>[a[0]+(b[0]-a[0])*k,a[1]+(b[1]-a[1])*k];
    pending.forEach((segment,j)=>{
     const coordinates=[at(j/pending.length),at((j+1)/pending.length)];
     bridgeGeometry.push({segment,coordinates});
    });
   }
   paintMiniRoute();
  }
  function paintMiniRoute(){
   miniData('route',routeGeometry.filter(row=>row.coordinates.length>1).map(({segment,coordinates})=>
    feature('LineString',coordinates,{current:segment.id===id})));
   miniData('bridges',bridgeGeometry.map(({coordinates})=>feature('LineString',coordinates)));
   drawMiniBookmarks();
   screenRoute=null;
   if(selection?.kind==='bridge'){
    const updated=bridgeGeometry.find(row=>row.segment.id===selection.target.id);
    showPreview(updated?{...selection,target:updated.segment}:null);
   }
  }
  function drawMiniBookmarks(){
   if(!small||!route)return;
   const points=route[current].points;
   miniData('bookmarks',bookmarks.map((bookmark,index)=>feature('Point',lngLat(position(bookmark.time,points)),{index})));
  }
  function drawRouteBookmarks(){
   if(!big)return;
   const L=window.L,group=big.bookmarks;
   group.clearLayers();
   const icon=L.divIcon({className:'map-bookmark',iconSize:[28,28],iconAnchor:[14,14]});
   for(const bookmark of bookmarks){
    const title=clock(bookmark.time)+' · '+bookmark.title;
    const marker=L.marker(position(bookmark.time).latlng,{icon,title,zIndexOffset:1000,autoPanOnFocus:false}).addTo(group);
    marker.getElement()?.setAttribute('aria-label',title);
    marker.on('click',()=>seekReplay(id,bookmark.time));
   }
  }
  window.addEventListener('rv:bookmarks',event=>{
   if(event.detail.id!==id)return;
   bookmarks=event.detail.items;drawMiniBookmarks();drawRouteBookmarks();
  });

  function drawMiniCar(){
   const map=small?.map;if(!map?.getLayer('rv-car'))return;
   const art=document.createElement('canvas');art.width=64;art.height=64;
   const ctx=art.getContext('2d');ctx.scale(2,2);
   const color=carColorHex()||'#F2F4F6';
   const rect=(x,y,w,h,r)=>ctx.roundRect?ctx.roundRect(x,y,w,h,r):ctx.rect(x,y,w,h);
   const arrow=mapCar==='arrow';
   ctx.fillStyle=color;ctx.strokeStyle=BASE;ctx.lineWidth=1.5;ctx.lineJoin='round';
   ctx.beginPath();
   if(arrow){ctx.moveTo(16,4);ctx.lineTo(26,26);ctx.lineTo(16,20);ctx.lineTo(6,26);ctx.closePath()}
   else rect(8.5,2.5,15,27,4);
   ctx.fill();ctx.stroke();
   if(!arrow){ctx.beginPath();ctx.fillStyle='#324157';rect(11,6,10,4,1);ctx.fill()}
   const image=ctx.getImageData(0,0,64,64);
   if(map.hasImage('rv-vehicle'))map.updateImage('rv-vehicle',image);
   else map.addImage('rv-vehicle',image,{pixelRatio:2});
  }
  redrawMiniCar=drawMiniCar;
  const shortArc=(from,to)=>((to-from)%360+540)%360-180;
  const zoomAt=speed=>Math.max(14.6,Math.min(17.1,17.35-speed*.022));
  const hasVideo=()=>typeof data!=='undefined'&&!!data?.video;
  function cameraAt(pos){
   const {map,bearing,zoom}=small;
   const {clientWidth,clientHeight}=map.getContainer();
   // Keep the vehicle marker below the HUD and inside the darker part of the corner fade.
   const padding={top:Math.min(clientHeight*.22,hud.offsetTop+hud.offsetHeight+12),bottom:0,left:clientWidth*.4,right:0};
   return {center:lngLat(pos),pitch:56,bearing,zoom,padding};
  }
  function followMini(pos){
   const {map}=small,speed=pos.speed*3.6,targetZoom=zoomAt(speed);
   small.bearing??=pos.course;small.zoom??=targetZoom;
   const delta=speed>=3?shortArc(small.bearing,pos.course):0,zoomDelta=targetZoom-small.zoom;
   small.bearing+=Math.abs(delta)<.05?delta:delta*.14;
   small.zoom+=Math.abs(zoomDelta)<.005?zoomDelta:zoomDelta*.14;
   small.smoothing=Math.abs(delta)>=.05||Math.abs(zoomDelta)>=.005;
   if(!resizing&&!large)map.jumpTo(cameraAt(pos));
   if(!resizing&&large&&miniFollowing&&!map.isMoving()){
    const center=map.getCenter(),[lng,lat]=lngLat(pos);
    if(center.lng!==lng||center.lat!==lat)map.jumpTo({center:[lng,lat]});
   }
   miniData('car',[feature('Point',lngLat(pos),{bearing:large?pos.bearing-map.getBearing():0})]);
  }
  function centreMini(duration){
   small.map.easeTo({center:lngLat(position(t)),pitch:0,bearing:0,zoom:FOLLOW_ZOOM,padding:{top:0,bottom:0,left:0,right:0},duration});
  }
  function fitDrive(duration=0){
   const {map}=small;if(!bounds||bounds.isEmpty())return;
   const pad=Math.min(40,map.getContainer().clientHeight/5),top=mini.querySelector('.video-map-toolbar').offsetHeight+16;
   map.setPadding({left:0,right:0,top:0,bottom:0});
   // linear fitBounds uses easeTo for the transition to north-up.
   map.fitBounds(bounds,{pitch:0,bearing:0,maxZoom:17.1,padding:{top:Math.max(top,pad),bottom:pad,left:pad,right:pad},linear:true,duration});
  }
  async function ensureSmall(){
   if(small||smallPending||!route)return;
   smallPending=true;
   try{
    const gl=await loadMapLibre();
    await waitSwap('small');
    if(disposed||small||!route||mini.hidden||mini.classList.contains('no-room'))return;
    const slot=mini.querySelector('.video-map-canvas'),reused=claimParked('small');
    const initial=position(typeof t==='number'?t:0);
    let map;
    if(reused){slot.replaceWith(reused.container);map=reused.map}
    else map=new gl.Map({
     container:slot,style:miniStyle(null,true),center:lngLat(initial),bearing:initial.course,pitch:56,
     zoom:zoomAt(initial.speed*3.6),attributionControl:false,trackResize:false,
     dragPan:false,dragRotate:false,boxZoom:false,touchZoomRotate:false,touchPitch:false,
     scrollZoom:false,doubleClickZoom:false,keyboard:false,renderWorldCopies:false,
    });
    small={map,handlers:[],abort:new AbortController(),styleKey:reused?.styleKey,appliedStyle:reused?.styleKey,basemaps:reused?.basemaps||new Map()};
    replayPool.active.set('small',driveKey);
    const ready=()=>{drawMiniCar();paintMiniRoute();followMini(position(typeof t==='number'?t:0));if(selection)showPreview(selection)};
    bindMap(small,'style.load',ready);
    bindMap(small,'move',()=>{screenRoute=null});
    for(const event of ['dragstart','zoomstart','movestart'])bindMap(small,event,e=>{if(large&&e.originalEvent)setMiniFollow(false)});
    map.keyboard.disableRotation();
    const canvas=map.getCanvas();canvas.tabIndex=-1;
    canvas.addEventListener('wheel',event=>{if(large&&!resizing&&event.deltaY)setMiniFollow(false)},
     {passive:true,signal:small.abort.signal});
    canvas.addEventListener('keydown',event=>{if(large&&/^(Arrow|[+=_-])/.test(event.key))setMiniFollow(false)},
     {signal:small.abort.signal});
    resizeMini();drawMiniRoute();ready();applyMiniStyle();
    if(reused?.pinned){setPinned(true);setLarge(true,true)}
   }catch{
    if(!disposed&&!small){mini.hidden=true;stage.classList.remove('has-video-map')}
   }finally{smallPending=false}
  }

  // CSS rotates the fullscreen stage clockwise. Project/hit-test against the canvas in its own frame.
  const turned=()=>!!stage.closest('.is-turned');
  const inStage=(box,frame)=>turned()?{top:frame.right-box.right,width:box.height,height:box.width}:{top:box.top-frame.top,width:box.width,height:box.height};
  const point=e=>turned()?{x:e.clientY,y:-e.clientX}:{x:e.clientX,y:e.clientY};
  function mapPoint(event){
   const box=small.map.getContainer().getBoundingClientRect();
   return turned()?{x:event.clientY-box.top,y:box.right-event.clientX}:{x:event.clientX-box.left,y:event.clientY-box.top};
  }
  function measureSmall(){
   const frame=stage.getBoundingClientRect(),box=inStage(frame,frame),speed=inStage(hud.getBoundingClientRect(),frame);
   stage.style.setProperty('--vmap-min-w',Math.ceil(speed.width)+'px');
   const rows=[...stage.querySelectorAll('.video-tools,.video-fullscreen')].map(el=>el.getBoundingClientRect()).filter(row=>row.width&&row.height);
   const room=Math.min(box.height,...rows.map(row=>inStage(row,frame).top-8));
   mini.style.setProperty('--vmap-room',Math.floor(room)+'px');
   mini.classList.toggle('no-room',room<speed.height+28);
  }
  function syncMini(){
   miniVideo=hasVideo();
   const on=!!route&&current>=0&&miniToggle.checked&&miniVideo&&mode!=='map';
   if(!on&&large)setLarge(false,true);
   mini.hidden=!on;stage.classList.toggle('has-video-map',on);
   if(on){measureSmall();if(small){resizeMini();lastKey=''}else ensureSmall()}
  }
  function resizeMini(){
   if(!small||mini.hidden)return false;
   const box=small.map.getContainer(),width=box.clientWidth,height=box.clientHeight;
   if(!width||!height||(small.width===width&&small.height===height))return false;
   small.width=width;small.height=height;small.map.resize();lastKey='';
   return true;
  }
  const resizeWatch=new ResizeObserver(()=>{
   if(mini.hidden)return;
   measureSmall();
   if(resizing)return;
   if(!small)ensureSmall();
   else resizeMini();
  });
  resizeWatch.observe(stage);resizeWatch.observe(mini);resizeWatch.observe(hud);
  miniToggle.addEventListener('change',syncMini);
  addEventListener('fullscreenchange',applyMiniStyle);
  function holdCanvas(width,height){
   Object.assign(small.map.getContainer().style,{inset:'auto',left:'50%',top:'50%',width:width+'px',height:height+'px',transform:'translate(-50%,-50%)'});
   resizeMini();
  }
  function releaseCanvas(){small.map.getContainer().removeAttribute('style');resizeMini()}
  function setExpandedInput(enabled){
   const map=small.map,canvas=map.getCanvas();
   for(const handler of [map.scrollZoom,map.keyboard]){
    if(enabled)handler.enable();else handler.disable();
   }
   canvas.tabIndex=enabled?0:-1;
  }
  function setLarge(next,instant=false){
   if(!small||next===large)return;
   const map=small.map,box=map.getContainer(),moving=!(instant||reduced()),wasFocused=mini.contains(document.activeElement);
   clearTimeout(returnTimer);const revision=++resizeRevision;resizing=moving;cancelGesture();
   if(moving){
    const inset=parseFloat(getComputedStyle(mini).getPropertyValue('--vmap-inset'))||0;
    holdCanvas(next?stage.clientWidth-2*inset:box.clientWidth,next?stage.clientHeight-2*inset:box.clientHeight);
   }else{mini.classList.add('is-instant');releaseCanvas()}
   large=next;
   mini.classList.toggle('is-large',large);stage.classList.toggle('video-map-large',large);
   openMap.hidden=large;openMap.setAttribute('aria-expanded',String(large));controls.hidden=!large;
   credit.open=false;
   setExpandedInput(false);setMiniFollow(true);showPreview(null);
   if(!moving)resizeMini();
   if(large)centreMini(moving?280:0);
   else map.easeTo({...cameraAt(position(t)),duration:moving?280:0});
   const finish=()=>{
    resizing=false;releaseCanvas();
    if(large)setExpandedInput(true);
    lastKey='';watchConversion();
   };
   if(moving){
    // CSS can start a frame late or retarget during a window resize. Release when it actually finishes.
    const afterTransition=()=>{
     if(disposed||revision!==resizeRevision)return;
     const transitions=mini.getAnimations().filter(animation=>animation.pending||animation.playState==='running');
     if(transitions.length)Promise.allSettled(transitions.map(animation=>animation.finished)).then(afterTransition);
     else finish();
    };
    afterTransition();
   }else{finish();mini.classList.remove('is-instant')}
   if(wasFocused)(large?closeMap:openMap).focus({preventScroll:true});
  }
  function setPinned(on){
   pinned=on;pin.setAttribute('aria-pressed',String(on));pin.textContent=on?'핀 고정됨':'핀 고정';
   if(on)clearTimeout(returnTimer);
  }
  function setMiniFollow(on){
   miniFollowing=on;
   followMiniButton.setAttribute('aria-pressed',String(on));followMiniButton.textContent=on?'내 차 추적':'자유 탐색 · 복귀';
  }
  openMap.onclick=()=>setLarge(true);
  closeMap.onclick=()=>setLarge(false);
  pin.onclick=()=>setPinned(!pinned);
  followMiniButton.onclick=()=>{if(!resizing){clearTimeout(returnTimer);setMiniFollow(true);centreMini(reduced()?0:280)}};
  overviewMiniButton.onclick=()=>{if(!resizing){clearTimeout(returnTimer);setMiniFollow(false);showPreview(null);fitDrive(reduced()?0:280)}};
  mini.addEventListener('keydown',event=>{if(large&&event.key==='Escape'){event.preventDefault();setLarge(false)}});

  function projectedRoute(){
   if(screenRoute)return screenRoute;
   const map=small.map;
   screenRoute=[...routeGeometry,...bridgeGeometry].map(row=>{
    const points=row.coordinates.map(lnglat=>map.project(lnglat));
    let left=Infinity,right=-Infinity,top=Infinity,bottom=-Infinity;
    for(const p of points){left=Math.min(left,p.x);right=Math.max(right,p.x);top=Math.min(top,p.y);bottom=Math.max(bottom,p.y)}
    return {...row,points,left,right,top,bottom};
   });
   return screenRoute;
  }
  function nearest(at,radius,includePins=true){
   const {map}=small;
   if(includePins&&map.getLayer('rv-bookmarks')){
    const hit=map.queryRenderedFeatures([[at.x-radius,at.y-radius],[at.x+radius,at.y+radius]],{layers:['rv-bookmarks']})[0];
    const bookmark=bookmarks[hit?.properties.index];
    if(bookmark)return {kind:'bookmark',bookmark,target:route[current],time:bookmark.time,coordinates:hit.geometry.coordinates};
   }
   let best=null,limit=radius*radius;
   for(const row of projectedRoute()){
    if(at.x<row.left-radius||at.x>row.right+radius||at.y<row.top-radius||at.y>row.bottom+radius)continue;
    const {segment,points,coordinates,distances}=row;
    for(let i=1;i<points.length;i++){
     const a=points[i-1],b=points[i],dx=b.x-a.x,dy=b.y-a.y;
     if(at.x<Math.min(a.x,b.x)-radius||at.x>Math.max(a.x,b.x)+radius||
        at.y<Math.min(a.y,b.y)-radius||at.y>Math.max(a.y,b.y)+radius)continue;
     const length=dx*dx+dy*dy,k=length?Math.max(0,Math.min(1,((at.x-a.x)*dx+(at.y-a.y)*dy)/length)):0;
     const distance=(at.x-a.x-dx*k)**2+(at.y-a.y-dy*k)**2;
     if(distance>limit||(distance===limit&&(best?.target.id===id||best?.kind==='route'&&!distances)))continue;
     limit=distance;
     const snapped=coordinates[i-1].map((coord,axis)=>coord+(coordinates[i][axis]-coord)*k);
     if(!distances){best={kind:'bridge',target:segment,coordinates:snapped};continue}
     const from=segment.points[i-1],to=segment.points[i];
     best={kind:'route',target:segment,coordinates:snapped,time:from[0]+(to[0]-from[0])*k,
      speed:(from[3]??0)+((to[3]??0)-(from[3]??0))*k,distance:distances[i-1]+(distances[i]-distances[i-1])*k};
    }
   }
   return best;
  }
  function showPreview(hit){
   selection=hit;preview.hidden=!hit;
   miniData('snap',hit?[feature('Point',hit.coordinates)]:[]);
   if(!hit)return;
   const name=Number.isInteger(hit.target.segment)?'구간 '+hit.target.segment:'현재 구간';
   preview.querySelector('b').textContent=hit.bookmark?.title||name;
   previewButton.hidden=hit.kind!=='bridge';previewStatus.hidden=true;
   if(hit.kind==='bridge'){
    const status=hit.target.status;
    preview.querySelector('span').textContent={queued:'변환 대기 중',processing:'변환 중',error:'변환 실패'}[status]||'미변환 · 예상 연결선';
    previewButton.disabled=conversions.has(hit.target.id)||status==='queued'||status==='processing';
   }else{
    const details=[hit.bookmark?name:null,clock(hit.time)];
    if(hit.kind==='route')details.push(Math.round(hit.speed*3.6)+' km/h',(hit.distance/1000).toFixed(2)+' km');
    preview.querySelector('span').textContent=details.filter(Boolean).join(' · ');
   }
  }
  function commitSnap(hit){
   if(!hit)return;
   showPreview(hit);
   if(hit.kind==='bridge')return;
   seekReplay(hit.target.id,hit.time);
   clearTimeout(returnTimer);
   if(!pinned)returnTimer=setTimeout(()=>setLarge(false),350);
  }
  function watchConversion(){
   clearTimeout(conversionTimer);
   if(!large||!unconverted.some(segment=>['queued','processing'].includes(segment.status)))return;
   conversionTimer=setTimeout(async()=>{await fetchReplayTrack(true);watchConversion()},5000);
  }
  previewButton.onclick=async()=>{
   const target=selection?.target;if(!target||previewButton.disabled)return;
   conversions.set(target.id,true);previewButton.disabled=true;previewStatus.hidden=true;
   try{
    const response=await fetch('../../api/logs/'+encodeURIComponent(target.id)+'/convert',{
     method:'POST',headers:{'X-RoadViewer-Request':'1'},signal:AbortSignal.timeout(20000),
    });
    const result=await response.json();
    if(!response.ok)throw Error(result.message||'변환을 요청하지 못했습니다.');
    target.status=result.status;
    if(selection?.target.id===target.id)showPreview(selection);
    await fetchReplayTrack(true);
    watchConversion();
   }catch(error){
    if(selection?.target.id===target.id){previewStatus.textContent=error.message;previewStatus.hidden=false}
   }finally{
    conversions.delete(target.id);
    if(selection?.target.id===target.id)previewButton.disabled=['queued','processing'].includes(selection.target.status);
   }
  };

  // Pointer coordinates are shared by snapping, panning and pinch zoom, including the rotated HA fullscreen frame.
  const pointers=new Map();
  let gesture=null;
  function cancelGesture(){
   gesture=null;
   for(const pointer of pointers.keys())if(mini.hasPointerCapture(pointer))mini.releasePointerCapture(pointer);
   pointers.clear();
  }
  mini.addEventListener('pointerdown',event=>{
   if(!large||resizing||event.button!==0||!event.target.closest('.maplibregl-canvas'))return;
   const at=mapPoint(event),radius=event.pointerType==='touch'?28:14;
   mini.setPointerCapture(event.pointerId);pointers.set(event.pointerId,at);
   clearTimeout(returnTimer);small.map.stop();
   if(pointers.size>1){gesture={kind:'pinch'};setMiniFollow(false);showPreview(null);return}
   const hit=nearest(at,radius);
   gesture={kind:hit?'hit':'pan',start:at,hit,radius};
   if(hit)showPreview(hit);else showPreview(null);
  });
  mini.addEventListener('pointermove',event=>{
   if(!pointers.has(event.pointerId)||!gesture)return;
   const at=mapPoint(event),last=pointers.get(event.pointerId),map=small.map;
   pointers.set(event.pointerId,at);
   if(gesture.kind==='pinch'){
    const other=[...pointers].find(([pointer])=>pointer!==event.pointerId)?.[1];
    if(!other)return;
    const from=Math.hypot(last.x-other.x,last.y-other.y),to=Math.hypot(at.x-other.x,at.y-other.y);
    if(from>0&&to>0){
     const around=map.unproject([(last.x+other.x)/2,(last.y+other.y)/2]);
     map.zoomTo(map.getZoom()+Math.log2(to/from),{around,duration:0},{originalEvent:event});
     map.panBy([(last.x-at.x)/2,(last.y-at.y)/2],{duration:0},{originalEvent:event});
    }
   }else if(gesture.kind==='pan'){
    if(!gesture.moved&&Math.hypot(at.x-gesture.start.x,at.y-gesture.start.y)<4)return;
    gesture.moved=true;
    setMiniFollow(false);map.panBy([last.x-at.x,last.y-at.y],{duration:0},{originalEvent:event});
   }else if(gesture.hit.kind==='route'||Math.hypot(at.x-gesture.start.x,at.y-gesture.start.y)>=4){
    const snap=nearest(at,gesture.radius,false);
    if(snap?.kind==='route'){gesture.hit=snap;showPreview(snap)}
   }
  });
  function endGesture(event){
   if(!pointers.delete(event.pointerId)||pointers.size)return;
   const ended=gesture;gesture=null;
   if(ended?.kind!=='hit')return;
   if(event.type==='pointerup')commitSnap(ended.hit);else showPreview(null);
  }
  for(const type of ['pointerup','pointercancel','lostpointercapture'])mini.addEventListener(type,endGesture);
  // Dragging the collapsed map opens it. The lower-left grip keeps its existing explicit resize behavior.
  let openStart=null;
  openMap.addEventListener('pointerdown',event=>{if(event.button===0){openStart=point(event);openMap.setPointerCapture(event.pointerId)}});
  openMap.addEventListener('pointermove',event=>{
   if(!openStart)return;
   const at=point(event);
   if(Math.hypot(at.x-openStart.x,at.y-openStart.y)>=8){openStart=null;setLarge(true)}
  });
  for(const type of ['pointerup','pointercancel','lostpointercapture'])openMap.addEventListener(type,()=>{openStart=null});

  // The bottom-left corner resizes the small map (mouse or touch), anchored at its top-right: from its least
  // width up to the picture's width or the room above the bottom row. Kept in this browser as a share of the picture's
  // width; a double tap on the corner goes back to the default size. Dragging never toggles 크게 보기.
  const SIZE_KEY='roadviewer-minimap-size',grip=document.createElement('span');grip.className='video-map-grip';grip.setAttribute('aria-hidden','true');mini.append(grip);
  const applySize=f=>{if(f){stage.style.setProperty('--vmap-f',f.toFixed(4));stage.classList.add('vmap-sized')}else{stage.style.removeProperty('--vmap-f');stage.classList.remove('vmap-sized')}};
  try{const f=Number(localStorage.getItem(SIZE_KEY));if(f>0&&f<1)applySize(f)}catch{}
  let sizing=null;
  grip.addEventListener('pointerdown',e=>{
   if(mini.classList.contains('is-large')||e.button>0)return;e.preventDefault();e.stopPropagation();try{grip.setPointerCapture(e.pointerId)}catch{}
   const frame=stage.getBoundingClientRect(),r=inStage(mini.getBoundingClientRect(),frame),box=inStage(frame,frame),room=parseFloat(mini.style.getPropertyValue('--vmap-room'))||box.height,at=point(e);
   const style=getComputedStyle(mini),ratio=Number(style.getPropertyValue('--vmap-ratio'));
   const min=Math.max(104,parseFloat(stage.style.getPropertyValue('--vmap-min-w'))||0),max=Math.max(min,Math.min(box.width,room/ratio));
   sizing={id:e.pointerId,x:at.x,y:at.y,w:r.width,min,max,ratio,stageW:box.width,moved:false,f:null};mini.classList.add('is-sizing');
  });
  grip.addEventListener('pointermove',e=>{
   if(!sizing||e.pointerId!==sizing.id)return;const at=point(e),dx=sizing.x-at.x,dy=(at.y-sizing.y)/sizing.ratio;
   if(!sizing.moved&&Math.hypot(dx,dy)<3)return;sizing.moved=true;e.preventDefault();
   const w=Math.min(sizing.max,Math.max(sizing.min,sizing.w+(Math.abs(dx)>=Math.abs(dy)?dx:dy)));sizing.f=w/sizing.stageW;applySize(sizing.f);
  });
  const endSizing=e=>{if(!sizing||e.pointerId!==sizing.id)return;mini.classList.remove('is-sizing');if(sizing.moved&&sizing.f)try{localStorage.setItem(SIZE_KEY,String(sizing.f))}catch{}sizing=null};
  grip.addEventListener('pointerup',endSizing);grip.addEventListener('pointercancel',endSizing);
  grip.addEventListener('click',e=>e.stopPropagation());
  grip.addEventListener('dblclick',e=>{e.stopPropagation();applySize(0);try{localStorage.removeItem(SIZE_KEY)}catch{}});
  redrawColours=()=>{
   applyMiniStyle();if(!big)return;
   big.lines.others.forEach(line=>line.setStyle({color:css('--muted')}));
   big.lines.current.setStyle({color:css('--accent')});
  };
  if(/^[0-9a-f]{32}$/.test(id))fetchReplayTrack();
 }
})();
