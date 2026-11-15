import { useEffect, useMemo, useState } from "react";
import { confirm, open } from "@tauri-apps/plugin-dialog";
import {
  api, fmtBytes, fmtCount,
  type FolderRule, type ImportSettings, type LibraryOptions, type PlanItem, type PlanView, type TreeNode,
} from "../api";
import { useJob } from "../App";

const GB = 1024 * 1024 * 1024;
const DEFAULT_LIB: LibraryOptions = { big_video_bytes: 2 * GB, event_span_days: 62, remove_copies: true, remove_smaller_versions: true };
const TAGS: Record<string, string> = {
  event: "이벤트", trip: "여행", loose: "낱장", received: "받은 사진", screenshot: "스크린샷", undated: "날짜 모름",
  raw: "RAW", live: "Live Photo", edited: "편집본",
};
/** Top-level areas that are not years. */
const AREAS: Record<string, string> = { "받은 사진": "받은 사진", "큰 동영상": "큰 동영상 (2GB 이상)", "_날짜확인필요": "날짜 확인 필요" };

/**
 * "라이브러리로 모으기": every photo and video into one library as year / year-month / event,
 * events named after the folders they came from. The preview is a folder tree; events can be
 * renamed, turned into loose photos or left out before running; one folder can be tried first.
 * Other drives are copied from and left as they are; copies on the library drive are removed.
 */
export default function LibraryPlan() {
  const { running, setRunning, lastDone } = useJob();
  const [settings, setSettings] = useState<ImportSettings | null>(null);
  const [lib, setLib] = useState<LibraryOptions>(DEFAULT_LIB);
  const [plan, setPlan] = useState<PlanView | null>(null);
  const [tree, setTree] = useState<TreeNode[]>([]);
  const [removes, setRemoves] = useState<PlanItem[]>([]);
  const [rules, setRules] = useState<FolderRule[]>([]);
  const [open_, setOpen] = useState<Set<string>>(new Set());
  const [renaming, setRenaming] = useState<{ node: TreeNode; name: string } | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [result, setResult] = useState<any>(null);
  const [planning, setPlanning] = useState(false);

  useEffect(() => {
    api.getImportSettings().then(setSettings).catch((e) => setErr(String(e)));
    api.getLibraryRules().then(setRules).catch(() => {});
  }, []);

  // "E:" on Windows; elsewhere the library folder itself.
  const drive = (() => {
    const d = settings?.target_dir ?? "";
    const m = /^([A-Za-z]:)/.exec(d);
    return m ? m[1].toUpperCase() : "라이브러리 드라이브";
  })();
  const preview = async (target = settings?.target_dir) => {
    if (!target) return pickTarget();
    setErr(null);
    setResult(null);
    setPlanning(true);
    try {
      await api.startPlan({
        target_dir: target, template: "", rename: "keep", mode: "move", filter: {}, uncertain_folder: null,
        remove_empty_dirs: true, layout: "library", library: lib,
      });
      setRunning("organize:plan");
    } catch (e) {
      setErr(String(e));
      setPlanning(false);
    }
  };

  useEffect(() => {
    if (!lastDone) return;
    if (lastDone.job === "organize:plan" && planning) {
      setPlanning(false);
      if (lastDone.ok) {
        const p = lastDone.result as PlanView;
        setPlan(p);
        api.planTree(p.id).then(setTree).catch((e) => setErr(String(e)));
        api.planItems(p.id, "remove", 0, 300).then(setRemoves).catch(() => setRemoves([]));
      } else setErr(lastDone.error ?? "미리보기를 만들지 못했습니다");
    }
    if (lastDone.job === "organize:run") {
      setPlan(null);
      setTree([]);
      if (lastDone.ok) {
        setResult(lastDone.result);
        preview(); // what is left (after trying one folder, or nothing)
      }
    }
  }, [lastDone]); // eslint-disable-line react-hooks/exhaustive-deps

  const pickTarget = async () => {
    const d = await open({ directory: true, defaultPath: settings?.target_dir || undefined, title: "라이브러리 위치 (예: E:\\사진)" });
    if (typeof d !== "string" || !settings) return;
    const s = { ...settings, target_dir: d };
    setSettings(s);
    await api.setImportSettings(s).catch(() => {});
    setPlan(null);
  };

  const saveRule = async (node: TreeNode, kind: FolderRule["kind"], name: string | null = null) => {
    const next = rules.filter((r) => !(r.volume_id === node.vol && r.rel_dir === node.from_dir));
    next.push({ volume_id: node.vol, rel_dir: node.from_dir, kind, name });
    setRules(next);
    await api.setLibraryRules(next);
    setRenaming(null);
    preview();
  };

  const run = async (groups?: string[]) => {
    if (!plan) return;
    const s = plan.summary;
    const text = groups
      ? `「${groups[0]}」 폴더만 먼저 정리합니다. 결과를 확인한 뒤 나머지를 실행하세요.`
      : [
          `라이브러리(${settings?.target_dir})로 모읍니다.`,
          `· ${drive} 안에서 옮김 ${fmtCount(s.moves)}개`,
          s.copies ? `· 다른 드라이브에서 복사 ${fmtCount(s.copies)}개 (${fmtBytes(s.copy_bytes)}) — 원래 자리는 그대로` : "",
          s.remove ? `· 라이브러리 드라이브 안의 똑같은 사본 ${fmtCount(s.remove)}개 정리 (${fmtBytes(s.remove_bytes)}, 격리)` : "",
          "",
          "모든 작업은 「작업 기록」에서 한 번에 되돌릴 수 있습니다.",
        ].filter(Boolean).join("\n");
    if (!(await confirm(text, { title: "라이브러리로 모으기", kind: "warning", okLabel: "실행", cancelLabel: "취소" }))) return;
    try {
      await api.startExecute(plan.id, groups);
      setRunning("organize:run");
    } catch (e) {
      setErr(String(e));
    }
  };

  // year / area -> nodes
  const sections = useMemo(() => {
    const m = new Map<string, TreeNode[]>();
    for (const n of tree) {
      const top = n.group.split("/")[0] || "(라이브러리 바로 아래)";
      (m.get(top) ?? m.set(top, []).get(top)!).push(n);
    }
    const keys = [...m.keys()].sort((a, b) => Number(a in AREAS) - Number(b in AREAS) || b.localeCompare(a));
    return keys.map((k) => ({ key: k, nodes: m.get(k)!, count: m.get(k)!.reduce((a, n) => a + n.count, 0), todo: m.get(k)!.reduce((a, n) => a + n.moves + n.copies, 0) }));
  }, [tree]);
  const toggle = (k: string) => setOpen((o) => { const n = new Set(o); n.has(k) ? n.delete(k) : n.add(k); return n; });
  const s = plan?.summary;

  return (
    <div className="library-plan">
      <div className="form-grid">
        <label>라이브러리 위치</label>
        <div className="inline-form">
          <span className="mono grow">{settings?.target_dir || "아직 고르지 않음"}</span>
          <button className="btn" onClick={pickTarget}>바꾸기</button>
        </div>
        <label>구조</label>
        <div className="muted small">
          <span className="mono">연도/연-월/이벤트</span> — 이벤트 이름은 원래 폴더 이름을 따릅니다 (앞의 날짜는 뗌).
          여러 해에 걸친 모음 폴더는 날짜별로 나눕니다. 낱장은 여행이면 여행 폴더, 아니면 연/월 바로 아래.
          RAW는 JPG 아래 <span className="mono">RAW</span> 폴더로, 받은 사진은 <span className="mono">받은 사진/</span>, 큰 동영상은 <span className="mono">큰 동영상/</span>으로.
        </div>
        <label>사본</label>
        <div className="col">
          <label className="check-row">
            <input type="checkbox" checked={lib.remove_copies} onChange={(e) => { setLib({ ...lib, remove_copies: e.target.checked }); setPlan(null); }} />
            라이브러리 드라이브 안의 똑같은 사본은 하나만 남기고 정리 (다른 드라이브는 그대로)
          </label>
          <label className="check-row">
            <input type="checkbox" checked={lib.remove_smaller_versions} onChange={(e) => { setLib({ ...lib, remove_smaller_versions: e.target.checked }); setPlan(null); }} />
            원본이 있는 축소본·재압축본도 정리 (편집본은 원본 옆에 남김)
          </label>
        </div>
        <label>큰 동영상</label>
        <div className="inline-form">
          <input type="number" min={1} step={0.5} style={{ width: 80 }} value={lib.big_video_bytes / GB}
            onChange={(e) => { setLib({ ...lib, big_video_bytes: Math.max(0.1, Number(e.target.value)) * GB }); setPlan(null); }} />
          <span className="muted small">GB 이상은 「큰 동영상」 폴더에 따로</span>
        </div>
      </div>
      {err && <div className="alert bad">{err}</div>}
      {result && (
        <div className={`alert ${result.failed ? "bad" : "ok"}`}>
          완료: 옮김·복사 {fmtCount(result.done)}개 · 함께 옮긴 xmp/aae 등 {fmtCount(result.sidecars ?? 0)}개
          {result.removed ? ` · 사본 정리 ${fmtCount(result.removed)}개 (${fmtBytes(result.removed_bytes)})` : ""}
          {result.failed ? ` · 실패 ${result.failed}개` : ""} — 「작업 기록」에서 되돌릴 수 있습니다.
          {result.errors?.length > 0 && <details><summary>오류 보기</summary><pre>{result.errors.join("\n")}</pre></details>}
        </div>
      )}

      <div className="btn-row">
        <button className="btn primary" disabled={!!running} onClick={() => preview()}>
          {plan ? "다시 미리보기" : "미리보기 (아무것도 바뀌지 않음)"}
        </button>
        {planning && <span className="muted small">계획을 세우는 중…</span>}
      </div>

      {s && plan && (
        <div className="plan">
          <div className="plan-stats">
            <div className="stat"><b>{fmtCount(s.moves)}</b><span>{drive} 안에서 옮김</span></div>
            <div className="stat"><b>{fmtCount(s.copies)}</b><span>다른 드라이브에서 복사 ({fmtBytes(s.copy_bytes)})</span></div>
            <div className="stat"><b>{fmtCount(s.remove)}</b><span>{drive} 사본 정리 ({fmtBytes(s.remove_bytes)})</span></div>
            <div className="stat"><b>{fmtCount(s.leave)}</b><span>다른 드라이브 사본 그대로</span></div>
            <div className="stat"><b>{fmtCount(s.in_place)}</b><span>이미 제자리</span></div>
            <div className="stat"><b>{fmtCount(s.events)}</b><span>이벤트 폴더</span></div>
            <div className="stat"><b>{fmtCount(s.paired)}</b><span>RAW·Live Photo·편집본 짝</span></div>
            <div className="stat"><b>{fmtCount(s.received)}</b><span>받은 사진</span></div>
            <div className="stat"><b>{fmtCount(s.big_videos)}</b><span>큰 동영상</span></div>
            <div className="stat"><b>{fmtCount(s.uncertain_dates)}</b><span>날짜 확인 필요</span></div>
            {s.skip_offline > 0 && <div className="stat warn"><b>{fmtCount(s.skip_offline)}</b><span>드라이브 연결 안 됨</span></div>}
          </div>
          {s.leftover_files > 0 && (
            <div className="alert">사진·동영상이 아닌 파일 {fmtCount(s.leftover_files)}개(문서 등)는 원래 폴더에 그대로 남습니다.</div>
          )}

          <div className="lib-tree">
            {sections.map((sec) => (
              <div key={sec.key} className="lib-sec">
                <button className="lib-sec-head" onClick={() => toggle(sec.key)}>
                  <span>{open_.has(sec.key) ? "▾" : "▸"}</span>
                  <b>{AREAS[sec.key] ?? sec.key}</b>
                  <span className="muted small">{fmtCount(sec.count)}개{sec.todo ? ` · 옮길 것 ${fmtCount(sec.todo)}` : " · 정리됨"}</span>
                </button>
                {open_.has(sec.key) && sec.nodes.map((n) => {
                  const rest = n.group.split("/").slice(1).join("/") || "(바로 아래)";
                  const isEvent = n.tag === "event" && n.from_dir;
                  return (
                    <div key={n.group} className={`lib-node ${n.moves + n.copies === 0 ? "done" : ""}`}>
                      <div className="lib-node-main">
                        {renaming?.node.group === n.group ? (
                          <span className="inline-form">
                            <input autoFocus value={renaming.name} onChange={(e) => setRenaming({ node: n, name: e.target.value })}
                              onKeyDown={(e) => { if (e.key === "Enter") saveRule(n, "event", renaming.name.trim() || null); if (e.key === "Escape") setRenaming(null); }} />
                            <button className="btn small primary" onClick={() => saveRule(n, "event", renaming.name.trim() || null)}>저장</button>
                          </span>
                        ) : (
                          <span className="mono">{rest}</span>
                        )}
                        {n.tag && <span className="pill">{TAGS[n.tag] ?? n.tag}</span>}
                        <span className="muted small">
                          {fmtCount(n.count)}개
                          {n.moves ? ` · 옮김 ${fmtCount(n.moves)}` : ""}{n.copies ? ` · 복사 ${fmtCount(n.copies)}` : ""}{n.in_place ? ` · 제자리 ${fmtCount(n.in_place)}` : ""}
                        </span>
                        <span className="spacer" />
                        {isEvent && (
                          <>
                            <button className="btn small ghost" onClick={() => setRenaming({ node: n, name: n.event ?? "" })}>이름 바꾸기</button>
                            <button className="btn small ghost" onClick={() => saveRule(n, "loose")} title="이벤트로 묶지 않고 날짜별 낱장으로">낱장으로</button>
                            <button className="btn small ghost" onClick={() => saveRule(n, "skip")} title="이 폴더는 옮기지 않음">빼기</button>
                          </>
                        )}
                        {n.moves + n.copies > 0 && (
                          <button className="btn small" disabled={!!running} onClick={() => run([n.group])}>이 폴더만 먼저</button>
                        )}
                      </div>
                      {n.sources.length > 0 && (
                        <div className="muted small mono lib-src" title={n.sources.map(([p, c]) => `${p} (${c})`).join("\n")}>
                          ← {n.sources[0][0]}{n.sources.length > 1 ? ` 외 ${n.sources.length - 1}곳` : ""}
                        </div>
                      )}
                    </div>
                  );
                })}
              </div>
            ))}
          </div>

          {removes.length > 0 && (
            <details className="lib-removes">
              <summary>정리될 {drive} 사본 {fmtCount(s.remove)}개 보기 (남는 쪽은 라이브러리로 옮겨짐)</summary>
              <ul className="mono small">
                {removes.map((r) => <li key={r.file_id}>{r.src}{r.tag === "version" ? " (축소본·재압축본)" : ""}</li>)}
              </ul>
              {s.remove > removes.length && <div className="muted small">처음 {removes.length}개만 표시합니다.</div>}
            </details>
          )}
          {rules.length > 0 && (
            <div className="muted small">
              폴더 고침 {rules.length}개 기억 중 ·{" "}
              <button className="link" onClick={async () => { setRules([]); await api.setLibraryRules([]); preview(); }}>모두 지우기</button>
            </div>
          )}

          <div className="btn-row">
            <button className="btn primary" disabled={!!running || s.moves + s.copies + s.remove === 0} onClick={() => run()}>
              실행: 옮김 {fmtCount(s.moves)} · 복사 {fmtCount(s.copies)}{s.remove ? ` · 사본 정리 ${fmtCount(s.remove)}` : ""}
            </button>
            <span className="muted small">멈추고 싶으면 아래 진행 막대의 「취소」 — 끝난 것까지는 그대로 남고, 다시 미리보기하면 남은 것만 나옵니다.</span>
          </div>
        </div>
      )}
    </div>
  );
}
