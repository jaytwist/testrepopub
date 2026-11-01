import { useEffect, useState } from "react";
import {
  api, DATE_SOURCE_NAMES, fileUrl, fmtBytes, fmtDuration, labelName, thumbUrl, toDatetimeLocal,
  type FileDetail, type GridItem, type LabelCount,
} from "../api";

interface Props {
  items: GridItem[];
  index: number;
  onIndex: (i: number) => void;
  onClose: () => void;
  onChanged?: () => void;
}

export default function Viewer({ items, index, onIndex, onClose, onChanged }: Props) {
  const item = items[index];
  const [detail, setDetail] = useState<FileDetail | null>(null);
  const [src, setSrc] = useState<string | null>(null);
  const [videoSrc, setVideoSrc] = useState<string | null>(null);
  const [videoState, setVideoState] = useState<"original" | "converting" | "converted" | "failed">("original");
  const [previewErr, setPreviewErr] = useState<string | null>(null);
  const [newLabel, setNewLabel] = useState("");
  const [allLabels, setAllLabels] = useState<LabelCount[]>([]);
  const [editDate, setEditDate] = useState<string | null>(null);

  const load = () => {
    api.fileDetail(item.id).then(setDetail);
  };

  useEffect(() => {
    setDetail(null);
    setSrc(null);
    setVideoSrc(null);
    setVideoState("original");
    setPreviewErr(null);
    setEditDate(null);
    load();
    if (item.kind === "video") {
      if (item.online) {
        api.fileDetail(item.id).then((d) => setVideoSrc(fileUrl(d.abs_path)));
      }
    } else {
      api.previewPath(item.id).then((p) => setSrc(fileUrl(p))).catch((e) => setPreviewErr(String(e)));
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [item.id]);

  useEffect(() => {
    api.labelCounts().then(setAllLabels).catch(() => {});
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") return onClose();
      if ((e.target as HTMLElement).tagName === "INPUT") return;
      if (e.key === "ArrowRight" && index < items.length - 1) onIndex(index + 1);
      if (e.key === "ArrowLeft" && index > 0) onIndex(index - 1);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [index, items.length, onClose, onIndex]);

  // The webview can't play this codec (HEVC, AVI, WMV...): transcode a preview with the bundled ffmpeg.
  const convertVideo = () => {
    if (videoState === "converting" || videoState === "converted") return;
    setVideoState("converting");
    api
      .videoPreviewPath(item.id)
      .then((p) => {
        setVideoSrc(fileUrl(p));
        setVideoState("converted");
      })
      .catch((e) => {
        setPreviewErr(String(e));
        setVideoState("failed");
      });
  };

  const addLabel = async (l: string) => {
    if (!l.trim()) return;
    await api.addLabel([item.id], l.trim());
    setNewLabel("");
    load();
    onChanged?.();
  };
  const removeLabel = async (l: string) => {
    await api.removeLabel([item.id], l);
    load();
    onChanged?.();
  };
  const saveDate = async () => {
    if (!editDate) return;
    await api.setDate([item.id], editDate);
    setEditDate(null);
    load();
    onChanged?.();
  };

  const f = detail?.file;
  const userLabels = detail?.labels.filter((l) => l.source === "user") ?? [];
  const ruleLabels = detail?.labels.filter((l) => l.source !== "user") ?? [];
  const suggestions = allLabels.filter((l) => l.user && !userLabels.some((u) => u.label === l.label)).slice(0, 8);

  return (
    <div className="viewer" onClick={onClose}>
      <div className="viewer-stage" onClick={(e) => e.stopPropagation()}>
        <button className="viewer-close" onClick={onClose} title="닫기 (Esc)">✕</button>
        {index > 0 && <button className="viewer-nav left" onClick={() => onIndex(index - 1)}>‹</button>}
        {index < items.length - 1 && <button className="viewer-nav right" onClick={() => onIndex(index + 1)}>›</button>}

        {item.kind === "video" ? (
          videoSrc ? (
            <video key={videoSrc} src={videoSrc} controls autoPlay className="viewer-media" onError={() => (videoState === "original" ? convertVideo() : setVideoState("failed"))} />
          ) : (
            <div className="viewer-placeholder">
              <img src={thumbUrl(item.id)} alt="" className="viewer-media dim" />
              <div className="viewer-msg">{item.online ? "불러오는 중…" : "드라이브가 연결되어 있지 않아 썸네일만 표시합니다"}</div>
            </div>
          )
        ) : src ? (
          <img key={src} src={src} alt="" className="viewer-media" onError={() => setSrc(thumbUrl(item.id))} />
        ) : (
          <div className="viewer-placeholder">
            <img src={thumbUrl(item.id)} alt="" className="viewer-media dim" />
            <div className="viewer-msg">{previewErr ?? "미리보기 준비 중…"}</div>
          </div>
        )}
        {videoState === "converting" && <div className="viewer-msg floating"><span className="spinner" /> 이 형식은 바로 재생이 안 되어 변환 중입니다…</div>}
        {videoState === "failed" && <div className="viewer-msg floating bad">재생할 수 없습니다. {previewErr}</div>}
      </div>

      <aside className="viewer-info" onClick={(e) => e.stopPropagation()}>
        {f ? (
          <>
            <div className="info-name" title={f.name}>{f.name}</div>
            <section>
              <h4>촬영 날짜</h4>
              {editDate == null ? (
                <div className="row-between">
                  <span className="big">{detail!.taken || "알 수 없음"}</span>
                  <button className="btn small ghost" onClick={() => setEditDate(toDatetimeLocal(f.taken_at))}>수정</button>
                </div>
              ) : (
                <div className="inline-form">
                  <input type="datetime-local" value={editDate} onChange={(e) => setEditDate(e.target.value)} />
                  <button className="btn small primary" onClick={saveDate}>저장</button>
                  <button className="btn small ghost" onClick={() => setEditDate(null)}>취소</button>
                </div>
              )}
              <div className={`conf conf-${detail!.confidence}`}>
                출처: {DATE_SOURCE_NAMES[f.date_source] ?? f.date_source}
                {detail!.confidence === "low" && " — 실제 촬영일과 다를 수 있습니다"}
              </div>
            </section>

            <section>
              <h4>라벨</h4>
              <div className="chips">
                {userLabels.map((l) => (
                  <span key={l.label} className="chip user">{l.label}<button onClick={() => removeLabel(l.label)}>×</button></span>
                ))}
                {ruleLabels.map((l) => (
                  <span key={l.label} className="chip" title="자동 라벨">{labelName(l.label)}</span>
                ))}
              </div>
              <div className="inline-form">
                <input placeholder="라벨 추가 (Enter)" value={newLabel} onChange={(e) => setNewLabel(e.target.value)} onKeyDown={(e) => e.key === "Enter" && addLabel(newLabel)} />
              </div>
              {suggestions.length > 0 && (
                <div className="chips suggestions">
                  {suggestions.map((s) => <button key={s.label} className="chip ghost" onClick={() => addLabel(s.label)}>+ {s.label}</button>)}
                </div>
              )}
            </section>

            <section className="kv">
              <div><span>종류</span><b>{f.kind === "video" ? "동영상" : f.kind === "raw" ? "RAW" : "사진"} · {f.ext.toUpperCase()}</b></div>
              {f.width && <div><span>해상도</span><b>{f.width} × {f.height}</b></div>}
              {f.duration_ms && <div><span>길이</span><b>{fmtDuration(f.duration_ms)}</b></div>}
              <div><span>크기</span><b>{fmtBytes(f.size)}</b></div>
              {f.camera && <div><span>카메라</span><b>{f.camera}</b></div>}
              {f.lat != null && f.lon != null && <div><span>위치</span><b>{f.lat.toFixed(5)}, {f.lon.toFixed(5)}</b></div>}
              <div><span>드라이브</span><b className={f.online ? "" : "warn-text"}>{f.online ? "연결됨" : "연결 안 됨"}</b></div>
            </section>
            <div className="info-path" title={detail!.abs_path}>{detail!.abs_path}</div>
            <div className="btn-row">
              <button className="btn small" disabled={!f.online} onClick={() => api.revealFile(f.id)}>탐색기에서 보기</button>
              <button className="btn small" disabled={!f.online} onClick={() => api.openFile(f.id)}>기본 앱으로 열기</button>
            </div>
            <div className="muted small">← → 이동 · Esc 닫기 · {index + 1} / {items.length}</div>
          </>
        ) : (
          <div className="muted">불러오는 중…</div>
        )}
      </aside>
    </div>
  );
}
