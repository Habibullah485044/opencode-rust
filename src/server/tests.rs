use super::*;
use crate::todo::{TodoOp, TodoResult};

/// A model mock that answers request n with `replies[n]` (the last one
/// again after that): a `tool:<name>:<json args>` reply is a tool call,
/// anything else plain text. Returns the base URL and every request seen.
async fn scripted_llm(replies: Vec<&'static str>) -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
    use axum::{routing::post, Router};
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
    let seen2 = seen.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |axum::extract::Json(body): axum::extract::Json<Value>| {
            let (seen, replies) = (seen2.clone(), replies.clone());
            async move {
                let n = {
                    let mut s = seen.lock().unwrap();
                    s.push(body);
                    s.len() - 1
                };
                let reply = replies[n.min(replies.len() - 1)];
                let chunk = match reply.strip_prefix("tool:").and_then(|r| r.split_once(':')) {
                    Some((name, args)) => json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":format!("c{n}"),"function":{"name":name,"arguments":args}}]},"finish_reason":"tool_calls"}]}),
                    None => json!({"choices":[{"delta":{"content":reply},"finish_reason":"stop"}]}),
                };
                let payload = format!("data: {chunk}\n\ndata: [DONE]\n\n");
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}/v1"), seen)
}

/// Settings → System prompt: as the main prompt it replaces Habi's base
/// instructions (the tool rules stay); added, it comes last. Applied
/// from the next request, no restart.
#[tokio::test]
async fn the_users_own_prompt_is_the_main_one_or_comes_last() {
    let dir = std::env::temp_dir().join(format!("ocrs-user-prompt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let st = build_state_ephemeral(crate::config::Config::default(), dir.clone()).await;
    let s = st.db.create_session("p", None).unwrap();
    let habi = st.composed_system(&s.id);
    *st.user_prompt.write().unwrap() = (Some("You are Crumb's own coding assistant.".into()), None);
    let main = st.composed_system(&s.id);
    assert_eq!(
        main,
        format!("You are Crumb's own coding assistant.{}", crate::agents::TOOL_GUIDE),
        "only the user's text and the tool guide"
    );
    assert!(
        !main.contains("## Skills") && !main.contains("Ponytail") && !main.contains("lazy senior"),
        "no skills, no ponytail"
    );
    assert!(st.own_prompt_only());
    *st.user_prompt.write().unwrap() = (None, Some("Always answer in Urdu.".into()));
    let added = st.composed_system(&s.id);
    assert!(added.starts_with(&habi[..60]), "Habi's own instructions first");
    assert!(added.ends_with("## The user's own instructions\n\nAlways answer in Urdu."), "yours last");
    let _ = std::fs::remove_dir_all(&dir);
}

/// With the user's own main prompt, Habi makes no extra model calls of
/// its own around their request: no spec (so no final check) and no
/// design review. Seen: the user pasted their prompt and still saw a
/// "Spec" being worked out first.
#[tokio::test]
async fn the_users_own_prompt_turns_off_habis_extra_steps() {
    let (base, seen) = scripted_llm(vec!["KIND: task\nGOAL: x\nOUTPUT: y\nDONE WHEN:\n- z"]).await;
    let dir = std::env::temp_dir().join(format!("ocrs-own-prompt-steps-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let since = std::time::SystemTime::now() - std::time::Duration::from_secs(1);
    std::fs::write(dir.join("index.html"), "<h1>Hi</h1>").unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = base;
    cfg.provider.model = "mock".into();
    cfg.provider.images = "on".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("p", None).unwrap();
    *st.user_prompt.write().unwrap() = (Some("You are my assistant.".into()), None);
    let none = AtomicBool::new(false);
    assert!(make_spec(&st, &s.id, "build a website for my bakery please", &none).await.is_none());
    assert!(design_review(&st, &s.id, "build a website", None, since, true, &none, None).await.is_none());
    assert!(seen.lock().unwrap().is_empty(), "no model call of Habi's own");
    *st.user_prompt.write().unwrap() = (None, None);
    assert!(
        make_spec(&st, &s.id, "build a website for my bakery please", &none).await.is_some(),
        "Habi's own mode plans as before"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A skill file stays in full while the model works from it, then
/// becomes a note naming it; other reads and failed reads are untouched.
#[test]
fn skill_files_read_earlier_are_not_resent_forever() {
    let call = |id: &str, name: &str, path: &str| ChatMsg {
        role: "assistant".into(),
        content: None,
        tool_calls: Some(vec![crate::provider::ToolCallReq {
            id: id.into(),
            kind: "function".into(),
            function: crate::provider::FunctionCall { name: name.into(), arguments: json!({"path": path}).to_string() },
        }]),
        tool_call_id: None,
    };
    let result = |id: &str, text: &str| ChatMsg {
        role: "tool".into(),
        content: Some(text.into()),
        tool_calls: None,
        tool_call_id: Some(id.into()),
    };
    let skill_text = "# Frontend design\n".repeat(400);
    let mut msgs = vec![
        ChatMsg { role: "user".into(), content: Some("build a page".into()), tool_calls: None, tool_call_id: None },
        call("s1", "read", r"C:\Users\x\.config\opencode-rust\skills\frontend-design\SKILL.md"),
        result("s1", &skill_text),
        call("r1", "read", "index.html"),
        result("r1", "<html>"),
        call("s2", "read", ".opencode/skills/missing/SKILL.md"),
        result("s2", "[error]\nnot found"),
        call("w1", "write", "index.html"),
        result("w1", "ok"),
    ];
    // One step after the read (reading index.html): still in full.
    let mut early = msgs[..5].to_vec();
    retire_skill_reads(&mut early);
    assert_eq!(early[2].content.as_deref(), Some(skill_text.as_str()), "kept while the model works from it");
    msgs.push(call("w2", "write", "style.css"));
    msgs.push(result("w2", "ok"));
    retire_skill_reads(&mut msgs);
    let note = msgs[2].content.as_deref().unwrap();
    assert!(note.contains("frontend-design") && note.contains("read it again") && note.len() < 300, "{note}");
    assert_eq!(msgs[4].content.as_deref(), Some("<html>"), "a project file stays");
    assert!(msgs[6].content.as_deref().unwrap().starts_with("[error]"), "a failed read stays");
}

/// Seen live: a mission step checked its work with tools, then wrote a
/// report without PASS/FAIL lines, and the whole step was redone. The
/// final check now asks once for the lines and keeps the evidence.
#[tokio::test]
async fn a_final_check_without_verdict_lines_is_asked_for_them() {
    let (base, seen) = scripted_llm(vec![
        r#"tool:list:{"path":"."}"#,
        "I looked at everything and it all works.",
        "PASS: index.html exists — the listing shows it",
    ])
    .await;
    let dir = std::env::temp_dir().join(format!("ocrs-final-lines-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("index.html"), "<p>hi</p>").unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = base;
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("check", None).unwrap();
    let spec = crate::spec::Spec {
        goal: "a page".into(),
        output: "index.html".into(),
        done_when: vec!["index.html exists".into()],
        assuming: vec![],
        design: None,
        skills: vec![],
    };
    let passed = final_check(&st, &s.id, &spec, &AtomicBool::new(false), None).await;
    assert_eq!(passed, Some(true), "the lines it was asked for count, with the evidence it gathered");
    let asked = seen.lock().unwrap().last().unwrap().to_string();
    assert!(asked.contains("had no PASS/FAIL lines"), "the third request asks for the lines");
    assert_eq!(seen.lock().unwrap().len(), 3, "one short extra reply, not a redo");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The design review shows the model screenshots of the page it changed,
/// as images, with the AI look to avoid; a model that takes no images
/// is not asked to judge pictures.
#[tokio::test]
async fn the_design_review_shows_the_model_its_page() {
    if crate::pagecheck::find_browser().is_none() {
        return;
    }
    let (base, seen) = scripted_llm(vec!["Changed the hero to a split layout.\nSEEN: italic word"]).await;
    let dir = std::env::temp_dir().join(format!("ocrs-design-review-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let since = std::time::SystemTime::now() - std::time::Duration::from_secs(1);
    std::fs::write(dir.join("index.html"), "<!doctype html><title>Crumb</title><h1>A little <em>happiness</em></h1>")
        .unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = base;
    cfg.provider.model = "mock".into();
    cfg.provider.images = "off".into();
    let st = build_state_ephemeral(cfg.clone(), dir.clone()).await;
    let s = st.db.create_session("site", None).unwrap();
    let none = design_review(&st, &s.id, "a bakery site", None, since, true, &AtomicBool::new(false), None).await;
    assert!(none.is_none() && seen.lock().unwrap().is_empty(), "no images, no review");

    cfg.provider.images = "on".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("site", None).unwrap();
    let done = design_review(&st, &s.id, "a bakery site", None, since, true, &AtomicBool::new(false), None).await;
    assert!(done.is_some());
    let req = seen.lock().unwrap().last().unwrap().to_string();
    assert!(req.contains("[design review]") && req.contains("coloured italics"), "the brief and the AI look");
    assert_eq!(req.matches("data:image/png;base64").count(), 2, "desktop and phone pictures reach the model");
    let usage = st.db.usage_log(0).unwrap();
    assert!(usage.iter().all(|e| e.purpose == "design review"), "counted as what it was: {usage:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Projects: a chat filed under another folder works there: its tools
/// write into that folder and its AGENTS.md reaches the prompt.
#[tokio::test]
async fn a_chat_runs_in_its_projects_folder() {
    use axum::{routing::post, Router};
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
    let seen2 = seen.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |axum::extract::Json(body): axum::extract::Json<Value>| {
            let seen = seen2.clone();
            async move {
                let n = {
                    let mut s = seen.lock().unwrap();
                    s.push(body);
                    s.len()
                };
                let first = if n == 1 {
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"w1","function":{"name":"write","arguments":"{\"path\":\"made.txt\",\"content\":\"hi\\n\"}"}}]},"finish_reason":"tool_calls"}]}"#
                } else {
                    r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}"#
                };
                let payload = format!("data: {first}\n\ndata: [DONE]\n\n");
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let base = std::env::temp_dir().join(format!("ocrs-projects-{}", std::process::id()));
    let (start, other) = (base.join("start"), base.join("other"));
    std::fs::create_dir_all(&start).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("AGENTS.md"), "OTHER-PROJECT-RULES\n").unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    cfg.agent.auto_review = false;
    let st = build_state_ephemeral(cfg, start.clone()).await;
    st.set_perm_mode(PermissionMode::Turbo);
    let project = st.db.upsert_project(&other).unwrap();
    let s = st.db.create_session_in("in other", None, Some(&project.id)).unwrap();
    assert_eq!(st.workspace_for(&s.id), other);

    run_agent(&st, &s.id, "make a file", Arc::new(AtomicBool::new(false))).await;

    assert_eq!(std::fs::read_to_string(other.join("made.txt")).unwrap_or_default(), "hi\n");
    assert!(!start.join("made.txt").exists(), "nothing lands in the start folder");
    let bodies = seen.lock().unwrap().clone();
    let system = bodies[0]["messages"][0]["content"].as_str().unwrap_or("");
    assert!(system.contains("OTHER-PROJECT-RULES"), "the project's AGENTS.md is in the prompt");
    // A chat whose project folder is gone falls back to the start folder.
    let gone = st.db.upsert_project(&base.join("missing")).unwrap();
    let g = st.db.create_session_in("gone", None, Some(&gone.id)).unwrap();
    assert_eq!(st.workspace_for(&g.id), start);
    let _ = std::fs::remove_dir_all(&base);
}

/// Prompt caching works only when each request starts with the previous
/// request unchanged. Over a tool call and a second prompt, check that,
/// that usage is asked for and saved, and that `/usage` shows the share.
#[tokio::test]
async fn requests_keep_a_stable_prefix_and_usage_is_recorded() {
    use axum::{routing::post, Router};
    let seen: Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
    let seen2 = seen.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |axum::extract::Json(body): axum::extract::Json<Value>| {
            let seen = seen2.clone();
            async move {
                let n = {
                    let mut s = seen.lock().unwrap();
                    s.push(body);
                    s.len() as u64
                };
                // Request 1 calls a tool; the others answer with text.
                let first = if n == 1 {
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"todo","arguments":"{\"op\":\"list\"}"}}]},"finish_reason":"tool_calls"}]}"#.to_string()
                } else {
                    r#"{"choices":[{"delta":{"content":"done"},"finish_reason":"stop"}]}"#.to_string()
                };
                let usage = json!({"choices": [], "usage": {
                    "prompt_tokens": 1000 * n, "completion_tokens": 5,
                    "prompt_tokens_details": {"cached_tokens": 1000 * (n - 1)}}});
                let payload = format!("data: {first}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let dir = std::env::temp_dir().join(format!("ocrs-cache-prefix-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    st.set_perm_mode(PermissionMode::Turbo);
    let s = st.db.create_session("cache", None).unwrap();
    run_agent(&st, &s.id, "first", Arc::new(AtomicBool::new(false))).await;
    run_agent(&st, &s.id, "second", Arc::new(AtomicBool::new(false))).await;

    let bodies = seen.lock().unwrap().clone();
    assert_eq!(bodies.len(), 3, "tool call, its follow-up, the second prompt");
    for (i, w) in bodies.windows(2).enumerate() {
        let (a, b) = (w[0]["messages"].as_array().unwrap(), w[1]["messages"].as_array().unwrap());
        assert_eq!(&b[..a.len()], &a[..], "request {} must start with request {} unchanged", i + 2, i + 1);
        assert_eq!(w[0]["tools"], w[1]["tools"], "the tool list never changes");
    }
    assert_eq!(bodies[0]["stream_options"]["include_usage"], true);
    assert!(!bodies[0].to_string().contains("cache_control"), "no markers for a non-Claude model");
    let t = st.db.usage_totals(Some(&s.id), None).unwrap();
    assert_eq!((t.requests, t.input, t.cached, t.output), (3, 6000, 3000, 15));
    assert!(st.usage_report(&s.id).contains("3.0K (50%)"), "{}", st.usage_report(&s.id));
    let _ = std::fs::remove_dir_all(&dir);
}

/// F-12: a server with the token "t" and a client for it.
async fn f12_server() -> (Shared, String, reqwest::Client) {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let addr = serve_with_token(st.clone(), 0, "t".into()).await.unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    (st, format!("http://{addr}"), http)
}

/// F-12: every new route needs the token.
#[tokio::test]
async fn f12_routes_need_the_token() {
    let (st, base, http) = f12_server().await;
    let s = st.db.create_session("x", None).unwrap();
    let routes = [
        (reqwest::Method::GET, format!("/api/sessions/{}/mode", s.id)),
        (reqwest::Method::PUT, format!("/api/sessions/{}/mode", s.id)),
        (reqwest::Method::GET, "/api/model".to_string()),
        (reqwest::Method::PUT, "/api/model".to_string()),
        (reqwest::Method::GET, "/api/permission-mode".to_string()),
        (reqwest::Method::PUT, "/api/permission-mode".to_string()),
        (reqwest::Method::GET, "/api/approvals".to_string()),
        (reqwest::Method::PATCH, format!("/api/sessions/{}", s.id)),
        (reqwest::Method::DELETE, format!("/api/sessions/{}", s.id)),
    ];
    for (m, p) in routes {
        let r = http.request(m.clone(), format!("{base}{p}")).send().await.unwrap();
        assert_eq!(r.status().as_u16(), 401, "{m} {p} without a token");
    }
    assert!(st.db.get_session(&s.id).unwrap().is_some(), "nothing was deleted");
}

/// F-12: session mode, model and permission mode, good and bad requests.
#[tokio::test]
async fn f12_mode_model_and_permission_routes() {
    let (st, base, http) = f12_server().await;
    let s = st.db.create_session("x", None).unwrap();
    let mode_url = format!("{base}/api/sessions/{}/mode", s.id);

    let r: Value = http.get(&mode_url).bearer_auth("t").send().await.unwrap().json().await.unwrap();
    assert_eq!(r["mode"], "build");
    let r = http.put(&mode_url).bearer_auth("t").json(&json!({"mode": "plan"})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(st.mode_for(&s.id), AgentMode::Plan);
    let r = http.put(&mode_url).bearer_auth("t").json(&json!({"mode": "fly"})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400, "unknown mode");
    let r = http.get(format!("{base}/api/sessions/ses_nope/mode")).bearer_auth("t").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 404, "unknown session");

    let r = http.put(format!("{base}/api/model")).bearer_auth("t").json(&json!({"model": "m2"})).send().await.unwrap();
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["current"], "m2");
    let v: Value = http.get(format!("{base}/api/model")).bearer_auth("t").send().await.unwrap().json().await.unwrap();
    assert_eq!(v["current"], "m2");
    assert!(v["available"].is_array());
    let r = http.put(format!("{base}/api/model")).bearer_auth("t").json(&json!({"model": " "})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400, "empty model id");

    let perm_url = format!("{base}/api/permission-mode");
    let v: Value = http.get(&perm_url).bearer_auth("t").send().await.unwrap().json().await.unwrap();
    assert_eq!(v["mode"], "default");
    let v: Value = http
        .put(&perm_url)
        .bearer_auth("t")
        .json(&json!({"mode": "manual"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["mode"], "manual");
    assert!(v["released"].as_array().unwrap().is_empty());
    assert_eq!(st.perm_mode(), PermissionMode::Manual);
    let r = http.put(&perm_url).bearer_auth("t").json(&json!({"mode": "yolo"})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400, "unknown permission mode");

    let v: Value =
        http.get(format!("{base}/api/approvals")).bearer_auth("t").send().await.unwrap().json().await.unwrap();
    assert_eq!(v, json!([]), "nothing waits");
}

/// F-12: rename and delete; DELETE stops a running run before it deletes.
#[tokio::test]
async fn f12_rename_and_delete_sessions() {
    let (st, base, http) = f12_server().await;
    let s = st.db.create_session("old", None).unwrap();
    st.db.add_message(&s.id, "user", &[Part::Text { text: "hi".into() }]).unwrap();
    let url = format!("{base}/api/sessions/{}", s.id);

    let v: Value =
        http.patch(&url).bearer_auth("t").json(&json!({"title": "new"})).send().await.unwrap().json().await.unwrap();
    assert_eq!(v["title"], "new");
    let r = http.patch(&url).bearer_auth("t").json(&json!({"title": "  "})).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 400, "empty title");
    let r = http
        .patch(format!("{base}/api/sessions/ses_nope"))
        .bearer_auth("t")
        .json(&json!({"title": "x"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 404);

    // A run that stops only when aborted.
    let stopped = Arc::new(AtomicBool::new(false));
    let seen = stopped.clone();
    st.kick_task(s.id.clone(), move |_st, _sid, abort| async move {
        while !abort.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        seen.store(true, Ordering::SeqCst);
    })
    .await;
    let r = http.delete(&url).bearer_auth("t").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 204);
    assert!(stopped.load(Ordering::SeqCst), "the run ended before the delete");
    assert!(st.db.get_session(&s.id).unwrap().is_none(), "session gone");
    assert!(st.db.messages(&s.id).unwrap().is_empty(), "messages gone");
    let r = http.delete(&url).bearer_auth("t").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 404, "already deleted");
}

/// T-028: deleting a chat through the route deletes its todos too.
#[tokio::test]
async fn deleting_a_chat_deletes_its_todos() {
    let (st, base, http) = f12_server().await;
    let s = st.db.create_session("todos", None).unwrap();
    st.todo_store.exec(&s.id, TodoOp::Add { content: "write tests".into() });
    assert_eq!(st.todo_store.list_all(&s.id).len(), 1);
    let r = http.delete(format!("{base}/api/sessions/{}", s.id)).bearer_auth("t").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 204);
    assert!(st.todo_store.list_all(&s.id).is_empty(), "the chat's todos went with it");
}

/// T-051: a refused request says why in a JSON body.
#[tokio::test]
async fn api_errors_say_why() {
    let (st, base, http) = f12_server().await;
    let r = http.get(format!("{base}/api/sessions/ses_nope/messages")).bearer_auth("t").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 404);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v, json!({"error": "session not found"}));

    // A run that ignores the abort until the test lets it go.
    let s = st.db.create_session("stuck", None).unwrap();
    let release = Arc::new(AtomicBool::new(false));
    let held = release.clone();
    st.kick_task(s.id.clone(), move |_st, _sid, _abort| async move {
        while !held.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    let r = http.delete(format!("{base}/api/sessions/{}", s.id)).bearer_auth("t").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 409);
    let v: Value = r.json().await.unwrap();
    assert!(v["error"].as_str().unwrap().contains("did not stop within"), "{v}");
    release.store(true, Ordering::SeqCst);
    assert!(st.db.get_session(&s.id).unwrap().is_some(), "not deleted");
}

/// T-011: a chat's folder is read from the database once; `forget_session`
/// and `forget_workspaces` drop the cached answer.
#[tokio::test]
async fn a_chats_folder_is_looked_up_once() {
    let base = std::env::temp_dir().join(format!("ocrs-ws-cache-{}", std::process::id()));
    let (start, other) = (base.join("start"), base.join("other"));
    std::fs::create_dir_all(&start).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let st = build_state_ephemeral(crate::config::Config::default(), start.clone()).await;
    let project = st.db.upsert_project(&other).unwrap();
    let s = st.db.create_session_in("cached", None, Some(&project.id)).unwrap();
    assert_eq!(st.workspace_for(&s.id), other);
    // Changed behind the cache's back: the second lookup does not see it,
    // so it did not ask the database.
    st.db.set_session_project(&s.id, None).unwrap();
    assert_eq!(st.workspace_for(&s.id), other, "answered from the cache");
    st.forget_session(&s.id);
    assert!(!st.workspaces.lock().unwrap().contains_key(&s.id), "forget_session drops the entry");
    assert_eq!(st.workspace_for(&s.id), start, "read again after forgetting");
    st.db.set_session_project(&s.id, Some(&project.id)).unwrap();
    st.forget_workspaces();
    assert_eq!(st.workspace_for(&s.id), other);
    let _ = std::fs::remove_dir_all(&base);
}

/// T-055: past the size limit the log moves to `.1` and a new one starts.
#[test]
fn the_log_rotates_past_its_size_limit() {
    let dir = std::env::temp_dir().join(format!("ocrs-log-rotate-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("core.log");
    let mut slot = None;
    for i in 0..5 {
        append_log(&mut slot, &path, 30, &format!("line {i} 0123456789"));
    }
    drop(slot);
    let old = std::fs::read_to_string(dir.join("core.log.1")).unwrap();
    let new = std::fs::read_to_string(&path).unwrap();
    assert!(old.contains("line 3") && !old.contains("line 0"), "the older .1 was replaced: {old}");
    assert_eq!(new.trim(), "line 4 0123456789", "a new log started");
    let _ = std::fs::remove_dir_all(&dir);
}

/// T-058: a failed "always allow" save is written to core.log.
#[tokio::test]
async fn a_failed_always_allow_is_logged() {
    let dir = std::env::temp_dir().join(format!("ocrs-always-fail-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("broken.db");
    let db = crate::db::Db::open(&path).unwrap();
    // Force every approval save to fail.
    rusqlite::Connection::open(&path).unwrap().execute_batch("DROP TABLE approvals").unwrap();
    let st = build_state_ephemeral_db(crate::config::Config::default(), dir.clone(), dir.join("skills"), db).await;
    let (tx, rx) = oneshot::channel();
    let detail = format!("always-fail-{}", std::process::id());
    let request = ServerEvent::ApprovalNeeded {
        session_id: "s".into(),
        approval_id: "apr_x".into(),
        tool: "bash".into(),
        detail: detail.clone(),
        preview: String::new(),
    };
    st.approvals
        .lock()
        .unwrap()
        .insert("apr_x".into(), ApprovalEntry { tx, tool: "bash".into(), detail: detail.clone(), request });
    assert!(st.decide("apr_x", true, true));
    assert!(rx.await.unwrap().allowed, "the call itself is still allowed");
    let log = std::fs::read_to_string(core_log_path()).unwrap_or_default();
    assert!(
        log.lines().any(|l| l.contains("saving \"always allow\" for bash") && l.contains(&detail)),
        "core.log has the failure"
    );
    drop(st);
    let _ = std::fs::remove_dir_all(&dir);
}

/// T-094: `forget_session` clears every per-chat map.
#[tokio::test]
async fn forget_session_clears_every_per_chat_map() {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let parent = st.db.create_session("parent", None).unwrap();
    let s = st.db.create_session("child", Some(&parent.id)).unwrap();
    let id = s.id.clone();
    st.stats.lock().unwrap().insert(id.clone(), RunStats::default());
    st.set_mode(&id, AgentMode::Plan);
    st.set_ponytail_level(&id, agents::PonytailLevel::Off);
    std::mem::forget(st.auto_approve_session(&id));
    FROZEN_LESSONS.get_or_init(Default::default).lock().unwrap().insert(id.clone(), None);
    COMPACT_FAILED_AT.get_or_init(Default::default).lock().unwrap().insert(id.clone(), 3);
    let _ = st.workspace_for(&id);
    st.load_parents(&id);
    assert!(st.workspaces.lock().unwrap().contains_key(&id));
    assert_eq!(st.parents.lock().unwrap().get(&id), Some(&Some(parent.id.clone())));

    st.forget_session(&id);
    assert!(!st.stats.lock().unwrap().contains_key(&id), "stats");
    assert!(!st.modes.lock().unwrap().contains_key(&id), "modes");
    assert!(!st.ponytail_levels.lock().unwrap().contains_key(&id), "ponytail levels");
    assert!(!st.auto_approve.lock().unwrap().contains_key(&id), "auto_approve");
    assert!(!FROZEN_LESSONS.get().unwrap().lock().unwrap().contains_key(&id), "FROZEN_LESSONS");
    assert!(!COMPACT_FAILED_AT.get().unwrap().lock().unwrap().contains_key(&id), "COMPACT_FAILED_AT");
    assert!(!st.workspaces.lock().unwrap().contains_key(&id), "workspaces");
    assert!(!st.parents.lock().unwrap().contains_key(&id), "parents");
}

/// P-04: common names, methods and mere mentions give no reuse hint.
#[test]
fn reuse_gate_ignores_common_names_and_mentions() {
    let dir = std::env::temp_dir().join(format!("ocrs-p04-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("other.rs"), "// calls render_page() here\nfn main() {}\n").unwrap();
    let write = serde_json::json!({
        "path": "new.rs",
        "content": "fn main() {}\nfn render_page() {}\nimpl X {\n    fn helper_method() {}\n}\n"
    });
    assert!(reuse_gate(&dir, "write", &write).is_none(), "main is common; render_page is only mentioned");
    assert!(top_level_symbols("impl X {\n    fn helper_method() {}\n}\n").is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// S-16: exports stay inside the workspace and never replace a file that
/// is not an earlier export (unless --force).
#[tokio::test]
async fn export_stays_inside_and_does_not_clobber() {
    let dir = std::env::temp_dir().join(format!("ocrs-s16-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let st = build_state_ephemeral(crate::config::Config::default(), dir.clone()).await;
    let s = st.db.create_session("exp", None).unwrap();

    let first = st.export_transcript(&s.id, None).unwrap();
    assert!(first.starts_with(&dir), "{}", first.display());
    st.export_transcript(&s.id, None).expect("an earlier export may be replaced");
    assert!(st.export_transcript(&s.id, Some("../outside.md")).is_err(), "no `..`");
    let outside = std::env::temp_dir().join("ocrs-s16-outside.md");
    assert!(
        st.export_transcript(&s.id, Some(outside.to_str().unwrap())).is_err(),
        "no absolute path outside the workspace"
    );

    std::fs::write(dir.join("notes.md"), "keep me").unwrap();
    assert!(st.export_transcript(&s.id, Some("notes.md")).is_err(), "a foreign file is kept");
    assert_eq!(std::fs::read_to_string(dir.join("notes.md")).unwrap(), "keep me");
    st.export_transcript(&s.id, Some("--force notes.md")).unwrap();
    let replaced = std::fs::read_to_string(dir.join("notes.md")).unwrap();
    assert!(replaced.starts_with(AppState::EXPORT_MARKER));

    let nested = st.export_transcript(&s.id, Some("docs/run.md")).unwrap();
    assert!(nested.is_file(), "missing folders are created");
    let _ = std::fs::remove_dir_all(&dir);
}

/// P-03: an aborted run gets no automatic review, even after a write.
#[tokio::test]
async fn aborted_run_gets_no_review() {
    use axum::{routing::post, Router};
    use std::sync::atomic::AtomicUsize;
    let dir = std::env::temp_dir().join(format!("ocrs-p03-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let abort = Arc::new(AtomicBool::new(false));
    let calls = Arc::new(AtomicUsize::new(0));
    let (abort_m, calls_m) = (abort.clone(), calls.clone());
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: axum::extract::Json<Value>| {
            let (abort, calls) = (abort_m.clone(), calls_m.clone());
            async move {
                let payload = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_w1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"made.txt\\\",\\\"content\\\":\\\"hello\\\\n\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                        "data: [DONE]\n\n",
                    )
                } else {
                    // The user presses stop while the model answers.
                    abort.store(true, Ordering::SeqCst);
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    "data: {\"choices\":[{\"delta\":{\"content\":\"late\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                };
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    cfg.permissions.defaults.insert("write".into(), "allow".into());
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("p03", None).unwrap();
    let mut rx = st.events.subscribe();

    run_agent(&st, &s.id, "make a file", abort.clone()).await;

    assert!(dir.join("made.txt").exists(), "the write happened before the abort");
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Note { text, .. } = ev {
            assert!(!text.contains("ponytail review"), "no review after an abort: {text}");
        }
    }
    let review_prompt = st.db.messages(&s.id).unwrap().iter().any(|m| {
        m.role == "user"
            && m.parts
                .iter()
                .any(|p| matches!(p, Part::Text { text } if text.contains("Review the current uncommitted changes")))
    });
    assert!(!review_prompt, "no dangling review prompt");
    let _ = std::fs::remove_dir_all(&dir);
}

/// P-02 helper: a git repo with one commit, and a mock that asks for one
/// `bash` call (`command`) and then answers with text. Returns the state.
async fn p02_state(tag: &str, command: &'static str) -> (Shared, std::path::PathBuf) {
    use axum::{routing::post, Router};
    let dir = std::env::temp_dir().join(format!("ocrs-p02-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| crate::gitsnap::git(&dir, args).unwrap();
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("hello.txt"), "hi\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::extract::Json<Value>| async move {
            let msgs = body["messages"].as_array().cloned().unwrap_or_default();
            let last_role = msgs.last().and_then(|m| m["role"].as_str()).unwrap_or("");
            let payload = if last_role != "tool" {
                let args = serde_json::json!({ "command": command }).to_string();
                let chunk = serde_json::json!({"choices":[{"delta":{"tool_calls":[
                    {"index":0,"id":"b1","function":{"name":"bash","arguments":args}}
                ]},"finish_reason":null}]});
                format!("data: {chunk}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n")
            } else {
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string()
            };
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    cfg.permissions.defaults.insert("bash".into(), "allow".into());
    (build_state_ephemeral(cfg, dir.clone()).await, dir)
}

/// P-02: did the run announce the automatic review?
async fn p02_reviewed(st: &Shared) -> bool {
    let s = st.db.create_session("p02", None).unwrap();
    let mut rx = st.events.subscribe();
    run_agent(st, &s.id, "do it", Arc::new(AtomicBool::new(false))).await;
    let mut reviewed = false;
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Note { text, .. } = ev {
            reviewed |= text.contains("ponytail review");
        }
    }
    reviewed
}

/// P-02: a read-only bash command gets no review.
#[tokio::test]
async fn read_only_bash_gets_no_review() {
    let (st, dir) = p02_state("ro", "git status").await;
    assert!(!p02_reviewed(&st).await, "nothing changed, so no review");
    let _ = std::fs::remove_dir_all(&dir);
}

/// P-02: a bash command that changes files still gets the review.
/// (`git init sub` creates new files without a `>` redirect: after S-08,
/// a redirect into a file asks for approval even when bash is allowed,
/// and nobody answers in a test.)
#[tokio::test]
async fn changing_bash_gets_the_review() {
    let (st, dir) = p02_state("rw", "git init sub").await;
    assert!(p02_reviewed(&st).await, "new files appeared, so review it");
    let _ = std::fs::remove_dir_all(&dir);
}

/// P-01: a refused call adds nothing to the Laziness Meter (and so
/// triggers no automatic review).
#[tokio::test]
async fn refused_write_is_not_counted() {
    use axum::{routing::post, Router};
    use std::sync::atomic::AtomicUsize;
    let dir = std::env::temp_dir().join(format!("ocrs-p01-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: axum::extract::Json<Value>| {
            let calls = calls2.clone();
            async move {
                let payload = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    concat!(
                        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_w1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"made.txt\\\",\\\"content\\\":\\\"hello\\\\n\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                        "data: [DONE]\n\n",
                    )
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                };
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    cfg.permissions.rules.push(crate::config::RuleCfg {
        tool: "write".into(),
        pattern: "*".into(),
        action: "deny".into(),
    });
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("p01", None).unwrap();

    run_agent(&st, &s.id, "make a file", Arc::new(AtomicBool::new(false))).await;

    assert!(!dir.join("made.txt").exists(), "the write was refused");
    let debt = st.debt_report(&s.id);
    assert!(debt.contains("+0"), "{debt}");
    assert!(debt.contains("0 write(s)"), "{debt}");
    assert_eq!(calls.load(Ordering::SeqCst), 2, "no review run was started");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A run gets a scratch folder git ignores, and the project is left as
/// it was when the run ends — also when the run fails.
#[tokio::test]
async fn the_scratch_folder_is_gone_after_a_run() {
    let dir = std::env::temp_dir().join(format!("ocrs-scratch-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let made = prepare_scratch(&dir);
    assert_eq!(std::fs::read_to_string(made.0.join(".gitignore")).unwrap(), "*\n");
    std::fs::write(made.0.join("probe.py"), "print(1)").unwrap();
    clear_scratch(&made);
    assert!(!dir.join(".opencode").exists(), "a .opencode made for scratch goes too");

    std::fs::create_dir_all(dir.join(".opencode").join("agents")).unwrap();
    let made = prepare_scratch(&dir);
    clear_scratch(&made);
    assert!(dir.join(".opencode").join("agents").exists(), "the project's own .opencode stays");
    assert!(!scratch_dir(&dir).exists());

    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = "http://127.0.0.1:9".into(); // the run fails
    cfg.provider.max_retries = 0;
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("s", None).unwrap();
    run_agent(&st, &s.id, "hi", Arc::new(AtomicBool::new(false))).await;
    assert!(!scratch_dir(&dir).exists(), "cleared after a failed run");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rewind_matches_shown_and_stored_text() {
    assert_eq!(rewind_key("\u{203a} make it blue"), "make it blue");
    assert_eq!(rewind_key("/mission ship it"), rewind_key("[mission] ship it"));
    assert_eq!(rewind_key("/deep 3 fix it"), rewind_key("[deep mode, 3 attempts] fix it"));
    assert_eq!(rewind_key("/deep fix it"), "fix it");
}

/// Rewinding a message puts the files back to before its run and drops
/// it and what came after; rewinding further goes further back.
#[tokio::test]
async fn rewind_puts_files_and_chat_back() {
    let dir = std::env::temp_dir().join(format!("ocrs-rewind-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| crate::gitsnap::git(&dir, args).unwrap();
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "core.autocrlf", "false"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);
    let st = build_state_ephemeral(crate::config::Config::default(), dir.clone()).await;
    let s = st.db.create_session("r", None).unwrap();
    let pause = || std::thread::sleep(std::time::Duration::from_millis(15));
    // Two runs, each as run_agent does it: snapshot, message, change, reply.
    for (ask, content) in [("first", "two\n"), ("second", "three\n")] {
        pause();
        crate::gitsnap::record_before_run(&dir, &st.state_dir, "pre-run").unwrap();
        pause();
        st.db.add_message(&s.id, "user", &[Part::Text { text: ask.into() }]).unwrap();
        std::fs::write(dir.join("a.txt"), content).unwrap();
        st.db.add_message(&s.id, "assistant", &[Part::Text { text: "done".into() }]).unwrap();
    }
    let r = rewind_chat(&st, &s.id, "\u{203a} second", 0).unwrap();
    assert_eq!((r.messages, r.undone), (2, 1));
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "two\n", "the second run is undone");
    assert_eq!(st.db.messages(&s.id).unwrap().len(), 2, "the first exchange stays");
    let r = rewind_chat(&st, &s.id, "first", 0).unwrap();
    assert_eq!((r.messages, r.undone), (2, 1));
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n", "back to before the first run");
    assert!(st.db.messages(&s.id).unwrap().is_empty());
    assert!(rewind_chat(&st, &s.id, "first", 0).is_err(), "nothing left to rewind");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn prefix_change_says_where_a_request_stopped_matching() {
    let m = |role: &str, text: &str| ChatMsg {
        role: role.into(),
        content: Some(text.into()),
        tool_calls: None,
        tool_call_id: None,
    };
    let sid = format!("prefix-{}", std::process::id());
    let tools = tools::tool_specs();
    let a = vec![m("system", "s"), m("user", "hi")];
    assert_eq!(prefix_change(&sid, &a, &tools), "first request");
    let b = vec![m("system", "s"), m("user", "hi"), m("assistant", "yo"), m("user", "more")];
    assert_eq!(prefix_change(&sid, &b, &tools), "same start", "only new messages at the end");
    let c = vec![m("system", "s"), m("user", "hi"), m("assistant", "CHANGED"), m("user", "more")];
    assert_eq!(prefix_change(&sid, &c, &tools), "message 2 (assistant) changed");
    let d = vec![m("system", "s2"), m("user", "hi")];
    assert_eq!(prefix_change(&sid, &d, &tools), "system prompt changed");
    assert_eq!(prefix_change(&sid, &d, &tools[..2]), "tool list changed");
}

#[test]
fn other_agents_tool_names_map_to_ours() {
    let (t, a) = tool_alias("list_files", &json!({})).unwrap();
    assert_eq!((t, a), ("list", json!({"path": "."})));
    let (t, a) = tool_alias("Bash", &json!({"command": "ls -la", "description": "x"})).unwrap();
    assert_eq!(t, "bash");
    assert_eq!(a["command"], "ls -la");
    let (t, a) = tool_alias("read_file", &json!({"file_path": "src/a.rs"})).unwrap();
    assert_eq!((t, a), ("read", json!({"path": "src/a.rs"})));
    let (t, a) = tool_alias("str_replace", &json!({"path": "a", "old_string": "x", "new_string": "y"})).unwrap();
    assert_eq!((t, a), ("edit", json!({"path": "a", "old_str": "x", "new_str": "y"})));
    assert_eq!(tool_alias("Read", &json!({"file_path": "a"})).unwrap().0, "read");
    assert!(tool_alias("launch_rockets", &json!({})).is_none());
}

#[test]
fn a_model_that_forgets_its_tools_is_spotted() {
    assert!(denies_file_access("I can\u{2019}t see or read your local folder unless you share its contents with me. Please paste the folder structure."));
    assert!(denies_file_access("I don't have access to your file system. Could you upload the files?"));
    assert!(!denies_file_access("I read the folder: it has 3 files, main.rs, lib.rs and Cargo.toml."));
    assert!(!denies_file_access("I can't access the network from here, so I could not download the page."));
}

#[test]
fn the_context_window_is_read_from_models() {
    let ollama = json!({"object":"list","data":[{"id":"qwen3.8","object":"model","context_window":16384}]});
    assert_eq!(context_from_models(&ollama, "qwen3.8"), Some(16384));
    let router = json!({"data":[{"id":"a","top_provider":{"context_length":131072}}]});
    assert_eq!(context_from_models(&router, "a"), Some(131072));
    assert_eq!(context_from_models(&json!({"data":[{"id":"x"}]}), "x"), None);
}

/// Ollama's `/v1/models` carries no context metadata; its native
/// `/api/ps` does (`context_length`). Without this fallback the window
/// is never known on a local Ollama and every fit-to-window guard
/// silently stays off.
#[test]
fn ollama_reports_the_context_window_on_api_ps() {
    let ps =
        json!({"models":[{"name":"qwen3.8:27b-iq4_xs:latest","model":"qwen3.8:27b-iq4_xs","context_length":8192}]});
    assert_eq!(context_from_ollama_ps(&ps, "qwen3.8:27b-iq4_xs"), Some(8192));
    assert_eq!(context_from_ollama_ps(&ps, "elsewhere:latest"), Some(8192), "first entry fallback");
    assert_eq!(context_from_ollama_ps(&json!({"models":[]}), "x"), None);
    assert_eq!(context_from_ollama_ps(&json!({"models":[{"name":"x"}]}), "x"), None, "no field = unknown");
}

#[test]
fn old_tool_results_shrink_to_fit_a_small_window() {
    let m = |role: &str, text: String| ChatMsg {
        role: role.into(),
        content: Some(text),
        tool_calls: None,
        tool_call_id: None,
    };
    let big = "x".repeat(20_000);
    let mut msgs = vec![m("system", "s".into()), m("user", "u".into())];
    for _ in 0..4 {
        msgs.push(m("tool", big.clone()));
    }
    let cut = fit_tool_results(&mut msgs, 16_384);
    assert!(cut >= 2, "{cut}");
    assert!(estimate_tokens(&msgs, &[]) + 2_500 <= 16_384, "fits now");
    assert!(
        msgs[5].content.as_ref().unwrap().len() < 20_000 || msgs[2].content.as_ref().unwrap().contains("shortened")
    );
    let mut small = vec![m("user", "hi".into()), m("tool", "ok".into())];
    assert_eq!(fit_tool_results(&mut small, 16_384), 0, "a small request is left alone");
    assert_eq!(fit_tool_results(&mut msgs.clone(), 0), 0, "no known limit, nothing cut");
}

#[test]
fn file_changes_are_seen_without_git() {
    let dir = std::env::temp_dir().join(format!("ocrs-nogit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("old.txt"), "x").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let since = std::time::SystemTime::now();
    assert!(!files_changed_since(&dir, since), "nothing changed (node --version)");
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(dir.join("new.txt"), "y").unwrap();
    assert!(files_changed_since(&dir, since), "a command wrote a file");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn context_overflow_errors_are_recognised() {
    assert!(is_context_overflow("provider returned 500 Internal Server Error: {\"error\":\"Jinja Exception: No user query found in messages.\"}"));
    assert!(is_context_overflow("This model's maximum context length is 32768 tokens"));
    assert!(!is_context_overflow("provider returned 401 Unauthorized"));
}

/// The task must not live only at the FRONT of a long chat: this server
/// drops the head of a long request before its chat template renders,
/// and the Qwen template then raises "No user query found in messages"
/// (seen live, twice, in the middle of a review run).
#[tokio::test]
async fn a_long_tool_chain_keeps_the_task_near_the_tail() {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let s = st.db.create_session("u", None).unwrap();
    st.db.add_message(&s.id, "user", &[Part::Text { text: "Write SUMMARY.md from notes/.".into() }]).unwrap();
    for i in 0..20 {
        st.db
            .add_message(
                &s.id,
                "assistant",
                &[
                    Part::Text { text: format!("step {i}") },
                    Part::ToolCall { id: format!("c{i}"), name: "list".into(), input: json!({"path": "."}) },
                ],
            )
            .unwrap();
        st.db
            .add_message(
                &s.id,
                "tool",
                &[Part::ToolResult { tool_id: format!("c{i}"), output: "ok".into(), is_error: false }],
            )
            .unwrap();
    }
    let msgs = build_chat_messages(&st, &s.id, "sys").unwrap();
    let tail = &msgs[msgs.len() - 13..];
    assert!(
        tail.iter().any(|m| m.role == "user"),
        "the task stays near the tail: {:?}",
        tail.iter().map(|m| m.role.as_str()).collect::<Vec<_>>()
    );
    let last = msgs.last().unwrap();
    assert_eq!(last.role, "user");
    assert!(
        last.content.as_deref().is_some_and(|c| c.contains("Write SUMMARY.md from notes/.")),
        "and it quotes the task itself: {:?}",
        last.content
    );
    // A short chat is sent as it is.
    let st2 = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let s2 = st2.db.create_session("u", None).unwrap();
    st2.db.add_message(&s2.id, "user", &[Part::Text { text: "Say hi.".into() }]).unwrap();
    st2.db
        .add_message(
            &s2.id,
            "assistant",
            &[Part::ToolCall { id: "c0".into(), name: "list".into(), input: json!({"path": "."}) }],
        )
        .unwrap();
    st2.db
        .add_message(&s2.id, "tool", &[Part::ToolResult { tool_id: "c0".into(), output: "ok".into(), is_error: false }])
        .unwrap();
    let short = build_chat_messages(&st2, &s2.id, "sys").unwrap();
    assert_eq!(short.len(), 4, "no reminder in a short chat");
}

/// A request always carries a user message, even when the stored
/// history (after a compaction) has only tool steps.
#[tokio::test]
async fn a_request_always_has_a_user_message() {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let s = st.db.create_session("u", None).unwrap();
    st.db.add_message(&s.id, "assistant", &[Part::Text { text: "working".into() }]).unwrap();
    let msgs = build_chat_messages(&st, &s.id, "sys").unwrap();
    assert_eq!(msgs[0].role, "system");
    assert_eq!(msgs[1].role, "user", "inserted right after the system prompt");
}

/// The prompt is packed for the server it is talking to: in the `system`
/// role when that role is delivered, as an `[app]` note at the end when
/// the server drops it (seen 2026-10-26, where the whole prompt was lost).
#[tokio::test]
async fn the_prompt_follows_what_the_server_does_with_system_messages() {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let s = st.db.create_session("u", None).unwrap();
    st.db.add_message(&s.id, "user", &[Part::Text { text: "Say hi.".into() }]).unwrap();

    let msgs = build_chat_messages(&st, &s.id, "SYS PROMPT").unwrap();
    assert_eq!(msgs[0].role, "system", "not asked yet: the normal shape");

    st.system_delivery.store(SYSTEM_DROPS, Ordering::Relaxed);
    let msgs = build_chat_messages(&st, &s.id, "SYS PROMPT").unwrap();
    assert_eq!(msgs[0].role, "user", "the role this server keeps");
    let last = msgs.last().unwrap();
    assert_eq!(last.role, "user");
    let text = last.content.as_deref().unwrap();
    assert!(text.contains("SYS PROMPT"), "the prompt is still sent: {text}");
    assert!(text.starts_with("[app]"), "an app note, so it is not read as the task: {text}");

    st.system_delivery.store(SYSTEM_DELIVERS, Ordering::Relaxed);
    let msgs = build_chat_messages(&st, &s.id, "SYS PROMPT").unwrap();
    assert_eq!(msgs[0].role, "system");
    assert!(
        msgs.iter().all(|m| !m.content.as_deref().is_some_and(|c| c.contains("SYS PROMPT")) || m.role == "system"),
        "nothing appended when the role is delivered"
    );
}

#[test]
fn invisible_characters_do_not_hide_an_announcement() {
    assert!(announces_work("\u{200b}\u{200b}I'll read the reverse-engineering doc and map out the extracted files."));
    assert!(denies_file_access("\u{200b}I can't see your local folder, please paste the files."));
    assert_eq!(strip_invisible("\u{feff}a\u{200b}b"), "ab");
}

#[test]
fn announced_work_is_spotted() {
    for t in [
        "I'll replace index.html with a complete, self-contained file meeting every requirement.",
        "No, not yet. Let me build the complete single-file page now.",
        "The existing site doesn't match. I\u{2019}ll build a single index.html, then verify it.",
        "Here is the plan:",
        "Next, I will add the cart drawer.",
    ] {
        assert!(announces_work(t), "should nudge: {t}");
    }
    for t in [
        "Done. I built index.html with all nine sections and checked it at 375 and 1280 px.",
        "I'll leave the colours to you. Let me know if you want a darker theme.",
        "Should I use a warm or a dark theme?",
        "",
        "I added the card. Earlier I said I'll check the cart; it works now. Everything passes.",
    ] {
        assert!(!announces_work(t), "should not nudge: {t}");
    }
}

/// A reply that only announces the work ("I'll build it now.") does not
/// end the run: the model is asked to go ahead, and does the work.
#[tokio::test]
async fn an_announced_plan_is_followed_by_the_work() {
    use axum::{routing::post, Router};
    let dir = std::env::temp_dir().join(format!("ocrs-nudge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::extract::Json<Value>| {
            let n = h.fetch_add(1, Ordering::SeqCst);
            let last = body["messages"].as_array().and_then(|m| m.last().cloned()).unwrap_or_default();
            let nudged = last["content"].as_str().unwrap_or("").starts_with("[app] ");
            async move {
                let text = |t: &str| {
                    let frame = serde_json::json!({"choices":[{"delta":{"content":t},"finish_reason":"stop"}]});
                    format!("data: {frame}\n\ndata: [DONE]\n\n")
                };
                let payload = match (n, nudged) {
                    (0, _) => text("The folder is empty. I'll build index.html now."),
                    (_, true) => {
                        let args = serde_json::json!({"path": "index.html", "content": "<h1>Ember</h1>\n"}).to_string();
                        let chunk = serde_json::json!({"choices":[{"delta":{"tool_calls":[
                            {"index":0,"id":"w1","function":{"name":"write","arguments":args}}
                        ]},"finish_reason":null}]});
                        format!("data: {chunk}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n")
                    }
                    _ => text("Done: index.html is written."),
                };
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    cfg.permissions.defaults.insert("write".into(), "allow".into());
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("nudge", None).unwrap();
    let opts = LoopOpts {
        model: None,
        max_steps: None,
        system: "s".into(),
        emit_activity: true,
        allow: None,
        can_spawn: false,
        workspace: None,
        agent: None,
    };
    st.db.add_message(&s.id, "user", &[Part::Text { text: "build the page".into() }]).unwrap();
    let out = run_loop(&st, &s.id, &opts, &AtomicBool::new(false)).await.unwrap();
    assert_eq!(out, "Done: index.html is written.");
    assert_eq!(std::fs::read_to_string(dir.join("index.html")).unwrap(), "<h1>Ember</h1>\n");
    assert_eq!(hits.load(Ordering::SeqCst), 3, "announce, nudge → write, summary");
    assert_eq!(st.first_request(&s.id).as_deref(), Some("build the page"), "the nudge is not the user's request");
    // Every request is in the cache log, and each one started like the last.
    let log = std::fs::read_to_string(st.state_dir.join("cache.log")).unwrap_or_default();
    let mine: Vec<&str> = log.lines().filter(|l| l.contains(short_id(&s.id))).collect();
    assert_eq!(mine.len(), 3, "{log}");
    assert!(mine[0].ends_with("first request") && mine[1..].iter().all(|l| l.ends_with("same start")), "{log}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// ask_user ends the turn (the answer comes as the next message), and a
/// step that only ticks off todos next to a final answer costs no extra
/// model call.
#[tokio::test]
async fn questions_and_bookkeeping_end_the_turn_without_more_calls() {
    use axum::{routing::post, Router};
    fn reply(text: &str, name: &str, args: Value) -> String {
        let frame = serde_json::json!({"choices": [{"delta": {"content": text, "tool_calls": [{
            "index": 0, "id": "c1", "function": {"name": name, "arguments": args.to_string()}
        }]}, "finish_reason": null}]});
        format!("data: {frame}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n")
    }
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::extract::Json<Value>| {
            h.fetch_add(1, Ordering::SeqCst);
            let last = body["messages"].as_array().and_then(|m| m.last().cloned()).unwrap_or_default();
            let wants_todo = last["content"].as_str().unwrap_or("").contains("wrap up");
            async move {
                let payload = if wants_todo {
                    reply("All done: the page is built.", "todo", serde_json::json!({"op": "list"}))
                } else {
                    reply(
                        "",
                        "ask_user",
                        serde_json::json!({"question": "Warm or dark theme?", "options": ["Warm", "Dark"]}),
                    )
                };
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, std::env::temp_dir()).await;
    let s = st.db.create_session("ask", None).unwrap();
    let opts = LoopOpts {
        model: None,
        max_steps: None,
        system: "s".into(),
        emit_activity: true,
        allow: None,
        can_spawn: false,
        workspace: None,
        agent: None,
    };
    let mut rx = st.events.subscribe();
    st.db.add_message(&s.id, "user", &[Part::Text { text: "build it".into() }]).unwrap();
    let out = run_loop(&st, &s.id, &opts, &AtomicBool::new(false)).await.unwrap();
    assert_eq!(out, "Warm or dark theme?");
    assert_eq!(hits.load(Ordering::SeqCst), 1, "no call after the question");
    let mut asked = None;
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Question { question, options, .. } = ev {
            asked = Some((question, options));
        }
    }
    assert_eq!(asked, Some(("Warm or dark theme?".to_string(), vec!["Warm".to_string(), "Dark".to_string()])));

    st.db.add_message(&s.id, "user", &[Part::Text { text: "Warm, and wrap up".into() }]).unwrap();
    let out = run_loop(&st, &s.id, &opts, &AtomicBool::new(false)).await.unwrap();
    assert_eq!(out, "All done: the page is built.");
    assert_eq!(hits.load(Ordering::SeqCst), 2, "the todo-only final step needed no extra call");
}

/// A reply that breaks off mid-stream is asked again; the run goes on.
#[tokio::test]
async fn a_reply_that_breaks_off_is_asked_again() {
    use axum::{routing::post, Router};
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: axum::extract::Json<Value>| {
            let n = h.fetch_add(1, Ordering::SeqCst);
            async move {
                let payload = if n == 0 {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"half a rep\"},\"finish_reason\":null}]}\n\n\
                         data: {\"error\":{\"message\":\"upstream connection reset\"}}\n\n"
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"whole reply\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                };
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, std::env::temp_dir()).await;
    let s = st.db.create_session("drop", None).unwrap();
    st.db.add_message(&s.id, "user", &[Part::Text { text: "hi".into() }]).unwrap();
    let opts = LoopOpts {
        model: None,
        max_steps: None,
        system: "s".into(),
        emit_activity: true,
        allow: None,
        can_spawn: false,
        workspace: None,
        agent: None,
    };
    let out = run_loop(&st, &s.id, &opts, &AtomicBool::new(false)).await.unwrap();
    assert_eq!(out, "whole reply");
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    let saved = st.db.messages(&s.id).unwrap();
    assert!(
        !saved.iter().any(|m| m.parts.iter().any(|p| matches!(p, Part::Text { text } if text.contains("half")))),
        "the broken-off part is not kept"
    );
}

/// B-26: running out of steps is reported, not a silent empty answer.
#[tokio::test]
async fn max_steps_exhaustion_is_reported() {
    use axum::{routing::post, Router};
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|_body: axum::extract::Json<Value>| async move {
            let payload = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"t1","function":{"name":"todo","arguments":"{\"op\":\"list\"}"}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}

data: [DONE]

"#;
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    cfg.agent.max_steps = 2;
    cfg.permissions.defaults.insert("todo".into(), "allow".into());
    let st = build_state_ephemeral(cfg, std::env::temp_dir()).await;
    let s = st.db.create_session("steps", None).unwrap();
    st.db.add_message(&s.id, "user", &[Part::Text { text: "loop".into() }]).unwrap();
    let mut rx = st.events.subscribe();
    let opts = LoopOpts {
        model: None,
        max_steps: None,
        system: "s".into(),
        emit_activity: true,
        allow: None,
        can_spawn: false,
        workspace: None,
        agent: None,
    };

    let out = run_loop(&st, &s.id, &opts, &AtomicBool::new(false)).await.unwrap();

    assert!(out.contains("stopped after 2 steps"), "{out}");
    let mut noted = false;
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Note { text, .. } = ev {
            noted |= text.contains("agent.max_steps");
        }
    }
    assert!(noted, "a note tells the user");
}

/// Deep mode: three attempts at once, each in its own worktree; only the
/// "simplest" attempt writes a file the check accepts. That attempt is
/// applied to the project, the others are dropped, every request uses
/// the model the chat switched to, and no worktree is left behind.
#[tokio::test]
async fn deep_mode_keeps_the_attempt_that_passes_the_check() {
    use axum::{routing::post, Router};
    let dir = std::env::temp_dir().join(format!("ocrs-deep-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let git = |args: &[&str]| crate::gitsnap::git(&dir, args).unwrap();
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "core.autocrlf", "false"]);
    std::fs::write(dir.join("hello.txt"), "hi\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);

    let models = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen = models.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |body: axum::extract::Json<Value>| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(body["model"].as_str().unwrap_or("?").to_string());
                let msgs = body["messages"].as_array().cloned().unwrap_or_default();
                let last_role = msgs.last().and_then(|m| m["role"].as_str()).unwrap_or("");
                let task = msgs
                    .iter()
                    .find(|m| m["role"] == "user")
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .to_string();
                let payload = if last_role != "tool" {
                    let word = if task.contains("simplest") { "good" } else { "bad" };
                    let args = serde_json::json!({ "path": "out.txt", "content": format!("{word}\n") }).to_string();
                    let chunk = serde_json::json!({"choices":[{"delta":{"tool_calls":[
                        {"index":0,"id":"w1","function":{"name":"write","arguments":args}}
                    ]},"finish_reason":null}]});
                    format!("data: {chunk}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n")
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"wrote out.txt\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string()
                };
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "first".into();
    cfg.agent.verify_command = Some("grep -q good out.txt".into());
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    st.set_model("switched").await;
    let s = st.db.create_session("deep", None).unwrap();

    let out = deep::run_deep(&st, &s.id, "write out.txt", 3, &Arc::new(AtomicBool::new(false))).await.unwrap();
    assert!(out.contains("attempt 2") && out.contains("← kept"), "{out}");
    assert!(out.contains("Applied attempt 2"), "{out}");
    assert_eq!(std::fs::read_to_string(dir.join("out.txt")).unwrap(), "good\n");
    let models = models.lock().unwrap().clone();
    assert_eq!(models.len(), 6, "two requests per attempt: {models:?}");
    assert!(models.iter().all(|m| m == "switched"), "{models:?}");
    let worktrees = git(&["worktree", "list"]);
    assert_eq!(worktrees.lines().count(), 1, "attempt worktrees removed: {worktrees}");
    assert!(git(&["branch", "--list", "ocrs/*"]).trim().is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Deep mode without git says why instead of running.
#[tokio::test]
async fn deep_mode_needs_git() {
    let dir = std::env::temp_dir().join(format!("ocrs-deep-nogit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let st = build_state_ephemeral(crate::config::Config::default(), dir.clone()).await;
    let s = st.db.create_session("deep", None).unwrap();
    let err = deep::run_deep(&st, &s.id, "x", 2, &Arc::new(AtomicBool::new(false))).await.unwrap_err();
    assert!(format!("{err:#}").contains("needs a git repository"), "{err:#}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// B-24: the automatic review prompt neither counts nor breaks the streak.
#[tokio::test]
async fn turbo_streak_ignores_the_review_prompt() {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let s = st.db.create_session("streak-review", None).unwrap();
    for i in 0..2 {
        let cont = format!("{TURBO_CONTINUE_PREFIX} keep going {i}");
        st.db.add_message(&s.id, "user", &[Part::Text { text: cont }]).unwrap();
        let review = format!("{AUTO_REVIEW_PROMPT_START} in this workspace (ponytail-review).");
        st.db.add_message(&s.id, "user", &[Part::Text { text: review }]).unwrap();
    }
    assert_eq!(turbo_streak(&st, &s.id), 2);
}

/// B-24: a turbo chain is ONE run for clients: the capped continuations
/// run, and exactly one RunFinished arrives, at the very end.
#[tokio::test]
async fn turbo_chain_finishes_once() {
    let dir = std::env::temp_dir().join(format!("ocrs-b24-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = mock_text_llm().await;
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("turbo", None).unwrap();
    st.todo_store.exec(&s.id, TodoOp::Add { content: "never done".into() });
    st.set_perm_mode(PermissionMode::Turbo);
    let mut rx = st.events.subscribe();

    st.kick_run(s.id.clone(), "start".into()).await;
    let ended = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        loop {
            match rx.recv().await {
                Ok(ServerEvent::RunFinished { .. }) => return,
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
    .await;
    assert!(ended.is_ok(), "the chain ends");
    let continues = st
        .db
        .messages(&s.id)
        .unwrap()
        .iter()
        .filter(|m| {
            m.role == "user"
                && m.parts.iter().any(|p| matches!(p, Part::Text { text } if text.starts_with(TURBO_CONTINUE_PREFIX)))
        })
        .count();
    // The mock only ever answers in words: after one continuation it has
    // said it is done, so no more rounds are paid for (the cap of
    // TURBO_MAX_CONTINUATIONS is checked in `turbo_streak_caps_and_resets`).
    assert_eq!(continues, 1, "a words-only answer ends the chain");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    while let Ok(ev) = rx.try_recv() {
        assert!(!matches!(ev, ServerEvent::RunFinished { .. }), "only one RunFinished");
    }
    assert!(!st.run_active(&s.id).await, "nothing left running");
    let _ = std::fs::remove_dir_all(&dir);
}

/// B-22: write/edit previews show the change; other tools get none.
#[test]
fn approval_preview_shows_the_change() {
    let w = approval_preview("write", &serde_json::json!({"path": "a.txt", "content": "one\ntwo\n"}));
    assert!(w.contains("write a.txt — 2 line(s)"), "{w}");
    assert!(w.contains("+ one") && w.contains("+ two"), "{w}");
    let e = approval_preview(
        "edit",
        &serde_json::json!({"path": "b.rs", "old_str": "let x = 1;", "new_str": "let x = 2;"}),
    );
    assert!(e.contains("- let x = 1;") && e.contains("+ let x = 2;"), "{e}");
    assert!(approval_preview("bash", &serde_json::json!({"command": "ls"})).is_empty());
    let big = "x\n".repeat(500);
    let w = approval_preview("write", &serde_json::json!({"path": "big.txt", "content": big}));
    assert!(w.contains("more line(s)"), "long content is shortened: {w}");
}

/// B-22: the prompt carries the preview, and a note shows it first.
#[tokio::test]
async fn approval_needed_carries_the_preview() {
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = "http://127.0.0.1:9".into();
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral(cfg, std::env::temp_dir()).await;
    let mut rx = st.events.subscribe();
    let st2 = st.clone();
    let waiting = tokio::spawn(async move {
        let abort = AtomicBool::new(false);
        check_permission_preview(&st2, "s1", "write", "a.txt", "write a.txt\n+ hi\n", &abort).await
    });
    let id = pending_approval_id(&st).await;
    assert!(st.decide(&id, false, false));
    assert!(!waiting.await.unwrap());
    let (mut note, mut preview) = (String::new(), String::new());
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ServerEvent::Note { text, .. } => note = text,
            ServerEvent::ApprovalNeeded { preview: p, .. } => preview = p,
            _ => {}
        }
    }
    assert!(note.contains("+ hi"), "note shows the change: {note}");
    assert_eq!(preview, "write a.txt\n+ hi\n");
}

/// B-08: the heartbeat sends the API key (providers answer 401 without
/// it) and a 2xx answer reports "up".
#[tokio::test]
async fn heartbeat_sends_the_api_key() {
    use axum::{http::HeaderMap, http::StatusCode as Code, routing::get, Router};
    let app = Router::new().route(
        "/v1/models",
        get(|headers: HeaderMap| async move {
            let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
            if auth == Some("Bearer sekret") {
                (Code::OK, "{\"data\":[]}")
            } else {
                (Code::UNAUTHORIZED, "missing key")
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1/");
    cfg.provider.model = "m".into();
    cfg.provider.api_key = "sekret".into();
    let st = build_state_ephemeral(cfg, std::env::temp_dir()).await;
    let mut rx = st.events.subscribe();
    st.start_heartbeat();
    let ok = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(ServerEvent::Heartbeat { ok, .. }) = rx.recv().await {
                return ok;
            }
        }
    })
    .await
    .expect("a heartbeat arrives");
    assert!(ok, "the key was sent, so the endpoint reports up");
}

/// The window the provider reports is learned at startup, without the
/// heartbeat: a headless run never starts the heartbeat, and with the
/// window unknown every guard that fits or compacts the chat stays off
/// (seen 2026-10-26 — the chat outgrew the model, the server trimmed the
/// oldest messages including the user's request, and the Qwen template
/// failed every step with "No user query found in messages").
#[tokio::test]
async fn the_context_window_is_learned_without_a_heartbeat() {
    use axum::{routing::get, Router};
    let app = Router::new().route(
        "/v1/models",
        get(|| async { "{\"object\":\"list\",\"data\":[{\"id\":\"m\",\"context_window\":16384}]}" }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1/");
    cfg.provider.model = "m".into();
    assert_eq!(cfg.agent.context_tokens, 0, "nothing configured: it must be detected");
    let st = build_state_ephemeral(cfg, std::env::temp_dir()).await;
    assert_eq!(context_limit(&st), 0, "unknown before the probe");
    st.start_context_probe();
    let learned = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let n = context_limit(&st);
            if n > 0 {
                return n;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the window is learned from /models");
    assert_eq!(learned, 16_384);
}

/// A provider that lists its models without a key but rejects chat
/// requests with 401: the status check must say the key was rejected
/// instead of "connected".
#[tokio::test]
async fn heartbeat_reports_a_rejected_key() {
    use axum::{
        http::StatusCode as Code,
        routing::{get, post},
        Router,
    };
    let app = Router::new()
        .route("/v1/models", get(|| async { (Code::OK, "{\"data\":[]}") }))
        .route("/v1/chat/completions", post(|| async { (Code::UNAUTHORIZED, "{\"error\":\"invalid API token\"}") }));
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "m".into();
    cfg.provider.api_key = "wrong".into();
    let st = build_state_ephemeral(cfg, std::env::temp_dir()).await;
    let s = st.db.create_session("k", None).unwrap();
    let err = one_shot(&st, "s", "hi", &AtomicBool::new(false), false).await.unwrap_err();
    assert!(format!("{err:#}").contains("401"), "{err:#}");
    let _ = s;
    let mut rx = st.events.subscribe();
    st.start_heartbeat();
    let (ok, detail) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(ServerEvent::Heartbeat { ok, detail, .. }) = rx.recv().await {
                return (ok, detail);
            }
        }
    })
    .await
    .expect("a heartbeat arrives");
    assert!(!ok && detail.contains("rejected the API key"), "{ok} {detail}");
}

/// B-09: a waiting approval can be fetched again (after a lag) and is
/// gone once it was answered.
#[tokio::test]
async fn pending_approvals_lists_waiting_requests() {
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = "http://127.0.0.1:9".into();
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral(cfg, std::env::temp_dir()).await;
    st.set_perm_mode(PermissionMode::Manual);
    assert!(st.pending_approvals().is_empty());
    let st2 = st.clone();
    let waiting = tokio::spawn(async move {
        let abort = AtomicBool::new(false);
        check_permission(&st2, "s1", "read", "src/lib.rs", &abort).await
    });
    let id = pending_approval_id(&st).await;
    let pending = st.pending_approvals();
    assert_eq!(pending.len(), 1);
    match &pending[0] {
        ServerEvent::ApprovalNeeded { approval_id, session_id, tool, detail, .. } => {
            assert_eq!(approval_id, &id);
            assert_eq!(session_id, "s1");
            assert_eq!(tool, "read");
            assert_eq!(detail, "src/lib.rs");
        }
        other => panic!("expected ApprovalNeeded, got {other:?}"),
    }
    assert!(st.decide(&id, true, false));
    assert!(waiting.await.unwrap());
    assert!(st.pending_approvals().is_empty(), "answered requests are gone");
}

/// B-06: any run kind can be registered, stopped with `abort_run`, and
/// its slot is released when it ends.
#[tokio::test]
async fn kick_task_registers_and_abort_run_stops_it() {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let sid = "ses_task".to_string();
    let (tx, rx) = tokio::sync::oneshot::channel();
    st.kick_task(sid.clone(), move |_st, _sid, abort| async move {
        while !abort.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let _ = tx.send(());
    })
    .await;
    assert!(st.run_active(&sid).await, "registered while it runs");
    assert!(st.abort_run(&sid).await, "abort reaches the task");
    tokio::time::timeout(std::time::Duration::from_secs(5), rx).await.expect("the task saw the abort").unwrap();
    for _ in 0..100 {
        if !st.run_active(&sid).await {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!st.run_active(&sid).await, "slot released");
    assert!(!st.abort_run(&sid).await, "nothing left to abort");
}
/// B-06: after an abort, the remaining tool calls of the turn do not run,
/// but each one still gets a tool result.
#[tokio::test]
async fn abort_skips_the_remaining_tool_calls() {
    use axum::{routing::post, Router};
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|_body: axum::extract::Json<Value>| async move {
            let payload = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"todo","arguments":"{\"op\":\"add\",\"content\":\"A\"}"}},{"index":1,"id":"c2","function":{"name":"todo","arguments":"{\"op\":\"add\",\"content\":\"B\"}"}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}

data: [DONE]

"#;
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let dir = std::env::temp_dir().join(format!("ocrs-b06-skip-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    st.set_perm_mode(PermissionMode::Manual);
    let s = st.db.create_session("abort", None).unwrap();
    let abort = Arc::new(AtomicBool::new(false));
    let (st2, sid2, a2) = (st.clone(), s.id.clone(), abort.clone());
    let run = tokio::spawn(async move { run_agent(&st2, &sid2, "add two todos", a2).await });

    let _ = pending_approval_id(&st).await; // the first call waits
    abort.store(true, Ordering::SeqCst);
    tokio::time::timeout(std::time::Duration::from_secs(10), run).await.expect("the run stops").unwrap();

    let outputs: Vec<String> = st
        .db
        .messages(&s.id)
        .unwrap()
        .into_iter()
        .flat_map(|m| m.parts)
        .filter_map(|p| match p {
            Part::ToolResult { output, .. } => Some(output),
            _ => None,
        })
        .collect();
    assert_eq!(outputs.len(), 2, "both calls got a result: {outputs:?}");
    assert!(outputs[1].contains("not run"), "{outputs:?}");
    let TodoResult::Listed { items } = st.todo_store.exec(&s.id, TodoOp::List) else { panic!("expected a list") };
    assert!(items.is_empty(), "no todo was added: {items:?}");
    let _ = std::fs::remove_dir_all(&dir);
}
/// B-10: every way an approval ends is announced once, with its reason.
#[tokio::test]
async fn approval_resolved_reports_every_outcome() {
    // T-144: the waits the test answers itself get a timeout no busy
    // machine reaches; only the timeout case runs on a 1 s one.
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = "http://127.0.0.1:9".into();
    cfg.provider.model = "stub".into();
    cfg.permissions.approval_timeout_secs = 600;
    let st = build_state_ephemeral(cfg.clone(), std::env::temp_dir()).await;
    st.set_perm_mode(PermissionMode::Manual);
    cfg.permissions.approval_timeout_secs = 1;
    let quick = build_state_ephemeral(cfg, std::env::temp_dir()).await;
    quick.set_perm_mode(PermissionMode::Manual);
    let mut rx = st.events.subscribe();
    let mut quick_rx = quick.events.subscribe();

    // Start one approval wait; `abort` lets the test cancel it.
    let wait_on = |st: &Shared, detail: &'static str, abort: Arc<AtomicBool>| {
        let st2 = st.clone();
        tokio::spawn(async move { check_permission(&st2, "s1", "read", detail, &abort).await })
    };
    let wait = |detail: &'static str, abort: Arc<AtomicBool>| wait_on(&st, detail, abort);
    let no_abort = || Arc::new(AtomicBool::new(false));

    // 1. The user says yes.
    let h = wait("a.txt", no_abort());
    let id = pending_approval_id(&st).await;
    assert!(st.decide(&id, true, false));
    assert!(h.await.unwrap());
    // 2. The user says "always".
    let h = wait("b.txt", no_abort());
    let id = pending_approval_id(&st).await;
    assert!(st.decide(&id, true, true));
    assert!(h.await.unwrap());
    // 3. Nobody answers (1 s timeout).
    assert!(!wait_on(&quick, "c.txt", no_abort()).await.unwrap());
    // 4. The run is aborted while it waits.
    let abort = no_abort();
    let h = wait("d.txt", abort.clone());
    let _ = pending_approval_id(&st).await;
    abort.store(true, Ordering::SeqCst);
    assert!(!h.await.unwrap());
    // 5. Turbo releases it.
    let h = wait("e.txt", no_abort());
    let _ = pending_approval_id(&st).await;
    st.set_perm_mode(PermissionMode::Turbo);
    assert!(h.await.unwrap());

    let resolved = |rx: &mut broadcast::Receiver<ServerEvent>| {
        let mut seen = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let ServerEvent::ApprovalResolved { session_id, reason, allowed, .. } = ev {
                assert_eq!(session_id, "s1");
                seen.push((reason, allowed));
            }
        }
        seen
    };
    let expected: Vec<(String, bool)> =
        vec![("user".into(), true), ("always".into(), true), ("abort".into(), false), ("turbo".into(), true)];
    assert_eq!(resolved(&mut rx), expected);
    assert_eq!(resolved(&mut quick_rx), vec![("timeout".to_string(), false)]);
}

// B-04 helpers: build chat messages in a few words.
fn hmsg(role: &str, content: Option<&str>) -> ChatMsg {
    ChatMsg { role: role.into(), content: content.map(str::to_string), tool_calls: None, tool_call_id: None }
}
fn hcalls(ids: &[&str]) -> ChatMsg {
    let calls = ids
        .iter()
        .map(|id| crate::provider::ToolCallReq {
            id: id.to_string(),
            kind: "function".into(),
            function: crate::provider::FunctionCall { name: "bash".into(), arguments: "{}".into() },
        })
        .collect();
    ChatMsg { role: "assistant".into(), content: None, tool_calls: Some(calls), tool_call_id: None }
}
fn hresult(id: &str) -> ChatMsg {
    ChatMsg { role: "tool".into(), content: Some("ok".into()), tool_calls: None, tool_call_id: Some(id.into()) }
}
fn hroles(m: &[ChatMsg]) -> Vec<&str> {
    m.iter().map(|x| x.role.as_str()).collect()
}

/// B-04: tool calls without a result get an "interrupted" result.
#[test]
fn repair_answers_dangling_tool_calls() {
    let fixed = repair_history(vec![
        hmsg("system", Some("S")),
        hmsg("user", Some("go")),
        hcalls(&["c1", "c2"]),
        hresult("c1"),
        hmsg("user", Some("next")),
    ]);
    assert_eq!(hroles(&fixed), vec!["system", "user", "assistant", "tool", "tool", "user"]);
    assert_eq!(fixed[4].tool_call_id.as_deref(), Some("c2"));
    assert_eq!(fixed[4].content.as_deref(), Some(INTERRUPTED_TOOL_RESULT));
    // The same at the very end of the history.
    let fixed = repair_history(vec![hmsg("system", Some("S")), hmsg("user", Some("go")), hcalls(&["c9"])]);
    assert_eq!(hroles(&fixed), vec!["system", "user", "assistant", "tool"]);
}

/// B-04: orphan tool results and empty assistant messages are dropped.
#[test]
fn repair_drops_orphans_and_empty_assistant_messages() {
    let fixed = repair_history(vec![
        hmsg("system", Some("S")),
        hresult("lost"),
        hmsg("user", Some("a")),
        hmsg("assistant", None),
        hmsg("user", Some("b")),
    ]);
    assert_eq!(hroles(&fixed), vec!["system", "user"]);
    assert_eq!(
        fixed[1].content.as_deref(),
        Some(
            "a

b"
        )
    );
}

/// B-04: one system message at the top; later notes become user text.
#[test]
fn repair_keeps_one_system_message() {
    let fixed = repair_history(vec![
        hmsg("system", Some("S")),
        hmsg("system", Some("summary")),
        hmsg("user", Some("u")),
        hmsg("system", Some("expanded command")),
        hmsg("assistant", Some("done")),
    ]);
    assert_eq!(hroles(&fixed), vec!["system", "user", "assistant"]);
    assert_eq!(
        fixed[0].content.as_deref(),
        Some(
            "S

summary"
        )
    );
    assert_eq!(
        fixed[1].content.as_deref(),
        Some(
            "u

[system note]
expanded command"
        )
    );
}

/// B-04: a healthy history goes through unchanged.
#[test]
fn repair_leaves_a_healthy_history_alone() {
    let healthy = vec![
        hmsg("system", Some("S")),
        hmsg("user", Some("go")),
        hcalls(&["c1"]),
        hresult("c1"),
        hmsg("assistant", Some("done")),
    ];
    let fixed = repair_history(healthy.clone());
    assert_eq!(hroles(&fixed), hroles(&healthy));
    assert_eq!(fixed[3].content.as_deref(), Some("ok"));
}

/// B-04 end to end: a session left with a dangling tool call (as after a
/// crash) still works. The mock answers 400 to an invalid history.
#[tokio::test]
async fn session_with_dangling_tool_call_still_works() {
    use axum::{http::header::CONTENT_TYPE, http::StatusCode as Code, routing::post, Router};
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|body: axum::extract::Json<Value>| async move {
            let msgs = body["messages"].as_array().cloned().unwrap_or_default();
            let mut valid = true;
            for (i, m) in msgs.iter().enumerate() {
                if m["role"] == "system" && i > 0 {
                    valid = false;
                }
                for c in m["tool_calls"].as_array().cloned().unwrap_or_default() {
                    let answered =
                        msgs[i + 1..].iter().take_while(|n| n["role"] == "tool").any(|n| n["tool_call_id"] == c["id"]);
                    valid &= answered;
                }
            }
            if valid {
                let ok = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},\"finish_reason\":\"stop\"}]}

data: [DONE]

";
                (Code::OK, [(CONTENT_TYPE, "text/event-stream")], ok.to_string())
            } else {
                (Code::BAD_REQUEST, [(CONTENT_TYPE, "text/plain")], "invalid history".to_string())
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let dir = std::env::temp_dir().join(format!("ocrs-b04-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("dangling", None).unwrap();
    st.db.add_message(&s.id, "user", &[Part::Text { text: "first".into() }]).unwrap();
    st.db
        .add_message(
            &s.id,
            "assistant",
            &[Part::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                input: serde_json::json!({ "command": "sleep 100" }),
            }],
        )
        .unwrap();
    let mut rx = st.events.subscribe();

    run_agent(&st, &s.id, "next", Arc::new(AtomicBool::new(false))).await;

    let mut finished = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::RunFinished { ok, message, .. } = ev {
            finished.push((ok, message));
        }
    }
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert!(finished[0].0, "the provider accepted the repaired history: {finished:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// S-01: the HTTP API needs the bearer token and a local Host header;
/// /api/health needs no token.
#[tokio::test]
async fn api_requires_token_and_local_host() {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let addr = serve_with_token(st, 0, "test-token".into()).await.unwrap();
    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let url = |p: &str| format!("http://{addr}{p}");

    let r = http.get(url("/api/health")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200, "health stays open");
    let r = http.get(url("/api/sessions")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401, "no token");
    let r = http.get(url("/api/sessions")).bearer_auth("wrong").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401, "wrong token");
    let r = http.get(url("/api/sessions")).bearer_auth("test-token").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200, "right token");
    let r =
        http.get(url("/api/sessions")).bearer_auth("test-token").header("Host", "example.test").send().await.unwrap();
    assert_eq!(r.status().as_u16(), 403, "foreign Host header");
}

/// S-01: Host check, token shape and secret comparison.
#[test]
fn host_check_and_token_helpers() {
    let mut h = axum::http::HeaderMap::new();
    h.insert(axum::http::header::HOST, "127.0.0.1:4000".parse().unwrap());
    assert!(host_allowed(&h, 4000));
    assert!(!host_allowed(&h, 4001), "port must match");
    h.insert(axum::http::header::HOST, "LOCALHOST:4000".parse().unwrap());
    assert!(host_allowed(&h, 4000));
    h.insert(axum::http::header::HOST, "evil.example:4000".parse().unwrap());
    assert!(!host_allowed(&h, 4000));
    assert!(!host_allowed(&axum::http::HeaderMap::new(), 4000), "no Host header");

    let (a, b) = (random_token().unwrap(), random_token().unwrap());
    assert_eq!(a.len(), 64);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b, "every token is new");
    assert!(same_secret(b"abc", b"abc"));
    assert!(!same_secret(b"abc", b"abd"));
    assert!(!same_secret(b"abc", b"ab"));
}

/// M6: `/sessions`, `/model`, `/export` and `/todos` all read the same
/// state the TUI renders — verify them against a real (ephemeral) state.
#[tokio::test]
async fn session_export_and_todo_reports() {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let s = st.db.create_session("export me", None).unwrap();
    st.db.add_message(&s.id, "user", &[Part::Text { text: "hello there".into() }]).unwrap();
    st.db.add_message(&s.id, "assistant", &[Part::Text { text: "hi back".into() }]).unwrap();

    // The picker list carries the session, its title and no parent.
    let list = st.session_list();
    let row = list.iter().find(|r| r.id == s.id).expect("session listed");
    assert_eq!(row.title, "export me");
    assert!(row.parent_id.is_none());

    // The export is standalone markdown naming both sides of the chat.
    let md = st.transcript_markdown(&s.id);
    assert!(md.starts_with("# export me"), "{md}");
    assert!(md.contains("**You**") && md.contains("hello there"), "{md}");
    assert!(md.contains("**Agent**") && md.contains("hi back"), "{md}");

    // Todos: honest empty state, then the real list with open counts.
    assert!(st.todos_report(&s.id).contains("none yet"));
    st.todo_store.exec(&s.id, TodoOp::Add { content: "ship it".into() });
    let first_id = match st.todo_store.exec(&s.id, TodoOp::List) {
        TodoResult::Listed { items } => items[0].id.clone(),
        other => panic!("expected list, got {other:?}"),
    };
    st.todo_store.exec(&s.id, TodoOp::Complete { id: first_id });
    st.todo_store.exec(&s.id, TodoOp::Add { content: "still open".into() });
    let report = st.todos_report(&s.id);
    assert!(report.contains("1 open / 2 total"), "{report}");
    assert!(report.contains("✓ ship it"), "{report}");
    assert!(report.contains("○ still open"), "{report}");
}

/// M6: `/model` switches the shared client in place, and an endpoint with
/// no model listing degrades to an empty list instead of erroring.
#[tokio::test]
async fn model_switching_and_listing_degradation() {
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = "http://127.0.0.1:9/v1".into();
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral(cfg, std::env::temp_dir()).await;
    assert_eq!(st.current_model().await, "stub");
    st.set_model("mock-model").await;
    assert_eq!(st.current_model().await, "mock-model");
    assert!(st.available_models().await.is_empty(), "unreachable endpoint lists nothing");
}

/// M6: `/model` discovery understands both common listing shapes.
#[tokio::test]
async fn list_models_parses_openai_and_ollama_shapes() {
    use axum::{routing::get, Router};
    async fn serve(body: &'static str) -> String {
        let app = Router::new().route("/v1/models", get(move || async move { body }));
        let l = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        format!("http://{addr}/v1")
    }
    // OpenAI shape, with a duplicate that must be collapsed.
    let base = serve(r#"{"data":[{"id":"b"},{"id":"a"},{"id":"a"}]}"#).await;
    let c = Client::new(&base, "", "m", 5).unwrap();
    assert_eq!(c.list_models().await.unwrap(), vec!["a", "b"]);
    // Ollama shape uses `name` instead of `id`.
    let base = serve(r#"{"models":[{"name":"llama3"},{"name":"qwen"}]}"#).await;
    let c = Client::new(&base, "", "m", 5).unwrap();
    assert_eq!(c.list_models().await.unwrap(), vec!["llama3", "qwen"]);
}

/// Laziness Meter: symbols are extracted from common languages.
#[test]
fn top_level_symbols_detects_defs() {
    let code = "pub fn parse_config() {}\nasync function fetchData() {}\nclass Widget {}\nstruct Thing;\n";
    let mut syms = top_level_symbols(code);
    syms.sort();
    assert_eq!(syms, vec!["Thing", "Widget", "fetchData", "parse_config"]);
}

/// Reuse gate: a write that redefines an existing symbol gets a hint the
/// model reads mid-run; an overbuilt write gets a size flag.
#[test]
fn reuse_gate_flags_duplicates_and_bloat() {
    let dir = std::env::temp_dir().join(format!("ocrs-gate-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("existing.rs"), "pub fn parse_config() {}\n").unwrap();

    let dup = serde_json::json!({"path": "new.rs", "content": "pub fn parse_config() {}\n"});
    let hint = reuse_gate(&dir, "write", &dup).expect("duplicate must be flagged");
    assert!(hint.contains("parse_config"), "names the duplicated symbol: {hint}");
    assert!(hint.contains("existing.rs"), "points at where it already lives");

    let bloated = serde_json::json!({
        "path": "big.rs",
        "content": "fn solo() {}\n".repeat(130)
    });
    let hint = reuse_gate(&dir, "write", &bloated).expect("bloat must be flagged");
    assert!(hint.contains("130 lines"), "names the size: {hint}");

    // A lean new function gets no nagging.
    let lean = serde_json::json!({"path": "ok.rs", "content": "pub fn brand_new() {}\n"});
    assert!(reuse_gate(&dir, "write", &lean).is_none());
    // Non-write tools are never gated.
    assert!(reuse_gate(&dir, "bash", &dup).is_none());
}

/// Laziness Meter: tool accounting feeds the debt report.
#[tokio::test]
async fn debt_report_counts_lines_and_hints() {
    let dir = std::env::temp_dir().join(format!("ocrs-debt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = crate::config::Config::default();
    let st = build_state_ephemeral(cfg, dir).await;
    account_tool(&st, "s1", "write", &serde_json::json!({"path": "a", "content": "x\ny\nz"}));
    account_tool(&st, "s1", "edit", &serde_json::json!({"old_str": "a\nb", "new_str": "a"}));
    bump_hints(&st, "s1");
    let report = st.debt_report("s1");
    assert!(report.contains("+4"), "3 written + 1 edited: {report}");
    assert!(report.contains("−2") || report.contains("-2"), "edit old=a/b deletes 2 lines: {report}");
    assert!(report.contains("1 reuse hint"), "{report}");
}

/// `/parallel` (Deep mode with 2 attempts): attempts that change no
/// file are both listed and nothing is applied.
#[tokio::test]
async fn parallel_runs_in_isolated_worktrees() {
    let dir = std::env::temp_dir().join(format!("ocrs-par-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A real git repo with one commit to branch from.
    let git = |args: &[&str]| crate::gitsnap::git(&dir, args).unwrap();
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("hello.txt"), "hi\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "init"]);

    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = mock_text_llm().await;
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;

    let s = st.db.create_session("par", None).unwrap();
    let out = deep::run_deep(&st, &s.id, "say OK", 2, &Arc::new(AtomicBool::new(false))).await.unwrap();
    assert!(out.contains("attempt 1") && out.contains("attempt 2"), "both rows present: {out}");
    assert!(out.contains("nothing was applied"), "{out}");
}

/// Ponytail review runs read-only against the mock and reports in full.
#[tokio::test]
async fn run_review_streams_findings() {
    let dir = std::env::temp_dir().join(format!("ocrs-review-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = mock_text_llm().await;
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral(cfg, dir).await;
    let s = st.db.create_session("p", None).unwrap();
    let mut rx = st.events.subscribe();

    let out = run_review(&st, &s.id, None, &Arc::new(AtomicBool::new(false))).await.unwrap();
    assert_eq!(out, "Hello", "review streamed its findings");
    while let Ok(ev) = rx.try_recv() {
        assert!(!matches!(ev, ServerEvent::RunFinished { .. }), "review never ends the turn itself — the caller does");
    }
}

/// A reply the output token limit cut off mid-task is continued, not
/// silently finished half-done (the "stops mid" complaint).
#[tokio::test]
async fn a_cut_off_reply_is_continued_not_finished() {
    use axum::{routing::post, Router};
    use std::sync::atomic::AtomicUsize;
    let dir = std::env::temp_dir().join(format!("ocrs-cutoff-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: axum::extract::Json<Value>| {
            let calls = calls2.clone();
            async move {
                let payload = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"half answ\"},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n"
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"er: done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                };
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    payload.to_string(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("cut", None).unwrap();

    run_agent(&st, &s.id, "do the work", Arc::new(AtomicBool::new(false))).await;

    assert_eq!(calls.load(Ordering::SeqCst), 2, "the model was asked to continue");
    let msgs = st.db.messages(&s.id).unwrap();
    assert!(
        msgs.iter().any(|m| m.role == "user"
            && m.parts.iter().any(|p| matches!(p, Part::Text { text }
                if text.contains("cut short by the output token limit")))),
        "continuation asked"
    );
    let all: String = msgs
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            Part::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(all.contains("half answ") && all.contains("er: done"), "{all}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A stream that keeps breaking off mid-reply (tunnel, proxy): the run
/// keeps the part that arrived instead of throwing the reply away and
/// failing after three retries.
#[tokio::test]
async fn a_broken_stream_keeps_the_part_that_arrived() {
    use axum::{routing::post, Router};
    use std::sync::atomic::AtomicUsize;
    let dir = std::env::temp_dir().join(format!("ocrs-dropped-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: axum::extract::Json<Value>| {
            let calls = calls2.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                // Two good SSE chunks, then the connection breaks — the
                // reply is cut mid-way. Each item waits a little first so
                // its bytes reach the client before the break: an error
                // the instant the body fails looks like a failed request
                // to the client (hyper: IncompleteMessage), never a
                // stream that broke off mid-reply.
                let chunks: Vec<Result<axum::body::Bytes, std::io::Error>> = vec![
                    Ok(axum::body::Bytes::from_static(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"},\"finish_reason\":null}]}\n\n",
                    )),
                    Ok(axum::body::Bytes::from_static(
                        b"data: {\"choices\":[{\"delta\":{\"content\":\"world\"},\"finish_reason\":null}]}\n\n",
                    )),
                    Err(std::io::Error::other("connection reset")),
                ];
                let stream = futures::stream::iter(chunks).then(|item| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                    item
                });
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], axum::body::Body::from_stream(stream))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("drop", None).unwrap();
    let mut rx = st.events.subscribe();

    run_agent(&st, &s.id, "say hi", Arc::new(AtomicBool::new(false))).await;

    assert_eq!(calls.load(Ordering::SeqCst), 4, "three retries, then the kept partial");
    // Each retried reply's streamed part is taken back, so a client shows
    // the kept reply once, not "Hello worldHello world…".
    let (mut shown, mut retracted) = (0usize, Vec::new());
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ServerEvent::AssistantDelta { text, .. } => shown += text.chars().count(),
            ServerEvent::ReplyRetracted { chars, .. } => retracted.push(chars),
            _ => {}
        }
    }
    assert_eq!(retracted, vec![11, 11, 11], "one take-back per retried attempt");
    assert_eq!(shown - retracted.iter().sum::<usize>(), "Hello world".len(), "what stays on screen");
    let msgs = st.db.messages(&s.id).unwrap();
    let all: String = msgs
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            Part::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(all.contains("Hello world"), "{all}");
    assert!(all.contains("[reply broke off"), "the marker explains the cut: {all}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A proxy that reports an upstream failure as fake reply text (seen
/// live: a wrapped "[Error: HTTP 500: …]" arriving as the model's
/// words) must never be saved as an answer — the run asks again.
#[tokio::test]
async fn an_error_page_in_place_of_a_reply_is_never_saved() {
    use axum::{routing::post, Router};
    use std::sync::atomic::AtomicUsize;
    let dir = std::env::temp_dir().join(format!("ocrs-errpage-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: axum::extract::Json<Value>| {
            let calls = calls2.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let content = if n < 2 {
                    "[Error: HTTP 500: {\"error\":\"upstream exploded\"}]"
                } else {
                    "all good"
                };
                let payload = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
                    serde_json::to_string(content).unwrap()
                );
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("errpage", None).unwrap();

    run_agent(&st, &s.id, "hi", Arc::new(AtomicBool::new(false))).await;

    let msgs = st.db.messages(&s.id).unwrap();
    let all: String = msgs
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            Part::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(!all.contains("upstream exploded"), "the error page must not be saved: {all}");
    assert!(all.contains("all good"), "{all}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A provider that answers only with wrapped errors fails the run with
/// the error — never as a silent fake answer.
#[tokio::test]
async fn a_provider_that_only_sends_error_pages_fails_the_run() {
    use axum::{routing::post, Router};
    let dir = std::env::temp_dir().join(format!("ocrs-erronly-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: axum::extract::Json<Value>| async move {
            let content = "[Error: HTTP 503: {\"error\":\"down\"}]";
            let payload = format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":{}}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n",
                serde_json::to_string(content).unwrap()
            );
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("erronly", None).unwrap();

    run_agent(&st, &s.id, "hi", Arc::new(AtomicBool::new(false))).await;

    let msgs = st.db.messages(&s.id).unwrap();
    let all: String = msgs
        .iter()
        .flat_map(|m| m.parts.iter())
        .filter_map(|p| match p {
            Part::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(!all.contains("[Error: HTTP"), "no fake answer may be saved: {all}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Plain-text mock provider: replies "Hello" to any chat request.
async fn mock_text_llm() -> String {
    use axum::{routing::post, Router};
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|_body: axum::extract::Json<Value>| async move {
            let payload = concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"},",
                "\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n"
            )
            .to_string();
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/v1")
}

/// Regression: the answer streams once as AssistantDelta; the completion
/// event must NOT carry it again (the TUI used to print it a second time).
#[tokio::test]
async fn run_agent_streams_answer_once_and_finishes_silently() {
    let dir = std::env::temp_dir().join(format!("ocrs-runagent-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = mock_text_llm().await;
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral(cfg, dir).await;
    let s = st.db.create_session("p", None).unwrap();
    let mut rx = st.events.subscribe();

    run_agent(&st, &s.id, "hi", Arc::new(AtomicBool::new(false))).await;

    let mut text = String::new();
    let mut finished: Vec<(bool, Option<String>)> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ServerEvent::AssistantDelta { text: t, .. } => text.push_str(&t),
            ServerEvent::RunFinished { ok, message, .. } => finished.push((ok, message)),
            _ => {}
        }
    }
    assert_eq!(text, "Hello", "answer streamed exactly once as deltas");
    assert_eq!(finished.len(), 1, "exactly one completion event");
    assert!(finished[0].0, "run succeeded");
    assert!(finished[0].1.is_none(), "answer must not be re-sent as the completion message");
}

/// Regression: when a newer run owns the session slot, the replaced run
/// must not announce completion (it used to flip the TUI busy state and
/// inject its stale answer into the new run's transcript).
#[tokio::test]
async fn superseded_run_stays_silent() {
    let dir = std::env::temp_dir().join(format!("ocrs-superseded-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = mock_text_llm().await;
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral(cfg, dir).await;
    let s = st.db.create_session("p", None).unwrap();
    let mut rx = st.events.subscribe();

    // A newer run owns the slot; ours still runs to completion.
    {
        let mut busy = st.busy.write().await;
        busy.insert(s.id.clone(), Arc::new(AtomicBool::new(false)));
    }
    run_agent(&st, &s.id, "hi", Arc::new(AtomicBool::new(false))).await;

    while let Ok(ev) = rx.try_recv() {
        assert!(!matches!(ev, ServerEvent::RunFinished { .. }), "a replaced run must not announce completion");
    }
}

/// kick_run registers the run so the next prompt can replace it.
#[tokio::test]
async fn kick_run_aborts_previous_flag_and_owns_cleanup() {
    let dir = std::env::temp_dir().join(format!("ocrs-kick-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = "http://127.0.0.1:9".into(); // fails fast
    cfg.provider.max_retries = 0; // F-01: no retry waits in this timing test
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral(cfg, dir).await;
    let sid = "ses_kick".to_string();

    st.kick_run(sid.clone(), "one".into()).await;
    // Replace immediately: the first flag must be aborted and swapped out.
    st.kick_run(sid.clone(), "two".into()).await;
    let entry = st.busy.read().await.get(&sid).cloned();
    assert!(entry.is_some(), "a run must be registered while active");

    // After both runs end (instant connect error), the slot is cleaned up
    // and only the current owner may remove it.
    for _ in 0..100 {
        if st.busy.read().await.get(&sid).is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(st.busy.read().await.get(&sid).is_none(), "slot released");
}

/// E2E: the agent calls the `todo` tool to add + list tasks, gets results
/// back as tool results, and the store persists per session.
#[tokio::test]
async fn todo_tool_add_and_list_e2e() {
    use axum::{routing::post, Router};
    use serde_json::Value;

    let dir = std::env::temp_dir().join(format!("ocrs-todo-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.permissions.defaults.insert("todo".into(), "allow".into());
    cfg.permissions.approval_timeout_secs = 2;
    // Mock: first turn emits a `todo add` call; second turn returns text.
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|body: axum::extract::Json<Value>| async move {
            let msgs = body["messages"].as_array().cloned().unwrap_or_default();
            let last_role = msgs.last().and_then(|m| m["role"].as_str()).unwrap_or("");
            let payload = if last_role != "tool" {
                r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_t1","function":{"name":"todo","arguments":"{\"op\":\"add\",\"content\":\"set up scaffolding\"}"}}]},"finish_reason":null}]}

data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}

data: [DONE]

"#
                .to_string()
            } else {
                "data: {\"choices\":[{\"delta\":{\"content\":\"todo added, listing next\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                    .to_string()
            };
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                payload,
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("td", None).unwrap();
    let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    crate::server::run_agent(&st, &s.id, "track my work", abort).await;

    let msgs = st.db.messages(&s.id).unwrap();
    assert!(msgs
        .iter()
        .any(|m| m.role == "assistant"
            && m.parts.iter().any(|p| matches!(p, Part::ToolCall { name, .. } if name == "todo"))));
    let tool_msg = msgs.iter().find(|m| m.role == "tool").expect("todo result");
    let text = match &tool_msg.parts[0] {
        Part::ToolResult { output, is_error, .. } => {
            assert!(!is_error, "todo succeeded: {output}");
            output.clone()
        }
        other => panic!("wrong part {other:?}"),
    };
    assert!(text.contains("set up scaffolding"), "tool result: {text}");

    // Verify the store holds it for this session.
    let TodoResult::Listed { items } = st.todo_store.exec(&s.id, TodoOp::List) else { panic!("expected listed") };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].content, "set up scaffolding");

    let _ = std::fs::remove_dir_all(&dir);
}

/// E2E: a run that writes a file is accounted by the Laziness Meter and
/// automatically gets the outside-the-diff ponytail review before the turn
/// ends — review output streams, exactly one RunFinished arrives last.
#[tokio::test]
async fn write_run_accounts_and_auto_reviews() {
    use axum::{routing::post, Router};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let dir = std::env::temp_dir().join(format!("ocrs-autoreview-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut cfg = crate::config::Config::default();
    cfg.permissions.defaults.insert("write".into(), "allow".into());
    cfg.permissions.approval_timeout_secs = 2;

    // Call 0: tool call `write`; call 1: main answer; call 2+: review.
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: axum::extract::Json<Value>| {
            let calls = calls2.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let payload = match n {
                    0 => concat!(
                        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_w1\",\"function\":{\"name\":\"write\",\"arguments\":\"{\\\"path\\\":\\\"made.txt\\\",\\\"content\\\":\\\"hello\\\\n\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
                        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                        "data: [DONE]\n\n",
                    )
                    .to_string(),
                    1 => "data: {\"choices\":[{\"delta\":{\"content\":\"main answer\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string(),
                    _ => "data: {\"choices\":[{\"delta\":{\"content\":\"review findings: none\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string(),
                };
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    payload,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("ar", None).unwrap();
    let mut rx = st.events.subscribe();

    run_agent(&st, &s.id, "make a marker", Arc::new(AtomicBool::new(false))).await;

    // Drain events in order.
    let mut text = String::new();
    let mut notes: Vec<String> = Vec::new();
    let mut finished: Vec<bool> = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ServerEvent::AssistantDelta { text: t, .. } => text.push_str(&t),
            ServerEvent::Note { text: t, .. } => notes.push(t),
            ServerEvent::RunFinished { ok, .. } => finished.push(ok),
            _ => {}
        }
    }
    // The write really happened in the workspace.
    assert_eq!(std::fs::read_to_string(dir.join("made.txt")).unwrap(), "hello\n");
    // Laziness Meter accounted it.
    let debt = st.debt_report(&s.id);
    assert!(debt.contains("+1"), "one written line: {debt}");
    assert!(debt.contains("1 write(s)"), "{debt}");
    assert!(debt.contains("0 reuse hint"), "{debt}");
    // Auto-review fired and streamed its findings before the turn ended.
    assert!(notes.iter().any(|n| n.contains("ponytail review")), "review note announced: {notes:?}");
    let main_at = text.find("main answer").expect("main answer streamed");
    let review_at = text.find("review findings").expect("review streamed");
    assert!(main_at < review_at, "review answers after the run: {text}");
    // The review prompt reached the transcript (read-only pass).
    let msgs = st.db.messages(&s.id).unwrap();
    assert!(
        msgs.iter().any(|m| m.role == "user"
            && m.parts.iter().any(|p| matches!(p, Part::Text { text }
                if text.contains("Review the current uncommitted changes")))),
        "review prompt recorded"
    );
    // Exactly one completion, and it is the final event.
    assert_eq!(finished, vec![true], "one successful completion, review included");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Wait (bounded) for the run to register a pending approval, then return
/// its id. A mode that should gate blocks here; a mode that should not gate
/// never registers one.
async fn pending_approval_id(st: &Shared) -> String {
    // Up to 30 s: a busy machine can be slow to start the waiting task.
    for _ in 0..3000 {
        if let Ok(map) = st.approvals.lock() {
            if let Some(id) = map.keys().next() {
                return id.clone();
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no approval was ever requested");
}

/// Turbo: full access — an `ask` tool runs without any approval prompt.
#[tokio::test]
async fn turbo_auto_approves_asks() {
    let dir = std::env::temp_dir().join(format!("ocrs-turbo-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = "http://127.0.0.1:9".into(); // never reached
    cfg.provider.model = "stub".into();
    // A hard deny rule — policy, not a prompt.
    cfg.permissions.rules.push(crate::config::RuleCfg {
        tool: "bash".into(),
        pattern: "rm *".into(),
        action: "deny".into(),
    });
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    st.set_perm_mode(PermissionMode::Turbo);

    // `write` defaults to "ask": normally this would block on the dialog.
    let abort = AtomicBool::new(false);
    assert!(check_permission(&st, "s1", "write", "made.txt", &abort).await, "turbo grants asks without prompting");
    assert!(st.approvals.lock().unwrap().is_empty(), "no pending approval left");

    assert!(!check_permission(&st, "s1", "bash", "rm -rf /", &abort).await, "deny rules survive turbo");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Manual: even a tool the config would allow outright waits for a nod;
/// denying it keeps the tool from running.
#[tokio::test]
async fn manual_mode_gates_allowed_tools() {
    let dir = std::env::temp_dir().join(format!("ocrs-manual-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = "http://127.0.0.1:9".into();
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    st.set_perm_mode(PermissionMode::Manual);

    // `read` is allowed by default; manual mode must still ask.
    let st2 = st.clone();
    let allowed = tokio::spawn(async move {
        let abort = AtomicBool::new(false);
        check_permission(&st2, "s1", "read", "src/main.rs", &abort).await
    });
    let id = pending_approval_id(&st).await;
    assert!(st.decide(&id, true, false), "approval was pending");
    assert!(allowed.await.unwrap(), "runs once the user says yes");

    let st2 = st.clone();
    let denied = tokio::spawn(async move {
        let abort = AtomicBool::new(false);
        check_permission(&st2, "s1", "list", ".", &abort).await
    });
    let id = pending_approval_id(&st).await;
    assert!(st.decide(&id, false, false), "approval was pending");
    assert!(!denied.await.unwrap(), "a no keeps it from running");

    // "Always allow" is the user's own grant — it survives manual mode.
    st.db.add_approval(&crate::db::workspace_key(&st.workspace), "write", "made.txt", "session").unwrap();
    let abort = AtomicBool::new(false);
    assert!(check_permission(&st, "s1", "write", "made.txt", &abort).await, "a remembered approval is not re-asked");
    assert!(st.approvals.lock().unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// Switching to Turbo is a blanket grant: whatever is blocked right now is
/// released instead of waiting for the approval timeout.
#[tokio::test]
async fn turbo_releases_an_already_blocked_prompt() {
    let dir = std::env::temp_dir().join(format!("ocrs-turbo-grant-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = "http://127.0.0.1:9".into();
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    st.set_perm_mode(PermissionMode::Manual);

    let st2 = st.clone();
    let blocked = tokio::spawn(async move {
        let abort = AtomicBool::new(false);
        check_permission(&st2, "s1", "write", "made.txt", &abort).await
    });
    let id = pending_approval_id(&st).await;

    let granted = st.set_perm_mode(PermissionMode::Turbo);
    assert_eq!(granted, vec![id], "the pending prompt is reported as granted");
    assert!(blocked.await.unwrap(), "the blocked tool now runs");
    assert!(st.approvals.lock().unwrap().is_empty(), "nothing left pending");

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn turbo_streak_caps_and_resets() {
    let dir = std::env::temp_dir().join(format!("ocrs-streak-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = crate::config::Config::default();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("streak", None).unwrap();

    // No open todos, no continuations yet.
    assert!(!todos_open(&st, &s.id));
    assert_eq!(turbo_streak(&st, &s.id), 0);

    st.todo_store.exec(&s.id, crate::todo::TodoOp::Add { content: "task one".into() });
    assert!(todos_open(&st, &s.id), "open todo keeps the work going");

    for i in 1..=TURBO_MAX_CONTINUATIONS {
        st.db
            .add_message(&s.id, "user", &[Part::Text { text: format!("{TURBO_CONTINUE_PREFIX} keep going {i}") }])
            .unwrap();
    }
    assert_eq!(turbo_streak(&st, &s.id), TURBO_MAX_CONTINUATIONS);

    // A real user message breaks the streak.
    st.db.add_message(&s.id, "user", &[Part::Text { text: "actual new request".into() }]).unwrap();
    assert_eq!(turbo_streak(&st, &s.id), 0);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A turbo run that keeps changing nothing — no file written, no todo
/// finished — is stopped instead of looping to the continuation cap. A real
/// change (a new todo, or an edited file) clears the stall and lets it go on.
#[tokio::test]
async fn turbo_stops_when_a_run_stops_changing_anything() {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("ocrs-turbostall-{}-{stamp}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // A real git repo so the workspace fingerprint is meaningful.
    crate::gitsnap::git(&dir, &["init"]).unwrap();
    crate::gitsnap::git(&dir, &["config", "user.email", "t@t"]).unwrap();
    crate::gitsnap::git(&dir, &["config", "user.name", "t"]).unwrap();

    let st = build_state_ephemeral(crate::config::Config::default(), dir.clone()).await;
    st.set_perm_mode(PermissionMode::Turbo);
    let s = st.db.create_session("stall", None).unwrap();
    st.todo_store.exec(&s.id, crate::todo::TodoOp::Add { content: "task one".into() });
    let done: Result<LoopEnd> = Ok(LoopEnd::Done("still working".into()));

    // First call sets the baseline and continues; the next is one stalled
    // round and still continues; the second stalled round in a row stops.
    assert!(turbo_continuation(&st, &s.id, &done).await.is_some(), "baseline round continues");
    assert!(turbo_continuation(&st, &s.id, &done).await.is_some(), "one stalled round still continues");
    assert!(turbo_continuation(&st, &s.id, &done).await.is_none(), "two stalled rounds in a row stop it");

    // A new open todo is real progress: the stall clears.
    st.todo_store.exec(&s.id, crate::todo::TodoOp::Add { content: "task two".into() });
    assert!(turbo_continuation(&st, &s.id, &done).await.is_some(), "a todo change resets the stall");
    assert!(turbo_continuation(&st, &s.id, &done).await.is_some(), "one stalled round again");

    // Writing a file is progress too: it resets the stall again.
    std::fs::write(dir.join("new.txt"), "hello").unwrap();
    assert!(turbo_continuation(&st, &s.id, &done).await.is_some(), "a file change resets the stall");
    assert!(turbo_continuation(&st, &s.id, &done).await.is_some(), "one stalled round once more");
    assert!(turbo_continuation(&st, &s.id, &done).await.is_none(), "stops again once nothing changes");

    let _ = std::fs::remove_dir_all(&dir);
}

/// /skill install: a worthy skill is vetted, installed and baked in — its
/// body lands in the system prompt; an unworthy one is refused.
#[tokio::test]
async fn skill_install_vets_and_bakes() {
    let dir = std::env::temp_dir().join(format!("ocrs-skill-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Mock verdicts: first call worthy, second call skip.
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls2 = calls.clone();
    use axum::{routing::post, Router};
    use serde_json::Value;
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: axum::extract::Json<Value>| {
            let calls = calls2.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let reply = if n == 0 {
                    "WORTHY: durable workflow with concrete steps"
                } else {
                    "SKIP: vague and redundant"
                };
                let payload = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{reply}\"}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
                );
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    payload,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let base = dir.join("installed");
    let st = build_state_ephemeral_skills(cfg, dir.clone(), base.clone()).await;

    // A local skill file to install.
    std::fs::write(
        dir.join("ship.md"),
        "---\nname: ship-it\ndescription: Ship safely\n---\n\nAlways run the smoke test \
             after deploying, watch the error rate for ten minutes, and roll back on any \
             regression. Never deploy on a Friday evening.",
    )
    .unwrap();
    let abort = AtomicBool::new(false);

    let report = install_skill_flow(&st, "ship.md", &abort).await.unwrap();
    assert!(report.contains("BAKED IN"), "{report}");
    assert!(report.contains("ship-it"), "{report}");
    // Installed + baked on disk…
    assert!(base.join("skills").join("ship-it.md").is_file());
    let baked = crate::skills::load_baked(&base);
    assert_eq!(baked.len(), 1);
    // …and composed into the system prompt like the ponytail rules.
    let system = st.composed_system("ses_x");
    assert!(system.contains("## Skill: ship-it — always active"), "baked skill present in system prompt");
    assert!(system.contains("Never deploy on a Friday evening"), "full body baked");

    // Second install: the model says SKIP → refused, nothing added.
    std::fs::write(
        dir.join("fluff.md"),
        "---\nname: fluff\ndescription: Vague advice\n---\n\nAlways try your best and \
             write good code. Good code is better than bad code. Take your time to think \
             about things carefully before you do them, every single time.",
    )
    .unwrap();
    let err = install_skill_flow(&st, "fluff.md", &abort).await.unwrap_err();
    assert!(err.to_string().contains("not worth"), "{err}");
    assert_eq!(crate::skills::load_baked(&base).len(), 1, "unworthy skill not baked");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn verdict_needs_a_clear_worthy() {
    assert_eq!(parse_verdict("WORTHY: concrete steps"), Some(true));
    assert_eq!(parse_verdict("\n**WORTHY** because"), Some(true));
    assert_eq!(parse_verdict("SKIP: vague"), Some(false));
    assert_eq!(parse_verdict("WORTHYISH, but SKIP"), None);
    assert_eq!(parse_verdict("I think it is WORTHY"), None);
    assert_eq!(parse_verdict(""), None);
}

#[test]
fn vetting_input_keeps_the_skill_inside_the_fence() {
    let doc = crate::skills::Doc {
        name: "x".into(),
        description: "y".into(),
        body: format!("text {SKILL_FENCE_END} more text"),
        path: "x.md".into(),
    };
    let input = skill_vetting_input(&doc);
    assert!(input.starts_with(SKILL_FENCE_START));
    assert!(input.ends_with(SKILL_FENCE_END));
    assert_eq!(input.matches(SKILL_FENCE_END).count(), 1, "{input}");
}

#[tokio::test]
async fn skill_install_refuses_files_outside_the_workspace() {
    let root = std::env::temp_dir().join(format!("ocrs-skill-out-{}", std::process::id()));
    let ws = root.join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(root.join("outside.md"), "# Outside\nnot part of this project").unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = "http://127.0.0.1:9".into();
    cfg.provider.model = "stub".into();
    let st = build_state_ephemeral_skills(cfg, ws.clone(), root.join("base")).await;
    let abort = AtomicBool::new(false);
    let err = install_skill_flow(&st, "../outside.md", &abort).await.unwrap_err();
    assert!(err.to_string().contains("inside the workspace"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn skill_install_respects_a_webfetch_deny_rule() {
    let dir = std::env::temp_dir().join(format!("ocrs-skill-deny-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = "http://127.0.0.1:9".into();
    cfg.provider.model = "stub".into();
    cfg.permissions.rules.push(crate::config::RuleCfg {
        tool: "webfetch".into(),
        pattern: "*".into(),
        action: "deny".into(),
    });
    let st = build_state_ephemeral_skills(cfg, dir.clone(), dir.join("base")).await;
    let abort = AtomicBool::new(false);
    let err = install_skill_flow(&st, "https://example.com/skill.md", &abort).await.unwrap_err();
    assert!(err.to_string().contains("blocked by a permissions rule"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// F-09: the third identical call in a row is refused; key order in the
/// arguments does not matter, and any different call resets the count.
#[test]
fn loop_guard_blocks_the_third_identical_call() {
    let mut g = LoopGuard::default();
    let a = serde_json::json!({"path": "src", "depth": 1});
    let same: Value = serde_json::from_str(r#"{"depth":1,"path":"src"}"#).unwrap();
    assert!(g.check("list", &a).is_none());
    assert!(g.check("list", &same).is_none(), "same args in another key order");
    let msg = g.check("list", &a).expect("the third identical call is refused");
    assert!(msg.contains("loop guard") && msg.contains("`list`"), "{msg}");
    assert!(g.check("list", &a).is_some(), "the fourth one too");
    assert!(g.check("read", &a).is_none(), "a different tool resets the count");
    assert!(g.check("list", &a).is_none());
    assert!(g.check("list", &a).is_none());
    let other = serde_json::json!({"path": "other"});
    assert!(g.check("list", &other).is_none(), "different args reset the count");
}

/// F-09 end to end: a model that keeps sending the same `list` call gets
/// two real results, then loop-guard errors instead of more runs.
#[tokio::test]
async fn run_loop_refuses_repeated_identical_calls() {
    use axum::{routing::post, Router};
    use std::sync::atomic::AtomicUsize;

    let dir = std::env::temp_dir().join(format!("ocrs-loopguard-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Requests 0-3: the same `list` call (a new call id each time); then text.
    let calls = Arc::new(AtomicUsize::new(0));
    let calls2 = calls.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: axum::extract::Json<Value>| {
            let calls = calls2.clone();
            async move {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let payload = if n < 4 {
                    let frame = serde_json::json!({"choices": [{"delta": {"tool_calls": [{
                        "index": 0,
                        "id": format!("call_{n}"),
                        "function": {"name": "list", "arguments": "{\"path\":\".\"}"}
                    }]}, "finish_reason": null}]});
                    format!(
                        "data: {frame}\n\n\
                             data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
                             data: [DONE]\n\n"
                    )
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"giving up\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                        .to_string()
                };
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    payload,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("lg", None).unwrap();
    let mut rx = st.events.subscribe();

    run_agent(&st, &s.id, "list the folder", Arc::new(AtomicBool::new(false))).await;

    let results: Vec<(String, bool)> = st
        .db
        .messages(&s.id)
        .unwrap()
        .iter()
        .filter_map(|m| match m.parts.first() {
            Some(Part::ToolResult { output, is_error, .. }) => Some((output.clone(), *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 4, "{results:?}");
    assert!(!results[0].1 && !results[1].1, "the first two calls ran: {results:?}");
    for (out, is_error) in &results[2..] {
        assert!(*is_error && out.contains("loop guard"), "refused: {out}");
    }
    let mut notes = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Note { text, .. } = ev {
            notes.push(text);
        }
    }
    assert!(notes.iter().any(|n| n.contains("loop guard")), "{notes:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// F-08: the check helper reports exit codes and output, the tail helper
/// never splits a character, a verify hand-back does not reset the turbo
/// streak, and verification is off by default.
#[tokio::test]
async fn verify_helpers_report_exit_codes_and_keep_the_turbo_cap() {
    let dir = std::env::temp_dir();
    let abort = AtomicBool::new(false);
    let t = std::time::Duration::from_secs(30);
    let (code, out) = run_verify_command("echo verify-ok", &dir, t, &abort).await.unwrap();
    assert_eq!(code, Some(0));
    assert!(out.contains("verify-ok"), "{out}");
    let (code, _) = run_verify_command("exit 3", &dir, t, &abort).await.unwrap();
    assert_eq!(code, Some(3));
    assert_eq!(tail_chars("abcdef", 3), "def");
    assert_eq!(tail_chars("äöü", 5), "äöü");

    let cfg = crate::config::Config::default();
    assert!(cfg.agent.verify_command.is_none(), "off unless configured");
    assert_eq!(cfg.agent.verify_max_rounds, 2);
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("v", None).unwrap();
    let text = format!("{TURBO_CONTINUE_PREFIX} keep going");
    st.db.add_message(&s.id, "user", &[Part::Text { text }]).unwrap();
    let text = format!("{VERIFY_PREFIX} `x` failed (exit 1).");
    st.db.add_message(&s.id, "user", &[Part::Text { text }]).unwrap();
    assert_eq!(turbo_streak(&st, &s.id), 1, "a verify hand-back neither counts nor resets");
}

#[test]
fn the_project_check_is_found_without_configuration() {
    let dir = std::env::temp_dir().join(format!("ocrs-detect-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("tests")).unwrap();
    assert_eq!(detect_verify_command(&dir), None);
    std::fs::write(dir.join("tests").join("test_app.py"), "").unwrap();
    assert_eq!(detect_verify_command(&dir).as_deref(), Some("python -m pytest -q"));
    std::fs::write(dir.join("package.json"), r#"{"scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#)
        .unwrap();
    assert_eq!(detect_verify_command(&dir).as_deref(), Some("python -m pytest -q"), "npm's placeholder is not a test");
    std::fs::write(dir.join("package.json"), r#"{"scripts":{"test":"vitest run"}}"#).unwrap();
    assert_eq!(detect_verify_command(&dir).as_deref(), Some("npm test --silent"));
    std::fs::write(dir.join("Cargo.toml"), "[package]").unwrap();
    assert_eq!(detect_verify_command(&dir).as_deref(), Some("cargo check --quiet"));
    assert!(command_unavailable("/usr/bin/python: No module named pytest"));
    assert!(!command_unavailable("FAILED tests/test_app.py::test_x"));
    assert_eq!(failure_count("ok\nerror: a\nFAILED b\n"), 2);
    assert_eq!(failure_count(""), 1, "a failing check is at least one problem");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Keep-best: a fix round that leaves more problems is undone, and the
/// next round starts from the better state.
#[tokio::test]
async fn a_fix_round_that_makes_things_worse_is_undone() {
    if crate::tools::shell_kind() != crate::permissions::Shell::Posix {
        return; // the check below is a POSIX shell script
    }
    use axum::{routing::post, Router};
    fn write_call(id: &str, path: &str, content: &str) -> String {
        let args = serde_json::json!({"path": path, "content": content}).to_string();
        let frame = serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0, "id": id, "function": {"name": "write", "arguments": args}
        }]}, "finish_reason": null}]});
        format!(
            "data: {frame}\n\ndata: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        )
    }
    let dir = std::env::temp_dir().join(format!("ocrs-keepbest-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|body: axum::extract::Json<Value>| async move {
            let msgs = body["messages"].as_array().cloned().unwrap_or_default();
            let verifies = msgs
                .iter()
                .filter(|m| m["role"] == "user" && m["content"].as_str().unwrap_or("").starts_with("[verify]"))
                .count();
            let payload = if msgs.last().is_some_and(|m| m["role"] == "tool") {
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string()
            } else {
                match verifies {
                    0 => write_call("c0", "errors.txt", "error a\n"),
                    1 => write_call("c1", "errors.txt", "error a\nerror b\nerror c\n"),
                    _ => write_call("c2", "fixed.txt", "ok\n"),
                }
            };
            ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], payload)
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let mut cfg = crate::config::Config::default();
    cfg.agent.auto_review = false;
    cfg.agent.verify_command = Some("test -f fixed.txt && exit 0; cat errors.txt; exit 1".into());
    cfg.permissions.defaults.insert("write".into(), "allow".into());
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("kb", None).unwrap();
    let mut rx = st.events.subscribe();
    run_agent(&st, &s.id, "make it", Arc::new(AtomicBool::new(false))).await;
    let mut notes = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let ServerEvent::Note { text, .. } = ev {
            notes.push(text);
        }
    }
    assert!(notes.iter().any(|n| n.contains("round 1 did not help (1 → 3 problem(s)); put back 1 file")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("passed after 2 fix round(s)")), "{notes:?}");
    // The fixed failure became a lesson that later prompts carry.
    let lessons = load_lessons(&st, &dir).expect("a lesson was saved");
    assert!(
        lessons.starts_with("- ") && lessons.contains("failed (exit 1)") && lessons.contains("fixed by: done"),
        "{lessons}"
    );
    // The next chat carries it; this chat keeps its prompt (prompt cache).
    let next = st.db.create_session("next", None).unwrap();
    assert!(st.composed_system(&next.id).contains("## Lessons from earlier runs in this project"));
    assert!(
        !st.composed_system(&s.id).contains("## Lessons from earlier runs"),
        "same prompt for the rest of this chat"
    );
    record_lesson(&st, &dir, "1. `x` failed (exit 1)", "again");
    assert_eq!(load_lessons(&st, &dir).unwrap().lines().count(), 2, "a new problem adds a line");
    record_lesson(&st, &dir, "1. `x` failed (exit 1)", "third");
    assert_eq!(load_lessons(&st, &dir).unwrap().lines().count(), 2, "the same problem replaces its line");
    let _ = std::fs::remove_file(lessons_path(&st, &dir));
    assert_eq!(std::fs::read_to_string(dir.join("errors.txt")).unwrap(), "error a\n", "the worse round was undone");
    let msgs = st.db.messages(&s.id).unwrap();
    assert!(msgs.iter().any(|m| m
        .parts
        .iter()
        .any(|p| matches!(p, Part::Text { text } if text.contains("its file changes were undone")))));
    let _ = std::fs::remove_dir_all(&dir);
}

/// F-08 end to end: the run writes a file, the check fails, the agent gets
/// the output as a `[verify]` message, fixes it, and the check passes.
#[tokio::test]
async fn verify_failure_runs_a_fix_round_until_the_check_passes() {
    use axum::{routing::post, Router};
    fn write_call(id: &str, path: &str) -> String {
        let args = serde_json::json!({"path": path, "content": "ok\n"}).to_string();
        let frame = serde_json::json!({"choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": id,
            "function": {"name": "write", "arguments": args}
        }]}, "finish_reason": null}]});
        format!(
            "data: {frame}\n\n\
                 data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
                 data: [DONE]\n\n"
        )
    }
    let dir = std::env::temp_dir().join(format!("ocrs-verify-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // The check passes only once `fixed.txt` exists.
    let check = match crate::tools::shell_kind() {
        crate::permissions::Shell::Posix => "test -f fixed.txt",
        crate::permissions::Shell::Cmd => "dir fixed.txt",
    };
    let mut cfg = crate::config::Config::default();
    cfg.agent.auto_review = false;
    cfg.agent.verify_command = Some(check.to_string());
    cfg.permissions.defaults.insert("write".into(), "allow".into());
    cfg.permissions.approval_timeout_secs = 2;
    // First request: write made.txt. After a `[verify]` message: write
    // fixed.txt. After any tool result: a short text answer.
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|body: axum::extract::Json<Value>| async move {
            let msgs = body["messages"].as_array().cloned().unwrap_or_default();
            let last = msgs.last().cloned().unwrap_or_default();
            let payload = if last["role"] == "tool" {
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                    .to_string()
            } else if last["content"].as_str().unwrap_or("").starts_with("[verify]") {
                write_call("call_fix", "fixed.txt")
            } else {
                write_call("call_make", "made.txt")
            };
            (
                [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                payload,
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    cfg.provider.base_url = format!("http://{addr}/v1");
    cfg.provider.model = "mock".into();
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("verify", None).unwrap();
    let mut rx = st.events.subscribe();

    run_agent(&st, &s.id, "make a marker", Arc::new(AtomicBool::new(false))).await;

    assert!(dir.join("fixed.txt").is_file(), "the fix round ran");
    let msgs = st.db.messages(&s.id).unwrap();
    assert!(
        msgs.iter().any(|m| m.role == "user"
            && m.parts.iter().any(|p| matches!(p, Part::Text { text }
                if text.starts_with("[verify]") && text.contains("failed (exit 1)")))),
        "the failure was handed back to the agent"
    );
    let mut notes = Vec::new();
    let mut finished = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        match ev {
            ServerEvent::Note { text, .. } => notes.push(text),
            ServerEvent::RunFinished { ok, .. } => finished.push(ok),
            _ => {}
        }
    }
    let failed_at = notes.iter().position(|n| n.contains("failed")).expect("failure note");
    let passed_at = notes.iter().position(|n| n.contains("passed")).expect("pass note");
    assert!(failed_at < passed_at, "{notes:?}");
    assert_eq!(finished, vec![true], "one completion, after the verify loop");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The estimator counts about 3 characters per token on purpose
/// (`estimate_tokens`): code and tool results tokenize worse than prose,
/// and measuring low would let requests overflow a small window.
#[test]
fn token_estimate_is_about_three_chars_per_token() {
    let msgs =
        vec![ChatMsg { role: "user".into(), content: Some("x".repeat(4000)), tool_calls: None, tool_call_id: None }];
    let n = estimate_tokens(&msgs, &[]);
    assert!((1330..1345).contains(&n), "{n}");
}

#[tokio::test]
async fn auto_compaction_runs_near_the_context_limit() {
    let dir = std::env::temp_dir().join(format!("ocrs-autocompact-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cfg = crate::config::Config::default();
    cfg.provider.base_url = mock_text_llm().await; // answers "Hello" = the summary
    cfg.provider.model = "stub".into();
    cfg.agent.context_tokens = 400; // tiny on purpose
    let st = build_state_ephemeral(cfg, dir.clone()).await;
    let s = st.db.create_session("auto", None).unwrap();
    for i in 0..12 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        let text = format!("message {i} {}", "x".repeat(200));
        st.db.add_message(&s.id, role, &[Part::Text { text }]).unwrap();
    }
    let mut events = st.events.subscribe();
    let abort = AtomicBool::new(false);
    let msgs = build_chat_messages(&st, &s.id, "system prompt").unwrap();
    assert!(auto_compact_if_needed(&st, &s.id, &msgs, &[], &abort).await, "compacted");
    assert_eq!(st.db.messages(&s.id).unwrap().len(), 7, "6 kept + 1 summary");
    let mut noted = false;
    while let Ok(ev) = events.try_recv() {
        if let ServerEvent::Note { text, .. } = ev {
            noted |= text.contains("compacted");
        }
    }
    assert!(noted, "the user is told about the compaction");
    let _ = std::fs::remove_dir_all(&dir);
}

/// T-146: one tool call reads the workspace's approvals once, however many
/// (tool, pattern) pairs the rules ask about.
#[tokio::test]
async fn permission_check_reads_approvals_once_per_call() {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    st.set_perm_mode(PermissionMode::Manual);
    let ws = crate::db::workspace_key(&st.workspace_for("s1"));
    st.db.add_approval(&ws, "bash", "ls && pwd && whoami", "session").unwrap();
    crate::db::APPROVAL_QUERIES.with(|n| n.set(0));
    let decided = super::tool_exec::decided_without_prompt(&st, &st.db, "s1", "bash", "ls && pwd && whoami");
    assert_eq!(crate::db::APPROVAL_QUERIES.with(|n| n.get()), 1, "one approvals query per tool call");
    assert_eq!(decided, Some(true), "the saved approval still counts");
}

/// T-151: a chat's parents are read in one query and cached, so a grant on
/// a grandparent counts and the next check needs no database.
#[tokio::test]
async fn auto_approval_reads_the_parent_chain_at_once() {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let top = st.db.create_session("mission", None).unwrap();
    let step = st.db.create_session("step", Some(&top.id)).unwrap();
    let agent = st.db.create_session("agent", Some(&step.id)).unwrap();
    let other = st.db.create_session("other", None).unwrap();
    let grant = st.auto_approve_session(&top.id);
    assert_eq!(st.perm_mode_for_async(&agent.id).await, PermissionMode::Turbo);
    {
        let parents = st.parents.lock().unwrap();
        assert_eq!(parents.get(&agent.id), Some(&Some(step.id.clone())));
        assert_eq!(parents.get(&step.id), Some(&Some(top.id.clone())), "the whole chain is cached");
        assert_eq!(parents.get(&top.id), Some(&None));
    }
    assert_eq!(st.perm_mode_for_async(&other.id).await, PermissionMode::Default);
    assert_eq!(st.perm_mode_for_async("ses_unknown").await, PermissionMode::Default);
    assert!(!st.parents.lock().unwrap().contains_key("ses_unknown"), "an unknown chat is not cached");
    drop(grant);
    assert_eq!(st.perm_mode_for_async(&agent.id).await, PermissionMode::Default);
}

/// T-148: the async folder lookup shares the cache with the sync one.
#[tokio::test]
async fn async_folder_lookup_uses_the_same_cache() {
    let base = std::env::temp_dir().join(format!("ocrs-ws-async-{}", std::process::id()));
    let (start, other) = (base.join("start"), base.join("other"));
    std::fs::create_dir_all(&start).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    let st = build_state_ephemeral(crate::config::Config::default(), start.clone()).await;
    let project = st.db.upsert_project(&other).unwrap();
    let s = st.db.create_session_in("cached", None, Some(&project.id)).unwrap();
    assert_eq!(st.workspace_for_async(&s.id).await, other);
    st.db.set_session_project(&s.id, None).unwrap();
    assert_eq!(st.workspace_for(&s.id), other, "the async lookup filled the cache");
    assert_eq!(st.workspace_for_async(&s.id).await, other, "answered from the cache");
    assert_eq!(st.workspace_for_async("ses_unknown").await, start);
    assert!(!st.workspaces.lock().unwrap().contains_key("ses_unknown"), "an unknown chat is not cached");
    let _ = std::fs::remove_dir_all(&base);
}

/// T-152: cache.log lines go through one kept handle and keep their format.
#[tokio::test]
async fn cache_log_lines_keep_their_format() {
    let st = build_state_ephemeral(crate::config::Config::default(), std::env::temp_dir()).await;
    let sid = format!("ses_cachelog{}", crate::db::now_ms_for_tests() % 100_000_000);
    let u = crate::provider::Usage { input: 200, cached: 50, cache_write: 0, output: 7 };
    log_cache(&st, &sid, Some(&u), "same start");
    log_cache(&st, &sid, None, "system prompt changed");
    let log = std::fs::read_to_string(st.state_dir.join("cache.log")).unwrap();
    let ours: Vec<&str> = log.lines().filter(|l| l.contains(short_id(&sid))).collect();
    assert_eq!(ours.len(), 2, "{log}");
    assert!(ours[0].ends_with(" in=200 cached=50 (25%) out=7 same start"), "{}", ours[0]);
    assert!(ours[1].ends_with(" no usage reported system prompt changed"), "{}", ours[1]);
}
