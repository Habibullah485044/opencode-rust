use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("Hello from the demo project!");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock went backwards")
        .as_secs();
    let out = Path::new("output.txt");
    File::create(out)
        .and_then(|mut f| f.write_all(format!("{}\n", now).as_bytes()))
        .expect("failed to write output.txt");
    println!("Wrote epoch seconds to output.txt");
}
