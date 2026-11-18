const fs=require('fs'),assert=require('node:assert/strict'),{chromium}=require('playwright');
// Full screen on phones and tablets shows the video in landscape: the browser's orientation lock where it works, and a
// quarter-turned frame where it is refused (the Home Assistant app's Android WebView), with touch seeking along the turn.
const serve=route=>{
 const p=new URL(route.request().url()).pathname;
 if(p.endsWith('/bookmarks'))return route.fulfill({json:{bookmarks:[]}});
 if(p==='/api/logs')return route.fulfill({json:{logs:[{id:'one',name:'00000395--0d0eda17c5 / 구간 7',status:'ready',pinned:false}]}});
 if(p.endsWith('/video'))return route.fulfill({body:fs.readFileSync(process.env.RV_TEST_VIDEO||'/tmp/roadviewer-test.mp4'),contentType:'video/mp4'});
 if(p.endsWith('/telemetry'))return route.fulfill({status:404,json:{error:'차량 정보 없음'}});
 if(p.endsWith('/data'))return route.fulfill({json:{route:'test',key:'overlay',duration:2,warnings:[],video:{start:0,duration:2},frames:[{t:0,id:0,valid:true,lanes:[],edges:[],lp:[],es:[],leads:[],liveTracksValid:false}]}});
 const name=p.startsWith('/view/')?'index.html':p.replace('/assets/','');
 return route.fulfill({body:fs.readFileSync('roadviewer/app/web/'+name),contentType:name.endsWith('.html')?'text/html':name.endsWith('.js')?'application/javascript':name.endsWith('.css')?'text/css':name.endsWith('.png')?'image/png':'image/svg+xml'});
};
async function open(browser,options,lock){
 const page=await browser.newPage(options),errors=[];page.on('pageerror',e=>errors.push(e.message));
 // The orientation lock is the browser's; here it records calls and resolves or refuses as asked.
 await page.addInitScript(lock=>{window.lockCalls=[];window.unlockCalls=0;const o=screen.orientation;
  o.lock=type=>{window.lockCalls.push(type);return lock==='resolve'?Promise.resolve():Promise.reject(new DOMException('not supported','NotSupportedError'))};
  o.unlock=()=>{window.unlockCalls++}},lock);
 await page.route('https://rv.test/**',serve);await page.goto('https://rv.test/view/one/');
 await page.waitForFunction(()=>!document.getElementById('play').disabled);page.errors=errors;return page;
}
const state=page=>page.evaluate(()=>{
 const stage=document.querySelector('.camera-panel'),frame=stage.querySelector('.stage-frame').getBoundingClientRect(),wrap=document.getElementById('videoStage'),bar=document.querySelector('.playback');
 return {full:document.fullscreenElement===stage,turned:stage.classList.contains('is-turned'),root:document.documentElement.classList.contains('stage-turned'),
  frame:[frame.left,frame.top,frame.width,frame.height].map(Math.round),wrap:[wrap.clientWidth,wrap.clientHeight],bar:bar.offsetWidth,
  transport:getComputedStyle(document.documentElement).getPropertyValue('--transport-height'),barHeight:bar.offsetHeight,strip:getComputedStyle(document.getElementById('segmentStrip')).display,lock:window.lockCalls,unlock:window.unlockCalls};
});
// Measure in the frame's local vertical axis, including the phone's quarter-turn fallback.
async function checkIdleOverlays(page){
 await page.evaluate(()=>{data.frames[0].gap=2;render()});
 await page.locator('#videoGap').waitFor({state:'visible'});
 const measure=()=>page.evaluate(()=>{
  const stage=document.querySelector('.camera-panel'),frame=document.getElementById('videoStage').getBoundingClientRect();
  const gap=document.getElementById('videoGap'),r=gap.getBoundingClientRect(),turned=stage.classList.contains('is-turned');
  const messages=document.querySelector('.video-messages').getBoundingClientRect();
  const probe=document.createElement('div');probe.style.paddingBottom='env(safe-area-inset-bottom)';stage.append(probe);
  const safe=parseFloat(getComputedStyle(probe).paddingBottom);probe.remove();
  // How far the screen goes on below the picture (the transport bar overlaps the picture only by what is left of it).
  const panel=stage.getBoundingClientRect(),below=turned?frame.left-panel.left:panel.bottom-frame.bottom;
  return {bottom:turned?frame.right-r.left:r.bottom-frame.top,height:turned?frame.width:frame.height,below,
   messageBottom:turned?frame.right-messages.left:messages.bottom-frame.top,
   transport:document.querySelector('.playback').offsetHeight,hud:parseFloat(getComputedStyle(gap).getPropertyValue('--hud-h')),safe,
   rect:[r.left,r.top,r.right,r.bottom],frame:[frame.left,frame.top,frame.right,frame.bottom]};
 });
 const settle=()=>page.waitForFunction(()=>['.video-gap','.video-messages','.playback'].every(selector=>
  document.querySelector(selector).getAnimations().every(a=>a.playState==='finished')));
 await settle();const shown=await measure(),lift=Math.max(0,shown.transport-shown.below);
 assert(Math.abs(shown.height-shown.bottom-(lift+shown.hud+20))<1,'visible gap clears both control rows: '+JSON.stringify(shown));
 // Exercise the app's playback-state observer without a short fixture video ending during the assertion.
 await page.evaluate(()=>{document.body.classList.remove('hud-awake');document.getElementById('play').dataset.playing='true'});
 await page.waitForFunction(()=>document.body.classList.contains('hud-idle'));
 await settle();const hidden=await measure();
 assert(Math.abs(hidden.height-hidden.bottom-(12+hidden.safe))<1,
  'hidden fullscreen gap keeps only the outer safe-area margin: '+JSON.stringify(hidden));
 assert(Math.abs(hidden.bottom-shown.bottom-(lift+shown.hud+8-hidden.safe))<1,'gap moves through the transport and video controls');
 assert(Math.abs(hidden.height-hidden.messageBottom-(12+hidden.safe))<1,'messages share the freed bottom edge');
 assert(hidden.rect[0]>=hidden.frame[0]&&hidden.rect[1]>=hidden.frame[1]&&hidden.rect[2]<=hidden.frame[2]&&hidden.rect[3]<=hidden.frame[3],'gap remains within the screen');
 await page.evaluate(()=>{document.getElementById('play').dataset.playing='false'});
 await page.waitForFunction(()=>!document.body.classList.contains('hud-idle'));
 await settle();const restored=await measure();
 assert(Math.abs(restored.bottom-shown.bottom)<1,'showing controls restores the gap position');
 assert(Math.abs(restored.messageBottom-shown.messageBottom)<1,'showing controls restores messages');
}
// Full screen: the stage has the picture's shape (the video's own aspect), centred on the screen, and every control that
// floats on it but the transport bar (camera and overlay tools, title, speed and wheel, fullscreen button, messages, map)
// lies on the picture, not on the black bars beside or above it.
async function checkOnPicture(page,label){
 await page.waitForFunction(()=>document.getElementById('video').videoWidth>0);
 const g=await page.evaluate(()=>{
  const stage=document.querySelector('.camera-panel'),turned=stage.classList.contains('is-turned'),wrap=document.getElementById('videoStage');
  const frame=wrap.getBoundingClientRect(),panel=stage.getBoundingClientRect(),video=document.getElementById('video');
  const local=b=>turned?{left:b.top-frame.top,top:frame.right-b.right,right:b.bottom-frame.top,bottom:frame.right-b.left}:{left:b.left-frame.left,top:b.top-frame.top,right:b.right-frame.left,bottom:b.bottom-frame.top};
  const controls=['.video-tools','.video-hud','.video-title','.video-fullscreen','.video-map','#error','#warnings','#overlayStatus','#status']
   .flatMap(selector=>[...wrap.querySelectorAll(selector)]).filter(e=>e.checkVisibility()&&e.getBoundingClientRect().width>0)
   .map(e=>({name:e.id||e.className,...local(e.getBoundingClientRect())}));
  const size=turned?[frame.height,frame.width]:[frame.width,frame.height],screen=turned?[panel.height,panel.width]:[panel.width,panel.height];
  return {size,screen,ar:video.videoWidth/video.videoHeight,centre:turned?[frame.top+frame.height/2-panel.top-panel.height/2,frame.left+frame.width/2-panel.left-panel.width/2]:[frame.left+frame.width/2-panel.left-panel.width/2,frame.top+frame.height/2-panel.top-panel.height/2],controls};
 });
 assert(Math.abs(g.size[0]/g.size[1]-g.ar)<.01,`${label}: the stage has the picture's shape ${JSON.stringify(g)}`);
 assert(Math.abs(g.size[0]-g.screen[0])<1||Math.abs(g.size[1]-g.screen[1])<1,`${label}: the picture fills the screen one way ${JSON.stringify(g)}`);
 assert(g.centre.every(d=>Math.abs(d)<1),`${label}: the picture is centred ${JSON.stringify(g.centre)}`);
 assert(g.controls.some(c=>c.name.includes('video-tools'))&&g.controls.some(c=>c.name==='fullscreenButton'),`${label}: controls measured ${JSON.stringify(g.controls.map(c=>c.name))}`);
 for(const c of g.controls)assert(c.left>=-.5&&c.top>=-.5&&c.right<=g.size[0]+.5&&c.bottom<=g.size[1]+.5,`${label}: ${c.name} lies on the picture ${JSON.stringify(c)} in ${g.size}`);
}
(async()=>{
 const browser=await chromium.launch({headless:true});
 try{
  // Refused lock on a portrait phone: the frame is laid out 844 wide and turned to cover the 390x844 screen.
  const phone=await open(browser,{viewport:{width:390,height:844},hasTouch:true,isMobile:true},'reject');
  await phone.locator('#fullscreenButton').tap();
  await phone.waitForFunction(()=>document.querySelector('.camera-panel').classList.contains('is-turned'));
  let s=await state(phone);
  assert.deepEqual(s.lock,['landscape']);assert(s.full&&s.root,'full screen, turned');assert.equal(s.strip,'none','no segment strip in touch full screen');
  assert.deepEqual(s.frame,[0,0,390,844],'the turned frame covers the screen');
  assert.equal(s.bar,844,'the transport bar runs the screen height');assert(s.wrap[0]>s.wrap[1],'the video area is landscape');
  assert.equal(s.transport,s.barHeight+'px','the bar is measured by its own height, not its turned length');
  await checkOnPicture(phone,'turned phone');await checkIdleOverlays(phone);
  // Touch seeking follows the finger down the turned bar.
  const seek=await phone.locator('#seek').boundingBox();
  // Measured while the finger is down: on release the bar follows the video's actual (keyframe) time.
  const cdp=await phone.context().newCDPSession(phone),x=seek.x+seek.width/2,y=fraction=>seek.y+8+fraction*(seek.height-16);
  await cdp.send('Input.dispatchTouchEvent',{type:'touchStart',touchPoints:[{x,y:y(.25)}]});
  for(const fraction of [.25,.75]){
   if(fraction!==.25)await cdp.send('Input.dispatchTouchEvent',{type:'touchMove',touchPoints:[{x,y:y(fraction)}]});
   const value=await phone.evaluate(()=>{const seek=document.getElementById('seek');return Number(seek.value)/Number(seek.max)});
   assert(Math.abs(value-fraction)<.02,`touch at ${fraction} of the turned bar seeks to ${value}`);
  }
  await cdp.send('Input.dispatchTouchEvent',{type:'touchEnd',touchPoints:[]});
  // A dialog opened while turned is turned with it.
  assert.equal(await phone.evaluate(()=>{const d=document.createElement('dialog');document.body.append(d);d.showModal();const r=getComputedStyle(d).rotate;d.remove();return r}),'90deg');
  // (Turning the device itself cannot be emulated: headless Chromium does not resize a full screen window.)
  await phone.evaluate(()=>document.exitFullscreen());await phone.waitForFunction(()=>!document.fullscreenElement&&!document.documentElement.classList.contains('stage-turned'),null,{timeout:3000}).catch(()=>{});
  s=await state(phone);assert(!s.turned&&!s.root,'leaving full screen undoes the turn');assert.notEqual(s.strip,'none','the strip is back after full screen');assert.equal(s.unlock,0,'nothing to unlock after a refused lock');
  assert.deepEqual(phone.errors,[]);

  // Accepted lock: the browser turns the screen itself, so the page does not, and leaving full screen unlocks.
  const locked=await open(browser,{viewport:{width:390,height:844},hasTouch:true,isMobile:true},'resolve');
  await locked.locator('#fullscreenButton').tap();await locked.waitForFunction(()=>window.lockCalls.length&&document.fullscreenElement);
  await locked.waitForTimeout(100);s=await state(locked);assert(s.full&&!s.turned&&!s.root,'an accepted lock is not turned again');assert.equal(s.strip,'none','no segment strip in touch full screen');
  await locked.evaluate(()=>document.exitFullscreen());await locked.waitForFunction(()=>!document.fullscreenElement&&window.unlockCalls,null,{timeout:3000}).catch(()=>{});
  assert.equal((await state(locked)).unlock,1);assert.deepEqual(locked.errors,[]);

  // Landscape tablet, stacked: the picture is sized as if the app bar folds away (it does while playing), so it fills the
  // card's width, and coming back from full screen (system bars hide and return) gives the same size, not a smaller one.
  // Paused, with the app bar shown, the bottom controls over the video move up clear of the transport bar.
  const tab=await open(browser,{viewport:{width:1000,height:780},hasTouch:true,isMobile:true},'reject'),tabCdp=await tab.context().newCDPSession(tab);
  const metrics=height=>tabCdp.send('Emulation.setDeviceMetricsOverride',{width:1000,height,deviceScaleFactor:2,mobile:true});
  const sized=()=>tab.evaluate(()=>{const v=document.getElementById('videoStage').getBoundingClientRect(),p=document.querySelector('.camera-panel'),s=getComputedStyle(p),bar=document.querySelector('.playback').getBoundingClientRect();
   return {stacked:document.body.classList.contains('layout-stacked'),size:[Math.round(v.width),Math.round(v.height)],room:Math.round(p.clientWidth-parseFloat(s.paddingLeft)-parseFloat(s.paddingRight)),
    button:document.getElementById('fullscreenButton').getBoundingClientRect().bottom,tools:document.querySelector('.video-tools').getBoundingClientRect().bottom,bar:bar.top}});
  await tab.waitForTimeout(800);const first=await sized();
  assert(first.stacked,'the tablet layout is stacked');assert(Math.abs(first.size[0]-first.room)<=1,'the picture fills the card width: '+first.size[0]+' / '+first.room);
  assert(first.button<=first.bar&&first.tools<=first.bar,`bottom video controls clear the transport bar: ${first.button}, ${first.tools} / ${first.bar}`);
  await tab.evaluate(()=>document.getElementById('fullscreenButton').click());await tab.waitForFunction(()=>document.fullscreenElement);await metrics(840);await tab.waitForTimeout(200);
  await tab.evaluate(()=>document.exitFullscreen());await tab.waitForTimeout(100);await metrics(780);await tab.waitForTimeout(500);
  assert.deepEqual((await sized()).size,first.size,'the same size after full screen');assert.deepEqual(tab.errors,[]);

  // Desktop: no orientation lock, no turn.
  const desk=await open(browser,{viewport:{width:1280,height:800}},'reject');
  await desk.locator('#fullscreenButton').click();await desk.waitForFunction(()=>document.fullscreenElement);await desk.waitForTimeout(100);
  s=await state(desk);assert.deepEqual(s.lock,[]);assert(!s.turned);assert.notEqual(s.strip,'none','desktop full screen keeps the segment strip');assert.deepEqual(desk.errors,[]);
  await checkOnPicture(desk,'desktop');await checkIdleOverlays(desk);
  // A screen wider than the picture: black bars beside it, the controls stay on the picture.
  await desk.evaluate(()=>document.exitFullscreen());await desk.waitForFunction(()=>!document.fullscreenElement);
  await desk.setViewportSize({width:1600,height:700});await desk.locator('#fullscreenButton').click();await desk.waitForFunction(()=>document.fullscreenElement);
  await checkOnPicture(desk,'wide desktop');await checkIdleOverlays(desk);assert.deepEqual(desk.errors,[]);
  // Tall desktop viewport keeps letterboxing instead of the touch rotation fallback.
  const tall=await open(browser,{viewport:{width:800,height:1432}},'reject');
  await tall.locator('#fullscreenButton').click();await tall.waitForFunction(()=>document.fullscreenElement);
  await checkOnPicture(tall,'tall desktop');await checkIdleOverlays(tall);assert.deepEqual(tall.errors,[]);
  console.log('PASS: the full screen stage keeps the picture\'s shape with its controls on the picture; full screen on touch screens locks landscape, or turns the frame a quarter turn when refused (screen-filling frame, bar measured by its height, touch seek along the turn, turned dialogs, no segment strip, undone on exit); desktop unchanged');
 }finally{await browser.close()}
})().catch(e=>{console.error(e);process.exit(1)});
