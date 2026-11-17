// Probe for video_overlay.cjs line 77 ('closing hands focus to nothing'), outside the gate: the same page, mocks and
// steps (open 겹쳐 보기 설정, press 초기화, set a height, Escape, wait until the dialog is hidden, read the focus), repeated
// CYCLES times, with an in-page record of when the dialog's close event ran. Run from a mirror's working folder:
//   NODE_PATH=integration/upstream_cjs/node_modules RV_TEST_VIDEO=... node video_overlay_focus.cjs [CYCLES]
// Prints one JSON line per cycle and a summary.
const fs=require('fs'),{chromium}=require('playwright');
(async()=>{
 const cycles=Number(process.argv[2]||30),browser=await chromium.launch({headless:true});
 try{
  const page=await browser.newPage({viewport:{width:390,height:844},reducedMotion:'reduce',colorScheme:'dark'});
  await page.route('https://rv.test/**',route=>{
   const p=new URL(route.request().url()).pathname;
   if(p.endsWith('/bookmarks'))return route.fulfill({json:{bookmarks:[]}});
   if(p==='/api/logs')return route.fulfill({json:{logs:[]}});
   if(p.endsWith('/video'))return route.fulfill({body:fs.readFileSync(process.env.RV_TEST_VIDEO),contentType:'video/mp4'});
   if(p.endsWith('/telemetry'))return route.fulfill({status:404,json:{error:'차량 정보 없음'}});
   if(p.endsWith('/data'))return route.fulfill({json:{route:'test',key:'overlay',duration:2,warnings:[],video:{start:0,duration:2},frames:[{t:0,id:0,valid:true,lanes:[],edges:[],lp:[],es:[],leads:[],liveTracksValid:false,overlay:{lanes:[],edges:[],path:[[.5,.6],[.5,.9]],markers:[]}}]}});
   const name=p.startsWith('/view/')?'index.html':p.replace('/assets/','');
   return route.fulfill({body:fs.readFileSync('roadviewer/app/web/'+name),contentType:name.endsWith('.js')?'application/javascript':name.endsWith('.css')?'text/css':name.endsWith('.png')?'image/png':'image/svg+xml'});
  });
  await page.route('https://rv.test/view/one/',route=>route.fulfill({body:fs.readFileSync('roadviewer/app/web/index.html'),contentType:'text/html'}));
  await page.goto('https://rv.test/view/one/');
  await page.waitForFunction(()=>!document.getElementById('play').disabled);
  await page.evaluate(()=>{
   const dialog=document.getElementById('overlayHeightDialog'),t0=()=>performance.now().toFixed(1);
   window.probe=[];
   dialog.addEventListener('close',()=>probe.push(['close',t0(),document.activeElement.id||document.activeElement.tagName]));
   document.addEventListener('keydown',e=>probe.push(['keydown '+e.key,t0(),document.activeElement.id||document.activeElement.tagName]),true);
   document.addEventListener('focusin',e=>probe.push(['focusin',t0(),e.target.id||e.target.tagName]),true);
  });
  const dialog=page.locator('#overlayHeightDialog'),results=[];
  for(let i=0;i<cycles;i++){
   await page.locator('#overlayHeightSettings').click();
   await page.locator('#overlayHeightReset').click();
   await page.locator('#overlayHeightRange').evaluate(e=>{e.value=75;e.dispatchEvent(new Event('input'))});
   await page.evaluate(()=>{probe.length=0});
   await page.keyboard.press('Escape');await dialog.waitFor({state:'hidden'});
   const seen=await page.evaluate(()=>({active:document.activeElement.id||document.activeElement.tagName,
    closeRan:probe.some(e=>e[0]==='close'),at:performance.now().toFixed(1),events:probe.slice()}));
   seen.pass=seen.active==='BODY';results.push(seen);console.log(JSON.stringify({cycle:i,...seen}));
   await page.waitForFunction(()=>probe.some(e=>e[0]==='close'));
  }
  const fails=results.filter(r=>!r.pass);
  console.log('SUMMARY '+JSON.stringify({cycles,fail:fails.length,
   failWithCloseNotRun:fails.filter(r=>!r.closeRan).length,passWithCloseNotRun:results.filter(r=>r.pass&&!r.closeRan).length,
   activeWhenFailing:[...new Set(fails.map(r=>r.active))]}));
 }finally{await browser.close()}
})().catch(e=>{console.error(e);process.exitCode=1});
