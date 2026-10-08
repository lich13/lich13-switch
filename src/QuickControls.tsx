import {useEffect,useRef,useState} from "react";
import {Moon,SquarePower} from "lucide-react";
import {command,subscribe} from "./bridge";
import {errorOf} from "./types";
type State={supported:boolean;enabled:boolean;batterySleep:number;revision:string;externalChanged?:boolean};
export default function QuickControls({visible,notify,error}:{visible:boolean;notify:(s:string)=>void;error:(s:string)=>void}) {
 const [state,setState]=useState<State|null>(null),[powerBusy,setPowerBusy]=useState(false),[quitBusy,setQuitBusy]=useState(false),[readError,setReadError]=useState("");
 const lock=useRef(false),quitting=useRef(false);
 useEffect(()=>{let disposed=false;let clean=()=>{};void subscribe<State>("clamshell-state",s=>{if(!disposed)setState(s);}).then(c=>disposed?c():clean=c);return()=>{disposed=true;clean();};},[]);
 useEffect(()=>{if(!visible)return;let disposed=false;
  const refresh=async()=>{if(lock.current)return;try{const s=await command<State>("get_clamshell_state");if(!disposed){setState(s);setReadError("");}}catch(e){if(!disposed)setReadError(errorOf(e).message);}};
  void refresh();const t=setInterval(()=>void refresh(),5000);return()=>{disposed=true;clearInterval(t);};
 },[visible]);
 const toggle=async()=>{if(!state||lock.current)return;lock.current=true;setPowerBusy(true);try{const next=await command<State>("set_clamshell_awake",{enabled:!state.enabled,expectedRevision:state.revision});setState(next);notify(next.enabled?"已开启合盖不休眠":"已恢复电池休眠设置");}catch(e){error(errorOf(e).message);try{setState(await command<State>("get_clamshell_state"));}catch{setReadError("电源状态读取失败");}}finally{lock.current=false;setPowerBusy(false);}};
 const quit=async()=>{if(quitting.current)return;quitting.current=true;setQuitBusy(true);try{const r=await command<{terminated:number;failed:number}>("force_quit_codex_clients");notify(`已退出 ${r.terminated} 个进程${r.failed?`，${r.failed} 个未能退出`:""}`);}catch(e){error(errorOf(e).message);}finally{quitting.current=false;setQuitBusy(false);}};
 return <div className="quick-controls">
  {(state?.supported||readError)&&<div className="quick-power"><label><Moon size={14}/>合盖不休眠<input aria-label="合盖不休眠" type="checkbox" role="switch" checked={state?.enabled??false} disabled={powerBusy||!!readError||!state} onChange={()=>void toggle()}/></label>{state?.externalChanged&&<span role="status">外部已修改</span>}{readError&&<span role="alert">{readError}</span>}</div>}
  <button className="quick-force" disabled={quitBusy} onClick={()=>void quit()}><SquarePower size={14}/>{quitBusy?"正在退出…":"强制退出 ChatGPT / Codex"}</button>
 </div>;
}
