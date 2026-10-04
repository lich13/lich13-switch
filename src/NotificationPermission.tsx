import { useEffect, useState } from "react";
import { command, subscribe } from "./bridge";
import { errorOf } from "./types";
type Permission = {permission: string;error: string|null};
export default function NotificationPermission() {
  const [state,setState]=useState<Permission|null>(null),[busy,setBusy]=useState(false);
  useEffect(()=>{let disposed=false;let clean=()=>{};void command<Permission>("notification_permission").then(s=>{if(!disposed)setState(s);}).catch(e=>{if(!disposed)setState({permission:"unknown",error:errorOf(e).message});});void subscribe<Permission>("notification-state",s=>{if(!disposed)setState(s);}).then(fn=>disposed?fn():clean=fn);return()=>{disposed=true;clean();};},[]);
  return <div className="setting-row"><span>通知权限</span><div className="permission-value"><span role="status">{state?.error??({granted:"已允许",denied:"系统已关闭",prompt:"未授权",unknown:"无法确认"}[state?.permission??""]??"读取中")}</span>{state?.permission==="prompt"&&<button type="button" className="text-button" disabled={busy} onClick={()=>{setBusy(true);void command<Permission>("notification_permission",{request:true}).then(setState).catch(e=>setState({permission:"unknown",error:errorOf(e).message})).finally(()=>setBusy(false));}}>授权</button>}</div></div>;
}
