// GPS route maps (maps.js): ① the list's 목록 | 지도 view, ② the route shape on a card and 주행 지도 (the drive full
// screen: map + segment sheet), ③ the replay's 주행 상황 | 지도 view and ④ the small map in the video's top-right corner,
// in the dark and the light theme. The OpenFreeMap basemap (MapLibre in Leaflet) is stubbed with small fixtures (no
// network) unless RV_MAP_REAL_TILES=1; the positions are synthetic, never a real drive.
// RV_MAP_PARTS=native runs the focused desktop scenario, RV_TESLA_SHOTS also captures desktop and mobile.
const fs=require('fs'),os=require('os'),path=require('path'),zlib=require('zlib'),assert=require('node:assert/strict'),{spawn}=require('child_process'),{chromium}=require('playwright');
const {settleLayout,waitForSteadyLayout}=require('./browser-helpers.cjs');
// OpenFreeMap stubs: a style per theme (a background, a GeoJSON street grid around the routes and a symbol layer of
// street names, so 글자 숨기기 has layers to hide), its TileJSON, empty vector tiles and empty glyph ranges.
// RV_MAP_REAL_TILES=1 lets the browser fetch the real OpenFreeMap styles and tiles instead (with RV_MAP_SHOTS, to look).
const REAL=process.env.RV_MAP_REAL_TILES==='1';
// RV_MAP_SCHEMES=dark or =light runs one theme only (the file took 140-170 s as one).
const SCHEMES=(process.env.RV_MAP_SCHEMES||'dark,light').split(',');
// RV_MAP_PARTS picks the sections: main (phone and desktop maps, per theme), many (the wide lists with many drives, per
// theme) and leak (pages left behind, once). The sections do not depend on each other. CI (tests/browser-groups.json) runs main once per theme and the
// rest in a third group: one theme with every section was the longest job of the run (about 100 s).
const PARTS=(process.env.RV_MAP_PARTS||'main,many,leak').split(',');
// Synthetic routes: in the stubbed run on empty ground (the fixture streets lie around it); with real tiles, in a city
// centre (a public landmark, not a drive) so the pictures show roads and names.
const ORIGIN=REAL?[37.55,126.96]:[10,20];
const OFM='https://tiles.openfreemap.org/',STYLE_OF={light:OFM+'styles/liberty',dark:OFM+'styles/dark'},SIMPLE_LIGHT=OFM+'styles/positron';
function fixtureStyle(scheme){
 const c=scheme==='dark'?{ground:'#24262a',road:'#464a52',text:'#9aa0a9'}:{ground:'#f0eeea',road:'#ffffff',text:'#555a60'};
 const streets=[];for(let k=0;k<12;k++){streets.push({type:'Feature',properties:{name:'가로 '+k},geometry:{type:'LineString',coordinates:[[19.99,9.99+k*0.004],[20.06,9.99+k*0.004]]}});streets.push({type:'Feature',properties:{name:'세로 '+k},geometry:{type:'LineString',coordinates:[[19.99+k*0.006,9.99],[19.99+k*0.006,10.05]]}})}
 return {version:8,name:'fixture-'+scheme,glyphs:OFM+'fonts/{fontstack}/{range}.pbf',
  sources:{openmaptiles:{type:'vector',url:OFM+'planet'},streets:{type:'geojson',data:{type:'FeatureCollection',features:streets}}},
  layers:[{id:'background',type:'background',paint:{'background-color':c.ground}},
   {id:'transportation',type:'line',source:'openmaptiles','source-layer':'transportation',paint:{'line-color':c.road}},
   {id:'street',type:'line',source:'streets',paint:{'line-color':c.road,'line-width':3}},
   {id:'street-name',type:'symbol',source:'streets',layout:{'symbol-placement':'line','text-field':['get','name'],'text-font':['Noto Sans Regular'],'text-size':11},paint:{'text-color':c.text}},
   {id:'building-3d',type:'fill-extrusion',source:'openmaptiles','source-layer':'building',paint:{'fill-extrusion-height':10}},
   {id:'poi_r1',type:'symbol',source:'streets',layout:{'text-field':['get','name'],'text-font':['Noto Sans Regular']}},
   {id:'poi-hidden',type:'symbol',source:'streets',layout:{visibility:'none','text-field':['get','name'],'text-font':['Noto Sans Regular']}}]};
}
// Each Leaflet map of the page (window.rvTestMaps), for the checks against MapLibre's own projection.
const hookMaps=()=>{window.rvTestNearLine=(root,at)=>Math.min(Infinity,...[...root.querySelectorAll('.leaflet-overlay-pane path.map-current')].flatMap(p=>{const M=p.getScreenCTM(),q=[...(p.getAttribute('d')||'').matchAll(/(-?[\d.]+)[ ,](-?[\d.]+)/g)].map(m=>[+m[1]*M.a+ +m[2]*M.c+M.e,+m[1]*M.b+ +m[2]*M.d+M.f]);
  return q.slice(1).map((b,i)=>{const a=q[i],dx=b[0]-a[0],dy=b[1]-a[1],l=dx*dx+dy*dy,k=l?Math.max(0,Math.min(1,((at[0]-a[0])*dx+(at[1]-a[1])*dy)/l)):0;return Math.hypot(a[0]+dx*k-at[0],a[1]+dy*k-at[1])})}));
 let L;Object.defineProperty(window,'L',{configurable:true,get:()=>L,set(v){L=v;try{v.Map.addInitHook(function(){(window.rvTestMaps||=[]).push(this)})}catch{}}})
 let gl;
 Object.defineProperty(window,'maplibregl',{configurable:true,get:()=>gl,set(api){
  gl=api;
  api.Map=class extends api.Map{
   constructor(options){
    super(options);
    if(!this.getContainer().classList.contains('video-map-canvas'))return;
    window.rvTestMini=this;
    window.rvNativeCreated=(window.rvNativeCreated||0)+1;
    window.rvNativeResizes=window.rvNativeRemoved=0;
    this.on('resize',()=>window.rvNativeResizes++);
    this.on('remove',()=>window.rvNativeRemoved++);
   }
  };
 }});
};
function stubTiles(route){
 const url=route.request().url(),style=url===SIMPLE_LIGHT?['light']:Object.entries(STYLE_OF).find(([,u])=>url===u);
 if(style)return route.fulfill({json:fixtureStyle(style[0])});
 if(url===OFM+'planet')return route.fulfill({json:{tilejson:'3.0.0',tiles:[OFM+'planet/fixture/{z}/{x}/{y}.pbf'],minzoom:0,maxzoom:14}});
 return route.fulfill({body:Buffer.alloc(0),contentType:'application/x-protobuf'});
}
const nativePoint=(page,a,b=a,k=0)=>page.evaluate(([a,b,k])=>{
 const map=rvTestMini,{x,y}=map.project([a[2]+k*(b[2]-a[2]),a[1]+k*(b[1]-a[1])]),box=map.getContainer().getBoundingClientRect();
 return document.fullscreenElement?.classList.contains('is-turned')?[box.right-y,box.top+x]:[box.left+x,box.top+y];
},[a,b,k]);
const tapNative=async (page,...at)=>page.touchscreen.tap(...await nativePoint(page,...at));
const waitSeek=(page,t,tolerance=.65)=>page.waitForFunction(([t,tol])=>Math.abs(document.getElementById('seek')?.valueAsNumber-t)<tol,[t,tolerance]);
const routeMap=page=>page.evaluateHandle(()=>rvTestMaps.find(map=>map.getContainer().classList.contains('route-map-canvas')));
const assertFollow=async(page,native=false)=>{
 await page.waitForFunction(async native=>{
  const el=document.querySelector(native?'#videoMap':'#routeMapView'),map=native?rvTestMini:rvTestMaps.find(m=>el.contains(m.getContainer()));
  if(map.getZoom()!==16)return false;
  const box=map.getContainer().getBoundingClientRect();
  if(native){
   const car=(await map.getSource('rv-car')?.getData())?.features[0];
   if(!car||map.isMoving())return false;
   const {x,y}=map.project(car.geometry.coordinates);
   return Math.hypot(x-box.width/2,y-box.height/2)<1;
  }
  const car=el.querySelector('.map-car-body').getBoundingClientRect();
  return Math.hypot(car.left+car.width/2-box.left-box.width/2,car.top+car.height/2-box.top-box.height/2)<1;
 },native);
};
const assertToolbar=async (page,selector)=>assert(await page.locator(selector).evaluate(toolbar=>{
 const box=toolbar.getBoundingClientRect(),buttons=[...toolbar.querySelectorAll('button')].map(el=>el.getBoundingClientRect());
 return buttons.every((b,i)=>b.left>=box.left&&b.right<=box.right&&buttons.slice(i+1).every(c=>b.right<=c.left||b.bottom<=c.top||c.right<=b.left||c.bottom<=b.top));
}),'map controls fit without overlap at 390 px');

async function assertMiniCar(page,kind){
 await page.waitForFunction(kind=>{
  const map=rvTestMini;
  if(!map.getLayer('rv-car'))return false;
  const icon=map.getLayoutProperty('rv-car','icon-image'),image=typeof icon==='string'?map.getImage(icon)?.data:null;
  if(!image)return false;
  const alpha=(x,y)=>image.data[4*(y*image.width+x)+3];
  // The upper shoulder of the vehicle is filled, while the arrow only has its tip at that height.
  return alpha(32,10)>200&&alpha(22,10)===(kind==='arrow'?0:255)
   &&map.queryRenderedFeatures({layers:['rv-car']}).some(marker=>marker.properties.bearing===0);
 },kind);
}
async function assertMiniCorner(page,kind='vehicle'){
 await page.locator('#egoSpeed').scrollIntoViewIfNeeded();
 const state=await page.evaluate(()=>{
  const mini=document.getElementById('videoMap'),stage=document.getElementById('videoStage');
  const mapBox=mini.getBoundingClientRect(),stageBox=stage.getBoundingClientRect(),hud=stage.querySelector('.video-hud');
  const map=rvTestMini,car=map.project(map.getCenter()),speed=hud.querySelector('#egoSpeed'),unit=hud.querySelector('.ego-speed span');
  const speedBox=speed.getBoundingClientRect();
  hud.style.pointerEvents='auto';
  const stack=document.elementsFromPoint(speedBox.left+speedBox.width/2,speedBox.top+speedBox.height/2);
  hud.style.removeProperty('pointer-events');
  const mapHit=stack.indexOf(mini.querySelector('.video-map-open'));
  return {top:mapBox.top-stageBox.top,right:stageBox.right-mapBox.right,margin:getComputedStyle(mini).margin,
   mask:getComputedStyle(mini.querySelector('.video-map-surface')).maskImage,
   hudAbove:stack.includes(speed)&&mapHit>stack.indexOf(speed),
   speedShadow:getComputedStyle(speed).textShadow,unitShadow:getComputedStyle(unit).textShadow,
   carBelowHud:mapBox.top+car.y>hud.getBoundingClientRect().bottom};
 });
 assert(Math.abs(state.top)<.5&&Math.abs(state.right)<.5&&state.margin==='0px','3D map is flush with the video corner '+JSON.stringify(state));
 assert.match(state.mask,/#videoMapFade/);
 assert(state.hudAbove&&state.carBelowHud,'the HUD remains above the map and clear of the vehicle '+JSON.stringify(state));
 assert.match(state.speedShadow,/rgba\(0, 0, 0, 0\.55\) 0px 1px 3px/,'speed retains its soft shadow');
 assert.equal(state.unitShadow,state.speedShadow,'the speed unit shares the shadow');
 assert(await page.locator('#videoMap').getByLabel('지도 출처').isHidden());
 await assertMiniCar(page,kind);
}
function setHudPlaying(page,playing){
 // Follow the real HUD observer while the short fixture stays paused.
 return page.evaluate(playing=>{
  document.body.classList.remove('hud-awake');
  document.getElementById('play').dataset.playing=String(playing);
 },playing);
}
function assertReplayHud(page,shown){
 return page.waitForFunction(shown=>['.video-title','.video-tools','.video-fullscreen','.video-map-grip'].every(selector=>{
  const style=getComputedStyle(document.querySelector(selector));
  return style.opacity===(shown?'1':'0')&&(style.pointerEvents!=='none')===shown;
 }),shown);
}
async function assertMiniClose(page){
 const {start,middle,end,closed}=await page.evaluate(async()=>{
  const mini=document.getElementById('videoMap'),map=rvTestMini,canvas=map.getContainer();
  const turned=!!mini.closest('.is-turned'),data=await map.getSource('rv-car').getData();
  const measure=()=>{
   const frame=mini.getBoundingClientRect(),box=canvas.getBoundingClientRect();
   const p=map.project(data.features[0].geometry.coordinates);
   return {w:mini.clientWidth,h:mini.clientHeight,canvasWidth:canvas.clientWidth,
    x:p.x+(turned?box.top-frame.top:box.left-frame.left),y:p.y+(turned?frame.right-box.right:box.top-frame.top),
    held:!!canvas.style.width,resizes:rvNativeResizes};
  };
  const nextFrame=()=>new Promise(requestAnimationFrame);
  const start=measure();
  mini.querySelector('.video-map-close').click();
  const transitions=mini.getAnimations().filter(animation=>animation instanceof CSSTransition);
  if(!transitions.some(animation=>animation.transitionProperty==='width'))throw Error('expected an animated minimap close');
  // Sample the clipping frame at fixed points even when a busy runner skips a rendered frame.
  for(const animation of transitions){animation.pause();animation.currentTime=animation.effect.getTiming().duration/2}
  while(map.isMoving())await nextFrame();
  const middle=measure();
  for(const animation of transitions)animation.currentTime=animation.effect.getTiming().duration-1;
  const end=measure();
  for(const animation of transitions)animation.play();
  await Promise.all(transitions.map(animation=>animation.finished));
  await nextFrame();await nextFrame();
  return {start,middle,end,closed:measure()};
 });
 const samples=JSON.stringify({start,middle,end,closed});
 assert(middle.w<start.w&&middle.w>closed.w&&middle.held&&middle.canvasWidth===start.w,'the full canvas stays centered behind the shrinking frame '+samples);
 assert(middle.x<middle.w&&middle.x>0&&middle.y<middle.h&&middle.y>0,'the car stays within the shrinking viewport '+samples);
 assert.equal(end.resizes,start.resizes,'the canvas does not resize before the transition finishes');
 assert.equal(closed.resizes,start.resizes+1,'closing resizes the WebGL canvas once');
 assert(!closed.held&&Math.abs(closed.x-closed.w*.7)<1&&Math.hypot(end.x-closed.x,end.y-closed.y)<2,'the car keeps its position when the held canvas is released '+samples);
}
async function assertMiniFade(page,mini){
 // Use solid layers to measure the actual composited SVG alpha independently of tiles, HUD and video content.
 const probe=await page.addStyleTag({content:`
  #videoStage{background:#000}
  #videoStage>:not(#videoMap),#videoMap .video-map-canvas{visibility:hidden}
  #videoMap .video-map-surface{background:#fff}
 `});
 let png;
 try{png=await mini.screenshot({scale:'css'})}finally{await probe.evaluate(el=>el.remove())}
 const light=await page.evaluate(async png=>{
  const img=await createImageBitmap(await (await fetch('data:image/png;base64,'+png)).blob());
  const {width:w,height:h}=img,s=Math.min(w,h);
  const canvas=new OffscreenCanvas(w,h),ctx=canvas.getContext('2d');ctx.drawImage(img,0,0);
  const at=(x,y)=>ctx.getImageData(Math.floor(x),Math.floor(y),1,1).data[0];
  return {top:at(w-.15*s,1),right:at(w-2,.15*s),left:[1,.37*s,.72*s].map(x=>at(x,.14*s)),
   bottom:[1,.37*s,.72*s].map(y=>at(w-.12*s,h-y)),corner:at(.37*s,h-.37*s),grip:at(11,h-20)};
 },png.toString('base64'));
 assert(light.top>=254&&light.right>=254,'video edges stay opaque '+JSON.stringify(light));
 for(const edge of [light.left,light.bottom]){
  assert(edge[0]<8&&edge[1]>145&&edge[1]<225&&edge[2]>240&&edge[1]-edge[0]>edge[2]-edge[1],'inner edges follow a nonlinear accelerated fade '+JSON.stringify(light));
 }
 assert(light.corner<45,'the feather follows a rounded lower-left corner '+JSON.stringify(light));
 assert(light.grip>130,'the thin resize grip paints over the transparent corner '+JSON.stringify(light));
}

(async()=>{
 const dir=fs.mkdtempSync(path.join(os.tmpdir(),'rv-maps-')),shots=process.env.RV_MAP_SHOTS,verifyShots=process.env.RV_TESLA_SHOTS;
 if(shots)fs.mkdirSync(shots,{recursive:true});
 if(verifyShots)fs.mkdirSync(verifyShots,{recursive:true});
 const screenshot=async(page,name)=>{
  if(!verifyShots)return;
  await settleLayout(page);
  if(REAL)await page.waitForFunction(()=>rvTestMini.getSource('openmaptiles')&&rvTestMini.areTilesLoaded());
  await page.screenshot({path:path.join(verifyShots,name+'.png')});
 };
 fs.writeFileSync(path.join(dir,'.processing-settings.json'),JSON.stringify({auto_convert:false}));
 // Route A: eight segments winding north-east (segments 1 and 2 have video); route B: one segment elsewhere; route C: no GPS.
 const track=(lat,lon,dlat,dlon)=>Array.from({length:61},(_,i)=>[i,+(lat+i*dlat).toFixed(6),+(lon+i*dlon+Math.sin(i/9)*2e-5).toFixed(6),12.5,Math.round(Math.atan2(dlon,dlat)*180/Math.PI+360)%360]);
 const turns=[[8e-5,6e-5],[0,1e-4],[7e-5,4e-5],[1e-4,0],[6e-5,-5e-5],[0,9e-5],[8e-5,3e-5],[5e-5,7e-5]];
 const routeA=[];{let [lat,lon]=ORIGIN;turns.forEach(([dlat,dlon],k)=>{routeA.push(track(lat,lon,dlat,dlon));lat=+(lat+60*dlat).toFixed(6);lon=+(lon+60*dlon).toFixed(6)})}
 // Segment 1 begins with a turn through north. Its sample at 31 s repeats the fix of 30 s while moving.
 routeA[1].forEach((p,i)=>{p[4]=(350+3*i)%360});
 for(let i=1;i<=5;i++){
  const prev=routeA[1][i-1],angle=prev[4]*Math.PI/180;
  routeA[1][i][1]=prev[1]+Math.cos(angle)*.0001;
  routeA[1][i][2]=prev[2]+Math.sin(angle)*.0001/Math.cos(prev[1]*Math.PI/180);
 }
 // A receiver heading that smoothly interpolates through a right turn from 10 s to 11 s.
 for(let i=6;i<routeA[1].length;i++)routeA[1][i][4]=i<=10?0:90;
 routeA[1][31][1]=routeA[1][30][1];routeA[1][31][2]=routeA[1][30][2];
 routeA[1][42][3]=0;routeA[1][42][4]=210;
 routeA[1][45][3]=40;
 const hex=k=>'0123456789'[k];
 const logs=[
  ...routeA.map((points,k)=>({id:hex(k).repeat(32),name:`00000395--0d0eda17c5 / 구간 ${k}`,uploaded:30+k,points,tags:k===0?[{name:'출근',color:'blue'}]:[],video:k===1||k===2})),
  {id:'c'.repeat(32),name:'00000396--1a2b3c4d5e / 구간 0',uploaded:20,points:track(ORIGIN[0]+.03,ORIGIN[1]+.03,-6e-5,-4e-5),tags:[{name:'주말',color:'rose'}]},
  {id:'d'.repeat(32),name:'00000397--9f8e7d6c5b / 구간 0',uploaded:10,points:null},
 ];
 const A=routeA.length,withVideo=logs[1].id;
 const savedBookmark={id:'ab'.repeat(16),time:8.375,title:'저장된 교차로',note:''};
 const writeLog=log=>{
  const folder=path.join(dir,log.id);fs.mkdirSync(path.join(folder,'prepared'),{recursive:true});
  fs.writeFileSync(path.join(folder,'rlog.zst'),'fixture');
  fs.writeFileSync(path.join(folder,'meta.json'),JSON.stringify({id:log.id,status:'ready',name:log.name,uploaded:log.uploaded,files:{},video:false,tags:log.tags||[],decoder_version:'v28-runtime-geometry',duration:60,bookmarks:log.id===withVideo?[savedBookmark]:[]}));
  fs.writeFileSync(path.join(folder,'prepared/data.json'),JSON.stringify({route:log.name,key:'map-'+log.id[0],duration:60,warnings:[],video:log.video?{start:0,duration:60}:null,frames:Array.from({length:61},(_,i)=>({t:i,id:i,valid:true,egoSpeedKph:(log.points?.[i]?.[3]??0)*3.6,lanes:[],edges:[],lp:[],es:[],leads:[]}))}));
  const summary={duration:60,warnings:[],model_frames:61,video:null,videos:{},started_at:1762400000+log.uploaded*60,start_v:3,track_v:2,has_track:!!log.points};
  if(log.points){summary.track_distance_m=900;fs.writeFileSync(path.join(folder,'prepared/track.json.gz'),zlib.gzipSync(JSON.stringify({v:2,source:'gpsLocationExternal',points:log.points})))}
  fs.writeFileSync(path.join(folder,'prepared/summary.json'),JSON.stringify(summary));
 };
 logs.forEach(writeLog);
 const python=spawn(process.env.RV_PYTHON||'python',['-u','-c',`import sys
sys.path.insert(0,'roadviewer/app')
import server
from werkzeug.serving import make_server
from werkzeug.middleware.dispatcher import DispatcherMiddleware
http=make_server('127.0.0.1',0,DispatcherMiddleware(server.app,{'/api/hassio_ingress/test':server.app}),threaded=True)
print('READY '+str(http.server_port),flush=True)
http.serve_forever()`],{env:{...process.env,RV_DATA:dir,RV_INGRESS_ONLY:'0'},stdio:['ignore','pipe','pipe']});
 let stderr='',browser;python.stderr.on('data',x=>stderr+=x);
 try{
  const port=await new Promise((resolve,reject)=>{const timer=setTimeout(()=>reject(Error('Server timeout '+stderr)),15000);python.once('exit',code=>{clearTimeout(timer);reject(Error('Server exit '+code+stderr))});python.stdout.on('data',x=>{const m=/READY (\d+)/.exec(String(x));if(m){clearTimeout(timer);resolve(Number(m[1]))}})});
  const base=`http://127.0.0.1:${port}/api/hassio_ingress/test/`;
  const video=fs.readFileSync(process.env.RV_TEST_VIDEO||'/tmp/roadviewer-test.mp4');
  browser=await chromium.launch({headless:true});
  for(const scheme of PARTS.includes('main')?SCHEMES:[]){
   const context=await browser.newContext({viewport:{width:390,height:844},hasTouch:true,isMobile:true,colorScheme:scheme,reducedMotion:'reduce'}),page=await context.newPage(),errors=[],tiles=[],stubbed=[];
   page.on('pageerror',error=>errors.push(error.message));await context.addInitScript(hookMaps);
   page.on('request',request=>{if(request.url().startsWith(OFM))tiles.push(request.url())});
   if(REAL)page.on('console',message=>{if(message.type()==='error')console.log('maps: console',message.text().slice(0,200))});
   else await context.route(OFM+'**',route=>{
    const url=route.request().url();stubbed.push(url);const style=url===SIMPLE_LIGHT?['light']:Object.entries(STYLE_OF).find(([,u])=>url===u);
    if(style)return route.fulfill({body:JSON.stringify(fixtureStyle(style[0])),contentType:'application/json'});
    if(url===OFM+'planet')return route.fulfill({body:JSON.stringify({tilejson:'3.0.0',tiles:[OFM+'planet/fixture/{z}/{x}/{y}.pbf'],minzoom:0,maxzoom:14}),contentType:'application/json'});
    return route.fulfill({body:Buffer.alloc(0),contentType:'application/x-protobuf'});  // tiles, glyphs: empty
   });
   await context.route(`**/api/logs/${withVideo}/video**`,route=>route.fulfill({body:video,contentType:'video/mp4'}));
   // Real tiles: give them time to arrive and draw before the picture.
   const shot=async name=>{if(shots){await settleLayout(page);await page.waitForTimeout(REAL?3000:150);await page.screenshot({path:path.join(shots,`${name}-${scheme}.png`)})}};
   // The theme's style (Liberty light, Dark dark), never the other one, for the maps that follow the theme.
   const tilesUsed=()=>{const styles=tiles.filter(url=>url.startsWith(OFM+'styles/'));assert(styles.length&&styles.every(url=>url===STYLE_OF[scheme]),`${scheme} styles: ${styles}`);
    if(!REAL)assert(stubbed.includes(OFM+'planet')&&stubbed.some(url=>url.startsWith(OFM+'planet/fixture/')),'the vector source and its tiles come from the stub: '+stubbed.slice(0,4))};
   // Taps the middle of a drawn route line (an SVG path of Leaflet's overlay pane).
   // The first point from the middle outwards that no map control covers.
   const tapLine=async(locator,share=.5)=>{const point=await locator.evaluate((p,share)=>{const m=p.getScreenCTM(),length=p.getTotalLength();
     // Away from the controls too: Chromium moves a tap onto a button or link within a few pixels.
     const controls=[...document.querySelectorAll('.leaflet-control')].map(c=>c.getBoundingClientRect()),near=(x,y)=>controls.some(r=>x>r.left-24&&x<r.right+24&&y>r.top-24&&y<r.bottom+24);
     const shares=Array.from({length:21},(_,i)=>i/20).sort((a,b)=>Math.abs(a-share)-Math.abs(b-share));
     for(const k of shares){const at=p.getPointAtLength(length*Math.min(1,Math.max(0,k))),x=Math.round(at.x*m.a+at.y*m.c+m.e),y=Math.round(at.x*m.b+at.y*m.d+m.f);if(document.elementFromPoint(x,y)===p&&!near(x,y))return {x,y}}
     const at=p.getPointAtLength(length*share);return {x:at.x*m.a+at.y*m.c+m.e,y:at.x*m.b+at.y*m.d+m.f}},share);await page.touchscreen.tap(point.x,point.y)};
   const listRow=id=>page.locator(`#list .log-row[data-id="${id}"]`);
   let listAttempts=0,driveAttempts=0;
   if(scheme==='dark'){
    await page.route('**/api/tracks',route=>++listAttempts===1?route.fulfill({status:503}):route.continue());
    await page.route(`**/api/logs/${logs[0].id}/track`,route=>++driveAttempts===1?route.fulfill({status:503}):route.continue());
   }
   await page.goto(base);await page.locator('.log-route').first().waitFor();
   assert(!await page.evaluate(()=>!!(window.L||window.maplibregl)),'Leaflet and MapLibre load only when a map is first shown');
   // ② The cards with GPS show their route (SVGs, no tiles); phones: a 지도 button left of 재생, no route in the thumbnail.
   // The 지도 buttons show while /api/tracks is on its way (no shape yet); maps.js draws every card's shape in one pass
   // when the routes arrive, so the first drawn shape is the point where the cards are complete.
   if(scheme==='dark')await page.locator('.route-map-button').first().waitFor();
   await page.locator('.route-map-button svg .shape-line').first().waitFor();
   if(scheme==='dark')assert(listAttempts>=2,'card maps recovered from a 503 without opening the overview');
   assert.equal(await page.locator('.route-map-button').count(),2);assert.equal(await page.locator('.route-map-button svg .shape-line').count(),2);
   assert.equal(await page.locator('.log-route[data-route*="00000397"] :is(.route-inset,.route-strip,.route-map-button)').count(),0,'no GPS: no route at all');
   // Both boxes from one layout once the row has stopped moving (two reads in turn once caught the list between
   // layouts: the button 56 px below 재생).
   {const card=page.locator('.log-route[data-route*="00000395"]');await settleLayout(page);await waitForSteadyLayout(page,'.log-route[data-route*="00000395"] :is(.route-map-button,.log-route-actions a.replay)');
    const {button,play}=await card.evaluate(e=>{const box=s=>{const r=e.querySelector(s).getBoundingClientRect();return {x:r.x,y:r.y,width:r.width,height:r.height}};return {button:box('.route-map-button'),play:box('.log-route-actions a.replay')}});
    assert(button.height>=44&&button.x+button.width<=play.x&&play.x-(button.x+button.width)<=12&&Math.abs(button.y+button.height/2-play.y-play.height/2)<2,JSON.stringify({button,play}));
    assert.equal(await card.locator('.route-map-button').innerText(),'지도');
    // The empty left of the actions row folds the card (지도 sits beside 재생).
    {const actions=card.locator('.log-route-actions'),open=await card.evaluate(e=>e.open);
     const position={x:12,y:(await actions.boundingBox()).height/2};await actions.tap({position});
     await page.waitForFunction(([id,was])=>document.querySelector(`.log-route[data-route*="${id}"]`).open!==was,['00000395',open]);
     await actions.tap({position});await page.waitForFunction(([id,was])=>document.querySelector(`.log-route[data-route*="${id}"]`).open===was,['00000395',open])}
    assert(await card.locator('.route-inset').isHidden()&&await card.locator('.route-strip').isHidden(),'phones: nothing in the thumbnail');
    assert.equal(await card.locator('.route-inset svg, .route-strip svg').count(),0,'phones skip unused desktop route SVGs');
    if(shots){await card.evaluate(e=>e.scrollIntoView({block:'center'}));await settleLayout(page);await card.screenshot({path:path.join(shots,`card-phone-${scheme}.png`)})}}
   assert.equal(tiles.length,0,'the shape draws no tiles');
   const cardA=page.locator('.log-route[data-route*="00000395"]'),drive=page.locator('#driveMap');
   const driveLines=drive.locator('.leaflet-overlay-pane path.leaflet-interactive');
   const driveOpen=async marked=>{
    await page.waitForFunction(({count,marked})=>{const d=document.getElementById('driveMap');return d.open&&d.querySelectorAll('.leaflet-overlay-pane path.leaflet-interactive').length===count&&(!marked||d.querySelector(`.log-row[data-id="${marked}"]`)?.classList.contains('is-map-marked'))},{count:A,marked});
    await settleLayout(page,'#driveMap');
   };
   const driveClosed=()=>page.waitForFunction(()=>!document.getElementById('driveMap').open&&!document.querySelector('#driveMap canvas.maplibregl-canvas'));
   // The geometry that matters: the map stays on screen above the sheet and the marked line lies inside it.
   const layout=()=>page.evaluate(()=>{
    const d=document.getElementById('driveMap'),r=s=>d.querySelector(s).getBoundingClientRect(),area=r('.drive-map-area'),bar=r('.drive-map-bar'),sheet=r('.drive-map-sheet'),list=r('.drive-map-list'),body=r('.drive-map-body');
    const marked=d.querySelector('.log-row.is-map-marked'),row=marked?.getBoundingClientRect();
    const strong=[...d.querySelectorAll('.leaflet-overlay-pane path.leaflet-interactive')].find(p=>Number(p.getAttribute('stroke-width'))===7)?.getBoundingClientRect();
    return {area:{top:Math.round(area.top),bottom:Math.round(area.bottom),h:Math.round(area.height)},bar:Math.round(bar.bottom),body:Math.round(body.height),sheet:Math.round(sheet.height),share:sheet.height/body.height,
     rowVisible:!!row&&row.top>=list.top-1&&row.bottom<=list.bottom+1,lineInside:!!strong&&strong.top>=area.top-1&&strong.bottom<=sheet.top+15&&strong.left>=area.left-1&&strong.right<=area.right+1,scrolled:d.querySelector('.drive-map-list').scrollTop};
   });
   // Opened from the card's route shape: a history entry with the drive's date, distance, duration and segments.
   await cardA.locator('.route-map-button').tap();await driveOpen();
   if(scheme==='dark')assert(driveAttempts>=2,'the drive dialog recovered from a 503 without closing');
   assert.equal(new URL(page.url()).hash,'#drive-map');
   assert.match(await page.locator('#driveMapTitle').innerText(),/\d/);
   // Opening focuses the title (not the back button) and shows no focus ring.
   assert.deepEqual(await page.evaluate(()=>({id:document.activeElement.id,ring:!!document.querySelector('#driveMap :focus-visible'),outline:getComputedStyle(document.activeElement).outlineStyle})),{id:'driveMapTitle',ring:false,outline:'none'});
   assert.equal(await page.locator('#driveMapFacts').innerText(),`${(A*0.9).toFixed(1)} km · ${A}:00 · ${A}개 구간`);
   assert.equal(await drive.locator('.drive-map-list>.log-row').count(),A,'one row per segment, in segment order');
   assert.deepEqual(await drive.locator('.drive-map-list>.log-row').evaluateAll(rows=>rows.map(r=>r.dataset.id[0])),logs.slice(0,A).map(log=>log.id[0]));
   assert.equal(await drive.locator('.drive-map-list .log-row a.replay').count(),A,'each row keeps its play button');
   assert.equal(await drive.locator('.drive-map-list .log-row .recording-more').count(),A,'and its ⋯ menu');
   assert.equal(await drive.locator('.leaflet-control-attribution').innerText(),'OpenFreeMap © OpenMapTiles Data from OpenStreetMap');
   assert.equal(await drive.locator('.leaflet-tile-pane canvas.maplibregl-canvas').count(),1,'MapLibre draws the basemap in the tile pane');
   {const l=await layout();assert(l.area.top>=l.bar-1&&l.area.h>=.28*844,JSON.stringify(l));assert(Math.abs(l.share-.54)<.02,JSON.stringify(l))}
   // A row's ⋯ menu in the sheet opens to the side with room (the list scrolls it clear when neither has one) and lies
   // over the rows after it, never cut by the sheet's edges (in a low sheet it slides over its own button).
   const menuClear=async id=>{const row=drive.locator(`.log-row[data-id="${id}"]`);await row.locator('.recording-more>summary').tap();await page.waitForTimeout(450);
    const g=await row.evaluate(r=>{const m=r.querySelector('.recording-menu'),menu=m.getBoundingClientRect(),list=r.closest('.drive-map-list').getBoundingClientRect();
     return {inside:menu.top>=list.top-.5&&menu.bottom<=list.bottom+.5,over:[menu.top+10,(menu.top+menu.bottom)/2,menu.bottom-10].every(y=>m.contains(document.elementFromPoint(menu.left+30,y))),menu:[menu.top,menu.bottom].map(Math.round),list:[list.top,list.bottom].map(Math.round)}});
    await row.evaluate(r=>{r.querySelector('.recording-more').open=false});await page.waitForTimeout(100);assert(g.inside&&g.over,'menu '+id[0]+' '+JSON.stringify(g))};
   for(const k of [0,2,3])await menuClear(logs[k].id);
   await drive.locator('.drive-map-list').evaluate(list=>{list.scrollTop=0});
   // A row tapped in the sheet comes to the middle of the sheet (as far as the sheet can scroll).
   {const mid=logs[Math.floor(A/2)].id;await drive.locator(`.log-row[data-id="${mid}"] .log-meta`).tap();
    await page.waitForFunction((([list,row])=>{const l=document.querySelector(list),r=l?.querySelector(row);if(!r)return false;const a=l.getBoundingClientRect(),b=r.getBoundingClientRect(),off=(b.top+b.height/2)-(a.top+a.height/2);return Math.abs(off)<6||(off<0&&l.scrollTop<=1)||(off>0&&l.scrollTop>=l.scrollHeight-l.clientHeight-1)}),['#driveMap .drive-map-list',`.log-row[data-id="${mid}"]`])}
   // A LOWER row with the sheet scrolled: its segment is marked and fitted on the map, which stays visible above.
   await drive.locator('.drive-map-list').evaluate(list=>{list.scrollTop=list.scrollHeight});
   const last=logs[A-1].id,before=page.url();
   await drive.locator(`.log-row[data-id="${last}"] .log-meta`).tap();
   await page.waitForFunction(id=>document.querySelector(`#driveMap .log-row[data-id="${id}"]`)?.classList.contains('is-map-marked'),last);
   assert.equal(page.url(),before,'a row tap marks instead of playing');
   await settleLayout(page,'#driveMap');
   {const l=await layout();assert(l.scrolled>0&&l.rowVisible&&l.lineInside&&l.area.h>=.28*844,JSON.stringify(l))}
   assert.deepEqual((await driveLines.evaluateAll(lines=>lines.map(l=>Number(l.getAttribute('stroke-opacity'))>.5))).filter(Boolean).length,1,'the other segments dim');
   await shot('drive');
   // A tap on the map's empty ground shows the whole drive again; a tap on an upper segment then marks its row and
   // scrolls the sheet back to it.
   {const box=await drive.locator('.drive-map-area').boundingBox();await page.touchscreen.tap(box.x+14,box.y+40)}
   await page.waitForFunction(()=>!document.querySelector('#driveMap .log-row.is-map-marked'));await page.waitForTimeout(100);
   // Every map folds its credit into an ⓘ from the start.
   assert(await drive.locator('.leaflet-control-attribution').isHidden()&&await drive.locator('.rv-credit-toggle').isVisible(),'the credit is folded');
   await tapLine(driveLines.nth(1),.5);
   await page.waitForFunction(id=>document.querySelector(`#driveMap .log-row[data-id="${id}"]`)?.classList.contains('is-map-marked'),logs[1].id);
   await page.waitForTimeout(100);{const l=await layout();assert(l.rowVisible,JSON.stringify(l))}
   // The sheet's handle: dragged up the sheet grows to 70% and the map keeps ≥ 30%; dragged down it snaps to 36%.
   const drag=async dy=>{const box=await drive.locator('.drive-map-handle').boundingBox(),x=box.x+box.width/2,y=box.y+box.height/2;await page.mouse.move(x,y);await page.mouse.down();await page.mouse.move(x,y+dy/2,{steps:4});await page.mouse.move(x,y+dy,{steps:4});await page.mouse.up();await page.waitForTimeout(120)};
   // Raising or lowering the sheet does not move the map (only a tap on a card does): the drawn route stays put.
   const routeBox=()=>page.evaluate(()=>{const b=document.querySelector('#driveMap .leaflet-overlay-pane path').getBoundingClientRect();return [b.left,b.top,b.width,b.height].map(Math.round)});
   const routeBefore=await routeBox();
   await drag(-160);{const l=await layout();assert(Math.abs(l.share-.7)<.02&&l.area.h>=.26*l.body,JSON.stringify(l))}
   await page.waitForTimeout(450);{const after=await routeBox();assert(after.every((v,i)=>Math.abs(v-routeBefore[i])<=1),'the sheet moved the map: '+JSON.stringify({routeBefore,after}))}
   await drive.locator(`.log-row[data-id="${last}"] .log-meta`).tap();await page.waitForTimeout(100);
   {const l=await layout();assert(l.rowVisible&&l.lineInside,JSON.stringify(l))}
   await shot('drive-up');
   // Dragged all the way down the cards hide and only the handle stays; a tap brings them back at the middle height.
   await drag(400);{const g=await page.evaluate(()=>{const b=document.querySelector('#driveMap .drive-map-body'),h=document.querySelector('#driveMap .drive-map-handle'),row=document.querySelector('#driveMap .drive-map-list>.log-row.is-seen');  // a card that was in sight (the others are left alone, clipped)
return {hidden:b.classList.contains('is-sheet-hidden'),now:h.getAttribute('aria-valuenow'),text:h.getAttribute('aria-valuetext'),rowOpacity:getComputedStyle(row).opacity,handle:h.getBoundingClientRect().bottom<=innerHeight}});
    assert(g.hidden&&g.now==='0'&&g.text==='숨김'&&g.rowOpacity==='0'&&g.handle,JSON.stringify(g))}
   await shot('drive-hidden');
   // The ⓘ in the corner is not under the clear sheet: a tap opens the credit, another folds it.
   {const toggle=drive.locator('.rv-credit-toggle'),credit=drive.locator('.leaflet-control-attribution');
    assert(await toggle.evaluate(t=>{const b=t.getBoundingClientRect();return t.contains(document.elementFromPoint(b.left+b.width/2,b.top+b.height/2))}),'nothing covers the ⓘ');
    await toggle.tap();await credit.waitFor();assert.equal(await toggle.getAttribute('aria-expanded'),'true');await toggle.tap();await credit.waitFor({state:'hidden'})}
   // The marked segment's pill sits on the hidden sheet; a tap brings the cards back and the pill leaves from where it
   // is (riding up with the sheet's own motion, no jump to the new height) — it is leaving right away.
   {const pill=drive.locator('.drive-map-card');assert(await pill.isVisible(),'the pill shows while the cards are away');
    await page.emulateMedia({reducedMotion:'no-preference'});  // this check is about the motion itself
    const top0=(await pill.boundingBox()).y;
    const next=await page.evaluate(()=>new Promise(resolve=>{document.querySelector('#driveMap .drive-map-handle').click();requestAnimationFrame(()=>{const p=document.querySelector('#driveMap .drive-map-card');resolve({top:p.getBoundingClientRect().top,leaving:p.classList.contains('is-leaving')||p.hidden})})}));
    assert(next.leaving&&next.top>0&&Math.abs(next.top-top0)<40,'the pill leaves without a jump: '+JSON.stringify({top0,next}))}
   await page.waitForTimeout(400);await page.emulateMedia({reducedMotion:'reduce'});{const l=await layout();assert(Math.abs(l.share-.54)<.02,JSON.stringify(l));assert(await drive.locator('.drive-map-card').isHidden(),'the pill has gone')}
   await drag(-30);{const l=await layout();assert(Math.abs(l.share-.54)<.02,'a short drag snaps back: '+JSON.stringify(l))}
   // A tap folds and unfolds like a disclosure: shown (at any height) -> hidden, hidden -> the middle height.
   {const hidden=()=>page.evaluate(()=>document.querySelector('#driveMap .drive-map-body').classList.contains('is-sheet-hidden'));
    await drive.locator('.drive-map-handle').tap();await page.waitForTimeout(150);assert(await hidden(),'a tap on a shown sheet hides it');
    await drive.locator('.drive-map-handle').tap();await page.waitForTimeout(150);{const l=await layout();assert(!await hidden()&&Math.abs(l.share-.54)<.02,'a tap on the hidden sheet brings the middle height: '+JSON.stringify(l))}
    await drag(-160);await drive.locator('.drive-map-handle').tap();await page.waitForTimeout(150);assert(await hidden(),'from the top height too, a tap hides it')
    await drive.locator('.drive-map-handle').tap();await page.waitForTimeout(150)}
   // The system back closes it (back in the list, the card's shape focused again).
   await page.goBack();await driveClosed();assert.notEqual(new URL(page.url()).hash,'#drive-map');
   assert(await page.locator('#list').isVisible());
   // ⋯ › 지도에서 보기 (only for segments with GPS) opens the same view with that segment marked; Escape closes it.
   await cardA.locator('.log-route-chevron').tap();await settleLayout(page);
   assert.equal(await listRow(logs[A+1].id).locator('button.map-segment').count(),0);
   {const row=listRow(logs[5].id);await row.locator('.recording-more>summary').tap();await row.getByRole('button',{name:'지도에서 보기',exact:true}).tap()}
   await driveOpen(logs[5].id);{const l=await layout();assert(l.rowVisible&&l.lineInside,JSON.stringify(l))}
   await page.keyboard.press('Escape');await driveClosed();assert.notEqual(new URL(page.url()).hash,'#drive-map');
   // The header's back button closes it too.
   await cardA.locator('.route-map-button').tap();await driveOpen();await page.locator('#driveMapBack').tap();await driveClosed();
   // Wide screens: the map fills the screen under the header; the list floats over its right side as a glass card
   // (16px in, its own scroll), the zoom buttons and attribution beside it, a marked segment fitted left of it.
   await page.setViewportSize({width:1280,height:800});await cardA.locator('.route-map-button').tap();await driveOpen();
   {const g=await page.evaluate(()=>{const r=s=>document.querySelector('#driveMap '+s).getBoundingClientRect(),area=r('.drive-map-area'),sheet=r('.drive-map-sheet'),bar=r('.drive-map-bar'),zoom=r('.leaflet-control-zoom'),credit=r('.rv-credit-toggle');
     return {full:area.left<=1&&area.right>=innerWidth-1&&area.top>=bar.bottom-1&&area.bottom>=innerHeight-1,inset:[innerWidth-sheet.right,sheet.top-bar.bottom,innerHeight-sheet.bottom].map(Math.round),w:Math.round(sheet.width),
      clear:zoom.right<=sheet.left-8&&area.right-credit.right>=0&&area.right-credit.right<=8&&area.bottom-credit.bottom>=0&&area.bottom-credit.bottom<=8&&sheet.bottom<=credit.top,glass:getComputedStyle(document.querySelector('#driveMap .drive-map-list>.log-row')).backdropFilter!=='none'&&getComputedStyle(document.querySelector('#driveMap .drive-map-sheet')).backgroundColor==='rgba(0, 0, 0, 0)',scrolls:getComputedStyle(document.querySelector('#driveMap .drive-map-list')).overflowY}});
    assert(g.full&&g.inset[0]===16&&g.inset[1]===16&&g.inset[2]>=16&&g.w>=320&&g.w<=400&&g.clear&&g.glass&&g.scrolls==='auto',JSON.stringify(g))}
   await drive.locator(`.log-row[data-id="${logs[3].id}"] .log-meta`).click();await page.waitForTimeout(150);
   {const g=await page.evaluate(()=>{const d=document.getElementById('driveMap'),sheet=d.querySelector('.drive-map-sheet').getBoundingClientRect(),line=[...d.querySelectorAll('.leaflet-overlay-pane path.leaflet-interactive')].find(p=>Number(p.getAttribute('stroke-width'))===7)?.getBoundingClientRect();return {left:!!line&&line.right<=sheet.left+1,line:line&&[line.left,line.right].map(Math.round),sheet:Math.round(sheet.left)}});
    assert(g.left,'the marked segment is fitted beside the card '+JSON.stringify(g))}
   await shot('drive-wide');
   await page.keyboard.press('Escape');await driveClosed();await page.setViewportSize({width:390,height:844});await settleLayout(page);
   // ① 목록 | 지도 in the logs panel's header: every drive the list shows, one colour each, fitted.
   const switcher=page.locator('#logView');await switcher.waitFor();
   assert.deepEqual(await switcher.locator('button').allTextContents(),['목록','지도']);
   {const head=await page.locator('.lib-panel-head').first().evaluate(h=>({scroll:h.scrollWidth<=h.clientWidth+1,page:document.documentElement.scrollWidth<=innerWidth}));assert.deepEqual(head,{scroll:true,page:true})}
   await switcher.getByRole('button',{name:'지도'}).tap();
   await page.waitForFunction(()=>document.querySelectorAll('#logMap .leaflet-overlay-pane path.leaflet-interactive').length===2);
   assert(await page.locator('#list').isHidden());
   const routeLines=page.locator('#logMap .leaflet-overlay-pane path.leaflet-interactive');
   const colours=await routeLines.evaluateAll(lines=>lines.map(l=>l.getAttribute('stroke')));assert.notEqual(colours[0],colours[1],'each drive in its own colour');
   assert.equal(await page.locator('#logMap .leaflet-control-attribution').innerText(),'OpenFreeMap © OpenMapTiles Data from OpenStreetMap');
   await page.locator('#logMap').scrollIntoViewIfNeeded();
   // Phones: the drives are a bottom sheet like the drive map's: a small glass handle with the count over the same glass
   // cards, at the middle height to start (no label, nothing autofocused).
   // The whole map on screen, clear of the header and the fixed tab bar.
   await page.locator('#logMap').evaluate(m=>m.scrollIntoView({block:'center'}));await page.waitForTimeout(100);
   {const handle=page.locator('#logMap .log-map-handle'),rows=page.locator('#logMapRows .map-list-row'),mapCard=page.locator('#logMapCard');
    const sheetState=()=>page.evaluate(()=>{const m=document.getElementById('logMap'),r=m.getBoundingClientRect(),s=document.getElementById('logMapList').getBoundingClientRect(),h=m.querySelector('.log-map-handle'),row=m.querySelector('#logMapRows>.map-list-row');
     return {hidden:m.classList.contains('is-sheet-hidden'),share:s.height/r.height,bottom:Math.round(r.bottom-s.bottom),now:h.getAttribute('aria-valuenow'),text:h.getAttribute('aria-valuetext'),rowOpacity:getComputedStyle(row).opacity}});
    const lineBox=(width=null)=>page.evaluate(width=>{const p=[...document.querySelectorAll('#logMap .leaflet-overlay-pane path.leaflet-interactive')].find(p=>width===null||Number(p.getAttribute('stroke-width'))===width),b=p.getBoundingClientRect();return [b.left,b.top,b.width,b.height].map(Math.round)},width);
    const drag=async dy=>{const box=await handle.boundingBox(),x=box.x+box.width/2,y=box.y+box.height/2;await page.mouse.move(x,y);await page.mouse.down();await page.mouse.move(x,y+dy/2,{steps:4});await page.mouse.move(x,y+dy,{steps:4});await page.mouse.up();await page.waitForTimeout(120)};
    const creditFree=()=>page.locator('#logMap .rv-credit-toggle').evaluate(t=>{const b=t.getBoundingClientRect();return t.contains(document.elementFromPoint(b.left+b.width/2,b.top+b.height/2))});
    assert.deepEqual((await rows.locator('.map-list-pick b').allTextContents()).map(title=>title.match(/^주행 \d+ · (?=\d)/)?.[0]).sort(),['주행 395 · ','주행 396 · ']);
    assert.equal((await handle.innerText()).trim(),'주행 목록 2개');assert(await handle.isVisible()&&await page.locator('#logMapList .log-map-list-head').isHidden());
    // The phone map reaches down to just above the bottom tab bar (whatever sits above it), with the page at the top.
    {const g=await page.evaluate(()=>{const m=document.getElementById('logMap').getBoundingClientRect(),t=document.querySelector('.lib-tabs').getBoundingClientRect();return {gap:Math.round(t.top-(m.top+window.scrollY+m.height)),h:Math.round(m.height)}});
     assert(g.gap>=8&&g.gap<=16,'the phone map ends just above the tab bar '+JSON.stringify(g))}
    // A short screen (a Fold's inner one): the map is taller than what is left under the header; the page stays where it
    // is, and scrolled until the chips reach the app bar the map ends just above the tab bar.
    {const size=page.viewportSize();await page.setViewportSize({width:476,height:670});
     await page.locator('#logView button[data-view="list"]').click();await page.locator('#logView button[data-view="map"]').click();
     await page.waitForTimeout(500);assert.equal(await page.evaluate(()=>scrollY),0,'opening the map does not scroll the page');
     await page.evaluate(()=>{const c=document.getElementById('logChips').getBoundingClientRect(),a=document.querySelector('.lib-appbar').getBoundingClientRect();scrollTo(0,c.top-a.bottom-8)});await page.waitForTimeout(200);
     const g=await page.evaluate(()=>{const m=document.getElementById('logMap').getBoundingClientRect(),t=document.querySelector('.lib-tabs').getBoundingClientRect(),c=document.getElementById('logChips').getBoundingClientRect(),a=document.querySelector('.lib-appbar').getBoundingClientRect();
      return {h:Math.round(m.height),gap:Math.round(t.top-m.bottom),chips:Math.round(c.top-a.bottom),chipsShown:c.bottom<=m.top+1,scrollY:Math.round(scrollY)}});
     assert(g.h>=420&&g.gap>=8&&g.gap<=16&&g.chips>=4&&g.chips<=12&&g.chipsShown,'the short-screen map fills below the chips '+JSON.stringify(g));
     await page.setViewportSize(size);await page.evaluate(()=>scrollTo(0,0));
     await page.locator('#logView button[data-view="list"]').click();await page.locator('#logView button[data-view="map"]').click();await page.waitForTimeout(400)}
    assert.equal(await handle.getAttribute('role'),'slider');assert.equal(await page.locator('#logMap [autofocus]').count(),0);
    assert.equal(await rows.count(),2);assert(await rows.first().evaluate(r=>getComputedStyle(r).backdropFilter!=='none'),'glass cards');
    assert.equal(await page.locator('#logMapRows .map-list-pick[data-no-ripple]').count(),2,'a press scales the whole card instead of rippling in its title part');
    {const g=await sheetState();assert(!g.hidden&&Math.abs(g.share-.54)<.02&&g.bottom===0&&g.now==='54',JSON.stringify(g))}
    assert(await creditFree(),'the open sheet leaves the ⓘ uncovered');
    // A tap on a route marks its card (its actions inline, scrolled into view); no bottom card while the cards show.
    await tapLine(routeLines.nth(1));await page.waitForFunction(()=>document.querySelectorAll('#logMapRows>.map-list-row')[1].classList.contains('is-selected'));await page.waitForTimeout(150);
    assert.deepEqual(await rows.nth(1).locator('.map-card-actions>:is(a,button)').allInnerTexts(),['재생','주행 지도','목록에서 보기']);
    assert(await mapCard.isHidden(),'no bottom card while the sheet is up');
    assert(await rows.nth(1).evaluate(r=>{const b=r.getBoundingClientRect(),l=r.parentElement.getBoundingClientRect();return b.top>=l.top-1&&b.bottom<=l.bottom+1}),'the marked card is in view');
    // A tap beside a route line, a finger's width away and on no line, marks that route all the same (the lines are a few
    // pixels wide; such a tap used to clear the selection). Here: beside the other route's line (the thinner one).
    {const beside=await page.evaluate(()=>{const p=[...document.querySelectorAll('#logMap .leaflet-overlay-pane path.leaflet-interactive')].find(p=>Number(p.getAttribute('stroke-width'))!==6),m=p.getScreenCTM(),length=p.getTotalLength();
      for(const k of [.5,.4,.6,.3,.7,.2,.8])for(const [dx,dy] of [[0,14],[14,0],[0,-14],[-14,0]]){const at=p.getPointAtLength(length*k),x=Math.round(at.x*m.a+at.y*m.c+m.e)+dx,y=Math.round(at.x*m.b+at.y*m.d+m.f)+dy,hit=document.elementFromPoint(x,y);
       if(hit&&hit.closest('.log-map-canvas')&&hit.localName!=='path'&&!hit.closest('.leaflet-control,#logMapList'))return {x,y}}return null});
     assert(beside,'a free spot beside the other route');await page.touchscreen.tap(beside.x,beside.y);
     await page.waitForFunction(()=>document.querySelectorAll('#logMapRows>.map-list-row')[0].classList.contains('is-selected'),null,{timeout:3000});
     // Back to the second route, by its own line (now the thinner one; a marked line is brought to the front).
     await tapLine(page.locator('#logMap .leaflet-overlay-pane path.leaflet-interactive:not([stroke-width="6"])').first());await page.waitForFunction(()=>document.querySelectorAll('#logMapRows>.map-list-row')[1].classList.contains('is-selected'));await page.waitForTimeout(150)}
    // A tap on a card selects that drive and fits the map to it above the sheet. (lineBox(): the first card's line, the marked
    // one having been brought to the front, the last path).
    const before=await lineBox();await rows.first().locator('.map-list-pick').tap();
    assert.equal(await page.locator('#logMapRows .map-card-actions').count(),1,'reduced motion: the buttons move at once, none left folding');
    await page.waitForTimeout(450);
    assert(await rows.first().evaluate(r=>r.classList.contains('is-selected'))&&!await rows.nth(1).evaluate(r=>r.classList.contains('is-selected')));
    {const g=await page.evaluate(()=>{const m=document.getElementById('logMap').getBoundingClientRect(),s=document.getElementById('logMapList').getBoundingClientRect(),line=[...document.querySelectorAll('#logMap .leaflet-overlay-pane path.leaflet-interactive')].find(p=>Number(p.getAttribute('stroke-width'))===6).getBoundingClientRect();
      return {inside:line.top>=m.top-1&&line.bottom<=s.top+1&&line.left>=m.left-1&&line.right<=m.right+1,line:[line.top,line.bottom].map(Math.round),sheet:Math.round(s.top)}});
     const after=await lineBox(6);assert(g.inside&&after[3]>before[3]*2,'fitted above the sheet '+JSON.stringify({g,before,after}))}
    await shot('list-map-middle');
    // Motion allowed: selecting another card morphs the list. The new card's buttons unfold while the old card's fold away
    // (unclickable meanwhile); at the end one row of buttons is left, at its own height. With RV_MAP_SHOTS the morph runs
    // slowed down and is photographed on its way.
    {await page.emulateMedia({reducedMotion:'no-preference'});
     const cdp=shots?await page.context().newCDPSession(page):null;if(cdp){await cdp.send('Animation.enable');await cdp.send('Animation.setPlaybackRate',{playbackRate:.12})}
     await rows.nth(1).locator('.map-list-pick').tap();
     const m=await page.evaluate(()=>{const r=[...document.querySelectorAll('#logMapRows>.map-list-row')],grow=r[1].querySelector(':scope>.map-card-actions:not(.is-folding)'),fold=r[0].querySelector(':scope>.map-card-actions.is-folding');
      return {grow:!!grow&&grow.getAnimations().some(a=>a.playState==='running'),fold:!!fold&&fold.getAnimations().some(a=>a.playState==='running'),inert:!!fold?.inert}});
     assert(m.grow&&m.fold&&m.inert,'the cards morph '+JSON.stringify(m));
     if(cdp){for(let k=0;k<4;k++){await page.screenshot({path:path.join(shots,`morph-${k}-${scheme}.png`)});await page.waitForTimeout(450)}}
     await page.waitForFunction(()=>!document.querySelector('#logMapRows .map-card-actions.is-folding')&&!document.querySelector('#logMapRows').getAnimations({subtree:true}).some(a=>a.playState==='running'),null,{timeout:8000});
     if(cdp){await page.screenshot({path:path.join(shots,`morph-4-${scheme}.png`)});await cdp.send('Animation.setPlaybackRate',{playbackRate:1});await cdp.detach()}
     const g=await page.evaluate(()=>{const box=document.querySelectorAll('#logMapRows>.map-list-row')[1].querySelector(':scope>.map-card-actions');return {count:document.querySelectorAll('#logMapRows .map-card-actions').length,h:box.offsetHeight,natural:box.scrollHeight,overflow:box.style.overflow}});
     assert(g.count===1&&g.h>30&&Math.abs(g.h-g.natural)<=1&&g.overflow==='','settled after the morph '+JSON.stringify(g));
     await page.emulateMedia({reducedMotion:'reduce'});await rows.first().locator('.map-list-pick').tap();await page.waitForTimeout(450)}
    // Raising and lowering the sheet keeps the map where it is; a drag snaps to the nearest height.
    const still=await lineBox(6);
    await drag(-160);{const g=await sheetState();assert(!g.hidden&&Math.abs(g.share-.7)<.02,JSON.stringify(g))}
    await page.waitForTimeout(450);{const after=await lineBox(6);assert(after.every((v,i)=>Math.abs(v-still[i])<=1),'the sheet moved the map: '+JSON.stringify({still,after}))}
    await drag(110);{const g=await sheetState();assert(Math.abs(g.share-.54)<.02,'snaps to the middle '+JSON.stringify(g))}
    // Pulled all the way down only the handle stays; the selected drive's card shows above it.
    await drag(400);await page.waitForTimeout(150);
    {const g=await sheetState();assert(g.hidden&&g.now==='0'&&g.text==='숨김'&&g.rowOpacity==='0',JSON.stringify(g))}
    {const after=await lineBox(6);assert(after.every((v,i)=>Math.abs(v-still[i])<=1),'hiding the sheet moved the map: '+JSON.stringify({still,after}))}
    await mapCard.waitFor();
    {const [c,h]=await page.evaluate(()=>['#logMapCard','#logMap .log-map-handle'].map(s=>document.querySelector(s).getBoundingClientRect()));assert(c.bottom<=h.top+1,'the card sits above the handle '+JSON.stringify([c.bottom,h.top]))}
    assert.deepEqual(await mapCard.locator('.map-card-actions>:is(a,button):not(.map-card-close)').allInnerTexts(),['재생','주행 지도','목록에서 보기']);
    await shot('list-map-hidden');
    {const toggle=page.locator('#logMap .rv-credit-toggle'),credit=page.locator('#logMap .leaflet-control-attribution');
     assert(await creditFree(),'nothing covers the ⓘ');await toggle.tap();await credit.waitFor();await toggle.tap();await credit.waitFor({state:'hidden'})}
    // A tap folds and unfolds: hidden -> the middle height (the card leaves), shown -> hidden (it is back).
    await handle.tap();await page.waitForTimeout(400);{const g=await sheetState();assert(!g.hidden&&Math.abs(g.share-.54)<.02,JSON.stringify(g))}
    assert(await mapCard.isHidden(),'the card leaves while the cards show');
    await handle.tap();await page.waitForTimeout(150);assert((await sheetState()).hidden,'a tap hides it');await mapCard.waitFor();
    // Shown again, the map starts like the drive map: every drive fitted, the sheet at the middle height.
    await switcher.getByRole('button',{name:'목록'}).tap();await page.locator('#list').waitFor();await switcher.getByRole('button',{name:'지도'}).tap();
    await page.waitForFunction(()=>document.querySelectorAll('#logMap .leaflet-overlay-pane path.leaflet-interactive').length===2);await page.locator('#logMap').scrollIntoViewIfNeeded();await page.waitForTimeout(150);
    {const g=await sheetState();assert(!g.hidden&&Math.abs(g.share-.54)<.02,JSON.stringify(g))}
    await handle.tap();await page.waitForTimeout(150);assert((await sheetState()).hidden)}
   // The checks below use the bottom card: the sheet stays hidden.
   await tapLine(routeLines.first());const card=page.locator('#logMapCard');await card.waitFor();
   assert.match(await card.locator('.map-card-facts').innerText(),new RegExp(`^(${(A*0.9).toFixed(1)} km · ${A}:00 · ${A}개 구간|900 m · 1:00 · 1개 구간)$`));
   assert.match(await card.locator('.map-card-play').getAttribute('href'),/view\/[a-f0-9]{32}\/$/);
   assert.deepEqual(await card.locator('.map-card-actions>:is(a,button):not(.map-card-close)').allInnerTexts(),['재생','주행 지도','목록에서 보기']);
   assert.equal((await routeLines.evaluateAll(lines=>lines.map(l=>Number(l.getAttribute('stroke-opacity'))<.5))).filter(Boolean).length,1,'the others dim');
   await shot('list-map');tilesUsed();
   // The card's 주행 지도 opens the same view for that drive; back returns to the list's map.
   {const facts=await card.locator('.map-card-facts').innerText(),count=Number(/(\d+)개 구간/.exec(facts)[1]);
    await card.getByRole('button',{name:'주행 지도'}).tap();
    await page.waitForFunction(count=>{const d=document.getElementById('driveMap');return d.open&&d.querySelectorAll('.drive-map-list>.log-row').length===count&&d.querySelectorAll('.leaflet-overlay-pane path.leaflet-interactive').length===count},count);
    assert.equal(await page.locator('#driveMapFacts').innerText(),facts);
    await page.goBack();await driveClosed();assert(await page.locator('#logMap').isVisible())}
   // The filters apply to the map too.
   await page.locator('#logSearch').fill('주말');await page.waitForFunction(()=>document.querySelectorAll('#logMap .leaflet-overlay-pane path.leaflet-interactive').length===1);
   await page.locator('#logSearch').fill('');await page.waitForFunction(()=>document.querySelectorAll('#logMap .leaflet-overlay-pane path.leaflet-interactive').length===2);
   await tapLine(routeLines.first());await card.waitFor();const shownRoute=await page.evaluate(()=>document.querySelector('#logMapCard .map-card-play').getAttribute('href'));
   await card.getByRole('button',{name:'목록에서 보기'}).tap();
   await page.locator('#list').waitFor();assert(await page.locator('#logMap').isHidden());
   {const id=shownRoute.split('/').filter(Boolean).at(-1);await page.waitForFunction(id=>{const r=document.querySelector(`#list .log-row[data-id="${id}"]`).closest('.log-route').getBoundingClientRect();return r.top>=0&&r.top<innerHeight},id)}
   // A separate stopped-start response: leave the moving fixture intact for smooth-following checks.
   {const stopped=routeA[1].map(p=>[...p]);stopped.slice(0,3).forEach(p=>{p[1]=stopped[0][1];p[2]=stopped[0][2];p[3]=0;p[4]=null});
    const trackUrl=`**/api/logs/${withVideo}/track`,stoppedTrack=async route=>{const response=await route.fetch(),value=await response.json();value.segments.find(s=>s.id===withVideo).points=stopped;await route.fulfill({response,json:value})};
    await page.route(trackUrl,stoppedTrack);
    await page.goto(base+`view/${withVideo}/`);await page.locator('#mapTab').waitFor();await page.locator('#mapTab').tap();
    const car=page.locator('#routeMapView .map-car-body');await car.waitFor();
    const first=stopped[0],next=stopped[3],expected=(Math.atan2((next[2]-first[2])*Math.cos(first[1]*Math.PI/180),next[1]-first[1])*180/Math.PI+360)%360;
    const angle=await car.evaluate(e=>Number(e.style.transform.match(/rotate\(([-.\d]+)deg\)/)?.[1]));
    assert(Math.abs(angle-expected)<.02,'the stopped start faces its first movement before playing');
    assert.notEqual(await page.locator('#play').getAttribute('data-playing'),'true','direction is set before playback');
    await page.unroute(trackUrl,stoppedTrack);
   }
   // ③ The replay's 주행 상황 | 지도: the whole drive, this log's segment solid and the others faded, a car marker following playback.
   await page.goto(base+`view/${withVideo}/`);await page.waitForFunction(()=>!!document.getElementById('play')&&!document.getElementById('play').disabled);
   // The 지도 tab sits right after 주행 상황 and shows only for a segment with a track; the tab row stays one row.
   const mapTab=page.locator('#mapTab');await mapTab.waitFor();
   assert.deepEqual((await page.locator('.vehicle-tabs [role=tab]').allTextContents()).map(t=>t.replace(/\d+$/,'').trim()),['주행 상황','지도','차선·도로','차량 정보','북마크']);
   {const tops=await page.locator('.vehicle-tabs [role=tab]').evaluateAll(tabs=>tabs.map(t=>Math.round(t.getBoundingClientRect().top)));assert.equal(new Set(tops).size,1,'one tab row '+tops)}
   assert.equal(await page.locator('#roadMode').count(),0,'no 주행 상황 | 지도 switch any more');
   await mapTab.tap();await page.locator('#mapView').waitFor();await page.locator('#routeMapView .map-car').waitFor();await shot('map-tab');
   const mapFollow=page.locator('#mapFollow'),mapOverview=page.locator('#mapOverview');
   await assertFollow(page);
   await assertToolbar(page,'#routeMapView .route-map-controls');
   const savedMark=page.locator('#routeMapView').getByRole('button',{name:/저장된 교차로/});
   assert.equal(await savedMark.evaluate(el=>getComputedStyle(el,'::before').backgroundColor),'rgb(245, 196, 81)');
   await savedMark.tap();await waitSeek(page,savedBookmark.time,.02);await assertFollow(page);
   const mapHandle=await routeMap(page);
   await mapOverview.tap();
   assert.equal(await mapFollow.getAttribute('aria-pressed'),'false');
   assert(await mapHandle.evaluate((map,tracks)=>map.getZoom()<16&&tracks.every(points=>points.every(([,lat,lon])=>map.getBounds().contains([lat,lon]))),routeA),'the entire drive fits in the analysis map');
   await mapFollow.tap();await assertFollow(page);await mapHandle.dispose();
   // The road-view vehicle is the default; changing the icon keeps its bearing and location intact.
   {const car=page.locator('#routeMapView .map-car-body');assert.equal(await car.getAttribute('data-car-icon'),'vehicle');
    assert.equal(await car.evaluate(e=>e.getBoundingClientRect().width>=44),true,'larger vehicle marker');
    assert.equal(await car.locator('.map-vehicle-shell').count(),1);
    const bearing=await car.evaluate(e=>e.style.transform.match(/rotate\(([^)]+)\)/)?.[1]);
    await page.locator('#routeMapView .rv-map-options-button').click();
    const choices=page.locator('.rv-map-options:not([hidden])');
    await choices.locator('[data-map-car="arrow"]').click();
    assert.equal(await car.getAttribute('data-car-icon'),'arrow');assert.equal(await car.locator('svg path').count(),1);
    assert.equal(await car.evaluate(e=>e.style.transform.match(/rotate\(([^)]+)\)/)?.[1]),bearing,'switching icons preserves the heading');
    assert.equal(await page.evaluate(()=>localStorage.getItem('roadviewer-map-car-icon')),'arrow');
    assert.equal(await page.locator('#mapCarIcon [data-map-car="arrow"]').getAttribute('aria-pressed'),'true','small map settings share the choice');
    await choices.locator('[data-map-color="blue"]').click();
    assert.equal(await car.getAttribute('data-car-color'),'blue');
    assert.equal(await car.evaluate(e=>e.style.getPropertyValue('--map-car-color')),'#8cbcff');
    assert.equal(await page.evaluate(()=>localStorage.getItem('roadviewer-map-car-color')),'blue');
    assert.equal(await page.locator('#mapCarColors [data-map-color="blue"]').getAttribute('aria-pressed'),'true');
    await choices.locator('[data-map-color="auto"]').click();
    assert.equal(await car.evaluate(e=>e.style.getPropertyValue('--map-car-color')),'','automatic colour restores theme defaults');
    await choices.locator('[data-map-car="vehicle"]').click();
    assert.equal(await car.getAttribute('data-car-icon'),'vehicle');assert.equal(await car.locator('.map-vehicle-shell').count(),1);
    assert.equal(await choices.locator('[data-map-car="vehicle"]').getAttribute('aria-pressed'),'true');
    await page.keyboard.press('Escape');
   }
   // This log's segment is the one solid line, whole (not only up to the car); the drive's other segments, before and after it, are faded.
   {const lines=await page.locator('#routeMapView .leaflet-overlay-pane path').evaluateAll(ps=>ps.map(p=>({current:p.classList.contains('map-current'),solid:Number(p.getAttribute('stroke-opacity'))>.5,dash:p.style.strokeDasharray||p.getAttribute('stroke-dasharray')||''})));
    assert(lines.filter(l=>l.solid).length===1&&lines.find(l=>l.solid)?.current&&!lines.find(l=>l.current).dash&&lines.filter(l=>!l.current).length>=1&&lines.filter(l=>!l.current).every(l=>!l.solid),'only the current segment is highlighted: '+JSON.stringify(lines))}
   await page.locator('#routeMapView .map-car').waitFor();
   // Leaflet's panes and controls (z-index 400–1000) stay inside the map: the floating playback bar is drawn over it.
   assert.equal(await page.evaluate(()=>getComputedStyle(document.querySelector('#routeMapView .rv-map')).isolation),'isolate');
   {const covered=await page.evaluate(()=>{const map=document.querySelector('#routeMapView .rv-map').getBoundingClientRect(),bar=document.querySelector('.playback').getBoundingClientRect();
     const top=Math.max(map.top,bar.top),bottom=Math.min(map.bottom,bar.bottom);if(bottom<=top)return null;const e=document.elementFromPoint(bar.left+bar.width/2,(top+bottom)/2);return !!e?.closest('.playback')});
    assert(covered!==false,'the playback bar stays above the map')}
   assert(await page.locator('#road').isHidden());assert.equal(await page.locator('#mapFollow').getAttribute('aria-pressed'),'true');
   // 따라가기 stacks inside the map view (its own stacking context): it never rises over the sticky video above it.
   assert.equal(await page.locator('#routeMapView').evaluate(v=>getComputedStyle(v).isolation),'isolate');
   const carAt=()=>page.locator('#routeMapView .map-car').evaluate(e=>e.style.transform||e.getAttribute('style'));
   const seek=value=>page.locator('#seek').evaluate((e,value)=>{e.value=String(value);e.dispatchEvent(new Event('input'))},value);
   const start=await carAt();await seek(40);await page.waitForFunction(start=>document.querySelector('#routeMapView .map-car').style.transform!==start,start);
   await assertFollow(page);
   // A save while the map already exists updates its markers, and jumps to the saved fractional second.
   await seek(34.25);
   await page.locator('#bookmarkOpen').tap();
   const liveEditor=page.locator('.bookmark-dialog');
   await liveEditor.locator('.bookmark-title').fill('지도에서 기록');
   await liveEditor.locator('.bookmark-save').tap();
   await liveEditor.waitFor({state:'hidden'});
   const liveMark=page.locator('#routeMapView').getByRole('button',{name:/지도에서 기록/});
   await seek(40);await liveMark.tap();await waitSeek(page,34.25,.02);
   await page.locator('#bookmarkTab').tap();
   page.once('dialog',dialog=>dialog.accept());
   await page.getByRole('button',{name:'지도에서 기록 삭제'}).tap();
   await page.getByRole('button',{name:'지도에서 기록 삭제'}).waitFor({state:'detached'});
   await mapTab.tap();assert.equal(await liveMark.count(),0);
   {const box=await page.locator('#routeMapView').boundingBox();await page.touchscreen.tap(box.x+box.width/2,box.y+box.height-30);
    await page.mouse.move(box.x+box.width/2,box.y+box.height/2);await page.mouse.down();await page.mouse.move(box.x+box.width/2+60,box.y+box.height/2+40,{steps:5});await page.mouse.up();
    await page.waitForFunction(()=>document.getElementById('mapFollow').getAttribute('aria-pressed')==='false');
    await mapFollow.tap();await assertFollow(page)}
   // A pinch narrows the 지도 tab's margin (basemap and lines) to 0.1 for the gesture (MapLibre redraws the whole canvas
   // each frame); a second after it, the full margin again, the car, lines and basemap together. A one-finger drag
   // moves what is drawn with the pane: a few redraws at most, the map left where the drag left it, nothing cut off.
   {await page.waitForFunction(()=>window.rvTestMaps.some(m=>document.querySelector('#routeMapView').contains(m.getContainer())&&m.rvGl?.loaded?.()),null,{timeout:15000});
    await page.evaluate(()=>{
     const m=document.querySelector('#routeMapView .rv-map'),map=window.rvTestMaps.find(x=>m.contains(x.getContainer())||x.getContainer()===m),gl=map.rvGl;
     const t=window.rvTestBig={map,renders:0,redraws:0};gl.on('render',()=>t.renders++);{const redraw=gl.redraw.bind(gl);gl.redraw=()=>{t.redraws++;return redraw()}}map.on('dragstart',()=>{t.atDrag={renders:t.renders,redraws:t.redraws}});
     const inside=(a,b)=>a.left<=b.left+1&&a.top<=b.top+1&&a.right>=b.right-1&&a.bottom>=b.bottom-1;
     t.state=()=>{
      let car=null;map.eachLayer(l=>{if(l.options?.icon?.options?.className==='map-car')car=l.getLatLng()});
      const box=m.getBoundingClientRect(),c=gl.getCanvas().getBoundingClientRect(),q=gl.project([car.lng,car.lat]),b=m.querySelector('.map-car-body').getBoundingClientRect(),at=[b.left+b.width/2,b.top+b.height/2];
      const line=window.rvTestNearLine(m,at);
      return {base:Math.hypot(c.left+q.x-at[0],c.top+q.y-at[1]),line,layer:map.rvGlLayer.options.padding,lines:map.options.renderer.options.padding,margin:map.rvMargin,
       basemapCovers:inside(c,box),linesCover:inside(m.querySelector('.leaflet-overlay-pane svg').getBoundingClientRect(),box),renders:t.renders,redraws:t.redraws};
     };
    });
    const cdp=await context.newCDPSession(page),state=()=>page.evaluate(()=>window.rvTestBig.state());
    const touch=(type,points)=>cdp.send('Input.dispatchTouchEvent',{type,touchPoints:points});
    await seek(20);await page.waitForTimeout(400);const full=await state();
    assert(full.layer===full.margin&&full.lines===full.margin&&full.margin>.1,'full margin before '+JSON.stringify(full));
    const b=await page.locator('#routeMapView .rv-map').boundingBox(),cx=b.x+b.width/2,cy=b.y+Math.min(b.height,844-b.y-120)/2;
    const pair=d=>[{x:cx-d,y:cy,id:1},{x:cx+d,y:cy,id:2}];
    await touch('touchStart',pair(40));const pinching=await state();
    for(let k=1;k<=10;k++){await touch('touchMove',pair(40+3*k));await page.waitForTimeout(16)}
    const during=await state();await touch('touchEnd',[]);await page.waitForTimeout(300);const ended=await state();
    assert.equal(await mapFollow.getAttribute('aria-pressed'),'false','a pinch turns 따라가기 off');
    assert(pinching.layer===.1&&pinching.lines===.1&&during.layer===.1&&during.lines===.1&&ended.layer===.1,'narrow while pinching '+JSON.stringify([pinching,during,ended]));
    assert(during.basemapCovers,'the basemap covers the map while pinching '+JSON.stringify(during));
    await page.waitForTimeout(1300);const after=await state();
    assert(after.layer===full.margin&&after.lines===full.margin&&after.margin===full.margin,'full margin after the pinch '+JSON.stringify(after));
    assert(after.base<=1&&after.line<=1&&after.basemapCovers&&after.linesCover,'together after the pinch '+JSON.stringify(after));
    // A pinch ending exactly on a zoom step (twice the spread: +1) with reduced motion (no zoom animation): Leaflet fires
    // no zoomend for it; the basemap still follows (not frozen out of place) and the full margin comes back.
    {const z0=await page.evaluate(()=>window.rvTestBig.map.getZoom());
     await touch('touchStart',pair(40));for(let k=1;k<=8;k++){await touch('touchMove',pair(40+5*k));await page.waitForTimeout(16)}
     await touch('touchEnd',[]);await page.waitForTimeout(1600);const step=await state();
     const z=await page.evaluate(()=>({zoom:window.rvTestBig.map.getZoom(),stuck:!!window.rvTestBig.map.rvGlLayer._zooming}));
     assert(z.zoom===z0+1&&!z.stuck,'the pinch ended on a whole step and closed its zoom '+JSON.stringify({z0,...z}));
     assert(step.base<=1&&step.basemapCovers&&step.layer===full.margin,'together after a pinch ending on a step '+JSON.stringify(step))}
    // One finger, ~300 px over ~1 s, held still before lifting (no inertia).
    await seek(30);await page.waitForTimeout(400);
    // A fixed place on the basemap, on screen (the car could still move a little after the seek).
    await page.evaluate(()=>{const t=window.rvTestBig;t.c0=t.map.getCenter()});
    const onScreen=()=>page.evaluate(()=>{const t=window.rvTestBig,gl=t.map.rvGl,c=gl.getCanvas().getBoundingClientRect(),q=gl.project([t.c0.lng,t.c0.lat]);return {x:c.left+q.x,y:c.top+q.y}}),from=await onScreen();
    let mid=null;
    await touch('touchStart',[{x:cx+150,y:cy,id:1}]);
    for(let k=1;k<=60;k++){await touch('touchMove',[{x:cx+150-5*k,y:cy+k/3,id:1}]);await page.waitForTimeout(16);if(k===40)mid=await state()}
    const held=await state();await page.waitForTimeout(150);await touch('touchEnd',[]);await page.waitForTimeout(400);
    // The redraw at rest can come later than that on a loaded machine: wait for the basemap to stand where the drag left it.
    for(let k=0;k<50;k++){const at=await onScreen();if(Math.abs(at.x-from.x+300)<=2&&Math.abs(at.y-from.y-20)<=2)break;await page.waitForTimeout(100)}
    const dragged=await state(),moved=await onScreen();
    assert.equal(await page.locator('#mapFollow').getAttribute('aria-pressed'),'false','a drag turns 따라가기 off');
    // From the drag's start: redrawn once past RECENTRE of the margin and once at rest; the other renders are
    // MapLibre's own (tiles of the newly drawn area). Each frame before (~60).
    const s0=await page.evaluate(()=>window.rvTestBig.atDrag);
    assert(dragged.redraws-s0.redraws<=2&&dragged.renders-s0.renders<=6,'a drag redraws the basemap a few times at most: '+JSON.stringify([dragged.redraws-s0.redraws,dragged.renders-s0.renders]));
    assert(Math.abs(moved.x-from.x+300)<=2&&Math.abs(moved.y-from.y-20)<=2,'the map stays where the drag left it '+JSON.stringify([from,moved]));
    assert(dragged.layer===full.margin&&mid.layer===full.margin,'full margin while dragging '+JSON.stringify([mid,dragged]));
    assert([mid,held,dragged].every(s=>s.basemapCovers&&s.linesCover),'nothing cut off while dragging '+JSON.stringify([mid,held,dragged]));
    assert(dragged.base<=1&&dragged.line<=1,'together after the drag '+JSON.stringify(dragged));
    await mapFollow.tap();await assertFollow(page)}
   tilesUsed();
   // ④ 겹쳐 보기 설정 › 지도 › 영상 위 작은 지도: off by default, kept on this browser, tap to enlarge and shrink.
   await page.locator('#overlayHeightSettings').tap();const dialog=page.locator('#overlayHeightDialog');await dialog.waitFor();
   assert.equal(await dialog.locator('h3',{hasText:'지도'}).count(),1);const toggle=dialog.getByLabel('영상 위 작은 지도');
   assert.equal(await toggle.isChecked(),false);assert(await page.locator('#videoMap').isHidden());
   await toggle.check();await page.locator('#overlayHeightClose').tap();await dialog.waitFor({state:'hidden'});
   // Never two maps: while the panel shows 지도 the small map stays hidden; back on 주행 상황 it appears.
   await page.waitForTimeout(300);assert(await page.locator('#videoMap').isHidden(),'no small map while the panel shows the map');
   await page.locator('#roadTab').tap();
   const mini=page.locator('#videoMap'),open=mini.locator('.video-map-open'),close=mini.locator('.video-map-close');
   await mini.locator('canvas').waitFor();await settleLayout(page);
   await page.waitForFunction(()=>window.rvTestMini?.getLayer('background')&&rvTestMini.getPaintProperty('background','background-color')==='#0E1219'&&rvTestMini.hasImage('rv-vehicle'));
   assert.equal(await mini.locator('.leaflet-container').count(),0);
   await assertMiniCorner(page);
   assert.equal(await page.evaluate(()=>rvTestMini.getPitch()),56);
   assert((await mini.boundingBox()).width<=174.5);
   if(scheme==='dark'){
    await setHudPlaying(page,true);await assertReplayHud(page,false);
    const picture=async()=>{
     const box=await page.locator('#videoStage').boundingBox();
     await page.touchscreen.tap(box.x+box.width*.42,box.y+box.height*.55);
    };
    await page.waitForTimeout(450);await picture();await assertReplayHud(page,true);
    await page.waitForTimeout(450);await picture();await assertReplayHud(page,false);
    await setHudPlaying(page,false);await assertReplayHud(page,true);
   }
   // The analysis tab's basemap preferences remain independent.
   await page.locator('#overlayHeightSettings').tap();await dialog.waitFor();
   await dialog.getByRole('button',{name:'라이트',exact:true}).tap();
   if(!REAL)await page.waitForFunction(()=>window.rvTestMini?.getLayer('background')&&rvTestMini.getPaintProperty('background','background-color')==='#f0eeea');
   await dialog.getByLabel('작은 지도 글자 숨기기').check();
   await page.waitForFunction(()=>rvTestMini.getStyle()?.layers.filter(layer=>layer.type==='symbol').every(layer=>layer.id==='rv-car'));
   assert.equal(await page.locator('#routeMapView .rv-map').evaluate(el=>el.dataset.tiles),scheme);
   await dialog.getByLabel('작은 지도 글자 숨기기').uncheck();
   await dialog.locator('#mapCarIcon [data-map-car="arrow"]').tap();
   await assertMiniCar(page,'arrow');
   await page.locator('#overlayHeightClose').tap();
   await assertMiniCorner(page,'arrow');
   await page.locator('#overlayHeightSettings').tap();
   await dialog.locator('#mapCarIcon [data-map-car="vehicle"]').tap();
   await assertMiniCar(page,'vehicle');
   await dialog.getByRole('button',{name:'다크',exact:true}).tap();
   await page.locator('#overlayHeightClose').tap();
   assert(await open.isVisible());
   await seek(24);
   await open.tap();await close.waitFor();
   await assertFollow(page,true);
   await assertToolbar(page,'#videoMap .video-map-toolbar');
   const miniFollow=mini.locator('.video-map-follow');
   await mini.locator('.video-map-overview').tap();
   assert.equal(await miniFollow.getAttribute('aria-pressed'),'false');
   await assertToolbar(page,'#videoMap .video-map-toolbar');
   await miniFollow.tap();await assertFollow(page,true);
   await mini.getByLabel('지도 출처').tap();
   assert(await mini.getByRole('link',{name:'OpenFreeMap'}).isVisible());
   await mini.getByLabel('지도 출처').tap();
   assert.equal(await page.evaluate(()=>rvTestMini.getPitch()),0);
   assert.equal(await page.evaluate(()=>rvTestMini.getBearing()),0);
   assert(await mini.locator('.video-map-legend').isHidden(),'the phone leaves room for the route');
   await tapNative(page,routeA[1][18],routeA[1][19],.5);
   await waitSeek(page,18.5);
   await open.waitFor(); // Auto-collapse is the default.
   await page.evaluate(()=>Object.defineProperty(screen.orientation,'lock',{configurable:true,value:()=>Promise.reject(new DOMException('refused','NotSupportedError'))}));
   await page.locator('#fullscreenButton').tap();
   await page.waitForFunction(()=>document.fullscreenElement?.classList.contains('is-turned'));
   await open.tap();await close.waitFor();
   const pin=mini.locator('.video-map-pin');await pin.tap();
   const touch=await context.newCDPSession(page);
   const turnedStart=await nativePoint(page,routeA[1][22]),turnedEnd=await nativePoint(page,routeA[1][26],routeA[1][27],.5);
   await touch.send('Input.dispatchTouchEvent',{type:'touchStart',touchPoints:[{x:turnedStart[0],y:turnedStart[1]}]});
   await touch.send('Input.dispatchTouchEvent',{type:'touchMove',touchPoints:[{x:turnedEnd[0],y:turnedEnd[1]}]});
   await mini.locator('.video-map-preview').waitFor();
   assert(Math.abs(Number(await page.locator('#seek').inputValue())-18.5)<.65);
   await touch.send('Input.dispatchTouchEvent',{type:'touchEnd',touchPoints:[]});
   await waitSeek(page,26.5);
   const snapCard=mini.locator('.video-map-preview');
   assert.match(await snapCard.innerText(),/구간 1.*45 km\/h/s,'turned fullscreen snaps to the same GPS timeline');
   await page.waitForTimeout(400);assert(await close.isVisible(),'pinned remains expanded after the seek');
   if(scheme==='dark'){
    await page.emulateMedia({reducedMotion:'no-preference'});
    await assertMiniClose(page);
    await page.emulateMedia({reducedMotion:'reduce'});
   }else await close.tap();
   if(scheme==='dark'){
    const grip=mini.getByRole('button',{name:'작은 지도 크기 조절'});
    await grip.evaluate(el=>{
     document.getElementById('play').dataset.playing='true';
     el.focus({preventScroll:true});el.blur();
    });
    await assertReplayHud(page,true);
    const before=await mini.boundingBox();
    const box=await grip.boundingBox(),x=box.x+box.width/2,y=box.y+box.height/2;
    await touch.send('Input.dispatchTouchEvent',{type:'touchStart',touchPoints:[{x,y}]});
    for(let step=1;step<=6;step++){
     await page.waitForTimeout(16);
     await touch.send('Input.dispatchTouchEvent',{type:'touchMove',touchPoints:[{x,y:y-step*5}]});
    }
    // Hold past the auto-hide delay, then release at rest so Chromium delivers the next tap.
    await page.waitForTimeout(3200);
    assert.equal(await page.evaluate(()=>document.body.classList.contains('hud-idle')),false,'holding the grip keeps the video controls awake');
    assert.equal(await grip.evaluate(el=>getComputedStyle(el,'::before').opacity),'1','dragging highlights the resize grip');
    await touch.send('Input.dispatchTouchEvent',{type:'touchEnd',touchPoints:[]});
    await setHudPlaying(page,false);
    await settleLayout(page);
    const after=await mini.boundingBox();
    assert(after.height>before.height+28&&Math.abs(after.width-before.width)<1&&await open.isVisible(),'touch drag follows the rotated stage axes');
   }
   await page.evaluate(()=>document.exitFullscreen());
   await page.waitForFunction(()=>!document.fullscreenElement);
   await mapTab.tap();await page.locator('#videoMap').waitFor({state:'hidden'});await page.locator('#routeMapView .map-car').waitFor();
   // Tapping segment 0 of the drive on the map opens that segment at that point.
   {const lines=page.locator('#routeMapView .leaflet-overlay-pane path.leaflet-interactive');assert(await lines.count()>=2);
    await seek(0);await mapOverview.tap();
    const first=lines.first();
    await page.evaluate(()=>scrollTo(0,document.scrollingElement.scrollHeight));
    await waitForSteadyLayout(page,'#routeMapView');
    await tapLine(first,.15);
    await page.waitForURL(new RegExp(`view/${logs[0].id}/$`));await page.waitForFunction(()=>!!document.getElementById('play')&&!document.getElementById('play').disabled&&Number(document.getElementById('seek').value)>0)}
   assert(await page.locator('#videoMiniMap').isChecked(),'the small map setting is kept');
   // A segment with GPS but no video: the panel's map only, no small map over the picture.
   await page.waitForTimeout(400);assert(await page.locator('#mapTab').isVisible());assert.equal(await page.locator('#mapTab').getAttribute('aria-selected'),'true','the 지도 tab is kept');assert(await page.locator('#videoMap').isHidden());
   assert.equal(await page.locator('#videoStage.has-video-map').count(),0);
   // A segment without GPS has neither the switch nor the small map.
   await page.goto(base+`view/${logs.at(-1).id}/`);await page.waitForFunction(()=>!!document.getElementById('play')&&!document.getElementById('play').disabled);await page.waitForTimeout(300);
   assert(await page.locator('#mapTab').isHidden());assert(await page.locator('#videoMap').isHidden());
   assert.deepEqual(errors,[]);
   await context.close();
  }
  if(PARTS.includes('native')||(PARTS.includes('main')&&SCHEMES.includes('dark'))){
   const context=await browser.newContext({viewport:{width:1440,height:900},colorScheme:'dark'});
   const page=await context.newPage(),errors=[];
   page.on('pageerror',error=>errors.push(error.message));
   await context.addInitScript(hookMaps);
   await context.addInitScript(()=>localStorage.setItem('roadviewer-map-car-icon','arrow'));
   let styleAttempts=0,trackAttempts=0;
   if(!REAL)await context.route(OFM+'**',route=>{
    if(route.request().url()===STYLE_OF.dark&&++styleAttempts===1)return route.fulfill({status:503});
    return stubTiles(route);
   });
   const testVideo=video;
   for(const log of [logs[1],logs[2]])await context.route('**/api/logs/'+log.id+'/video**',route=>route.fulfill({body:testVideo,contentType:'video/mp4'}));
   const missing=logs[4].id;let requests=0;
   await page.route('**/api/logs/'+withVideo+'/track',async route=>{
    if(++trackAttempts===1)return route.fulfill({status:503});
    const headers={...route.request().headers()};delete headers['if-none-match'];
    const response=await route.fetch({headers}),value=await response.json();
    value.segments=value.segments.filter(segment=>segment.id!==missing);
    value.unconverted=[{id:missing,segment:4,status:requests?'queued':'unconverted'}];
    await route.fulfill({response,json:value});
   });
   await page.route('**/api/logs/'+missing+'/convert',route=>{
    assert.equal(route.request().method(),'POST');
    assert.equal(route.request().headers()['x-roadviewer-request'],'1');
    requests++;
    return route.fulfill({json:{status:'queued'}});
   });
   await page.goto(base+'view/'+withVideo+'/');
   await page.locator('#play:enabled').waitFor();
   const dialog=page.locator('#overlayHeightDialog'),mini=page.locator('#videoMap'),open=mini.locator('.video-map-open'),close=mini.locator('.video-map-close');
   await page.locator('#overlayHeightSettings').click();
   await dialog.getByLabel('영상 위 작은 지도').check();
   await page.locator('#overlayHeightClose').click();
   await page.waitForFunction(()=>window.rvTestMini?.getLayer('background')&&rvTestMini.getPaintProperty('background','background-color')==='#0E1219'&&rvTestMini.hasImage('rv-vehicle'));
   const seek=async value=>page.locator('#seek').evaluate((el,t)=>{el.value=t;el.dispatchEvent(new Event('input'))},value);
   assert(trackAttempts>=2,'the replay retried a failed track fetch');
   if(!REAL){
    await page.waitForFunction(()=>rvTestMini.getSource('openmaptiles'));
    assert(styleAttempts>=2,'the minimap reloaded the basemap after a failed style request');
   }
   await seek(3);await page.waitForFunction(()=>Math.abs(rvTestMini.getBearing()+1)<.15);
   await seek(4);await page.waitForFunction(()=>Math.abs(rvTestMini.getBearing()-2)<.15);
   await seek(42);await page.waitForFunction(()=>Math.abs(rvTestMini.getZoom()-17.1)<.02);
   assert(Math.abs(await page.evaluate(()=>rvTestMini.getBearing())-2)<.15,'low speed keeps the previous heading');
   await seek(45);await page.waitForFunction(()=>Math.abs(rvTestMini.getZoom()-14.6)<.02);
   await seek(10.1);
   await page.waitForFunction(()=>Math.abs(rvTestMini.getBearing()-9)<.5);
   const turn=await page.evaluate(async()=>{
    const map=rvTestMini,seek=document.getElementById('seek'),play=document.getElementById('play');
    const samples=[];
    let frame;
    const sample=now=>{samples.push({now,time:seek.valueAsNumber,bearing:map.getBearing()});frame=requestAnimationFrame(sample)};
    frame=requestAnimationFrame(sample);play.click();
    const deadline=performance.now()+6000;
    await new Promise(resolve=>{
     const wait=()=>{if(seek.valueAsNumber>=10.75||performance.now()>deadline)resolve();else requestAnimationFrame(wait)};
     requestAnimationFrame(wait);
    });
    play.click();cancelAnimationFrame(frame);return samples;
   });
   const duringTurn=turn.filter(sample=>sample.time>10.2&&sample.time<10.75);
   assert(duringTurn.some(sample=>sample.bearing>12&&sample.bearing<45),'the heading changes between GPS samples');
   for(let i=1;i<duringTurn.length;i++){
    const previous=duringTurn[i-1],next=duringTurn[i];
    assert(Math.abs(next.bearing-previous.bearing)<=55*(next.now-previous.now)/1000+1,'the rotating map respects the angular speed limit');
   }
   // Observe the seek on the first animation frame, well before a smoothed turn could complete.
   const snapped=await page.evaluate(async()=>{
    const seek=document.getElementById('seek');seek.value='14';seek.dispatchEvent(new Event('input'));
    await new Promise(requestAnimationFrame);return {bearing:rvTestMini.getBearing(),zoom:rvTestMini.getZoom()};
   });
   assert(Math.abs(snapped.zoom-16.36)<.02&&Math.abs(snapped.bearing-90)<.15,JSON.stringify(snapped));
   const forward=await page.evaluate(([,lat,lon])=>{const map=rvTestMini,car=map.project(map.getCenter()),next=map.project([lon,lat]);
    return {x:next.x-car.x,y:next.y-car.y};
   },routeA[1][15]);
   assert(forward.y<0&&Math.abs(forward.x)<1,'the route extends up from the arrow '+JSON.stringify(forward));
   await page.locator('#bookmarkOpen').click();
   const editor=page.locator('.bookmark-dialog');
   await editor.locator('.bookmark-title').fill('차선 합류');
   await editor.locator('.bookmark-save').click();
   await editor.waitFor({state:'hidden'});
   await page.waitForFunction(async()=>((await rvTestMini.getSource('rv-bookmarks').getData()).features.length===2));
   await page.evaluate(()=>scrollTo(0,0));await settleLayout(page);
   await assertMiniCorner(page,'arrow');
   await screenshot(page,'01-desktop-3d-minimap');

   const grip=mini.getByRole('button',{name:'작은 지도 크기 조절'});
   const picture=await page.locator('#videoStage').boundingBox();
   await page.mouse.move(5,5);
   await setHudPlaying(page,true);await assertReplayHud(page,false);
   await page.mouse.move(picture.x+picture.width*.42,picture.y+picture.height*.55);
   await assertReplayHud(page,true);await assertReplayHud(page,false);
   const handle=await grip.boundingBox();
   await page.mouse.move(handle.x+handle.width/2,handle.y+handle.height/2);
   await assertReplayHud(page,true);
   // The first movement wakes the controls while the hidden grip still passes pointer events through.
   await grip.hover({position:{x:12,y:24}});
   await page.waitForFunction(()=>getComputedStyle(document.querySelector('.video-map-grip'),'::before').opacity==='1');
   const dragGrip=async (dx,dy)=>{
    const {x,y,width,height}=await grip.boundingBox(),at=[x+width/2,y+height/2];
    await page.mouse.move(...at);await page.mouse.down();
    await page.mouse.move(at[0]+dx,at[1]+dy,{steps:4});
    await page.mouse.up();await page.mouse.move(5,5);await settleLayout(page);
   };
   const collapsed=await mini.boundingBox(),key='roadviewer-minimap-size';
   assert((await grip.boundingBox()).width>=44,'the resize grip has a touch-sized hit area');
   await assertMiniFade(page,mini);
   await dragGrip(-135,0);
   await setHudPlaying(page,false);await assertReplayHud(page,true);
   const wide=await mini.boundingBox();
   assert(wide.width>collapsed.width+130&&Math.abs(wide.height-collapsed.height)<1,'horizontal drag changes only width');
   await assertMiniFade(page,mini);
   await screenshot(page,'05-desktop-wide-minimap');
   await dragGrip(255,0);
   const narrow=await mini.boundingBox();
   assert(Math.abs(narrow.height-wide.height)<1,'shrinking width does not change height');
   await dragGrip(0,85);
   const tall=await mini.boundingBox();
   assert(tall.height>narrow.height+80&&Math.abs(tall.width-narrow.width)<1&&tall.height>tall.width,'vertical drag changes only height');
   await assertMiniFade(page,mini);
   await assertMiniCorner(page,'arrow');
   await screenshot(page,'06-desktop-tall-minimap');
   const saved=await page.evaluate(key=>JSON.parse(localStorage.getItem(key)),key);
   assert(saved.width>0&&saved.height>0&&await open.isVisible(),'dragging saves both dimensions without expanding the map');
   await page.reload();
   await page.waitForFunction(()=>window.rvTestMini?.getSource('rv-car')&&rvTestMini.getPadding().left>0);
   const restored=await mini.boundingBox();
   assert(Math.abs(restored.width-tall.width)<1&&Math.abs(restored.height-tall.height)<1,'both dimensions survive a reload');
   await page.waitForFunction(()=>{const map=rvTestMini,box=map.getContainer(),canvas=map.getCanvas();
    return canvas.clientWidth===box.clientWidth&&canvas.clientHeight===box.clientHeight&&Math.abs(map.getPadding().left-box.clientWidth*.4)<1
     &&Math.abs(map.getPadding().top-Math.min(box.clientHeight*.22,document.querySelector('.video-hud').offsetTop+document.querySelector('.video-hud').offsetHeight+12))<1;
   });
   await open.click();await page.waitForFunction(()=>!document.querySelector('.video-map-canvas').style.width&&!rvTestMini.isMoving());
   await assertMiniClose(page);
   await grip.dblclick();
   await page.waitForFunction(([w,h])=>{const el=document.getElementById('videoMap');return Math.abs(el.clientWidth-w)<1&&Math.abs(el.clientHeight-h)<1&&!el.getAnimations().length},[collapsed.width,collapsed.height]);
   assert.equal(await page.evaluate(key=>localStorage.getItem(key),key),null,'double-click restores the default size');
   await seek(14);await waitSeek(page,14,.02);

   const geometry=()=>page.locator('#videoStage').boundingBox();
   const frame=await geometry(),initialResizes=await page.evaluate(()=>rvNativeResizes);
   await open.click();await page.waitForFunction(()=>!document.querySelector('.video-map-canvas').style.width&&!rvTestMini.isMoving());
   assert.equal(await page.evaluate(()=>rvNativeResizes)-initialResizes,1,'opening resizes the WebGL canvas once');
   assert.equal(await page.evaluate(()=>rvTestMini.getPitch()),0);
   assert.equal(await page.evaluate(()=>rvTestMini.getBearing()),0);
   await assertFollow(page,true);
   assert.deepEqual(await geometry(),frame,'the overlay leaves the video geometry intact');
   assert.equal(await page.evaluate(()=>!!window.L),false,'native replay loads without Leaflet');
   const pin=mini.locator('.video-map-pin'),follow=mini.locator('.video-map-follow'),overview=mini.locator('.video-map-overview'),card=mini.locator('.video-map-preview');
   await pin.click();
   const a=await nativePoint(page,routeA[1][30],routeA[1][31],.2),b=await nativePoint(page,routeA[1][38],routeA[1][39],.5);
   await page.mouse.move(...a);await page.mouse.down();await page.mouse.move(...b,{steps:5});
   await card.waitFor();assert.equal(Number(await page.locator('#seek').inputValue()),14,'scrubbing previews before committing');
   assert.match(await card.innerText(),/구간 1.*45 km\/h.*km/s);
   await page.mouse.up();await waitSeek(page,38.5);
   await assertFollow(page,true);
   await screenshot(page,'02-desktop-expanded-snap');
   const blank=await page.locator('#videoMap canvas').boundingBox();
   await page.mouse.move(blank.x+blank.width*.2,blank.y+blank.height*.4);await page.mouse.down();
   await page.mouse.move(blank.x+blank.width*.25,blank.y+blank.height*.42,{steps:5});await page.mouse.up();
   assert.equal(await follow.getAttribute('aria-pressed'),'false');
   await follow.click();await page.waitForFunction(()=>!rvTestMini.isMoving());
   assert.equal(await follow.getAttribute('aria-pressed'),'true');
   await assertFollow(page,true);
   await page.locator('#videoMap canvas').hover({position:{x:180,y:130}});await page.mouse.wheel(0,-120);
   await page.waitForFunction(()=>document.querySelector('.video-map-follow').getAttribute('aria-pressed')==='false'&&rvTestMini.getZoom()>16&&!rvTestMini.isMoving());
   await follow.click();await assertFollow(page,true);

   // Clicking a newly created bookmark seeks to its exact time and uses its title in the card.
   await page.mouse.click(...await nativePoint(page,routeA[1][14]));
   await waitSeek(page,14,.02);assert.match(await card.innerText(),/차선 합류/);
   await overview.click();await page.waitForFunction(()=>!rvTestMini.isMoving()&&rvTestMini.getZoom()<16);
   assert.equal(await follow.getAttribute('aria-pressed'),'false');
   assert(await page.evaluate(tracks=>{const box=rvTestMini.getCanvas();return tracks.every(points=>points.every(([,lat,lon])=>{
    const {x,y}=rvTestMini.project([lon,lat]);return x>=0&&x<=box.clientWidth&&y>=0&&y<=box.clientHeight;
   }))},routeA),'the entire drive fits in the expanded replay map');
   await page.mouse.click(...await nativePoint(page,routeA[3].at(-1),routeA[5][0],.5));
   assert.match(await card.innerText(),/구간 4.*미변환/s);
   await card.getByRole('button',{name:'변환',exact:true}).click();
   await card.getByText('변환 대기 중').waitFor();
   assert.equal(requests,1);
   assert(await card.getByRole('button',{name:'변환',exact:true}).isDisabled());

   // The next segment reuses the same MapLibre instance and the pinned expansion.
   await page.mouse.click(...await nativePoint(page,routeA[2][24],routeA[2][25],.5));
   await page.waitForURL(new RegExp('/view/'+logs[2].id+'/$'));await waitSeek(page,24.5);
   await close.waitFor();
   assert.equal(await pin.getAttribute('aria-pressed'),'true');
   assert.equal(await page.evaluate(()=>rvNativeCreated),1);
   const beforeClose=await page.evaluate(()=>rvNativeResizes);
   await close.click();
   await page.waitForFunction(()=>!document.querySelector('.video-map-canvas').style.width&&!rvTestMini.isMoving());
   assert.equal(await page.evaluate(()=>rvNativeResizes)-beforeClose,1,'closing resizes the WebGL canvas once');
   await page.getByRole('link',{name:'로그 목록'}).click();await page.waitForURL(base);
   assert.equal(await page.evaluate(()=>rvNativeRemoved),1,'leaving the drive releases the native map');
   assert.deepEqual(errors,[]);
   await context.close();
  }
  if(verifyShots){
   const context=await browser.newContext({viewport:{width:390,height:844},hasTouch:true,isMobile:true,colorScheme:'dark',reducedMotion:'reduce'});
   if(!REAL)await context.route(OFM+'**',stubTiles);
   const testVideo=video;
   await context.route('**/api/logs/'+withVideo+'/video**',route=>route.fulfill({body:testVideo,contentType:'video/mp4'}));
   const page=await context.newPage();await context.addInitScript(hookMaps);
   await page.goto(base+'view/'+withVideo+'/');await page.locator('#play:enabled').waitFor();
   await page.locator('#overlayHeightSettings').tap();await page.getByLabel('영상 위 작은 지도').check();await page.locator('#overlayHeightClose').tap();
   await page.waitForFunction(()=>window.rvTestMini?.getLayer('background')&&rvTestMini.getPaintProperty('background','background-color')==='#0E1219'&&rvTestMini.hasImage('rv-vehicle'));
   await screenshot(page,'03-mobile-3d-minimap');
   await page.locator('#seek').evaluate(el=>{el.value='34';el.dispatchEvent(new Event('input'))});
   await waitSeek(page,34,.02);
   await page.locator('.video-map-open').tap();await page.locator('.video-map-pin').tap();
   await assertFollow(page,true);
   await tapNative(page,routeA[1][35]);await page.locator('.video-map-preview').waitFor();
   await screenshot(page,'04-mobile-expanded-snap');
   await context.close();
  }
  // Many drives (60 of one segment and one of 40) on wide screens: the all-drives map lists them in a glass card on
  // the right (its own scroll, a row per drive in its line's colour; hover highlights, a tap selects with the actions
  // inline and fits beside the card), and the card folds away to its button (then a selection shows the bottom card).
  // The drive map's segment card scrolls on its own and folds the same way. Both fold states are kept.
  const many=Array.from({length:60},(_,k)=>({id:(0xe0000000+k).toString(16).padEnd(32,'0'),name:`${(0x500+k).toString(16).padStart(8,'0')}--5a5a5a5a5a / 구간 0`,uploaded:100+k,points:track(ORIGIN[0]-.02+(k%10)*.005,ORIGIN[1]-.03+Math.floor(k/10)*.008,5e-5,4e-5)}));
  const long=[];{let [lat,lon]=[ORIGIN[0]+.01,ORIGIN[1]-.04];for(let k=0;k<40;k++){const d=turns[k%turns.length];long.push({id:(0xf0000000+k).toString(16).padEnd(32,'0'),name:`00000600--6b6b6b6b6b / 구간 ${k}`,uploaded:200+k,points:track(lat,lon,d[0]/3,d[1]/3)});lat=+(lat+20*d[0]).toFixed(6);lon=+(lon+20*d[1]).toFixed(6)}}
  [...many,...long].forEach(writeLog);
  const drives=2+many.length+1;
  for(const scheme of PARTS.includes('many')?SCHEMES:[]){
   const context=await browser.newContext({viewport:{width:1280,height:800},colorScheme:scheme,reducedMotion:'reduce'}),page=await context.newPage(),errors=[];
   page.on('pageerror',error=>errors.push(error.message));await context.addInitScript(hookMaps);
   if(!REAL)await context.route(OFM+'**',route=>{const url=route.request().url(),style=url===SIMPLE_LIGHT?['light']:Object.entries(STYLE_OF).find(([,u])=>url===u);
    if(style)return route.fulfill({body:JSON.stringify(fixtureStyle(style[0])),contentType:'application/json'});
    if(url===OFM+'planet')return route.fulfill({body:JSON.stringify({tilejson:'3.0.0',tiles:[OFM+'planet/fixture/{z}/{x}/{y}.pbf'],minzoom:0,maxzoom:14}),contentType:'application/json'});
    return route.fulfill({body:Buffer.alloc(0),contentType:'application/x-protobuf'})});
   const shot=async name=>{if(shots){await settleLayout(page);await page.waitForTimeout(REAL?3000:150);await page.screenshot({path:path.join(shots,`${name}-${scheme}.png`)})}};
   await page.goto(base);await page.locator('#logView').waitFor();
   // Mouse screens: the route is a glass inset in the thumbnail's top-right corner; hovering the thumbnail grows the map
   // strip out of its right edge (pushing the title aside), and the strip opens 주행 지도. No GPS: none of it.
   {const c=page.locator('.log-route[data-route*="00000395"]'),thumb=c.locator('.route-thumb'),strip=c.locator('.route-strip');
    await c.locator('.route-inset svg').waitFor();await c.scrollIntoViewIfNeeded();await page.mouse.move(640,5);
    const rects=()=>c.evaluate(e=>Object.fromEntries(['.route-thumb','.route-inset','.route-strip','.route-main'].map(sel=>{const r=e.querySelector(sel).getBoundingClientRect();return [sel.slice(7),{x:r.x,y:r.y,w:r.width,h:r.height}]})));
    const rect=async sel=>(await rects())[sel.slice(7)];
    await page.waitForTimeout(300);const {thumb:t,inset,strip:rest,main:title}=await rects();
    assert(Math.round(inset.w)===46&&Math.abs(t.x+t.w-inset.x-inset.w-6)<1.5&&Math.abs(inset.y-t.y-6)<1.5&&rest.w===0,JSON.stringify({t,inset,rest}));
    assert(await c.locator('.route-map-button').isHidden(),'no 지도 button with a mouse');
    assert.equal(await c.locator('.route-map-button svg .shape-line').count(),0,'desktop skips unused mobile route SVG');
    assert.equal(await page.locator('.log-route[data-route*="00000397"] :is(.route-inset,.route-strip,.route-map-button)').count(),0);
    if(shots){await settleLayout(page);await c.screenshot({path:path.join(shots,`card-rest-${scheme}.png`)})}
    await thumb.hover();await page.waitForTimeout(400);
    const {strip:open,main:moved,thumb:t2}=await rects();t.x=t2.x;t.y=t2.y;title.x+=0;
    assert(Math.abs(open.w-open.h)<1.5&&Math.abs(open.x-t.x-t.w)<1.5&&Math.abs(open.h-t.h)<1.5&&moved.x>=title.x+open.w-2,JSON.stringify({open,t,title,moved}));
    assert.equal(await c.locator('.route-inset').evaluate(e=>getComputedStyle(e).opacity),'0');
    if(shots){await c.screenshot({path:path.join(shots,`card-hover-${scheme}.png`)})}
    assert.equal(await strip.getAttribute('aria-label'),'주행 지도 열기');
    await strip.click();await page.waitForFunction(()=>document.getElementById('driveMap').open);
    // With a mouse (pointer:fine) the broad button rule (min-height 36px) must not stretch the map's round ⓘ.
    {const toggle=page.locator('#driveMap .rv-credit-toggle');await toggle.waitFor();const box=await toggle.boundingBox();assert(Math.abs(box.width-24)<.5&&Math.abs(box.height-24)<.5,'the credit ⓘ stays round with a mouse: '+JSON.stringify(box))}
    assert(!await c.evaluate(e=>e.open),'the strip does not fold the card');
    await page.keyboard.press('Escape');await page.waitForFunction(()=>!document.getElementById('driveMap').open);
    assert(await page.evaluate(()=>document.activeElement===document.body),'closing hands focus to nothing');
    await page.mouse.move(640,5);await page.waitForTimeout(400);assert.equal((await rect('.route-strip')).w,0,'folds back')}
   await page.locator('#logView').getByRole('button',{name:'지도'}).click();
   const rows=page.locator('#logMapRows .map-list-row'),fold=page.locator('#logMap>.map-fold');
   await page.waitForFunction(n=>document.querySelectorAll('#logMapRows .map-list-row').length===n&&document.querySelectorAll('#logMap .leaflet-overlay-pane path.leaflet-interactive').length===n,drives,{timeout:20000});
   const card=()=>page.evaluate(()=>{const r=s=>document.querySelector(s).getBoundingClientRect(),map=r('#logMap'),list=r('#logMapList'),button=r('#logMap>.map-fold'),rows=document.getElementById('logMapRows'),zoom=r('#logMap .leaflet-control-zoom'),credit=r('#logMap .rv-credit-toggle');
    return {right:Math.round(map.right-list.right),below:Math.abs(list.top-button.top)<=1,bottom:list.bottom<=map.bottom-15,w:Math.round(list.width),scrolls:rows.scrollHeight>rows.clientHeight+40,button:[map.right-button.right,button.top-map.top].map(Math.round),size:Math.round(button.width),
     clear:zoom.right<list.left&&map.right-credit.right>=0&&map.right-credit.right<=8&&map.bottom-credit.bottom>=0&&map.bottom-credit.bottom<=8&&list.bottom<=credit.top,glass:getComputedStyle(document.querySelector('#logMapRows>.map-list-row')).backdropFilter!=='none'&&getComputedStyle(document.getElementById('logMapList')).backgroundColor==='rgba(0, 0, 0, 0)',shown:!!list.width}});
   {const g=await card();assert(g.shown&&g.right===16&&g.below&&g.bottom&&g.w>=300&&g.w<=380&&g.scrolls&&g.clear&&g.glass&&g.button[0]===16&&g.button[1]===16&&g.size===40,JSON.stringify(g))}
   assert.equal(await fold.getAttribute('aria-expanded'),'true');assert.equal(await fold.getAttribute('aria-label'),'주행 목록 접기');
   assert.equal(await page.locator('#logMapList .log-map-list-count').innerText(),`${drives}개`);
   // The count label's bottom lines up with the fold button's; the first card starts at least 12px below them.
   {const [head,button,first]=await page.evaluate(()=>['#logMapList .log-map-list-head','#logMap>.map-fold','#logMapRows>.map-list-row'].map(s=>document.querySelector(s).getBoundingClientRect()));
    assert(Math.abs(head.bottom-button.bottom)<=1&&first.top-button.bottom>=12,'label on the fold button bottom line, cards below '+JSON.stringify([head.bottom,button.bottom,first.top]))}
   // Each row in its line's colour; a hovered row's line stands out.
   const strong=()=>page.locator('#logMap .leaflet-overlay-pane path.leaflet-interactive').evaluateAll(lines=>lines.map((l,i)=>Number(l.getAttribute('stroke-width'))===6?l.getAttribute('stroke'):null).filter(Boolean));
   await rows.nth(4).hover();await page.waitForTimeout(50);
   {const lit=await strong(),swatch=await rows.nth(4).locator('.map-list-pick').evaluate(b=>getComputedStyle(b,'::before').backgroundColor);assert.equal(lit.length,1,'the hovered drive');assert.equal(swatch,await page.evaluate(c=>{const e=document.createElement('i');e.style.color=c;document.body.append(e);const v=getComputedStyle(e).color;e.remove();return v},lit[0]),'the row\'s swatch is its line\'s colour')}
   // A tap selects: actions inline, no bottom card, the drive fitted beside the card.
   await rows.nth(4).locator('.map-list-pick').click();await page.waitForTimeout(250);
   assert.equal(await rows.nth(4).locator('.map-list-pick').evaluate(b=>getComputedStyle(b).backgroundColor),'rgba(0, 0, 0, 0)','the selected card lights evenly: no hover tint on its title part alone');
   await page.mouse.move(10,400);await page.waitForTimeout(150);
   assert(await rows.nth(4).evaluate(r=>r.classList.contains('is-selected')));
   assert.deepEqual(await rows.nth(4).locator('.map-card-actions>:is(a,button)').allInnerTexts(),['재생','주행 지도','목록에서 보기']);
   assert(await page.locator('#logMapCard').isHidden(),'no bottom card while the list shows');
   {const g=await page.evaluate(()=>{const list=document.getElementById('logMapList').getBoundingClientRect(),line=[...document.querySelectorAll('#logMap .leaflet-overlay-pane path.leaflet-interactive')].find(p=>Number(p.getAttribute('stroke-width'))===6).getBoundingClientRect();return {left:line.right<=list.left+1,line:[line.left,line.right].map(Math.round),list:Math.round(list.left)}});assert(g.left,JSON.stringify(g))}
   await shot('list-wide');
   // The cards fade out at an edge only with more beyond it: at the top of the column, only at the bottom.
   {const fades=()=>page.evaluate(()=>{const c=document.getElementById('logMapRows'),cards=[...c.children];return {top:c.classList.contains('fade-top'),bottom:c.classList.contains('fade-bottom'),masked:cards.filter(e=>e.style.maskImage).length}});
    await page.evaluate(()=>document.getElementById('logMapRows').scrollTop=0);await page.waitForTimeout(100);
    {const f=await fades();assert(!f.top&&f.bottom&&f.masked>=1,JSON.stringify(f))}
    // The top fade grows with the distance scrolled (none, half after 7px, full after 14px), and a masked card keeps its
    // shadow (the mask reaches past it, unclipped).
    {const widths=[];for(const top of [0,7,40]){widths.push(await page.evaluate(top=>new Promise(done=>{const c=document.getElementById('logMapRows');c.scrollTop=top;requestAnimationFrame(()=>requestAnimationFrame(()=>done(parseFloat(c.style.getPropertyValue('--fade-top'))))) }),top))}
     assert.deepEqual(widths,[0,14,28],'top fade width follows the scroll distance');
     const masked=await page.evaluate(()=>{const card=[...document.getElementById('logMapRows').children].find(e=>e.style.maskImage),st=card&&getComputedStyle(card);return st&&[st.maskClip,st.maskRepeat,st.boxShadow!=='none']});
     assert.deepEqual(masked,['no-clip','no-repeat',true],'the masked card keeps its shadow')}
    await page.evaluate(()=>{const c=document.getElementById('logMapRows');c.scrollTop=c.scrollHeight/2});await page.waitForTimeout(100);
    {const f=await fades();assert(f.top&&f.bottom&&f.masked>=2,JSON.stringify(f))}
    await shot('list-wide-scrolled');
    await page.evaluate(()=>{const c=document.getElementById('logMapRows');c.scrollTop=c.scrollHeight});await page.waitForTimeout(100);
    {const f=await fades();assert(f.top&&!f.bottom,JSON.stringify(f))}
    await rows.nth(4).scrollIntoViewIfNeeded()}
   // 지도 표시: one layers button under the zoom buttons opens a popover (towards the map's middle; Escape or a tap
   // elsewhere closes it) with two switches. 글자 숨기기 hides every name and symbol in the style's own colours (no other
   // style); 회색 지도 switches to the calm style (Positron light / Dark) without 3D buildings and points of interest,
   // names kept. Both keep the view and are kept on this browser (off by default).
   {const button=page.locator('#logMap .rv-map-options-button'),pop=page.locator('.rv-map-options:not([hidden])');  // open, it sits on the page (above whatever lies over the map)
    const box=kind=>pop.locator(`input[data-kind=${kind}]`),open=async()=>{if(await pop.isHidden()){await button.click();await pop.waitFor()}};
    {const z=await page.locator('#logMap .leaflet-control-zoom').boundingBox(),b=await button.boundingBox();assert(Math.abs(b.x-z.x)<2&&b.y>=z.y+z.height-1&&b.y<z.y+z.height+16&&Math.abs(b.width-z.width)<3,JSON.stringify({z,b}))}
    assert(await pop.isHidden());assert.equal(await button.getAttribute('aria-expanded'),'false');
    await open();assert.equal(await button.getAttribute('aria-expanded'),'true');
    {const [b,p,m]=await Promise.all([button.boundingBox(),pop.boundingBox(),page.locator('#logMap .rv-map').boundingBox()]);
     assert(p.x>=m.x-1&&p.x+p.width<=m.x+m.width+1&&p.y>=m.y-1,'inside the map '+JSON.stringify({b,p,m}));assert(p.x>=b.x+b.width||p.x+p.width<=b.x,'beside the button '+JSON.stringify({b,p}))}
    assert.deepEqual(await pop.locator('b').allInnerTexts(),['글자 숨기기','회색 지도']);assert.equal(await box('labels').isChecked(),false);assert.equal(await box('grey').isChecked(),false);
    if(scheme==='light')await shot('list-wide-options');
    await page.keyboard.press('Escape');await pop.waitFor({state:'hidden'});
    const askedStyles=[];page.on('request',r=>{if(r.url().startsWith(OFM+'styles/'))askedStyles.push(r.url())});
    await open();await box('labels').check();
    await page.waitForFunction(()=>document.querySelector('#logMap .rv-map').dataset.labels==='hidden',null,{timeout:5000});
    assert.deepEqual(askedStyles,[],'글자 숨기기 asks no other style');
    assert.equal(await page.evaluate(()=>document.querySelector('#logMap .rv-map').dataset.simple),'false','the colours stay');
    assert.equal(await page.evaluate(()=>localStorage.getItem('roadviewer-map-nolabels')),'1');assert(await button.evaluate(b=>b.classList.contains('is-on')),'the button shows a choice is on');
    if(scheme==='light')await shot('list-wide-nolabels');
    await box('labels').uncheck();await page.waitForFunction(()=>document.querySelector('#logMap .rv-map').dataset.labels==='shown',null,{timeout:5000});
    assert.equal(await page.evaluate(()=>localStorage.getItem('roadviewer-map-nolabels')),'0');
    await page.mouse.click(5,400);await pop.waitFor({state:'hidden'});
    const styleUrls=[];page.on('request',r=>{if(r.url().startsWith(OFM+'styles/'))styleUrls.push(r.url())});
    const view=()=>page.evaluate(()=>document.querySelector('#logMap .leaflet-map-pane').style.transform);
    const before=await view();
    await open();await box('grey').check();
    await page.waitForFunction(()=>document.querySelector('#logMap .rv-map').dataset.simple==='true');
    await page.waitForFunction(()=>(document.querySelector('#logMap .rv-map').dataset.removed||'')!=='',null,{timeout:20000});await page.waitForTimeout(REAL?2500:200);
    if(scheme==='light')assert(styleUrls.includes(SIMPLE_LIGHT),'grey style '+styleUrls);
    else assert.deepEqual(styleUrls,[],'dark grey style reuses cached basemap '+styleUrls);
    assert.equal(await page.evaluate(()=>document.querySelector('#logMap .rv-map').dataset.labels),'shown','회색 지도 keeps the names');
    {const removed=(await page.evaluate(()=>document.querySelector('#logMap .rv-map').dataset.removed)).split(' ');
     assert(!REAL?removed.includes('building-3d')&&removed.includes('poi_r1')&&!removed.includes('street-name'):removed.every(id=>!/name|label/.test(id)||/poi/.test(id)),removed.join(' '))}
    assert.equal(await view(),before,'the view stays');assert(await rows.nth(4).evaluate(r=>r.classList.contains('is-selected')),'the selection stays');
    assert.equal(await page.evaluate(()=>localStorage.getItem('roadviewer-map-simple')),'1');
    await page.keyboard.press('Escape');await pop.waitFor({state:'hidden'});
    if(scheme==='light')await shot('list-wide-simple');
    await page.reload();await page.locator('#logView').waitFor();await page.waitForFunction(()=>document.querySelector('#logMap .rv-map-options-button'),null,{timeout:20000});
    await open();assert.equal(await box('grey').isChecked(),true,'kept on');
    await box('grey').uncheck();assert.equal(await page.evaluate(()=>localStorage.getItem('roadviewer-map-simple')),'0');
    await page.waitForFunction(()=>document.querySelector('#logMap .rv-map').dataset.simple==='false');await page.keyboard.press('Escape');
    await page.waitForFunction(n=>document.querySelectorAll('#logMap .leaflet-overlay-pane path.leaflet-interactive').length===n,drives,{timeout:20000});
    await rows.nth(4).locator('.map-list-pick').click();await page.mouse.move(10,400);await page.waitForTimeout(150)}
   // A line tapped on the map selects its row and scrolls the list to it.
   {const last=page.locator('#logMap .leaflet-overlay-pane path.leaflet-interactive').last();await last.evaluate(p=>{const L=window.L;p.dispatchEvent(new MouseEvent('click',{bubbles:true,clientX:0,clientY:0}))});
    await page.waitForFunction(()=>{const r=document.querySelector('#logMapRows .map-list-row.is-selected');if(!r)return false;const b=r.getBoundingClientRect(),l=document.getElementById('logMapRows').getBoundingClientRect();return b.top>=l.top-1&&b.bottom<=l.bottom+1})}
   // Picking rows one after another (each grows with its buttons, the last one shrinks back) leaves no card in the
   // middle of the list with an edge fade: a fade kept from a card's earlier place cut it off and hid the cards below.
   {await page.evaluate(()=>{document.getElementById('logMapRows').scrollTop=0});
    for(const i of [3,2,1,0,1,0,2]){await rows.nth(i).locator('.map-list-pick').click();await page.mouse.move(10,400);await page.waitForTimeout(350);
     const wrong=await page.evaluate(()=>{const c=document.getElementById('logMapRows'),a=c.getBoundingClientRect();return [...c.children].map((k,j)=>({j,b:k.getBoundingClientRect(),mask:k.style.maskImage})).filter(k=>k.mask&&k.b.top>=a.top+45&&k.b.bottom<=a.bottom-45).map(k=>k.j)});
     assert.deepEqual(wrong,[],'after picking row '+i)}}
   // A row tapped in the list comes to the middle of the list, also when it was already in sight.
   {await rows.nth(30).locator('.map-list-pick').click();await page.mouse.move(10,400);
    await page.waitForFunction((([list,row])=>{const l=document.querySelector(list),r=l?.querySelector(row);if(!r)return false;const a=l.getBoundingClientRect(),b=r.getBoundingClientRect(),off=(b.top+b.height/2)-(a.top+a.height/2);return Math.abs(off)<6||(off<0&&l.scrollTop<=1)||(off>0&&l.scrollTop>=l.scrollHeight-l.clientHeight-1)}),['#logMapRows','.map-list-row.is-selected']);
    assert(await rows.nth(30).evaluate(r=>r.classList.contains('is-selected')))}
   // At the end of the list, with the last row selected, a row tapped above it must not make the list jump: the last row
   // loses its buttons, the list is shorter for a moment and the browser pulled the position up in one step.
   {await page.emulateMedia({reducedMotion:'no-preference'});const n=await rows.count();
    await rows.nth(n-1).locator('.map-list-pick').click();await page.mouse.move(10,400);await page.waitForTimeout(900);
    const from=await page.evaluate(()=>{const l=document.getElementById('logMapRows');window.rvLow=l.scrollTop;l.addEventListener('scroll',()=>{window.rvLow=Math.min(window.rvLow,l.scrollTop)});return {top:l.scrollTop,max:l.scrollHeight-l.clientHeight}});
    await rows.nth(n-2).locator('.map-list-pick').click();await page.mouse.move(10,400);await page.waitForTimeout(700);
    const s=await page.evaluate(()=>({low:window.rvLow,top:document.getElementById('logMapRows').scrollTop}));
    assert(from.top>=from.max-1&&s.low>=from.top-3&&s.top>=from.top-3&&await rows.nth(n-2).evaluate(r=>r.classList.contains('is-selected')),'the list stays at its end '+JSON.stringify({from,s}));
    await page.emulateMedia({reducedMotion:'reduce'});await rows.nth(30).locator('.map-list-pick').click();await page.mouse.move(10,400);await page.waitForTimeout(400)}
   // With motion, the bottom card waits for the folding cards to leave before it rises in (it used to cross them).
   {await page.emulateMedia({reducedMotion:'no-preference'});await fold.click();
    const early=await page.locator('#logMapCard').evaluate(c=>({hidden:c.hidden,delay:parseFloat(c.style.animationDelay),fill:c.style.animationFillMode,opacity:getComputedStyle(c).opacity}));
    assert(!early.hidden&&early.delay>=200&&early.delay<=300&&early.fill==='backwards'&&early.opacity==='0',JSON.stringify(early));
    await page.waitForFunction(()=>getComputedStyle(document.getElementById('logMapCard')).opacity==='1');
    await fold.click();await page.locator('#logMapCard').waitFor({state:'hidden'});await page.emulateMedia({reducedMotion:'reduce'});await page.waitForTimeout(500)}
   // Folded: only the button (same place, chevron turned), the bottom card for a selection, no right padding.
   const pane=()=>page.evaluate(()=>getComputedStyle(document.querySelector('#logMap .leaflet-map-pane')).transform+'|'+document.querySelector('#logMap .leaflet-proxy')?.style.transform);
   const view=await pane();
   const before=await fold.boundingBox();await fold.click();
   await page.waitForTimeout(400);assert.equal(await pane(),view,'folding keeps the map where it is');
   assert.equal(await fold.getAttribute('aria-expanded'),'false');assert.equal(await fold.getAttribute('aria-label'),'주행 목록 펼치기');
   assert(await page.locator('#logMapList').isHidden());assert.deepEqual(await fold.boundingBox(),before,'the button stays put');
   // Only the cards that were in sight fade and slide away (is-seen); the others, clipped by the column, are left alone
   // (with every card of a long list animating, folding stuttered on phones).
   {const s=await page.evaluate(()=>{const k=[...document.getElementById('logMapRows').children],seen=k.filter(r=>r.classList.contains('is-seen'));return {seen:seen.length,all:k.length,first:seen[0]&&getComputedStyle(seen[0]).opacity,rest:getComputedStyle(k.find(r=>!r.classList.contains('is-seen'))).opacity}});
    assert(s.seen>0&&s.seen<12&&s.all>40&&s.first==='0'&&s.rest==='1',JSON.stringify(s))}
   assert(await page.locator('#logMapCard').isVisible(),'folded: the selection shows the bottom card');
   assert.equal(await page.evaluate(()=>localStorage.getItem('roadviewer-map-list-folded')),'1');
   await shot('list-wide-folded');
   await page.reload();await page.locator('#logView').waitFor();await page.waitForFunction(()=>document.querySelectorAll('#logMap .leaflet-overlay-pane path.leaflet-interactive').length>0,null,{timeout:20000});
   assert.equal(await fold.getAttribute('aria-expanded'),'false','kept folded');assert(await page.locator('#logMapList').isHidden());
   await fold.click();await page.locator('#logMapList').waitFor();
   await page.waitForTimeout(400);assert.equal(await pane(),view,'unfolding keeps the map where it is');
   // The 40-segment drive's map: its segment card scrolls on its own and folds like the list.
   const longRow=rows.filter({hasText:'40개 구간'});await longRow.locator('.map-list-pick').click();await longRow.getByRole('button',{name:'주행 지도'}).click();
   await page.waitForFunction(()=>{const d=document.getElementById('driveMap');return d.open&&d.querySelectorAll('.drive-map-list>.log-row').length===40&&d.querySelectorAll('.leaflet-overlay-pane path.leaflet-interactive').length===40},null,{timeout:20000});
   await settleLayout(page,'#driveMap');
   // Its 지도 표시 popover: Escape closes the popover only, the drive map stays open.
   {const pop=page.locator('#driveMap>.rv-map-options:not([hidden])');await page.locator('#driveMap .rv-map-options-button').click();await pop.waitFor();
    // Drawn above the segment cards: the popover's own middle is what a tap there reaches.
    assert(await pop.evaluate(p=>{const r=p.getBoundingClientRect();return p.contains(document.elementFromPoint(r.left+r.width/2,r.top+r.height-6))}),'the popover is drawn above the segment cards');
    // 지도 테마: 자동 (the app's theme) | 라이트 | 다크, for the page's maps, kept on this browser.
    {const box=page.locator('#driveMap .drive-map-canvas'),tone=name=>pop.locator(`button[data-tone=${name}]`),other=scheme==='dark'?'light':'dark';
     assert.equal(await tone('auto').getAttribute('aria-pressed'),'true');assert.equal(await box.getAttribute('data-tiles'),scheme);
     await tone(other).click();await page.waitForFunction(o=>document.querySelector('#driveMap .drive-map-canvas').dataset.tiles===o,other);
     assert.equal(await tone(other).getAttribute('aria-pressed'),'true');assert.equal(await page.evaluate(()=>localStorage.getItem('roadviewer-map-tone')),other);
     assert(await page.locator('#driveMap .rv-map-options-button').evaluate(b=>b.classList.contains('is-on')),'the layers button shows a fixed theme');
     // Only the map changes: the cards over it stay in the app's theme (nearly solid glass, so the other map does not show through as theirs).
     assert.equal(await page.evaluate(()=>document.documentElement.dataset.mapContrast),other);
     {const alpha=await page.locator('#driveMap .drive-map-list>.log-row').first().evaluate(row=>Number(getComputedStyle(row).backgroundColor.match(/[\d.]+(?=\))/)?.[0]??1));assert(alpha>.8,'solid cards over the other theme\'s map '+alpha)}
     await tone('auto').click();await page.waitForFunction(o=>document.querySelector('#driveMap .drive-map-canvas').dataset.tiles===o,scheme);
     assert.equal(await page.evaluate(()=>localStorage.getItem('roadviewer-map-tone')),'auto');assert.equal(await page.evaluate(()=>document.documentElement.dataset.mapContrast),undefined)}
    await page.keyboard.press('Escape');await pop.waitFor({state:'hidden'});await page.waitForTimeout(200);assert(await page.evaluate(()=>document.getElementById('driveMap').open),'the drive map stays open')}
   {const g=await page.evaluate(()=>{const d=document.getElementById('driveMap'),r=s=>d.querySelector(s).getBoundingClientRect(),sheet=r('.drive-map-sheet'),button=r('.drive-map-fold'),list=d.querySelector('.drive-map-list');return {scrolls:list.scrollHeight>list.clientHeight+40,bottom:Math.round(innerHeight-sheet.bottom),below:Math.abs(sheet.top-button.top)<=1,button:[innerWidth-button.right].map(Math.round)}});
    assert(g.scrolls&&g.bottom>=16&&g.below&&g.button[0]===16,JSON.stringify(g))}
   await shot('drive-wide-40');
   {const button=page.locator('#driveMap .drive-map-fold'),at=await button.boundingBox();await button.click();
    assert.equal(await button.getAttribute('aria-expanded'),'false');assert(await page.locator('#driveMap .drive-map-sheet').isHidden());assert.deepEqual(await button.boundingBox(),at);
    // Folded with a segment marked: its glass pill shows at the bottom with 재생; ✕ clears the mark and the pill.
    {const pill=page.locator('#driveMap .drive-map-card');assert(await pill.isHidden(),'no pill without a marked segment');
     await button.click();await page.locator('#driveMap .drive-map-list>.log-row .log-meta').first().click();await button.click();await pill.waitFor();assert.match(await pill.locator('.map-card-title').innerText(),/^구간 \d+/);
     assert(await pill.locator('a.map-card-play').count()===1,'재생 in the pill');await shot('drive-wide-folded-pill');
     assert.equal(await pill.locator('.map-card-actions>*').count(),2,'재생 and ✕ only');
     await pill.locator('.map-card-close').click();
     // It sinks out (no instant cut) before it is hidden.
     if(!await page.evaluate(()=>matchMedia('(prefers-reduced-motion: reduce)').matches))assert.equal(await pill.evaluate(p=>p.classList.contains('is-leaving')&&getComputedStyle(p).animationName),'rv-map-card-out','the pill animates out');
     await pill.waitFor({state:'hidden'});assert.equal(await page.locator('#driveMap .drive-map-list>.log-row.is-map-marked').count(),0)}
    await shot('drive-wide-folded');await button.click();assert(await page.locator('#driveMap .drive-map-sheet').isVisible())}
   assert.deepEqual(errors,[]);
   await context.close();
   if(scheme==='dark'){
    // Mobile regression checks on a 63-drive list:
    // ① pageScope forwards IntersectionObserver options (rootMargin: '240px 0px'),
    // ② mobile skips desktop SVGs (.route-inset, .route-strip),
    // ③ 2-stage /api/tracks?ids=... fills top cards while full /api/tracks is still pending,
    // ④ tapping a pending card's 지도 button opens 주행 지도 immediately via /api/logs/{id}/track.
    const mContext=await browser.newContext({viewport:{width:390,height:844},hasTouch:true,isMobile:true,colorScheme:'dark',reducedMotion:'reduce'});
    await mContext.addInitScript(()=>{
     window.rvObserverOptions=[];
     const Orig=window.IntersectionObserver;
     window.IntersectionObserver=class extends Orig{constructor(cb,opts){super(cb,opts);window.rvObserverOptions.push(opts)}};
    });
    await mContext.addInitScript(hookMaps);
    if(!REAL)await mContext.route(OFM+'**',stubTiles);
    const mPage=await mContext.newPage(),mErrors=[];
    mPage.on('pageerror',e=>mErrors.push(e.message));
    const trackUrls=[];let releaseFull;const fullHeld=new Promise(r=>{releaseFull=r});
    await mPage.route('**/api/tracks*',async route=>{
     const url=route.request().url();trackUrls.push(url);
     if(!url.includes('?ids='))await fullHeld;
     await route.continue();
    });
    await mPage.goto(base);
    await mPage.locator('.route-map-button svg .shape-line').first().waitFor();
    // ① pageScope forwarded IntersectionObserver options.
    assert(await mPage.evaluate(()=>window.rvObserverOptions.some(o=>o&&o.rootMargin==='240px 0px')),'IntersectionObserver receives rootMargin 240px 0px');
    // ② Mobile skips unused desktop SVGs.
    assert.equal(await mPage.locator('.route-inset svg, .route-strip svg').count(),0,'mobile skips desktop inset/strip SVGs');
    // ③ Stage 1 (?ids= with 12 drives) completed while full /api/tracks is still held.
    const firstStageUrl=trackUrls.find(u=>u.includes('?ids='));
    assert(firstStageUrl,'two-stage loading requests ?ids= first: '+JSON.stringify(trackUrls));
    assert.equal(new URL(firstStageUrl).searchParams.get('ids').split(',').length,12,'priority batch requests 12 drives');
    assert(await mPage.locator('.route-map-button svg .shape-line').count()>=6,'top cards rendered shapes from stage 1 before full /api/tracks');
    // ④ Card outside the first 12 (still pending full /api/tracks) shows placeholder and opens 주행 지도 immediately on tap.
    const pendingCard=mPage.locator('.log-route').nth(20);
    await pendingCard.scrollIntoViewIfNeeded();
    assert.equal(await pendingCard.locator('.route-map-button svg .shape-line').count(),0,'card outside priority batch has no route shape yet');
    assert.equal(await pendingCard.locator('.route-map-button .route-placeholder').count(),1,'pending button displays placeholder icon');
    await pendingCard.locator('.route-map-button').tap();
    await mPage.waitForFunction(()=>{const d=document.getElementById('driveMap');return d.open&&d.querySelectorAll('.leaflet-overlay-pane path.leaflet-interactive').length>=1},{timeout:15000});
    assert.match(await mPage.locator('#driveMapFacts').innerText(),/\d+\.\d km|m/,'drive dialog shows distance from single-drive track API');
    await mPage.keyboard.press('Escape');
    await mPage.waitForFunction(()=>!document.getElementById('driveMap').open);
    // Release full /api/tracks and confirm the previously pending card now draws its route shape.
    releaseFull();
    await pendingCard.locator('.route-map-button svg .shape-line').waitFor({timeout:15000});
    assert.equal(await mPage.locator('.route-inset svg, .route-strip svg').count(),0,'mobile still skips desktop SVGs after full load');
    assert.deepEqual(mErrors,[]);
    await mContext.close();
   }
  }
  // Opening one segment after another does not keep the pages left behind: nodes, listeners and heap stay level (a
  // listener left on the window or on the connection object, or a map never removed, kept every page and its log:
  // 1,500 nodes more with each one). The measured page is never touched by evaluate or a selector wait (the debugger's
  // own handles keep a page alive): the buttons' places come from a second page, and only taps and counters follow.
  if(PARTS.includes('leak')&&SCHEMES.includes('dark')){const context=await browser.newContext({viewport:{width:390,height:844},hasTouch:true,isMobile:true,colorScheme:'dark',reducedMotion:'reduce'});
   await context.route(OFM+'**',route=>{const url=route.request().url(),style=url===SIMPLE_LIGHT?['light']:Object.entries(STYLE_OF).find(([,u])=>url===u);
    if(style)return route.fulfill({body:JSON.stringify(fixtureStyle(style[0])),contentType:'application/json'});
    if(url===OFM+'planet')return route.fulfill({body:JSON.stringify({tilejson:'3.0.0',tiles:[OFM+'planet/fixture/{z}/{x}/{y}.pbf'],minzoom:0,maxzoom:14}),contentType:'application/json'});
    return route.fulfill({body:Buffer.alloc(0),contentType:'application/x-protobuf'})});
   await context.route(`**/api/logs/${withVideo}/video**`,route=>route.fulfill({body:video,contentType:'video/mp4'}));
   const start=base+'view/'+logs[3].id+'/',probe=await context.newPage();await probe.goto(start);await probe.waitForSelector('#nextLog');await probe.waitForTimeout(1200);
   const at=await probe.evaluate(()=>Object.fromEntries(['nextLog','previousLog'].map(id=>{const r=document.getElementById(id).getBoundingClientRect();return [id,[r.left+r.width/2,r.top+r.height/2]]})));await probe.close();
   const page=await context.newPage();await page.goto(start);await page.waitForTimeout(2500);
   const cdp=await context.newCDPSession(page);await cdp.send('Performance.enable');await cdp.send('HeapProfiler.enable');
   const count=async()=>{for(let i=0;i<3;i++){await cdp.send('HeapProfiler.collectGarbage');await new Promise(r=>setTimeout(r,250))}
    const m=Object.fromEntries((await cdp.send('Performance.getMetrics')).metrics.map(x=>[x.name,x.value]));return {nodes:m.Nodes,listeners:m.JSEventListeners,url:page.url().slice(-9,-1)}};
   const tap=async id=>{const [x,y]=at[id];await cdp.send('Input.dispatchTouchEvent',{type:'touchStart',touchPoints:[{x,y}]});await cdp.send('Input.dispatchTouchEvent',{type:'touchEnd',touchPoints:[]});await new Promise(r=>setTimeout(r,1300))};
   const before=await count();for(const id of ['previousLog','previousLog','nextLog','nextLog','nextLog','previousLog'])await tap(id);const after=await count();
   assert.equal(before.url,logs[3].id.slice(-8));assert.equal(after.url,logs[3].id.slice(-8),'the taps opened other segments and came back '+JSON.stringify(after));
   assert(after.nodes<=before.nodes*1.2&&after.listeners<=before.listeners*1.2,'pages left behind are freed '+JSON.stringify({before,after}));
   await context.close()}
  console.log('maps ok');
 }finally{await browser?.close();python.kill();fs.rmSync(dir,{recursive:true,force:true})}
})().catch(error=>{console.error(error);process.exit(1)});
