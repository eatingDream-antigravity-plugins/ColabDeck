export const SNAP_CACHE_KEY = "colab_mcp_snap_v1";
export const FRESH_BOOT_MS = 2500;
export const MAX_CACHE_HISTORY = 64;

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export interface CacheEnvelope<T = Record<string, unknown>> {
  v: number;
  savedAt: number;
  data: T;
}

export interface BootSnapShape {
  rev?: number;
  bootTs?: number;
  syncing?: boolean;
  partial?: boolean;
}

export function compactSnapForCache<T extends object>(snap: T): T {
  const out: Record<string, unknown> = {};
  for (const [k, v] of Object.entries(snap)) {
    if (k === "hf_token" || k === "snapshot" || k === "unchanged" || k === "server_ms" || k === "bootTs" || k === "partial") continue;
    out[k] = k === "balance_history" && Array.isArray(v) ? v.slice(-MAX_CACHE_HISTORY) : v;
  }
  return out as T;
}

export function readCachedSnap<T extends BootSnapShape>(storage?: StorageLike | null): CacheEnvelope<T> | null {
  try {
    const raw = storage?.getItem(SNAP_CACHE_KEY);
    if (!raw) return null;
    const parsed = JSON.parse(raw) as Partial<CacheEnvelope<T>> | null;
    if (!parsed || parsed.v !== 1 || typeof parsed.savedAt !== "number" || !Number.isFinite(parsed.savedAt)) return null;
    if (!parsed.data || typeof parsed.data !== "object" || typeof parsed.data.rev !== "number") return null;
    return { v: 1, savedAt: parsed.savedAt, data: compactSnapForCache(parsed.data) };
  } catch {
    return null;
  }
}

export function writeCachedSnap<T extends BootSnapShape>(
  storage: StorageLike | null | undefined,
  snap: T,
  now = Date.now()
): boolean {
  try {
    if (!storage || typeof snap.rev !== "number") return false;
    const env: CacheEnvelope<T> = { v: 1, savedAt: now, data: compactSnapForCache(snap) };
    storage.setItem(SNAP_CACHE_KEY, JSON.stringify(env));
    return true;
  } catch {
    return false;
  }
}

export function selectBootHydration<T extends BootSnapShape>(
  bootSnap: T | null | undefined,
  cached: CacheEnvelope<T> | null,
  now = Date.now()
): { snap: T | null; fromCache: boolean; freshBoot: boolean } {
  const validBoot = bootSnap && typeof bootSnap === "object" && typeof bootSnap.rev === "number" ? bootSnap : null;
  if (!validBoot || (cached && cached.savedAt > (validBoot.bootTs ?? 0))) {
    return cached ? { snap: cached.data, fromCache: true, freshBoot: false } : { snap: null, fromCache: false, freshBoot: false };
  }
  const freshBoot = typeof validBoot.bootTs === "number" && Number.isFinite(validBoot.bootTs) && now - validBoot.bootTs < FRESH_BOOT_MS;
  return { snap: validBoot, fromCache: false, freshBoot };
}

export function updateEwmaRtt(prevEwma: number, rpcTotalMs: number, serverMs = 0): { netRttMs: number; ewmaRttMs: number } {
  const netRttMs = Math.max(0, Math.round(rpcTotalMs - (Number.isFinite(serverMs) ? serverMs : 0)));
  const ewmaRttMs = prevEwma > 0 ? Math.round(prevEwma * 0.7 + netRttMs * 0.3) : netRttMs;
  return { netRttMs, ewmaRttMs };
}

export function adaptPollDelay(baseMs: number, ewmaRttMs: number, down: boolean, failCount: number): number {
  if (down) return Math.min(30_000, 5_000 * 2 ** Math.max(0, failCount - 3));
  return Math.max(baseMs, Math.min(15_000, Math.round(ewmaRttMs * 3)));
}

export function canSchedulePoll(connOk: boolean, down: boolean, auto: boolean, hidden: boolean, embedded: boolean): boolean {
  return (connOk || down) && auto && !hidden && embedded;
}
