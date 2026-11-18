import { useEffect, useState } from "react";
import { api, fmtBytes, fmtDate, thumbUrl, type Family, type FamilyMember } from "../api";

const folderOf = (p: string) => p.slice(0, Math.max(p.lastIndexOf("/"), p.lastIndexOf("\\"))) || p;

/**
 * For a file that has copies or versions: which one is the original, where it is, why it is the
 * original, and what the others are — with the original's picture, so "same photo?" is answered
 * at a glance. Shown wherever a 원본 / 복사본 / 사본 / 백업 / 변형본 mark appears.
 */
export default function OriginalCard({ id, version, onCompare, onMakeOriginal, onOpen }: {
  id: number;
  /** Changes when the file's state may have changed (reload). */
  version?: number;
  onCompare?: (ids: number[]) => void;
  onMakeOriginal?: (id: number) => void;
  /** Open another member big. */
  onOpen?: (id: number) => void;
}) {
  const [fam, setFam] = useState<Family | null>(null);
  const [all, setAll] = useState(false);
  useEffect(() => {
    let live = true;
    setAll(false);
    api.fileFamily(id).then((f) => live && setFam(f)).catch(() => live && setFam(null));
    return () => { live = false; };
  }, [id, version]);
  if (!fam) return null;
  const orig = fam.members[0];
  const me = fam.members.find((m) => m.is_self);
  const others = fam.members.filter((m) => m !== orig);
  const shown = all ? others : others.slice(0, 4);
  const line = (m: FamilyMember) => (
    <div key={m.file.id} className={`fam-row ${m.is_self ? "self" : ""}`}>
      <span className={`role-tag ${m.role}`}>{m.relation}</span>
      <button className="link fam-path" title={m.abs_path} disabled={m.is_self || !onOpen} onClick={() => onOpen?.(m.file.id)}>
        <bdi>{m.abs_path}</bdi>
      </button>
      {m.is_self && <span className="muted small">이 파일</span>}
      {!m.file.online && <span className="warn-text small">연결 안 됨</span>}
      <button className="btn small ghost" disabled={!m.file.online} title="탐색기에서 이 파일이 있는 폴더 열기" onClick={() => api.revealFile(m.file.id)}>폴더 열기</button>
    </div>
  );
  return (
    <section className="fam-card">
      <div className="fam-head">
        <b>{fam.kind === "exact" ? "같은 파일" : "같은 사진의 다른 버전"} {fam.members.length}개</b>
        {me && <span className={`role-tag ${me.role}`}>이 파일은 {me.relation}</span>}
      </div>
      <div className="fam-orig">
        <button className="fam-thumb" disabled={orig.is_self || !onOpen} onClick={() => onOpen?.(orig.file.id)} title="원본 크게 보기">
          <img src={thumbUrl(orig.file.id)} alt="" />
        </button>
        <div className="fam-orig-text">
          <div><span className="role-tag original">원본</span> <b>{orig.file.name}</b>{orig.is_self && <span className="muted small"> (이 파일)</span>}</div>
          <div className="mono small fam-path" title={orig.abs_path}><bdi>{folderOf(orig.abs_path)}</bdi></div>
          <div className="muted small">
            {fmtBytes(orig.file.size)}{orig.file.width ? ` · ${orig.file.width}×${orig.file.height}` : ""} · {fmtDate(orig.file.taken_at ?? orig.file.mtime, true)}
          </div>
          <div className="muted small">원본인 이유: {fam.why}</div>
        </div>
      </div>
      {others.length > 0 && (
        <div className="fam-list">
          {shown.map(line)}
          {others.length > shown.length && <button className="link small" onClick={() => setAll(true)}>나머지 {others.length - shown.length}개 보기</button>}
        </div>
      )}
      <div className="btn-row">
        {me && !orig.is_self && onCompare && (
          <button className="btn small" onClick={() => onCompare([orig.file.id, id])}>⇆ 원본과 나란히 보기</button>
        )}
        <button className="btn small ghost" disabled={!orig.file.online} onClick={() => api.revealFile(orig.file.id)}>원본 폴더 열기</button>
        {me && fam.kind === "exact" && !orig.is_self && onMakeOriginal && (
          <button className="btn small ghost" onClick={() => onMakeOriginal(id)} title="이 파일을 원본으로 삼습니다 (지우지 않음)">이 파일을 원본으로</button>
        )}
      </div>
    </section>
  );
}
