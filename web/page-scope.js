// Resources belong to one mounted page; old asynchronous work must never address the next page.
export function pageScope(root,url,navigate){
 const controller=new AbortController(),listeners=new Set(),tasks=new Map(),observers=new Set(),requests=new Set(),transitions=new Set(),cleanups=new Set(),suspends=new Set(),resumes=new Set();
 let phase='active',nextTask=0;
 const active=()=>phase==='active';
 const live=callback=>(...args)=>{if(active())return callback(...args)};
 const capture=options=>typeof options==='boolean'?options:!!options?.capture;
 const findListener=(target,type,callback,options)=>[...listeners].find(item=>item.target===target&&item.type===type&&item.callback===callback&&item.capture===capture(options));
 const unlisten=(target,type,callback,options)=>{
  const item=findListener(target,type,callback,options);if(!item)return;
  listeners.delete(item);target.removeEventListener(type,item.run,item.capture);
 };
 function listen(target,type,callback,options){
  if(!callback||controller.signal.aborted||options?.signal?.aborted||findListener(target,type,callback,options))return;
  const item={target,type,callback,capture:capture(options),run(event){
   if(!active())return;
   if(options?.once)unlisten(target,type,callback,options);
   if(typeof callback==='function')callback.call(target,event);else callback.handleEvent(event);
  }};
  listeners.add(item);
  target.addEventListener(type,item.run,{passive:options?.passive,capture:item.capture,signal:options?.signal?AbortSignal.any([controller.signal,options.signal]):controller.signal});
 }
 const scopedWindow=Object.create(null);
 const windowProxy=new Proxy(window,{
  get(target,key){
   if(key in scopedWindow)return scopedWindow[key];
   if(key==='addEventListener')return (...args)=>listen(window,...args);
   if(key==='removeEventListener')return (...args)=>unlisten(window,...args);
   if(key==='requestIdleCallback'||key==='cancelIdleCallback')return env[key];
   const value=Reflect.get(target,key,target);return typeof value==='function'?value.bind(target):value;
  },
  set(target,key,value){scopedWindow[key]=value;return true}
 });
 // Overflow locks still belong to the viewport, while classes and CSS variables belong to this page.
 const bodyStyle=new Proxy(root.style,{get(target,key){if(key==='overflow')return active()?document.body.style.overflow:'';const value=Reflect.get(target,key,target);return typeof value==='function'?value.bind(target):value},set(target,key,value){if(key==='overflow'){if(active())document.body.style.overflow=value}else target[key]=value;return true}});
 const body=new Proxy(root,{get(target,key){if(key==='style')return bodyStyle;const value=Reflect.get(target,key,target);return typeof value==='function'?value.bind(target):value}});
 const documentTarget=type=>type==='visibilitychange'||type.startsWith('key')?document:root;
 const doc=new Proxy(document,{
  get(target,key){
   if(key==='startViewTransition'&&target.startViewTransition)return update=>{const transition=target.startViewTransition(update);transitions.add(transition);transition.finished.then(()=>transitions.delete(transition),()=>transitions.delete(transition));return transition};
   if(key==='body')return body;
   if(key==='hidden')return !active()||target.hidden;
   if(key==='getElementById')return id=>root.querySelector('#'+CSS.escape(id));
   if(key==='querySelector'||key==='querySelectorAll')return root[key].bind(root);
   // Keys go to the real document: focus stays on the body (nothing keeps it), outside the page root.
   if(key==='addEventListener')return (type,...args)=>listen(documentTarget(type),type,...args);
   if(key==='removeEventListener')return (type,...args)=>unlisten(documentTarget(type),type,...args);
   const value=Reflect.get(target,key,target);return typeof value==='function'?value.bind(target):value;
  },
  set(target,key,value){if(String(key).startsWith('on'))root[key]=value;return true}
 });
 // Stable handles let pending work keep its place when the native timer or frame is paused.
 const schedule=(start,cancel,callback,repeat=false)=>{
  if(phase==='disposed')return 0;
  const id=++nextTask,task={start(){task.handle=start((...args)=>{if(!repeat)tasks.delete(id);if(active())callback(...args)})},stop(){cancel(task.handle)}};
  tasks.set(id,task);if(active())task.start();return id;
 };
 const cancel=id=>{tasks.get(id)?.stop();tasks.delete(id)};
 const observer=Type=>class extends Type{constructor(callback,...rest){super(live(callback),...rest);observers.add(this)}};
 const env={window:windowProxy,document:doc,location:{href:url.href,pathname:url.pathname,origin:url.origin,search:url.search,assign:to=>navigate(new URL(to,url)),replace:to=>navigate(new URL(to,url),{replace:true}),reload:()=>navigate(url,{replace:true})},
  fetch:(input,options={})=>fetch(new URL(input,url),{...options,signal:options.signal?AbortSignal.any([controller.signal,options.signal]):controller.signal}),
  setTimeout:(callback,delay,...args)=>schedule(run=>setTimeout(run,delay,...args),clearTimeout,callback),clearTimeout:cancel,
  setInterval:(callback,delay,...args)=>schedule(run=>setInterval(run,delay,...args),clearInterval,callback,true),clearInterval:cancel,
  requestAnimationFrame:callback=>schedule(requestAnimationFrame,cancelAnimationFrame,callback),cancelAnimationFrame:cancel,
  requestIdleCallback:window.requestIdleCallback&&((callback,options)=>schedule(run=>requestIdleCallback(run,options),cancelIdleCallback,callback)),cancelIdleCallback:cancel,
  ResizeObserver:observer(ResizeObserver),MutationObserver:observer(MutationObserver),IntersectionObserver:observer(IntersectionObserver),
  // A bare addEventListener() in a page script is the window's: left to the real one it outlived the page and kept the
  // whole page (its DOM, its log data) alive, one more with every log opened.
  addEventListener:(...args)=>listen(window,...args),removeEventListener:(...args)=>unlisten(window,...args),
  // What a page must undo itself when it goes (a Leaflet map: it listens on the window and keeps its container).
  onPageDispose:callback=>{cleanups.add(callback)},
  onPageSuspend:callback=>{suspends.add(callback)},onPageResume:callback=>{resumes.add(callback)},
  XMLHttpRequest:class extends XMLHttpRequest{constructor(){super();requests.add(this);this.addEventListener('loadend',()=>requests.delete(this))}open(method,path,...rest){super.open(method,new URL(path,url).href,...rest)}},
  matchMedia:query=>{const media=matchMedia(query);return new Proxy(media,{get(target,key){if(key==='addEventListener')return (...args)=>listen(target,...args);if(key==='removeEventListener')return (...args)=>unlisten(target,...args);const value=Reflect.get(target,key,target);return typeof value==='function'?value.bind(target):value}})}
 };
 const call=callbacks=>callbacks.forEach(callback=>{try{callback()}catch(error){console.error(error)}});
 const closeDialogs=()=>{for(const dialog of root.querySelectorAll('dialog[open]'))dialog.close()};
 return {env,suspend(){
  if(!active())return;
  phase='suspended';tasks.forEach(task=>task.stop());transitions.forEach(t=>t.skipTransition());
  call(suspends);closeDialogs();document.body.style.overflow='';
 },resume(){
  if(phase!=='suspended')return;
  phase='active';tasks.forEach(task=>task.start());call(resumes);
 },dispose(){
  phase='disposed';transitions.forEach(t=>t.skipTransition());controller.abort();listeners.clear();tasks.forEach(task=>task.stop());tasks.clear();observers.forEach(o=>o.disconnect());requests.forEach(r=>r.abort());
  call(cleanups);cleanups.clear();suspends.clear();resumes.clear();
  root.getAnimations({subtree:true}).forEach(a=>a.cancel());
  for(const v of root.querySelectorAll('video')){v.pause();v.removeAttribute('src');v.load()}
  closeDialogs();
  if(!root.hidden){document.body.style.overflow='';document.documentElement.classList.remove('rv-layout-transition')}
 }};
}
