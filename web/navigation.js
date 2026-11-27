const host=document.getElementById('pageHost'),notice=document.getElementById('navigationStatus'),assets=new URL('./',import.meta.url),version=new URL(import.meta.url).search;
// With this file's version, like the page modules: imported by its bare name it came from the browser's cache for hours
// after an update, an older page scope under newer page scripts.
const {pageScope}=await import(new URL('page-scope.js'+version,assets));
const base=new URL('../',assets),libraryStyle=document.getElementById('libraryStyle');
let current=null,libraryPage=null,revision=0,pending=null,backPress=0;
// Both fragments are static. Keep the parsed templates and resolve their links against each destination on mount.
const shells=new Map();
let libraryRequest;
async function loadShell(url,kind,signal){
 const request=new URL(url);request.searchParams.set('fragment','1');
 const response=await fetch(request,{signal,cache:'no-store'});
 if(!response.ok)throw Error('화면을 불러오지 못했습니다. 다시 시도해 주세요.');
 const html=new DOMParser().parseFromString(await response.text(),'text/html');
 if(!html.querySelector('main')||!html.body.classList.contains(kind+'-page'))throw Error('예상하지 못한 응답입니다. 연결 상태를 확인해 주세요.');
 html.querySelectorAll('script').forEach(s=>s.remove());shells.set(kind,html);return html;
}
async function shell(url,kind,signal){
 const cached=shells.get(kind);if(cached)return cached;
 if(kind==='library')return libraryRequest??=loadShell(url,kind).finally(()=>{libraryRequest=null});
 return loadShell(url,kind,signal);
}
function prefetchLibrary(){const idle=window.requestIdleCallback||(f=>setTimeout(f,800));idle(()=>{shell(base,'library').catch(()=>{});import(new URL('page-library.js'+version,assets)).catch(()=>{})})}
// A slim progress line shows that a tap started a page change when it takes more than a moment.
let busyTimer=0;
function busy(on){clearTimeout(busyTimer);if(on)busyTimer=setTimeout(()=>document.documentElement.classList.add('rv-navigating'),120);else document.documentElement.classList.remove('rv-navigating')}
// Library tabs live in the hash (#upload, #storage …): a hash-only change stays on the mounted page.
const samePage=url=>!!current&&url.origin===current.url.origin&&url.pathname===current.url.pathname&&url.search===current.url.search;
const route=url=>url.origin===base.origin&&(url.pathname===base.pathname?'library':new RegExp('^'+base.pathname.replace(/[.*+?^${}()|[\]\\]/g,'\\$&')+'view/[^/]+/$').test(url.pathname)?'replay':null);
function saveScroll(){history.replaceState({...history.state,rvScroll:scrollY},'',location.href)}
// A tap on a link that shows its press ([data-ripple-now], [data-glow]: back and the list's 재생) keeps the page this long, so the wave is seen before the swap.
const PRESS_HOLD=200;
async function prepare(url,kind,signal,ticket,previous){
 const [html,module]=await Promise.all([shell(url,kind,signal),import(new URL('page-'+kind+'.js'+version,assets))]);
 if(ticket!==revision)return;
 const root=document.createElement('div');root.className=html.body.className;
 root.append(...html.body.cloneNode(true).childNodes);
 for(const element of root.querySelectorAll('[href],[src]'))for(const attr of ['href','src'])if(element.hasAttribute(attr))element.setAttribute(attr,new URL(element.getAttribute(attr),url).href);
 root.hidden=true;host.append(root);
 const scope=pageScope(root,url,navigate);
 try{
  const api=module.mount(scope.env);
  if(previous){let timer;try{await Promise.race([api.ready,new Promise(resolve=>timer=setTimeout(resolve,400))])}finally{clearTimeout(timer)}}
  if(ticket===revision)return {url,kind,root,scope,api,title:html.title};
 }catch(error){scope.dispose();root.remove();throw error}
 scope.dispose();root.remove();
}
async function navigate(to,{replace=false,pop=false,pressed=0}={}){
 const url=new URL(to,location.href),kind=route(url);if(!kind)return;
 if(current&&!current.api.canLeave()){
  notice.textContent='업로드 또는 정리 작업이 끝난 뒤 이동해 주세요.';
  if(pop)history.pushState(current.state,'',current.url);return;
 }
 const ticket=++revision;pending?.abort();
 pending=new AbortController();notice.setAttribute('role','status');notice.textContent='';
 const previous=current,retained=kind==='library'&&previous!==libraryPage&&libraryPage?.url.search===url.search;
 try{
  if(!retained)busy(true);
  const page=retained?libraryPage:await prepare(url,kind,pending.signal,ticket,previous);
  if(!page)return;
  const hold=!retained&&pressed&&!matchMedia('(prefers-reduced-motion: reduce)').matches?PRESS_HOLD-(performance.now()-pressed):0;
  if(hold>0)await new Promise(resolve=>setTimeout(resolve,hold));
  if(ticket!==revision){page.scope.dispose();page.root.remove();return}
  if(previous?.kind==='library')previous.scroll=scrollY;
  if(!pop){
   let state=previous?{}:history.state;
   if(previous&&kind==='replay'){
    // A segment replacement keeps its predecessor. Different drives keep their own replay entries.
    const back=history.state?.rvLibraryBack||0;
    const distance=previous.kind==='library'?1:back&&back+1;
    state={rvLibraryBack:replace?back:distance};
   }
   if(!replace)saveScroll();
   history[replace?'replaceState':'pushState'](state,'',url);
  }
  globalThis.__rvNavNext=kind;
  try{
   if(previous?.kind==='library'&&kind==='replay'){
    previous.scope.suspend();previous.root.hidden=true;
   }else if(previous){previous.scope.dispose();previous.root.remove()}
   if(kind==='library'){
    if(libraryPage&&libraryPage!==previous&&libraryPage!==page){libraryPage.scope.dispose();libraryPage.root.remove()}
    libraryPage=page;
   }
  }finally{globalThis.__rvNavNext=null}
  document.getElementById('pageBase').href=url.href;
  if(host.firstElementChild!==page.root)host.prepend(page.root);
  libraryStyle.disabled=kind!=='library';document.documentElement.dataset.page=kind;
  current=page;page.url=url;page.state=history.state;page.root.hidden=false;
  document.title=page.title;notice.textContent='';
  const scroll=(pop?history.state?.rvScroll:null)??page.scroll??0;
  if(retained)page.scope.resume();
  scrollTo({left:0,top:scroll,behavior:'instant'});
  dispatchEvent(new Event('pageshow'));busy(false);
  if(kind==='replay'&&!libraryPage)prefetchLibrary();

 }catch(error){
  if(ticket!==revision||error.name==='AbortError')return;
  busy(false);
  if(pop&&current)history.pushState(current.state,'',current.url);
  notice.setAttribute('role','alert');notice.textContent=error.message||'화면을 불러오지 못했습니다.';
  const retry=document.createElement('button');retry.type='button';retry.textContent='다시 시도';retry.onclick=()=>navigate(url,{replace});notice.append(' ',retry);
 }
}
document.addEventListener('click',event=>{
 const link=event.target.closest('a[href]');if(!link||event.defaultPrevented||event.button!==0||event.ctrlKey||event.metaKey||event.shiftKey||event.altKey||link.target||link.hasAttribute('download'))return;
 const url=new URL(link.href);if(!route(url)||(url.hash&&samePage(url)))return;event.preventDefault();
 const pressed=link.matches('[data-ripple-now],[data-glow]')?performance.now():0;
 if(link.matches('.replay-page .back')){
  if(backPress||!samePage(new URL(location.href)))return;
  const back=history.state?.rvLibraryBack;
  if(back){saveScroll();backPress=pressed;if(back===1)history.back();else history.go(-back);return}
  void navigate(url,{replace:true,pressed});return;
 }
 void navigate(url,{pressed});
});
addEventListener('popstate',()=>{const pressed=backPress;backPress=0;if(samePage(new URL(location.href)))return;void navigate(location.href,{pop:true,pressed})});
history.scrollRestoration='manual';
void navigate(location.href,{replace:true});
