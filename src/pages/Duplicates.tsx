import { useEffect, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import { absPath, api, fmtBytes, fmtCount, fmtDate, thumbUrl, type DupGroup, type DupSummary } from "../api";
import { useJob } from "../App";

const KINDS = [
  { id: "exact", title: "완전히 같은 파일", desc: "바이트 단위로 동일한 파일 (여러 드라이브에 복사된 사본)", safe: true },
  { id: "similar_image", title: "비슷한 사진", desc: "크기 조정·재압축·메신저 전송본 등 같은 사진의 다른 버전", safe: false },
  { id: "similar_video", title: "비슷한 동영상", desc: "다시 인코딩되거나 잘린 같은 영상 (ffmpeg 필요)", safe: false },
];

export default function Duplicates() {
  const { version, running, setRunning } = useJob();
  const [summary, setSummary] = useState<DupSummary[]>([]);
  const [kind, setKind] = useState("exact");
  const [groups, setGroups] = useState<DupGroup[]>([]);
  const [similarity, setSimilarity] = useState("high");
  const [error, setError] = useState<string | null>(null);
  const [limit, setLimit] = useState(50);

  const reload = () => {
    api.dupSummary().then(setSummary);
    api.dupGroups(kind, 0, limit).then(setGroups);
  };
  useEffect(reload, [version, kind, limit]);

  const find = async (k: string) => {
    setError(null);
    try {
      await api.startDedupe(k, similarity);
      setRunning(`dedupe:${k}`);
      setKind(k);
    } catch (e) {
      setError(String(e));
    }
  };

  const toggleKeep = async (g: DupGroup, fileId: number, keep: boolean) => {
    await api.setKeep(g.id, fileId, keep);
    setGroups((gs) =>
      gs.map((x) => (x.id !== g.id ? x : { ...x, members: x.members.map((m) => (m.file.id === fileId ? { ...m, keep } : m)) })),
    );
  };

  const apply = async (groupIds?: number[]) => {
    const s = summary.find((x) => x.kind === kind);
    const what = groupIds ? `${groupIds.length}개 그룹` : `모든 그룹 (${fmtCount(s?.groups ?? 0)}개, ${fmtBytes(s?.reclaimable_bytes ?? 0)})`;
    const ok = await confirm(
      `${what}에서 "보존"이 아닌 파일을 격리함으로 옮깁니다.\n\n파일은 삭제되지 않으며 「작업 기록」에서 언제든 되돌릴 수 있습니다.`,
      { title: "중복 정리", kind: "warning" },
    );
    if (!ok) return;
    try {
      await api.startDedupeApply(kind, groupIds);
      setRunning("dedupe:apply");
    } catch (e) {
      setError(String(e));
    }
  };

  const cur = summary.find((s) => s.kind === kind);

  return (
    <div className="page">
      <header className="page-head">
        <div>
          <h1>중복 정리</h1>
          <div className="muted">연결된 모든 드라이브를 비교합니다. 각 그룹에서 가장 좋은 파일(해상도·원본 날짜·카메라 정보 기준)을 자동으로 "보존"으로 고릅니다.</div>
        </div>
      </header>
      {error && <div className="alert bad">{error}</div>}

      <div className="cards three">
        {KINDS.map((k) => {
          const s = summary.find((x) => x.kind === k.id);
          return (
            <div key={k.id} className={`card dup-kind ${kind === k.id ? "active" : ""}`} onClick={() => setKind(k.id)}>
              <b>{k.title}</b>
              <div className="muted small">{k.desc}</div>
              <div className="dup-stat">
                {s ? <>{fmtCount(s.groups)}개 그룹 · <b>{fmtBytes(s.reclaimable_bytes)}</b> 확보 가능</> : <span className="muted">아직 검사 안 함</span>}
              </div>
              <div className="btn-row">
                {k.id === "similar_image" && (
                  <select value={similarity} onClick={(e) => e.stopPropagation()} onChange={(e) => setSimilarity(e.target.value)}>
                    <option value="very_high">아주 비슷한 것만</option>
                    <option value="high">비슷함 (권장)</option>
                    <option value="medium">조금 비슷함</option>
                  </select>
                )}
                <button className="btn small primary" disabled={!!running} onClick={(e) => { e.stopPropagation(); find(k.id); }}>찾기</button>
              </div>
            </div>
          );
        })}
      </div>

      {cur && cur.groups > 0 && (
        <div className="row-between sticky-actions">
          <div>
            <b>{KINDS.find((k) => k.id === kind)?.title}</b> · {fmtCount(cur.groups)}개 그룹 · {fmtCount(cur.files)}개 파일
            {!KINDS.find((k) => k.id === kind)?.safe && <span className="warn-text"> · 비슷한 항목은 한 번씩 확인 후 적용하세요</span>}
          </div>
          <button className="btn primary" disabled={!!running} onClick={() => apply()}>모두 적용 ({fmtBytes(cur.reclaimable_bytes)} 확보)</button>
        </div>
      )}

      <div className="dup-groups">
        {groups.map((g) => (
          <div key={g.id} className="dup-group">
            <div className="row-between">
              <span className="muted small">{g.members.length}개 · {fmtBytes(g.reclaimable_bytes)} 확보 가능</span>
              <button className="btn small" disabled={!!running || !g.members.some((m) => m.keep)} onClick={() => apply([g.id])}>이 그룹만 적용</button>
            </div>
            <div className="dup-members">
              {g.members.map((m) => (
                <div key={m.file.id} className={`dup-member ${m.keep ? "keep" : "drop"}`}>
                  <img src={thumbUrl(m.file.id)} alt="" loading="lazy" />
                  <label className="keep-toggle">
                    <input type="checkbox" checked={m.keep} onChange={(e) => toggleKeep(g, m.file.id, e.target.checked)} />
                    {m.keep ? "보존" : "격리"}
                  </label>
                  <div className="dup-meta">
                    <div className="mono small path-tail" title={absPath(m.file)}><bdi>{absPath(m.file)}</bdi></div>
                    <div className="small muted">
                      {m.file.width ? `${m.file.width}×${m.file.height} · ` : ""}{fmtBytes(m.file.size)} · {fmtDate(m.file.taken_at)}
                      {!m.file.online && " · 연결 안 됨"}
                    </div>
                  </div>
                </div>
              ))}
            </div>
          </div>
        ))}
        {groups.length >= limit && <button className="btn" onClick={() => setLimit(limit + 50)}>더 보기</button>}
        {cur == null && <div className="empty small"><p>위에서 「찾기」를 누르면 결과가 여기에 표시됩니다.</p></div>}
      </div>
    </div>
  );
}
