import { useEffect, useState } from "react";
import { useNav } from "../App";
import ZoomImage from "./ZoomImage";
import Compare from "./Compare";
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
  /** One-by-one review of cleanup suggestions: K keeps (remembered), Delete marks for deletion. */
  review?: ReviewProps;
}

export interface ReviewProps {
  title: string;
  marked: Set<number>;
  kept: Set<number>;
  onMark: (id: number) => void;
  onKeep: (id: number) => void;
  /** Close the review; the marked files are deleted together (after one confirmation). */
  onFinish: () => void;
}

const REASON_NAMES: Record<string, string> = { blurry: "흐린 사진", burst_extra: "연사" };

export default function Viewer({ items, index, onIndex, onClose, onChanged, review }: Props) {
  const nav = useNav();
  const item = items[index];
  const [detail, setDetail] = useState<FileDetail | null>(null);
  const [src, setSrc] = useState<string | null>(null);
  const [videoSrc, setVideoSrc] = useState<string | null>(null);
  const [videoState, setVideoState] = useState<"original" | "converting" | "converted" | "failed">("original");
  const [previewErr, setPreviewErr] = useState<string | null>(null);
  const [newLabel, setNewLabel] = useState("");
  const [allLabels, setAllLabels] = useState<LabelCount[]>([]);
  const [editDate, setEditDate] = useState<string | null>(null);
  const [vlc, setVlc] = useState<string | null>(null);
  const [vlcMsg, setVlcMsg] = useState<string | null>(null);
  const [comparing, setComparing] = useState<number[] | null>(null);
  const [similarMsg, setSimilarMsg] = useState<string | null>(null);

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
    setVlcMsg(null);
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
    api.vlcPath().then(setVlc).catch(() => {});
  }, []);

  /** Show this photo's surroundings in the library: its folder, or everything from that day. */
  const openContext = (what: "folder" | "day") => {
    const f = detail?.file;
    if (!f) return;
    onClose();
    if (what === "folder") {
      const dir = f.rel_path.includes("/") ? f.rel_path.slice(0, f.rel_path.lastIndexOf("/")) : "";
      const name = dir.split("/").pop() || "(최상위 폴더)";
      nav.openLibrary({ title: `폴더: ${name}`, filter: { volume_id: f.volume_id, dir, sort: "taken", desc: false }, groupBy: "none" });
    } else if (f.taken_at != null) {
      const day = f.taken_at - (((f.taken_at % 86400) + 86400) % 86400);
      nav.openLibrary({ title: `${detail!.taken.slice(0, 10)} 하루`, filter: { date_from: day, date_to: day + 86399, sort: "taken", desc: false }, groupBy: "none" });
    }
  };

  /** Resized / recompressed / edited versions of this photo anywhere, side by side. */
  const findSimilar = async () => {
    setSimilarMsg(null);
    const ids = await api.similarTo(item.id);
    if (ids.length === 0) setSimilarMsg("다른 곳에서 비슷한 사진을 찾지 못했습니다 (사진 분석이 끝난 사진만 찾습니다)");
    else setComparing([item.id, ...ids]);
  };

  const playInVlc = () => {
    setVlcMsg(null);
    api.openInVlc(item.id).then(() => setVlcMsg("VLC에서 재생합니다")).catch((e) => setVlcMsg(String(e)));
  };

  const close = review ? review.onFinish : onClose;
  const next = () => index < items.length - 1 && onIndex(index + 1);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (comparing) return; // the compare screen on top handles its own keys
      if (e.key === "Escape") return close();
      if ((e.target as HTMLElement).tagName === "INPUT") return;
      if (e.key === "ArrowRight") next();
      if (e.key === "ArrowLeft" && index > 0) onIndex(index - 1);
      if (!review) return;
      if (e.key === "k" || e.key === "K" || e.key === "ㅏ") {
        review.onKeep(item.id);
        next();
      } else if (e.key === "Delete" || e.key === "d" || e.key === "D" || e.key === "ㅇ") {
        e.preventDefault();
        const wasMarked = review.marked.has(item.id);
        review.onMark(item.id);
        if (!wasMarked) next();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [index, items.length, close, onIndex, review, item.id, comparing]);

  // Other shots of the same burst, for side-by-side comparison.
  const burst = item.burst_id != null ? items.filter((it) => it.burst_id === item.burst_id) : [];

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
    setVlcMsg(null);
    load();
    onChanged?.();
  };

  const f = detail?.file;
  const userLabels = detail?.labels.filter((l) => l.source === "user") ?? [];
  const ruleLabels = detail?.labels.filter((l) => l.source !== "user") ?? [];
  const suggestions = allLabels.filter((l) => l.user && !userLabels.some((u) => u.label === l.label)).slice(0, 8);

  return (
    <div className={`viewer ${review ? "reviewing" : ""}`} onClick={close}>
      <div className="viewer-stage" onClick={(e) => e.stopPropagation()}>
        <button className="viewer-close" onClick={close} title="닫기 (Esc)">✕</button>
        {review && (
          <div className="review-bar">
            <b>{review.title}</b>
            <span className="muted">{index + 1} / {items.length}</span>
            {review.marked.has(item.id) ? <span className="mark del">삭제할 목록에 있음</span> : review.kept.has(item.id) ? <span className="mark keep">남기기로 함</span> : null}
            <span className="spacer" />
            <button className="btn small" onClick={() => { review.onKeep(item.id); next(); }} title="K">남기기 <kbd>K</kbd></button>
            <button className={`btn small ${review.marked.has(item.id) ? "" : "danger"}`} onClick={() => { const was = review.marked.has(item.id); review.onMark(item.id); if (!was) next(); }} title="Delete">
              {review.marked.has(item.id) ? "삭제 취소" : "삭제 목록에 담기"} <kbd>Del</kbd>
            </button>
            <button className="btn small primary" onClick={review.onFinish}>
              {review.marked.size > 0 ? `끝내고 ${review.marked.size}개 삭제` : "검토 끝내기"}
            </button>
          </div>
        )}
        {index > 0 && <button className="viewer-nav left" onClick={() => onIndex(index - 1)}>‹</button>}
        {index < items.length - 1 && <button className="viewer-nav right" onClick={() => onIndex(index + 1)}>›</button>}

        {item.kind === "video" ? (
          videoSrc ? (
            <video key={videoSrc} src={videoSrc} controls autoPlay className="viewer-media" onError={() => (videoState === "original" ? (vlc ? setVideoState("failed") : convertVideo()) : setVideoState("failed"))} />
          ) : (
            <div className="viewer-placeholder">
              <img src={thumbUrl(item.id)} alt="" className="viewer-media dim" />
              <div className="viewer-msg">{item.online ? "불러오는 중…" : "드라이브가 연결되어 있지 않아 썸네일만 표시합니다"}</div>
            </div>
          )
        ) : src ? (
          <ZoomImage key={src} src={src} resetKey={item.id} onError={() => setSrc(thumbUrl(item.id))} />
        ) : (
          <div className="viewer-placeholder">
            <img src={thumbUrl(item.id)} alt="" className="viewer-media dim" />
            <div className="viewer-msg">{previewErr ?? "미리보기 준비 중…"}</div>
          </div>
        )}
        {videoState === "converting" && <div className="viewer-msg floating"><span className="spinner" /> 이 형식은 바로 재생이 안 되어 변환 중입니다…</div>}
        {videoState === "failed" && (
          <div className="viewer-msg floating choice">
            <span>이 동영상 형식은 앱 안에서 바로 재생되지 않습니다.{previewErr ? ` (${previewErr})` : ""}</span>
            <span className="btn-row">
              {vlc && <button className="btn small primary" onClick={playInVlc}>VLC로 재생</button>}
              {!previewErr && <button className="btn small" onClick={() => { setVideoState("original"); convertVideo(); }}>변환해서 여기서 보기</button>}
            </span>
          </div>
        )}
        {burst.length > 1 && (
          <div className="burst-strip">
            {burst.map((b) => {
              const i = items.indexOf(b);
              return (
                <button key={b.id} className={`burst-thumb ${b.id === item.id ? "cur" : ""} ${review?.marked.has(b.id) ? "marked" : ""}`} onClick={() => onIndex(i)} title={b.burst_rank === 0 ? "가장 잘 나온 사진" : ""}>
                  <img src={thumbUrl(b.id)} alt="" />
                  {b.burst_rank === 0 && <span className="star">★</span>}
                  {review?.marked.has(b.id) && <span className="x">🗑</span>}
                </button>
              );
            })}
          </div>
        )}
      </div>

      <aside className="viewer-info" onClick={(e) => e.stopPropagation()}>
        {f ? (
          <>
            <div className="info-name" title={f.name}>{f.name}</div>
            {detail!.reasons.length > 0 && (
              <section className="reasons">
                <h4>정리 제안</h4>
                {detail!.reasons.map(([k, d]) => (
                  <div key={k} className="reason"><b>{REASON_NAMES[k] ?? k}</b> {d}</div>
                ))}
              </section>
            )}
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
              {detail!.people.length > 0 && (
                <div>
                  <span>인물</span>
                  <b className="people-links">
                    {detail!.people.map(([pid, name], i) => (
                      <span key={pid}>
                        {i > 0 && ", "}
                        <a onClick={() => { onClose(); nav.openLibrary({ title: `${name}의 사진`, filter: { person_ids: [pid] }, groupBy: "month" }); }}>{name}</a>
                      </span>
                    ))}
                  </b>
                </div>
              )}
              {f.lat != null && f.lon != null && (
                <div><span>장소</span><b title={`${f.lat.toFixed(5)}, ${f.lon.toFixed(5)}`}>{detail!.place ?? `${f.lat.toFixed(5)}, ${f.lon.toFixed(5)}`}</b></div>
              )}
              <div><span>드라이브</span><b className={f.online ? "" : "warn-text"}>{f.online ? "연결됨" : "연결 안 됨"}</b></div>
            </section>
            <div className="info-path" title={detail!.abs_path}>{detail!.abs_path}</div>
            <div className="context-links">
              <a onClick={() => openContext("folder")}>📁 같은 폴더 사진</a>
              {f.taken_at != null && <a onClick={() => openContext("day")}>📅 같은 날 사진</a>}
              {f.kind !== "video" && <a onClick={findSimilar}>⧉ 비슷한 사진 찾기</a>}
            </div>
            {similarMsg && <div className="small muted">{similarMsg}</div>}
            <div className="btn-row">
              <button className="btn small" disabled={!f.online} onClick={() => api.revealFile(f.id)}>탐색기에서 보기</button>
              <button className="btn small" disabled={!f.online} onClick={() => api.openFile(f.id)}>기본 앱으로 열기</button>
              {f.kind === "video" && vlc && <button className="btn small" disabled={!f.online} onClick={playInVlc}>VLC로 재생</button>}
            </div>
            {vlcMsg && <div className="small muted">{vlcMsg}</div>}
            <div className="muted small">
              ← → 이동 · {review ? "K 남기기 · Delete 삭제 목록 · " : ""}Esc {review ? "끝내기" : "닫기"} · {index + 1} / {items.length}
            </div>
          </>
        ) : (
          <div className="muted">불러오는 중…</div>
        )}
      </aside>
      {comparing && (
        <div onClick={(e) => e.stopPropagation()}>
          <Compare ids={comparing} onClose={() => setComparing(null)} />
        </div>
      )}
    </div>
  );
}
