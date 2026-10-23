//! Check a web page the way the agent does: `cargo run --example check_page -- <folder> [page] [expected text]`.
#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first() else {
        eprintln!("usage: check_page <folder> [page, default index.html] [text the page must show]...");
        std::process::exit(2);
    };
    let page = args.get(1).map(String::as_str).unwrap_or("index.html");
    let expect: Vec<String> = args.iter().skip(2).cloned().collect();
    match opencode_rust::pagecheck::check(std::path::Path::new(dir), page, &[375, 1440], &expect).await {
        Ok(r) => {
            println!("{}", r.render());
            std::process::exit(if r.score() == 0 { 0 } else { 1 });
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(2);
        }
    }
}
