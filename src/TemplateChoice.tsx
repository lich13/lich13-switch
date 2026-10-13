import { useState } from "react";

export type PreparedTemplate = { ready: boolean; token: string | null; rows: { id: number; name: string }[] };

export default function TemplateChoice({ template, busy, select }: { template: PreparedTemplate; busy: boolean; select: (id: number) => void }) {
  const [selected, setSelected] = useState<number | null>(null);
  return <fieldset className="template-choice" disabled={busy}>
    <legend>选择一个账号作为导出原型</legend>
    <div className="exchange-list">
      {template.rows.map(row => <label className="exchange-row" key={row.id}>
        <input type="radio" name="template-account" checked={selected === row.id} onChange={() => setSelected(row.id)} />
        <span><strong>{row.name}</strong></span>
      </label>)}
    </div>
    <button disabled={selected === null} onClick={() => selected !== null && select(selected)}>使用此原型</button>
  </fieldset>;
}
