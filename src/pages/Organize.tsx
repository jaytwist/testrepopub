import { useEffect, useState } from "react";
import { open, confirm } from "@tauri-apps/plugin-dialog";
import { api, fmtBytes, fmtCount, type FileFilter, type FilteredStats, type OrganizeOptions, type PlanItem, type PlanView } from "../api";
import { useJob } from "../App";
import FilterBar from "../components/FilterBar";

const PRESETS = [
  { tpl: "{year}/{year}-{month}", name: "연도 / 연-월", example: "2019/2019-05/IMG_0001.jpg" },
  { tpl: "{year}/{date}", name: "연도 / 날짜", example: "2019/2019-05-12/IMG_0001.jpg" },
  { tpl: "{kind}/{year}/{year}-{month}", name: "종류 / 연도 / 연-월", example: "사진/2019/2019-05/…, 동영상/…, 스크린샷/…" },
  { tpl: "{year}/{year}-{month} {label}", name: "연-월 + 라벨", example: "2019/2019-05 가족여행/…" },
  { tpl: "{source}/{year}", name: "출처 / 연도", example: "카메라/2019, 카카오톡/2021, 다운로드/…" },
  { tpl: "{year}/{trip}", name: "연도 / 여행", example: "2024/2024년 6월 Honolulu 여행/…, 여행 아닌 사진은 2024/" },
];

const ACTION_NAMES: Record<string, string> = {
  move: "이동", copy: "복사", in_place: "이미 정리됨", skip_duplicate: "중복이라 건너뜀", skip_offline: "드라이브 연결 안 됨",
};

export default function Organize({ preset }: { preset?: { key: number; filter: FileFilter } | null }) {
  const { running, setRunning, lastDone } = useJob();
  const [opts, setOpts] = useState<OrganizeOptions>({
    target_dir: "",
    template: PRESETS[0].tpl,
    rename: "keep",
    mode: "move",
    filter: preset?.filter ?? {},
    uncertain_folder: "_날짜확인필요",
    remove_empty_dirs: true,
  });
  const [plan, setPlan] = useState<PlanView | null>(null);
  const [items, setItems] = useState<PlanItem[]>([]);
  const [tab, setTab] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<any>(null);
  const [target, setTarget] = useState<FilteredStats | null>(null);

  useEffect(() => {
    api.filteredStats(opts.filter).then(setTarget).catch(() => setTarget(null));
  }, [opts.filter]);
  // Opened for a set of files (e.g. the library selection).
  useEffect(() => {
    if (preset) setOpts((o) => ({ ...o, filter: preset.filter }));
  }, [preset?.key]); // eslint-disable-line react-hooks/exhaustive-deps

  // Pick up results of our jobs.
  useEffect(() => {
    if (!lastDone) return;
    if (lastDone.job === "organize:plan" && lastDone.ok) {
      setPlan(lastDone.result);
      setTab(null);
    }
    if (lastDone.job === "organize:run") {
      setPlan(null);
      setItems([]);
      if (lastDone.ok) setResult(lastDone.result);
    }
  }, [lastDone]);

  useEffect(() => {
    if (plan) api.planItems(plan.id, tab, 0, 300).then(setItems).catch(() => setItems([]));
  }, [plan, tab]);

  const set = (p: Partial<OrganizeOptions>) => {
    setOpts({ ...opts, ...p });
    setPlan(null);
  };

  const pickTarget = async () => {
    const d = await open({ directory: true, title: "정리된 사진을 모을 폴더 (예: D:\\사진 라이브러리)" });
    if (typeof d === "string") set({ target_dir: d });
  };

  const preview = async () => {
    setError(null);
    setResult(null);
    try {
      await api.startPlan(opts);
      setRunning("organize:plan");
    } catch (e) {
      setError(String(e));
    }
  };

  const execute = async () => {
    if (!plan) return;
    const s = plan.summary;
    const n = s.moves + s.copies;
    const ok = await confirm(
      `${fmtCount(n)}개 파일(${fmtBytes(s.bytes)})을 ${opts.mode === "move" ? "이동" : "복사"}합니다.\n` +
        (s.cross_volume ? `다른 드라이브로 가는 ${fmtCount(s.cross_volume)}개는 복사 후 내용을 검증하고 원본을 지웁니다.\n` : "") +
        `\n모든 작업은 「작업 기록」에서 되돌릴 수 있습니다.`,
      { title: "정리 실행", kind: "warning" },
    );
    if (!ok) return;
    try {
      await api.startExecute(plan.id);
      setRunning("organize:run");
    } catch (e) {
      setError(String(e));
    }
  };

  const s = plan?.summary;
  const tabs: [string | null, string, number][] = s
    ? [
        [null, "전체", s.total],
        [opts.mode, opts.mode === "move" ? "이동" : "복사", opts.mode === "move" ? s.moves : s.copies],
        ["skip_duplicate", "중복 건너뜀", s.skip_duplicate],
        ["in_place", "이미 정리됨", s.in_place],
        ["skip_offline", "연결 안 됨", s.skip_offline],
      ]
    : [];

  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>한 곳으로 정리</h1>
          <div className="muted">흩어진 사진·동영상을 하나의 폴더에 날짜별로 모읍니다. 먼저 미리보기로 결과를 확인한 뒤 실행하세요.</div>
        </div>
      </header>
      {error && <div className="alert bad">{error}</div>}
      {result && (
        <div className={`alert ${result.failed ? "bad" : "ok"}`}>
          완료: {fmtCount(result.done)}개 · {fmtBytes(result.bytes)}{result.failed ? ` · 실패 ${result.failed}개` : ""}{result.cancelled ? " · 중간에 취소됨" : ""}
          {result.errors?.length > 0 && <details><summary>오류 보기</summary><pre>{result.errors.join("\n")}</pre></details>}
        </div>
      )}

      <div className="form-grid">
        <label>모을 폴더</label>
        <div className="inline-form">
          <input className="grow mono" placeholder="예: D:\사진 라이브러리" value={opts.target_dir} onChange={(e) => set({ target_dir: e.target.value })} />
          <button className="btn" onClick={pickTarget}>찾아보기</button>
        </div>

        <label>폴더 구조</label>
        <div>
          <div className="presets">
            {PRESETS.map((p) => (
              <button key={p.tpl} className={`preset ${opts.template === p.tpl ? "on" : ""}`} onClick={() => set({ template: p.tpl })}>
                <b>{p.name}</b>
                <span className="mono small muted">{p.example}</span>
              </button>
            ))}
          </div>
          <div className="inline-form">
            <input className="grow mono" value={opts.template} onChange={(e) => set({ template: e.target.value })} />
          </div>
          <div className="muted small">사용 가능: {"{year} {month} {day} {date} {kind} {camera} {source} {label} {trip}"}</div>
        </div>

        <label>파일 이름</label>
        <div className="seg">
          <button className={opts.rename === "keep" ? "on" : ""} onClick={() => set({ rename: "keep" })}>원래 이름 유지</button>
          <button className={opts.rename === "date_prefix" ? "on" : ""} onClick={() => set({ rename: "date_prefix" })}>날짜_시간_원래이름</button>
        </div>

        <label>방식</label>
        <div className="seg">
          <button className={opts.mode === "move" ? "on" : ""} onClick={() => set({ mode: "move" })}>이동 (원본 위치에서 옮김)</button>
          <button className={opts.mode === "copy" ? "on" : ""} onClick={() => set({ mode: "copy" })}>복사 (원본 유지)</button>
        </div>

        <label>날짜 불확실</label>
        <label className="check-row">
          <input type="checkbox" checked={opts.uncertain_folder != null} onChange={(e) => set({ uncertain_folder: e.target.checked ? "_날짜확인필요" : null })} />
          촬영 날짜를 확신할 수 없는 파일은 <span className="mono">_날짜확인필요</span> 폴더에 따로 모으기
        </label>

        <label>빈 폴더</label>
        <label className="check-row">
          <input type="checkbox" checked={opts.remove_empty_dirs} onChange={(e) => set({ remove_empty_dirs: e.target.checked })} />
          이동 후 비게 된 원래 폴더 지우기
        </label>

        <label>대상 파일</label>
        <div>
          {opts.filter.ids ? (
            <div className="alert ok selection-only">
              라이브러리에서 고른 <b>{opts.filter.ids.length}장</b>만 정리합니다.
              <button className="btn small ghost" onClick={() => set({ filter: {} })}>해제하고 필터로 고르기</button>
            </div>
          ) : (
            <FilterBar filter={opts.filter} onChange={(f: FileFilter) => set({ filter: f })} showSort={false} />
          )}
          <div className="muted small">
            {target && <><b className="text">대상 {fmtCount(target.count)}개 · {fmtBytes(target.bytes)}</b> · </>}
            필터를 비워두면 연결된 모든 드라이브의 모든 사진·동영상이 대상입니다.
          </div>
        </div>
      </div>

      <div className="btn-row">
        <button className="btn primary" disabled={!!running || !opts.target_dir} onClick={preview}>미리보기 (아무것도 바뀌지 않음)</button>
      </div>

      {s && plan && (
        <div className="plan">
          <div className="plan-stats">
            <div className="stat"><b>{fmtCount(s.moves + s.copies)}</b><span>{opts.mode === "move" ? "이동" : "복사"}할 파일</span></div>
            <div className="stat"><b>{fmtBytes(s.bytes)}</b><span>용량</span></div>
            <div className="stat"><b>{fmtCount(s.folders)}</b><span>폴더</span></div>
            <div className="stat"><b>{fmtCount(s.skip_duplicate)}</b><span>중복이라 건너뜀</span></div>
            <div className="stat"><b>{fmtCount(s.renamed)}</b><span>이름 충돌 → 번호 붙임</span></div>
            <div className="stat"><b>{fmtCount(s.uncertain_dates)}</b><span>날짜 확인 필요</span></div>
            {s.skip_offline > 0 && <div className="stat warn"><b>{fmtCount(s.skip_offline)}</b><span>드라이브 연결 안 됨</span></div>}
          </div>
          <div className="seg">
            {tabs.map(([a, name, n]) => (
              <button key={name} className={tab === a ? "on" : ""} onClick={() => setTab(a)}>{name} {fmtCount(n)}</button>
            ))}
          </div>
          <table className="table plan-table">
            <thead><tr><th>현재 위치</th><th /><th>정리 후 위치</th><th>처리</th></tr></thead>
            <tbody>
              {items.map((it) => (
                <tr key={it.file_id}>
                  <td className="mono small">{it.src}</td>
                  <td>→</td>
                  <td className="mono small">{it.dst}{it.renamed && <span className="pill">번호 붙임</span>}</td>
                  <td className="small">{ACTION_NAMES[it.action]}{it.cross_volume && (it.action === "move" || it.action === "copy") ? " · 다른 드라이브" : ""}</td>
                </tr>
              ))}
            </tbody>
          </table>
          {items.length >= 300 && <div className="muted small">처음 300개만 표시합니다.</div>}
          <div className="btn-row">
            <button className="btn primary" disabled={!!running || s.moves + s.copies === 0} onClick={execute}>
              실행: {fmtCount(s.moves + s.copies)}개 {opts.mode === "move" ? "이동" : "복사"}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
