import { useEffect, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, fmtCount, type DupRules as Rules } from "../api";

const tail = (p: string) => {
  const parts = p.split(/[\\/]/).filter(Boolean);
  return parts.slice(-2).join("\\") || p;
};

/**
 * "Which copy to keep" rules by folder, applied to every duplicate group at once:
 * copies in a keep folder win, copies in a drop folder lose.
 */
export default function DupRulesCard({ onChanged }: { onChanged: () => void }) {
  const [rules, setRules] = useState<Rules>({ keep: [], drop: [] });
  const [suggest, setSuggest] = useState<[string, number][]>([]);
  const [msg, setMsg] = useState<string | null>(null);
  const [openCard, setOpenCard] = useState(false);

  useEffect(() => {
    api.getDupRules().then((r) => {
      setRules(r);
      if (r.keep.length + r.drop.length > 0) setOpenCard(true);
    });
    api.dupRuleSuggestions().then(setSuggest).catch(() => {});
  }, []);

  const save = async (next: Rules) => {
    setRules(next);
    const changed = await api.setDupRules(next);
    setMsg(changed > 0 ? `${fmtCount(changed)}개 묶음의 남길 파일이 규칙에 따라 바뀌었습니다` : "규칙을 저장했습니다 (바뀐 묶음 없음)");
    onChanged();
  };

  const add = async (which: "keep" | "drop", path?: string) => {
    const picked = path ?? ((await open({ directory: true, title: which === "keep" ? "항상 남길 폴더" : "먼저 지울 폴더" })) as string | null);
    if (!picked) return;
    const other = which === "keep" ? "drop" : "keep";
    await save({ ...rules, [which]: [...rules[which].filter((p) => p !== picked), picked], [other]: rules[other].filter((p) => p !== picked) });
  };
  const remove = (which: "keep" | "drop", p: string) => save({ ...rules, [which]: rules[which].filter((x) => x !== p) });

  const count = rules.keep.length + rules.drop.length;
  const unused = suggest.filter(([p]) => !rules.drop.includes(p) && !rules.keep.includes(p));

  return (
    <div className="card dup-rules">
      <div className="row-between" onClick={() => setOpenCard(!openCard)} style={{ cursor: "pointer" }}>
        <b>남기기 규칙 {count > 0 && <span className="muted small">· {count}개</span>}</b>
        <span className="muted small">
          {openCard ? "접기 ▴" : "폴더 기준으로 모든 묶음에 한 번에 적용 ▾"}
        </span>
      </div>
      {openCard && (
        <>
          <div className="rules-cols">
            {(["keep", "drop"] as const).map((which) => (
              <div key={which} className="rules-col">
                <div className="small">
                  <b>{which === "keep" ? "항상 남길 폴더" : "먼저 지울 폴더"}</b>{" "}
                  <span className="muted">{which === "keep" ? "이 안의 사본을 남깁니다" : "다른 곳에 사본이 있으면 이 안의 것을 지웁니다"}</span>
                </div>
                <div className="chips">
                  {rules[which].map((p) => (
                    <span key={p} className={`chip ${which === "keep" ? "user" : "rule-drop"}`} title={p}>
                      {tail(p)}
                      <button onClick={() => remove(which, p)}>×</button>
                    </span>
                  ))}
                  <button className="chip ghost" onClick={() => add(which)}>+ 폴더</button>
                </div>
              </div>
            ))}
          </div>
          {unused.length > 0 && (
            <div className="small">
              <span className="muted">추천 — 메신저·다운로드 폴더의 사본: </span>
              {unused.map(([p, n]) => (
                <button key={p} className="chip ghost" title={p} onClick={() => add("drop", p)}>
                  + {tail(p)} <span className="muted">{fmtCount(n)}</span>
                </button>
              ))}
            </div>
          )}
          {msg && <div className="small ok-text">{msg}</div>}
        </>
      )}
    </div>
  );
}
