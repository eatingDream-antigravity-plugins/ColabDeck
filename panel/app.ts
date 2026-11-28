import { App, type ToolCallResult } from "./kit/bridge.ts";
import { tr, fmtDur, fmtCu, fmtErr, projectChart, type BalancePoint, type Locale, type LocalizedText } from "./i18n.ts";
import { adaptPollDelay, canSchedulePoll, readCachedSnap, selectBootHydration, updateEwmaRtt, writeCachedSnap, type StorageLike } from "./sync.ts";

interface Account {
  id: string;
  label?: string;
  email?: string | null;
  auth_mode?: string;
}

interface Watchdog {
  armed?: boolean;
  deadline_epoch?: number | null;
  remaining_secs?: number | null;
}

interface GuardConcern {
  kind?: string;
  limit?: string;
  current?: number;
  threshold?: number;
}

interface GuardDecision {
  decision?: string;
  concerns?: GuardConcern[];
}

interface Session {
  display_name?: string;
  endpoint?: string;
  hardware?: string;
  watchdog?: Watchdog | null;
  is_orphan?: boolean;
  shape?: string;
  gpu?: string;
  vram_mib?: number;
  rate_cu_per_hour?: number | null;
  rate_measured?: boolean;
  pending_stop?: boolean;
  state?: string;
  external_name?: string;
  started_at_epoch?: number | null;
  uptime_secs?: number | null;
  start_source?: string;
  spend_cu?: number | null;
  spend_estimated?: boolean;
  guard_decision?: GuardDecision | null;
}

interface PendingStop {
  endpoint?: string;
  session?: string;
  is_orphan?: boolean;
  status?: { last_error?: string } | null;
}

interface ProvisionJob {
  job_id: string;
  name?: string;
  accelerator?: string;
  done?: boolean;
  error?: unknown;
  last_status?: string;
}

interface EndedSession {
  name?: string;
  hardware?: string;
  stop_reason?: string;
  ended_at_epoch?: number;
  duration_secs?: number;
  spend_cu?: number | null;
}

interface TunnelSpec {
  session: string;
  remote_port: number;
}

interface TunnelState {
  state?: string;
  error?: { ui_kind?: string; localized?: LocalizedText | null } | null;
}

interface Tunnel {
  id: number | string;
  local_url: string;
  spec: TunnelSpec;
  state?: TunnelState | null;
  healthy?: boolean | null;
}

interface PluginInput {
  id: string;
  type?: string;
  label?: LocalizedText;
  help?: LocalizedText;
  default?: string | number | boolean;
  options?: string[];
  required?: boolean;
}

interface PluginStage {
  id: string;
  label?: LocalizedText;
}

interface PluginManifest {
  id: string;
  name?: LocalizedText;
  description?: LocalizedText;
  provision?: { accelerator?: string };
  inputs?: PluginInput[];
  stages?: PluginStage[];
}

interface PluginItem {
  manifest: PluginManifest;
}

interface PluginEndpoint {
  local_url?: string;
  path?: string;
  kind?: string;
}

interface PluginRun {
  run_id: string;
  state?: string;
  stage?: string;
  progress?: number | null;
  session?: string;
  endpoint?: string;
  plugin_id?: string;
  message?: string;
  endpoints?: PluginEndpoint[];
  log_tail?: string[];
  created?: boolean;
}

interface HfModel {
  id: string;
  downloads: number;
}

interface HfFitItem {
  vram_gib: number;
  fits: boolean;
  quant?: string;
  kv?: string;
}

interface HfFitResult {
  repo: string;
  fits?: HfFitItem[];
}

interface SwitchPreviewData {
  closing_tunnels?: unknown[];
  open_tunnels?: number;
  watched_sessions?: { name: string; remaining_secs?: number | null }[];
  unwatched_sessions?: { name: string }[];
  orphan_sessions?: { external_name?: string; hardware?: string; endpoint?: string }[];
}

interface Usage {
  projected_balance_cu?: number | null;
  stale?: boolean;
  rate_cu_per_hour?: number | null;
  eta_secs?: number | null;
}

interface EnvInfo {
  colab_found?: boolean;
  colab_version?: string;
  adc_file_present?: boolean;
  ssh_key_ready?: boolean;
  colab_path?: string;
}

interface GuardSettings {
  max_runtime_secs?: number | null;
  idle_timeout_secs?: number | null;
  max_spend_cu?: number | null;
  min_balance_cu?: number | null;
}

interface Settings {
  locale?: string;
  defaults?: {
    accelerator?: string;
    min_vram_mib?: number | null;
    high_mem?: boolean;
    max_concurrent_sessions?: number;
    poll_active_secs?: number;
    poll_idle_secs?: number;
    guard?: GuardSettings;
  };
}

interface HardwareRate {
  cu_per_hour?: number;
  measured?: boolean;
  high_mem_cu_per_hour?: number | null;
  high_mem_measured?: boolean;
}

interface Snap {
  rev: number;
  bootTs?: number;
  partial?: boolean;
  unchanged?: boolean;
  syncing?: boolean;
  backend_mode?: string;
  guard_paused?: boolean;
  host_guard_paused?: boolean;
  settings_error?: string | null;
  last_error?: string | null;
  last_error_kind?: string | null;
  last_error_localized?: LocalizedText | null;
  active_account?: Account | null;
  accounts?: Account[];
  settings?: Settings;
  settings_path?: string;
  env?: EnvInfo | null;
  usage?: Usage | null;
  balance_history?: BalancePoint[];
  sessions?: Session[];
  pending_stops?: PendingStop[];
  provision_jobs?: ProvisionJob[];
  ended_sessions?: EndedSession[];
  tunnels?: Tunnel[];
  plugins?: PluginItem[];
  plugin_runs?: PluginRun[];
  hf_token_set?: boolean;
  hardware_rates?: Record<string, HardwareRate>;
  server_ms?: number;
  snapshot?: Snap;
}

interface McpContentItem {
  type?: string;
  text?: string;
}

interface McpToolResult extends ToolCallResult {
  isError?: boolean;
  content?: McpContentItem[];
}

interface RpcError extends Error {
  code?: string;
  loc?: LocalizedText | null;
}

interface BannerState {
  k: string;
  m: string;
  r?: number;
  raw?: string | null;
  key: string;
}

type TabId = "overview" | "sessions" | "tunnels" | "deploy" | "settings";
type FieldKind = "acc" | "sel" | "chk" | "txt" | "number";
type FieldDef = readonly [string, string, FieldKind, (Record<string, unknown> | null)?, string?];
type DirtyForm = HTMLFormElement & { _d?: number };
type ElemAttrs = string | Record<string, unknown> | null | undefined;
type ElemChild = Node | string | number | boolean | null | undefined;

const ACCS = "G4 A100 H100 L4 T4 NONE V5E1 V6E1".split(" "), GPU_ACCS = ACCS.slice(0, 5), NO_HM = new Set(["L4", "V5E1", "V6E1"]);
const TABS: readonly TabId[] = ["overview", "sessions", "tunnels", "deploy", "settings"];
const NS = "http://www.w3.org/2000/svg", CK = "checkbox", NM = "number", PW = "password", DC = document, ST = String, CT = clearTimeout;
const CM = (x: string): string => `colab_${x}`, TS = CM("session_action"), TT = CM("tunnel_action"), TP = CM("app_plugins"), TA = CM("app_accounts"), TK = CM("app_settings"), TW = CM("app_watchdog"), LL = "llama-cpp";
const RNG: readonly (readonly [number, string])[] = [[3600, "c1"], [21600, "c2"], [86400, "c3"], [604800, "c4"], [0, "c5"]], BNT = "bridge_no_tools", CLE = "cli_error";
const AP = "aria-pressed", AS = "aria-selected", OK = "ok", WN = "wn", ER = "er", IN0 = "in", AC = "ac", T3 = "t3", ZH: Locale = "zh-CN", MT = "mt mn", PL0 = "pl", LS = "ls", TT0 = "tt", MU = "mu", MN0 = "mn", BS0 = "b s";
const $ = (i: string): HTMLElement => DC.getElementById(i)!, Q = <E extends Element = Element>(s: string, r: ParentNode = DC): E | null => r.querySelector<E>(s), QA = <E extends Element = Element>(s: string, r: ParentNode = DC): NodeListOf<E> => r.querySelectorAll<E>(s), ON = <K extends keyof DocumentEventMap>(e: Document, k: K, f: (ev: DocumentEventMap[K]) => void): void => e.addEventListener(k, f);
const OA = Object.assign, OE = Object.entries, M = Math, MR = M.round, MX = M.max, MN = M.min, NF = Number.isFinite, NI = Number.isInteger;
const isOff = (s: string | undefined): boolean => s === "stopped" || s === "failed";
const GF: readonly FieldDef[] = [["ac", "accelerator", "acc", null], ["mr", "max_runtime_h", NM, { min: 0, step: 0.5 }], ["im", "idle_min", NM, { min: 0, step: 5 }], ["ms", "max_spend_cu", NM, { min: 0, step: 1 }], ["bf", "min_balance_cu", NM, { min: 0, step: 1 }]];
const NK = GF.slice(1).map((x) => x[1]), KA = "__accelerator", KMC = "max_concurrent_sessions", KPA = "poll_active_secs", KPI = "poll_idle_secs", TBQ = "#tb button";

const T = (n: Element, v: unknown): void => { const s = v == null ? "" : ST(v); if (n.textContent !== s) n.textContent = s; };
const L = (n: Element, k: string, v?: Record<string, unknown>): void => T(n, t(k, v)), H = (n: HTMLElement | SVGElement, h: unknown): void => { if ("hidden" in n) n.hidden = !!h; else n.style.display = h ? "none" : ""; };
const HE = (e: HTMLElement, l: HTMLElement, n: unknown): void => { H(e, n); H(l, !n); };
const D = (n: HTMLElement, k: string, v: unknown): void => { n.dataset[k] = v == null ? "" : ST(v); };
const K = (n: HTMLElement, v: unknown): void => D(n, "k", v), PT = (n: HTMLElement, v: unknown, k: string): void => { T(n, v); K(n, k); }, PK = (n: HTMLElement, k: string, c: string): void => PT(n, t(k), c), PE = (n: HTMLElement, nm: unknown, ep: unknown): void => { D(n, "name", nm); D(n, "ep", ep); };
const TG = (n: Element, c: string, v: unknown): void => { n.classList.toggle(c, !!v); };
const A = (n: Element, k: string, v: unknown): void => { if (v == null) n.removeAttribute(k); else n.setAttribute(k, ST(v)); };

const I18: [Element, string, string | undefined][] = [], R = <T extends Element>(n: T, k: string, a?: string): T => (I18.push([n, k, a]), n);
function E<K extends keyof HTMLElementTagNameMap>(tag: K, a?: ElemAttrs, ...ks: ElemChild[]): HTMLElementTagNameMap[K] {
  const n = DC.createElement(tag);
  if (typeof a === "string") n.className = a;
  else if (a) for (const [k, v] of OE(a)) if (v != null && v !== false) {
    if (k === "c") n.className = ST(v);
    else if (k === "t") n.textContent = ST(v);
    else if (k === "h") n.hidden = !!v;
    else if (k === "v" && "value" in n) (n as HTMLInputElement).value = ST(v);
    else if (k === "ck" && "checked" in n) (n as HTMLInputElement).checked = !!v;
    else n.setAttribute(k, v === true ? "" : ST(v));
  }
  for (const c of ks) if (c != null && c !== false) n.append(typeof c === "object" ? c : ST(c));
  return n;
}
const DV = (c?: ElemAttrs, ...k: ElemChild[]): HTMLDivElement => E("div", c, ...k), DH = (c: string, ...k: ElemChild[]): HTMLDivElement => DV({ c, h: 1 }, ...k), CD = (...k: ElemChild[]): HTMLDivElement => DV("cd", ...k), CK0 = (k: string, ...c: ElemChild[]): HTMLDivElement => DV({ c: "cd", "data-k": k, h: 1 }, ...c), RW = (...k: ElemChild[]): HTMLDivElement => DV("rw", ...k), RL = (...k: ElemChild[]): HTMLDivElement => DV("rl", ...k), IT = (...k: ElemChild[]): HTMLDivElement => DV("it", ...k);
const SP = (c?: string, x?: string): HTMLSpanElement => E("span", x ? { c, t: x } : c), S3 = (n: number, c?: string): HTMLSpanElement[] => Array.from({ length: n }, () => SP(c)), PL = (k: string, x: string, h?: number): HTMLSpanElement => E("span", { c: PL0, "data-k": k, t: x, h });
const IN = (o?: ElemAttrs, ph?: string): HTMLInputElement => { const n = E("input", o); return ph ? R(n, ph, "placeholder") : n; }, LB = (k?: string): HTMLLabelElement => { const n = E("label", "kl"); return k ? R(n, k) : n; };
const SL = (o?: ElemAttrs, ...k: ElemChild[]): HTMLSelectElement => E("select", o, ...k), OP = (v?: string, x?: string): HTMLOptionElement => E("option", v != null ? { v, t: x } : null);
const B = (c: string, a: string, k?: string | null, ex?: Record<string, unknown>): HTMLButtonElement => { const n = E("button", OA({ c, "data-a": a }, ex)); return k ? R(n, k) : n; }, BS = (x: string, a: string, k?: string | null, ex?: Record<string, unknown>): HTMLButtonElement => B(x ? `${BS0} ${x}` : BS0, a, k, ex);
const S = <K extends keyof SVGElementTagNameMap>(tag: K, a?: Record<string, unknown> | null, ...ks: ElemChild[]): SVGElementTagNameMap[K] => { const n = DC.createElementNS(NS, tag); for (const [k, v] of OE(a || {})) if (v != null) n.setAttribute(k, ST(v)); for (const c of ks) if (c) n.append(typeof c === "object" ? c : ST(c)); return n; };

for (const n of TABS) { $("tb").append(E("button", { c: "tb", role: "tab", "data-tab": n, [AS]: ST(n === TABS[0]) })); $("mn").append(E("section", { id: `v-${n}`, c: "vw", h: n !== TABS[0] })); }
[["rb", "rf", "aria-label"], ["bx", "cl", "aria-label"], ["bd", "dt"], ["ba", "ry"]].forEach(([i, k, a]) => R($(i), k, a));

const syncMaps = new WeakMap<Element, Map<string, readonly [Element, ...Element[]]>>();
function sync<T, R extends readonly [Element, ...Element[]]>(p: Element, items: readonly T[], keyFn: (it: T, i: number) => string | number, mk: (it: T) => R, upd: (r: R, it: T) => void): void {
  let map = syncMaps.get(p) as Map<string, R> | undefined;
  if (!map) { map = new Map(); syncMaps.set(p, map); }
  const seen = new Set<string>();
  items.forEach((it, i) => { const k = ST(keyFn(it, i)); seen.add(k); let r = map.get(k); if (!r) map.set(k, r = mk(it)); upd(r, it); if (p.children[i] !== r[0]) p.insertBefore(r[0], p.children[i] || null); });
  for (const [k, r] of map) if (!seen.has(k)) { r[0].remove(); map.delete(k); }
}

const accLbl = (a?: string): string => a === "NONE" ? "CPU" : a === "V5E1" ? "TPU v5e" : a === "V6E1" ? "TPU v6e" : (a || "");
const syncSel = (sel: HTMLSelectElement, items: readonly Session[], z?: string): void => {
  const prev = sel.value, opts: readonly { display_name?: string; hardware?: string }[] = z ? [{}, ...items] : items.length ? items : [{}];
  sync(sel, opts, (x) => x.display_name || "_", () => [OP()] as const, (r, { display_name: d = "", hardware: h }) => { r[0].value = d; T(r[0], d ? `${d} (${accLbl(h)})` : (z || "—")); });
  if (prev && opts.some((x) => x.display_name === prev)) sel.value = prev;
};
const mkEmp = (k: string): HTMLDivElement => DV("cd ep", E("span", { c: "em", "aria-hidden": "true" }, Q(".em svg")!.cloneNode(true), SP("dt")), R(SP(), k));
const mkAcc = (name: string, list: readonly string[] = ACCS): HTMLSelectElement => SL({ name }, ...list.map((a) => OP(a, accLbl(a))));
const mkFlds = (defs: readonly FieldDef[]): [HTMLDivElement, HTMLSelectElement | HTMLInputElement][] => defs.map(([lk, name, kind, opts, ph]) => {
  const ctl = kind === "acc" ? mkAcc(name) : kind === "sel" ? SL({ name }) : IN(OA({ name, type: kind === "chk" ? CK : kind === NM ? NM : "text" }, opts), ph), lb = LB(lk);
  return [kind === "chk" ? DV("rl ck", ctl, lb) : DV("fi", lb, ctl), ctl];
});
const mkFrm = (flds: readonly [HTMLDivElement, unknown][], cb?: () => void): DirtyForm => { const f: DirtyForm = E("form", "fg", ...flds.map((x) => x[0])); f.oninput = () => (f._d = 1); f.onchange = () => { f._d = 1; cb?.(); }; return f; };

let snap: Snap | null = null, snapAt = Date.now(), lastSync = 0, snapFromCache = false, tab: TabId = TABS[0], lang: Locale = ZH, pickedLang = false, chartIdx = 2;
let auto = true, busy = false, pollT: ReturnType<typeof setTimeout> | null = null, delayMs = 4e3, ewmaRttMs = 0, pollSeq = 0, failCount = 0, down = false, connOk = false, epoch = 0, snapEp = -1;
let armed: string | null = null, swPrev: { id: string; data: SwitchPreviewData } | null = null, hfModels: HfModel[] = [], hfFit: HfFitResult | null = null, selPlg = LL, builtPlg: string | null = null, selRepo = "unsloth/Qwen3-32B-GGUF";
let bnr: BannerState | null = null, bnrRaw = false, dimErr: string | null = null, fbT: ReturnType<typeof setTimeout> | undefined;

const bridge = App({ name: "colab-mcp", version: "0.2.3" }), hst = (): boolean => bridge.hasServerTools(), emb = (): boolean => !!(window.parent && window.parent !== window), actA = (): Account | null | undefined => snap?.active_account;
const openLink = (url?: string): void => { if (!url) return; if (bridge.isConnected() && bridge.getHostCapabilities()?.openLinks) void bridge.request("ui/open-link", { url }, { timeoutMs: 1e4 }).catch(() => {}); else window.open(url, "_blank", "noopener,noreferrer"); };
const t = (k: string, v?: Record<string, unknown> | null): string => tr(lang, k, v), dur = (s: number | null | undefined): string => fmtDur(s, lang), stL = (k: string): string => { const m = k === "user" ? "user_stop" : k, v = t(`s.${m}`); return v === `s.${m}` ? k : v; };
const loc = (m?: LocalizedText | null): string => m?.[lang] || m?.en || Object.values(m || {})[0] || "";
const accName = (a?: Account | null): string => !a ? "" : a.label === "Local ADC" ? t("gd") : (a.label || "");
const isPs = (s?: Snap | null): boolean => !!((s?.guard_paused || s?.host_guard_paused) && !s?.syncing);
const optN = (v: unknown): number | null => v !== "" && v != null && NF(+v) ? +v : null, posN = (v: unknown): number | null => { const n = optN(v); return n != null && n > 0 ? n : null; };
const chkN = (f: Record<string, unknown>, ks: readonly string[]): boolean => ks.every((k) => { const s = `${f[k] ?? ""}`.trim(); return !s || (+s >= 0 && NF(+s)); });
const P = <O extends Record<string, unknown>>(o: O, ds?: DOMStringMap): O & Record<string, unknown> => OA(o, ds?.ep && { endpoint: ds.ep }, actA()?.id && { account_id: actA()!.id }, snap?.backend_mode && { expected_backend_mode: snap.backend_mode });
const gd = (g: GuardSettings): (string | number)[] => [g.max_runtime_secs ? g.max_runtime_secs / 3600 : "", g.idle_timeout_secs ? MR(g.idle_timeout_secs / 60) : "", g.max_spend_cu ?? "", g.min_balance_cu ?? ""];
const flash = (m: string): void => { const n = $("fb"); T(n, m); H(n, 0); CT(fbT); fbT = setTimeout(() => H(n, 1), 2600); };

function curBnr(): BannerState | undefined {
  if (bnr) return bnr;
  const s = snap, se = s?.settings_error, le = s?.last_error;
  if (se && dimErr !== `s:${se}`) return { k: WN, m: t("fb"), raw: se, key: `s:${se}` };
  if (le && s) { const key = `e:${s.last_error_kind || ""}:${le}`; if (dimErr !== key) return { k: ER, m: fmtErr(lang, s.last_error_kind, s.last_error_localized), raw: le, key }; }
  if (down) return { k: WN, m: auto && emb() ? t("lo", { s: dur(MR(delayMs / 1e3)) }) : t("of"), r: 1, key: "down" };
  return undefined;
}
function syncBnr(): void {
  const b = $("bn"), p = curBnr();
  if (!p) { bnrRaw = false; H(b, 1); return; }
  K(b, p.k); T($("bt"), p.m); H($("bd"), !p.raw); T($("br"), p.raw || ""); H($("br"), !p.raw || !bnrRaw); H($("ba"), !p.r); H(b, 0);
}
const showBnr = (m: string, k = WN, r = 0, raw: string | null = null): void => { dimErr = null; bnrRaw = false; bnr = { m, k, r, raw, key: `u:${m}` }; syncBnr(); };
const showErr = (k: string, raw: string, locO?: LocalizedText | null, r = 0): void => showBnr(fmtErr(lang, k, locO), ER, r, raw || null);

function parseR(r: McpToolResult | null | undefined): Partial<Snap> & { error?: string; kind?: string; localized?: LocalizedText; snapshot?: Snap; url?: string; fits?: HfFitItem[]; repo?: string; models?: HfModel[] } & SwitchPreviewData | undefined {
  if (r?.structuredContent) return r.structuredContent as unknown as Partial<Snap> & SwitchPreviewData;
  try {
    const txt = r?.content?.find((c) => c.type === "text")?.text;
    return txt ? (JSON.parse(txt) as Partial<Snap> & SwitchPreviewData) : undefined;
  } catch {
    return undefined;
  }
}
const nowS = (): number => (Date.now() / 1e3) | 0, elap = (): number => MX(0, ((Date.now() - snapAt) / 1e3) | 0);
const liveUp = (x: Session): number | null => x.started_at_epoch != null ? MX(0, nowS() - x.started_at_epoch) : (x.uptime_secs != null ? x.uptime_secs + elap() : null);
const liveRem = (w?: Watchdog | null): number | null => !w?.armed ? null : (w.deadline_epoch != null ? MX(0, w.deadline_epoch - nowS()) : (w.remaining_secs != null ? MX(0, w.remaining_secs - elap()) : null));
const extLbl = (hw?: string): string => hw ? t("eh", { hw: accLbl(hw) }) : t("eu"), isUnk = (v?: string | null): boolean => !v || /^\[\x3f\]$/.test(v);
const sName = ({ is_orphan: o, external_name: e, display_name: d, hardware: h }: Session): string => o ? (e || extLbl(h)) : (isUnk(d) ? extLbl(h) : d!);
const upTxt = (x: Session): string => { const d = dur(liveUp(x)); return `${t("up")} ${x.start_source === "first_seen" ? (lang === ZH ? `${d}以上` : `${d}+`) : d}`; };
const spTxt = (x: Session): string => `${t("sp")} ${x.spend_cu != null ? `${x.spend_estimated ? "≈ " : ""}${fmtCu(x.spend_cu)} CU` : "—"}`;
const rateTxt = (a: string, hi?: boolean): string => { const r = snap?.hardware_rates?.[a]; if (!r) return ""; const u = hi && !NO_HM.has(a) && r.high_mem_cu_per_hour != null; return `≈ ${fmtCu(u ? r.high_mem_cu_per_hour : r.cu_per_hour)} CU/h (${t((u ? r.high_mem_measured : r.measured) ? "me" : "es")})`; };
const L0: Record<string, string> = { runtime: "r", spend: "s", balance: "b" };
const fmtWarn = ({ kind: k, limit: l, current: c, threshold: h }: GuardConcern): string => { const p = (l && L0[l]) || "b"; return k === "usage_unknown" ? t("wu", { l: t(`l${p}`) }) : t(`w${p}`, p === "r" ? { c: dur(c), t: dur(h) } : { c: fmtCu(c), t: fmtCu(h) }); };
const fmtTick = (sec: number, isL: boolean): string => { const d = new Date(sec * 1e3), p = (n: number): string => ST(n).padStart(2, "0"); return isL ? `${p(d.getMonth() + 1)}-${p(d.getDate())}` : `${p(d.getHours())}:${p(d.getMinutes())}`; };

function mkPsCard(): [HTMLDivElement, (ps: PendingStop[], ss?: Session[]) => void] {
  const ls = DV(LS), box = CK0(ER, R(DV(TT0), "sf"), ls);
  return [box, (ps, ss = []) => {
    H(box, !ps.length); if (!ps.length) return;
    sync(ls, ps, (p) => p.endpoint || p.session || "", () => { const a = SP(MN0), b = SP(MU), c = BS("d a", "rs"); return [IT(RW(RL(a, b), c)), a, b, c] as const; }, (r, p) => {
      const ep = p.endpoint || "", orp = p.is_orphan || isUnk(p.session), lv = ss.find((x) => x.endpoint === ep);
      T(r[1], orp ? (lv?.hardware ? t("ep", { hw: accLbl(lv.hardware) }) : t("eu")) : p.session);
      T(r[2], p.status?.last_error ? t("e.cli_error") : ep); H(r[3], !ep); PE(r[3], orp ? "" : p.session, ep); L(r[3], "ry");
    });
  }];
}

const ov = (() => {
  const cs = ["bl", "br", "rt", "cg"].map((k) => { const v = DV("hv"), s = DV("hm el"); return [DV("hc", R(DV("kl"), k), v, s), v, s] as const; });
  const [bN, rN, rtN, gT] = S3(4), [rtD, gD] = S3(2, "sd"), bU = E("small", { t: "CU" }), bS = R(PL(WN, "", 1), "sl"), rU = E("small", { t: "CU/h" });
  cs[0][1].append(bN, bU, bS); cs[1][1].append(rN, rU); cs[2][1].append(rtD, rtN); cs[3][1].className = "hvs"; cs[3][1].append(gD, gT);
  const suC = CK0(WN, RW(RL(R(DV(TT0), "st0"), R(DV(MU), "sh0")), BS("c", "gt", "t.settings", { "data-v": "settings" }))), [psEl, psR] = mkPsCard();
  const spD = SP(MT), rBtns = RNG.map(([, k], i) => R(E("button", { "data-a": "crg", "data-v": i, [AP]: ST(i === chartIdx) }), k));
  const ys = S3(3), xs = S3(3), yCol = DV("cy", ...ys), area = S("path", { fill: "var(--as)", stroke: "none" }), poly = S("polyline", { stroke: "var(--ac)", "stroke-width": "2", "vector-effect": "non-scaling-stroke" }), tip = S("title");
  const svg = S("svg", { class: "sp", viewBox: "0 0 320 68", preserveAspectRatio: "none" }, S("path", { d: "M0 6H320M0 34H320M0 62H320", stroke: "var(--l2)", "vector-effect": "non-scaling-stroke" }), area, poly, tip);
  const spEmp = R(DH("ce"), "ce"), xRow = DV("cx", ...xs), spC = DH("cd", RW(RL(R(SP("kl"), "bt"), spD), DV("seg", ...rBtns)), DV("cg", yCol, DV("ov", svg, spEmp), xRow));
  const wL = DV(LS), wC = CK0(WN, R(DV(TT0), "gw"), wL), oL = DV(LS), oC = CK0(WN, R(DV(TT0), "ot"), R(DV(MU), "od"), oL);
  $("v-overview").append(DV("cd hero", DV("hg", ...cs.map((c) => c[0]))), suC, psEl, spC, wC, oC, CD(R(DV(TT0), "qa"), RL(...[["b p", "sessions", "ng"], ["b c", "deploy", "dg"], ["b", "tunnels", "fw"]].map(([c, v, k]) => B(c, "gt", k, { "data-v": v })))));

  return (s: Snap): void => {
    const { usage: u, sessions: ss = [], pending_stops: pss = [], syncing: sy, backend_mode: bm, balance_history: bh = [] } = s, env = s.env || {};
    const orp = ss.filter((x) => x.is_orphan), wrn = ss.filter((x) => x.guard_decision?.decision === "warn"), ps = isPs(s), arm = ss.filter((x) => x.watchdog?.armed).length, nk = t(sy ? "ld" : "uk");
    T(bN, fmtCu(u?.projected_balance_cu)); H(bU, !u); H(bS, !u?.stale); T(cs[0][2], u ? accName(actA()) : nk);
    T(rN, fmtCu(u?.rate_cu_per_hour)); H(rU, !u); T(cs[1][2], u ? (u.eta_secs != null ? `${t("et")} ${dur(u.eta_secs)}` : t("bz")) : nk);
    K(rtD, ss.length ? (orp.length ? WN : OK) : T3); T(rtN, ss.length); T(cs[2][2], orp.length ? `${orp.length} ${t("ex")}` : t("wn", { n: arm }));
    K(gD, sy ? T3 : ps ? WN : wrn.length || pss.length ? ER : arm ? OK : T3);
    L(gT, sy ? "ld" : ps ? "gp" : wrn.length || pss.length ? "gw" : arm ? "ga" : "gi"); T(cs[3][2], ss.length ? t("vn", { a: arm, n: ss.length }) : t("za"));
    H(suC, !(bm !== "mock" && s.env && (!env.colab_found || (actA()?.auth_mode !== "oauth2" && !env.adc_file_present))));
    psR(pss, ss);

    const ch = projectChart(bh, RNG[chartIdx][0], nowS());
    H(spC, !ch.visible);
    if (ch.visible) {
      rBtns.forEach((b, i) => A(b, AP, ST(i === chartIdx)));
      [svg, yCol, xRow].forEach((n) => H(n, ch.sparse)); H(spEmp, !ch.sparse);
      if (ch.sparse) T(spD, "CU");
      else {
        const { delta, maxV, minV, coords, tMin, tMax } = ch;
        T(spD, `${delta > 0.005 ? "+" : ""}${fmtCu(delta)} CU`);
        [`${fmtCu(maxV)} CU`, fmtCu((maxV + minV) / 2), `${fmtCu(minV)} CU`].forEach((v, i) => T(ys[i], v));
        const pts = coords.map((c) => `${c.x},${c.y}`).join(" "), first = coords[0], last = coords[coords.length - 1];
        A(poly, "points", pts); A(area, "d", `M${first.x},62 L${pts} L${last.x},62 Z`); T(tip, `${fmtTick(last.at, false)} · ${fmtCu(last.v)} CU`);
        const isL = tMax - tMin > 129600;
        [fmtTick(tMin, isL), fmtTick(MR((tMin + tMax) / 2), isL), t("cn")].forEach((v, i) => T(xs[i], v));
      }
    }
    H(wC, !wrn.length);
    if (wrn.length) sync(wL, wrn, (w) => w.endpoint || w.display_name || "", () => { const a = SP(MN0), b = SP(MU); return [RW(a, b), a, b] as const; }, (r, w) => { T(r[1], sName(w)); T(r[2], (w.guard_decision?.concerns || []).map(fmtWarn).join(" · ")); });
    H(oC, !orp.length);
    if (orp.length) sync(oL, orp, (o) => o.endpoint || "", () => { const a = PL(WN, ""), b = SP(MN0), c = SP(MT), d = BS("d", "rp"); return [IT(RW(RL(a, b, c), d)), b, c, d, a] as const; }, (r, o) => {
      const { endpoint: ep = "", external_name: en, hardware: hw } = o, a = armed === `rp:${ep}`;
      T(r[4], en ? "CLI" : t("bo")); T(r[1], en ? `${en} · ${accLbl(hw)}` : extLbl(hw));
      T(r[2], `${upTxt(o)} · ${spTxt(o)}`); D(r[3], "ep", ep); TG(r[3], "a", a); L(r[3], a ? "cs" : "stopBill");
    });
  };
})();

const sv = (() => {
  const flds = mkFlds([GF[0], ["mv", "min_vram_mib", NM, { min: 0, step: 1e3, placeholder: "70000" }], ...GF.slice(1), ["sn", "name", "txt", null, "an"], ["hm", "high_mem", "chk"]]);
  const updHint = (): void => { const a = flds[0][1].value, nh = NO_HM.has(a), hm = flds[7][1] as HTMLInputElement; hm.disabled = nh; if (nh) hm.checked = false; const rt = rateTxt(a, hm.checked); T(hint, `${rt ? `${rt} · ` : ""}${t(a === "A100" ? "p1" : "pd")}`); };
  const form = mkFrm(flds, updHint), hint = SP("mt"), [psEl, psR] = mkPsCard(), jL = DH(LS), emp = mkEmp("nos"), sL = DV(LS), eL = DV(LS), eC = DH("cd", R(DV(TT0), "ed"), eL);
  $("v-sessions").append(CD(R(DV(TT0), "ns"), form, RW(hint, B("b p", "pv", "sv"))), psEl, jL, emp, sL, eC);

  return {
    form,
    r(s: Snap): void {
      const { settings, sessions: ss = [], provision_jobs: pjs = [], ended_sessions: ended = [], pending_stops: pss = [] } = s, def = settings?.defaults || {}, g = def.guard || {}, jobs = pjs.filter((j) => !j.done || j.error);
      if (!form._d) { [def.accelerator || "G4", def.min_vram_mib || "", ...gd(g)].forEach((v, i) => (flds[i][1].value = ST(v))); (flds[7][1] as HTMLInputElement).checked = !!(def.high_mem && !NO_HM.has(def.accelerator || "")); }
      updHint(); psR(pss, ss); H(jL, !jobs.length);
      sync(jL, jobs, (j) => j.job_id, () => { const a = PL(AC, ""), b = SP(MN0), c = DV(MU), d = BS("d", "cv"); return [CD(RW(RL(a, b), d), c), b, c, d, a] as const; }, (r, j) => {
        PT(r[4], stL(j.error ? "failed" : "starting"), j.error ? ER : AC); T(r[1], `${j.name} (${accLbl(j.accelerator)})`); T(r[2], j.last_status || (j.error ? t("e.cli_error") : "")); D(r[3], "id", j.job_id); L(r[3], "cc"); H(r[3], j.done);
      });
      HE(emp, sL, ss.length);
      sync(sL, ss, (x) => x.endpoint || x.display_name || "", () => {
        const nm = SP("tt mn"), [hw, wd] = S3(2, PL0), stp = BS("d", ""), dsc = DV(MU), [up, rt, sp] = S3(3), bs = ["we", "", "qf", "ou", "rk"].map((a) => BS("", a)), acts = RL(...bs);
        return [CD(RW(RL(nm, hw, wd), stp), dsc, DV("rw mt mn", up, rt, sp), acts), nm, hw, wd, stp, dsc, up, rt, sp, acts, bs[0], bs[1], bs[2], bs[3], bs[4]] as const;
      }, (r, x) => {
        const { display_name: dn = "", endpoint: ep = "", hardware: hw, watchdog: wd, is_orphan: orp, shape, gpu, vram_mib: vm, rate_cu_per_hour: rph, rate_measured: rm0, pending_stop: ps0, state: st0, external_name: en } = x;
        const k = ep || dn, a = armed === `${orp ? "rp" : "st"}:${k}`, ps = ps0 || st0 === "pending_stop";
        A(r[0], "data-k", orp || ps || !wd?.armed ? WN : null); T(r[1], sName(x)); PT(r[2], accLbl(hw), ps || orp ? WN : OK);
        if (ps) PK(r[3], "ps", WN); else if (orp) PK(r[3], en ? "ec" : "bo", WN); else { const rm = liveRem(wd); PT(r[3], wd?.armed ? (rm != null ? `${t("ar")} · ${dur(rm)}` : t("ar")) : t("un"), wd?.armed ? OK : WN); }
        D(r[4], "a", orp ? "rp" : "st"); PE(r[4], dn, ep); TG(r[4], "a", a); L(r[4], a ? "cs" : "stopBill");
        const shp = /high/i.test(shape || "") ? t("hm") : /standard/i.test(shape || "") ? t("std") : shape;
        T(r[5], [accLbl(hw), gpu, vm ? `${MR(vm / 1024)} GB` : null, shp, ep].filter(Boolean).join(" · "));
        T(r[6], upTxt(x)); T(r[7], rph != null ? `${rm0 ? "" : "≈ "}${fmtCu(rph)} CU/h` : t("uk")); T(r[8], spTxt(x)); H(r[9], orp);
        if (!orp) {
          const wA = armed === `wd:${k}`, rkA = armed === `rk:${k}`;
          D(r[11], "a", wd?.armed ? "wc" : "wa"); r[11].className = `${BS0}${wd?.armed && wA ? " d a" : ""}`; r[14].className = `${BS0}${rkA ? " d a" : ""}`;
          ["e2", wd?.armed ? (wA ? "cfw" : "cw") : "aw", "fw", "oc", rkA ? "cfrk" : "rk"].forEach((lk, i) => { const btn = r[(10 + i) as 10 | 11 | 12 | 13 | 14]; PE(btn, dn, ep); L(btn, lk); });
        }
      });
      H(eC, !ended.length);
      if (ended.length) sync(eL, ended.slice(0, 10), (e, i) => `${e.name}:${e.ended_at_epoch}:${i}`, () => { const a = SP(MN0), b = SP(PL0), c = SP(MT); return [IT(RW(RL(a, b), c)), a, b, c] as const; }, (r, { name: n, hardware: h, stop_reason: sr, duration_secs: ds0, spend_cu: sc }) => {
        T(r[1], isUnk(n) ? extLbl(h) : `${n} (${accLbl(h)})`); PT(r[2], stL(sr || "stopped"), T3); T(r[3], `${dur(ds0)} · ${sc != null ? `${fmtCu(sc)} CU` : "—"}`);
      });
    },
  };
})();

const tv = (() => {
  const flds = mkFlds([["se", "session", "sel", null], ["lp", "local_port", NM, { min: 1024, max: 65535, v: 8000 }], ["rp", "remote_port", NM, { min: 1, max: 65535, v: 8000 }], ["hp", "health_path", "txt", { v: "/health", placeholder: "/health" }]]);
  const form = E("form", "fg", ...flds.map((f) => f[0])), sel = flds[0][1] as HTMLSelectElement, op = B("b p", "ot", "op"), emp = mkEmp("not"), list = DV(LS);
  $("v-tunnels").append(DV({ c: "bnr", "data-k": WN }, RW(R(SP(), "tn"))), CD(R(DV(TT0), "fw"), form, RW(SP(), op)), emp, list);
  return {
    sel, form,
    r(s: Snap): void {
      const nm = (s.sessions || []).filter((x) => !x.is_orphan), ts = s.tunnels || [];
      syncSel(sel, nm); sel.disabled = op.disabled = !nm.length; HE(emp, list, ts.length);
      sync(list, ts, (u) => u.id, () => {
        const u = SP("mn tt"), tg = SP(MT), st = SP(PL0), hl = PL("", "", 1), bs = [["c", "ol"], ["", "cp"], ["d", "ct"], ["", "dt"]].map(([c, a]) => BS(c, a)), er = DH(MU);
        return [CD(RW(RL(u, tg, st, hl), RL(...bs)), er), u, tg, st, hl, bs[0], bs[1], bs[2], bs[3], er] as const;
      }, (r, { id, local_url: url, spec, state: stO, healthy: hl }) => {
        const st = stO?.state || "unknown", act = !isOff(st);
        T(r[1], url); T(r[2], `→ ${spec.session}:${spec.remote_port}`); PT(r[3], stL(st), st === "up" ? OK : st === "failed" ? ER : st === "stopped" ? T3 : WN);
        H(r[4], hl == null); if (hl != null) PK(r[4], hl ? "ho" : "he", hl ? OK : WN);
        D(r[5], "url", url); D(r[6], "v", url); D(r[7], "id", id); D(r[8], "id", id);
        H(r[5], !act); H(r[7], !act); H(r[8], act); ["ou", "cu", "cl", "cl"].forEach((k, i) => L(r[(5 + i) as 5 | 6 | 7 | 8], k));
        H(r[9], !stO?.error); if (stO?.error) T(r[9], fmtErr(lang, stO.error.ui_kind, stO.error.localized));
      });
    },
  };
})();

const dv = (() => {
  const pN = SP(TT0), pI = PL(IN0, ""), pS = SL({ style: "width:auto" }), pD = DV(MU);
  pS.onchange = () => { selPlg = pS.value; if (snap) dv.r(snap); };
  const hfQ = IN({ c: "f1", v: selRepo, placeholder: selRepo }), hfM = DH(LS);
  const fitR = SP(MN0), fitP = RL(), fitB = DH("it", RW(fitR, fitP)), hfW = DH(LS, DV("hr"), RW(hfQ, BS("c", "hs", "sr"), BS("", "hi", "in")), hfM, fitB), fD = E("form", "fg");
  const dRt = SP(MT), tokIn = IN({ type: PW, c: "f1" }, "ht"), tokP = R(PL(OK, "", 1), "hs"), emp = mkEmp("nor"), rL = DV(LS), srcIn = IN("f1", "pl");
  $("v-deploy").append(CD(RW(RL(pN, pI), pS), pD, hfW, fD, RW(DV("rl f1", tokIn, BS("", "tk", "st"), tokP, dRt), B("b p", "dp", "dp"))), emp, rL, CD(R(DV(TT0), "ip"), RW(srcIn, BS("c", "ip", "ib"))));
  let depFlds: [HTMLLabelElement, string | PluginInput, HTMLSelectElement | HTMLInputElement | null][] = [], tSel: HTMLSelectElement | null = null, aSel: HTMLSelectElement | null = null, sofCk: HTMLInputElement | null = null;
  const updDep = (): void => { const ex = !!tSel?.value; if (aSel) aSel.disabled = ex; if (sofCk) sofCk.disabled = ex; T(dRt, ex ? "" : rateTxt(aSel?.value || "G4")); };
  function buildForm(mf: PluginManifest): void {
    fD.replaceChildren(); depFlds = []; tSel = SL({ name: "__session" }); const [tl, al, sl] = [LB(), LB(), LB()];
    aSel = mkAcc(KA, mf.id === LL ? GPU_ACCS : ACCS); aSel.value = mf.provision?.accelerator || "G4";
    sofCk = IN({ name: "__sof", type: CK, ck: true }); tSel.onchange = aSel.onchange = updDep;
    fD.append(DV("fi", tl, tSel), DV("fi", al, aSel), DV("rl ck", sofCk, sl)); depFlds.push([tl, "ts", null], [al, "ac", null], [sl, "so", null]);
    for (const inp of mf.inputs || []) {
      const { id, type: ty, options: opts, required: req } = inp, v = id === "repo" ? selRepo : (inp.default ?? "");
      const ctl = ty === "select" ? SL({ name: id }, ...(opts || []).map((o) => OP(o, o))) : ty === "bool" ? IN({ name: id, type: CK, ck: !!v }) : IN({ name: id, type: ty === NM ? NM : ty === "secret" ? PW : "text", v: ST(v), required: !!req });
      if (ty === "select") ctl.value = ST(v); const lb = LB(); fD.append(ty === "bool" ? DV("rl ck", ctl, lb) : DV("fi", lb, ctl)); depFlds.push([lb, inp, ctl]);
    }
  }
  return {
    hfQ, fD, tokIn, srcIn,
    r(s: Snap): void {
      const { plugins: plgs = [], plugin_runs: runs = [], sessions: ss = [] } = s, cur = plgs.find((p) => p.manifest.id === selPlg) || plgs[0], mf = cur?.manifest;
      if (mf) selPlg = mf.id; sync(pS, plgs, (p) => p.manifest.id, () => [OP()] as const, (r, { manifest: m }) => { r[0].value = m.id; T(r[0], loc(m.name)); });
      if (mf) pS.value = mf.id; T(pN, mf ? loc(mf.name) : t("t.deploy")); T(pI, mf ? mf.id : ""); H(pI, !mf); T(pD, mf ? loc(mf.description) : "");
      H(hfW, mf?.id !== LL); H(hfM, !hfModels.length);
      if (hfModels.length) sync(hfM, hfModels.slice(0, 5), (m) => m.id, () => { const a = SP("mn el"), b = SP(MT), c = BS("", "pr"); return [IT(RW(a, RL(b, c))), a, b, c] as const; }, (r, { id, downloads: dl }) => { T(r[1], id); T(r[2], `↓${dl.toLocaleString()}`); D(r[3], "v", id); L(r[3], "in"); });
      H(fitB, !hfFit); if (hfFit) { T(fitR, hfFit.repo); sync(fitP, hfFit.fits || [], (f) => f.vram_gib, () => [SP(PL0)] as const, (r, { vram_gib: g, fits, quant: q, kv }) => PT(r[0], `${g} GB → ${fits ? `${q} (${kv})` : t("om")}`, fits ? OK : ER)); }
      if (mf && builtPlg !== mf.id) { builtPlg = mf.id; buildForm(mf); }
      if (tSel) syncSel(tSel, ss.filter((x) => !x.is_orphan), t("ns"));
      for (const [lb, sp, ctl] of depFlds) { T(lb, typeof sp === "string" ? t(sp) : (loc(sp.label) || sp.id)); if (ctl && typeof sp !== "string" && sp.help) A(ctl, "title", loc(sp.help)); }
      updDep(); H(tokP, !s.hf_token_set); HE(emp, rL, runs.length);
      sync(rL, runs, (r) => r.run_id, () => {
        const id = SP("tt mn"), se = SP(MT), st = SP(PL0), cn = BS("d", "cr"), sb = BS("d", "st"), bar = E("i"), mg = DV(MU), eps = DH(LS), lg = E("pre", { c: "lg wb", h: 1 });
        return [CD(RW(RL(id, se, st), RL(sb, cn)), DV("pg", bar), mg, eps, lg), id, se, st, cn, bar, mg, eps, lg, sb] as const;
      }, (r, { run_id: rid, state: st = "", stage, progress: prg, session: ses, endpoint: ep, plugin_id: pid, message: msg, endpoints: eps, log_tail: lgs, created }) => {
        const pct = MX(0, MN(100, MR((prg ?? (st === "ready" ? 1 : 0.15)) * 100))), liveS = created && ep ? ss.find((z) => z.endpoint === ep) : null;
        const stgLbl = stage === "tunnel" ? t("tu") : stage ? loc(plgs.find((p) => p.manifest.id === pid)?.manifest.stages?.find((z) => z.id === stage)?.label) || stage : "";
        T(r[1], pid); T(r[2], ses); PT(r[3], `${stL(st)}${stgLbl ? ` · ${stgLbl}` : ""}`, st === "ready" ? OK : st === "failed" ? ER : AC);
        D(r[4], "id", rid); L(r[4], st === "ready" ? "stp" : "cc"); H(r[4], !["running", "ready", "cancel_failed"].includes(st));
        const showSb = !!(liveS && (st === "failed" || st === "cancel_failed")); H(r[9], !showSb);
        if (showSb) { PE(r[9], liveS.display_name, liveS.endpoint); TG(r[9], "a", armed === `st:${liveS.endpoint}`); L(r[9], "cs"); }
        r[5].style.width = `${pct}%`; T(r[6], msg);
        const live = (eps || []).filter((e): e is Required<Pick<PluginEndpoint, "local_url">> & PluginEndpoint => Boolean(e.local_url)); H(r[7], !live.length);
        sync(r[7], live, (e) => `${e.local_url}${e.path || ""}`, () => { const a = SP("mn el"), b = BS("c", "ol"), c = BS("", "cp"), d = BS("", "cp"); return [IT(RW(a, RL(b, c, d))), a, b, c, d] as const; }, (er, { local_url: lu, path: pt = "", kind }) => {
          const f = `${lu}${pt === "/" ? "" : pt}`, cUrl = kind === "openai" && pt === "/v1" ? `${lu}/v1/models` : f;
          T(er[1], f); D(er[2], "url", lu); L(er[2], "ou"); D(er[3], "v", f); L(er[3], "cu"); D(er[4], "v", `curl -sS '${cUrl.replace(/'/g, "'\\''")}'`); L(er[4], "cr");
        });
        const logs = (lgs || []).join("\n"); H(r[8], !logs); T(r[8], logs);
      });
    },
  };
})();

const setv = (() => {
  const bmP = SP(PL0), bD = B("b", "bm", "sd", { "data-v": "mock" }), bR = B("b", "bm", "rl", { "data-v": "real" });
  const aL = DV(LS), swT = DV(TT0), swB = DV(LS), swC = DH("it", swT, swB, RL(BS("p", "swc", "cfs"), BS("", "swx", "cc")));
  const aId = IN(null, "ai"), aLb = IN(null, "al"), aEm = IN({ c: "ae", placeholder: "user@example.com" });
  const flds = mkFlds([...GF, ["mc", KMC, NM, { min: 1, max: 8 }], ["p2", KPA, NM, { min: 10, max: 600, step: 5 }], ["p3", KPI, NM, { min: 30, max: 3600, step: 30 }]]), form = mkFrm(flds);
  const sP = SP("mt mn el"), sCp = BS("", "cp", "cu"), lAdc = SP(), [d1, d2, d3] = S3(3, PL0), d4 = SP("mt mn el");
  $("v-settings").append(
    CD(RW(R(SP(TT0), "bm"), bmP), RL(bD, bR)),
    CD(R(DV(TT0), "acc"), R(DV(MU), "sww"), aL, swC, DV("af", aId, aLb, aEm, BS("c", "ad", "aa"))),
    CD(R(DV(TT0), "df"), form, RW(RL(R(SP("mt"), "fl"), sP, sCp), B("b p", "sv", "ss"))),
    CD(R(DV(TT0), "dc"), DV("g2 mu", RL("Colab CLI:", d1), RL(lAdc, d2), RL(R(SP(), "sk"), d3), RL(R(SP(), "pt"), d4)))
  );
  const jn = <T>(a: readonly T[] | undefined, f: (x: T) => string): string => (a || []).map(f).join(", ") || "0";
  return {
    form, aId, aLb, aEm,
    r(s: Snap): void {
      const { settings, accounts: accs = [], backend_mode: bm, settings_path: spath = "" } = s, def = settings?.defaults || {}, g = def.guard || {}, env = s.env || {}, mock = bm === "mock";
      PK(bmP, mock ? "dm" : "rl", mock ? WN : OK); bD.className = `b${mock ? " c" : ""}`; bR.className = `b${!mock ? " c" : ""}`;
      sync(aL, accs, (a) => a.id, () => { const l = SP(TT0), m = SP(PL0), em = SP(MT), ac = PL(OK, ""), sw = BS("", "sw"), rm = BS("d", "rm", null, { t: "×" }); return [IT(RW(RL(l, m, em, ac), RL(sw, rm))), l, m, em, ac, sw, rm] as const; }, (r, a) => {
        const { id, auth_mode: am, email } = a, act = id === actA()?.id, rmA = armed === `rm:${id}`;
        T(r[1], accName(a)); PT(r[2], am === "adc" ? t("gs") : "OAuth2", am === "adc" ? IN0 : AC);
        T(r[3], email || ""); H(r[3], !email); L(r[4], "act"); H(r[4], !act); D(r[5], "id", id); H(r[5], act); L(r[5], "sw");
        D(r[6], "id", id); H(r[6], id === "adc-default"); TG(r[6], "a", rmA); T(r[6], rmA ? t("rc") : "×");
      });
      H(swC, !swPrev);
      if (swPrev) {
        const { closing_tunnels: ct, open_tunnels: ot, watched_sessions: ws, unwatched_sessions: uw = [], orphan_sessions: or = [] } = swPrev.data || {}, rows: readonly [string, string, string][] = [
          [t("s1"), ST(ct?.length ?? ot ?? 0), T3],
          [t("s2"), jn(ws, (x) => `${x.name}${x.remaining_secs != null ? ` (${dur(x.remaining_secs)})` : ""}`), OK],
          [t("s3"), jn(uw, (x) => x.name), uw.length ? ER : T3],
          [t("s4"), jn(or, (x) => x.external_name || (x.hardware ? extLbl(x.hardware) : x.endpoint) || ""), or.length ? WN : T3],
        ];
        T(swT, `${t("si")} → ${swPrev.id}`);
        sync(swB, rows, (x) => x[0], () => { const a = SP(MU), b = SP(PL0); return [RW(a, b), a, b] as const; }, (r, x) => { T(r[1], x[0]); PT(r[2], x[1], x[2]); });
      }
      if (!form._d) [def.accelerator || "G4", ...gd(g), def[KMC] ?? 2, def[KPA] ?? 30, def[KPI] ?? 300].forEach((v, i) => (flds[i][1].value = ST(v)));
      T(sP, spath); D(sCp, "v", spath); H(sCp, !spath);
      T(lAdc, `${t("gs")}:`); PT(d1, env.colab_found ? (env.colab_version || t("gi")) : t("mg"), env.colab_found ? OK : ER);
      PK(d2, env.adc_file_present ? "pr" : "nf", env.adc_file_present ? OK : WN);
      PK(d3, env.ssh_key_ready ? "gi" : "ar0", env.ssh_key_ready ? OK : WN); T(d4, env.colab_path || "—");
    },
  };
})();

function renderHeader(): void {
  const s = snap, mock = s?.backend_mode === "mock", ss = s?.sessions || [], ps = isPs(s), aa = actA(), ab = $("ab");
  A(DC.documentElement, "lang", lang); D($("app"), "ps", ps ? "1" : "0");
  for (const [n, k, a] of I18) if (a) A(n, a, t(k)); else L(n, k);
  K($("d0"), s?.last_error ? ER : down || ps ? WN : ss.length ? OK : T3);
  PT($("m0"), mock ? t("dm") : (aa?.auth_mode === "oauth2" ? "OAuth2" : t("gs")), mock ? WN : IN0); T($("s0"), aa?.email || accName(aa) || "");
  TG(ab, "on", auto && !down); T($("al"), auto ? `${t("au")} · ${dur(MR(delayMs / 1e3))}` : t("pa"));
  if (lastSync) A(ab, "title", t("ud", { t: new Date(lastSync).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }) }));
  QA<HTMLElement>('[data-a="lg"]').forEach((b) => A(b, AP, ST(b.dataset.v === lang)));
  const actT = (s?.tunnels || []).filter((x) => !isOff(x.state?.state)).length;
  QA<HTMLElement>(TBQ).forEach((b) => { const k = b.dataset.tab || "", n = k === "sessions" ? ss.length : k === "tunnels" ? actT : 0; T(b, `${t(`t.${k}`)}${n ? ` (${n})` : ""}`); A(b, AS, ST(k === tab)); });
  syncBnr();
}

const VIEWS: Record<TabId, (s: Snap) => void> = { overview: ov, sessions: sv.r, tunnels: tv.r, deploy: dv.r, settings: setv.r };
const renderAll = (): void => { renderHeader(); for (const n of TABS) H($(`v-${n}`), tab !== n); if (snap) VIEWS[tab](snap); };
const getStorage = (): StorageLike | null => { try { return typeof localStorage !== "undefined" ? localStorage : null; } catch { return null; } };
const nowMs = (): number => typeof performance !== "undefined" ? performance.now() : Date.now();
const applySnap = (d: Snap | null | undefined, epk = epoch, fromCache = false): void => {
  if (!d || d.unchanged || epk < snapEp || (!snapFromCache && snap && epk === snapEp && d.rev != null && d.rev < snap.rev)) return;
  snap = d; snapEp = epk; snapFromCache = fromCache; snapAt = lastSync = Date.now();
  if (!fromCache) writeCachedSnap(getStorage(), d);
  if (d.settings?.locale && !pickedLang) lang = d.settings.locale === "en" ? "en" : ZH; renderAll();
};
const onSnap = (d: Partial<Snap> | null | undefined, epk: number): void => { if (d?.snapshot) applySnap(d.snapshot, epk); else if (d?.rev) applySnap(d as Snap, epk); };

function schedulePoll(): void {
  if (pollT !== null) { CT(pollT); pollT = null; }
  const a = (snap?.sessions?.length || 0) > 0 || (snap?.provision_jobs || []).some((j) => !j.done) || (snap?.plugin_runs || []).some((r) => r.state === "running") || !!snap?.syncing;
  delayMs = adaptPollDelay(a ? 4e3 : 12e3, ewmaRttMs, down, failCount);
  renderHeader();
  if (canSchedulePoll(connOk, down, auto, DC.hidden, emb())) pollT = setTimeout(() => { void poll(false); }, delayMs);
}
async function rpc(tool: string, args: Record<string, unknown>) {
  if (!emb()) throw OA(new Error(""), { code: BNT }) as RpcError;
  if (!hst()) { await bridge.connect(); epoch += 1; }
  if (!(connOk = hst())) throw OA(new Error(""), { code: BNT }) as RpcError;
  const res = await bridge.callServerTool<McpToolResult>({ name: tool, arguments: args }), d = parseR(res);
  if (res?.isError) throw OA(new Error(d?.error || res.content?.[0]?.text || ""), { code: d?.kind || CLE, loc: d?.localized }) as RpcError;
  return d;
}
const errCode = (e: RpcError, fb: string): string => e.code || (/^Timeout:/i.test(e.message || "") ? "bridge_timeout" : fb);
async function poll(force = false): Promise<void> {
  if ((!force && busy) || !emb() || (!force && DC.hidden)) return;
  const seq = ++pollSeq; busy = true; $("sw").classList.add("on"); const t0 = nowMs(), epk = epoch;
  try {
    const p = await rpc(CM("app_poll"), { since_rev: (force || snapFromCache || !!snap?.partial) ? null : (snap?.rev ?? null), force });
    if (seq !== pollSeq) return;
    ewmaRttMs = updateEwmaRtt(ewmaRttMs, nowMs() - t0, +(p?.server_ms ?? 0) || 0).ewmaRttMs;
    const wasDown = down; failCount = 0; down = false; lastSync = Date.now();
    if (p && !p.unchanged && p.rev != null) applySnap(p as Snap, epk);
    if (wasDown) { bnr = null; dimErr = null; syncBnr(); flash(t("reconnected")); }
  } catch (e) {
    if (seq !== pollSeq) return;
    const err = e as RpcError; failCount += 1; if (failCount >= 3) { down = true; dimErr = null; } else if (force) showErr(errCode(err, CLE), err.message || ST(err), err.loc, 1);
  } finally {
    if (seq === pollSeq) { busy = false; $("sw").classList.remove("on"); schedulePoll(); }
  }
}
async function act(tool: string, args: Record<string, unknown>, msgOk?: string) {
  const epk = epoch;
  try {
    const d = await rpc(tool, args);
    bnr = null; dimErr = null; syncBnr(); onSnap(d, epk); if (msgOk) flash(msgOk); return d;
  } catch (e) { const err = e as RpcError; showErr(errCode(err, "bridge_rpc"), err.message || ST(err), err.loc); return null; }
}
const A1 = (tl: string, ac: string, ex: Record<string, unknown>, m?: string) => act(tl, OA({ action: ac }, ex), m), okP = (n: number | null, lo: number): n is number => n != null && NI(n) && n >= lo && n <= 65535;
const formObj = (f: HTMLFormElement): Record<string, unknown> => { const o: Record<string, unknown> = {}; for (const e of Array.from(f.elements)) if ((e instanceof HTMLInputElement || e instanceof HTMLSelectElement || e instanceof HTMLTextAreaElement) && e.name) o[e.name] = e instanceof HTMLInputElement && e.type === CK ? e.checked : e.value; return o; };
const gArgs = (f: Record<string, unknown>) => { const mr = posN(f.max_runtime_h), im = posN(f.idle_min); return { accelerator: (f.accelerator as string) || "G4", max_runtime_secs: mr ? MR(mr * 3600) : null, idle_timeout_secs: im ? MR(im * 60) : null, max_spend_cu: posN(f.max_spend_cu), min_balance_cu: optN(f.min_balance_cu) }; };
const armRun = <T>(k: string, fn: () => T): T | void => { if (armed !== k) { armed = k; return renderAll(); } armed = null; return fn(); };
const doInsp = async (repo: string): Promise<void> => { selRepo = repo; dv.hfQ.value = repo; const ri = Q<HTMLInputElement>('[name="repo"]', dv.fD); if (ri) ri.value = repo; const d = await A1(TP, "inspect_hf", { repo, accelerator: Q<HTMLSelectElement>(`[name="${KA}"]`, dv.fD)?.value || "G4" }); if (d?.fits && snap) { hfFit = { repo: d.repo || repo, fits: d.fits }; dv.r(snap); } };

ON(DC, "click", async (ev) => {
  const tgt = ev.target instanceof Element ? ev.target : null; if (!tgt) return;
  const tb = tgt.closest<HTMLElement>(TBQ); if (tb) { tab = (tb.dataset.tab as TabId) || TABS[0]; armed = swPrev = null; return renderAll(); }
  const b = tgt.closest<HTMLElement>("[data-a]"); if (!b) return;
  const ds = b.dataset, { a = "", id = "", name = "", ep = "", v = "", url } = ds;
  const mp: Record<string, [string, string, Record<string, unknown>, string, string?, number?] | undefined> = {
    cv: [TS, "cancel_provision", { job_id: id }, "dn"], we: [TW, "extend", { session: name, hours: 2 }, "e2"], wa: [TW, "arm", { session: name, hours: 2 }, "dn"],
    ct: [TT, "close", { tunnel_id: +id }, "cx", undefined, 1], dt: [TT, "dismiss", { tunnel_id: +id }, "rm", undefined, 1], cr: [TP, "cancel_run", { run_id: id }, "dn"],
    rs: [TS, name ? "stop" : "reap_orphan", name ? { session: name } : { endpoint: ep }, "wk"],
    st: [TS, "stop", { session: name }, "wk", `st:${ep || name}`], rp: [TS, "reap_orphan", { endpoint: ep }, "wk", `rp:${ep}`],
    wc: [TW, "cancel", { session: name }, "dn", `wd:${ep || name}`], rk: [TS, "restart_kernel", { session: name }, "dn", `rk:${ep || name}`], rm: [TA, "remove", { account_id: id }, "rm", `rm:${id}`, 1],
  };
  const actDef = mp[a];
  if (actDef) { const run = () => A1(actDef[0], actDef[1], actDef[5] ? actDef[2] : P(actDef[2], ds), t(actDef[3])); if (actDef[4]) armRun(actDef[4], run); else void run(); return; }
  switch (a) {
    case "xb": { const p = curBnr(); if (p?.key) dimErr = p.key; bnr = null; bnrRaw = false; return syncBnr(); }
    case "bd": return (bnrRaw = !bnrRaw, syncBnr());
    case "rc": return (bnr = dimErr = null, void poll(true));
    case "au": return (auto = !auto, schedulePoll());
    case "rf": return void poll(true);
    case "lg": pickedLang = true; lang = v === "en" ? "en" : ZH; renderAll(); if (hst()) void A1(TK, "update", { patch: { locale: lang } }); return;
    case "crg": chartIdx = +v; if (snap) ov(snap); return;
    case "gt": tab = (v as TabId) || TABS[0]; return renderAll();
    case "ol": return openLink(url);
    case "cp": try { await navigator.clipboard.writeText(v || ""); flash(t("cp")); } catch { flash(v || ""); } return;
    case "qf": tab = "tunnels"; renderAll(); if (name) tv.sel.value = name; return;
    case "pv": { const f = formObj(sv.form); if (!chkN(f, [...NK, "min_vram_mib"])) return showBnr(t("bn"), ER); if (await A1(TS, "provision", P({ name: ST(f.name ?? "").trim() || null, high_mem: !!f.high_mem, min_vram_mib: posN(f.min_vram_mib), ...gArgs(f) }), t("wk"))) sv.form._d = 0; return; }
    case "ou": { const d = await A1(TS, "url", P({ session: name }, ds)); if (d?.url) openLink(d.url); return; }
    case "ot": { const f = formObj(tv.form), lp = optN(f.local_port), rp = optN(f.remote_port); if (!okP(lp, 1024) || !okP(rp, 1) || rp === 8080) return showBnr(t("pe"), ER); void A1(TT, "open", P({ session: f.session, local_port: lp, remote_port: rp, health_path: ST(f.health_path ?? "").trim() || null }), t("dn")); return; }
    case "hs": { selRepo = dv.hfQ.value.trim(); const d = await A1(TP, "search_hf", { query: selRepo }); if (d?.models && snap) { hfModels = d.models; dv.r(snap); } return; }
    case "hi": case "pr": return void doInsp(v || dv.hfQ.value.trim() || selRepo);
    case "tk": if (await A1(TA, "set_hf_token", { token: dv.tokIn.value.trim() }, t("dn"))) dv.tokIn.value = ""; return;
    case "dp": { const { __accelerator: acc = "G4", __session: sess, __sof: sof, ...inputs } = formObj(dv.fD); void A1(TP, "deploy", P({ plugin_id: selPlg, session: sess || null, accelerator: acc, stop_on_failure: !sess && !!sof, inputs }), t("wk")); return; }
    case "ip": { const s = dv.srcIn.value.trim(); if (!s) return; const g = s.includes("://") || s.includes("@") || s.endsWith(".git"); if (await A1(TP, g ? "install_git" : "install_dir", g ? { url: s } : { path: s }, t("is"))) dv.srcIn.value = ""; return; }
    case "bm": epoch += 1; if (await A1(TK, "update", { patch: { backend_mode: v } }, t(OK))) { sv.form._d = setv.form._d = 0; if (!busy) void poll(true); } return;
    case "sw": { const d = await A1(TA, "preview_switch", { account_id: id }); if (d && snap) { swPrev = { id, data: d }; setv.r(snap); } return; }
    case "swx": swPrev = null; if (snap) setv.r(snap); return;
    case "swc": if (!swPrev) return; { const tid = swPrev.id; swPrev = null; void A1(TA, "switch", { account_id: tid, confirm: true }, t("dn")); return; }
    case "ad": { const aid = setv.aId.value.trim(); if (!aid) return; if (await A1(TA, "add", { account_id: aid, label: setv.aLb.value.trim() || aid, email: setv.aEm.value.trim() || null, auth_mode: "oauth2" }, t(OK))) setv.aId.value = setv.aLb.value = setv.aEm.value = ""; return; }
    case "sv": {
      const f = formObj(setv.form), mc = optN(f[KMC]), paS = optN(f[KPA]), piS = optN(f[KPI]);
      if (!chkN(f, NK)) return showBnr(t("bn"), ER); if (mc != null && (!NI(mc) || mc < 1 || mc > 8)) return showBnr(t("bc"), ER);
      const patch: Record<string, unknown> = { ...gArgs(f), [KMC]: mc || 2 }; if (paS != null) patch[KPA] = MX(10, MN(600, MR(paS))); if (piS != null) patch[KPI] = MX(30, MN(3600, MR(piS)));
      if (await A1(TK, "update", { patch }, t(OK))) setv.form._d = 0;
    }
  }
});

ON(DC, "visibilitychange", () => { if (DC.hidden) { if (pollT !== null) { CT(pollT); pollT = null; } } else if ((connOk || down) && auto && emb()) void poll(false); });
bridge.ontoolresult = (res) => onSnap(parseR(res as McpToolResult), epoch);
const bootSnap = (window as Window & { __COLAB_BOOT__?: Snap }).__COLAB_BOOT__;
const bootPick = selectBootHydration(bootSnap, readCachedSnap<Snap>(getStorage()));
if (bootPick.snap) applySnap(bootPick.snap, epoch, bootPick.fromCache); else renderHeader();
setInterval(() => { if (!DC.hidden && (tab === "sessions" || tab === "overview") && snap?.sessions?.length) VIEWS[tab](snap); }, 1e3);
const setDown = (): void => { failCount = 3; down = true; schedulePoll(); };
if (emb()) bridge.connect().then(() => { if (!bridge.isConnected() || !hst()) return setDown(); connOk = true; if (bootPick.freshBoot) schedulePoll(); else if (!snap || snap.syncing || !bootPick.freshBoot) void poll(false); }).catch(setDown);
