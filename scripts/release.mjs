#!/usr/bin/env node
// Bump the version everywhere, commit, tag, and push. GitHub Actions then builds the signed
// installers and the update feed; installed apps offer "업데이트 후 재시작" on their next start.
//
//   npm run release -- 0.2.0 "바뀐 점: 정렬 기능 추가, 버그 수정"
//   npm run release -- 0.2.0 "notes" --no-push     (commit + tag only)
import { execSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";

const [version, notes = "", ...flags] = process.argv.slice(2);
if (!/^\d+\.\d+\.\d+$/.test(version ?? "")) {
  console.error('사용법: npm run release -- <x.y.z> "바뀐 점"');
  process.exit(1);
}
const run = (cmd) => execSync(cmd, { stdio: "inherit" });
const dirty = execSync("git status --porcelain").toString().trim();
if (dirty) {
  console.error("커밋하지 않은 변경이 있습니다. 먼저 커밋하세요:\n" + dirty);
  process.exit(1);
}

const edit = (file, fn) => writeFileSync(file, fn(readFileSync(file, "utf8")));
edit("package.json", (s) => s.replace(/"version": "[^"]+"/, `"version": "${version}"`));
edit("src-tauri/tauri.conf.json", (s) => s.replace(/"version": "[^"]+"/, `"version": "${version}"`));
edit("Cargo.toml", (s) => s.replace(/(\[workspace\.package\][^[]*?version = )"[^"]+"/, `$1"${version}"`));
// Keep Cargo.lock in step for our own crates.
edit("Cargo.lock", (s) =>
  s.replace(/(name = "(?:media-core|media-organizer)"\nversion = )"[^"]+"/g, `$1"${version}"`),
);
edit("package-lock.json", (s) => {
  const j = JSON.parse(s);
  j.version = version;
  if (j.packages?.[""]) j.packages[""].version = version;
  return JSON.stringify(j, null, 2) + "\n";
});

run("git add package.json package-lock.json src-tauri/tauri.conf.json Cargo.toml Cargo.lock");
run(`git commit -m "Release v${version}"`);
const msg = notes || `Media Organizer v${version}`;
execSync(`git tag -a v${version} -F -`, { input: msg, stdio: ["pipe", "inherit", "inherit"] });
if (flags.includes("--no-push")) {
  console.log(`\n태그 v${version} 생성. 배포하려면: git push && git push origin v${version}`);
} else {
  run("git push");
  run(`git push origin v${version}`);
  console.log(`\nv${version} 배포 시작. GitHub의 Actions 탭에서 진행 상황을 볼 수 있습니다 (약 10–15분).`);
}
