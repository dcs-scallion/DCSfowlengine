use std::fs::File;
use std::io::Read;

#[test]
fn inspect_save_csar_field() {
    let path = std::env::var("FOWL_SAVE").unwrap_or_else(|_| {
        r"C:\Users\Robo\Saved Games\DCS\Rust_Fowl_Engine_2.0_Caucasus1985-SARH".into()
    });
    let mut f = File::open(&path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let mut compressed = Vec::new();
    f.read_to_end(&mut compressed).unwrap();
    let json = zstd::decode_all(&compressed[..]).unwrap();
    let s = String::from_utf8_lossy(&json);
    println!("file: {path}");
    println!("csar_downed key count: {}", s.matches("\"csar_downed\"").count());
    for (i, pos) in s.match_indices("\"csar_downed\"").enumerate() {
        let end = (pos.0 + 1200).min(s.len());
        println!("--- entry {i} ---");
        println!("{}", &s[pos.0..end]);
    }
}
