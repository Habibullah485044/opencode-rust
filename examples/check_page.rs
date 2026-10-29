//! Check a web page the way the agent does: `cargo run --example check_page -- <folder> [page] [expected text]`.
//! The page is a file in the folder, or the localhost URL of a running app.
#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(dir) = args.first() else {
        eprintln!("usage: check_page <folder> [page or http://localhost:PORT/, default index.html] [text the page must show]...");
        std::process::exit(2);
    };
    let page = args.get(1).map(String::as_str).unwrap_or("index.html");
    let expect: Vec<String> = args.iter().skip(2).cloned().collect();
    let input = serde_json::json!({"path": page, "expect_text": expect});
    match opencode_rust::pagecheck::run_tool(std::path::Path::new(dir), &input).await.map(|(_, r)| r) {
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
