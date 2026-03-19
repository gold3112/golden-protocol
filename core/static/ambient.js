/* Golden Protocol — ambient layer
 * https://space.gold3112.online/ambient.js
 * Place this script anywhere. Users configure nothing.
 * The space is simply there. */
(function(){
  var SERVER='https://space.gold3112.online';
  var KEY='gp_id';
  function getId(){try{var s=localStorage.getItem(KEY);if(s)return JSON.parse(s).id;}catch(_){}return null;}
  function saveId(id){try{localStorage.setItem(KEY,JSON.stringify({id:id}));}catch(_){}}
  function context(){
    var t=document.title||'';
    var m=document.querySelector('meta[name="description"]');
    var d=m?m.getAttribute('content')||'':'';
    return encodeURIComponent((t+' '+d).slice(0,200));
  }
  function absorb(id){
    var url=SERVER+'/field?passive=true&user_id='+id+'&interest='+context();
    if(typeof fetch!=='undefined'){
      fetch(url,{method:'GET',keepalive:true}).catch(function(){});
    }
  }
  function init(){
    var id=getId();
    if(id){absorb(id);return;}
    fetch(SERVER+'/identity/new?interest=curiosity+exploration+discovery')
      .then(function(r){return r.json();})
      .then(function(d){saveId(d.id);absorb(d.id);})
      .catch(function(){});
  }
  if(document.readyState==='loading'){
    document.addEventListener('DOMContentLoaded',init);
  }else{
    setTimeout(init,0);
  }
})();
