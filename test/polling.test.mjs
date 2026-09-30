import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import vm from 'node:vm';
import test from 'node:test';
import ts from 'typescript';

const source=await readFile(new URL('../src/App.tsx',import.meta.url),'utf8');
const marker=source.indexOf('const readDiagnostics =');
const start=source.lastIndexOf('  useEffect(() => {',marker);
const end=source.indexOf('  useEffect(() => {',marker);
const effect=ts.transpileModule(source.slice(start,end),{compilerOptions:{target:ts.ScriptTarget.ES2022}}).outputText;
const flush=()=>new Promise(setImmediate);

function fixture(mode,monitorViewMode,invokeOverride) {
  const calls=[],statuses=[],diagnostics=[],errors=[],status={document_id:'authority-id',state:'active',session_active_seconds:42};
  let poll,cleanup;
  const context=vm.createContext({mode,monitorViewMode,Promise,documentIdRef:{current:null},
    useEffect:fn=>{cleanup=fn();},window:{setInterval(fn,delay){assert.equal(delay,1000);poll=fn;return 1;},clearInterval(id){assert.equal(id,1);poll=null;}},
    invoke(command){calls.push(command);return invokeOverride?invokeOverride(command):Promise.resolve(command==='get_core_status'?status:{current_file_chars:100});},
    receiveStatus:(_source,value)=>statuses.push(value),setWatchDiagnostics:value=>diagnostics.push(value),setWatchError:value=>errors.push(value),
  });
  vm.runInContext(effect,context);
  return {calls,statuses,diagnostics,errors,status,context,tick:()=>poll?.(),stop:()=>cleanup(),hasTimer:()=>poll!==null};
}

test('compact polls only authoritative status; advanced and editor retain diagnostics',async()=>{
  for(const [mode,view,reads] of [['monitor','compact',0],['monitor','advanced',1],['editor','compact',1]]){
    const f=fixture(mode,view);f.tick();await flush();
    assert.equal(f.calls.filter(value=>value==='get_core_status').length,1);
    assert.equal(f.calls.filter(value=>value==='get_watch_diagnostics').length,reads);
    assert.equal(f.diagnostics.length,reads);assert.equal(f.statuses[0],f.status);
    assert.equal(f.context.documentIdRef.current,'authority-id');
    f.stop();assert.equal(f.hasTimer(),false);
  }
});

test('slow requests never overlap and obsolete view responses cannot update state',async()=>{
  let resolve;
  const f=fixture('monitor','advanced',command=>command==='get_core_status'?new Promise(yes=>{resolve=yes;}):Promise.resolve({current_file_chars:999}));
  f.tick();f.tick();assert.equal(f.calls.length,2,'only one status + diagnostics pair');
  f.stop();resolve({document_id:'obsolete'});await flush();
  assert.deepEqual(f.statuses,[]);assert.deepEqual(f.diagnostics,[]);assert.deepEqual(f.errors,[]);
  assert.equal(f.context.documentIdRef.current,null);
});

test('late rejection after cleanup cannot replace a newer view error state',async()=>{
  let reject;
  const f=fixture('monitor','compact',()=>new Promise((_yes,no)=>{reject=no;}));
  f.tick();f.stop();reject(new Error('old view disconnected'));await flush();
  assert.deepEqual(f.errors,[]);assert.deepEqual(f.statuses,[]);
});

test('completed or failed polls release the single in-flight slot',async()=>{
  let fail=true;
  const f=fixture('monitor','compact',()=>fail?Promise.reject(new Error('offline')):Promise.resolve({document_id:'back'}));
  f.tick();await flush();assert.equal(f.errors.length,1);
  fail=false;f.tick();await flush();assert.equal(f.calls.length,2);assert.equal(f.statuses[0].document_id,'back');
  f.stop();
});
