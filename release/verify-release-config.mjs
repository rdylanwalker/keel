#!/usr/bin/env node
// Regression check for .releaserc.json (SDD §9.3).
//
// Usage: npm ci --prefix release --ignore-scripts && node release/verify-release-config.mjs <repo-root>
//
// Loads the pinned release tooling from release/node_modules, resolving plugins the same
// way @semantic-release/core's loadPlugin() does at runtime, and asserts that:
//   * release notes list ⚠ BREAKING CHANGES → Features → Bug Fixes → Performance
//     Improvements → Reverts, and hidden types (chore, docs, ...) stay out of the notes;
//   * the commit analyzer bumps breaking → minor (pre-1.0), feat → minor,
//     fix/perf/revert → patch and chore → no release.
import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import assert from "node:assert/strict";

const repo = resolve(process.argv[2] ?? ".");
const releaseDir = `${repo}/release`;
const coreUtils = `${releaseDir}/node_modules/@semantic-release/core/lib/plugins/utils.js`;
const topReq = createRequire(`${releaseDir}/package.json`);
// semantic-release resolves plugin names relative to @semantic-release/core; fall back to
// semantic-release itself if a future layout drops the core package.
const runtimeReq = existsSync(coreUtils)
  ? createRequire(coreUtils)
  : createRequire(topReq.resolve("semantic-release"));
const load = async (req, name) => import(pathToFileURL(req.resolve(name)).href);

const { generateNotes } = await load(runtimeReq, "@semantic-release/release-notes-generator");
const analyzers = {
  runtime: (await load(runtimeReq, "@semantic-release/commit-analyzer")).analyzeCommits,
  pinned: (await load(topReq, "@semantic-release/commit-analyzer")).analyzeCommits,
};

const rc = JSON.parse(readFileSync(`${repo}/.releaserc.json`, "utf8"));
const cfg = (name) => {
  const entry = rc.plugins.find((p) => Array.isArray(p) && p[0] === name);
  assert.ok(entry, `${name} must be configured with options in .releaserc.json`);
  return entry[1];
};
const notesCfg = cfg("@semantic-release/release-notes-generator");
const analyzerCfg = cfg("@semantic-release/commit-analyzer");

const logger = { log() {}, error() {}, warn() {}, success() {} };
let seq = 0;
const mk = (message) => ({ hash: (++seq).toString(16).padStart(40, "0"), message });
const ctx = (commits) => ({
  cwd: repo,
  env: {},
  logger,
  commits,
  options: { repositoryUrl: "https://github.com/example/keel.git" },
  lastRelease: { gitTag: "v0.8.0", version: "0.8.0" },
  nextRelease: { gitTag: "v0.9.0", version: "0.9.0" },
});

// --- release notes: section order and hidden types --------------------------------
const commits = [
  mk("fix(core): correct parser edge case"),
  mk("feat(ui): add topology view"),
  mk("feat(server)!: drop legacy route\n\nBREAKING CHANGE: /api/v0 is removed"),
  mk("perf(kube): cache watchers"),
  mk("chore: bump deps"),
  mk("revert: feat(ui): add topology view\n\nThis reverts commit bbbbbbb."),
  mk("docs: tweak readme"),
  mk("ci: tune workflow"),
];
const notes = await generateNotes(notesCfg, ctx(commits));
const order = [
  "### ⚠ BREAKING CHANGES",
  "### Features",
  "### Bug Fixes",
  "### Performance Improvements",
  "### Reverts",
];
const idx = order.map((h) => notes.indexOf(h));
idx.forEach((i, n) => assert.ok(i >= 0, `release notes are missing "${order[n]}":\n${notes}`));
for (let n = 1; n < idx.length; n++) {
  assert.ok(idx[n - 1] < idx[n], `"${order[n - 1]}" must precede "${order[n]}":\n${notes}`);
}
assert.ok(notes.includes("/api/v0 is removed"), "BREAKING CHANGE footer text missing");
for (const hidden of ["bump deps", "tweak readme", "tune workflow"]) {
  assert.ok(!notes.includes(hidden), `hidden commit "${hidden}" leaked into notes:\n${notes}`);
}

// `feat!` without a footer is still listed as a breaking change, ahead of Features.
const bangOnly = await generateNotes(
  notesCfg,
  ctx([mk("feat(cli)!: rename flag"), mk("fix: small fix")]),
);
assert.ok(
  bangOnly.includes("BREAKING CHANGES") &&
    bangOnly.indexOf("BREAKING CHANGES") < bangOnly.indexOf("### Features"),
  `feat! must render a leading BREAKING CHANGES section:\n${bangOnly}`,
);

// --- commit analyzer: release types ---------------------------------------------
const cases = [
  ["feat(server)!: drop route\n\nBREAKING CHANGE: gone", "minor"],
  ["feat(cli)!: rename flag", "minor"],
  ["fix(x): y\n\nBREAKING CHANGE: z", "minor"],
  ["feat(ui): add thing", "minor"],
  ["fix(core): bug", "patch"],
  ["perf(kube): faster", "patch"],
  ["revert: feat(ui): add thing", "patch"],
  ['Revert "feat(ui): add thing"\n\nThis reverts commit abc1234.', "patch"],
  ["chore: deps", null],
  ["docs: readme", null],
  ["ci: workflow", null],
];
for (const [label, analyzeCommits] of Object.entries(analyzers)) {
  for (const [message, expected] of cases) {
    const got = await analyzeCommits(analyzerCfg, ctx([mk(message)]));
    assert.equal(got, expected, `[${label} analyzer] ${JSON.stringify(message)}`);
  }
  assert.equal(await analyzeCommits(analyzerCfg, ctx(commits)), "minor", `[${label}] mixed set`);
}

console.log("OK: release note section order, hidden types and release types verified");
