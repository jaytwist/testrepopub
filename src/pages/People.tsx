import { useCallback, useEffect, useRef, useState } from "react";
import { confirm } from "@tauri-apps/plugin-dialog";
import {
  api, faceUrl, fmtCount, onAnalysisDone, onAnalysisStatus,
  type AnalysisOptions, type FaceRow, type LaneStatus, type PairSuggestion, type PersonRow,
} from "../api";
import { useJob, useNav } from "../App";

const label = (p: PersonRow) => p.name ?? "이름 없음";

/** Round face crop. */
function Face({ id, size = 96 }: { id: number | null; size?: number }) {
  return (
    <span className="face" style={{ width: size, height: size }}>
      {id != null && <img src={faceUrl(id)} alt="" loading="lazy" draggable={false} />}
    </span>
  );
}

/** Name field: Enter saves; a name someone else already has offers to merge the two. */
function NameEditor({ person, onDone, autoFocus }: { person: PersonRow; onDone: () => void; autoFocus?: boolean }) {
  const [value, setValue] = useState(person.name ?? "");
  // Enter and the blur that follows must not save (and ask to merge) twice.
  const saving = useRef(false);
  const save = async () => {
    if (saving.current) return;
    saving.current = true;
    const name = value.trim();
    if (name === (person.name ?? "")) return onDone();
    const other = await api.renamePerson(person.id, name);
    if (other != null) {
      const ok = await confirm(`이미 「${name}」(이)라는 인물이 있습니다. 같은 사람으로 합칠까요?\n\n합치면 되돌릴 수 없습니다.`, {
        title: "같은 사람으로 합치기", kind: "info", okLabel: "합치기", cancelLabel: "취소",
      });
      if (ok) await api.mergePeople(other, [person.id]);
    }
    onDone();
  };
  return (
    <input
      className="name-input"
      autoFocus={autoFocus}
      placeholder="이름 추가"
      value={value}
      onChange={(e) => setValue(e.target.value)}
      onClick={(e) => e.stopPropagation()}
      onKeyDown={(e) => {
        if (e.key === "Enter") save();
        if (e.key === "Escape") onDone();
      }}
      onBlur={save}
    />
  );
}

export default function People() {
  const { version } = useJob();
  const nav = useNav();
  const [opts, setOpts] = useState<AnalysisOptions | null>(null);
  const [lane, setLane] = useState<LaneStatus | null>(null);
  const [people, setPeople] = useState<PersonRow[] | null>(null);
  const [showHidden, setShowHidden] = useState(false);
  const [selected, setSelected] = useState<Set<number>>(new Set());
  const [naming, setNaming] = useState<number | null>(null);
  const [pairs, setPairs] = useState<PairSuggestion[]>([]);
  const [skipped, setSkipped] = useState<Set<string>>(new Set());
  const [detail, setDetail] = useState<PersonRow | null>(null);
  const [err, setErr] = useState<string | null>(null);

  const load = useCallback(() => {
    api.peopleList(showHidden).then(setPeople).catch((e) => setErr(String(e)));
    api.peopleSuggestions().then(setPairs).catch(() => {});
  }, [showHidden]);

  useEffect(load, [load, version]);
  useEffect(() => {
    api.getAnalysisOptions().then(setOpts);
    api.analysisStatus().then(setLane);
    const a = onAnalysisStatus(() => api.analysisStatus().then(setLane));
    const b = onAnalysisDone(load);
    return () => {
      a.then((u) => u());
      b.then((u) => u());
    };
  }, [load]);
  // While analysing, new people appear as they are found.
  useEffect(() => {
    if (!lane?.running) return;
    const t = setInterval(load, 20000);
    return () => clearInterval(t);
  }, [lane?.running, load]);

  const toggle = (id: number) => {
    const next = new Set(selected);
    next.has(id) ? next.delete(id) : next.add(id);
    setSelected(next);
  };

  const mergeSelected = async () => {
    const chosen = (people ?? []).filter((p) => selected.has(p.id));
    if (chosen.length < 2) return;
    const names = new Set(chosen.map((p) => p.name).filter(Boolean));
    if (names.size > 1 && !(await confirm(`서로 다른 이름(${[...names].join(", ")})이 있습니다. 그래도 한 사람으로 합칠까요?`, { title: "합치기", kind: "warning" }))) return;
    // Keep the named one, else the one with the most photos.
    const into = [...chosen].sort((a, b) => Number(!!b.name) - Number(!!a.name) || b.photos - a.photos)[0];
    await api.mergePeople(into.id, chosen.filter((p) => p.id !== into.id).map((p) => p.id));
    setSelected(new Set());
    load();
  };

  /** Photos of any of the selected people, into person folders ({person}/{year}). */
  const organizeSelected = async () => {
    const lists = await Promise.all([...selected].map((id) => api.queryIds({ person_ids: [id] })));
    nav.openOrganize({ ids: [...new Set(lists.flat())] }, "{person}/{year}");
  };

  const hideSelected = async (hidden: boolean) => {
    await api.hidePeople([...selected], hidden);
    setSelected(new Set());
    load();
  };

  const turnOn = async () => {
    if (!opts) return;
    const next = { ...opts, people: true };
    await api.setAnalysisOptions(next);
    setOpts(next);
  };

  const pair = pairs.find((p) => !skipped.has(`${p.a.id}-${p.b.id}`));
  const answer = async (same: boolean | null) => {
    if (!pair) return;
    if (same == null) setSkipped(new Set(skipped).add(`${pair.a.id}-${pair.b.id}`));
    else {
      await api.answerPeoplePair(pair.a.id, pair.b.id, same);
      load();
    }
  };

  if (detail) {
    return <PersonDetail key={detail.id} person={detail} onBack={() => { setDetail(null); load(); }} onOpen={(p) => openPhotos(p)} onGo={setDetail} />;
  }

  function openPhotos(p: PersonRow) {
    nav.openLibrary({ title: `${label(p)} · ${fmtCount(p.photos)}장`, filter: { person_ids: [p.id] }, groupBy: "month" });
  }

  const pending = lane ? lane.pending_online : 0;
  const off = opts && !opts.people;

  return (
    <div className="page people-page">
      <header className="page-head">
        <div>
          <h1>인물</h1>
          <div className="muted">같은 사람이 나온 사진을 자동으로 묶습니다. 얼굴 정보는 이 PC 안에서만 처리되고 밖으로 나가지 않습니다.</div>
        </div>
        <label className="check-row small">
          <input type="checkbox" checked={showHidden} onChange={(e) => setShowHidden(e.target.checked)} /> 숨긴 인물도 보기
        </label>
      </header>
      {err && <div className="alert bad">{err}</div>}

      {off && (
        <div className="card people-off">
          <b>인물 찾기가 꺼져 있습니다</b>
          <span className="muted small">켜면 아직 분석하지 않은 사진부터 얼굴을 찾습니다. 이미 분석한 사진은 얼굴을 찾기 위해 한 번 더 읽습니다.</span>
          <div><button className="btn primary" onClick={turnOn}>인물 찾기 켜기</button></div>
        </div>
      )}
      {!off && lane && !lane.faces_available && (
        <div className="alert">인물 기능에 필요한 파일(AI 모델)이 이 설치에 없습니다. 정식 설치 파일이나 최신 휴대용 버전을 사용하세요.</div>
      )}
      {!off && lane && (
        <div className={`people-diag ${lane.last_error ? "bad" : ""}`}>
          <span>
            얼굴 분석: {pending > 0 ? <>남은 사진 <b>{fmtCount(pending)}</b>장{lane.running ? (lane.paused ? " (쉬는 중)" : " · 진행 중") : " · 대기"}</> : "모두 끝남"}
          </span>
          <span>찾은 얼굴 <b>{fmtCount(lane.faces_found)}</b>개</span>
          <span>엔진: {lane.provider === "DirectML" ? "그래픽카드(DirectML)" : lane.provider === "CPU" ? "CPU" : lane.faces_available ? "아직 시작 전" : "AI 파일 없음"}</span>
          {pending > 0 && <span className="muted">인물은 분석 중에도 몇 분마다 나타납니다</span>}
          {lane.last_error && (
            <>
              <span className="warn-text">{lane.last_error}</span>
              <button className="btn small" onClick={() => api.analysisStart()}>다시 시도</button>
            </>
          )}
        </div>
      )}

      {pair && (
        <div className="card same-person">
          <div className="sp-faces">
            <div><Face id={pair.a.cover_face_id} size={72} /><span className="small">{label(pair.a)}</span></div>
            <div><Face id={pair.b.cover_face_id} size={72} /><span className="small">{label(pair.b)}</span></div>
          </div>
          <div className="sp-ask">
            <b>같은 사람인가요?</b>
            {pair.shared_events > 0 && (
              <span className="muted small">같은 폴더·같은 시기 사진 {fmtCount(pair.shared_events)}곳에 둘 다 나옵니다</span>
            )}
            <div className="btn-row">
              <button className="btn primary" onClick={() => answer(true)}>같음</button>
              <button className="btn" onClick={() => answer(false)}>다름</button>
              <button className="btn ghost" onClick={() => answer(null)}>모르겠음</button>
            </div>
          </div>
        </div>
      )}

      {selected.size > 0 && (
        <div className="selbar">
          <b>{selected.size}명 선택</b>
          <button className="btn small" disabled={selected.size < 2} onClick={mergeSelected}>같은 사람으로 합치기</button>
          <button className="btn small" onClick={organizeSelected} title="고른 사람들이 나온 사진을 사람별 폴더로 정리">⇲ 사람별 폴더로 정리</button>
          {showHidden && [...selected].every((id) => people?.find((p) => p.id === id)?.hidden) ? (
            <button className="btn small" onClick={() => hideSelected(false)}>다시 보이기</button>
          ) : (
            <button className="btn small" onClick={() => hideSelected(true)}>숨기기</button>
          )}
          <button className="btn small ghost" onClick={() => setSelected(new Set())}>선택 해제</button>
        </div>
      )}

      {people && people.length === 0 && !off && (
        <div className="empty">
          <div className="empty-icon">☺</div>
          <h2>아직 찾은 인물이 없습니다</h2>
          <p>
            같은 사람이 나온 사진이 3장 이상 모이면 인물로 묶어 여기에 보여 드립니다.
            {lane && ` 지금까지 찾은 얼굴 ${fmtCount(lane.faces_found)}개.`}
          </p>
        </div>
      )}

      <div className="person-grid">
        {(people ?? []).map((p) => (
          <div key={p.id} className={`person-card ${selected.has(p.id) ? "sel" : ""} ${p.hidden ? "hidden-person" : ""}`} onClick={() => (selected.size > 0 ? toggle(p.id) : setDetail(p))}>
            <button className="check" onClick={(e) => { e.stopPropagation(); toggle(p.id); }} title="선택">{selected.has(p.id) ? "✓" : ""}</button>
            <Face id={p.cover_face_id} size={112} />
            {naming === p.id ? (
              <NameEditor person={p} autoFocus onDone={() => { setNaming(null); load(); }} />
            ) : p.name ? (
              <b className="person-name" onClick={(e) => { e.stopPropagation(); setNaming(p.id); }} title="이름 바꾸기">{p.name}</b>
            ) : (
              <button className="btn small ghost add-name" onClick={(e) => { e.stopPropagation(); setNaming(p.id); }}>이름 추가</button>
            )}
            <span className="muted small">{fmtCount(p.photos)}장</span>
          </div>
        ))}
      </div>
    </div>
  );
}

/** Choose where to send faces: search, then a person (named first) or a new person. */
function SendPicker({ from, onPick, onClose }: { from: number; onPick: (to: PersonRow | null) => void; onClose: () => void }) {
  const [all, setAll] = useState<PersonRow[] | null>(null);
  const [q, setQ] = useState("");
  useEffect(() => {
    api.peopleList(true).then(setAll).catch(() => setAll([]));
  }, []);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  const needle = q.trim().toLowerCase();
  const list = (all ?? [])
    .filter((x) => x.id !== from && (!needle || (x.name ?? "").toLowerCase().includes(needle)))
    .sort((a, b) => Number(b.name != null) - Number(a.name != null) || Number(a.hidden) - Number(b.hidden) || b.photos - a.photos);
  return (
    <div className="send-back" onClick={onClose}>
      <div className="send-picker card" onClick={(e) => e.stopPropagation()}>
        <b>어느 인물로 보낼까요?</b>
        <input autoFocus placeholder="이름으로 찾기" value={q} onChange={(e) => setQ(e.target.value)}
          onKeyDown={(e) => { if (e.key === "Enter" && list.length > 0) onPick(list[0]); }} />
        <div className="send-list">
          <button className="send-row new" onClick={() => onPick(null)}>
            <span className="face" style={{ width: 40, height: 40 }}>＋</span>
            <span>새 인물로</span>
          </button>
          {all == null && <span className="muted small">불러오는 중…</span>}
          {list.map((x) => (
            <button key={x.id} className="send-row" onClick={() => onPick(x)}>
              <Face id={x.cover_face_id} size={40} />
              <span className={x.name ? "" : "muted"}>{label(x)}{x.hidden ? " (숨김)" : ""}</span>
              <span className="muted small">{fmtCount(x.photos)}장</span>
            </button>
          ))}
        </div>
      </div>
    </div>
  );
}

/** One person: their faces, "not this person", sending faces to someone else, cover choice,
 *  and the way to their photos. */
function PersonDetail({ person, onBack, onOpen, onGo }: {
  person: PersonRow; onBack: () => void; onOpen: (p: PersonRow) => void; onGo: (p: PersonRow) => void;
}) {
  const nav = useNav();
  const [faces, setFaces] = useState<FaceRow[] | null>(null);
  const [picked, setPicked] = useState<Set<number>>(new Set());
  const [anchor, setAnchor] = useState<number | null>(null);
  const [p, setP] = useState(person);
  const [naming, setNaming] = useState(false);
  const [sending, setSending] = useState(false);
  const [sent, setSent] = useState<{ n: number; to: PersonRow } | null>(null);

  const load = useCallback(() => {
    api.personFaces(person.id).then(setFaces);
    api.peopleList(true).then((all) => {
      const me = all.find((x) => x.id === person.id);
      if (me) setP(me);
      else onBack(); // merged away
    });
  }, [person.id, onBack]);
  useEffect(load, [load]);

  /** Click toggles; Shift+click selects the range from the last clicked face. */
  const toggle = (id: number, range = false) => {
    const next = new Set(picked);
    const ids = (faces ?? []).map((f) => f.id);
    if (range && anchor != null && ids.includes(anchor)) {
      const [a, b] = [ids.indexOf(anchor), ids.indexOf(id)].sort((x, y) => x - y);
      ids.slice(a, b + 1).forEach((x) => next.add(x));
    } else {
      next.has(id) ? next.delete(id) : next.add(id);
    }
    setAnchor(id);
    setPicked(next);
  };
  const sendTo = async (to: PersonRow | null) => {
    setSending(false);
    const n = picked.size;
    const id = await api.moveFaces([...picked], p.id, to?.id ?? null);
    setPicked(new Set());
    const target = to ?? (await api.peopleList(true)).find((x) => x.id === id);
    if (target) setSent({ n, to: target });
    load();
  };
  const notThem = async () => {
    await api.rejectFaces([...picked], p.id);
    setPicked(new Set());
    load();
  };
  const makeCover = async () => {
    await api.setPersonCover(p.id, [...picked][0]);
    setPicked(new Set());
    load();
  };

  return (
    <div className="page people-page">
      <header className="page-head person-head">
        <button className="btn ghost" onClick={onBack}>‹ 인물</button>
        <Face id={p.cover_face_id} size={72} />
        <div>
          {naming ? (
            <NameEditor person={p} autoFocus onDone={() => { setNaming(false); load(); }} />
          ) : (
            <h1 className="person-title" onClick={() => setNaming(true)} title="이름 바꾸기">{p.name ?? "이름 추가"}</h1>
          )}
          <div className="muted">사진 {fmtCount(p.photos)}장 · 다른 사람 얼굴을 골라 「→ 다른 인물로 보내기」나 「이 사람 아님」을 누르세요</div>
        </div>
        <button className="btn" onClick={() => nav.openOrganize({ person_ids: [p.id] }, "{person}/{year}")} title="이 사람이 나온 사진을 사람별 폴더로 복사·이동">
          ⇲ 이 사람 사진 한 곳으로 정리
        </button>
        <button className="btn primary" onClick={() => onOpen(p)}>사진 보기</button>
      </header>

      {picked.size > 0 && (
        <div className="selbar">
          <b>얼굴 {picked.size}개 선택</b>
          <button className="btn small primary" onClick={() => setSending(true)}>→ 다른 인물로 보내기</button>
          <button className="btn small danger" onClick={notThem}>이 사람 아님</button>
          <button className="btn small" disabled={picked.size !== 1} onClick={makeCover}>대표 얼굴로</button>
          {faces && picked.size < faces.length && (
            <button className="btn small ghost" onClick={() => setPicked(new Set(faces.map((f) => f.id)))}>모두 선택</button>
          )}
          <button className="btn small ghost" onClick={() => setPicked(new Set())}>선택 해제</button>
          <span className="muted small">Shift+클릭: 여러 개 한 번에</span>
        </div>
      )}
      {sent && (
        <div className="alert ok sent-note">
          얼굴 {fmtCount(sent.n)}개를 「{label(sent.to)}」(으)로 보냈습니다.
          <button className="btn small" onClick={() => onGo(sent.to)}>그 인물 보기</button>
          <button className="btn small ghost" onClick={() => setSent(null)}>닫기</button>
        </div>
      )}
      {sending && <SendPicker from={p.id} onPick={sendTo} onClose={() => setSending(false)} />}

      <div className="face-grid">
        {(faces ?? []).map((f) => (
          <button key={f.id} className={`face-tile ${picked.has(f.id) ? "sel" : ""}`} onClick={(e) => toggle(f.id, e.shiftKey)} title={f.locked ? "확인된 얼굴" : ""}>
            <img src={faceUrl(f.id)} alt="" loading="lazy" draggable={false} />
            {picked.has(f.id) && <span className="tick">✓</span>}
          </button>
        ))}
      </div>
    </div>
  );
}
