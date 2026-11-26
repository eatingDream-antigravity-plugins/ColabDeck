import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { STRINGS, tr, fmtDur, fmtCu } from "../i18n.js";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const panelDir = path.join(__dirname, "..");
const panelHtmlPath = path.join(panelDir, "dist", "panel.html");

const FAMILY_TOKENS = [
  "--bg", "--s1", "--s2", "--s0",
  "--t1", "--t2", "--t3",
  "--ln", "--l2", "--sh",
  "--ac", "--ah", "--ok", "--wn", "--er", "--in",
];

test("i18n catalogs have identical keys in zh-CN and en", () => {
  const zhKeys = Object.keys(STRINGS["zh-CN"]).sort();
  const enKeys = Object.keys(STRINGS.en).sort();
  assert.deepEqual(zhKeys, enKeys);
  assert.equal(tr("zh-CN", "stopBill"), "停止并结束计费");
  assert.equal(tr("en", "stopBill"), "Stop & End Billing");
  assert.equal(tr("en", "reconnected"), "✓ Reconnected");
  assert.equal(fmtDur(5400), "1h 30m");
  assert.equal(fmtCu(8.9), "8.90");
});

test("packed panel.html satisfies zero-innerHTML, design-token, and <=43,500 B budget invariants", () => {
  assert.ok(fs.existsSync(panelHtmlPath), "dist/panel.html must exist");
  const html = fs.readFileSync(panelHtmlPath, "utf8");
  const bytes = Buffer.byteLength(html, "utf8");

  assert.ok(bytes <= 43_500, `panel size ${bytes} B exceeds 43,500 B target`);
  assert.ok(bytes < 46_080, `panel size ${bytes} B exceeds 45 KiB hard limit`);

  // Dark-first zero-white-flash boot & strict CSP
  assert.ok(html.includes('style="background:#141311;color-scheme:dark"'), "dark html boot style required");
  assert.ok(html.includes('<meta name="color-scheme" content="dark">'), "single dark color-scheme meta required");
  assert.ok(html.includes('<body style="background:#141311">'), "dark body inline style required");
  assert.ok(!html.includes('content="dark light"'), "must never use dark light meta");
  assert.ok(html.includes("default-src 'none'"), "strict CSP required");
  assert.ok(html.includes("/*__COLAB_MCP_BOOT__*/"), "SSR boot placeholder required");

  // Single inline <script> tag, zero external URLs in HTML/CSS/JS (only W3C SVG namespace URI allowed)
  const scriptOpens = html.match(/<script\b/gi) || [];
  assert.equal(scriptOpens.length, 1, `expected exactly 1 <script> tag, found ${scriptOpens.length}`);
  const urls = (html.match(/https?:\/\/[^\s"'<>`]+/gi) || []).filter((u) => u !== "http://www.w3.org/2000/svg");
  assert.deepEqual(urls, [], `must contain zero external URLs, found: ${urls.join(", ")}`);

  // Zero prefers-color-scheme, zero opacity:0 in @keyframes, zero innerHTML/outerHTML
  assert.ok(!/prefers-color-scheme/i.test(html), "must never use prefers-color-scheme");
  assert.ok(!/--buncss-/i.test(html), "must strip --buncss-* declarations");
  assert.ok(!/\b(?:innerHTML|outerHTML)\b/.test(html), "zero innerHTML/outerHTML allowed in panel bundle");

  const keyframes = html.match(/@keyframes\s+[^{]+\{[^}]*(?:\{[^}]*\}[^}]*)*\}/g) || [];
  for (const kf of keyframes) {
    assert.ok(!/opacity\s*:\s*0(?:\D|$)/i.test(kf), `opacity:0 forbidden in keyframes: ${kf}`);
  }

  // All 16 canonical tokens present in both Dark (:root) and Light (:root[data-theme="light"])
  const lightMatch = html.match(/:root\[data-theme=["']?light["']?\]\{([^}]+)\}/);
  assert.ok(lightMatch, "light theme block required");
  const lightBlock = lightMatch[1];
  for (const tok of FAMILY_TOKENS) {
    assert.ok(html.includes(`${tok}:`), `missing dark token ${tok}`);
    assert.ok(lightBlock.includes(`${tok}:`), `missing light token ${tok}`);
  }
  assert.ok(/--ok\s*:\s*#2f7d53/i.test(lightBlock), "light --ok must equal #2F7D53");
  assert.ok(/--bg\s*:\s*#faf8f4/i.test(lightBlock), "light --bg must equal #FAF8F4");

  // Bridge security & UX invariants in source
  const bridgeSrc = fs.readFileSync(path.join(panelDir, "bridge.js"), "utf8");
  assert.ok(bridgeSrc.includes("ev.source !== target"), "bridge must check ev.source === window.parent");
  assert.ok(bridgeSrc.includes('LATEST_PROTOCOL_VERSION = "2026-01-26"'), "bridge must validate protocolVersion 2026-01-26");
  assert.ok(bridgeSrc.includes("serverTools"), "bridge must gate calls on hostCaps.serverTools");

  const appSrc = fs.readFileSync(path.join(panelDir, "app.js"), "utf8");
  assert.ok(!/\b(?:innerHTML|outerHTML)\b/.test(appSrc), "app.js must not use innerHTML/outerHTML");
  assert.ok(appSrc.includes('D($("app"), "ps"'), "app.js must wire real data-ps paused state");
  assert.ok(appSrc.includes("failCount >= 3"), "app.js must trigger disconnected banner after 3 failures");
  assert.ok(!/name\s*:\s*["']colab_path["']/.test(appSrc), "panel must not expose writable colab_path input");
  assert.ok(appSrc.includes("d.rev < snap.rev"), "panel must reject out-of-order stale snapshots");
  assert.ok(appSrc.includes("expected_backend_mode"), "panel must send expected_backend_mode on billing actions");
  assert.ok(appSrc.includes("created && ep"), "failed-deploy stop CTA must pin created + endpoint");
  assert.ok(appSrc.includes("s.env || {}"), "panel must handle env: null on cold start without throwing");
  assert.ok(bridgeSrc.includes("return null;"), "bridge parseC must fail closed on unknown colors");
  assert.ok(html.includes("ColabDeck"), "panel must carry ColabDeck brand name");
  assert.ok(appSrc.includes("endpoint: ds.ep"), "panel must pin endpoint on session actions");
  assert.ok(appSrc.includes("cancel_failed"), "panel must keep cancel available on ready/cancel_failed runs");
  assert.ok(appSrc.includes("preview_switch"), "panel must preview account switch impact before switching");
  assert.ok(appSrc.includes("stop_on_failure"), "panel must expose deploy stop_on_failure");
  assert.ok(appSrc.includes("external_name"), "panel must render unmanaged external_name/uptime");
  assert.ok(!html.includes('class="cd"></span>'), "auto chip dot must not collide with .cd card class");
  assert.ok(html.includes("max-width:520px"), "narrow 2x2 KPI breakpoint must be 520px");
});
