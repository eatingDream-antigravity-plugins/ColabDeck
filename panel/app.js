import { createMcpBridge } from "./bridge.js";
import { tr, fmtDur, fmtCu } from "./i18n.js";

const ACCS = "G4 A100 H100 L4 T4 NONE V5E1 V6E1".split(" "), NO_HM = new Set(["L4", "V5E1", "V6E1"]);
const TABS = "overview sessions tunnels deploy settings".split(" "), NS = "http://www.w3.org/2000/svg", CK = "checkbox", DC = document, ST = String, CT = clearTimeout;
const TS = "colab_session_action", TT = "colab_tunnel_action", TP = "colab_app_plugins", TA = "colab_app_accounts", TK = "colab_app_settings", TW = "colab_app_watchdog";
const $ = (id) => DC.getElementById(id), Q = (s, r = DC) => r.querySelector(s), QA = (s, r = DC) => r.querySelectorAll(s), ON = (el, e, f) => el.addEventListener(e, f);
const OA = Object.assign, OE = Object.entries, M = Math, MR = M.round, MX = M.max, MN = M.min, NF = Number.isFinite, NI = Number.isInteger;
const isOff = (s) => s === "stopped" || s === "failed";
const GF = [["ac", "accelerator", "acc"], ["mr", "max_runtime_h", "num", { min: 0, step: 0.5 }], ["im", "idle_min", "num", { min: 0, step: 5 }], ["ms", "max_spend_cu", "num", { min: 0, step: 1 }], ["bf", "min_balance_cu", "num", { min: 0, step: 1 }]];
const NK = GF.slice(1).map((x) => x[1]), KA = "__accelerator", KMC = "max_concurrent_sessions", TBQ = "#tabs button[data-tab]";

const T = (n, v) => { const s = v == null ? "" : ST(v); if (n && n.textContent !== s) n.textContent = s; };
const H = (n, h) => { if (n && n.hidden !== !!h) n.hidden = !!h; };
const D = (n, k, v) => { if (!n) return; const s = v == null ? "" : ST(v); if (n.dataset[k] !== s) n.dataset[k] = s; };
const K = (n, v) => D(n, "k", v), PT = (n, v, k) => { T(n, v); K(n, k); }, PE = (n, nm, ep) => { D(n, "name", nm); D(n, "ep", ep); };
const TG = (n, c, v) => n.classList.toggle(c, !!v);
const A = (n, k, v) => { if (!n) return; if (v == null || v === false) n.removeAttribute(k); else { const s = v === true ? "" : ST(v); if (n.getAttribute(k) !== s) n.setAttribute(k, s); } };
const PH = (n, k) => A(n, "placeholder", t(k));

const AM = { c: "className", t: "textContent", h: "hidden", v: "value", ck: "checked" };
function E(tag, a, ...ks) {
  const n = DC.createElement(tag);
  if (typeof a === "string") n.className = a;
  else if (a) for (const [k, v] of OE(a)) {
    if (v == null || v === false) continue;
    if (AM[k]) n[AM[k]] = k === "h" || k === "ck" ? !!v : ST(v);
    else if (k === "st") n.style.cssText = v;
    else n.setAttribute(k, v === true ? "" : ST(v));
  }
  for (const c of ks) if (c != null && c !== false) n.append(typeof c === "string" || typeof c === "number" ? DC.createTextNode(ST(c)) : c);
  return n;
}
const DV = (c, ...k) => E("div", c, ...k), CD = (...k) => DV("cd", ...k), RW = (...k) => DV("rw", ...k), RL = (...k) => DV("rl", ...k), IT = (...k) => DV("it", ...k);
const SP = (c, t) => E("span", t ? { c, t } : c), PL = (k, t, h) => E("span", { c: "pl", "data-k": k, t, h }), IN = (o) => E("input", o), LB = () => E("label"), SL = (o, ...k) => E("select", o, ...k), OP = (v, t) => E("option", v != null ? { v, t } : null);
const B = (c, a, t, ex) => E("button", OA({ c, "data-a": a, t }, ex)), BS = (x, a, t, ex) => B(x ? `b s ${x}` : "b s", a, t, ex);
const S = (tag, a, ...ks) => { const n = DC.createElementNS(NS, tag); for (const [k, v] of OE(a || {})) if (v != null) n.setAttribute(k, ST(v)); for (const c of ks) if (c) n.append(c); return n; };

for (const n of TABS) {
  $("tabs").append(E("button", { role: "tab", "data-tab": n, "aria-selected": ST(n === TABS[0]) }));
  $("main").append(E("section", { id: `v-${n}`, c: "vw", h: n !== TABS[0] }));
}

function sync(p, items, keyFn, mk, upd) {
  const map = p._m || (p._m = new Map()), seen = new Set();
  items.forEach((it, i) => {
    const k = ST(keyFn(it, i)); seen.add(k);
    let r = map.get(k); if (!r) { r = mk(it); map.set(k, r); }
    upd(r, it);
    if (p.children[i] !== r[0]) p.insertBefore(r[0], p.children[i] || null);
  });
  for (const [k, r] of map) if (!seen.has(k)) { r[0].remove(); map.delete(k); }
}

const syncSel = (sel, items, z) => {
  const prev = sel.value, opts = z ? [{ display_name: "", hardware: "" }, ...items] : items.length ? items : [{ display_name: "", hardware: "—" }];
  sync(sel, opts, (x) => x.display_name || "_", () => [OP()], (r, x) => { const dn = x.display_name; r[0].value = dn; T(r[0], dn ? `${dn} (${x.hardware})` : (z || "—")); });
  if (prev && opts.some((x) => x.display_name === prev)) sel.value = prev;
};
const mkEmp = (k) => { const tx = SP(); return [DV("cd ep", E("span", { c: "em", "aria-hidden": "true" }, Q(".em svg").cloneNode(true), SP("dt")), tx), tx, k]; };
const mkAcc = (name) => SL({ name }, ...ACCS.map((a) => OP(a, a === "NONE" ? "CPU" : a)));
const mkFlds = (defs) => defs.map(([lk, name, kind, opts]) => {
  const ctl = kind === "acc" ? mkAcc(name) : kind === "sel" ? SL({ name }) : IN(OA({ name, type: kind === "chk" ? CK : kind === "num" ? "number" : "text" }, opts));
  const lb = LB(); return [kind === "chk" ? DV("fi ck", ctl, lb) : DV("fi", lb, ctl), lb, ctl, lk];
});
const mkFrm = (id, flds, cb) => { const f = E("form", { id, c: "fg" }, ...flds.map((x) => x[0])); f.oninput = () => { f.dataset.dirty = "1"; }; f.onchange = () => { f.dataset.dirty = "1"; cb?.(); }; return f; };

let snap = null, snapAt = Date.now(), lastSync = 0, tab = TABS[0], lang = "zh-CN", pickedLang = false;
let auto = true, busy = false, pollT = null, delayMs = 4e3, rttMs = 120, failCount = 0, down = false, connOk = false, epoch = 0, snapEp = -1;
let armedStop = null, armedWd = null, swPrev = null, hfModels = [], hfFit = null, selPlg = "llama-cpp", builtPlg = null;
let selRepo = "unsloth/Qwen3-32B-GGUF", bnr = null, bnrOff = false, fbT = null;

const bridge = createMcpBridge({ name: "colab-mcp", version: "0.1.0" }), hst = () => bridge.hasServerTools(), emb = () => bridge.isEmbedded(), actA = () => snap?.active_account;
const t = (k) => tr(lang, k), stL = (k) => { const v = t(`s.${k}`); return v === `s.${k}` ? k : v; };
const loc = (m) => m?.[lang] || m?.en || Object.values(m || {})[0] || "";
const isPs = (s) => Boolean(s?.guard_paused || s?.host_guard_paused);
const optN = (v) => (v !== "" && v != null && NF(+v) ? +v : null), posN = (v) => { const n = optN(v); return n > 0 ? n : null; };
const chkN = (f, ks) => ks.every((k) => { const s = `${f[k] ?? ""}`.trim(); return !s || (+s >= 0 && NF(+s)); });
const clr = (...ids) => ids.forEach((i) => { if ($(i)) $(i).value = ""; });
const P = (o, ds) => OA(o, ds?.ep ? { endpoint: ds.ep } : null, actA()?.id ? { account_id: actA().id } : null, snap?.backend_mode ? { expected_backend_mode: snap.backend_mode } : null);
const gd = (g) => [g.max_runtime_secs ? g.max_runtime_secs / 3600 : "", g.idle_timeout_secs ? MR(g.idle_timeout_secs / 60) : "", g.max_spend_cu ?? "", g.min_balance_cu ?? ""];
const flash = (m) => { const n = $("fb"); T(n, m); H(n, 0); CT(fbT); fbT = setTimeout(() => H(n, 1), 2600); };
function syncBnr() {
  const b = $("bnr"), p = bnr && !bnrOff ? bnr : snap?.last_error && !bnrOff ? { k: "er", m: snap.last_error } : down ? { k: "wn", m: `${t("clost")} (${MR(delayMs / 1e3)}s)`, r: 1 } : null;
  if (!p) return H(b, 1);
  K(b, p.k); T($("bnr-t"), p.m); T($("bnr-act"), t("ry")); H($("bnr-act"), !p.r); H(b, 0);
}
const showBnr = (m, k = "wn", r = 0) => { bnrOff = false; bnr = { m, k, r }; syncBnr(); };
const parseR = (r) => { if (!r) return null; if (r.structuredContent) return r.structuredContent; const x = r.content?.find?.((c) => c.type === "text")?.text; if (x) try { return JSON.parse(x); } catch {} return null; };
const nowS = () => (Date.now() / 1e3) | 0, elap = () => MX(0, ((Date.now() - snapAt) / 1e3) | 0);
const liveUp = (x) => x.started_at_epoch != null ? MX(0, nowS() - x.started_at_epoch) : (x.uptime_secs != null ? x.uptime_secs + elap() : null);
const liveRem = (w) => !w?.armed ? null : (w.deadline_epoch != null ? MX(0, w.deadline_epoch - nowS()) : (w.remaining_secs != null ? MX(0, w.remaining_secs - elap()) : null));
const sName = (x) => x.is_orphan && x.external_name ? `${x.external_name} · ext` : x.display_name;
const upTxt = (x) => `${t("up")} ${x.start_source === "first_seen" ? "≥ " : ""}${fmtDur(liveUp(x))}`;
const spTxt = (x) => `${t("sp")} ${x.spend_cu != null ? `${x.spend_estimated ? "≈ " : ""}${fmtCu(x.spend_cu)} CU` : "—"}`;
const rateTxt = (a) => { const r = snap?.hardware_rates?.[a]; return r ? `≈ ${fmtCu(r.cu_per_hour)} CU/h (${t(r.measured ? "me" : "es")})` : ""; };

function mkPsCard() {
  const tl = DV("tt"), ls = DV("ls"), box = DV({ c: "cd e", h: 1 }, tl, ls);
  return {
    el: box,
    r(ps) {
      H(box, !ps.length); if (!ps.length) return;
      T(tl, t("sf"));
      sync(ls, ps, (p) => p.endpoint || p.session, () => { const nm = SP("mn"), er = SP("mu"), bt = BS("d a", "rs"); return [IT(RW(RL(nm, er), bt)), nm, er, bt]; }, (r, p) => {
        const ep = p.endpoint || "", orp = p.is_orphan || !p.session || p.session === "[?]";
        T(r[1], p.session || ep); T(r[2], p.status?.last_error || ep);
        H(r[3], !ep); PE(r[3], orp ? "" : p.session, ep); T(r[3], t("ry"));
      });
    },
  };
}

const ov = (() => {
  const cs = [0, 1, 2, 3].map(() => { const l = DV("hl"), v = DV("hv"), s = DV("hm el"); return [DV("hc", l, v, s), l, v, s]; });
  const bN = SP(), bU = E("small", { t: "CU" }), bS = PL("wn", "", 1), rN = SP(), rU = E("small", { t: "CU/h" }), rtD = SP("sd"), rtN = SP(), gD = SP("sd"), gT = SP();
  cs[0][2].append(bN, bU, bS); cs[1][2].append(rN, rU); cs[2][2].append(rtD, rtN); cs[3][2].className = "hvs"; cs[3][2].append(gD, gT);
  const suT = DV("tt"), suD = DV("mu"), suB = BS("c", "gt", "", { "data-v": "settings" }), suC = DV({ c: "cd w", h: 1 }, RW(RL(suT, suD), suB)), psC = mkPsCard();
  const spL = SP("hl"), spP = SP("mt mn"), poly = S("polyline", { stroke: "var(--ac)", "stroke-width": "1.8", fill: "none" }), spC = DV({ c: "cd", h: 1 }, RW(spL, spP), S("svg", { class: "sp", viewBox: "0 0 320 42", preserveAspectRatio: "none" }, poly));
  const wT = DV("tt"), wL = DV("ls"), wC = DV({ c: "cd w", h: 1 }, wT, wL), oT = DV("tt"), oD = DV("mu"), oL = DV("ls"), oC = DV({ c: "cd w", h: 1 }, oT, oD, oL);
  const qT = DV("tt"), qbs = [["b p", "sessions"], ["b c", "deploy"], ["b", "tunnels"]].map(([c, v]) => B(c, "gt", "", { "data-v": v }));
  $("v-overview").append(DV("cd hero", DV("hg", ...cs.map((c) => c[0]))), suC, psC.el, spC, wC, oC, CD(qT, RL(...qbs)));

  return (s) => {
    const u = s.usage, ss = s.sessions || [], pss = s.pending_stops || [], env = s.env || {};
    const orp = ss.filter((x) => x.is_orphan), wrn = ss.filter((x) => x.guard_decision?.decision === "warn"), ps = isPs(s), arm = ss.filter((x) => x.watchdog?.armed).length;
    T(cs[0][1], t("bl")); T(bN, u ? fmtCu(u.projected_balance_cu) : "—"); H(bU, !u); T(bS, t("sl")); H(bS, !u?.stale); T(cs[0][3], u ? (actA()?.label || "") : t("uk"));
    T(cs[1][1], t("br")); T(rN, u ? fmtCu(u.rate_cu_per_hour) : "—"); H(rU, !u); T(cs[1][3], u ? (u.eta_secs != null ? `${t("et")} ${fmtDur(u.eta_secs)}` : t("bz")) : t("uk"));
    T(cs[2][1], t("rt")); K(rtD, ss.length ? (orp.length ? "wn" : "ok") : "t3"); T(rtN, ss.length); T(cs[2][3], orp.length ? `${orp.length} [?]` : `${arm} WD`);
    T(cs[3][1], t("cg")); K(gD, ps ? "wn" : wrn.length || pss.length ? "er" : arm ? "ok" : "t3"); T(gT, ps ? t("gp") : wrn.length || pss.length ? t("gw") : arm ? t("ga") : t("gi")); T(cs[3][3], ss.length ? `${arm}/${ss.length} VM` : t("za"));
    const needSu = s.backend_mode !== "mock" && s.env && (!env.colab_found || !env.adc_file_present);
    H(suC, !needSu); if (needSu) { T(suT, t("sut")); T(suD, t("suh")); T(suB, t("t.settings")); }
    psC.r(pss);
    const pts = s.balance_history || []; H(spC, pts.length < 2);
    if (pts.length >= 2) {
      T(spL, t("bt")); T(spP, `${pts.length} pts`);
      const vs = pts.map((p) => p.balance_cu), mn = MN(...vs), sp = MX(0.01, MX(...vs) - mn);
      A(poly, "points", vs.map((v, i) => `${((i / (vs.length - 1)) * 320).toFixed(1)},${(38 - ((v - mn) / sp) * 34).toFixed(1)}`).join(" "));
    }
    H(wC, !wrn.length);
    if (wrn.length) {
      T(wT, t("gw"));
      sync(wL, wrn, (w) => w.endpoint || w.display_name, () => { const a = SP("mn"), b = SP("mu"); return [RW(a, b), a, b]; }, (r, w) => {
        T(r[1], sName(w)); T(r[2], (w.guard_decision?.concerns || []).map((c) => c.kind === "usage_unknown" ? `${c.limit}: ${t("uk")}` : `${c.limit}: ${fmtCu(c.current)}/${fmtCu(c.threshold)}`).join(" · "));
      });
    }
    H(oC, !orp.length);
    if (orp.length) {
      T(oT, t("ot")); T(oD, t("od"));
      sync(oL, orp, (o) => o.endpoint, () => { const nm = SP("mn"), mt = SP("mt mn"), bt = BS("d", "rp"); return [IT(RW(RL(PL("wn", "[?]"), nm, mt), bt)), nm, mt, bt]; }, (r, o) => {
        const ep = o.endpoint, a = armedStop === `orphan:${ep}`;
        T(r[1], `${o.external_name ? `${o.external_name} · ` : ""}${o.hardware}`); T(r[2], `${upTxt(o)} · ${spTxt(o)}`);
        D(r[3], "ep", ep); TG(r[3], "a", a); T(r[3], t(a ? "cs" : "stopBill"));
      });
    }
    T(qT, t("qa")); T(qbs[0], t("ng")); T(qbs[1], t("dg")); T(qbs[2], t("fw"));
  };
})();

const sv = (() => {
  const ttl = DV("tt"), flds = mkFlds([GF[0], ["mv", "min_vram_mib", "num", { min: 0, step: 1e3, placeholder: "70000" }], ...GF.slice(1), ["sn", "name", "txt"], ["hm", "high_mem", "chk", { id: "chk-hm" }]]);
  const updHint = () => { const a = flds[0][2].value, nh = NO_HM.has(a), rt = rateTxt(a); T(hint, `${rt ? `${rt} · ` : ""}${t(a === "A100" ? "pa1" : "pd")}`); flds[7][2].disabled = nh; if (nh) flds[7][2].checked = false; };
  const form = mkFrm("f-prov", flds, updHint), hint = SP("mt"), sub = B("b p", "pv");
  const psC = mkPsCard(), jL = DV({ c: "ls", h: 1 }), emp = mkEmp("nos"), sL = DV("ls"), eT = DV("tt"), eL = DV("ls"), eC = DV({ c: "cd", h: 1 }, eT, eL);
  $("v-sessions").append(CD(ttl, form, RW(hint, sub)), psC.el, jL, emp[0], sL, eC);

  return {
    form,
    r(s) {
      const def = s.settings?.defaults || {}, g = def.guard || {}, ss = s.sessions || [], jobs = (s.provision_jobs || []).filter((j) => !j.done || j.error), ended = s.ended_sessions || [];
      T(ttl, t("ns")); T(sub, t("sv")); for (const f of flds) T(f[1], t(f[3])); PH(flds[6][2], "an");
      if (!form.dataset.dirty) { [def.accelerator || "G4", def.min_vram_mib || "", ...gd(g)].forEach((v, i) => { flds[i][2].value = ST(v); }); flds[7][2].checked = Boolean(def.high_mem && !NO_HM.has(def.accelerator)); }
      updHint(); psC.r(s.pending_stops || []); H(jL, !jobs.length);
      sync(jL, jobs, (j) => j.job_id, () => { const bg = PL("ac", ""), nm = SP("mn"), st = DV("mu"), bt = BS("d", "cv"); return [CD(RW(RL(bg, nm), bt), st), nm, st, bt, bg]; }, (r, j) => {
        PT(r[4], t(j.error ? "gw" : "pv"), j.error ? "er" : "ac"); T(r[1], `${j.name} (${j.accelerator})`); T(r[2], j.error || j.last_status); D(r[3], "id", j.job_id); T(r[3], t("cc")); H(r[3], j.done);
      });
      T(emp[1], t("nos")); H(emp[0], ss.length); H(sL, !ss.length);
      sync(sL, ss, (x) => x.endpoint || x.display_name, () => {
        const nm = SP("tt mn"), hw = SP("pl"), wd = SP("pl"), stp = BS("d", ""), dsc = DV("mu"), up = SP(), rt = SP(), sp = SP();
        const bs = ["we", "", "qf", "ou", "rk"].map((a) => BS("", a)), acts = RL(...bs);
        return [CD(RW(RL(nm, hw, wd), stp), dsc, DV("rw mt mn", up, rt, sp), acts), nm, hw, wd, stp, dsc, up, rt, sp, acts, ...bs];
      }, (r, x) => {
        const { display_name: dn, endpoint: ep, hardware: hw, watchdog: wd, is_orphan: orp } = x;
        const sk = orp ? `orphan:${ep}` : `sess:${ep || dn}`, a = armedStop === sk, ps = x.pending_stop || x.state === "pending_stop";
        TG(r[0], "w", orp || ps || (!orp && !wd?.armed));
        T(r[1], sName(x)); PT(r[2], hw, ps || orp ? "wn" : "ok");
        if (ps) PT(r[3], t("ps"), "wn");
        else if (orp) PT(r[3], x.external_name ? "[?] CLI" : t("bo"), "wn");
        else { const rm = liveRem(wd); PT(r[3], wd?.armed ? (rm != null ? `${t("ar")} · ${fmtDur(rm)}` : t("ar")) : t("un"), wd?.armed ? "ok" : "wn"); }
        D(r[4], "a", orp ? "rp" : "st"); PE(r[4], dn, ep); TG(r[4], "a", a); T(r[4], t(a ? "cs" : "stopBill"));
        const shp = x.shape === "High-RAM" ? t("hm") : x.shape === "Standard" ? t("std") : x.shape;
        T(r[5], [hw, x.gpu, x.vram_mib ? `${MR(x.vram_mib / 1024)} GB` : null, shp, ep].filter(Boolean).join(" · "));
        T(r[6], upTxt(x)); T(r[7], x.rate_cu_per_hour != null ? `${x.rate_measured ? "" : "≈ "}${fmtCu(x.rate_cu_per_hour)} CU/h` : t("uk")); T(r[8], spTxt(x)); H(r[9], orp);
        if (!orp) {
          for (let i = 10; i <= 14; i++) PE(r[i], dn, ep);
          const wArm = armedWd === (ep || dn);
          T(r[10], t("e2")); D(r[11], "a", wd?.armed ? "wc" : "wa"); r[11].className = `b s ${wd?.armed && wArm ? "d a" : ""}`;
          T(r[11], t(wd?.armed ? (wArm ? "cfw" : "cw") : "aw")); T(r[12], t("fw")); T(r[13], t("oc")); T(r[14], t("rk"));
        }
      });
      H(eC, !ended.length);
      if (ended.length) {
        T(eT, t("ed"));
        sync(eL, ended.slice(0, 10), (e, i) => `${e.name}:${e.ended_at_epoch}:${i}`, () => { const a = SP("mn"), b = SP("pl"), c = SP("mt mn"); return [IT(RW(RL(a, b), c)), a, b, c]; }, (r, e) => {
          T(r[1], `${e.name} (${e.hardware})`); PT(r[2], stL(e.stop_reason || "stopped"), "t3"); T(r[3], `${fmtDur(e.duration_secs)} · ${e.spend_cu != null ? `${fmtCu(e.spend_cu)} CU` : "—"}`);
        });
      }
    },
  };
})();

const tv = (() => {
  const ttl = DV("tt"), flds = mkFlds([["se", "session", "sel"], ["lp", "local_port", "num", { min: 1024, max: 65535, v: 8000 }], ["rp", "remote_port", "num", { min: 1, max: 65535, v: 8000 }], ["hp", "health_path", "txt", { v: "/health", placeholder: "/health" }]]);
  const sel = flds[0][2], ntT = SP(), op = B("b p", "ot"), emp = mkEmp("not"), list = DV("ls");
  $("v-tunnels").append(DV({ c: "bnr", "data-k": "wn" }, ntT), CD(ttl, E("form", { id: "f-tun", c: "fg ft" }, ...flds.map((f) => f[0])), RW(SP(), op)), emp[0], list);
  return {
    sel,
    r(s) {
      const nm = (s.sessions || []).filter((x) => !x.is_orphan), ts = s.tunnels || [];
      T(ttl, t("fw")); T(ntT, t("tn")); T(op, t("op")); for (const f of flds) T(f[1], t(f[3]));
      syncSel(sel, nm); sel.disabled = op.disabled = !nm.length;
      T(emp[1], t("not")); H(emp[0], ts.length); H(list, !ts.length);
      sync(list, ts, (u) => u.id, () => {
        const u = SP("mn tt"), tg = SP("mt mn"), st = SP("pl"), hl = PL("", "", 1), b1 = BS("c", "ol"), b2 = BS("", "cp"), b3 = BS("d", "ct"), b4 = BS("", "dt"), er = DV({ c: "mu", h: 1 });
        return [CD(RW(RL(u, tg, st, hl), RL(b1, b2, b3, b4)), er), u, tg, st, hl, b1, b2, b3, b4, er];
      }, (r, u) => {
        const { id, local_url: url, spec, state: stO, healthy: hl } = u, st = stO?.state || "unknown", act = !isOff(st);
        T(r[1], url); T(r[2], `→ ${spec.session}:${spec.remote_port}`); PT(r[3], stL(st), st === "up" ? "ok" : st === "failed" ? "er" : st === "stopped" ? "t3" : "wn");
        H(r[4], hl == null); if (hl != null) PT(r[4], t(hl ? "ho" : "he"), hl ? "ok" : "wn");
        D(r[5], "url", url); T(r[5], t("ou")); H(r[5], !act); D(r[6], "v", url); T(r[6], t("cu"));
        D(r[7], "id", id); T(r[7], t("cl")); H(r[7], !act); D(r[8], "id", id); T(r[8], t("ds")); H(r[8], act);
        H(r[9], !stO?.error); if (stO?.error) T(r[9], stO.error.message);
      });
    },
  };
})();

const dv = (() => {
  const pN = SP("tt"), pI = PL("in", ""), pS = SL({ id: "sel-plg", st: "width:auto;min-width:130px" }), pD = DV("mu");
  const hfQ = IN({ id: "hf-q", v: selRepo, placeholder: selRepo, st: "flex:1" }), bS = BS("c", "hs"), bI = BS("", "hi"), hfM = DV({ c: "ls", h: 1 });
  const fitR = SP("mn"), fitP = RL(), fitB = DV({ c: "it", h: 1 }, RW(fitR, fitP)), hfW = DV({ c: "ls", h: 1 }, DV("hr"), RW(hfQ, bS, bI), hfM, fitB), fD = E("form", { id: "f-dep", c: "fg" });
  const dRt = SP("mt mn"), tok = IN({ id: "hf-tok", type: "password", st: "max-width:200px" }), bT = BS("", "tk"), tokP = PL("ok", "", 1);
  const bDep = B("b p", "dp"), emp = mkEmp("nor"), rL = DV("ls"), iT = DV("tt"), iIn = IN({ id: "plg-src", st: "flex:1" }), iB = BS("c", "ip");
  $("v-deploy").append(CD(RW(RL(pN, pI), pS), pD, hfW, fD, RW(DV({ c: "rl", st: "flex:1" }, tok, bT, tokP, dRt), bDep)), emp[0], rL, CD(iT, RW(iIn, iB)));
  let depFlds = [], tSel = null, aSel = null, sofCk = null;
  const updDep = () => { const ex = Boolean(tSel?.value); if (aSel) aSel.disabled = ex; if (sofCk) sofCk.disabled = ex; T(dRt, ex ? "" : rateTxt(aSel?.value || "G4")); };
  function buildForm(mf) {
    fD.replaceChildren(); depFlds = [];
    tSel = SL({ name: "__session" }); const tl = LB(), al = LB(), sl = LB();
    aSel = mkAcc(KA); aSel.value = mf.provision?.accelerator || "G4";
    sofCk = IN({ name: "__sof", type: CK, ck: true }); tSel.onchange = aSel.onchange = updDep;
    fD.append(DV("fi", tl, tSel), DV("fi", al, aSel), DV("fi ck", sofCk, sl)); depFlds.push([tl, "ts"], [al, "ac"], [sl, "sof"]);
    for (const inp of mf.inputs || []) {
      const { id, type: ty, options: opts, required: req } = inp, v = id === "repo" ? selRepo : (inp.default ?? "");
      const ctl = ty === "select" ? SL({ name: id }, ...(opts || []).map((o) => OP(o, o)))
        : ty === "bool" ? IN({ name: id, type: CK, ck: !!v }) : IN({ name: id, type: ty === "number" ? "number" : ty === "secret" ? "password" : "text", v: ST(v), required: !!req });
      if (ty === "select") ctl.value = ST(v);
      const lb = LB(); fD.append(ty === "bool" ? DV("fi ck", ctl, lb) : DV("fi", lb, ctl)); depFlds.push([lb, inp]);
    }
  }
  return {
    hfQ, fD,
    r(s) {
      const plgs = s.plugins || [], runs = s.plugin_runs || [], ss = s.sessions || [], cur = plgs.find((p) => p.manifest.id === selPlg) || plgs[0], mf = cur?.manifest;
      sync(pS, plgs, (p) => p.manifest.id, () => [OP()], (r, p) => { r[0].value = p.manifest.id; T(r[0], loc(p.manifest.name)); });
      if (mf) pS.value = mf.id;
      T(pN, mf ? loc(mf.name) : t("t.deploy")); T(pI, mf ? mf.id : ""); H(pI, !mf); T(pD, mf ? loc(mf.description) : "");
      H(hfW, mf?.id !== "llama-cpp"); T(bS, t("sr")); T(bI, t("in")); H(hfM, !hfModels.length);
      if (hfModels.length) sync(hfM, hfModels.slice(0, 5), (m) => m.id, () => { const id = SP("mn el"), dl = SP("mt mn"), bt = BS("", "pr"); return [IT(RW(id, RL(dl, bt))), id, dl, bt]; }, (r, m) => { T(r[1], m.id); T(r[2], `↓${m.downloads.toLocaleString()}`); D(r[3], "v", m.id); T(r[3], t("in")); });
      H(fitB, !hfFit);
      if (hfFit) { T(fitR, hfFit.repo); sync(fitP, hfFit.fits || [], (f) => f.vram_gib, () => [SP("pl")], (r, f) => PT(r[0], `${f.vram_gib}GB → ${f.fits ? `${f.quant} (${f.kv})` : "OOM"}`, f.fits ? "ok" : "er")); }
      if (mf && builtPlg !== mf.id) { builtPlg = mf.id; buildForm(mf); }
      if (tSel) syncSel(tSel, ss.filter((x) => !x.is_orphan), t("nv"));
      for (const [lb, sp] of depFlds) T(lb, typeof sp === "string" ? t(sp) : (loc(sp.label) || sp.id));
      updDep(); PH(tok, "ht"); T(bT, t("st")); T(tokP, t("hs")); H(tokP, !s.hf_token_set); T(bDep, t("dp"));
      T(iT, t("ip")); PH(iIn, "pls"); T(iB, t("ib")); T(emp[1], t("nor")); H(emp[0], runs.length); H(rL, !runs.length);
      sync(rL, runs, (r) => r.run_id, () => {
        const id = SP("tt mn"), se = SP("mt mn"), st = SP("pl"), cn = BS("d", "cr"), sb = BS("d", "st"), bar = E("i"), mg = DV("mu"), eps = DV({ c: "ls", h: 1 }), lg = E("pre", { c: "lg", h: 1 });
        return [CD(RW(RL(id, se, st), RL(sb, cn)), DV("pg", bar), mg, eps, lg), id, se, st, cn, bar, mg, eps, lg, sb];
      }, (r, x) => {
        const { run_id: rid, state: st, stage, progress: prg, session: ses, endpoint: ep, plugin_id: pid, message: msg, endpoints: eps, log_tail: lgs, created } = x;
        const pct = MX(0, MN(100, MR((prg ?? (st === "ready" ? 1 : 0.15)) * 100))), liveS = created && ep ? ss.find((z) => z.endpoint === ep) : null;
        T(r[1], pid); T(r[2], ses); PT(r[3], `${stL(st)}${stage ? ` · ${stage}` : ""}`, st === "ready" ? "ok" : st === "failed" ? "er" : "ac");
        D(r[4], "id", rid); T(r[4], t("cnr")); H(r[4], !["running", "ready", "cancel_failed"].includes(st));
        const showSb = Boolean(liveS && (st === "failed" || st === "cancel_failed"));
        H(r[9], !showSb); if (showSb) { const a = armedStop === `sess:${liveS.endpoint}`; PE(r[9], liveS.display_name, liveS.endpoint); TG(r[9], "a", a); T(r[9], t(a ? "cs" : "sbs")); }
        r[5].style.width = `${pct}%`; T(r[6], msg);
        const live = (eps || []).filter((e) => e.local_url); H(r[7], !live.length);
        sync(r[7], live, (e) => `${e.local_url}${e.path}`, () => { const u = SP("mn el"), op = BS("c", "ol"), cp = BS("", "cp"), cu = BS("", "cp"); return [IT(RW(u, RL(op, cp, cu))), u, op, cp, cu]; }, (er, e) => {
          const f = `${e.local_url}${e.path === "/" ? "" : e.path}`;
          T(er[1], f); D(er[2], "url", e.local_url); T(er[2], t("ou")); D(er[3], "v", f); T(er[3], t("cu")); D(er[4], "v", `curl -sS ${f}`); T(er[4], t("cr"));
        });
        const logs = (lgs || []).join("\n"); H(r[8], !logs); T(r[8], logs);
      });
    },
  };
})();

const setv = (() => {
  const bmT = SP("tt"), bmP = SP("pl"), bD = B("b", "bm", "", { "data-v": "mock" }), bR = B("b", "bm", "", { "data-v": "real" });
  const aT = DV("tt"), aW = DV("mu"), aL = DV("ls"), swT = DV("tt"), swB = DV("ls"), swOk = BS("p", "swc"), swNo = BS("", "swx"), swC = DV({ c: "it", h: 1 }, swT, swB, RL(swOk, swNo));
  const aId = IN({ id: "acc-id" }), aLb = IN({ id: "acc-lbl" }), aEm = IN({ id: "acc-em", c: "ae", placeholder: "user@example.com" }), aBt = BS("c", "ad");
  const dT = DV("tt"), flds = mkFlds([...GF, ["mc", KMC, "num", { min: 1, max: 8 }]]), form = mkFrm("f-set", flds);
  const sP = SP("mt mn el"), svB = B("b p", "sv"), docT = DV("tt"), d1 = SP("pl"), d2 = SP("pl"), d3 = SP("pl"), d4 = SP("mt mn el"), lSsh = SP(), lPath = SP();
  $("v-settings").append(CD(RW(bmT, bmP), RL(bD, bR)), CD(aT, aW, aL, swC, DV("af", aId, aLb, aEm, aBt)), CD(dT, form, RW(sP, svB)), CD(docT, DV("g2 mu", RL("colab CLI:", d1), RL("ADC:", d2), RL(lSsh, d3), RL(lPath, d4))));
  const jn = (a, f) => (a || []).map(f).join(", ") || "0";
  return {
    form,
    r(s) {
      const def = s.settings?.defaults || {}, g = def.guard || {}, accs = s.accounts || [], env = s.env || {}, mock = s.backend_mode === "mock";
      T(bmT, t("bm")); PT(bmP, t(mock ? "dm" : "rl"), mock ? "wn" : "ok");
      T(bD, t("sd")); T(bR, t("srl")); bD.className = `b ${mock ? "c" : ""}`; bR.className = `b ${!mock ? "c" : ""}`;
      T(aT, t("acc")); T(aW, t("sww")); T(aBt, t("aa")); PH(aId, "ai"); PH(aLb, "al");
      sync(aL, accs, (a) => a.id, () => { const l = SP("tt"), m = SP("pl"), em = SP("mt mn"), ac = PL("ok", ""), sw = BS("", "sw"), rm = BS("d", "rm", "×"); return [IT(RW(RL(l, m, em, ac), RL(sw, rm))), l, m, em, ac, sw, rm]; }, (r, a) => {
        const act = a.id === actA()?.id;
        T(r[1], a.label); PT(r[2], a.auth_mode.toUpperCase(), a.auth_mode === "adc" ? "in" : "ac");
        T(r[3], a.email || ""); H(r[3], !a.email); T(r[4], t("act")); H(r[4], !act);
        D(r[5], "id", a.id); H(r[5], act); T(r[5], t("sw")); D(r[6], "id", a.id); H(r[6], a.id === "adc-default");
      });
      H(swC, !swPrev);
      if (swPrev) {
        const d = swPrev.data || {}, uw = d.unwatched_sessions || [], or = d.orphan_sessions || [], rows = [
          [t("swt"), ST(d.closing_tunnels?.length ?? d.open_tunnels ?? 0), "t3"],
          [t("swwa"), jn(d.watched_sessions, (x) => `${x.name}${x.remaining_secs != null ? ` (${fmtDur(x.remaining_secs)})` : ""}`), "ok"],
          [t("swu"), jn(uw, (x) => x.name), uw.length ? "er" : "t3"],
          [t("swo"), jn(or, (x) => x.external_name || x.endpoint), or.length ? "wn" : "t3"],
        ];
        T(swT, `${t("swi")} → ${swPrev.id}`); T(swOk, t("cfs")); T(swNo, t("cc"));
        sync(swB, rows, (x) => x[0], () => { const a = SP("mu"), b = SP("pl"); return [RW(a, b), a, b]; }, (r, x) => { T(r[1], x[0]); PT(r[2], x[1], x[2]); });
      }
      T(dT, t("df")); T(svB, t("ss")); for (const f of flds) T(f[1], t(f[3]));
      if (!form.dataset.dirty) [def.accelerator || "G4", ...gd(g), def[KMC] ?? 2].forEach((v, i) => { flds[i][2].value = ST(v); });
      T(sP, s.settings_path || ""); T(docT, t("dc")); T(lSsh, t("sk")); T(lPath, t("pt"));
      PT(d1, env.colab_found ? (env.colab_version || "OK") : t("mg"), env.colab_found ? "ok" : "er");
      PT(d2, t(env.adc_file_present ? "pr" : "nf"), env.adc_file_present ? "ok" : "wn");
      PT(d3, env.ssh_key_ready ? "ED25519/ECDSA OK" : t("acr"), env.ssh_key_ready ? "ok" : "wn");
      T(d4, env.colab_path || "—");
    },
  };
})();

function renderHeader() {
  const s = snap, mock = s?.backend_mode === "mock", ss = s?.sessions || [], ps = isPs(s), aa = actA();
  A(DC.documentElement, "lang", lang); D($("app"), "ps", ps ? "1" : "0");
  K($("h-dot"), s?.last_error ? "er" : down || ps ? "wn" : ss.length ? "ok" : "t3");
  PT($("h-mode"), mock ? t("dm") : (aa?.auth_mode === "oauth2" ? "OAuth2" : "ADC"), mock ? "wn" : "in");
  T($("h-sub"), aa?.email || aa?.label || "");
  const ab = $("auto-btn"); TG(ab, "on", auto && !down);
  T($("auto-lbl"), auto ? `${t("au")} ${MR(delayMs / 1e3)}s` : t("pa"));
  if (lastSync) A(ab, "title", `Sync ${new Date(lastSync).toLocaleTimeString()}`);
  QA('.seg button[data-a="lg"]').forEach((b) => A(b, "aria-pressed", ST(b.dataset.v === lang)));
  const actT = (s?.tunnels || []).filter((x) => !isOff(x.state?.state)).length;
  QA(TBQ).forEach((b) => {
    const k = b.dataset.tab, n = k === "sessions" ? ss.length : k === "tunnels" ? actT : 0;
    T(b, `${t(`t.${k}`)}${n ? ` (${n})` : ""}`); A(b, "aria-selected", ST(k === tab));
  });
  syncBnr();
}

const VIEWS = { overview: ov, sessions: sv.r, tunnels: tv.r, deploy: dv.r, settings: setv.r };
const renderAll = () => { renderHeader(); for (const n of TABS) H($(`v-${n}`), tab !== n); if (snap) VIEWS[tab](snap); };
const applySnap = (d, epk = epoch) => {
  if (!d || d.unchanged || (snap && epk === snapEp && d.rev != null && d.rev < snap.rev)) return;
  snap = d; snapEp = epk; snapAt = lastSync = Date.now(); if (d.settings?.locale && !pickedLang) lang = d.settings.locale === "en" ? "en" : "zh-CN"; renderAll();
};
const onSnap = (d, epk) => { if (d?.snapshot) applySnap(d.snapshot, epk); else if (d?.rev) applySnap(d, epk); };
function schedulePoll() {
  CT(pollT);
  const a = (snap?.sessions?.length || 0) > 0 || (snap?.provision_jobs || []).some((j) => !j.done) || (snap?.plugin_runs || []).some((r) => r.state === "running");
  delayMs = down ? MN(3e4, 5e3 * 2 ** MX(0, failCount - 3)) : (rttMs > 1200 ? MN(3e4, a ? 8e3 : 24e3) : (a ? 4e3 : 12e3));
  renderHeader();
  if (connOk && auto && !DC.hidden && emb()) pollT = setTimeout(() => poll(false), delayMs);
}
async function ensureConn() { if (!emb()) return false; if (hst()) return (connOk = true); await bridge.connect(); epoch += 1; return (connOk = hst()); }
async function poll(force = false) {
  if (busy || !emb()) return;
  busy = true; $("swp")?.classList.add("on");
  const t0 = Date.now(), epk = epoch;
  try {
    if (!await ensureConn()) throw new Error("No serverTools");
    const res = await bridge.callTool("colab_app_poll", { since_rev: force ? null : (snap?.rev ?? null), force }), p = parseR(res);
    if (res?.isError) throw new Error(p?.error || res.content?.[0]?.text || "Poll error");
    rttMs = MX(10, MR(Date.now() - t0 - (+p?.server_ms || 0)));
    const wasDown = down; failCount = 0; down = false; lastSync = Date.now();
    if (p && !p.unchanged) applySnap(p, epk);
    if (wasDown) { bnr = null; syncBnr(); flash(t("reconnected")); }
  } catch (e) {
    failCount += 1;
    if (failCount >= 3) { down = true; bnrOff = false; } else if (force) showBnr(e.message || ST(e), "er", 1);
  } finally { busy = false; $("swp")?.classList.remove("on"); schedulePoll(); }
}
async function act(tool, args, msgOk) {
  const epk = epoch;
  try {
    if (!await ensureConn()) { showBnr("Open in Antigravity", "in"); return null; }
    const res = await bridge.callTool(tool, args), d = parseR(res);
    if (res?.isError) { showBnr(d?.error || res.content?.[0]?.text || "Error", "er"); return null; }
    bnr = null; syncBnr(); onSnap(d, epk);
    if (msgOk) flash(msgOk);
    return d;
  } catch (e) { showBnr(e.message || ST(e), "er"); return null; }
}
const A1 = (tl, ac, ex, m) => act(tl, OA({ action: ac }, ex), m);
const okP = (n, lo) => NI(n) && n >= lo && n <= 65535;
const formObj = (id) => { const f = $(id), o = {}; if (f) { for (const [k, v] of new FormData(f).entries()) o[k] = v; QA("input[type=checkbox]", f).forEach((c) => { o[c.name] = c.checked; }); } return o; };
const gArgs = (f) => ({ accelerator: f.accelerator || "G4", max_runtime_secs: posN(f.max_runtime_h) ? MR(posN(f.max_runtime_h) * 3600) : null, idle_timeout_secs: posN(f.idle_min) ? MR(posN(f.idle_min) * 60) : null, max_spend_cu: posN(f.max_spend_cu), min_balance_cu: optN(f.min_balance_cu) });
const doStop = (isRp, name, ep, ds) => { const k = isRp ? `orphan:${ep}` : `sess:${ep || name}`; if (armedStop !== k) { armedStop = k; return renderAll(); } armedStop = null; return A1(TS, isRp ? "reap_orphan" : "stop", isRp ? P({ endpoint: ep }) : P({ session: name }, ds), "…"); };
const doInsp = async (repo) => { selRepo = repo; dv.hfQ.value = repo; const ri = Q('[name="repo"]', dv.fD); if (ri) ri.value = repo; const d = await A1(TP, "inspect_hf", { repo, accelerator: Q(`[name="${KA}"]`, dv.fD)?.value || "G4" }); if (d?.fits) { hfFit = d; dv.r(snap); } };

ON(DC, "click", async (ev) => {
  const tb = ev.target.closest(TBQ);
  if (tb) { tab = tb.dataset.tab; armedStop = armedWd = swPrev = null; return renderAll(); }
  const b = ev.target.closest("[data-a]"); if (!b) return;
  const ds = b.dataset, { a, id, name, ep, v, url } = ds, ok = t("ok"), trm = (i) => $(i)?.value?.trim() || "", PS = (ex) => P(OA({ session: name }, ex), ds);
  const mp = {
    cv: [TS, "cancel_provision", P({ job_id: id }), ok], we: [TW, "extend", PS({ hours: 2 }), "+2h"],
    wa: [TW, "arm", PS({ hours: 2 }), ok], rk: [TS, "restart_kernel", PS(), ok],
    ct: [TT, "close", { tunnel_id: +id }, ok], dt: [TT, "dismiss", { tunnel_id: +id }, ok],
    cr: [TP, "cancel_run", P({ run_id: id }), ok], rm: [TA, "remove", { account_id: id }, ok],
    rs: [TS, name ? "stop" : "reap_orphan", name ? PS() : P({ endpoint: ep }), "…"],
  };
  if (mp[a]) return A1(...mp[a]);
  const fn = {
    xb: () => { bnr = null; bnrOff = true; syncBnr(); },
    rc: () => { bnr = null; poll(true); },
    au: () => { auto = !auto; schedulePoll(); },
    rf: () => poll(true),
    lg: () => { pickedLang = true; lang = v; renderAll(); if (hst()) A1(TK, "update", { patch: { locale: lang } }); },
    gt: () => { tab = v; renderAll(); },
    ol: () => bridge.openLink(url),
    cp: async () => { try { await navigator.clipboard.writeText(v || ""); flash(t("cp")); } catch { flash(v || ""); } },
    qf: () => { tab = "tunnels"; renderAll(); if (name) tv.sel.value = name; },
    pv: async () => { const f = formObj("f-prov"); if (!chkN(f, [...NK, "min_vram_mib"])) return showBnr(t("bn"), "er"); const r = await A1(TS, "provision", P(OA({ name: f.name?.trim() || null, high_mem: !!f.high_mem, min_vram_mib: posN(f.min_vram_mib) }, gArgs(f))), "…"); if (r) delete sv.form.dataset.dirty; },
    st: () => doStop(0, name, ep, ds), rp: () => doStop(1, name, ep, ds),
    wc: () => { const k = ep || name; if (armedWd !== k) { armedWd = k; return sv.r(snap); } armedWd = null; return A1(TW, "cancel", PS(), ok); },
    ou: async () => { const d = await A1(TS, "url", PS()); if (d?.url) bridge.openLink(d.url); },
    ot: () => { const f = formObj("f-tun"), lp = optN(f.local_port), rp = optN(f.remote_port); if (!okP(lp, 1024) || !okP(rp, 1) || rp === 8080) return showBnr(t("pe"), "er"); return A1(TT, "open", { session: f.session, local_port: lp, remote_port: rp, health_path: f.health_path?.trim() || null }, ok); },
    hs: async () => { selRepo = dv.hfQ.value.trim(); const d = await A1(TP, "search_hf", { query: selRepo }); if (d?.models) { hfModels = d.models; dv.r(snap); } },
    hi: () => doInsp(dv.hfQ.value.trim() || selRepo), pr: () => doInsp(v),
    tk: async () => { if (await A1(TA, "set_hf_token", { token: trm("hf-tok") }, ok)) clr("hf-tok"); },
    dp: () => { const f = formObj("f-dep"), acc = f[KA] || "G4", sess = f.__session || null, sof = !sess && !!f.__sof; delete f[KA]; delete f.__session; delete f.__sof; return A1(TP, "deploy", P({ plugin_id: selPlg, session: sess, accelerator: acc, stop_on_failure: sof, inputs: f }), "…"); },
    ip: async () => { const s = trm("plg-src"); if (!s) return; const g = s.includes("://") || s.includes("@") || s.endsWith(".git"); if (await A1(TP, g ? "install_git" : "install_dir", g ? { url: s } : { path: s }, ok)) clr("plg-src"); },
    bm: async () => { if (await A1(TK, "update", { patch: { backend_mode: v } }, ok)) { epoch += 1; snap = null; return poll(true); } },
    sw: async () => { const d = await A1(TA, "preview_switch", { account_id: id }); if (d) { swPrev = { id, data: d }; setv.r(snap); } },
    swx: () => { swPrev = null; setv.r(snap); },
    swc: () => { if (!swPrev) return; const tid = swPrev.id; swPrev = null; return A1(TA, "switch", { account_id: tid, confirm: true }, ok); },
    ad: async () => { const aid = trm("acc-id"); if (!aid) return; if (await A1(TA, "add", { account_id: aid, label: trm("acc-lbl") || aid, email: trm("acc-em") || null, auth_mode: "oauth2" }, ok)) clr("acc-id", "acc-lbl", "acc-em"); },
    sv: async () => { const f = formObj("f-set"), mc = optN(f[KMC]); if (!chkN(f, NK) || (mc != null && (!NI(mc) || mc < 1 || mc > 8))) return showBnr(t("bn"), "er"); if (await A1(TK, "update", { patch: OA(gArgs(f), { [KMC]: mc || 2 }) }, ok)) delete setv.form.dataset.dirty; },
  }[a];
  if (fn) return fn();
});

ON(DC, "change", (ev) => { if (ev.target?.id === "sel-plg") { selPlg = ev.target.value; dv.r(snap); } });
ON(DC, "visibilitychange", () => { if (DC.hidden) CT(pollT); else if (connOk && auto && emb()) poll(false); });
bridge.ontoolresult = (res) => onSnap(parseR(res));
if (window.__COLAB_BOOT__ && typeof window.__COLAB_BOOT__ === "object") applySnap(window.__COLAB_BOOT__); else renderHeader();
setInterval(() => { if (!DC.hidden && (tab === "sessions" || tab === "overview") && snap?.sessions?.length) VIEWS[tab](snap); }, 1e3);
const setDown = () => { failCount = 3; down = true; syncBnr(); };
if (emb()) bridge.connect().then((ok) => { if (!ok || !hst()) return setDown(); connOk = true; schedulePoll(); }).catch(setDown);