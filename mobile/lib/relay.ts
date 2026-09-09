// Relay client: joins the machine's private Realtime channel and speaks
// ranch frames inside `broadcast` events. Mirrors what the daemon's
// relay thread does, from the client side.
import { RealtimeChannel } from "@supabase/supabase-js";
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
      const f = payload as Frame | undefined;
      if (!f || typeof f !== "object" || !("t" in f)) return;
      if (f.t === "Chunk") {
        const c = f as unknown as { chunk_id: string; i: number; n: number; data: string };
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
          // don't reject after the initial join — supabase-js resubscribes
          // automatically; onReady fires again when it's back
          this.onStatus("reconnecting…");
        }
      });
    });
  }

  send(f: Frame) {
    this.channel?.send({
      type: "broadcast",
      event: "frame",
      payload: f,
    });
  }

  leave() {
    this.channel?.unsubscribe();
    supabase.removeChannel(this.channel!);
    this.channel = null;
  }
}
