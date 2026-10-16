import assert from "node:assert/strict";
import {readFileSync} from "node:fs";
import {test} from "node:test";
import vm from "node:vm";
import ts from "typescript";
const text=readFileSync(new URL("./App.tsx",import.meta.url),"utf8");
const app=ts.createSourceFile("App.tsx",text,ts.ScriptTarget.Latest,true,ts.ScriptKind.TSX);
const names=["RelayProfileList","removeRelayProfile","duplicateRelayProfile","reorderRelayProfiles"];
const source=app.statements.filter(n=>ts.isFunctionDeclaration(n)&&names.includes(n.name?.text??"")).map(n=>n.getText(app)).join("\n");
const code=ts.transpileModule(source,{compilerOptions:{jsx:ts.JsxEmit.React,jsxFactory:"jsx",target:ts.ScriptTarget.ES2022}}).outputText;
const ctx=vm.createContext({
  jsx:(type:any,props:any,...children:any[])=>({type,props,children}),
  useSensors:(...v:any[])=>v,useSensor:()=>null,PointerSensor:0,KeyboardSensor:0,sortableKeyboardCoordinates:0,
  DndContext:"dnd",SortableContext:"sortable",SortableRelayProfileCard:"card",verticalListSortingStrategy:0,closestCenter:0,
  isAggregateRelayProfile:(p:any)=>p.relayMode==="aggregate",normalizeAggregateRelayProfile:(p:any)=>p,
  normalizeAggregateConfig:(v:any)=>v,normalizeRelayModelRoutes:(v:any)=>v??[],
  syncLegacyRelayFields:(s:any)=>({...s,activeRelayId:s.relayProfiles.some((p:any)=>p.id===s.activeRelayId)?s.activeRelayId:s.relayProfiles[0]?.id}),
  defaultSettings:{relayProfiles:[{id:"default"}]},t:(s:string)=>s,tf:(s:string,args:any[])=>s.replace("{0}",args[0]),
});
vm.runInContext(code,ctx);
const fixture=()=>({activeRelayId:"agg",relayProfiles:[{id:"default",relayMode:"pureApi"},{id:"member",relayMode:"pureApi"},{id:"unused",relayMode:"pureApi"},{id:"agg",relayMode:"aggregate",aggregate:{members:[{profileId:"member"},{profileId:"unused"}],routes:[]}}]});
test("provider page passes complete settings to mutations and filters only visible cards",()=>{
  const parent=text.slice(text.indexOf("  const saveRelaySettings ="),text.indexOf("function multiLaunchSupported("));
  const list=parent.slice(parent.indexOf("<RelayProfileList"),parent.indexOf("/>",parent.indexOf("<RelayProfileList")));
  assert.match(list,/form=\{normalized\}/);assert.match(list,/visibleProfiles=\{visibleProfiles\}/);
  assert.match(list,/onFormChange=\{saveRelaySettings\}/);
  const form=fixture(),visible=form.relayProfiles.slice(0,3);let saved:any;
  const tree=ctx.RelayProfileList({form,visibleProfiles:visible,onFormChange:(v:any)=>{saved=v}});
  const sortable=tree.children[0],cards=sortable.children[0].children[0];
  assert.deepEqual([...sortable.props.items],["default","member","unused"]);
  for(const card of cards)assert.equal(card.props.form,form);
  tree.props.onDragEnd({active:{id:"unused"},over:{id:"default"}});
  assert.equal(saved.activeRelayId,"agg");assert.equal(saved.relayProfiles.length,4);
  assert.equal(saved.relayProfiles.find((p:any)=>p.id==="agg"),form.relayProfiles[3]);
});
test("deleting an inactive direct profile preserves the hidden active aggregate and removes its references",()=>{
  const form=fixture();const next=ctx.removeRelayProfile(form,"unused");
  assert.equal(next.activeRelayId,"agg");
  assert.deepEqual(next.relayProfiles.map((p:any)=>p.id),["default","member","agg"]);
  assert.deepEqual(next.relayProfiles[2].aggregate.members.map((m:any)=>m.profileId),["member"]);
  assert.equal(form.relayProfiles.length,4);
});
test("copy and reorder preserve the hidden active provider and the other page's complete configuration",()=>{
  const form=fixture();
  for(const next of [ctx.duplicateRelayProfile(form,"member"),ctx.reorderRelayProfiles(form,"unused","default")]){
    assert.equal(next.activeRelayId,"agg");
    assert.equal(next.relayProfiles.find((p:any)=>p.id==="agg"),form.relayProfiles[3]);
    assert.ok(next.relayProfiles.some((p:any)=>p.id==="member"));
  }
});
