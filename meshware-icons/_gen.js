const TEAL='#12857a', AMBER='#f0b254', OUT='meshware-icons';
const PAL = {
  tile:  {top:'#ffffff', left:'#a5d0cc', right:'#6cb3ad', seam:TEAL, led:AMBER},
  white: {top:'#ffffff', left:'#a5d0cc', right:'#6cb3ad', seam:'none', led:AMBER},
  teal:  {top:TEAL, left:'#0f6e65', right:'#0b524b', seam:'none', led:AMBER},
  mono:  {top:'currentColor', left:'currentColor', right:'currentColor', seam:'none', led:'currentColor', lo:.6, ro:.35},
};
const hex = dy => `50,${22+dy} 84.6,${42+dy} 84.6,${58+dy} 50,${78+dy} 15.4,${58+dy} 15.4,${42+dy}`;
function slab(p, wide){
  const stroke = p.seam==='none' ? '' : ` stroke="${p.seam}" stroke-width="${wide?3:2.5}" stroke-linejoin="round"`;
  const lo = p.lo!=null?` fill-opacity="${p.lo}"`:'', ro = p.ro!=null?` fill-opacity="${p.ro}"`:'';
  return `<g${stroke}><polygon points="50,22 84.6,42 50,62 15.4,42" fill="${p.top}"/><polygon points="15.4,42 50,62 50,78 15.4,58" fill="${p.left}"${lo}/><polygon points="50,62 84.6,42 84.6,58 50,78" fill="${p.right}"${ro}/></g><circle cx="24" cy="55" r="${wide?3.5:3}" fill="${p.led}"/>`;
}
function mark(kind, p, wide){
  const s = slab(p, wide);
  if (kind==='node') return `<g transform="translate(50 50) scale(${wide?1.3:1.15}) translate(-50 -50)">${s}</g>`;
  const S = wide?1.02:0.95;
  return `<g transform="translate(50 50) scale(${S}) translate(-50 -50)"><defs><mask id="mB" maskUnits="userSpaceOnUse" x="-20" y="-20" width="140" height="140"><rect x="-20" y="-20" width="140" height="140" fill="#fff"/><polygon points="${hex(0)}" fill="#000"/><polygon points="${hex(-20)}" fill="#000"/></mask><mask id="mM" maskUnits="userSpaceOnUse" x="-20" y="-20" width="140" height="140"><rect x="-20" y="-20" width="140" height="140" fill="#fff"/><polygon points="${hex(-20)}" fill="#000"/></mask></defs><g mask="url(#mB)"><g transform="translate(0 20)">${s}</g></g><g mask="url(#mM)">${s}</g><g transform="translate(0 -20)">${s}</g></g>`;
}
const NS='xmlns="http://www.w3.org/2000/svg"';
function tile(kind, rounded, wide){
  const frac = kind==='node' ? (wide?.86:.72) : (wide?.92:.76);
  const sz = Math.round(1024*frac), off = (1024-sz)/2;
  return `<svg ${NS} width="1024" height="1024" viewBox="0 0 1024 1024"><rect width="1024" height="1024" rx="${rounded?230:0}" fill="${TEAL}"/><svg x="${off}" y="${off}" width="${sz}" height="${sz}" viewBox="0 0 100 100">${mark(kind,PAL.tile,wide)}</svg></svg>`;
}
function markSvg(kind, pal, wide, color){
  const c = color ? ` color="${color}"` : '';
  return `<svg ${NS} width="512" height="512" viewBox="0 0 100 100"${c}>${mark(kind,PAL[pal],wide)}</svg>`;
}
function loadImg(src){ return new Promise((res,rej)=>{ const img=new Image(); img.onload=()=>res(img); img.onerror=()=>rej(new Error('svg load failed')); img.src=src; }); }
async function raster(svg, size){
  const img = await loadImg('data:image/svg+xml;charset=utf-8,'+encodeURIComponent(svg));
  const c = createCanvas(size,size); c.getContext('2d').drawImage(img,0,0,size,size); return c;
}
async function pngBlob(c){ return c.convertToBlob ? await c.convertToBlob({type:'image/png'}) : await new Promise(r=>c.toBlob(r,'image/png')); }
function ico(list){
  const n=list.length; let off=6+16*n; const parts=[];
  const h=new DataView(new ArrayBuffer(6)); h.setUint16(0,0,true); h.setUint16(2,1,true); h.setUint16(4,n,true); parts.push(h.buffer);
  for (const p of list){ const d=new DataView(new ArrayBuffer(16)); d.setUint8(0,p.size>=256?0:p.size); d.setUint8(1,p.size>=256?0:p.size); d.setUint16(4,1,true); d.setUint16(6,32,true); d.setUint32(8,p.buf.byteLength,true); d.setUint32(12,off,true); off+=p.buf.byteLength; parts.push(d.buffer); }
  for (const p of list) parts.push(p.buf);
  return new Blob(parts,{type:'image/x-icon'});
}
function icns(entries){
  let total=8; for (const e of entries) total+=8+e.buf.byteLength;
  const parts=[]; const h=new DataView(new ArrayBuffer(8)); h.setUint32(0,0x69636e73); h.setUint32(4,total); parts.push(h.buffer);
  for (const e of entries){ const d=new DataView(new ArrayBuffer(8)); for(let i=0;i<4;i++) d.setUint8(i,e.type.charCodeAt(i)); d.setUint32(4,8+e.buf.byteLength); parts.push(d.buffer,e.buf); }
  return new Blob(parts,{type:'image/icns'});
}

G = {TEAL,AMBER,OUT,PAL,tile,mark,markSvg,raster,pngBlob,ico,icns};
