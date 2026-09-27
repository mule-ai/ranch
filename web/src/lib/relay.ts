// Relay client: joins the machine's private Realtime channel and speaks
// ranch frames inside `broadcast` events. Mirrors what the daemon's
// relay thread does, from the client side.
import type { RealtimeChannel } from "@supabase/supabase-js";
import { supabase } from "./supabase";
import { Frame } from "./frames";

export type FrameHandler = (f: Frame) => void;

export class Relay {
  channel: RealtimeChannel | null = null;
  machineId: string;
  /** frame listeners — screens subscribe/unsubscribe instead of
   *  overwriting each other's handler (App list + Terminal) */
  private listeners = new Set<FrameHandler>();
  onFrame(handler: FrameHandler): () => void {
    this.listeners.add(handler);
    return () => this.listeners.delete(handler);
  }
  /** fired every time the channel reaches SUBSCRIBED (initial + reconnects) */
  onReady: () => void = () => {};
  onStatus: (s: string) => void = () => {};
  // chunk reassembly: chunk_id -> { parts, received, n }
  private chunks = new Map<string, { parts: (string | null)[]; received: number }>();

  constructor(machineId: string) {
    this.machineId = machineId;
  }

  /** Join and resolve only once the channel is actually SUBSCRIBED —
   *  frames sent before that are silently dropped by supabase-js. */
  async join(): Promise<void> {
    await supabase.realtime.setAuth();
    const topic = `machines:${this.machineId}`;
    const ch = supabase.channel(topic, {
      config: { broadcast: { self: false }, presence: { key: "mobile" }, private: true },
    });
    this.channel = ch;

    // supabase-js unwraps the phoenix payload; the daemon wraps frames as
    // {event:"frame", payload:<frame>}, so `payload` here IS the frame.
    ch.on("broadcast", { event: "frame" }, ({ payload }) => {
      this.lastInbound = Date.now();
      const f = payload as Frame | undefined;
      if (!f || typeof f !== "object" || !("t" in f)) return;
      if (f.t === "Chunk") {
        const c = f as unknown as { chunk_id: string; i: number; n: number; data: string };
        // absurd n would allocate n slots — drop corrupt/hostile batches
        // (largest legitimate payload ≈ 70 chunks; matches MAX_CHUNKS)
        if (!c.chunk_id || c.n <= 0 || c.n > 512 || c.i < 0 || c.i >= c.n) return;
        let entry = this.chunks.get(c.chunk_id);
        if (!entry) {
          entry = { parts: Array(c.n).fill(null), received: 0 };
          this.chunks.set(c.chunk_id, entry);
        }
        if (entry.parts[c.i] === null) {
          entry.parts[c.i] = c.data;
          entry.received++;
        }
        if (entry.received === c.n) {
          this.chunks.delete(c.chunk_id);
          try {
            const assembled = JSON.parse(entry.parts.join("")) as Frame;
            if (assembled && typeof assembled === "object" && "t" in assembled) {
              for (const h of this.listeners) h(assembled);
            }
          } catch {
            // torn batch — the seq-gap / re-attach path recovers
          }
        }
        return;
      }
      for (const h of this.listeners) h(f);
    });

    this.onStatus("connecting…");
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error("realtime join timed out")),
        15000
      );
      ch.subscribe((status) => {
        // ANY status callback means the socket/channel is alive —
        // touch() so the idle watchdog doesn't tear down a healthy
        // but quiet connection (supabase-js's own heartbeat traffic
        // doesn't flow through the broadcast listener).
        this.touch();
        if (status === "SUBSCRIBED") {
          clearTimeout(timer);
          this.onStatus("online");
          this.onReady();
          resolve();
        } else if (
          status === "CHANNEL_ERROR" ||
          status === "TIMED_OUT" ||
          status === "CLOSED"
        ) {
          // supabase-js is supposed to resubscribe on its own, but in
          // practice a half-dead socket (laptop sleep, NAT timeout) can
          // leave the channel erroring forever without a fresh join.
          // Tear the channel down and re-join from scratch.
          this.onStatus("reconnecting…");
          this.reconnectSoon();
        }
      });
    });
  }

  /** inbound frame watchdog state */
  private lastInbound = Date.now();
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;

  /** Tear down + re-join after a short delay (deduped). */
  private reconnectSoon() {
    if (this.reconnectTimer) return;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      if (!this.channel) return;
      try {
        this.channel.unsubscribe();
        supabase.removeChannel(this.channel);
      } catch {
        /* already gone */
      }
      this.channel = null;
      this.join().catch(() => this.reconnectSoon());
    }, 2000);
  }

  /** Force a reconnect if no frames (of any kind) arrived recently.
   *  Screens call this when the UI looks stuck. */
  ensureAlive(maxAgeMs = 30000) {
    if (Date.now() - this.lastInbound < maxAgeMs) return;
    this.lastInbound = Date.now();
    this.reconnectSoon();
  }

  /** note inbound activity (any frame) so ensureAlive doesn't fire */
  touch() {
    this.lastInbound = Date.now();
  }

  lastInboundAt() {
    return this.lastInbound;
  }

  /** Max bytes of one broadcast's ranch frame before we chunk it.
   *  Supabase Realtime silently drops single private-channel broadcasts
   *  above ~256 KiB (measured 2026-09-27: 256,000 B arrives, 266,240 B
   *  never does); 192 KiB matches the daemon's MAX_FRAME. */
  private static readonly MAX_FRAME = 192 * 1024;

  send(f: Frame) {
    const json = JSON.stringify(f);
    if (json.length <= Relay.MAX_FRAME) {
      this.channel?.send({
        type: "broadcast",
        event: "frame",
        payload: f,
      });
      return;
    }
    // Oversized frame (PROTOCOL §5): split into Chunk slices the daemon
    // reassembles. JS slicing is UTF-16 code-unit safe — surrogate pairs
    // survive the split+join round trip.
    const cid = `c${Date.now().toString(36)}-${Math.floor(Math.random() * 1e9).toString(36)}`;
    const n = Math.ceil(json.length / Relay.MAX_FRAME);
    let start = 0;
    for (let i = 0; i < n; i++) {
      const end = Math.min(json.length, start + Relay.MAX_FRAME);
      this.channel?.send({
        type: "broadcast",
        event: "frame",
        payload: { t: "Chunk", chunk_id: cid, i, n, data: json.slice(start, end) },
      });
      start = end;
    }
  }

  leave() {
    this.channel?.unsubscribe();
    supabase.removeChannel(this.channel!);
    this.channel = null;
  }
}
