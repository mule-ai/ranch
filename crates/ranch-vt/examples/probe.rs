fn main() {
    match ranch_vt::Vt::new(80, 24) {
        Ok(vt) => {
            vt.write(b"hello probe\r\n");
            println!("screen: {:?}", vt.screen());
        }
        Err(e) => println!("ERR: {e}"),
    }
}
