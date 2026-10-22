//! M2: `/compact` context compaction — summarize older history into one
//! system message and keep the recent tail intact (the schema preserves
//! ordering, so the summary slots in right before the kept messages).

use crate::server::{Shared, ServerEvent};
use crate::provider::{ChatMsg, StreamEvent};
use anyhow::{bail, Result};
use futures::StreamExt;
use std::sync::atomic::{AtomicBool, Ordering};

/// Messages at the end that always survive compaction untouched.
pub const KEEP_TAIL: usize = 6;
/// Minimum compressible prefix worth summarizing.
pub const MIN_COMPRESS: usize = 4;
/// Character budget for the serialized prefix sent to the summarizer.
pub const BUDGET: usize = 60_000;

#[derive(Debug, Clone, PartialEq)]
pub struct CompactOutcome {
    pub removed: usize,
    pub kept: usize,
    pub summary: String,
}

/// Where the compressible prefix ends. `None` = nothing worth compacting.
pub fn split_at(len: usize) -> Option<usize> {
    if len <= KEEP_TAIL + MIN_COMPRESS {
        return None;
    }
    Some(len - KEEP_TAIL)
}

/// Serialize the prefix for the summarizer, keeping the *most recent* part
/// when it exceeds the budget (recent context matters most).
pub fn render_transcript(lines: &[String]) -> String {
    let text = lines.join("\n");
    if text.len() <= BUDGET {
        return text;
    }
    let mut start = text.len() - BUDGET;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    format!("… [older transcript truncated]\n{}", &text[start..])
}

pub const SUMMARY_MARKER: &str = "[context compacted]";

/// Like [`split_at`], but the kept tail never starts with a `tool` message:
/// the assistant message that asked for those tool calls must stay in front
/// of its results (providers reject a tool result without its call). The
/// split moves earlier (keeping more) when it can, otherwise later.
pub fn safe_split(msgs: &[crate::db::Message]) -> Option<usize> {
    let start = split_at(msgs.len())?;
    let is_tool = |i: usize| msgs.get(i).is_some_and(|m| m.role == "tool");
    let mut s = start;
    while s > 0 && is_tool(s) {
        s -= 1;
    }
    if s >= MIN_COMPRESS && !is_tool(s) {
        return Some(s);
    }
    let mut s = start;
    while s < msgs.len() && is_tool(s) {
        s += 1;
    }
    (s < msgs.len()).then_some(s)
}

/// Limits for the summarizer's transcript: long enough to keep real detail.
pub const COMPACT_LIMITS: crate::sessions::FlattenLimits =
    crate::sessions::FlattenLimits { text: 4000, reasoning: 0, tool_args: 1000, tool_result: 1500 };

/// One transcript line per message for the summarizer: `role: text`. An
/// earlier summary is kept whole, so a second `/compact` does not cut the
/// first summary down.
pub fn transcript_lines(msgs: &[crate::db::Message]) -> Vec<String> {
    msgs.iter()
        .map(|m| {
            let texts: Vec<&str> = m
                .parts
                .iter()
                .filter_map(|p| match p {
                    crate::db::Part::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            let earlier_summary =
                m.role == "system" && texts.iter().any(|t| t.starts_with(SUMMARY_MARKER));
            if earlier_summary {
                format!("system: {}", texts.join("\n"))
            } else {
                format!("{}: {}", m.role, crate::sessions::flatten_msg_with(m, COMPACT_LIMITS))
            }
        })
        .collect()
}

async fn summarize(st: &Shared, transcript: &str, abort: &AtomicBool) -> Result<String> {
    let msgs = vec![
        ChatMsg {
            role: "system".into(),
            content: Some(
                "You are a context-compression assistant. Summarize the conversation \
                 transcript below for an AI coding agent that will continue working. \
                 Preserve: the user's goals and constraints, decisions made, files \
                 touched, current state, and unresolved threads. Be dense and factual. \
                 Reply with the summary only — no preamble."
                    .into(),
            ),
            tool_calls: None,
            tool_call_id: None,
        },
        ChatMsg {
            role: "user".into(),
            content: Some(transcript.to_string()),
            tool_calls: None,
            tool_call_id: None,
        },
    ];
    let client = st.client.lock().await;
    let model = st
        .cfg
        .provider
        .small_model
        .clone()
        .unwrap_or_else(|| client.model.clone());
    let mut stream = client.stream(&model, &msgs, &[]).await?;
    drop(client);
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        if abort.load(Ordering::SeqCst) {
            bail!("aborted");
        }
        match ev? {
            StreamEvent::Text(t) => text.push_str(&t),
            StreamEvent::Done { finish_reason } => {
                // B-03: a cut-off summary would replace real history.
                if finish_reason.as_deref() == Some("length") {
                    bail!(
                        "the summary was cut off by the model's output token limit; \
                         history was left unchanged"
                    );
                }
                break;
            }
            _ => {}
        }
    }
    Ok(text.trim().to_string())
}

/// Compact a session's history. `Err` when there's too little to compact
/// or the summarizer call fails (the transcript is left untouched).
pub async fn compact(
    st: &Shared,
    session_id: &str,
    abort: &AtomicBool,
) -> Result<CompactOutcome> {
    let msgs = st.db.messages(session_id)?;
    let Some(split) = safe_split(&msgs) else {
        bail!(
            "nothing to compact — only {} message(s) (need > {}), or no safe split point",
            msgs.len(),
            KEEP_TAIL + MIN_COMPRESS
        );
    };
    let prefix: Vec<_> = msgs[..split].to_vec();
    let tail = &msgs[split..];
    // Only these are deleted later; anything newer survives.
    let summarized_ids: Vec<String> = prefix.iter().map(|m| m.id.clone()).collect();

    let lines = transcript_lines(&prefix);
    let transcript = render_transcript(&lines);
    let summary = summarize(st, &transcript, abort).await?;
    if summary.is_empty() {
        bail!("summarizer returned an empty summary — compaction cancelled");
    }
    let wrapped = format!("{SUMMARY_MARKER} ({} earlier messages summarized)\n{summary}", prefix.len());
    st.db.replace_with_summary(session_id, &summarized_ids, &wrapped)?;
    let _ = st.events.send(ServerEvent::AssistantDelta {
        session_id: session_id.to_string(),
        text: String::new(), // no streaming text; the caller reports the outcome
    });
    Ok(CompactOutcome { removed: prefix.len(), kept: tail.len(), summary: wrapped })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_thresholds() {
        assert_eq!(split_at(0), None);
        assert_eq!(split_at(10), None); // 6 tail + 4 min
        assert_eq!(split_at(11), Some(5));
        assert_eq!(split_at(30), Some(24));
    }

    fn msg(role: &str, text: &str) -> crate::db::Message {
        crate::db::Message {
            id: format!("m-{role}-{}", text.len()),
            session_id: "s".into(),
            role: role.into(),
            parts: vec![crate::db::Part::Text { text: text.into() }],
            created_at: 0,
        }
    }

    #[test]
    fn kept_tail_never_starts_with_a_tool_result() {
        // 12 messages; plain split_at would start the tail at index 6.
        let mut msgs: Vec<crate::db::Message> = (0..12).map(|i| msg("user", &format!("u{i}"))).collect();
        msgs[5] = msg("assistant", "calls two tools");
        msgs[6] = msg("tool", "result 1");
        msgs[7] = msg("tool", "result 2");
        assert_eq!(split_at(msgs.len()), Some(6));
        assert_eq!(safe_split(&msgs), Some(5), "the tool call stays with its results");

        // No room to move back: move forward past the tool results instead.
        let mut short: Vec<crate::db::Message> = (0..11).map(|i| msg("user", &format!("u{i}"))).collect();
        short[0] = msg("assistant", "calls five tools");
        for m in short.iter_mut().take(6).skip(1) {
            *m = msg("tool", "r");
        }
        assert_eq!(safe_split(&short), Some(6));
    }

    #[test]
    fn transcript_has_roles_and_keeps_an_earlier_summary_whole() {
        let earlier = format!("{SUMMARY_MARKER} (5 earlier messages summarized)
{}", "fact ".repeat(200));
        let msgs = vec![
            msg("system", &earlier),
            msg("user", "please refactor the parser"),
            msg("assistant", "done"),
        ];
        let lines = transcript_lines(&msgs);
        assert!(lines[0].starts_with("system: [context compacted]"), "{}", lines[0]);
        assert!(lines[0].len() > 1000, "earlier summary not cut to 400 characters");
        assert_eq!(lines[1], "user: please refactor the parser");
        assert_eq!(lines[2], "assistant: done");
    }

    #[test]
    fn render_keeps_recent_within_budget() {
        let lines: Vec<String> = (0..5).map(|i| format!("user: m{i}")).collect();
        assert_eq!(render_transcript(&lines).matches('\n').count(), 4);

        // Over budget: oldest dropped, newest kept, marker present.
        let big: Vec<String> = (0..200).map(|i| format!("user: line {i} {}", "x".repeat(500))).collect();
        let out = render_transcript(&big);
        assert!(out.len() <= BUDGET + 60);
        assert!(out.starts_with("… [older transcript truncated]"));
        assert!(out.contains("line 199"), "recent content preserved");
        assert!(!out.contains("line 0 "), "oldest dropped");
    }

    /// End-to-end with a mock provider: 12 messages → compaction keeps the
    /// last 6 + one system summary; a short session refuses to compact.
    #[tokio::test]
    async fn compact_summarizes_and_keeps_tail() {
        use crate::db::Part;
        let dir = std::env::temp_dir().join(format!(
            "ocrs-compact-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Mock: any summarize request answers with fixed text.
        let mock_url = {
            use axum::{routing::post, Router};
            let app = Router::new().route(
                "/v1/chat/completions",
                post(|_body: axum::extract::Json<serde_json::Value>| async move {
                    let payload = concat!(
                        "data: {\"choices\":[{\"delta\":{\"content\":\"EARLY-PHASE-SUMMARY\"},\"finish_reason\":null}]}\n\n",
                        "data: [DONE]\n\n"
                    );
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
            format!("http://{addr}/v1")
        };

        let mut cfg = crate::config::Config::default();
        cfg.provider.base_url = mock_url;
        cfg.provider.model = "mock".into();
        let st = crate::server::build_state_ephemeral(cfg, dir.clone()).await;
        let s = st.db.create_session("c", None).unwrap();

        // Too little history → refused, nothing changed.
        let abort = AtomicBool::new(false);
        st.db.add_message(&s.id, "user", &[Part::Text { text: "hi".into() }]).unwrap();
        let err = compact(&st, &s.id, &abort).await.unwrap_err();
        assert!(err.to_string().contains("nothing to compact"));
        assert_eq!(st.db.messages(&s.id).unwrap().len(), 1);

        // 12 messages total → 6 compressed, 6 kept.
        for i in 1..12 {
            let role = if i % 2 == 1 { "user" } else { "assistant" };
            st.db.add_message(&s.id, role, &[Part::Text { text: format!("msg{i}") }]).unwrap();
        }
        let before = st.db.messages(&s.id).unwrap();
        assert_eq!(before.len(), 12);
        let tail_ids: Vec<String> = before[6..].iter().map(|m| m.id.clone()).collect();

        let out = compact(&st, &s.id, &abort).await.unwrap();
        assert_eq!(out.removed, 6);
        assert_eq!(out.kept, 6);
        assert!(out.summary.contains("EARLY-PHASE-SUMMARY"));
        assert!(out.summary.starts_with(SUMMARY_MARKER));

        let after = st.db.messages(&s.id).unwrap();
        assert_eq!(after.len(), 7, "6 kept + 1 summary");
        assert_eq!(after[0].role, "system");
        match &after[0].parts[0] {
            Part::Text { text } => assert!(text.contains("EARLY-PHASE-SUMMARY")),
            other => panic!("unexpected {other:?}"),
        }
        // The tail survived verbatim and in order, right after the summary.
        let after_tail: Vec<String> = after[1..].iter().map(|m| m.id.clone()).collect();
        assert_eq!(after_tail, tail_ids);
        assert!(
            after[1].parts.iter().any(|p| matches!(p, Part::Text { text } if text == "msg6")),
            "kept tail starts at msg6 (index 6 of 12)"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
