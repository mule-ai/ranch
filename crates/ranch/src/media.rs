//! Image media (M-images): the daemon-side registry that decides which
//! files on the daemon machine clients may fetch bytes for via `FileGet`.
//!
//! All clients see the same conversation rows: image references are plain
//! absolute paths (`ChatMsg.image_refs` / `ChatMsg.attachments`). To
//! actually render pixels, a client asks the daemon with `FileGet`, and
//! the daemon answers only for *registered* media paths:
//!
//! - files uploaded from a client via `FilePut` (the uploads dir),
//! - `ChatSend` attachments that are images,
//! - images the agent read during chat (tool-call rows with an image path).
//!
//! The registry is a small JSON array under the state dir, persisted so
//! images from old conversations stay viewable after a daemon restart.

use std::collections::HashSet;
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use base64::Engine as _;
use ranch_protocol::ChatMsg;

/// Max bytes of one image inlined into an agent prompt (matches the
/// `FilePut` upload cap).
pub const IMAGE_INLINE_CAP: u64 = 10 * 1024 * 1024;

/// An image inlined into a pi `prompt` RPC (`images` field).
pub struct PiImage {
    pub path: String,
    pub b64: String,
    pub mime: String,
}

/// Load an image file for vision inlining. Errors when the file is
/// unreadable, not an image, or over the inline cap.
pub fn load_image(path: &str) -> Result<PiImage, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    if (bytes.len() as u64) > IMAGE_INLINE_CAP {
        return Err(format!(
            "{path}: {} bytes exceeds the {} MiB inline cap",
            bytes.len(),
            IMAGE_INLINE_CAP / (1024 * 1024)
        ));
    }
    let mime = sniff_mime(path).ok_or_else(|| format!("{path}: not a recognized image"))?;
    let enc = base64::engine::general_purpose::STANDARD;
    Ok(PiImage {
        path: path.to_string(),
        b64: enc.encode(bytes),
        mime,
    })
}

/// Max number of paths kept in the registry (FIFO eviction).
const CAP: usize = 4096;

static REG: OnceLock<Mutex<Reg>> = OnceLock::new();

struct Reg {
    paths: HashSet<String>,
    /// insertion order, for FIFO eviction
    order: VecDeque<String>,
}

fn state_file() -> std::path::PathBuf {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    home.join(".local/state/ranch/media.json")
}

fn reg() -> &'static Mutex<Reg> {
    REG.get_or_init(|| {
        let mut r = Reg {
            paths: HashSet::new(),
            order: VecDeque::new(),
        };
        // load the persisted registry (best effort)
        if let Ok(raw) = std::fs::read_to_string(state_file()) {
            if let Ok(list) = serde_json::from_str::<Vec<String>>(raw.trim()) {
                for p in list.into_iter().take(CAP) {
                    if r.paths.insert(p.clone()) {
                        r.order.push_back(p);
                    }
                }
            }
        }
        Mutex::new(r)
    })
}

fn persist(g: &Reg) {
    let raw = serde_json::to_string(&g.order.iter().collect::<Vec<_>>()).unwrap_or_default();
    if let Some(parent) = state_file().parent() {
        let _ = std::fs::create_dir_all(parent);
        let _ = std::fs::write(state_file(), raw);
    }
}

/// Record `path` as viewable media. Idempotent; cheap. Called from the
/// main loop, the local-pi reader thread, and the forge worker, so this
/// takes the process-global lock (held only long enough to write a small
/// file, if at all).
pub fn register(path: &str) {
    let mut g = match reg().lock() {
        Ok(g) => g,
        Err(_) => return,
    };
    register_into(&mut g, path);
}

fn register_into(g: &mut Reg, path: &str) {
    let p = path.to_string();
    if !g.paths.insert(p.clone()) {
        return;
    }
    g.order.push_back(p.clone());
    while g.order.len() > CAP {
        if let Some(old) = g.order.pop_front() {
            g.paths.remove(&old);
        }
    }
    persist(g);
}

pub fn is_registered(path: &str) -> bool {
    reg()
        .lock()
        .map(|g| g.paths.contains(path))
        .unwrap_or(false)
}

// ---------- image type detection ----------

fn ext(p: &str) -> String {
    Path::new(p)
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default()
}

/// Extension-based image test — cheap, no I/O. Used to classify
/// attachments before they reach the agent.
pub fn is_image_path(p: &str) -> bool {
    matches!(
        ext(p).as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "avif" | "heic" | "heif" | "svg"
    )
}

/// True when the file at `path` is a recognized image — by extension OR by
/// magic bytes. Mobile photo pickers hand out opaque names without an
/// extension (e.g. uploads/1790510420-1000006546), so extension alone is
/// not enough: an extensionless upload would otherwise be inlined as
/// binary "text" instead of a vision payload.
pub fn file_is_image(path: &str) -> bool {
    if is_image_path(path) {
        return true;
    }
    let Ok(head) = std::fs::read(Path::new(path)) else {
        return false;
    };
    sniff_mime_bytes(&head).is_some()
}

/// Magic-byte mime sniffing with an extension fallback. `None` when the
/// file is not readable or not a recognized image.
pub fn sniff_mime(path: &str) -> Option<String> {
    let head = std::fs::read(Path::new(path)).ok().map(|b| b)?;
    sniff_mime_bytes(&head).or_else(|| mime_from_ext(path))
}

fn mime_from_ext(path: &str) -> Option<String> {
    let m = match ext(path).as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "avif" => "image/avif",
        "heic" | "heif" => "image/heif",
        "svg" => "image/svg+xml",
        _ => return None,
    };
    Some(m.into())
}

fn sniff_mime_bytes(b: &[u8]) -> Option<String> {
    let magic = |n: usize| b.get(..n).is_some();
    if magic(4) && b[..4] == [0x89, b'P', b'N', b'G'] {
        return Some("image/png".into());
    }
    if magic(3) && b[..3] == [0xFF, 0xD8, 0xFF] {
        return Some("image/jpeg".into());
    }
    if magic(4) && b[..4] == *b"GIF8" {
        return Some("image/gif".into());
    }
    if magic(12) && b[..4] == *b"RIFF" && b[8..12] == *b"WEBP" {
        return Some("image/webp".into());
    }
    if magic(2) && b[..2] == *b"BM" {
        return Some("image/bmp".into());
    }
    if magic(12) && b[4..8] == *b"ftyp" {
        let brand = &b[8..12];
        return Some(if brand == b"avif" || brand == b"avis" {
            "image/avif".into()
        } else {
            "image/heif".into()
        });
    }
    None
}

/// Pull image paths out of a tool call's args JSON (keys: `path`,
/// `file_path`, `filePath`, `file`, `image`, `image_path`, …). Returns
/// only the ones that are images.
pub fn image_refs_from_tool_args(args_json: &str) -> Vec<String> {
    let obj = match serde_json::from_str::<serde_json::Value>(args_json) {
        Ok(serde_json::Value::Object(o)) => o,
        _ => return Vec::new(),
    };
    const KEYS: &[&str] = &[
        "path", "file_path", "filePath", "file", "image", "image_path", "imagePath", "filename",
    ];
    let mut out = Vec::new();
    for k in KEYS {
        if let Some(p) = obj.get(*k).and_then(|v| v.as_str()) {
            if is_image_path(p) && !out.iter().any(|x| x == p) {
                out.push(p.to_string());
            }
        }
    }
    out
}

/// Post-pass over derived chat rows: any tool row whose args reference an
/// image gets `image_refs` set and the path registered so clients can
/// `FileGet` it later. Used on replay paths (session-file read,
/// `get_messages` resync) where rows are built in bulk.
pub fn flag_image_refs(msgs: &mut [ChatMsg]) {
    for m in msgs.iter_mut() {
        if m.role != "tool" {
            continue;
        }
        if m.image_refs.is_some() {
            continue;
        }
        if let Some(args) = &m.tool_args {
            let refs = image_refs_from_tool_args(args);
            if !refs.is_empty() {
                m.image_refs = Some(refs.clone());
                for r in &refs {
                    register(r);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_magic() {
        assert_eq!(sniff_mime_bytes(&[0x89, b'P', b'N', b'G']), Some("image/png".into()));
        assert_eq!(sniff_mime_bytes(&[0xFF, 0xD8, 0xFF]), Some("image/jpeg".into()));
        assert_eq!(sniff_mime_bytes(b"GIF89a...."), Some("image/gif".into()));
        assert_eq!(
            sniff_mime_bytes(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("image/webp".into())
        );
        assert_eq!(sniff_mime_bytes(b"BM...."), Some("image/bmp".into()));
        assert_eq!(sniff_mime_bytes(&[0; 4]), None);
    }

    #[test]
    fn ext_detection() {
        assert!(is_image_path("/tmp/a.PNG"));
        assert!(is_image_path("x/a.webp"));
        assert!(!is_image_path("/tmp/a.rs"));
        assert!(!is_image_path("noext"));
    }

    #[test]
    fn file_is_image_sniffs_extensionless() {
        // mobile photo pickers hand out opaque names (e.g.
        // "uploads/1790510420-1000006546"): no extension, so the
        // classification must fall back to magic bytes
        let dir = std::env::temp_dir().join("ranch-media-test");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("upload-png");
        std::fs::write(&png, [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]).unwrap();
        let txt = dir.join("upload-txt");
        std::fs::write(&txt, b"hello world").unwrap();
        let bin = dir.join("upload-bin");
        std::fs::write(&bin, [0x50, 0x4B, 0x03, 0x04, 0]).unwrap(); // zip magic: not an image
        assert!(file_is_image(png.to_str().unwrap()));
        assert!(!file_is_image(txt.to_str().unwrap()));
        assert!(!file_is_image(bin.to_str().unwrap()));
        assert!(!file_is_image("/definitely/missing/noext"));
        // extension still wins without needing the file to exist
        assert!(file_is_image("/definitely/missing/x.png"));
    }

    #[test]
    fn tool_args_refs() {
        let refs = image_refs_from_tool_args(r#"{"path":"/tmp/screenshot.png"}"#);
        assert_eq!(refs, vec!["/tmp/screenshot.png".to_string()]);
        let refs = image_refs_from_tool_args(r#"{"file_path":"/tmp/a.jpg","cmd":"x"}"#);
        assert_eq!(refs, vec!["/tmp/a.jpg".to_string()]);
        let refs = image_refs_from_tool_args(r#"{"path":"/tmp/a.rs"}"#);
        assert!(refs.is_empty());
        let refs = image_refs_from_tool_args("not json");
        assert!(refs.is_empty());
    }

    #[test]
    fn flag_tool_rows_with_images() {
        let mut msgs = vec![
            ChatMsg {
                seq: 1,
                role: "tool".into(),
                text: String::new(),
                tool_name: Some("read".into()),
                tool_call_id: None,
                tool_output: Some("img".into()),
                tool_args: Some(r#"{"path":"/tmp/s.png"}"#.into()),
                duration_ms: None,
                created_at: None,
                attachments: None,
                image_refs: None,
            },
            ChatMsg {
                seq: 2,
                role: "assistant".into(),
                text: "hi".into(),
                tool_name: None,
                tool_call_id: None,
                tool_output: None,
                tool_args: None,
                duration_ms: None,
                created_at: None,
                attachments: None,
                image_refs: None,
            },
        ];
        flag_image_refs(&mut msgs);
        assert_eq!(msgs[0].image_refs.as_deref(), Some([String::from("/tmp/s.png")].as_slice()));
        assert!(msgs[1].image_refs.is_none());
    }

    #[test]
    fn registry_eviction() {
        let mut g = Reg {
            paths: HashSet::new(),
            order: VecDeque::new(),
        };
        for i in 0..(CAP + 10) {
            register_into(&mut g, &format!("/tmp/img-{i}.png"));
        }
        assert_eq!(g.order.len(), CAP);
        assert!(!g.paths.contains("/tmp/img-0.png"));
        assert!(g.paths.contains(&format!("/tmp/img-{}.png", CAP + 9)));
        register_into(&mut g, "/tmp/dupe.png");
        assert_eq!(g.order.len(), CAP);
    }
}
