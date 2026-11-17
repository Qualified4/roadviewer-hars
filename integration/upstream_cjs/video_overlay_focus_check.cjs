// HARS's own check for what reference/python/tests/video_overlay.cjs line 77 ('closing hands focus to nothing') guards:
// after 겹쳐 보기 설정 is closed with Escape, focus belongs to nothing (document.body). The same page, mocks and steps as
// the original (open the dialog from its button, press 초기화 so focus is inside the dialog, set a height, Escape, wait
// until the dialog is hidden), then also wait for the dialog's close event, which the app hands focus on from
// (video-overlay.js) and which arrives after the dialog is already hidden. Only then the original line 77's condition.
// The original waits for hidden only and so reads the focus too early now and then; see docs/hars/BROWSER-TESTS.md.
// run_browser_tests.py runs this from a mirror working folder whenever it runs video_overlay (expectations.json
// hars_checks); a failure fails the gate. CYCLES (argument, default 5) repeats the steps on one page.
const fs=require('fs'),assert=require('node:assert/strict'),{chromium}=require('playwright');
(async()=>{
 const cycles=Number(process.argv[2]||5),browser=await chromium.launch({headless:true});
 try{
  const page=await browser.newPage({viewport:{width:390,height:844},reducedMotion:'reduce',colorScheme:'dark'}),errors=[];
  page.on('pageerror',e=>errors.push(e.message));
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
  const dialog=page.locator('#overlayHeightDialog');
  const setHeight=async cm=>page.locator('#overlayHeightRange').evaluate((e,cm)=>{e.value=cm;e.dispatchEvent(new Event('input'))},cm);
  for(let i=0;i<cycles;i++){
   await page.locator('#overlayHeightSettings').click();
   assert(await dialog.isVisible());
   await page.locator('#overlayHeightReset').click();assert.equal(await page.locator('#overlayHeightRange').inputValue(),'60');
   await setHeight(75);
   assert.equal(await page.evaluate(()=>document.activeElement.id),'overlayHeightReset','focus is inside the dialog before Escape');
   await page.evaluate(()=>{window.hcClosed=false;document.getElementById('overlayHeightDialog').addEventListener('close',()=>{window.hcClosed=true},{once:true})});
   await page.keyboard.press('Escape');await dialog.waitFor({state:'hidden'});
   await page.waitForFunction(()=>window.hcClosed);
   const active=await page.evaluate(()=>document.activeElement.id||document.activeElement.tagName);
   assert(await page.evaluate(()=>document.activeElement===document.body),'closing hands focus to nothing');
   console.log(`cycle ${i+1}/${cycles}: close event ran, focus ${active}`);
  }
  assert.deepEqual(errors,[]);
  console.log(`PASS: video_overlay focus after Escape (HARS check, ${cycles} cycles)`);
 }finally{await browser.close()}
})().catch(e=>{console.error(e);process.exitCode=1});
