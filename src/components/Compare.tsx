import { useEffect, useMemo, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  api, DATE_SOURCE_NAMES, fileUrl, fmtBytes, fmtCount, fmtDate, fmtDuration, labelName, thumbUrl,
  type CompareInfo, type FileDetail,
} from "../api";
import { useJob } from "../App";

const sep = (p: string) => (p.includes("\\") ? "\\" : "/");
const folderOf = (p: string) => p.slice(0, p.lastIndexOf(sep(p))) || p;

interface Field {
  label: string;
  value: (d: FileDetail) => string;
  /** Clicking the value does something (e.g. show the folder in Explorer). */
  onClick?: (d: FileDetail) => void;
}

/**
 * 2–4 files side by side: big pictures (click to zoom all at the same spot), a row-aligned table
 * where differing values stand out, which files are byte-identical, and keep / delete per file
 * (pre-set to keep only the recommended one). Deleting uses the normal quarantine + undo.
 */
export default function Compare({ ids, onClose }: { ids: number[]; onClose: () => void }) {
  const { running, setRunning } = useJob();
  const [info, setInfo] = useState<CompareInfo | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [srcs, setSrcs] = useState<Record<number, string>>({});
  const [keep, setKeep] = useState<Set<number>>(new Set());
  const [zoom, setZoom] = useState<{ x: number; y: number } | null>(null);

  useEffect(() => {
    api
      .compareFiles(ids)
      .then((c) => {
        setInfo(c);
        setKeep(new Set(c.recommended != null ? [c.recommended] : ids));
      })
      .catch((e) => setErr(String(e)));
    for (const id of ids) {
      api.previewPath(id).then((p) => setSrcs((s) => ({ ...s, [id]: fileUrl(p) }))).catch(() => setSrcs((s) => ({ ...s, [id]: thumbUrl(id) })));
    }
  }, [ids.join(",")]); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  const files = info?.files ?? [];
  const anyVideo = files.some((d) => d.file.kind === "video");
  const fields: Field[] = useMemo(
    () => [
      // Where each copy lives first: that is what decides which one to delete.
      { label: "폴더", value: (d) => folderOf(d.abs_path), onClick: (d) => d.file.online && api.revealFile(d.file.id) },
      { label: "드라이브", value: (d) => `${d.file.mount} · ${d.file.online ? "연결됨" : "연결 안 됨"}` },
      { label: "파일 이름", value: (d) => d.file.name },
      { label: "촬영 날짜", value: (d) => `${d.taken || "알 수 없음"} · ${DATE_SOURCE_NAMES[d.file.date_source] ?? d.file.date_source}` },
      { label: "파일 수정 시각", value: (d) => fmtDate(d.file.mtime, true) },
      { label: "크기", value: (d) => fmtBytes(d.file.size) },
      { label: "해상도", value: (d) => (d.file.width ? `${d.file.width} × ${d.file.height}` : "—") },
      ...(anyVideo ? [{ label: "길이", value: (d: FileDetail) => (d.file.duration_ms ? fmtDuration(d.file.duration_ms) : "—") }] : []),
      { label: "카메라", value: (d) => d.file.camera ?? "—" },
      { label: "장소", value: (d) => d.place ?? "—" },
      { label: "인물", value: (d) => d.people.map(([, n]) => n).join(", ") || "—" },
      { label: "라벨", value: (d) => d.labels.map((l) => labelName(l.label)).join(", ") || "—" },
    ],
    [anyVideo],
  );

  const sameGroup = (id: number) => info?.identical.findIndex((g) => g.includes(id)) ?? -1;
  const toDelete = files.filter((d) => !keep.has(d.file.id));
  const bytes = toDelete.reduce((a, d) => a + d.file.size, 0);

  const toggle = (id: number) => {
    const next = new Set(keep);
    next.has(id) ? next.delete(id) : next.add(id);
    setKeep(next);
  };

  const remove = async () => {
    if (toDelete.length === 0 || running) return;
    if (keep.size === 0 && !(await confirm("모든 파일을 지우기로 골랐습니다. 하나도 남기지 않을까요?", { title: "모두 삭제", kind: "warning" }))) return;
    const ok = await confirm(
      `${fmtCount(toDelete.length)}개(${fmtBytes(bytes)})를 삭제할까요?\n\n같은 드라이브의 격리 폴더로 옮겨지며, 「되돌리기」나 「작업 기록」에서 복원할 수 있습니다.`,
      { title: "비교한 파일 삭제", kind: "warning", okLabel: "삭제", cancelLabel: "취소" },
    );
    if (!ok) return;
    try {
      await api.startDelete(toDelete.map((d) => d.file.id));
      setRunning("delete");
      onClose();
    } catch (e) {
      setErr(String(e));
    }
  };

  const verdict =
    info == null
      ? ""
      : info.identical.some((g) => g.length === files.length)
        ? "모두 내용이 완전히 같은 파일입니다"
        : info.identical.length > 0
          ? "일부는 내용이 완전히 같은 파일입니다"
          : "비슷해 보여도 서로 다른 파일입니다";

  return (
    <div className="compare">
      <header className="cmp-head">
        <b>{ids.length}장 비교</b>
        {info && <span className={`cmp-verdict ${info.identical.length > 0 ? "same" : ""}`}>{verdict}</span>}
        <span className="muted small">사진을 누르면 모두 같은 곳을 확대합니다 · Esc 닫기</span>
        <span className="spacer" />
        <button className="btn small" onClick={onClose}>닫기</button>
      </header>
      {err && <div className="alert bad">{err}</div>}

      <div className="cmp-grid" style={{ gridTemplateColumns: `140px repeat(${ids.length}, minmax(0, 1fr))` }}>
        <div />
        {ids.map((id) => {
          const d = files.find((x) => x.file.id === id);
          const g = sameGroup(id);
          return (
            <div key={id} className={`cmp-col-head ${keep.has(id) ? "" : "dropping"}`}>
              <div
                className={`cmp-img ${zoom ? "zoomed" : ""}`}
                onClick={(e) => {
                  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
                  setZoom(zoom ? null : { x: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) / r.height });
                }}
                onMouseMove={(e) => {
                  if (!zoom) return;
                  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
                  setZoom({ x: (e.clientX - r.left) / r.width, y: (e.clientY - r.top) / r.height });
                }}
              >
                {srcs[id] && (
                  <img
                    src={d?.file.kind === "video" ? thumbUrl(id) : srcs[id]}
                    alt=""
                    draggable={false}
                    style={zoom ? { transform: "scale(3)", transformOrigin: `${zoom.x * 100}% ${zoom.y * 100}%` } : undefined}
                    onError={(e) => ((e.target as HTMLImageElement).src = thumbUrl(id))}
                  />
                )}
                {d?.file.kind === "video" && <span className="badge-video">▶ {fmtDuration(d.file.duration_ms)}</span>}
              </div>
              <div className="cmp-tags">
                {info?.recommended === id && <span className="badge-best">★ 추천</span>}
                {d?.dup_role && <span className={`role-tag ${d.dup_role}`}>{d.dup_role === "original" ? "원본" : "사본"}</span>}
                {g >= 0 && <span className="cmp-same">내용 같음 {String.fromCharCode(65 + g)}</span>}
              </div>
              <div className="seg cmp-keep">
                <button className={keep.has(id) ? "on" : ""} onClick={() => !keep.has(id) && toggle(id)}>남기기</button>
                <button className={keep.has(id) ? "" : "on danger-on"} onClick={() => keep.has(id) && toggle(id)}>지우기</button>
              </div>
            </div>
          );
        })}

        {fields.map((f) => {
          const values = ids.map((id) => {
            const d = files.find((x) => x.file.id === id);
            return d ? f.value(d) : "";
          });
          const differs = new Set(values).size > 1;
          return (
            <div key={f.label} className="cmp-row" style={{ display: "contents" }}>
              <div className="cmp-label">{f.label}</div>
              {ids.map((id, i) => {
                const d = files.find((x) => x.file.id === id);
                return (
                  <div
                    key={id}
                    className={`cmp-cell ${differs ? "diff" : "same"} ${f.onClick && d?.file.online ? "link" : ""} ${f.label === "폴더" ? "path-tail" : ""}`}
                    title={values[i]}
                    onClick={() => d && f.onClick?.(d)}
                  >
                    <bdi>{values[i]}</bdi>
                  </div>
                );
              })}
            </div>
          );
        })}
      </div>

      <footer className="cmp-foot">
        <span className="muted small">진하게 표시된 항목이 서로 다른 값입니다. ★ 추천은 해상도·원본 날짜·출처로 고른 남길 파일입니다.</span>
        <span className="spacer" />
        <button className="btn danger" disabled={toDelete.length === 0 || !!running} onClick={remove}>
          🗑 지우기로 고른 {fmtCount(toDelete.length)}개 삭제{toDelete.length > 0 ? ` (${fmtBytes(bytes)})` : ""}
        </button>
        <button className="btn" onClick={onClose}>닫기</button>
      </footer>
    </div>
  );
}
