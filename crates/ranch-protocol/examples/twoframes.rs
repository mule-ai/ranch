use ranch_protocol::{Decoder, Frame};
fn main() {
    let mut d = Decoder::new();
    let f1 = Frame::Attach { id: "a".into(), client: "c".into(), session: "s".into(), pane: None };
    let f2 = Frame::Detach { id: "b".into(), client: "c".into() };
    let l1 = serde_json::to_string(&f1).unwrap();
    let l2 = serde_json::to_string(&f2).unwrap();
    let bytes = format!("{l1}\n{l2}\n").into_bytes();
    let out = d.feed(&bytes);
    println!("fed 2 frames, got {} frames", out.len());
    for f in &out { println!("  {:?}", f); }
}
