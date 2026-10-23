import { useEffect, useRef, useState } from "react";
import { command } from "./bridge";
import type { DshOfficialModel } from "./bridge-contracts";
import { errorOf } from "./types";

export function useDshCatalog() {
  const [models, setModels] = useState<DshOfficialModel[]>([]);
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(true);
  const generation = useRef(0);
  async function refresh() {
    const seq = ++generation.current;
    setLoading(true);
    try {
      const result = await command("dsh_official_models", { refresh: true });
      if (seq !== generation.current) return;
      setModels(result.models);
      setError(result.error ?? "");
    } catch (e) {
      if (seq === generation.current) setError(errorOf(e).message);
    } finally {
      if (seq === generation.current) setLoading(false);
    }
  }
  useEffect(() => {
    let gone = false;
    void command("dsh_official_models", { refresh: false })
      .then((result) => {
        if (!gone) setModels(result.models);
      })
      .catch(() => {})
      .finally(() => {
        if (!gone) void refresh();
      });
    return () => {
      gone = true;
      generation.current++;
    };
  }, []);
  return { models, error, loading, refresh };
}
