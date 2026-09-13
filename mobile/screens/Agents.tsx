// Agents screen (Phase B): browse/create/edit/delete forge agent
// profiles through the daemon's forge proxy; tap "launch" to open an
// agent pane running that profile (SessionsCreate kind=forge +
// profile_id).
import { useCallback, useEffect, useState } from "react";
import {
  Alert,
  ActivityIndicator,
  BackHandler,
  FlatList,
  Pressable,
  StyleSheet,
  Text,
  TextInput,
  View,
} from "react-native";
import { Relay } from "../lib/relay";
import { Frame, ModelChoice, ProfileSummary, ProfileDraft, nextId } from "../lib/frames";

const PROVIDERS = ["openai", "anthropic", "proxy-anthropic", "proxy", "google", "gemini", "custom"];
const KNOWN_TOOLS = ["bash", "read", "write", "edit"];

type Props = { relay: Relay; onLaunch: (p: ProfileSummary) => void; onExit: () => void };

export function AgentsScreen({ relay, onLaunch, onExit }: Props) {
  const [profiles, setProfiles] = useState<ProfileSummary[] | null>(null);
  const [editing, setEditing] = useState<{ id: string | null; draft: ProfileDraft } | null>(null);

  // back gesture/button: close the profile form first (mirrors the
  // form's "‹ agents" control), then exit the screen
  useEffect(() => {
    const sub = BackHandler.addEventListener("hardwareBackPress", () => {
      if (editing) {
        setEditing(null);
      } else {
        onExit();
      }
      return true;
    });
    return () => sub.remove();
  }, [editing, onExit]);

  const refresh = useCallback(() => {
    relay.send({ t: "ProfileList", id: nextId(), client: "mobile", req_id: nextId() } as Frame);
  }, [relay]);

  useEffect(() => {
    const un = relay.onFrame((f: Frame) => {
      switch (f.t) {
        case "ProfileListOk":
          setProfiles(f.profiles);
          break;
        case "ProfilePutOk":
          setEditing(null);
          refresh();
          break;
        case "ProfileDeleteOk":
          refresh();
          break;
      }
    });
    refresh();
    return un;
  }, [relay, refresh]);

  if (editing) {
    return (
      <ProfileForm
        relay={relay}
        profileId={editing.id}
        initial={editing.draft}
        onDone={() => setEditing(null)}
      />
    );
  }

  return (
    <View style={s.wrap}>
      <View style={s.header}>
        <Pressable onPress={onExit} hitSlop={8}>
          <Text style={s.back}>‹ sessions</Text>
        </Pressable>
        <Text style={s.title}>agents</Text>
        <View style={{ width: 70 }} />
      </View>
      {profiles === null ? (
        <ActivityIndicator color="#4ade80" style={{ marginTop: 32 }} />
      ) : (
        <FlatList
          data={profiles}
          keyExtractor={(p) => p.id}
          contentContainerStyle={{ paddingBottom: 40 }}
          ListEmptyComponent={<Text style={s.dim}>No agent profiles yet.</Text>}
          renderItem={({ item }) => (
            <View style={s.row}>
              <Text style={s.rowTitle}>{item.name}</Text>
              <Text style={s.dim}>
                {item.provider}/{item.model}
              </Text>
              <View style={s.btnRow}>
                <Pressable style={[s.chip, s.chipGo]} onPress={() => onLaunch(item)}>
                  <Text style={s.chipGoText}>launch</Text>
                </Pressable>
                <Pressable
                  style={s.chip}
                  onPress={async () => {
                    const rid = nextId();
                    relay.send({ t: "ProfileGet", id: nextId(), client: "mobile", req_id: rid, profile: item.id } as Frame);
                    const un = relay.onFrame((f: Frame) => {
                      if (f.t === "ProfileGetOk" && f.req_id === rid) {
                        un();
                        setEditing({
                          id: f.profile.id,
                          draft: {
                            name: f.profile.name,
                            description: f.profile.description,
                            provider: f.profile.provider,
                            model: f.profile.model,
                            base_url: f.profile.base_url,
                            working_dir: f.profile.working_dir,
                            git_url: f.profile.git_url,
                            git_ref: f.profile.git_ref,
                            nix_shell: f.profile.nix_shell,
                            system_prompt: f.profile.system_prompt,
                            tools: f.profile.tools,
                          },
                        });
                      }
                    });
                  }}
                >
                  <Text style={s.chipText}>edit</Text>
                </Pressable>
                <Pressable
                  style={s.chip}
                  onLongPress={() =>
                    Alert.alert("delete profile", `delete "${item.name}"?`, [
                      { text: "cancel", style: "cancel" },
                      {
                        text: "delete",
                        style: "destructive",
                        onPress: () =>
                          relay.send({ t: "ProfileDelete", id: nextId(), client: "mobile", req_id: nextId(), profile: item.id } as Frame),
                      },
                    ])
                  }
                >
                  <Text style={[s.chipText, { color: "#f87171" }]}>delete</Text>
                </Pressable>
              </View>
            </View>
          )}
        />
      )}
      <Pressable
        style={s.newBtn}
        onPress={() =>
          setEditing({
            id: null,
            draft: { name: "", provider: PROVIDERS[0], model: "", system_prompt: "", tools: KNOWN_TOOLS },
          })
        }
      >
        <Text style={s.newBtnText}>new agent profile</Text>
      </Pressable>
    </View>
  );
}

function ProfileForm({
  relay,
  profileId,
  initial,
  onDone,
}: {
  relay: Relay;
  profileId: string | null;
  initial: ProfileDraft;
  onDone: () => void;
}) {
  const [draft, setDraft] = useState<ProfileDraft>(initial);
  const [busy, setBusy] = useState(false);
  // pi model catalog via the daemon (empty when forge is unreachable —
  // then the model field falls back to free text)
  const [catalog, setCatalog] = useState<ModelChoice[]>([]);
  // which provider's models to show in the picker (null = all)
  const [catProv, setCatProv] = useState<string | null>(null);
  const set = (patch: Partial<ProfileDraft>) => setDraft((d) => ({ ...d, ...patch }));

  useEffect(() => {
    const rid = nextId();
    const un = relay.onFrame((f: Frame) => {
      if (f.t === "ModelCatalogOk" && f.req_id === rid) {
        un();
        setCatalog(f.models);
      }
    });
    relay.send({ t: "ModelCatalog", id: nextId(), client: "mobile", req_id: rid } as Frame);
    return un;
  }, [relay]);

  const save = () => {
    if (!draft.name.trim() || !draft.model.trim()) return;
    setBusy(true);
    const rid = nextId();
    relay.send({
      t: "ProfilePut", id: nextId(), client: "mobile", req_id: rid,
      profile_id: profileId,
      draft: { ...draft, api_key: draft.api_key?.trim() ? draft.api_key : null },
    } as unknown as Frame);
    const un = relay.onFrame((f: Frame) => {
      if ((f.t === "ProfilePutOk" && f.req_id === rid) || (f.t === "Error" && f.req_id === rid)) {
        un();
        onDone();
      }
    });
  };

  return (
    <View style={s.wrap}>
      <View style={s.header}>
        <Pressable onPress={onDone} hitSlop={8}>
          <Text style={s.back}>‹ agents</Text>
        </Pressable>
        <Text style={s.title}>{profileId ? `edit: ${initial.name}` : "new profile"}</Text>
        <View style={{ width: 70 }} />
      </View>
      <FlatList
        data={[]} // header-only form
        ListHeaderComponent={
          <View style={{ gap: 10, paddingBottom: 40 }}>
            <Text style={s.label}>name</Text>
            <TextInput style={s.input} value={draft.name} onChangeText={(v) => set({ name: v })} autoCapitalize="none" />
            <Text style={s.label}>description</Text>
            <TextInput style={s.input} value={draft.description ?? ""} onChangeText={(v) => set({ description: v || null })} autoCapitalize="none" />
            <Text style={s.label}>provider</Text>
            <View style={{ flexDirection: "row", flexWrap: "wrap", gap: 6 }}>
              {PROVIDERS.map((p) => (
                <Pressable key={p} style={[s.chip, draft.provider === p && s.chipOn]} onPress={() => set({ provider: p })}>
                  <Text style={[s.chipText, draft.provider === p && s.chipTextOn]}>{p}</Text>
                </Pressable>
              ))}
            </View>
            <Text style={s.label}>model</Text>
            {(() => {
              const provs = Array.from(new Set(catalog.map((m) => m.provider)));
              const models = catProv
                ? catalog.filter((m) => m.provider === catProv)
                : catalog;
              if (catalog.length === 0) {
                // no catalog (forge unreachable): free text fallback
                return (
                  <TextInput style={s.input} value={draft.model} onChangeText={(v) => set({ model: v })} autoCapitalize="none" placeholder="model id" placeholderTextColor="#4b5563" />
                );
              }
              return (
                <View style={{ gap: 6 }}>
                  <View style={{ flexDirection: "row", flexWrap: "wrap", gap: 6 }}>
                    {provs.map((p) => (
                      <Pressable
                        key={p}
                        style={[s.chip, catProv === p && s.chipOn]}
                        onPress={() => setCatProv(catProv === p ? null : p)}
                      >
                        <Text style={[s.chipText, catProv === p && s.chipTextOn]}>{p}</Text>
                      </Pressable>
                    ))}
                  </View>
                  <View style={{ flexDirection: "row", flexWrap: "wrap", gap: 6 }}>
                    {(!draft.model.trim() || models.some((m) => m.id === draft.model)) ? null : (
                      <Pressable style={[s.chip, s.chipOn]} onPress={() => set({ model: "" })}>
                        <Text style={[s.chipText, s.chipTextOn]} numberOfLines={1}>{draft.model}</Text>
                      </Pressable>
                    )}
                    {models.map((m) => (
                      <Pressable
                        key={m.provider + "/" + m.id}
                        style={[s.chip, draft.model === m.id && s.chipOn]}
                        onPress={() => set({ model: m.id, provider: m.provider })}
                      >
                        <Text style={[s.chipText, draft.model === m.id && s.chipTextOn]} numberOfLines={1}>
                          {m.name !== m.id ? `${m.name} (${m.id})` : m.id}
                        </Text>
                      </Pressable>
                    ))}
                  </View>
                  {draft.model.trim() !== "" && (
                    <Text style={s.dim}>model: {draft.model}</Text>
                  )}
                </View>
              );
            })()}
            <Text style={s.label}>system prompt</Text>
            <TextInput
              style={[s.input, { minHeight: 90, textAlignVertical: "top" }]}
              value={draft.system_prompt ?? ""}
              onChangeText={(v) => set({ system_prompt: v })}
              multiline
            />
            <Text style={s.label}>api key (write-only; blank keeps stored)</Text>
            <TextInput style={s.input} value={draft.api_key ?? ""} onChangeText={(v) => set({ api_key: v })} secureTextEntry autoCapitalize="none" />
            <Text style={s.label}>working dir</Text>
            <TextInput style={s.input} value={draft.working_dir ?? ""} onChangeText={(v) => set({ working_dir: v || null })} autoCapitalize="none" placeholder="/absolute/path" placeholderTextColor="#4b5563" />
            <Text style={s.label}>tools</Text>
            <View style={{ flexDirection: "row", flexWrap: "wrap", gap: 6 }}>
              {KNOWN_TOOLS.map((t) => (
                <Pressable
                  key={t}
                  style={[s.chip, draft.tools.includes(t) && s.chipOn]}
                  onPress={() =>
                    set({ tools: draft.tools.includes(t) ? draft.tools.filter((x) => x !== t) : [...draft.tools, t] })
                  }
                >
                  <Text style={[s.chipText, draft.tools.includes(t) && s.chipTextOn]}>{t}</Text>
                </Pressable>
              ))}
            </View>
            <Pressable style={[s.newBtn, busy && { opacity: 0.5 }]} onPress={save} disabled={busy}>
              <Text style={s.newBtnText}>{busy ? "saving…" : "save profile"}</Text>
            </Pressable>
          </View>
        }
        renderItem={() => null}
      />
    </View>
  );
}

const s = StyleSheet.create({
  wrap: { flex: 1, backgroundColor: "#101014", paddingTop: 60, paddingHorizontal: 16 },
  header: { flexDirection: "row", alignItems: "center", justifyContent: "space-between", marginBottom: 12 },
  back: { color: "#4ade80", width: 70 },
  title: { color: "#f3f4f6", fontWeight: "700", fontSize: 17 },
  row: { paddingVertical: 12, borderBottomWidth: 1, borderBottomColor: "#1f2430", gap: 2 },
  rowTitle: { color: "#f3f4f6", fontSize: 17, fontWeight: "600" },
  dim: { color: "#6b7280", fontSize: 13 },
  label: { color: "#9ca3af", fontSize: 12, marginTop: 6 },
  input: {
    backgroundColor: "#1a1b23", borderRadius: 8, paddingHorizontal: 12, paddingVertical: 8,
    color: "#f3f4f6", fontSize: 14,
  },
  btnRow: { flexDirection: "row", gap: 8, marginTop: 8 },
  chip: {
    borderWidth: 1, borderColor: "#374151", borderRadius: 999,
    paddingHorizontal: 12, paddingVertical: 4,
  },
  chipOn: { borderColor: "#4ade80", backgroundColor: "rgba(74,222,128,0.12)" },
  chipText: { color: "#9ca3af", fontSize: 13 },
  chipTextOn: { color: "#4ade80", fontSize: 13, fontWeight: "600" },
  chipGo: { borderColor: "#4ade80" },
  chipGoText: { color: "#4ade80", fontSize: 13, fontWeight: "700" },
  newBtn: {
    backgroundColor: "#16a34a", borderRadius: 8, paddingVertical: 12,
    alignItems: "center", marginVertical: 8,
  },
  newBtnText: { color: "#fff", fontWeight: "700" },
});
