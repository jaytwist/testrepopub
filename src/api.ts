// Typed wrappers around the Tauri commands in src-tauri/src/lib.rs.
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface FileFilter {
  kind?: string | null;
  label?: string | null;
  volume_id?: number | null;
  source_id?: number | null;
  year?: number | null;
  month?: number | null;
  text?: string | null;
  date_confidence?: string | null;
  online_only?: boolean;
  ids?: number[] | null;
  date_from?: number | null;
  date_to?: number | null;
  size_min?: number | null;
  size_max?: number | null;
  mp_min?: number | null;
  mp_max?: number | null;
  dur_min?: number | null;
  dur_max?: number | null;
  camera?: string | null;
  ext?: string | null;
  has_gps?: boolean | null;
  dir?: string | null;
  suggest?: string | null;
  in_burst?: boolean;
  burst_id?: number | null;
  trip_id?: number | null;
  person_ids?: number[] | null;
  /** Backup check: "elsewhere" = also on another drive, "only_here" = on no other drive. */
  copies?: "elsewhere" | "only_here" | null;
  stack_bursts?: boolean;
  sort?: SortKey | null;
  desc?: boolean | null;
}

export type GroupBy = "month" | "year" | "folder" | "burst" | "none";
export interface GroupRow {
  key: string; count: number; bytes: number; year: number | null; month: number | null;
  volume_id: number | null; dir: string | null; mount: string | null; volume_label: string | null;
  burst_id: number | null; first_taken: number | null;
}
export interface DupRules { keep: string[]; drop: string[] }
export interface CompareInfo { files: FileDetail[]; identical: number[][]; recommended: number | null }
export interface PersonRow { id: number; name: string | null; cover_face_id: number | null; photos: number; hidden: boolean }
export interface FaceRow { id: number; file_id: number; locked: boolean }
/** A file that is not deleted unless forced, and why. */
export interface Blocked { id: number; reason: "last_copy" | "keep_folder" | "original" | "favorite" }

export interface PairSuggestion { a: PersonRow; b: PersonRow; similarity: number; shared_events: number }
export interface TripRow { id: number; title: string; start_ts: number; end_ts: number; count: number; cover_file_id: number | null; place: string }
export interface TripsSummary { trips: TripRow[]; home: string; located: number; total: number }
export interface Bucket { count: number; bytes: number }
export interface CleanupSummary {
  blurry: Bucket; burst_extras: Bucket; burst_groups: number; screenshots: Bucket; large_videos: Bucket;
  exact_duplicates: Bucket; total_bytes: number; analyzed: number; pending: number;
}
export type SortKey = "taken" | "mtime" | "size" | "name" | "resolution" | "duration";

export interface VolumeRow {
  id: number; uid: string; label: string; mount: string; removable: boolean; online: boolean;
  last_seen: number; file_count: number; total_bytes: number;
}
export interface SourceRow {
  id: number; volume_id: number; rel_root: string; last_scan: number; volume_label: string;
  mount: string; online: boolean; abs_path: string;
}
export interface FileRow {
  id: number; volume_id: number; rel_path: string; name: string; ext: string; kind: string; size: number;
  mtime: number; taken_at: number | null; date_source: string; width: number | null; height: number | null;
  duration_ms: number | null; camera: string | null; lat: number | null; lon: number | null;
  orientation: number | null; hash: string | null; missing: boolean; quarantined: boolean; mount: string; online: boolean;
}
export interface GridItem {
  id: number; kind: string; taken_at: number | null; width: number | null; height: number | null;
  duration_ms: number | null; online: boolean; size: number; mtime: number; name: string;
  burst_size: number | null; burst_rank: number | null; burst_id: number | null;
  /** Byte-identical copies on other drives. */
  copies: number;
  favorite: boolean;
  /** "original" / "copy" when byte-identical copies exist. */
  dup_role: "original" | "copy" | null;
}
export interface ValueCount { value: string; count: number }
export interface FilterOptions { cameras: ValueCount[]; exts: ValueCount[]; years: number[]; max_size: number; with_gps: number }
export interface FilteredStats {
  count: number; bytes: number; photos: number; videos: number; raws: number;
  first_taken: number | null; last_taken: number | null;
}
export interface ScanOptions {
  min_image_bytes: number; min_image_side: number; skip_hidden: boolean; skip_app_folders: boolean;
  skip_cloud_only: boolean; exclude_dirs: string[];
}
export interface SkipCounts {
  system_folders: number; hidden_folders: number; app_files: number; hidden_files: number;
  system_files: number; cloud_only: number; too_small: number;
}
export interface ScanStats {
  source_id: number; seen: number; added: number; updated: number; unchanged: number; moved: number; reused: number; missing: number;
  errors: number; skipped: SkipCounts;
}
export interface SuggestedFolder { label: string; path: string; kind: "user" | "drive"; already_added: boolean }
export interface MonthCount { year: number; month: number; count: number }
export interface LibraryStats {
  files: number; bytes: number; photos: number; videos: number; raws: number; offline_files: number;
  low_confidence_dates: number; quarantined_files: number; quarantined_bytes: number;
}
export interface LabelRow { label: string; source: string }
export interface LabelCount { label: string; count: number; user: boolean }
export interface FileDetail {
  file: FileRow; reasons: [string, string][]; labels: LabelRow[]; abs_path: string; taken: string; confidence: string; thumb_cached: boolean; place: string | null;
  people: [number, string][];
  dup_role: "original" | "copy" | null; dup_group_size: number;
}
export interface DupMember { file: FileRow; keep: boolean }
export interface DupGroup { id: number; kind: string; members: DupMember[]; reclaimable_bytes: number }
export interface DupSummary { kind: string; groups: number; files: number; reclaimable_bytes: number }
export interface OrganizeOptions {
  target_dir: string; template: string; rename: "keep" | "date_prefix"; mode: "move" | "copy";
  filter: FileFilter; uncertain_folder: string | null; remove_empty_dirs: boolean;
}
export interface PlanSummary {
  total: number; moves: number; copies: number; in_place: number; skip_duplicate: number; skip_offline: number;
  renamed: number; cross_volume: number; bytes: number; uncertain_dates: number; folders: number;
}
export interface PlanView { id: string; summary: PlanSummary; options: OrganizeOptions }
export interface PlanItem {
  file_id: number; src: string; dst: string;
  action: "move" | "copy" | "in_place" | "skip_duplicate" | "skip_offline";
  renamed: boolean; cross_volume: boolean; size: number;
}
export interface BatchRow {
  id: string; kind: string; description: string; created_at: number; op_count: number; state: string; bytes: number;
}
export interface Progress { job: string; stage: string; done: number; total: number; message: string }
export interface JobDone { job: string; ok: boolean; error: string | null; result: any }
export interface UpdateInfo { current: string; available: boolean; version: string | null; notes: string | null; date: string | null }
export interface UpdateProgress { downloaded: number; total: number | null }
export interface LaneStatus {
  running: boolean; paused: boolean; reason: "user" | "job" | "battery" | null; done: number; total: number;
  eta_secs: number; current: string; provider: string; pending_online: number; pending_offline: number;
  analyzed: number; faces_available: boolean; faces_found: number; last_error: string | null;
}
export interface AnalysisOptions { auto: boolean; people: boolean; gpu: boolean; pause_on_battery: boolean }
export interface AppInfo { version: string; data_dir: string; cache_dir: string; ffmpeg: boolean; job: string | null }

export const api = {
  appInfo: () => invoke<AppInfo>("app_info"),
  listVolumes: (refresh = false) => invoke<VolumeRow[]>("list_volumes", { refresh }),
  listSources: () => invoke<SourceRow[]>("list_sources"),
  addSource: (path: string) => invoke<number>("add_source", { path }),
  removeSource: (id: number) => invoke<void>("remove_source", { id }),
  startScan: (sourceId?: number) => invoke<void>("start_scan", { sourceId: sourceId ?? null }),
  getScanOptions: () => invoke<ScanOptions>("get_scan_options"),
  cleanupSummary: () => invoke<CleanupSummary>("cleanup_summary"),
  tripsSummary: () => invoke<TripsSummary>("trips_summary"),
  compareFiles: (ids: number[]) => invoke<CompareInfo>("compare_files", { ids }),
  getDupRules: () => invoke<DupRules>("get_dup_rules"),
  /** Save and re-pick keepers; returns how many groups changed. */
  setDupRules: (rules: DupRules) => invoke<number>("set_dup_rules", { rules }),
  dupRuleSuggestions: () => invoke<[string, number][]>("dup_rule_suggestions"),
  similarTo: (id: number) => invoke<number[]>("similar_to", { id }),
  setOriginal: (id: number) => invoke<void>("set_original", { id }),
  prepareCopies: () => invoke<number>("prepare_copies"),
  peopleList: (includeHidden = false) => invoke<PersonRow[]>("people_list", { includeHidden }),
  personFaces: (person: number) => invoke<FaceRow[]>("person_faces", { person }),
  peopleSuggestions: () => invoke<PairSuggestion[]>("people_suggestions"),
  /** Returns the id of another person who already has this name (then nothing changed). */
  renamePerson: (person: number, name: string) => invoke<number | null>("rename_person", { person, name }),
  mergePeople: (into: number, others: number[]) => invoke<void>("merge_people", { into, others }),
  rejectFaces: (faces: number[], person: number) => invoke<void>("reject_faces", { faces, person }),
  /** Send faces to another person (`to`), or to a new person (null). Returns where they went. */
  moveFaces: (faces: number[], from: number, to: number | null) => invoke<number>("move_faces", { faces, from, to }),
  hidePeople: (people: number[], hidden: boolean) => invoke<void>("hide_people", { people, hidden }),
  setPersonCover: (person: number, face: number) => invoke<void>("set_person_cover", { person, face }),
  answerPeoplePair: (a: number, b: number, same: boolean) => invoke<void>("answer_people_pair", { a, b, same }),
  clearFaceData: () => invoke<void>("clear_face_data"),
  burstExtraIds: () => invoke<number[]>("burst_extra_ids"),
  dismiss: (ids: number[], kind: string) => invoke<number>("dismiss", { ids, kind }),
  analysisStatus: () => invoke<LaneStatus>("analysis_status"),
  analysisPause: (paused: boolean) => invoke<void>("analysis_pause", { paused }),
  analysisStart: () => invoke<void>("analysis_start"),
  getAnalysisOptions: () => invoke<AnalysisOptions>("get_analysis_options"),
  setAnalysisOptions: (options: AnalysisOptions) => invoke<void>("set_analysis_options", { options }),
  setScanOptions: (options: ScanOptions) => invoke<void>("set_scan_options", { options }),
  defaultScanOptions: () => invoke<ScanOptions>("default_scan_options"),
  suggestedFolders: () => invoke<SuggestedFolder[]>("suggested_folders"),
  filterOptions: () => invoke<FilterOptions>("filter_options"),
  filteredStats: (filter: FileFilter) => invoke<FilteredStats>("filtered_stats", { filter }),
  groupSummary: (filter: FileFilter, groupBy: GroupBy) => invoke<GroupRow[]>("group_summary", { filter, groupBy }),
  startThumbs: () => invoke<void>("start_thumbs"),
  cancelJob: () => invoke<void>("cancel_job"),

  libraryStats: () => invoke<LibraryStats>("library_stats"),
  timeline: (filter: FileFilter) => invoke<MonthCount[]>("timeline", { filter }),
  grid: (filter: FileFilter, offset: number, limit: number) => invoke<GridItem[]>("grid", { filter, offset, limit }),
  queryIds: (filter: FileFilter) => invoke<number[]>("query_ids", { filter }),
  fileDetail: (id: number) => invoke<FileDetail>("file_detail", { id }),
  previewPath: (id: number) => invoke<string>("preview_path", { id }),
  videoPreviewPath: (id: number) => invoke<string>("video_preview_path", { id }),
  revealFile: (id: number) => invoke<void>("reveal_file", { id }),
  vlcPath: () => invoke<string | null>("vlc_path"),
  openInVlc: (id: number) => invoke<void>("open_in_vlc", { id }),
  checkUpdate: () => invoke<UpdateInfo>("check_update"),
  installUpdate: () => invoke<void>("install_update"),
  openFile: (id: number) => invoke<void>("open_file", { id }),

  labelCounts: () => invoke<LabelCount[]>("label_counts"),
  addLabel: (ids: number[], label: string) => invoke<number>("add_label", { ids, label }),
  removeLabel: (ids: number[], label: string) => invoke<number>("remove_label", { ids, label }),
  setDate: (ids: number[], datetime: string) => invoke<number>("set_date", { ids, datetime }),
  shiftDate: (ids: number[], deltaSecs: number) => invoke<number>("shift_date", { ids, deltaSecs }),

  startDedupe: (kind: string, similarity?: string) => invoke<void>("start_dedupe", { kind, similarity: similarity ?? null }),
  dupSummary: () => invoke<DupSummary[]>("dup_summary"),
  dupGroups: (kind: string, offset: number, limit: number) => invoke<DupGroup[]>("dup_groups", { kind, offset, limit }),
  setKeep: (groupId: number, fileId: number, keep: boolean) => invoke<void>("set_keep", { groupId, fileId, keep }),
  startDedupeApply: (kind: string, groupIds?: number[]) => invoke<void>("start_dedupe_apply", { kind, groupIds: groupIds ?? null }),

  startPlan: (options: OrganizeOptions) => invoke<void>("start_plan", { options }),
  planItems: (planId: string, action: string | null, offset: number, limit: number) =>
    invoke<PlanItem[]>("plan_items", { planId, action, offset, limit }),
  startExecute: (planId: string) => invoke<void>("start_execute", { planId }),

  batches: () => invoke<BatchRow[]>("batches"),
  startUndo: (batchId: string) => invoke<void>("start_undo", { batchId }),
  /** Quarantine files. Protected ones (see protectCheck) are left in place unless `force`. */
  startDelete: (ids: number[], force = false) => invoke<void>("start_delete", { ids, force }),
  protectCheck: (ids: number[]) => invoke<Blocked[]>("protect_check", { ids }),
  startPurge: (batchId: string) => invoke<void>("start_purge", { batchId }),
};

/** Absolute path of an indexed file using the drive's native separator. */
export function absPath(f: { mount: string; rel_path: string }): string {
  const sep = f.mount.includes("\\") ? "\\" : "/";
  const m = f.mount.endsWith(sep) ? f.mount : f.mount + sep;
  return m + f.rel_path.split("/").join(sep);
}

export const thumbUrl = (id: number) => convertFileSrc(String(id), "thumb");
export const faceUrl = (faceId: number) => convertFileSrc(`f-${faceId}`, "thumb");
export const fileUrl = (path: string) => convertFileSrc(path);

export function onProgress(cb: (p: Progress) => void): Promise<UnlistenFn> {
  return listen<Progress>("job-progress", (e) => cb(e.payload));
}
export function onUpdateProgress(cb: (p: UpdateProgress) => void): Promise<UnlistenFn> {
  return listen<UpdateProgress>("update-progress", (e) => cb(e.payload));
}
export function onAnalysisStatus(cb: (s: LaneStatus) => void): Promise<UnlistenFn> {
  return listen<LaneStatus>("analysis-status", (e) => cb(e.payload));
}
export function onDriveConnected(cb: (d: { volume_id: number; label: string }) => void): Promise<UnlistenFn> {
  return listen<{ volume_id: number; label: string }>("drive-connected", (e) => cb(e.payload));
}
export function onAnalysisDone(cb: () => void): Promise<UnlistenFn> {
  return listen("analysis-done", () => cb());
}
export function onJobDone(cb: (d: JobDone) => void): Promise<UnlistenFn> {
  return listen<JobDone>("job-done", (e) => cb(e.payload));
}

/** Resolve when the named job (prefix match) finishes. */
export function waitJob(prefix: string): Promise<JobDone> {
  return new Promise((resolve) => {
    let un: UnlistenFn | undefined;
    onJobDone((d) => {
      if (d.job.startsWith(prefix)) {
        un?.();
        resolve(d);
      }
    }).then((u) => (un = u));
  });
}

// ---------------------------------------------------------------------------------------------
// Formatting
// ---------------------------------------------------------------------------------------------

export function fmtBytes(n: number): string {
  if (!n) return "0 B";
  const u = ["B", "KB", "MB", "GB", "TB"];
  const i = Math.min(u.length - 1, Math.floor(Math.log(n) / Math.log(1024)));
  return `${(n / 1024 ** i).toFixed(i >= 3 ? 2 : i ? 1 : 0)} ${u[i]}`;
}

export function fmtCount(n: number): string {
  return n.toLocaleString("ko-KR");
}

/** "YYYY-MM-DD" (from <input type="date">) to a wall-clock timestamp; endOfDay for range ends. */
export function dateInputToTs(v: string, endOfDay = false): number | null {
  if (!v) return null;
  const [y, m, d] = v.split("-").map(Number);
  return Date.UTC(y, m - 1, d, endOfDay ? 23 : 0, endOfDay ? 59 : 0, endOfDay ? 59 : 0) / 1000;
}
export function tsToDateInput(ts: number | null | undefined): string {
  if (ts == null) return "";
  const d = new Date(ts * 1000);
  const p = (x: number) => String(x).padStart(2, "0");
  return `${d.getUTCFullYear()}-${p(d.getUTCMonth() + 1)}-${p(d.getUTCDate())}`;
}

/** taken_at is camera wall-clock time stored as UTC seconds. */
export function fmtDate(ts: number | null, withTime = false): string {
  if (ts == null) return "날짜 없음";
  const d = new Date(ts * 1000);
  const p = (x: number) => String(x).padStart(2, "0");
  const s = `${d.getUTCFullYear()}.${p(d.getUTCMonth() + 1)}.${p(d.getUTCDate())}`;
  return withTime ? `${s} ${p(d.getUTCHours())}:${p(d.getUTCMinutes())}` : s;
}

export function toDatetimeLocal(ts: number | null): string {
  if (ts == null) return "";
  const d = new Date(ts * 1000);
  const p = (x: number) => String(x).padStart(2, "0");
  return `${d.getUTCFullYear()}-${p(d.getUTCMonth() + 1)}-${p(d.getUTCDate())}T${p(d.getUTCHours())}:${p(d.getUTCMinutes())}`;
}

/** "약 2시간 10분" style remaining time. */
export function fmtEta(secs: number): string {
  if (!secs || secs < 60) return "1분 이내";
  const m = Math.round(secs / 60);
  if (m < 60) return `약 ${m}분`;
  const h = Math.floor(m / 60);
  return `약 ${h}시간${m % 60 ? ` ${m % 60}분` : ""}`;
}

export function fmtDuration(ms: number | null): string {
  if (!ms) return "";
  const s = Math.round(ms / 1000);
  const m = Math.floor(s / 60);
  const h = Math.floor(m / 60);
  const p = (x: number) => String(x).padStart(2, "0");
  return h ? `${h}:${p(m % 60)}:${p(s % 60)}` : `${m}:${p(s % 60)}`;
}

/** The user label behind ♥ favorites. */
export const FAVORITE = "favorite";
/** The user label that marks "this copy is the original". */
export const ORIGINAL = "original";
/** Labels with their own controls (♥, original), kept out of the free-form label chips. */
export const SYSTEM_LABELS = [FAVORITE, ORIGINAL];

const LABEL_NAMES: Record<string, string> = {
  favorite: "♥ 즐겨찾기",
  original: "원본으로 지정",
  screenshot: "스크린샷",
  "src:kakaotalk": "카카오톡",
  "src:whatsapp": "WhatsApp",
  "src:telegram": "텔레그램",
  "src:line": "LINE",
  "src:instagram": "인스타그램",
  "src:download": "다운로드",
  edited: "편집본",
  gps: "위치정보",
  "date:uncertain": "날짜 불확실",
};

export function labelName(l: string): string {
  if (LABEL_NAMES[l]) return LABEL_NAMES[l];
  if (l.startsWith("camera:")) return `📷 ${l.slice(7)}`;
  return l;
}

export const DATE_SOURCE_NAMES: Record<string, string> = {
  exif: "사진 정보(EXIF)",
  video: "동영상 정보",
  user: "직접 지정",
  filename: "파일 이름",
  folder: "폴더 이름",
  mtime: "파일 수정일(추정)",
};

export const KIND_NAMES: Record<string, string> = { photo: "사진", video: "동영상", raw: "RAW", screenshot: "스크린샷" };
