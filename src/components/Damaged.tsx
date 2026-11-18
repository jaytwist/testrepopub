import { useCallback, useEffect, useMemo, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import { api, fileUrl, fmtBytes, fmtCount, fmtDate, thumbUrl, type DamagedDeleteReport, type DamagedFile } from "../api";
import { useJob } from "../App";

const PAGE = 300;

/**
 * The files that look damaged or could not be opened, all in one place: look at them, tick the
 * ones to remove, and delete those for good. Nothing is ticked beforehand, and nothing is
 * deleted without the confirmation at the end.
 */
export default function Damaged({ onClose }: { onClose: () => void }) {
  const { running, setRunning, lastDone, version } = useJob();
  const [files, setFiles] = useState<DamagedFile[]>([]);
  const [total, setTotal] = useState(0);
  const [bytes, setBytes] = useState(0);
  const [loading, setLoading] = useState(true);
  const [picked, setPicked] = useState<Set<number>>(new Set());
  const [big, setBig] = useState<DamagedFile | null>(null);
  const [bigSrc, setBigSrc] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async (more = false) => {
    setLoading(true);
    try {
      const l = await api.damagedList(more ? files.length : 0, PAGE);
      setFiles((prev) => (more ? [...prev, ...l.files] : l.files));
      setTotal(l.total);
      setBytes(l.bytes);
      if (!more) setPicked((p) => new Set([...p].filter((id) => l.files.some((f) => f.id === id))));
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [files.length]);

  // At the start, and after every job (a delete, a check for damage).
  useEffect(() => {
    load(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [version]);

  useEffect(() => {
    setBigSrc(null);
    if (big && big.kind !== "video" && big.online) api.previewPath(big.id).then((p) => setBigSrc(fileUrl(p))).catch(() => setBigSrc(null));
  }, [big]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") (big ? setBig(null) : onClose());
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [big, onClose]);

  const pickable = useMemo(() => files.filter((f) => !f.protected && f.online), [files]);
  const pickedBytes = useMemo(() => files.filter((f) => picked.has(f.id)).reduce((a, f) => a + f.size, 0), [files, picked]);
  const toggle = (f: DamagedFile) => {
    if (f.protected || !f.online) return;
    setPicked((p) => {
      const n = new Set(p);
      if (n.has(f.id)) n.delete(f.id);
      else n.add(f.id);
      return n;
    });
  };

  const remove = async () => {
    const alone = files.filter((f) => picked.has(f.id) && f.other_copies === 0).length;
    const ok = await confirm(
      `고른 파일 ${fmtCount(picked.size)}개(${fmtBytes(pickedBytes)})를 영구 삭제합니다.\n\n` +
        (alone > 0 ? `이 중 ${fmtCount(alone)}개는 같은 사진의 다른 복사본을 찾지 못했습니다. 지우면 그 사진은 남지 않습니다.\n\n` : "") +
        "격리함을 거치지 않고 바로 지워지며, 되돌릴 수 없습니다.",
      { title: "깨진 파일 영구 삭제", kind: "error" },
    );
    if (!ok) return;
    try {
      await api.startDeleteDamaged([...picked]);
      setRunning("delete:damaged");
    } catch (e) {
      setError(String(e));
    }
  };

  const notDamaged = async () => {
    await api.dismiss([...picked], "damaged");
    setPicked(new Set());
    load(false);
  };

  const done = lastDone && lastDone.job === "delete:damaged" && lastDone.ok ? (lastDone.result as DamagedDeleteReport) : null;

  return (
    <div className="triage damaged">
      <div className="damaged-head">
        <div>
          <h2>깨진 파일 확인</h2>
          <div className="muted small">
            구조가 깨졌거나(중간에서 잘림, 빈 구간, 색인 없음) 열 수 없는 사진·영상 {fmtCount(total)}개 · {fmtBytes(bytes)}. 눌러서 고르고, 🔍로 크게 보세요.
            아무것도 미리 골라 두지 않았습니다. ♥·원본 표시·보존 폴더의 파일은 여기서 지워지지 않습니다.
          </div>
        </div>
        <button className="btn" onClick={onClose}>닫기</button>
      </div>
      {error && <div className="alert bad">{error}</div>}
      {done && (
        <div className={`alert ${done.failed ? "bad" : "ok"}`}>
          {fmtCount(done.deleted)}개 영구 삭제 · {fmtBytes(done.bytes)} 확보
          {done.kept_protected > 0 && ` · 보호된 ${fmtCount(done.kept_protected)}개는 남김`}
          {done.skipped > 0 && ` · ${fmtCount(done.skipped)}개는 건너뜀(목록에 없거나 드라이브 미연결)`}
          {done.failed > 0 && <> · 실패 {fmtCount(done.failed)}개<pre>{done.errors.slice(0, 20).join("\n")}</pre></>}
        </div>
      )}
      <div className="damaged-bar">
        <button className="btn small" disabled={!pickable.length} onClick={() => setPicked(new Set(pickable.map((f) => f.id)))}>보이는 것 모두 고르기</button>
        <button className="btn small" disabled={!pickable.length} onClick={() => setPicked(new Set(pickable.filter((f) => f.other_copies > 0).map((f) => f.id)))}
          title="같은 사진의 다른 복사본(같은 크기, 같은 앞부분)이 따로 있는 것만 고릅니다">다른 복사본이 있는 것만 고르기</button>
        <button className="btn small" disabled={!picked.size} onClick={() => setPicked(new Set())}>고른 것 해제</button>
        <span className="grow" />
        <span className="muted small">{picked.size > 0 ? `${fmtCount(picked.size)}개 · ${fmtBytes(pickedBytes)} 고름` : "고른 것 없음"}</span>
        <button className="btn small" disabled={!picked.size || !!running} onClick={notDamaged} title="멀쩡한 파일입니다: 이 목록에서 빼고 다시 올리지 않습니다">깨지지 않음 (목록에서 빼기)</button>
        <button className="btn danger" disabled={!picked.size || !!running} onClick={remove}>고른 {fmtCount(picked.size)}개 영구 삭제</button>
      </div>
      <div className="damaged-grid">
        {!loading && files.length === 0 && <div className="muted">깨진 것으로 보이는 파일이 없습니다.</div>}
        {files.map((f) => (
          <div key={f.id} className={`damaged-card${picked.has(f.id) ? " picked" : ""}${f.protected || !f.online ? " locked" : ""}`} onClick={() => toggle(f)}>
            <div className="damaged-thumb">
              <Thumb id={f.id} />
              <button className="damaged-zoom" title="크게 보기" onClick={(e) => { e.stopPropagation(); setBig(f); }}>🔍</button>
              {picked.has(f.id) && <span className="damaged-check">✓</span>}
            </div>
            <div className="damaged-name" title={`${f.folder}\n${f.name}`}>{f.name}</div>
            <div className="small warn-text">{f.why}</div>
            <div className="small muted" title={f.folder}>{fmtBytes(f.size)}{f.taken_at ? ` · ${fmtDate(f.taken_at)}` : ""} · {tail(f.folder)}</div>
            <div className="small">
              {f.protected ? <span className="pill">보호됨 — 지우지 않음</span>
                : !f.online ? <span className="pill">드라이브 미연결</span>
                : f.other_copies > 0 ? <span className="pill ok" title="같은 크기, 같은 앞부분의 파일이 목록 밖에 있습니다: 같은 사진의 온전한 복사본일 가능성이 높습니다">다른 복사본 {fmtCount(f.other_copies)}개 있음</span>
                : <span className="pill warn" title="같은 사진의 다른 복사본을 찾지 못했습니다">다른 복사본 없음</span>}
            </div>
          </div>
        ))}
      </div>
      {files.length < total && (
        <div className="center"><button className="btn" disabled={loading} onClick={() => load(true)}>더 보기 ({fmtCount(files.length)} / {fmtCount(total)})</button></div>
      )}
      {big && (
        <div className="damaged-big" onClick={() => setBig(null)}>
          <img src={bigSrc ?? thumbUrl(big.id)} alt="" onError={() => setBigSrc(null)} />
          <div className="damaged-big-info" onClick={(e) => e.stopPropagation()}>
            <b>{big.name}</b> · {big.why} · {fmtBytes(big.size)}
            <div className="small muted">{big.folder}</div>
            <div className="row">
              <button className="btn small" onClick={() => api.openFile(big.id)}>기본 프로그램으로 열기</button>
              <button className="btn small" onClick={() => api.revealFile(big.id)}>폴더에서 보기</button>
              {!big.protected && big.online && <button className="btn small" onClick={() => toggle(big)}>{picked.has(big.id) ? "고른 것 해제" : "지울 것으로 고르기"}</button>}
              <button className="btn small" onClick={() => setBig(null)}>닫기 (Esc)</button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

const tail = (folder: string) => folder.split(/[\\/]/).filter(Boolean).slice(-2).join("/");

/** A thumbnail, or a note that there is none (a file too damaged to make one from). */
function Thumb({ id }: { id: number }) {
  const [bad, setBad] = useState(false);
  return bad ? <div className="damaged-nothumb">미리보기 없음</div> : <img src={thumbUrl(id)} alt="" loading="lazy" onError={() => setBad(true)} />;
}
