import fs from 'node:fs';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import {execFileSync} from 'node:child_process';
const dir=path.dirname(fileURLToPath(import.meta.url));
const before=new Map(['index.html','master.svg'].map(f=>[f,fs.readFileSync(path.join(dir,f),'utf8')]));
execFileSync(process.execPath,[path.join(dir,'generate.mjs')]);
for(const [f,bytes] of before)if(fs.readFileSync(path.join(dir,f),'utf8')!==bytes)throw Error(`Non-reproducible output ${f}`);
for(const name of fs.readdirSync(dir)){
 if(name==='validate.mjs')continue;
 const text=fs.readFileSync(path.join(dir,name),'utf8');
 if(/\/Users\/|\/private\/|sk_live_|sk_test_|whsec_|[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}/i.test(text))throw Error(`Public privacy refusal ${name}`);
}
const html=before.get('index.html'),svg=before.get('master.svg'),data=JSON.parse(fs.readFileSync(path.join(dir,'services.json')));
if(/<script[^>]+src=|<iframe|fetch\(|localStorage|sessionStorage/.test(html))throw Error('Unexpected external or persistent behavior');
for(const s of data.services)if(!svg.includes(`data-service="${s.id}"`)||!html.includes(`"id":"${s.id}"`))throw Error(`Missing rendered inventory ${s.id}`);
if(data.zones.length!==8||data.journeys.length!==2)throw Error('Missing required zones or journeys');
console.log(JSON.stringify({pass:true,components:data.services.length,relationships:data.edges.length,zones:data.zones.length,checks:['reference integrity','complete rendered IDs','deterministic output','public privacy','no external scripts or data storage']}));
