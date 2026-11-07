import { confirm, message } from "@tauri-apps/plugin-dialog";
import { api, fmtBytes, fmtCount, type Blocked } from "./api";

const REASONS: Record<Blocked["reason"], string> = {
  last_copy: "마지막 한 장",
  original: "원본으로 지정",
  favorite: "♥ 즐겨찾기",
  keep_folder: "항상 남길 폴더",
};

/**
 * The one way files are deleted from the UI. Asks once, and when some of the files are protected
 * (original, ♥, a keep folder, or the last copy of a photo) offers to leave those out.
 * Starts the delete job and returns true, or returns false if the user cancelled.
 */
export async function confirmDelete(ids: number[], title = "삭제", bytes?: number): Promise<boolean> {
  if (ids.length === 0) return false;
  const size = bytes != null ? ` (${fmtBytes(bytes)})` : "";
  const blocked = await api.protectCheck(ids).catch(() => [] as Blocked[]);
  const after = "\n\n같은 드라이브의 격리 폴더로 옮겨지며, 「작업 기록」에서 되돌릴 수 있습니다.";
  if (blocked.length === 0) {
    const ok = await confirm(`${fmtCount(ids.length)}개${size}를 삭제할까요?${after}`, {
      title, kind: "warning", okLabel: "삭제", cancelLabel: "취소",
    });
    if (!ok) return false;
    await api.startDelete(ids);
    return true;
  }
  const counts = new Map<string, number>();
  blocked.forEach((b) => counts.set(REASONS[b.reason], (counts.get(REASONS[b.reason]) ?? 0) + 1));
  const why = [...counts].map(([r, n]) => `${r} ${fmtCount(n)}`).join(" · ");
  const rest = ids.length - blocked.length;
  const leaveOut = rest > 0 ? `보호된 것 빼고 ${fmtCount(rest)}개 삭제` : "삭제하지 않기";
  const answer = await message(
    `${fmtCount(ids.length)}개 중 ${fmtCount(blocked.length)}개는 보호되어 있습니다 (${why}).\n\n보호된 파일은 남겨 둘까요?${after}`,
    { title, kind: "warning", buttons: { yes: leaveOut, no: "보호된 것까지 모두 삭제", cancel: "취소" } },
  );
  if (answer === leaveOut || answer === "Yes") {
    if (rest === 0) return false;
    await api.startDelete(ids); // the backend leaves the protected ones in place
    return true;
  }
  if (answer === "보호된 것까지 모두 삭제" || answer === "No") {
    await api.startDelete(ids, true);
    return true;
  }
  return false;
}
