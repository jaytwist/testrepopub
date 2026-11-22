import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  absPath, api, fileUrl, fmtBytes, fmtCount, fmtDate, fmtDuration, thumbUrl, VARIANT_NAMES,
  type TriageGroup, type TriageKind, type TriageMember,
} from "../api";
import { useJob } from "../App";
import { confirmDelete } from "../deleteFlow";

const PAGE = 100;
const folderOf = (p: string) => p.slice(0, Math.max(p.lastIndexOf("/"), p.lastIndexOf("\\"))) || p;

/** What a review decided: kept files per group, and the files not kept (to delete). */
export interface ReviewResult {
  decisions: { key: string; keep: number[] }[];
  drops: { id: number; size: number; key: string }[];
}

interface Props {
  kind: TriageKind;
  /** Similar photos: only groups with resized/recompressed ("safe") or edited versions. */
  variant?: "safe" | "edited";
  title: string;
  onClose: () => void;
  /** Year mode (「한 곳으로 정리」): only that year's groups; nothing is deleted here — the result
   *  goes back to the year's delete list (deleted when the year is pushed). */
  source?: { planId: string; year: string };
  onFinish?: (r: ReviewResult) => void;
  /** Year mode: decisions made earlier, to go on from. */
  initial?: Map<string, Set<number>>;
}

/**
 * Quick review, one group at a time: the suggested keeper is pre-chosen; ← → / 1–9 pick another,
 * a click keeps several, Space confirms and moves on, S keeps all, Z goes back, Esc finishes.
 * Decisions are remembered; the dropped files are removed together at the end (one confirmation,
 * protected files stay, undoable from 「작업 기록」).
 */
export default function Triage({ kind, variant, title, onClose, source, onFinish, initial }: Props) {
  const { running, setRunning } = useJob();
  const [groups, setGroups] = useState<TriageGroup[] | null>(null);
  const [more, setMore] = useState(true);
  const [idx, setIdx] = useState(0);
  const [keep, setKeep] = useState<Map<string, Set<number>>>(() => new Map(initial ?? []));
  const [done, setDone] = useState<string[]>(() => [...(initial?.keys() ?? [])]);
  const resumed = useRef(false);
  const [srcs, setSrcs] = useState<Record<number, string>>({});
  const [err, setErr] = useState<string | null>(null);
  const loading = useRef(false);

  const loadMore = useCallback(async () => {
    if (loading.current || !more) return;
    loading.current = true;
    try {
      const page = source
        ? await api.yearTriageGroups(source.planId, source.year, kind, groups?.length ?? 0, PAGE)
        : await api.triageGroups(kind, variant ?? null, groups?.length ?? 0, PAGE);
      setGroups((g) => [...(g ?? []), ...page]);
      if (page.length < PAGE) setMore(false);
    } catch (e) {
      setErr(String(e));
      setMore(false);
    } finally {
      loading.current = false;
    }
  }, [kind, variant, groups?.length, more]);

  useEffect(() => {
    if (groups == null || (more && idx > groups.length - 10)) loadMore();
  }, [idx, groups, more, loadMore]);

  // Going on from earlier decisions: start at the first group not decided yet.
  useEffect(() => {
    if (resumed.current || !groups || !initial?.size) return;
    resumed.current = true;
    const first = groups.findIndex((gr) => !initial.has(gr.key));
    if (first > 0) setIdx(first);
  }, [groups]); // eslint-disable-line react-hooks/exhaustive-deps

  const g = groups?.[idx];
  // The suggestion; never "keep none" — if the stored suggestion keeps nothing, the first
  // (recommended) photo is kept.
  const keepOf = useCallback(
    (gr: TriageGroup) => {
      const s = keep.get(gr.key) ?? new Set(gr.members.filter((m) => m.keep).map((m) => m.file.id));
      return s.size === 0 && gr.members.length > 0 ? new Set([gr.members[0].file.id]) : s;
    },
    [keep],
  );
  const setKeepOf = (gr: TriageGroup, s: Set<number>) => setKeep((k) => new Map(k).set(gr.key, s));

  // Pictures: quick thumbnail first, then the full preview, for this group and the next.
  useEffect(() => {
    for (const gr of [groups?.[idx], groups?.[idx + 1]]) {
      for (const m of gr?.members ?? []) {
        const id = m.file.id;
        if (srcs[id] || m.file.kind === "video") continue;
        api.previewPath(id).then((p) => setSrcs((s) => ({ ...s, [id]: fileUrl(p) }))).catch(() => {});
      }
    }
  }, [idx, groups]); // eslint-disable-line react-hooks/exhaustive-deps

  /** Keep only member `i` (plus backups, which always stay). */
  const only = (i: number) => {
    if (!g || i < 0 || i >= g.members.length) return;
    const s = new Set(g.members.filter((m) => m.locked).map((m) => m.file.id));
    s.add(g.members[i].file.id);
    setKeepOf(g, s);
  };
  /** Click: keep several (at least one stays). */
  const toggle = (m: TriageMember) => {
    if (!g || m.locked) return;
    const s = new Set(keepOf(g));
    if (s.has(m.file.id)) {
      if (s.size === 1) return;
      s.delete(m.file.id);
    } else s.add(m.file.id);
    setKeepOf(g, s);
  };
  const next = (keepAll = false) => {
    if (!g) return;
    const chosen = keepAll ? new Set(g.members.map((m) => m.file.id)) : keepOf(g);
    // A group is never emptied here.
    if (chosen.size === 0) return setErr("한 장은 남겨야 합니다.");
    setErr(null);
    setKeepOf(g, chosen);
    setDone((d) => (d.includes(g.key) ? d : [...d, g.key]));
    setIdx((i) => i + 1);
  };
  const back = () => {
    if (idx === 0 || !groups) return;
    const prev = groups[idx - 1].key;
    setDone((d) => d.filter((k) => k !== prev));
    setIdx(idx - 1);
  };

  // What the confirmed decisions remove so far.
  const drops = useMemo(() => {
    const byKey = new Map((groups ?? []).map((gr) => [gr.key, gr]));
    return done.flatMap((k) => {
      // A decision from an earlier visit whose group is not loaded (or no longer exists) is left
      // as it was.
      const gr = byKey.get(k);
      const s = keep.get(k);
      if (!gr || !s || s.size === 0) return [];
      return gr.members.filter((m) => !s.has(m.file.id) && !m.locked).map((m) => m.file);
    });
  }, [done, keep, groups]);
  const dropBytes = drops.reduce((a, f) => a + f.size, 0);

  const finish = async () => {
    if (onFinish) {
      const byKey = new Map((groups ?? []).map((gr) => [gr.key, gr]));
      // Only groups seen in this visit (loaded, with a photo kept): earlier decisions on groups
      // not loaded now stay as they are in the year's list.
      const seen = done.filter((k) => byKey.has(k) && (keep.get(k)?.size ?? 0) > 0);
      onFinish({
        decisions: seen.map((k) => ({ key: k, keep: [...keep.get(k)!] })),
        drops: seen.flatMap((k) => {
          const s = keep.get(k)!;
          return byKey.get(k)!.members.filter((m) => !s.has(m.file.id) && !m.locked).map((m) => ({ id: m.file.id, size: m.file.size, key: k }));
        }),
      });
      onClose();
      return;
    }
    try {
      const decided = done.filter((k) => (keep.get(k)?.size ?? 0) > 0);
      if (decided.length > 0) await api.triageCommit(decided.map((k) => ({ key: k, keep: [...keep.get(k)!] })));
      if (drops.length > 0 && !running && (await confirmDelete(drops.map((f) => f.id), title, dropBytes, title))) setRunning("delete");
      onClose();
    } catch (e) {
      setErr(String(e));
    }
  };

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.target as HTMLElement)?.tagName === "INPUT") return;
      const k = e.key;
      if (k === "Escape") return void finish();
      if (!g) return;
      const kept = keepOf(g);
      const cur = g.members.findIndex((m) => !m.locked && kept.has(m.file.id));
      if (k === "ArrowRight") only(Math.min(g.members.length - 1, cur + 1));
      else if (k === "ArrowLeft") only(Math.max(0, cur - 1));
      else if (/^Digit[1-9]$/.test(e.code)) {
        const i = Number(e.code.slice(5)) - 1;
        if (e.shiftKey && g.members[i]) toggle(g.members[i]);
        else only(i);
      } else if (k === " " || k === "Enter") next();
      else if (k === "s" || k === "S" || k === "ㄴ") next(true);
      else if (k === "z" || k === "Z" || k === "ㅋ") back();
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  });

  const total = groups?.length ?? 0;
  const maxSharp = Math.max(0, ...(g?.members.map((m) => m.sharpness ?? 0) ?? [0]));
  // Values that differ between the photos stand out.
  const differs = (f: (m: TriageMember) => string) => g != null && new Set(g.members.map(f)).size > 1;
  const res = (m: TriageMember) => (m.file.width ? `${m.file.width} × ${m.file.height}` : "—");
  const size = (m: TriageMember) => fmtBytes(m.file.size);
  const date = (m: TriageMember) => fmtDate(m.file.taken_at ?? m.file.mtime, true);
  const folder = (m: TriageMember) => folderOf(absPath(m.file));

  return (
    <div className="triage">
      <header className="cmp-head">
        <b>{title}</b>
        {groups && total > 0 && (
          <span className="tri-progress">
            <span className="tri-bar"><span style={{ width: `${Math.min(100, (100 * Math.min(idx, total)) / total)}%` }} /></span>
            {fmtCount(Math.min(idx + 1, total))} / {fmtCount(total)}{more ? "+" : ""} 묶음
          </span>
        )}
        <span className="tri-drop">지울 것 <b>{fmtCount(drops.length)}</b>개 · {fmtBytes(dropBytes)}</span>
        <span className="spacer" />
        <button className="btn primary" onClick={finish}>
          {source
            ? drops.length > 0 ? `끝내기 — 지울 목록에 ${fmtCount(drops.length)}장` : "끝내기"
            : drops.length > 0 ? `끝내고 ${fmtCount(drops.length)}개 정리` : "끝내기"} <kbd>Esc</kbd>
        </button>
      </header>
      {err && <div className="alert bad">{err}</div>}

      {groups == null ? (
        <div className="empty"><p>불러오는 중…</p></div>
      ) : !g ? (
        <div className="empty">
          <div className="empty-icon">✓</div>
          <h2>{total === 0 ? "검토할 것이 없습니다" : "모두 봤습니다"}</h2>
          {drops.length > 0 && <p>지울 것 {fmtCount(drops.length)}개 ({fmtBytes(dropBytes)}). 「끝내고 정리」를 누르면 한 번에 정리합니다.</p>}
          {idx > 0 && <button className="btn" onClick={back}>← 이전 묶음 <kbd>Z</kbd></button>}
        </div>
      ) : (
        <div className="tri-row">
          {g.members.map((m, i) => {
            const kept = keepOf(g).has(m.file.id);
            const isVideo = m.file.kind === "video";
            return (
              <div key={m.file.id} className={`tri-card ${kept ? "keep" : "drop"} ${m.locked ? "locked" : ""}`}
                onClick={() => toggle(m)}
                title={m.locked ? (m.role === "backup" ? "다른 드라이브의 백업: 지우지 않습니다" : m.role === "other_year" ? "다른 해의 사진: 그 해를 정리할 때 검토합니다" : m.role === "outside" ? "작업 범위 밖: 남는 쪽이며 여기서 지우지 않습니다" : m.role === "filed" ? "이미 라이브러리에 정리된 사진: 남습니다" : m.role === "original" ? "편집본의 원본: 항상 남깁니다" : "편집본: 항상 남깁니다") : "누르면 남기기/지우기"}>
                <div className="tri-img">
                  <img src={srcs[m.file.id] ?? thumbUrl(m.file.id)} alt="" draggable={false}
                    onError={(e) => ((e.target as HTMLImageElement).src = thumbUrl(m.file.id))} />
                  <span className="tri-num">{i + 1}</span>
                  {isVideo && <span className="badge-video">▶ {fmtDuration(m.file.duration_ms)}</span>}
                  <span className={`tri-state ${kept ? "keep" : "drop"}`}>{kept ? "남기기" : "지우기"}</span>
                </div>
                <div className="cmp-tags">
                  {i === 0 && <span className="badge-best">★ 추천</span>}
                  {m.role === "backup" && <span className="role-tag backup">백업 · 지우지 않음</span>}
                  {m.role === "other_year" && <span className="role-tag backup">다른 해 · 그 해에서 검토</span>}
                  {m.role === "outside" && <span className="role-tag backup">범위 밖 · 남음</span>}
                  {m.role === "filed" && <span className="role-tag backup">라이브러리에 있음 · 남음</span>}
                  {m.role === "original" && <span className="role-tag original">원본 · 남음</span>}
                  {m.variant && <span className="role-tag variant">{VARIANT_NAMES[m.variant]}</span>}
                  {kind === "burst" && m.sharpness != null && maxSharp > 0 && (
                    <span className="muted small">선명도 {Math.round((100 * m.sharpness) / maxSharp)}%</span>
                  )}
                </div>
                <div className="tri-meta">
                  <span className={differs(res) ? "diff" : ""}>{res(m)}</span>
                  <span className={differs(size) ? "diff" : ""}>{size(m)}</span>
                  <span className={differs(date) ? "diff" : ""}>{date(m)}</span>
                  <span className={`path-tail ${differs(folder) ? "diff" : ""}`} title={absPath(m.file)}><bdi>{folder(m)}</bdi></span>
                  {!m.file.online && <span className="warn-text">드라이브 연결 안 됨</span>}
                </div>
              </div>
            );
          })}
        </div>
      )}

      <footer className="cmp-foot tri-keys">
        <span><kbd>←</kbd><kbd>→</kbd> 남길 사진 고르기</span>
        <span><kbd>1</kbd>–<kbd>9</kbd> 바로 고르기</span>
        <span>클릭: 여러 장 남기기</span>
        <span><kbd>Space</kbd> 확정·다음</span>
        <span><kbd>S</kbd> 모두 남기기</span>
        <span><kbd>Z</kbd> 이전 묶음</span>
        <span className="spacer" />
        <span className="muted small">
          {source
            ? "남기지 않은 사진은 이 해의 「지울 목록」에 들어가고, Push할 때 영구 삭제됩니다"
            : "지운 파일은 격리 폴더로 옮겨지며 「작업 기록」에서 되돌릴 수 있습니다"}
        </span>
      </footer>
    </div>
  );
}
