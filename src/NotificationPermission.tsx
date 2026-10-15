import { subscriptions } from "./subscriptions";
import { useEffect, useState } from "react";
import { command } from "./bridge";
import { errorOf } from "./types";
import type { NotificationState as Permission } from "./bridge-contracts";
export default function NotificationPermission() {
  const [state,setState]=useState<Permission|null>(null),[busy,setBusy]=useState(false);
  useEffect(()=>{let disposed=false;const events=subscriptions(e=>setState(s=>({permission:s?.permission??"unknown",error:errorOf(e).message})));void command("notification_permission").then(s=>{if(!disposed)setState(s);}).catch(e=>{if(!disposed)setState({permission:"unknown",error:errorOf(e).message});});events.watch("notification-state",setState);return()=>{disposed=true;events.dispose();};},[]);
  const run=async(name:"notification_permission"|"test_notification"|"open_notification_settings",args:{request?:boolean}={})=>{setBusy(true);try{const result=await command(name,args);if(result)setState(result);}catch(e){setState(s=>({permission:s?.permission??"unknown",error:errorOf(e).message}));}finally{setBusy(false);}};
  const permissionLabel=({granted:"已允许",denied:"系统已关闭",prompt:"未授权",unknown:"无法确认"}[state?.permission??""]??"读取中");
  const deliveryLabel=state?.permission==="granted"?({pending:"等待发送",accepted:"系统已接受",failed:"发送失败",idle:""}[state.delivery??"idle"]):"";
  return <div className="notification-controls"><div className="setting-row"><span>通知权限</span><div className="permission-value"><span role="status">{state?.error??`${permissionLabel}${deliveryLabel?` · ${deliveryLabel}`:""}`}</span>{state?.permission==="prompt"&&<button type="button" className="text-button" disabled={busy} onClick={()=>void run("notification_permission",{request:true})}>授权</button>}</div></div><div className="usage-actions"><button type="button" disabled={busy||state?.delivery==="pending"} onClick={()=>void run("test_notification")}>发送测试通知</button><button type="button" disabled={busy} onClick={()=>void run("open_notification_settings")}>系统通知设置</button></div></div>;
}
