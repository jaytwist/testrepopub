import { useEffect, useState } from "react";
import { api, fmtBytes, fmtCount, thumbUrl, type Family, type FamilyMember } from "../api";

const split = (p: string) => {
  const i = Math.max(p.lastIndexOf("/"), p.lastIndexOf("\\"));
  return i < 0 ? { folder: "", name: p } : { folder: p.slice(0, i), name: p.slice(i + 1) };
};

/**
 * Where the same photo is, everywhere: one row per file (the original first), whatever its name.
 * A click on a row opens its folder in the file manager with the file selected.
 */
export default function CopiesPanel({ id, onClose, onShowFolder, onCompare }: {
  id: number;
  onClose: () => void;
  /** Show that folder in the library. */
  onShowFolder?: (volumeId: number, dir: string) => void;
  onCompare?: (ids: number[]) => void;
}) {
  const [fam, setFam] = useState<Family | null | undefined>(undefined);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    setFam(undefined);
    api.fileFamily(id).then((f) => live && setFam(f)).catch((e) => live && (setFam(null), setError(String(e))));
    return () => { live = false; };
  }, [id]);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") { e.stopPropagation(); onClose(); }
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);

  const reveal = (m: FamilyMember) => {
    setError(null);
    api.revealFile(m.file.id).catch((e) => setError(String(e)));
  };
  const members = fam?.members ?? [];
  const drives = new Set(members.map((m) => m.file.volume_id)).size;

  return (
    <div className="copies-back" onClick={onClose}>
      <div className="copies-panel" onClick={(e) => e.stopPropagation()}>
        <div className="copies-head">
          <img src={thumbUrl(id)} alt="" />
          <div>
            <h3>{fam === undefined ? "찾는 중…" : members.length > 1 ? `같은 사진 ${fmtCount(members.length)}장` : "이 사진은 한 장뿐입니다"}</h3>
            {members.length > 1 && (
              <div className="muted small">
                {fam?.kind === "exact" ? "내용이 같은 파일입니다(이름·폴더가 달라도)." : "같은 사진의 다른 버전입니다(크기·화질이 다를 수 있음)."} 드라이브 {fmtCount(drives)}곳.
                {fam?.why ? ` 원본으로 본 이유: ${fam.why}` : ""}
              </div>
            )}
            <div className="muted small">줄을 누르면 그 폴더가 열리고 파일이 선택됩니다.</div>
          </div>
          <button className="btn small" onClick={onClose}>닫기</button>
        </div>
        {error && <div className="alert bad small">{error}</div>}
        <div className="copies-list">
          {members.map((m) => {
            const { folder, name } = split(m.abs_path);
            const rel = m.file.rel_path;
            const dir = rel.includes("/") ? rel.slice(0, rel.lastIndexOf("/")) : "";
            return (
              <div key={m.file.id} className={`copies-row${m.file.online ? "" : " off"}`} title={m.file.online ? "폴더 열기" : "드라이브가 연결되어 있지 않습니다"}
                onClick={() => m.file.online && reveal(m)}>
                <span className={`role-tag ${m.role}`}>{m.relation}</span>
                <div className="copies-where">
                  <div className="copies-folder"><bdi>{folder}</bdi></div>
                  <div className="small">
                    <b>{name}</b> <span className="muted">· {fmtBytes(m.file.size)}</span>
                    {m.is_self && <span className="muted"> · 지금 보이는 파일</span>}
                    {!m.file.online && <span className="warn-text"> · 연결 안 됨</span>}
                    {m.damaged && <span className="warn-text"> · ⚠ 깨진 것 같음</span>}
                  </div>
                </div>
                {onShowFolder && (
                  <button className="btn small ghost" title="라이브러리에서 이 폴더의 사진 보기" onClick={(e) => { e.stopPropagation(); onShowFolder(m.file.volume_id, dir); }}>앱에서 보기</button>
                )}
                <button className="btn small" disabled={!m.file.online} onClick={(e) => { e.stopPropagation(); reveal(m); }}>폴더 열기</button>
              </div>
            );
          })}
        </div>
        {onCompare && members.length > 1 && (
          <div className="copies-foot">
            <button className="btn small" onClick={() => onCompare(members.slice(0, 4).map((m) => m.file.id))}>나란히 비교</button>
          </div>
        )}
      </div>
    </div>
  );
}
