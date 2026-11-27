import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { STRINGS, tr, fmtDur, fmtCu, fmtErr, projectChart } from "../i18n.ts";
import { App, applyHostContext, LATEST_PROTOCOL_VERSION } from "../kit/bridge.ts";
import {
  assertPanelInvariants,
  assertI18nParity,
  assertEnNoCjk,
  DEFAULT_MAX_BYTES,
  WARN_BYTES,
} from "../kit/guard.ts";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const panelDir = path.join(__dirname, "..");
const panelHtmlPath = path.join(panelDir, "dist", "panel.html");

test("i18n catalogs satisfy 1:1 parity, EN no-CJK, and human copy invariants", () => {
  assertI18nParity(STRINGS.en, STRINGS["zh-CN"]);
  assertEnNoCjk(STRINGS.en);

  assert.equal(tr("zh-CN", "stopBill"), "停止并结束计费");
  assert.equal(tr("en", "stopBill"), "Stop & end billing");
  assert.equal(tr("en", "reconnected"), "✓ Reconnected");
  assert.equal(tr("zh-CN", "reconnected"), "✓ 已重新连接");
  assert.equal(tr("en", "bo"), "Browser");
  assert.equal(tr("zh-CN", "bo"), "浏览器");
  assert.equal(tr("en", "ec"), "External CLI");
  assert.equal(tr("zh-CN", "ec"), "外部 CLI");
  assert.equal(tr("en", "ex"), "external");
  assert.equal(tr("zh-CN", "ex"), "外部");

  for (const reason of [
    "user_stop",
    "guard_spend",
    "guard_runtime",
    "guard_balance",
    "watchdog_idle",
    "watchdog_deadline",
    "provision",
    "vanished",
  ]) {
    assert.ok(STRINGS.en[`s.${reason}`], `missing en stop reason s.${reason}`);
    assert.ok(STRINGS["zh-CN"][`s.${reason}`], `missing zh-CN stop reason s.${reason}`);
  }
  for (const l of ["en", "zh-CN"] as const) {
    for (const [k, v] of Object.entries(STRINGS[l])) {
      assert.ok(!v.includes("[?]"), `i18n key ${l}.${k} must not expose raw [?]`);
    }
  }
});

test("fmtDur handles 64-bit durations, days, and localized EN/ZH output", () => {
  assert.equal(fmtDur(null, "en"), "—");
  assert.equal(fmtDur(-5, "en"), "—");
  assert.equal(fmtDur(42, "en"), "42s");
  assert.equal(fmtDur(42, "zh-CN"), "42 秒");
  assert.equal(fmtDur(5400, "en"), "1h 30m");
  assert.equal(fmtDur(5400, "zh-CN"), "1 小时 30 分");
  assert.equal(fmtDur(3 * 86400 + 4 * 3600, "en"), "3d 4h");
  assert.equal(fmtDur(3 * 86400 + 4 * 3600, "zh-CN"), "3 天 4 小时");
  assert.equal(fmtDur(3_000_000_000, "en"), "34722d 5h");
  assert.equal(fmtCu(8.9), "8.90");
  assert.equal(fmtCu(null), "—");
});

test("fmtErr prefers Rust localized payload and falls back to bridge/CLI copy", () => {
  const loc = { en: "Install the Colab CLI.", "zh-CN": "未找到 Colab CLI。" };
  assert.equal(fmtErr("zh-CN", "cli_missing", loc), "未找到 Colab CLI。");
  assert.equal(fmtErr("en", "cli_missing", loc), "Install the Colab CLI.");
  assert.equal(fmtErr("zh-CN", "bridge_timeout", null), STRINGS["zh-CN"]["e.bridge_timeout"]);
  assert.equal(fmtErr("en", "unknown_kind", null), STRINGS.en["e.cli_error"]);
});

test("projectChart uses real timestamps for X coordinates and supports range filtering", () => {
  const empty = projectChart([], 86400, 10000);
  assert.equal(empty.visible, false);

  const now = 7 * 86400;
  const pts = [
    { at_secs: now - 6 * 86400, balance_cu: 100 },
    { at_secs: now - 3600, balance_cu: 80 },
    { at_secs: now - 3000, balance_cu: 78 },
    { at_secs: now, balance_cu: 70 },
  ];

  // Unevenly spaced timestamps within 1h: middle point is 1/6 across, NOT 1/2 (array index)
  const h1 = projectChart(pts, 3600, now);
  assert.equal(h1.visible, true);
  assert.ok(h1.visible && !h1.sparse);
  assert.equal(h1.filtered.length, 3);
  assert.equal(h1.coords[0].x, 0);
  assert.equal(h1.coords[2].x, 320);
  assert.ok(h1.coords[1].x > 45 && h1.coords[1].x < 65, `expected ~53.3 for 600/3600, got ${h1.coords[1].x}`);
  assert.equal(h1.delta, -10);

  // 7d window includes all 4 points
  const d7 = projectChart(pts, 604800, now);
  assert.ok(d7.visible && !d7.sparse);
  assert.equal(d7.filtered.length, 4);
  assert.equal(d7.minV, 70);
  assert.equal(d7.maxV, 100);

  // Sparse range (< 2 points in window) keeps chart card visible with sparse flag
  const sparse = projectChart([{ at_secs: now - 7200, balance_cu: 50 }, { at_secs: now - 10, balance_cu: 49 }], 3600, now);
  assert.equal(sparse.visible, true);
  assert.ok(sparse.visible && sparse.sparse);
});

test("packed panel.html passes Panel Kit Lucid Indigo invariants and security/UX regressions", () => {
  assert.ok(fs.existsSync(panelHtmlPath), "dist/panel.html must exist");
  const html = fs.readFileSync(panelHtmlPath, "utf8");
  const report = assertPanelInvariants(html, {
    maxBytes: DEFAULT_MAX_BYTES,
    hardMaxBytes: DEFAULT_MAX_BYTES,
    warnBytes: WARN_BYTES,
    bootPlaceholder: "/*__COLAB_MCP_BOOT__*/",
  });
  assert.ok(report.bytes <= DEFAULT_MAX_BYTES, `panel size ${report.bytes} B exceeds ${DEFAULT_MAX_BYTES} B`);
  assert.ok(html.includes("Colab Deck"), "panel must carry Colab Deck brand name");

  // Bridge security invariants (SSOT panel/kit/bridge.ts)
  assert.equal(LATEST_PROTOCOL_VERSION, "2026-01-26");
  assert.equal(typeof App, "function");
  assert.equal(typeof applyHostContext, "function");
  assert.ok(!fs.existsSync(path.join(panelDir, "bridge.js")), "duplicate panel/bridge.js must be deleted");
  assert.ok(!fs.existsSync(path.join(panelDir, "bridge.ts")), "duplicate panel/bridge.ts must not exist");
  const bridgeSrc = fs.readFileSync(path.join(panelDir, "kit", "bridge.ts"), "utf8");
  assert.ok(bridgeSrc.includes("ev.source !== target"), "bridge must check ev.source === window.parent");
  assert.ok(bridgeSrc.includes('LATEST_PROTOCOL_VERSION = "2026-01-26"'), "bridge must validate protocolVersion 2026-01-26");
  assert.ok(bridgeSrc.includes("serverTools"), "bridge must gate calls on hostCaps.serverTools");
  assert.ok(bridgeSrc.includes("return null;"), "bridge must fail closed on unknown colors");

  // App state-machine & bug-fix invariants
  const appSrc = fs.readFileSync(path.join(panelDir, "app.ts"), "utf8");
  assert.ok(appSrc.includes('./kit/bridge.ts"'), "app.ts must import SSOT ./kit/bridge.ts");
  assert.ok(!appSrc.includes('from "./bridge.js"') && !appSrc.includes('from "./bridge.ts"'), "app.ts must not import duplicate ./bridge");
  assert.ok(!/\b(?:innerHTML|outerHTML)\b/.test(appSrc), "app.ts must not use innerHTML/outerHTML");
  assert.ok(appSrc.includes('D($("app"), "ps"'), "app.ts must wire real data-ps paused state");
  assert.ok(appSrc.includes("failCount >= 3"), "app.ts must trigger disconnected banner after 3 failures");
  assert.ok(!/name\s*:\s*["']colab_path["']/.test(appSrc), "panel must not expose writable colab_path input");
  assert.ok(appSrc.includes("d.rev < snap.rev"), "panel must reject out-of-order stale snapshots");
  assert.ok(appSrc.includes("epk < snapEp"), "panel must track request epoch across mode switches");
  assert.ok(appSrc.includes("!snap || snap.syncing"), "cold-start connect must poll immediately while syncing");
  assert.ok(appSrc.includes("dimErr !== key"), "banner dismiss must only suppress matching error key");
  assert.ok(appSrc.includes("expected_backend_mode"), "panel must send expected_backend_mode on billing actions");
  assert.ok(appSrc.includes("created && ep"), "failed-deploy stop CTA must pin created + endpoint");
  assert.ok(appSrc.includes("s.env || {}"), "panel must handle env: null on cold start without throwing");
  assert.ok(appSrc.includes("endpoint: ds.ep"), "panel must pin endpoint on session actions");
  assert.ok(appSrc.includes("cancel_failed"), "panel must keep cancel available on ready/cancel_failed runs");
  assert.ok(appSrc.includes("preview_switch"), "panel must preview account switch impact before switching");
  assert.ok(appSrc.includes("stop_on_failure"), "panel must expose deploy stop_on_failure");
  assert.ok(appSrc.includes("external_name"), "panel must render external_name/uptime");
  assert.ok(appSrc.includes("poll_active_secs") && appSrc.includes("poll_idle_secs"), "settings must expose poll intervals");
  assert.ok(appSrc.includes("settings_path") && appSrc.includes("settings_error"), "settings hot-reload path/error wired");
  assert.ok(appSrc.includes("/v1/models"), "OpenAI endpoint curl must target /v1/models");
  assert.ok(appSrc.includes("GPU_ACCS"), "llama-cpp deploy must restrict accelerator options to GPUs");
  assert.ok(appSrc.includes('${orp.length} ${t("ex")}'), "overview KPI must use i18n external count");
  assert.ok(appSrc.includes('en ? "CLI" : t("bo")'), "overview external row must use CLI/Browser badge");
  assert.ok(appSrc.includes('en ? "ec" : "bo"'), "sessions card must use External CLI/Browser badge");
  assert.ok(!html.includes('class="cd"></span>'), "auto chip dot must not collide with .cd card class");
  assert.ok(html.includes("max-width:520px"), "narrow breakpoint must be 520px");
  assert.ok(html.includes("max-width:640px"), "2x2 KPI breakpoint must be 640px");
});
