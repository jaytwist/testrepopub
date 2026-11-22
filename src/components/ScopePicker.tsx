import { useEffect, useMemo, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, fmtBytes, fmtCount, type DirCount, type Scope, type VolumeRow } from "../api";
import { useScope } from "../App";

interface Node { dir: string; name: string; files: number; bytes: number; kids: Node[] }

/** Folders as a tree; each folder counts what is in it and below it. */
function buildTree(rows: DirCount[]): Node {
  const root: Node = { dir: "", name: "", files: 0, bytes: 0, kids: [] };
  const at = new Map<string, Node>([["", root]]);
  const node = (dir: string): Node => {
    const have = at.get(dir);
    if (have) return have;
    const i = dir.lastIndexOf("/");
    const parent = node(i < 0 ? "" : dir.slice(0, i));
    const n: Node = { dir, name: dir.slice(i + 1), files: 0, bytes: 0, kids: [] };
    parent.kids.push(n);
    at.set(dir, n);
    return n;
  };
  for (const r of rows) {
    const n = node(r.dir);
    // Add to the folder and to every folder above it.
    for (let d: string | null = n.dir; d !== null; d = d === "" ? null : d.includes("/") ? d.slice(0, d.lastIndexOf("/")) : "") {
      const a = at.get(d)!;
      a.files += r.files;
      a.bytes += r.bytes;
    }
  }
  const sort = (n: Node) => { n.kids.sort((a, b) => a.name.localeCompare(b.name, "ko")); n.kids.forEach(sort); };
  sort(root);
  return root;
}

/** The scope as a full path, for a tooltip. */
function fullPath(scope: Scope, volumes: VolumeRow[]): string {
  const v = volumes.find((x) => x.id === scope.volume_id);
  if (!v) return "";
  const sep = v.mount.includes("\\") ? "\\" : "/";
  return v.mount.replace(/[\\/]+$/, "") + (scope.dir ? sep + scope.dir.split("/").join(sep) : "");
}

/** "백업 드라이브 › RAID_Recovery › 2019" */
export function scopeText(scope: Scope, volumes: VolumeRow[]): string {
  const v = volumes.find((x) => x.id === scope.volume_id);
  const drive = v ? v.label : `드라이브 ${scope.volume_id}`;
  return scope.dir ? `${drive} › ${scope.dir.split("/").join(" › ")}` : `${drive} 전체`;
}

/**
 * The work scope: everything, one drive, or one folder with everything below it. Shown on top
 * of 「라이브러리」 and 「정리」 — both use the same scope.
 */
export default function ScopePicker({ note }: { note?: string }) {
  const { scope, setScope } = useScope();
  const [volumes, setVolumes] = useState<VolumeRow[]>([]);
  const [openPanel, setOpenPanel] = useState(false);
  const [vol, setVol] = useState<number | null>(null);
  const [rows, setRows] = useState<DirCount[] | null>(null);
  const [expanded, setExpanded] = useState<Set<string>>(new Set([""]));
  const [error, setError] = useState<string | null>(null);

  useEffect(() => { api.listVolumes().then(setVolumes).catch(() => {}); }, [openPanel]);
  useEffect(() => {
    if (vol == null) return;
    setRows(null);
    api.folderTree(vol).then(setRows).catch((e) => setError(String(e)));
  }, [vol]);
  const tree = useMemo(() => (rows ? buildTree(rows) : null), [rows]);

  const choose = async (s: Scope | null) => {
    setError(null);
    try {
      await setScope(s);
      setOpenPanel(false);
    } catch (e) {
      setError(String(e));
    }
  };
  const fromExplorer = async () => {
    const picked = await open({ directory: true, multiple: false, title: "정리할 드라이브나 폴더 고르기" });
    if (typeof picked !== "string") return;
    const s = await api.scopeForPath(picked).catch(() => null);
    if (!s) {
      setError("등록된 드라이브·폴더 안이 아닙니다. 먼저 「드라이브 · 폴더」에서 추가하고 스캔하세요.");
      return;
    }
    choose(s);
  };
  const showPanel = () => {
    setOpenPanel((o) => !o);
    setVol(scope?.volume_id ?? volumes.find((v) => v.file_count > 0)?.id ?? null);
    if (scope?.dir) {
      // Open the tree down to the chosen folder.
      const parts = scope.dir.split("/");
      setExpanded(new Set(["", ...parts.map((_, i) => parts.slice(0, i + 1).join("/"))]));
    }
  };

  const row = (n: Node, depth: number) => {
    const isOpen = expanded.has(n.dir);
    const here = scope && scope.volume_id === vol && scope.dir === n.dir;
    return (
      <div key={n.dir}>
        <div className={`scope-row${here ? " on" : ""}`} style={{ paddingLeft: 8 + depth * 16 }}>
          <button className="scope-twisty" disabled={!n.kids.length} onClick={() => setExpanded((s) => { const x = new Set(s); if (x.has(n.dir)) x.delete(n.dir); else x.add(n.dir); return x; })}>
            {n.kids.length ? (isOpen ? "▾" : "▸") : ""}
          </button>
          <span className="scope-name" title={n.dir || "드라이브 전체"}>{n.dir ? n.name : "드라이브 전체"}</span>
          <span className="muted small">{fmtCount(n.files)}개 · {fmtBytes(n.bytes)}</span>
          <button className="btn small" onClick={() => vol != null && choose({ volume_id: vol, dir: n.dir })}>{here ? "지금 범위" : "여기로"}</button>
        </div>
        {isOpen && n.kids.map((k) => row(k, depth + 1))}
      </div>
    );
  };

  return (
    <div className={`scope-bar${scope ? " set" : ""}`}>
      <span className="scope-label">범위</span>
      <button className="btn small scope-current" onClick={showPanel} title={scope ? fullPath(scope, volumes) : "이 화면에서 볼 드라이브나 폴더(하위 폴더 포함)를 고릅니다"}>
        {scope ? scopeText(scope, volumes) : "전체 (모든 드라이브)"} ▾
      </button>
      {scope && <span className="small">하위 폴더 포함{note ? ` · ${note}` : ""}</span>}
      {scope && <button className="btn small ghost" onClick={() => choose(null)}>전체로</button>}
      {error && <span className="small warn-text">{error}</span>}
      {openPanel && (
        <div className="scope-panel" onClick={(e) => e.stopPropagation()}>
          <div className="scope-drives">
            {volumes.filter((v) => v.file_count > 0).map((v) => (
              <button key={v.id} className={`chip ${vol === v.id ? "on" : ""} ${v.online ? "" : "offline"}`} onClick={() => setVol(v.id)}>
                {v.label} <span className="muted">{fmtCount(v.file_count)}</span>
              </button>
            ))}
            <span className="grow" />
            <button className="btn small" onClick={fromExplorer}>탐색기에서 고르기…</button>
            <button className="btn small" onClick={() => choose(null)}>전체</button>
            <button className="btn small ghost" onClick={() => setOpenPanel(false)}>닫기</button>
          </div>
          <div className="scope-tree">
            {vol == null ? <div className="muted small">드라이브를 고르세요.</div> : !tree ? <div className="muted small">불러오는 중…</div> : row(tree, 0)}
          </div>
        </div>
      )}
    </div>
  );
}
