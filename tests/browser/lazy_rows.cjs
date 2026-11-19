// Folded drive cards show at once; their segment rows are built afterwards in idle-time slices. A card opened first builds
// its own rows before the fold measures them, selection mode builds every row, and filters/counts work from the data.
const fs=require('fs'),assert=require('node:assert/strict'),{chromium}=require('playwright');
const {settleLayout}=require('./browser-helpers.cjs');
(async()=>{
 const browser=await chromium.launch({headless:true});
 try{
  const context=await browser.newContext({viewport:{width:1280,height:900}});
  // Idle callbacks wait in a queue the test runs by hand (deadline: no idle time left, so each slice is ~2 ms).
  await context.addInitScript(()=>{window.__idle=[];window.requestIdleCallback=callback=>{window.__idle.push(callback);return window.__idle.length};window.cancelIdleCallback=()=>{}});
  let tagged=false,unchanged=0;
  const page=await context.newPage(),errors=[];page.on('pageerror',e=>errors.push(e.message));
  const drives=['aaaaaaaaaa','bbbbbbbbbb','cccccccccc'];
  const logs=drives.flatMap((hash,d)=>Array.from({length:40},(_,i)=>({id:`${d}-${i}`,name:`0000039${d}--${hash}--${39-i}`,status:'ready',bytes:10,prepared_bytes:20,uploaded:1700000000-d*3600+i,video:true,tags:i===7?[{name:'찾기',color:'blue'}]:[]})));
  await page.route('**/*',route=>{
   const p=new URL(route.request().url()).pathname;
   if(p==='/api/logs'){
    // tagged: the list carries an ETag and an unchanged one is answered 304, as HARS does.
    if(tagged&&route.request().headers()['if-none-match']==='"long"'){unchanged++;return route.fulfill({status:304})}
    return route.fulfill({json:{logs,concurrency:1,auto_convert:false,max_upload_mb:512,storage_used_bytes:0},headers:tagged?{ETag:'"long"'}:{}});
   }
   if(p==='/api/progress')return route.fulfill({json:{progress:{}}});
   if(p.startsWith('/api/'))return route.fulfill({status:404,json:{error:'not_found'}});
   const name=p==='/'?'library.html':p.replace('/assets/',''),file='roadviewer/app/web/'+name;
   if(!fs.existsSync(file))return route.abort();
   return route.fulfill({body:fs.readFileSync(file),contentType:name.endsWith('.js')?'application/javascript':name.endsWith('.css')?'text/css':name.endsWith('.html')?'text/html':name.endsWith('.svg')?'image/svg+xml':name.endsWith('.woff2')?'font/woff2':'application/octet-stream'});
  });
  const rowCount=(selector='#list')=>page.evaluate(selector=>document.querySelectorAll(selector+' .log-row').length,selector);
  const card=i=>`.log-route[data-route="0000039${i}--${drives[i]}"]`;
  const slice=()=>page.evaluate(()=>{const callback=window.__idle.shift();if(!callback)return false;callback({didTimeout:false,timeRemaining:()=>0});return true});
  const load=async()=>{await page.goto('https://rv.test/');await page.waitForFunction(()=>document.querySelectorAll('.log-route').length===3)};

  // 1. Cards first: summaries are complete, no row exists yet, and a poll with the same list builds none.
  await load();
  assert.equal(await rowCount(),0,'no segment row is built with the cards');
  assert.deepEqual(await page.locator('.log-route-count').allTextContents(),['40개 구간','40개 구간','40개 구간']);
  assert.equal(await page.locator(card(0)+' .route-bar .route-seg').count(),20,'the card bar is drawn from the data');
  await page.evaluate(()=>refresh());assert.equal(await rowCount(),0,'polling leaves rows of folded cards queued');
  // Search and tag counts work from the data, without rows.
  await page.locator('#logSearch').fill('찾기');
  assert.equal(await page.locator(card(1)+' .log-route-count').textContent(),'1 / 40개 구간');
  assert.equal(await page.locator('#tagFilterStatus').textContent(),'3개 로그 표시 중');
  assert.equal(await rowCount(),0);
  await page.locator('#logSearch').fill('');

  // 2. Opening a card before its rows exist: its rows are built in segment order before the fold starts, and it folds open
  // with the normal motion (is-folding, height animating from the closed card), as rows not new (no entrance).
  const opened=await page.evaluate(selector=>{
   const element=document.querySelector(selector),from=element.getBoundingClientRect().height;element.querySelector('.log-route-chevron').click();
   const rows=[...element.querySelectorAll('.log-route-segments>.log-row')];
   return {from,rows:rows.length,order:rows.map(row=>row.querySelector('.recording-name-text')?.textContent||row.textContent).slice(0,3),update:rows.every(row=>row.classList.contains('is-update')),folding:element.classList.contains('is-folding'),open:element.open,animations:element.getAnimations().length};
  },card(1));
  assert.equal(opened.rows,40,'the opened card has all its rows');assert(opened.update,'unfolded rows skip the new-row entrance');
  assert(opened.folding&&opened.open&&opened.animations>0,'the card unfolds with the fold animation: '+JSON.stringify(opened));
  await settleLayout(page,'#panel-logs');
  const body=await page.locator(card(1)+'>.log-route-segments').boundingBox();assert(body.height>40*40,'the unfolded body holds the rows: '+body.height);
  const segments=await page.locator(card(1)+' .log-row').evaluateAll(rows=>rows.map(row=>row.dataset.id));
  assert.deepEqual(segments,Array.from({length:40},(_,i)=>`1-${39-i}`),'segments ascending (segment 0 first)');
  assert.equal(await rowCount(),40,'other cards are still waiting');

  // 3. The rest fills in the background in several short slices, a whole card at a time.
  let slices=0,partial=false;
  while(await slice()){slices++;const n=await rowCount();if(n%40)partial=true}
  assert(slices>=3,'the background build is split into slices: '+slices);assert(!partial,'a folded card shows its rows all at once');
  assert.equal(await rowCount(),120,'every row exists after the background build');
  assert.equal(await page.locator(card(0)+' .log-row').first().getAttribute('data-id'),'0-39');
  // A code-set [open] (no click) still finds its rows before the next frame.
  await load();
  const programmatic=await page.evaluate(async selector=>{const element=document.querySelector(selector);element.open=true;await Promise.resolve();return element.querySelectorAll('.log-row').length},card(2));
  assert.equal(programmatic,40,'a card opened by code builds its rows');

  // 4. Selection mode works on every row, built or not.
  await load();assert.equal(await rowCount(),0);
  await page.locator('#bulkOpen').click();await page.locator('#bulkBar').waitFor();
  assert.equal(await page.locator('.segment-select input').count(),120,'selection mode sees every row');
  await page.locator('#bulkAll').check();assert.equal(await page.locator('#bulkCount').textContent(),'120개 구간 선택');
  await page.locator('#bulkClose').click();

  // 5. A long list: the background fills the cards that start within the first 120 rows; a card further down gets its
  // rows when it opens. The 3-second poll of an unchanged list (304) leaves the page as it is; a refresh goes through it.
  drives.push('dddddddddd','eeeeeeeeee');tagged=true;
  for(const d of [3,4])logs.push(...Array.from({length:40},(_,i)=>({id:`${d}-${i}`,name:`0000039${d}--${drives[d]}--${39-i}`,status:'ready',bytes:10,prepared_bytes:20,uploaded:1700000000-d*3600+i,video:true,tags:[]})));
  await page.goto('https://rv.test/');await page.waitForFunction(()=>document.querySelectorAll('.log-route').length===5);
  while(await slice());
  assert.equal(await rowCount(),120,'the background stops after the cards near the top');
  assert.equal(await rowCount(card(3)),0);assert.equal(await rowCount(card(4)),0);
  assert.equal(await page.locator(card(4)+' .log-route-count').textContent(),'40개 구간','a card without rows still counts its segments');
  await page.locator(card(4)+'>summary').click();await page.waitForFunction(selector=>document.querySelectorAll(selector+' .log-row').length===40,card(4));
  assert.equal(await rowCount(card(3)),0,'opening one card builds only its own rows');
  const count=page.locator(card(0)+' .log-route-count');
  await count.evaluate(element=>{element.textContent='표시'});
  await page.evaluate(()=>refresh({poll:true}));assert.equal(unchanged,1);assert.equal(await count.textContent(),'표시','a poll of an unchanged list does not go through the list again');
  await page.evaluate(()=>refresh());assert.equal(unchanged,2);assert.equal(await count.textContent(),'40개 구간','a refresh does');

  assert.deepEqual(errors,[]);
  console.log('lazy rows: cards before rows, background slices, open-before-built, selection mode, long list, unchanged poll ok');
 }finally{await browser.close()}
})().catch(e=>{console.error(e);process.exit(1)});
