import { useEffect, useMemo, useState } from "react";
import { api, fmtCount, thumbUrl, type CopyFolderInfo, type FolderFiles, type GridItem } from "../api";
import Viewer from "./Viewer";

const PAGE = 200;
const ref = (f: CopyFolderInfo) => ({ volume_id: f.volume_id, rel_dir: f.rel_dir });

export interface Peek { kind: "files"; folder: CopyFolderInfo; role: string }

/**
 * Look inside a folder before deciding: all its photos and videos, sub-folder by sub-folder. A
 * click on a photo opens it large (videos play). (Two folders side by side: `FolderWalk`.)
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
        <Files folder={peek.folder} role={peek.role} open={open} onClose={onClose} />
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
