// In-memory chat history cache (session+pane -> msgs + hasMore).
//
// Why: the daemon now sends only the last N chat rows on snapshot
// (Attach.chat_limit). Older rows are paged via ChatHistory. This cache
// keeps everything the client has already seen, so RE-OPENING a chat
// doesn't refetch/refetch-and-render history, and scrollback that was
// loaded once stays loaded for the life of the app process.
//
// LRU-ish: most-recently-touched entries survive; oldest are evicted.
// Per-pane history is capped so a very long session can't grow the
// cache without bound (evicted older rows can always be re-paged from
// the daemon).

import type { ChatMsg } from "./frames";

export type ChatCacheEntry = { msgs: ChatMsg[]; hasMore: boolean };

const cache = new Map<string, ChatCacheEntry>();

const MAX_ENTRIES = 24; // ~24 chat panes
const MAX_MSGS = 2000; // per pane

export const chatKey = (session: string, pane: string) => `${session}/${pane}`;

/** Read (and touch for LRU). */
export function getCachedChat(session: string, pane: string): ChatCacheEntry | undefined {
  const k = chatKey(session, pane);
  const e = cache.get(k);
  if (e) {
    cache.delete(k);
    cache.set(k, e);
  }
  return e;
}

/** Store (touch, cap entries, cap per-pane rows). */
export function saveCachedChat(
  session: string,
  pane: string,
  msgs: ChatMsg[],
  hasMore: boolean,
): void {
  const k = chatKey(session, pane);
  let m = msgs;
  if (m.length > MAX_MSGS) m = m.slice(m.length - MAX_MSGS);
  // hasMore must stay true if we trimmed or if caller says more exist
  const more = hasMore || msgs.length > MAX_MSGS;
  cache.delete(k);
  cache.set(k, { msgs: m, hasMore: more });
  while (cache.size > MAX_ENTRIES) {
    const first = cache.keys().next().value as string | undefined;
    if (first === undefined || first === k) break;
    cache.delete(first);
  }
}

/** Drop all panes of a session (session killed / compaction reset). */
export function clearSessionChat(session: string): void {
  for (const k of [...cache.keys()]) {
    if (k.startsWith(session + "/")) cache.delete(k);
  }
}

/**
 * Merge a fresh snapshot tail with cached older rows.
 * The snapshot tail is authoritative for recent rows; the cache supplies
 * anything older than the tail. Returns msgs oldest-first.
 */
export function mergeChat(
  cached: ChatCacheEntry | undefined,
  tail: ChatMsg[],
  tailHasMore: boolean,
): { msgs: ChatMsg[]; hasMore: boolean } {
  if (!cached || cached.msgs.length === 0) {
    return { msgs: tail, hasMore: tailHasMore };
  }
  if (tail.length === 0) {
    return { msgs: cached.msgs, hasMore: cached.hasMore };
  }
  const minTail = tail[0].seq;
  const older = cached.msgs.filter((m) => m.seq < minTail);
  return { msgs: [...older, ...tail], hasMore: tailHasMore || older.length > 0 };
}
