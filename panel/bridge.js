export const LATEST_PROTOCOL_VERSION = "2026-01-26";
const BG = "--color-background-primary", J = "2.0", U = "ui/notifications/", isO = (v) => Boolean(v && typeof v === "object");
const lum = (r, g, b) => (0.2126 * r + 0.7152 * g + 0.0722 * b) / 255;

function parseC(s) {
  s = String(s || "").trim();
  if (!s || /^(transparent|none|initial|unset)$/i.test(s)) return null;
  let m = s.match(/^#([0-9a-f]{3,8})$/i);
  if (m && [3, 4, 6, 8].includes(m[1].length)) {
    const h = m[1], sh = h.length <= 4, d = (i) => parseInt(sh ? h[i] + h[i] : h.slice(i * 2, i * 2 + 2), 16);
    return { a: h.length % 4 ? 1 : d(3) / 255, l: lum(d(0), d(1), d(2)) };
  }
  m = s.match(/^rgba?\(\s*([\d.]+)[,\s]+([\d.]+)[,\s]+([\d.]+)(?:\s*[,/]\s*([\d.]*)(%?))?\s*\)$/i);
  if (m) {
    const a = m[4] ? (m[5] ? +m[4] / 100 : +m[4]) : 1;
    if (Number.isFinite(+m[1] + +m[2] + +m[3] + a)) return { a, l: lum(+m[1], +m[2], +m[3]) };
  }
  return null;
}

function syncTheme(ctx) {
  if (!isO(ctx)) return;
  const d = document, r = d.documentElement, vs = ctx.styles?.variables;
  if (isO(vs)) for (const [k, v] of Object.entries(vs)) {
    if (!k.startsWith("--") || v == null) continue;
    const val = String(v).trim();
    if (k === BG) { const c = parseC(val); if (!c || c.a < 0.9) continue; }
    r.style.setProperty(k, val);
  }
  const raw = String(ctx.theme || "").toLowerCase();
  let t = /dark|night|black/.test(raw) ? "dark" : /light|day|white/.test(raw) ? "light" : null;
  if (!t) { const c = parseC(vs?.[BG] || getComputedStyle(r).getPropertyValue(BG)); if (c && c.a >= 0.5 && c.l != null) t = c.l < 0.45 ? "dark" : "light"; }
  if (t) {
    const bg = `var(${BG},${t === "light" ? "#FAF8F4" : "#141311"})`;
    r.setAttribute("data-theme", t); r.style.colorScheme = t; r.style.backgroundColor = bg;
    if (d.body) d.body.style.backgroundColor = bg;
    d.querySelector('meta[name="color-scheme"]')?.setAttribute("content", t);
  }
}

export function createMcpBridge(appInfo) {
  const W = typeof window < "u" ? window : null, OA = Object.assign, target = W ? W.parent : null, inFrame = Boolean(target && target !== W), pending = new Map();
  let nextId = 1, initOk = false, connP = null, hostCaps = null, hostCtx = null, pinned = null;
  const self = { ontoolresult: null }, post = (m) => { try { target?.postMessage(m, pinned || "*"); } catch {} };

  function onMsg(ev) {
    if (ev.source !== target) return;
    if (pinned && ev.origin !== pinned) return;
    const m = ev?.data; if (!isO(m) || m.jsonrpc !== J) return;
    if (!pinned && ev.origin && ev.origin !== "null") pinned = ev.origin;
    const mm = m.method;
    if (m.id !== undefined && !mm) {
      const e = pending.get(m.id); if (!e) return;
      pending.delete(m.id); clearTimeout(e.t);
      m.error ? e.rej(new Error(m.error.message || "RPC")) : e.res(m.result);
    } else if (m.id !== undefined) {
      post(mm === "ping" || mm === "ui/resource-teardown" ? { jsonrpc: J, id: m.id, result: {} } : { jsonrpc: J, id: m.id, error: { code: -32601, message: "Not found" } });
    } else if (mm === `${U}tool-result`) self.ontoolresult?.(m.params);
    else if (mm === `${U}host-context-changed`) {
      const p = m.params || {};
      hostCtx = OA({}, hostCtx, p, p.styles ? { styles: OA({}, hostCtx?.styles, p.styles, { variables: OA({}, hostCtx?.styles?.variables, p.styles.variables) }) } : null);
      syncTheme(hostCtx);
    } else if (mm === "notifications/progress") for (const e of pending.values()) e.reset();
  }

  function req(method, params, ms = 6e4) {
    const id = nextId++;
    return new Promise((res, rej) => {
      const mk = () => setTimeout(() => { pending.delete(id); rej(new Error(`Timeout ${method}`)); }, ms);
      const e = { res, rej, t: mk(), reset() { clearTimeout(e.t); e.t = mk(); } };
      pending.set(id, e); post({ jsonrpc: J, id, method, params });
    });
  }

  function close() {
    initOk = false; hostCaps = null; W?.removeEventListener("message", onMsg);
    for (const e of pending.values()) { clearTimeout(e.t); e.rej(new Error("Closed")); }
    pending.clear();
  }

  const hasServerTools = () => Boolean(initOk && isO(hostCaps?.serverTools) && !Array.isArray(hostCaps.serverTools));

  return OA(self, {
    isEmbedded: () => inFrame,
    hasServerTools,
    callTool: (name, args = {}, ms = 6e4) => hasServerTools() ? req("tools/call", { name, arguments: args }, ms) : Promise.reject(new Error("No serverTools")),
    openLink: (url) => {
      if (initOk && hostCaps?.openLinks) return req("ui/open-link", { url }, 1e4).catch(() => {});
      try { W?.open(url, "_blank", "noopener,noreferrer"); } catch {}
      return Promise.resolve();
    },
    connect() {
      if (!inFrame) return Promise.resolve(false);
      if (initOk) return Promise.resolve(true);
      if (connP) return connP;
      W.addEventListener("message", onMsg);
      return (connP = (async () => {
        try {
          const r = await req("ui/initialize", { appInfo, appCapabilities: {}, protocolVersion: LATEST_PROTOCOL_VERSION }, 15e3);
          if (!isO(r) || r.protocolVersion !== LATEST_PROTOCOL_VERSION) throw new Error("Bad protocol");
          hostCaps = r.hostCapabilities || {}; hostCtx = r.hostContext || {}; initOk = true;
          syncTheme(hostCtx); post({ jsonrpc: J, method: `${U}initialized` });
          return true;
        } catch (err) { close(); throw err; } finally { connP = null; }
      })());
    },
  });
}