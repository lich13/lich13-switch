import { useCallback, useEffect, useRef, useState } from "react";
import { command } from "./bridge";
import { confirmAction } from "./confirmation";
import { errorOf } from "./types";

let beforeLeave: (() => Promise<boolean>) | null = null;
export const flushTestPrompt = () => beforeLeave?.() ?? Promise.resolve(true);

export function useTestPrompt(initial: string | undefined) {
  const [prompt, setPrompt] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const draft = useRef<string | null>(null),
    saved = useRef("");
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const flight = useRef<Promise<boolean> | null>(null);
  useEffect(() => {
    if (initial === undefined || draft.current !== null) return;
    draft.current = initial;
    saved.current = initial;
    setPrompt(initial);
  }, [initial]);
  const flush = useCallback((): Promise<boolean> => {
    if (timer.current) clearTimeout(timer.current);
    timer.current = null;
    if (flight.current) return flight.current;
    const work = async () => {
      try {
        while (draft.current !== null && draft.current !== saved.current) {
          const value = draft.current;
          if (!value.trim()) throw new Error("文案不能为空，原文案已保留。");
          setSaving(true);
          await command("provider_test_save_prompt", { prompt: value });
          saved.current = value;
        }
        setError("");
        return true;
      } catch (e) {
        setError(errorOf(e).message);
        return false;
      } finally {
        setSaving(false);
      }
    };
    const result = work();
    flight.current = result;
    void result.finally(() => {
      if (flight.current === result) flight.current = null;
    });
    return result;
  }, []);
  const change = useCallback(
    (value: string) => {
      draft.current = value;
      setPrompt(value);
      setError("");
      if (timer.current) clearTimeout(timer.current);
      timer.current = setTimeout(() => {
        void flush();
      }, 500);
    },
    [flush],
  );
  useEffect(() => {
    const leave = async () => {
      if (await flush()) return true;
      if (!(await confirmAction("全局测试文案尚未保存。放弃本次输入并离开？")))
        return false;
      draft.current = saved.current;
      setPrompt(saved.current);
      setError("");
      if (timer.current) clearTimeout(timer.current);
      return true;
    };
    beforeLeave = leave;
    return () => {
      if (beforeLeave === leave) beforeLeave = null;
      if (timer.current) clearTimeout(timer.current);
    };
  }, [flush]);
  return {
    prompt,
    change,
    saving,
    error,
    flush,
    snapshot: () => draft.current,
  };
}
