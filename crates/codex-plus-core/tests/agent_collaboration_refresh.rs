use std::process::Command;

const SCRIPT: &str = include_str!("../../../assets/inject/agent-collaboration-refresh.js");

// 仅运行自有 vm fixture，不连接任何 CDP/真实页面，也不启动模型或 agent。
const FIXTURE: &str = r#"
const assert = require('node:assert/strict');
const vm = require('node:vm');
const source = process.argv[1];
const mode = process.argv[2];
const uuid = '01a14973-0aa5-72a1-850f-79e8462acfc1';
const names = ['list_agents','delegate_task','delegate_to_deepseek','delegate_to_claude_code','delegate_to_codex','send_task','get_task','wait_task','list_tasks','cancel_task'];
const tools = Object.fromEntries(names.map(name => [name,{name,inputSchema:{type:'object'}}]));
const item = runtimeStatus => ({name:'codex_plus_collaboration',runtimeStatus,tools,toolsError:null});
const key = ['mcp','servers','status','local','full'];
const calls = [];
const cacheCalls = [];
const query = {queryKey:key,state:{data:{data:[]},isInvalidated:false}};
const otherQuery = {queryKey:['mcp','servers','status','remote','full'],state:{data:{keep:true}}};
const queryClient = {
  getQueryCache() { return {find(filter) { assert.equal(filter.exact,true); assert.deepEqual(Array.from(filter.queryKey),key); return query; }}; },
  setQueryData() { throw new Error('The diagnostic must never synthesize cache data'); },
  getQueryData(requested) { assert.deepEqual(Array.from(requested),key); return query.state.data; },
  async invalidateQueries(filter) { cacheCalls.push(['invalidate',filter]); assert.equal(filter.exact,true); assert.equal(filter.refetchType,'none'); assert.deepEqual(Array.from(filter.queryKey),key); query.state.isInvalidated=true; },
  async refetchQueries(filter) { cacheCalls.push(['refetch',filter]); assert.equal(filter.exact,true); assert.equal(filter.type,'all'); assert.deepEqual(Array.from(filter.queryKey),key); query.state.data={data:[item(null)]}; query.state.isInvalidated=false; },
};
const client = {
  hostId:'local', disposed:false,
  async sendRequest(method,params) {
    calls.push([method,params]);
    if (method==='config/mcpServer/reload') {
      assert.equal(params,null);
      if (mode==='reload-error') throw new Error('FAKE_SECRET_FIXTURE must never escape');
      return {};
    }
    assert.equal(method,'mcpServerStatus/list');
    assert.equal(params.serverName,'codex_plus_collaboration');
    assert.equal(params.detail,'toolsAndAuthOnly');
    assert.equal(params.limit,100);
    if (mode==='missing-tools' && calls.length===1) return {data:[{name:'codex_plus_collaboration',tools:{get_task:{name:'get_task'}}}]};
    if (mode==='preflight-error' && calls.length===1) throw new Error('FAKE_SECRET_PREFLIGHT must never escape');
    if (params.threadId) assert.equal(params.threadId,uuid);
    return {data:[item(params.threadId ? mode==='thread-not-ready' ? 'starting' : 'connected' : null)]};
  },
  __codexPlusModelOriginalSendRequest() { throw new Error('Original methods must not be used'); },
};
const entry = {hostId:'local',getterBound:false,ref:{deref:()=>client}};
const registry = new Map([['local',entry]]);
const root = {};
const window = {location:{pathname:'/thread/'+uuid,search:'',hash:''},__codexPlusNativeHostClients:registry,__codexQueryClient:queryClient};
if (mode==='no-bridge') delete window.__codexPlusNativeHostClients;
if (mode==='remote-host') client.hostId='remote';
if (mode==='mismatched-entry') entry.hostId='remote';
if (mode==='disposed') client.disposed=true;
if (mode==='getter-bound') entry.getterBound=true;
if (mode==='missing-query-client') delete window.__codexQueryClient;
if (mode==='no-thread') window.location.pathname='/home';
if (mode==='temporary-thread') window.location.pathname='/thread/client-new-thread:temporary';
if (mode==='fiber-query-client') {
  delete window.__codexQueryClient;
  root.__reactFiber$fixture={memoizedProps:{client:queryClient}};
}
const document = {body:root,documentElement:{},getElementById:id=>id==='root'?root:null,createElement(){throw new Error('DOM mutation forbidden');}};
const context = {window,document,Map,Set,Promise,JSON,Object,Array,Error,setTimeout,clearTimeout};
(async()=>{
 const report=JSON.parse(await vm.runInNewContext(source,context,{timeout:1000}));
 if (['no-bridge','remote-host','mismatched-entry','disposed','getter-bound'].includes(mode)) {
   assert.equal(report.status,'unavailable'); assert.equal(report.reloadSucceeded,false); assert.equal(calls.length,0);
 } else if (mode==='reload-error') {
   assert.equal(report.status,'error'); assert.equal(report.reloadSucceeded,false); assert.equal(calls.length,2); assert.ok(!JSON.stringify(report).includes('FAKE_SECRET_FIXTURE'));
 } else {
   assert.equal(report.reloadSucceeded,true);
   assert.deepEqual(calls.map(call=>call[0]),['mcpServerStatus/list','config/mcpServer/reload','mcpServerStatus/list']);
   assert.equal(calls[0][1].threadId,undefined);
   assert.equal(calls[2][1].threadId,['no-thread','temporary-thread'].includes(mode)?undefined:uuid);
   assert.equal(report.toolCount,10);
   if (mode==='missing-query-client') {
     assert.equal(report.status,'partial'); assert.equal(report.cacheRefreshed,false); assert.equal(report.toolsReady,true); assert.equal(cacheCalls.length,0);
   } else {
     assert.deepEqual(cacheCalls.map(call=>call[0]),['invalidate','refetch']);
     assert.equal(report.cacheRefreshed,true);
     assert.deepEqual(otherQuery.state.data,{keep:true});
     if (mode==='thread-not-ready') {
       assert.equal(report.status,'partial'); assert.equal(report.threadVerified,false); assert.equal(report.toolsReady,false);
     } else {
       assert.equal(report.status,'ready'); assert.equal(report.toolsReady,true); assert.equal(report.threadVerified,!['no-thread','temporary-thread'].includes(mode));
     }
   }
 }
 assert.deepEqual(Object.keys(report).sort(),['status','message','reloadSucceeded','toolsReady','toolCount','threadVerified','cacheRefreshed'].sort());
 assert.ok(!JSON.stringify(report).includes('FAKE_SECRET_PREFLIGHT'));
 console.log('fixture passed: '+mode);
})().catch(error=>{console.error(error);process.exitCode=1;});
"#;

fn fixture(mode: &str) {
    let output = Command::new("node")
        .args(["-e", FIXTURE, SCRIPT, mode])
        .output()
        .expect("node is required for renderer fixture tests");
    assert!(
        output.status.success(),
        "fixture {mode}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn native_refresh_refuses_missing_remote_disposed_and_cross_scope_clients() {
    for mode in [
        "no-bridge",
        "remote-host",
        "mismatched-entry",
        "disposed",
        "getter-bound",
    ] {
        fixture(mode);
    }
}

#[test]
fn native_refresh_uses_only_fixed_rpc_sequence_and_refreshes_exact_global_full_cache() {
    fixture("ready");
    fixture("fiber-query-client");
    fixture("no-thread");
    fixture("temporary-thread");
}

#[test]
fn native_refresh_does_not_require_the_old_global_catalog_to_already_see_new_tools() {
    fixture("missing-tools");
    fixture("preflight-error");
}

#[test]
fn native_refresh_reports_partial_without_cache_or_connected_current_thread() {
    fixture("missing-query-client");
    fixture("thread-not-ready");
}

#[test]
fn native_refresh_failure_does_not_echo_provider_or_transport_details() {
    fixture("reload-error");
}

#[tokio::test]
async fn native_refresh_without_trusted_ports_is_unavailable_without_side_effects() {
    let report = codex_plus_core::agent_collaboration::native_refresh::refresh_in_codex(&[]).await;
    assert_eq!(report.status, "unavailable");
    assert!(!report.reload_succeeded);
    assert!(!report.tools_ready);
    assert!(!report.cache_refreshed);
}
