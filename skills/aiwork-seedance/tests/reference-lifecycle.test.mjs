import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {once} from 'node:events';
import {createServer} from 'node:http';
import {mkdtemp,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import path from 'node:path';
import {fileURLToPath} from 'node:url';
import test from 'node:test';

const runner=process.env.AIWORK_SEEDANCE_RUNNER||fileURLToPath(new URL('../scripts/aiwork-seedance.ps1',import.meta.url));
const quote=value=>`'${String(value).replaceAll("'","''")}'`;
const png=Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl6nRAAAAAASUVORK5CYII=','base64');

async function fixture(){
 const root=await mkdtemp(path.join(tmpdir(),'aiwork-reference-test-'));
 const file=path.join(root,"人物 it's.png");await writeFile(file,png);
 const uploads=[],submissions=[];let expired=new Set(),drop=false,dropUpload=false,reject=null,delay=0;
 const server=createServer(async(req,res)=>{
  const chunks=[];for await(const chunk of req)chunks.push(chunk);
  const body=chunks.length?JSON.parse(Buffer.concat(chunks).toString('utf8')):{};
  const json=(status,value)=>{res.writeHead(status,{'content-type':'application/json'});res.end(JSON.stringify(value));};
  if(req.url==='/v1/assets'){
   if(delay)await new Promise(resolve=>setTimeout(resolve,delay));
   const id='asset-'+(uploads.length+1);uploads.push({id,...body});
   if(dropUpload){req.socket.destroy();return;}
   json(200,{id,created_at:Math.floor(Date.now()/1000),expires_at:Math.floor(Date.now()/1000)+1800});return;
  }
  if(req.url==='/v1/videos/generations'){
   submissions.push({body,key:req.headers['idempotency-key']});
   if(drop){req.socket.destroy();return;}
   if(reject){json(400,reject);return;}
   if((body.image_asset_ids||[]).some(id=>expired.has(id))){json(400,{error:{code:'reference_asset_expired',message:'参考素材已过期',request_id:'request-expired'}});return;}
   json(200,{task:{id:'request-task-'+submissions.length,status:'queued'},work_context:{work_id:'work-test',base_version_id:'version-test'}});return;
  }
  json(404,{error:{code:'not_found'}});
 });
 server.listen(0,'127.0.0.1');await once(server,'listening');
 const base=`http://127.0.0.1:${server.address().port}/v1`;
 async function run(action,params={},options={}){
  const args=[quote(action),...Object.entries(params).flatMap(([name,value])=>['-'+name,Array.isArray(value)?`@(${value.map(quote).join(',')})`:quote(value)])];
  const command=`$OutputEncoding=[Console]::OutputEncoding=[Text.UTF8Encoding]::new($false); & ${quote(runner)} ${args.join(' ')}`;
  const env=Object.fromEntries(Object.entries(process.env).filter(([key])=>!key.startsWith('AIWORK')));
  const child=spawn('powershell.exe',['-NoProfile','-ExecutionPolicy','Bypass','-Command',command],{windowsHide:true,env:{...env,APPDATA:root,AIWORK_GATEWAY_BASE_URL:options.base||base,AIWORK_API_KEY:options.key||'fake-reference-key'}});
  let stdout='',stderr='';child.stdout.setEncoding('utf8').on('data',chunk=>stdout+=chunk);child.stderr.setEncoding('utf8').on('data',chunk=>stderr+=chunk);
  const [code]=await once(child,'close');let result;try{result=JSON.parse(stdout.replace(/^\uFEFF/,''));}catch{result=null;}
  return {code,result,stdout,stderr};
 }
 return {root,file,uploads,submissions,run,expire:()=>expired=new Set(uploads.map(u=>u.id)),drop:()=>drop=true,dropUpload:()=>dropUpload=true,reject:value=>reject=value,delay:value=>delay=value,
  close:async()=>{server.closeAllConnections();await new Promise(resolve=>server.close(resolve));await rm(root,{recursive:true,force:true});}};
}

test('an expired ID uploaded by this client is refreshed from its original bytes before a new generation',async()=>{
 const f=await fixture();try{
  const first=await f.run('upload',{AssetPath:f.file});assert.equal(first.code,0,first.stderr);f.expire();
  const submitted=await f.run('submit',{Prompt:'原创测试',ImageAssetId:[first.result.asset_id],IdempotencyKey:'fresh-1'});
  assert.equal(submitted.code,0,submitted.stderr);assert.equal(f.uploads.length,2);
  assert.deepEqual(f.submissions[0].body.image_asset_ids,['asset-2']);
  assert.ok(Buffer.from(f.uploads[1].data_base64,'base64').equals(png));assert.equal(f.submissions.length,1);
 }finally{await f.close();}
});

test('independent batch items refresh all selected references and preserve their order',async()=>{
 const f=await fixture();try{
  const other=path.join(f.root,'场景.png');await writeFile(other,Buffer.concat([png,Buffer.from([42])]));
  const a=await f.run('upload',{AssetPath:f.file}),b=await f.run('upload',{AssetPath:other});
  for(const key of ['batch-1','batch-2'])assert.equal((await f.run('submit',{Prompt:'原创测试',ImageAssetId:[b.result.asset_id,a.result.asset_id],IdempotencyKey:key})).code,0);
  assert.equal(f.uploads.length,6);assert.deepEqual(f.submissions.map(s=>s.body.image_asset_ids),[['asset-3','asset-4'],['asset-5','asset-6']]);
  for(const i of [2,4])assert.equal(f.uploads[i].data_base64,f.uploads[1].data_base64);
  for(const i of [3,5])assert.equal(f.uploads[i].data_base64,f.uploads[0].data_base64);
 }finally{await f.close();}
});

test('repeating one successful submission key returns its original task without uploading or submitting again',async()=>{
 const f=await fixture();try{
  const params={Prompt:'原创测试',ImagePath:[f.file],IdempotencyKey:'repeat-success'};
  const a=await f.run('submit',params),b=await f.run('submit',params);
  assert.equal(a.code,0,a.stderr);assert.equal(b.code,0,b.stderr);assert.equal(a.result.task_id,b.result.task_id);
  assert.equal(f.uploads.length,1);assert.equal(f.submissions.length,1);
 }finally{await f.close();}
});

test('a lost submission response is not replayed by repeating its key',async()=>{
 const f=await fixture();try{
  f.drop();const params={Prompt:'原创测试',ImagePath:[f.file],IdempotencyKey:'unknown-response'};
  assert.notEqual((await f.run('submit',params)).code,0);const second=await f.run('submit',params);
  assert.notEqual(second.code,0);assert.equal(f.submissions.length,1);assert.equal(f.uploads.length,1);
  assert.match(second.stderr,/SUBMISSION_UNKNOWN|结果未知/);
 }finally{await f.close();}
});

test('an unregistered ID and a changed source are rejected before any upload or generation',async()=>{
 const f=await fixture();try{
  const unknown=await f.run('submit',{Prompt:'原创测试',ImageAssetId:['asset-from-another-client'],IdempotencyKey:'unknown-source'});
  assert.notEqual(unknown.code,0);assert.equal(f.uploads.length,0);assert.equal(f.submissions.length,0);
  const asset=await f.run('upload',{AssetPath:f.file});await writeFile(f.file,Buffer.concat([png,Buffer.from([1])]));
  const changed=await f.run('submit',{Prompt:'原创测试',ImageAssetId:[asset.result.asset_id],IdempotencyKey:'changed-source'});
  assert.notEqual(changed.code,0);assert.match(changed.stderr,/REFERENCE_CHANGED|变化/);assert.equal(f.uploads.length,1);assert.equal(f.submissions.length,0);
 }finally{await f.close();}
});

test('reference source records are scoped to the gateway credential',async()=>{
 const f=await fixture();try{
  const asset=await f.run('upload',{AssetPath:f.file});
  const wrong=await f.run('submit',{Prompt:'原创测试',ImageAssetId:[asset.result.asset_id],IdempotencyKey:'other-owner'},{key:'another-fake-key'});
  assert.notEqual(wrong.code,0);assert.equal(f.uploads.length,1);assert.equal(f.submissions.length,0);
 }finally{await f.close();}
});

test('HTTP rejection preserves a safe code and request ID without provider text or credentials',async()=>{
 const f=await fixture();try{
  f.reject({error:{code:'reference_asset_expired',message:'PRIVATE_PROMPT https://private.test/?token=SECRET fake-reference-key',request_id:'request-fake-expired'}});
  const result=await f.run('submit',{Prompt:'原创测试',IdempotencyKey:'structured-400'});
  assert.notEqual(result.code,0);assert.match(result.stderr,/reference_asset_expired/);assert.match(result.stderr,/request-fake-expired/);
  assert.doesNotMatch(result.stderr+result.stdout,/PRIVATE_PROMPT|private\.test|SECRET|fake-reference-key/);assert.equal(f.submissions.length,1);
 }finally{await f.close();}
});

test('same-key changes and simultaneous duplicate calls cannot create a second generation',async()=>{
 const f=await fixture();try{
  f.delay(100);const params={Prompt:'原创测试',ImagePath:[f.file],IdempotencyKey:'concurrent-one'};
  const results=await Promise.all([f.run('submit',params),f.run('submit',params)]);
  assert.ok(results.some(r=>r.code===0),JSON.stringify(results));assert.equal(f.submissions.length,1);assert.equal(f.uploads.length,1);
  const changed=await f.run('submit',{...params,Prompt:'另一个需求'});assert.notEqual(changed.code,0);assert.equal(f.submissions.length,1);
 }finally{await f.close();}
});

test('nine references stay complete and the tenth is blocked before uploading',async()=>{
 const f=await fixture();try{
  const accepted=await f.run('submit',{Prompt:'原创测试',ImagePath:Array(9).fill(f.file),IdempotencyKey:'nine-images'});
  assert.equal(accepted.code,0,accepted.stderr);assert.equal(f.uploads.length,9);assert.deepEqual(f.submissions[0].body.image_asset_ids,Array.from({length:9},(_,i)=>'asset-'+(i+1)));
  const rejected=await f.run('submit',{Prompt:'原创测试',ImagePath:Array(10).fill(f.file),IdempotencyKey:'ten-images'});
  assert.notEqual(rejected.code,0);assert.equal(f.uploads.length,9);assert.equal(f.submissions.length,1);
 }finally{await f.close();}
});

test('a missing later file or an unknown upload prevents the generation POST',async()=>{
 const f=await fixture();try{
  const missing=await f.run('submit',{Prompt:'原创测试',ImagePath:[f.file,path.join(f.root,'missing.png')],IdempotencyKey:'missing-file'});
  assert.notEqual(missing.code,0);assert.equal(f.uploads.length,0);assert.equal(f.submissions.length,0);
  f.dropUpload();const params={Prompt:'原创测试',ImagePath:[f.file],IdempotencyKey:'upload-unknown'};
  assert.notEqual((await f.run('submit',params)).code,0);assert.notEqual((await f.run('submit',params)).code,0);
  assert.equal(f.uploads.length,1);assert.equal(f.submissions.length,0);
 }finally{await f.close();}
});
