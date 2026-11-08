import { useEffect, useRef, useState } from "react";
import { open } from "@tauri-apps/plugin-dialog";
import { api, fmtBytes, fmtCount, type Card, type ImportPreview, type ImportSettings } from "../api";
import { useJob } from "../App";

const STRUCTURES: { label: string; template: string; example: string }[] = [
  { label: "연도 / 연-월", template: "{year}/{year}-{month}", example: "2026/2026-11/IMG_0001.JPG" },
  { label: "연-월", template: "{year}-{month}", example: "2026-11/IMG_0001.JPG" },
  { label: "연도 / 날짜", template: "{year}/{date}", example: "2026/2026-11-08/IMG_0001.JPG" },
  { label: "그대로 한 폴더", template: "", example: "IMG_0001.JPG" },
];

/**
 * Import from a camera card: only photos not already on another drive are copied into the backup
 * folder (recognised by content, even if renamed), indexed while copying — the backup drive is not
 * scanned. Undoable from 「작업 기록」.
 */
export default function ImportPanel({ card, onClose }: { card: Card; onClose: () => void }) {
  const { running, setRunning, lastDone } = useJob();
  const [settings, setSettings] = useState<ImportSettings | null>(null);
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [phase, setPhase] = useState<"idle" | "checking" | "ready" | "importing">("idle");
  const started = useRef(false);

  useEffect(() => {
    api.getImportSettings().then(setSettings).catch((e) => setErr(String(e)));
  }, []);

  const check = async () => {
    setErr(null);
    setPhase("checking");
    try {
      await api.startImportPreview(card.mount);
      setRunning("import:preview");
    } catch (e) {
      setErr(String(e));
      setPhase("idle");
    }
  };
  // Look at the card as soon as the panel opens.
  useEffect(() => {
    if (!started.current && !running) {
      started.current = true;
      check();
    }
  }, []); // eslint-disable-line react-hooks/exhaustive-deps

  useEffect(() => {
    if (!lastDone) return;
    if (lastDone.job === "import:preview" && phase === "checking") {
      if (lastDone.ok) {
        setPreview(lastDone.result as ImportPreview);
        setPhase("ready");
      } else {
        setErr(lastDone.error ?? "카드를 읽지 못했습니다");
        setPhase("idle");
      }
    }
    if (lastDone.job === "import" && phase === "importing") {
      if (lastDone.ok) onClose();
      else {
        setErr(lastDone.error ?? "가져오지 못했습니다");
        setPhase("ready");
      }
    }
  }, [lastDone]); // eslint-disable-line react-hooks/exhaustive-deps

  const save = async (s: ImportSettings) => {
    setSettings(s);
    await api.setImportSettings(s).catch((e) => setErr(String(e)));
  };
  const pickTarget = async () => {
    const dir = await open({ directory: true, defaultPath: settings?.target_dir || undefined, title: "가져올 위치 (백업 폴더)" });
    if (typeof dir === "string" && settings) save({ ...settings, target_dir: dir });
  };

  const go = async () => {
    if (!settings?.target_dir) return pickTarget();
    setErr(null);
    setPhase("importing");
    try {
      await api.startImport(card.mount);
      setRunning("import");
    } catch (e) {
      setErr(String(e));
      setPhase("ready");
    }
  };

  const cancel = async () => {
    if (preview && phase !== "importing") await api.importCancel(preview.source_id).catch(() => {});
    onClose();
  };

  const structure = STRUCTURES.find((s) => s.template === settings?.template);

  return (
    <div className="send-back" onClick={cancel}>
      <div className="card import-panel" onClick={(e) => e.stopPropagation()}>
        <div className="cc-head">
          <span className="cc-icon">⇩</span>
          <h2>SD 카드에서 가져오기</h2>
        </div>
        <div className="muted small">
          「{card.label}」 ({card.mount}) — 이미 백업된 사진은 이름이 달라도 알아보고 건너뜁니다. 백업 드라이브는 다시 훑지 않습니다.
        </div>
        {err && <div className="alert bad">{err}</div>}

        <div className="import-row">
          <span className="muted">가져올 위치</span>
          <b className="mono path-tail" title={settings?.target_dir}><bdi>{settings?.target_dir || "아직 고르지 않음"}</bdi></b>
          <button className="btn small" onClick={pickTarget}>바꾸기</button>
        </div>
        <div className="import-row">
          <span className="muted">폴더 구조</span>
          <div className="seg">
            {STRUCTURES.map((s) => (
              <button key={s.label} className={settings?.template === s.template ? "on" : ""} onClick={() => settings && save({ ...settings, template: s.template })}>
                {s.label}
              </button>
            ))}
          </div>
        </div>
        {structure && <div className="muted small mono">예: {settings?.target_dir || "…"}/{structure.example}</div>}

        <div className="import-result">
          {phase === "checking" && <span className="muted">카드의 사진을 확인하는 중…</span>}
          {phase === "importing" && <span className="muted">복사하는 중… (진행 상황은 아래 막대에 나옵니다)</span>}
          {preview && phase === "ready" && (
            preview.new_files > 0 ? (
              <span>새 사진·동영상 <b>{fmtCount(preview.new_files)}</b>개 · {fmtBytes(preview.new_bytes)}
                {preview.already > 0 && <span className="muted"> (이미 있는 {fmtCount(preview.already)}개는 건너뜀)</span>}</span>
            ) : (
              <span>새로운 사진이 없습니다 <span className="muted">— {fmtCount(preview.already)}개 모두 이미 백업되어 있습니다</span></span>
            )
          )}
        </div>

        <div className="btn-row">
          <button className="btn primary" disabled={phase !== "ready" || !preview || preview.new_files === 0 || !!running} onClick={go}>
            {settings?.target_dir ? `${fmtCount(preview?.new_files ?? 0)}개 가져오기` : "가져올 위치 고르기"}
          </button>
          {phase === "idle" && <button className="btn" disabled={!!running} onClick={check}>다시 확인</button>}
          <button className="btn ghost" onClick={cancel} disabled={phase === "importing"}>닫기</button>
        </div>
        <div className="muted small">가져온 사진은 「작업 기록」에서 되돌릴 수 있습니다. SD 카드의 사진은 지우지 않습니다.</div>
      </div>
    </div>
  );
}
