import { useEffect, useMemo, useState } from "react";
import { api, fmtCount, thumbUrl, type CopyFolderInfo, type FolderCompare, type FolderFiles, type GridItem } from "../api";
import Viewer from "./Viewer";

const PAGE = 200;
const ref = (f: CopyFolderInfo) => ({ volume_id: f.volume_id, rel_dir: f.rel_dir });

export type Peek =
  | { kind: "files"; folder: CopyFolderInfo; role: string }
  | { kind: "compare"; keep: CopyFolderInfo; copy: CopyFolderInfo };

/**
 * Look inside a folder before deciding: all its photos and videos (sub-folder by sub-folder), or
 * two folders side by side — what both have, what only the original has, what only the copy has.
 * A click on a photo opens it large (videos play).
 */
export default function FolderPeek({ peek, onClose }: { peek: Peek; onClose: () => void }) {
  const [view, setView] = useState<{ items: GridItem[]; index: number } | null>(null);
  /** Open one photo large, with the photos shown around it to page through. */
  const open = async (id: number, around: number[]) => {
    const rows = await api.grid({ ids: around }, 0, around.length).catch(() => [] as GridItem[]);
    const byId = new Map(rows.map((r) => [r.id, r]));
    const items = around.map((i) => byId.get(i)).filter((x): x is GridItem => x != null);
    const index = items.findIndex((x) => x.id === id);
    if (index >= 0) setView({ items, index });
  };
  return (
    <div className="send-back" onClick={onClose}>
      <div className="card sel-preview fp" onClick={(e) => e.stopPropagation()}>
        {peek.kind === "files" ? <Files folder={peek.folder} role={peek.role} open={open} onClose={onClose} /> : <Side keep={peek.keep} copy={peek.copy} open={open} onClose={onClose} />}
      </div>
      {view && (
        <div onClick={(e) => e.stopPropagation()}>
          <Viewer items={view.items} index={view.index} onIndex={(index) => setView((v) => (v ? { ...v, index } : v))} onClose={() => setView(null)} />
        </div>
      )}
    </div>
  );
}

type Open = (id: number, around: number[]) => void;

function Tile({ id, around, open }: { id: number; around: number[]; open: Open }) {
  return (
    <button className="keep-tile" onClick={() => open(id, around)}>
      <img src={thumbUrl(id)} loading="lazy" alt="" draggable={false} />
    </button>
  );
}

function Files({ folder, role, open, onClose }: { folder: CopyFolderInfo; role: string; open: Open; onClose: () => void }) {
  const [dirs, setDirs] = useState<FolderFiles[] | null>(null);
  const [shown, setShown] = useState(PAGE);
  useEffect(() => {
    api.folderFiles(ref(folder)).then(setDirs).catch(() => setDirs([]));
  }, [folder]);
  const total = (dirs ?? []).reduce((a, d) => a + d.ids.length, 0);
  // The first `shown` files, kept in their sub-folders.
  const visible = useMemo(() => {
    let left = shown;
    const out: FolderFiles[] = [];
    for (const d of dirs ?? []) {
      if (left <= 0) break;
      out.push({ dir: d.dir, ids: d.ids.slice(0, left) });
      left -= d.ids.length;
    }
    return out;
  }, [dirs, shown]);
  const around = useMemo(() => visible.flatMap((d) => d.ids), [visible]);
  return (
    <>
      <div className="row-between">
        <h3>{role} 폴더의 사진·영상 <span className="muted small">{fmtCount(total)}개</span></h3>
        <button className="btn small ghost" onClick={onClose}>닫기</button>
      </div>
      <div className="fp-path mono">{folder.abs_path}</div>
      {!dirs && <div className="muted small">불러오는 중…</div>}
      {visible.map((d) => (
        <div key={d.dir}>
          {(dirs?.length ?? 0) > 1 && <div className="fp-dir small">{d.dir === "" ? "(이 폴더)" : d.dir} <span className="muted">· {fmtCount(dirs!.find((x) => x.dir === d.dir)!.ids.length)}개</span></div>}
          <div className="keep-grid">{d.ids.map((id) => <Tile key={id} id={id} around={around} open={open} />)}</div>
        </div>
      ))}
      {total > shown && <button className="btn small" onClick={() => setShown((n) => n + PAGE)}>더 보기 ({fmtCount(total - shown)}개 남음)</button>}
    </>
  );
}

function Side({ keep, copy, open, onClose }: { keep: CopyFolderInfo; copy: CopyFolderInfo; open: Open; onClose: () => void }) {
  const [cmp, setCmp] = useState<FolderCompare | null>(null);
  const [tab, setTab] = useState<"shared" | "keep" | "copy">("shared");
  const [shown, setShown] = useState(PAGE);
  useEffect(() => {
    setCmp(null);
    api.folderCompare(ref(keep), ref(copy)).then((c) => {
      setCmp(c);
      // Open on what differs, if anything does: that is what the user came to see.
      setTab(c.only_copy.length ? "copy" : c.only_keep.length ? "keep" : "shared");
    }).catch(() => setCmp({ shared: [], only_keep: [], only_copy: [] }));
  }, [keep, copy]);
  useEffect(() => setShown(PAGE), [tab]);
  const n = cmp ? { shared: cmp.shared.length, keep: cmp.only_keep.length, copy: cmp.only_copy.length } : null;
  const same = n != null && n.keep === 0 && n.copy === 0;
  const pairs = cmp?.shared.slice(0, shown) ?? [];
  const keepIds = pairs.map((p) => p[0]);
  const copyIds = pairs.map((p) => p[1]);
  const single = (tab === "keep" ? cmp?.only_keep : cmp?.only_copy)?.slice(0, shown) ?? [];
  const count = tab === "shared" ? n?.shared ?? 0 : tab === "keep" ? n?.keep ?? 0 : n?.copy ?? 0;
  return (
    <>
      <div className="row-between">
        <h3>두 폴더 나란히 비교</h3>
        <button className="btn small ghost" onClick={onClose}>닫기</button>
      </div>
      <div className="fp-two">
        <div><span className="cf-tag keep">원본 · 남김</span><div className="fp-path mono">{keep.abs_path}</div></div>
        <div><span className="cf-tag go">복사본</span><div className="fp-path mono">{copy.abs_path}</div></div>
      </div>
      {!cmp && <div className="muted small">비교하는 중…</div>}
      {n && (
        <>
          <div className={`alert ${same ? "ok" : "warn"} small`}>
            {same
              ? `두 폴더의 사진·영상 ${fmtCount(n.shared)}개가 전부 같습니다.`
              : `같은 것 ${fmtCount(n.shared)}개 · 원본에만 ${fmtCount(n.keep)}개 · 복사본에만 ${fmtCount(n.copy)}개. 지우는 것은 "양쪽에 있음"뿐이고, 복사본에만 있는 것은 그 폴더에 남습니다.`}
          </div>
          <div className="fp-tabs">
            <button className={`fp-tab ${tab === "shared" ? "on" : ""}`} onClick={() => setTab("shared")}>양쪽에 있음 ({fmtCount(n.shared)})</button>
            <button className={`fp-tab ${tab === "keep" ? "on" : ""}`} onClick={() => setTab("keep")}>원본에만 ({fmtCount(n.keep)})</button>
            <button className={`fp-tab ${tab === "copy" ? "on" : ""}`} onClick={() => setTab("copy")}>복사본에만 ({fmtCount(n.copy)})</button>
          </div>
          {tab === "shared" ? (
            <div className="fp-pairs">
              {pairs.map(([k, c]) => (
                <div key={c} className="fp-pair">
                  <Tile id={k} around={keepIds} open={open} />
                  <span className="muted">=</span>
                  <Tile id={c} around={copyIds} open={open} />
                </div>
              ))}
            </div>
          ) : (
            <div className="keep-grid">{single.map((id) => <Tile key={id} id={id} around={single} open={open} />)}</div>
          )}
          {count === 0 && <div className="muted small">없습니다.</div>}
          {count > shown && <button className="btn small" onClick={() => setShown((x) => x + PAGE)}>더 보기 ({fmtCount(count - shown)}개 남음)</button>}
        </>
      )}
    </>
  );
}
