import { useEffect, useState } from "react";
import { api, labelName, type FileFilter, type LabelCount, type MonthCount, type VolumeRow, fmtCount } from "../api";
import { useJob } from "../App";

interface Props {
  filter: FileFilter;
  onChange: (f: FileFilter) => void;
  timeline?: MonthCount[];
}

export default function FilterBar({ filter, onChange, timeline }: Props) {
  const { version } = useJob();
  const [labels, setLabels] = useState<LabelCount[]>([]);
  const [volumes, setVolumes] = useState<VolumeRow[]>([]);
  const [text, setText] = useState(filter.text ?? "");

  useEffect(() => {
    api.labelCounts().then(setLabels).catch(() => {});
    api.listVolumes().then(setVolumes).catch(() => {});
  }, [version]);

  // Debounce free-text search.
  useEffect(() => {
    const t = setTimeout(() => {
      if ((filter.text ?? "") !== text) onChange({ ...filter, text: text || null });
    }, 300);
    return () => clearTimeout(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [text]);

  const years = Array.from(new Set((timeline ?? []).map((m) => m.year).filter((y) => y > 0))).sort((a, b) => b - a);
  const set = (patch: Partial<FileFilter>) => onChange({ ...filter, ...patch });
  const kinds: [string | null, string][] = [[null, "전체"], ["photo", "사진"], ["video", "동영상"], ["raw", "RAW"], ["screenshot", "스크린샷"]];
  const userLabels = labels.filter((l) => l.user || !l.label.includes(":") || l.label.startsWith("src:") || l.label.startsWith("camera:"));

  return (
    <div className="filterbar">
      <div className="seg">
        {kinds.map(([k, name]) => (
          <button key={name} className={(filter.kind ?? null) === k ? "on" : ""} onClick={() => set({ kind: k })}>{name}</button>
        ))}
      </div>
      <select value={filter.year ?? ""} onChange={(e) => set({ year: e.target.value ? Number(e.target.value) : null, month: null })}>
        <option value="">모든 연도</option>
        {years.map((y) => <option key={y} value={y}>{y}년</option>)}
      </select>
      {filter.year != null && (
        <select value={filter.month ?? ""} onChange={(e) => set({ month: e.target.value ? Number(e.target.value) : null })}>
          <option value="">모든 월</option>
          {Array.from({ length: 12 }, (_, i) => i + 1).map((m) => <option key={m} value={m}>{m}월</option>)}
        </select>
      )}
      <select value={filter.label ?? ""} onChange={(e) => set({ label: e.target.value || null })}>
        <option value="">모든 라벨</option>
        {userLabels.map((l) => <option key={l.label} value={l.label}>{labelName(l.label)} ({fmtCount(l.count)})</option>)}
      </select>
      <select value={filter.volume_id ?? ""} onChange={(e) => set({ volume_id: e.target.value ? Number(e.target.value) : null })}>
        <option value="">모든 드라이브</option>
        {volumes.map((v) => <option key={v.id} value={v.id}>{v.online ? "● " : "○ "}{v.label} ({v.mount})</option>)}
      </select>
      <select value={filter.date_confidence ?? ""} onChange={(e) => set({ date_confidence: e.target.value || null })}>
        <option value="">날짜: 전체</option>
        <option value="high">날짜 확실 (EXIF)</option>
        <option value="medium">파일명에서 추출</option>
        <option value="low">날짜 확인 필요</option>
      </select>
      <input className="search" placeholder="이름·경로·카메라·라벨 검색" value={text} onChange={(e) => setText(e.target.value)} />
    </div>
  );
}
