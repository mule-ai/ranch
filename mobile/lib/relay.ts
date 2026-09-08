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
  onFrame: FrameHandler = () => {};
  onStatus: (s: string) => void = () => {};

  constructor(machineId: string) {
    this.machineId = machineId;
  }

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
      if (f && typeof f === "object" && "t" in f) this.onFrame(f);
    });
    ch.subscribe();
    this.onStatus("connecting…");
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
