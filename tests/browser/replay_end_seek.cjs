const fs=require('fs'),assert=require('node:assert/strict'),{chromium}=require('playwright');
const {spawn}=require('node:child_process'),{once}=require('node:events');
const os=require('node:os'),path=require('node:path'),net=require('node:net');

async function availablePort(){
 const server=net.createServer().listen(0,'127.0.0.1');
 await once(server,'listening');
 const port=server.address().port;await new Promise(resolve=>server.close(resolve));return port;
}

async function nasPlayback(browser,data){
 const temp=fs.mkdtempSync(path.join(os.tmpdir(),'rv-prepared-'));
 const root=path.join(temp,'data'),share=path.join(temp,'share'),id='a'.repeat(32);
 const folder=path.join(root,id),prepared=path.join(folder,'prepared');
 fs.mkdirSync(prepared,{recursive:true});fs.mkdirSync(share);
 fs.writeFileSync(path.join(folder,'meta.json'),JSON.stringify({id,name:'NAS seek fixture',status:'ready',uploaded:1}));
 fs.writeFileSync(path.join(prepared,'summary.json'),'{}');
 fs.writeFileSync(path.join(prepared,'data.json'),JSON.stringify(data));
 fs.copyFileSync(process.env.RV_TEST_VIDEO,path.join(prepared,'camera.mp4'));
 const port=await availablePort(),base='http://127.0.0.1:'+port;
 const server=spawn(process.env.HARS_BIN,[],{env:{...process.env,HARS_DATA:root,HARS_BIND:'127.0.0.1:'+port,HARS_DEVICE_BIND:'127.0.0.1:0',HARS_INGRESS_ONLY:'0',RV_ARCHIVE_ALLOW_ANY_PATH:'1',RUST_LOG:'warn'},stdio:'inherit'});
 const exit=once(server,'exit');
 const until=async check=>{
  const deadline=Date.now()+10000;
  while(Date.now()<deadline){
   if(await check())return;
   if(server.exitCode!==null)throw Error('HARS exited before the NAS test finished');
   await new Promise(resolve=>setTimeout(resolve,40));
  }
  throw Error('HARS NAS fixture did not settle');
 };
 const request=suffix=>fetch(base+suffix);
 try{
  await until(async()=>{try{return (await request('/health')).ok}catch{return false}});
  const replay=await browser.newPage();
  await replay.goto(base+'/#storage');
  await replay.locator('#preparedLocation:enabled').waitFor();
  await replay.getByLabel('저장 위치',{exact:true}).selectOption('nas');
  await replay.getByLabel('NAS 공유 폴더',{exact:true}).fill(share);
  await replay.getByRole('button',{name:'재생 저장소 저장'}).click();
  await replay.getByText('NAS 연결됨',{exact:true}).waitFor();
  replay.once('dialog',dialog=>dialog.accept());
  await replay.getByRole('button',{name:'기존 재생 데이터 NAS로 이동'}).click();
  await replay.waitForFunction(()=>document.getElementById('preparedMoveStatus')?.textContent.includes('완료'));
  const result=await (await request('/api/settings/prepared/move')).json();
  assert.equal(result.moved,1,JSON.stringify(result));assert(!fs.existsSync(prepared));
  assert.equal(JSON.parse(fs.readFileSync(path.join(folder,'meta.json'))).prepared_nas.share,share);
  const ranges=[];
  replay.on('response',response=>{if(new URL(response.url()).pathname.endsWith('/video')&&response.status()===206)ranges.push(response.headers()['content-range'])});
  await replay.goto(base+'/view/'+id+'/');
  await replay.waitForFunction(()=>document.querySelector('#play')&&!document.querySelector('#play').disabled);
  await replay.locator('#play').click();
  await replay.waitForFunction(()=>document.querySelector('video').currentTime>.1);
  await replay.locator('#play').click();
  await replay.locator('#seek').evaluate(el=>{el.value='1.25';el.dispatchEvent(new Event('input',{bubbles:true}))});
  await replay.waitForFunction(()=>Math.abs(document.querySelector('video').currentTime-1.25)<.15);
  assert(ranges.some(range=>/^bytes \d+-\d+\/\d+$/.test(range)),'NAS MP4 playback served byte ranges');
  await replay.close();
  console.log('PASS: real HARS playback and seeking with a verified NAS generation on a temporary share');
 }finally{
  server.kill('SIGKILL');await exit;
  fs.rmSync(temp,{recursive:true,force:true});
 }
}
(async()=>{
 const browser=await chromium.launch({headless:true});
 try{
  const page=await browser.newPage(),errors=[];page.on('pageerror',e=>errors.push(e.message));
  const data={route:'play request test',key:'play-request',duration:2.15,warnings:[],video:{start:0,duration:2,frames:20},frames:Array.from({length:44},(_,i)=>({t:i/20,id:i,valid:true,lanes:[],edges:[],lp:[],es:[],leads:[]}))};
  await page.route('https://rv.test/**',route=>{
   const p=new URL(route.request().url()).pathname;
   if(p.endsWith('/bookmarks'))return route.fulfill({json:{bookmarks:[]}});
   if(p==='/api/logs')return route.fulfill({json:{logs:[]}});
   // The replay loads vehicle data in the background for the event track; none here.
   if(p.endsWith('/telemetry'))return route.fulfill({status:404,json:{error:'차량 정보 없음'}});
   if(p.endsWith('/data'))return route.fulfill({json:data});
   if(p.endsWith('/video'))return route.fulfill({body:fs.readFileSync((process.env.RV_TEST_VIDEO||'/tmp/roadviewer-test.mp4')),contentType:'video/mp4'});
   const name=p.startsWith('/view/')?'index.html':p.replace('/assets/','');
   return route.fulfill({body:fs.readFileSync('roadviewer/app/web/'+name),contentType:name.endsWith('.js')?'application/javascript':name.endsWith('.css')?'text/css':name.endsWith('.html')?'text/html':name.endsWith('.png')?'image/png':'image/svg+xml'});
  });
  await page.goto('https://rv.test/view/test/');await page.waitForFunction(()=>!document.getElementById('play').disabled);
  const result=await page.evaluate(async()=>{
   pause();const oldVideo=data.video,oldDuration=data.duration,raf=window.requestAnimationFrame;window.requestAnimationFrame=()=>0;
   let mediaTime=2,seeking=false,ended=true,paused=true,ready=2,restarts=0,plays=0,writes=0;
   Object.defineProperties(v,{
    duration:{configurable:true,get:()=>2},currentTime:{configurable:true,get:()=>mediaTime,set:value=>{mediaTime=value>=1.999?2:value;seeking=true;ready=2;ended=mediaTime>=2;writes++}},
    seeking:{configurable:true,get:()=>seeking},ended:{configurable:true,get:()=>ended},readyState:{configurable:true,get:()=>ready},paused:{configurable:true,get:()=>paused},
    pause:{configurable:true,value:()=>{paused=true}},play:{configurable:true,value:()=>{plays++;if(ended){restarts++;mediaTime=0;ended=false}paused=false;return Promise.resolve()}}
   });
   const settle=async()=>{seeking=false;v.dispatchEvent(new Event('seeked'));await Promise.resolve();await Promise.resolve();await Promise.resolve()};
   try{
    data.duration=2;data.video={start:0,duration:2.02,frames:20};playing=true;seekPointer=7;
    setTime(2);await settle();window.dispatchEvent(new PointerEvent('pointerup',{pointerId:7}));
    for(let i=0;i<5;i++){await settle();tick(last+16)}
    const end={restarts,plays,playing,paused,time:mediaTime,t,writes};
    data.duration=2.15;data.video={start:0,duration:2,frames:20};seekPointer=7;playing=true;mediaTime=2;ended=true;seeking=false;
    const writesBefore=writes;setTime(2.15);for(let i=0;i<5;i++)await settle();const repeatedEndSeeks=writes-writesBefore;
    seekPointer=null;
    // Ended media without future frames must not freeze the log clock while waiting for readyState 3.
    data.duration=2.4;data.video={start:0,duration:2.05,frames:20};playing=true;t=1.99;pendingVideoSeek=null;seeking=false;ended=true;mediaTime=2;ready=2;
    tick(last+100);const shortVideo={t,restarts};
    // A pending backward seek must settle before playback resumes.
    seekPointer=null;playing=true;setTime(.5);const before=plays;syncVideo();const seekingPlays=plays-before;ready=4;await settle();const resumed=plays>before;
    return {end,shortVideo,seekingPlays,resumed,repeatedEndSeeks};
   }finally{
    for(const key of ['duration','currentTime','seeking','ended','readyState','paused','play','pause'])delete v[key];
    data.video=oldVideo;data.duration=oldDuration;window.requestAnimationFrame=raf;pendingVideoSeek=null;seekPointer=null;pause();
   }
  });
  assert.equal(result.end.restarts,0,'end seek must never restart the video at frame zero');
  assert(!result.end.playing&&result.end.paused,'drag release at log end stops playback');
  assert.equal(result.repeatedEndSeeks,0,'rounded final frame must not trigger endless seeked requests');assert.equal(result.end.time,2);assert.equal(result.end.t,2);
  assert(result.shortVideo.t>1.99,'ended video cannot stall the log clock');
  assert.equal(result.shortVideo.restarts,0);assert.equal(result.seekingPlays,0);assert(result.resumed,'backward seek resumes only after seeked');
  assert.deepEqual(errors,[]);await page.close();await nasPlayback(browser,data);console.log('PASS: end seek with duration rounding, delayed seeked, ended media and backward resume');
 }finally{await browser.close()}
})().catch(e=>{console.error(e);process.exitCode=1});
