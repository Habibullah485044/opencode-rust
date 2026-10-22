//! Drawing: the Freebuff-CLI palette, the frame layout, and the widgets.
//! The team and spawned-agent panels share [`draw_agent_card`].

use super::state::{wrap_n, App, EntryKind, TeamPhase, TeamStatus};
use crate::commands::{builtins, help_text};
use ratatui::{
    prelude::*,
    widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap},
};

// ---- Freebuff-CLI dark palette (taken from Freebuff's own theme source) ----
/// Accent green: user lines, brand chip, focused input border.
const FB_GREEN: Color = Color::Rgb(0x9E, 0xFC, 0x62);
/// Near-black text used ON green fills (Freebuff's inverted CTA fg).
const FB_INK: Color = Color::Rgb(0x10, 0x13, 0x1a);
/// Main text.
const FB_FG: Color = Color::Rgb(0xf1, 0xf5, 0xf9);
/// Dimmed text: thinking, placeholders, footer hints.
const FB_MUTED: Color = Color::Rgb(0xac, 0xb3, 0xbf);
/// Panel borders.
const FB_BORDER: Color = Color::Rgb(0x53, 0x61, 0x75);
/// Bar / panel background.
const FB_SURFACE: Color = Color::Rgb(0x20, 0x23, 0x27);
/// AI-side markers (tool lines).
const FB_AI: Color = Color::Rgb(0x6b, 0x72, 0x80);
/// System notes.
const FB_SECONDARY: Color = Color::Rgb(0xa3, 0xae, 0xd0);
const FB_SUCCESS: Color = Color::Rgb(0x22, 0xc5, 0x5e);
const FB_ERROR: Color = Color::Rgb(0xef, 0x44, 0x44);
const FB_WARNING: Color = Color::Rgb(0xFF, 0xA5, 0x00);
/// Per-agent accents for team cards (existing palette only).
const TEAM_ACCENTS: [Color; 4] = [FB_GREEN, FB_SECONDARY, FB_WARNING, FB_AI];
/// Fixed team-panel height: outer borders + 6-row cards.
const TEAM_H: u16 = 8;
pub fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    // Header — Freebuff style: dark surface bar, green brand chip, muted details.
    // Busy spinner: the loop redraws ~20×/s so the frame keeps moving.
    const SPIN: [&str; 4] = ["◐", "◓", "◑", "◒"];
    let status = if app.busy {
        format!("{} working", SPIN[app.tick as usize % SPIN.len()])
    } else {
        "ready".to_string()
    };
    let session = app.session_id.as_deref().unwrap_or("-");
    let api = match app.api {
        Some((true, ms)) => {
            Span::styled(format!("api ok {ms}ms"), Style::default().fg(FB_SUCCESS))
        }
        Some((false, _)) => Span::styled("api DOWN", Style::default().fg(FB_ERROR)),
        None => Span::styled("api …", Style::default().fg(FB_MUTED)),
    };
    let sep = || Span::styled(" │ ", Style::default().fg(FB_BORDER));
    // Left: who/where/state. Right: connection and model facts, right-aligned
    // so the bar reads as a status strip instead of a word pile.
    let mut left = vec![
        Span::styled(
            " opencode-rust ",
            Style::default()
                .fg(FB_INK)
                .bg(FB_GREEN)
                .add_modifier(Modifier::BOLD),
        ),
        sep(),
        Span::styled(format!("session {session}"), Style::default().fg(FB_MUTED)),
        sep(),
        Span::styled(format!("mode {}", app.mode.as_str()), Style::default().fg(FB_MUTED)),
        sep(),
        Span::styled(
            status,
            Style::default().fg(if app.busy { FB_WARNING } else { FB_SUCCESS }),
        ),
    ];
    if app.turbo {
        left.push(sep());
        left.push(Span::styled(
            " TURBO ",
            Style::default()
                .fg(FB_INK)
                .bg(FB_WARNING)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let mut right: Vec<Span> = Vec::new();
    if let Some(m) = &app.model {
        right.push(Span::styled(format!("model {m}"), Style::default().fg(FB_SECONDARY)));
        right.push(sep());
    }
    right.push(api);
    if let Some((added, deleted, hints)) = app.stats {
        right.push(sep());
        right.push(Span::styled(
            format!("debt +{added} −{deleted} · {hints} hint(s)"),
            Style::default().fg(FB_SECONDARY),
        ));
    }
    // B-35: terminal columns, not characters.
    let right_w = right.iter().map(|s| s.width()).sum::<usize>().min(u16::MAX as usize) as u16;
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(right_w)])
        .split(chunks[0]);
    f.render_widget(
        Paragraph::new(Line::from(left)).style(Style::default().bg(FB_SURFACE)),
        cols[0],
    );
    f.render_widget(
        Paragraph::new(Line::from(right)).style(Style::default().bg(FB_SURFACE)),
        cols[1],
    );

    // Live panels above the transcript: team cards and/or spawned-agent
    // cards (the transcript below doubles as the activity log). The agents
    // panel wins contested space.
    let show_agents = !app.agents.is_empty() && chunks[1].height >= TEAM_H + 6;
    let show_team = app.team.is_some()
        && chunks[1].height >= TEAM_H + 6 + if show_agents { TEAM_H } else { 0 };
    let transcript_area = if show_team || show_agents {
        let mut cons: Vec<Constraint> = Vec::new();
        if show_team {
            cons.push(Constraint::Length(TEAM_H));
        }
        if show_agents {
            cons.push(Constraint::Length(TEAM_H));
        }
        cons.push(Constraint::Min(3));
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints(cons)
            .split(chunks[1]);
        let mut i = 0;
        if show_team {
            draw_team(f, app, parts[i]);
            i += 1;
        }
        if show_agents {
            draw_agents(f, app, parts[i]);
            i += 1;
        }
        parts[i]
    } else {
        chunks[1]
    };

    // Transcript
    let mut lines: Vec<Line> = Vec::new();
    for e in &app.entries {
        let style = match e.kind {
            EntryKind::User => Style::default().fg(FB_GREEN).add_modifier(Modifier::BOLD),
            EntryKind::Assistant => Style::default().fg(FB_FG),
            EntryKind::Thinking => {
                Style::default().fg(FB_MUTED).add_modifier(Modifier::ITALIC)
            }
            EntryKind::Tool => Style::default().fg(FB_AI),
            EntryKind::System => Style::default().fg(FB_SECONDARY),
            EntryKind::Error => Style::default().fg(FB_ERROR).add_modifier(Modifier::BOLD),
        };
        // Uniform 2-space gutter; user turns get a `›` marker in it.
        let marker = if e.kind == EntryKind::User { "› " } else { "  " };
        for l in e.text.lines() {
            lines.push(Line::from(format!("{marker}{l}")).style(style));
        }
    }
    // B-14: scroll by screen rows AFTER wrapping. Counting logical lines
    // hid the newest output whenever long lines wrapped onto several rows.
    let inner_w = transcript_area.width.saturating_sub(2);
    let view_h = transcript_area.height.saturating_sub(2) as usize;
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(inner_w); // rows; no block set yet
    let max_scroll = total.saturating_sub(view_h);
    app.scroll_max.set(max_scroll.min(u16::MAX as usize) as u16);
    let up = (app.scroll as usize).min(max_scroll);
    let top = max_scroll - up; // first row shown
    // The title counts the scrollback so nobody wonders where the rest went.
    let title = if top > 0 {
        format!(" transcript · ↑{top} earlier ")
    } else {
        " transcript ".to_string()
    };
    let transcript = paragraph
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(FB_BORDER))
                .title(title),
        )
        .scroll((top.min(u16::MAX as usize) as u16, 0));
    f.render_widget(transcript, transcript_area);

    // Input
    let placeholder = if app.busy {
        " agent working — input queued until ready"
    } else {
        " type your message…"
    };
    // B-35: show the slice around the cursor, measured in terminal columns.
    let input_w = chunks[2].width.saturating_sub(2) as usize;
    let (input_text, cursor_col) = if app.input.is_empty() {
        (placeholder.to_string(), 0)
    } else {
        super::state::input_view(&app.input, app.cursor, input_w)
    };
    let input = Paragraph::new(Span::styled(
        input_text,
        Style::default().fg(if app.input.is_empty() {
            FB_AI
        } else {
            FB_FG
        }),
    ))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(if app.busy {
                FB_BORDER
            } else {
                FB_GREEN
            }))
            .title(" message "),
    );
    f.render_widget(input, chunks[2]);
    if !app.busy {
        let x = chunks[2].x + 1 + cursor_col.min(u16::MAX as usize) as u16;
        let y = chunks[2].y + 1;
        if x < chunks[2].right().saturating_sub(1) {
            f.set_cursor_position((x, y));
        }
    }

    // M6: command palette — floats directly above the input box.
    if let Some(pal) = &app.palette {
        let want = pal.matches.len() as u16 + 2;
        let h = want.min(chunks[2].y.saturating_sub(1));
        if h >= 3 {
            let area = Rect {
                x: chunks[2].x,
                y: chunks[2].y - h,
                width: chunks[2].width.min(64),
                height: h,
            };
            f.render_widget(Clear, area);
            let mut lines: Vec<Line> = Vec::new();
            for (i, &idx) in pal.matches.iter().take(h as usize - 2).enumerate() {
                let b = &builtins()[idx];
                let sel = i == pal.sel;
                lines.push(
                    Line::from(format!(" {:<26} {}", b.usage(), b.help)).style(if sel {
                        Style::default()
                            .fg(FB_INK)
                            .bg(FB_GREEN)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(FB_FG)
                    }),
                );
            }
            let block = Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(FB_BORDER))
                    .title(" commands "),
            );
            f.render_widget(block, area);
        }
    }

    // Footer hint — inverted green CTA bar when action is required.
    let (hint, attention) = if app.approval.is_some() {
        (" APPROVAL NEEDED: [y] once  [a] always  [n/Esc] deny ", true)
    } else if app.pending_edit.is_some() {
        (" EDIT PREVIEW: [y] apply  [n/Esc] reject ", true)
    } else if app.palette.is_some() {
        (" ↑↓ select   Tab complete   Enter run   Esc dismiss ", false)
    } else {
        (
            " Enter send · Tab complete · / commands · F1 help · ↑↓ scroll · /team /edit /parallel ",
            false,
        )
    };
    let footer = Paragraph::new(hint).style(if attention {
        Style::default()
            .fg(FB_INK)
            .bg(FB_GREEN)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(FB_MUTED).bg(FB_SURFACE)
    });
    f.render_widget(footer, chunks[3]);

    // Approval overlay — centered box on top (rendered last = on top).
    // B-15: the overlays below are skipped while one that gets the keys
    // first (state.rs `on_key`) is open, so nothing ever covers it.
    draw_approval(f, app);

    // Mission picker overlay — Freebuff's Missions dialog layout:
    // mission list with the chosen one checked, effort row underneath.
    let covered = app.approval.is_some() || app.pending_edit.is_some();
    if let Some(mp) = app.mission_picker.as_ref().filter(|_| !covered) {
        let area = centered_rect(60, 45, f.area());
        f.render_widget(Clear, area);
        let mut lines: Vec<Line> = vec![
            Line::from("Missions").style(Style::default().fg(FB_INK).add_modifier(Modifier::BOLD)),
            Line::from(""),
        ];
        for (i, k) in crate::mission::MissionKind::all().iter().enumerate() {
            let checked = if i == mp.kind_idx { "✓" } else { " " };
            lines.push(
                Line::from(format!("  {checked} {}", k.label()))
                    .style(if i == mp.kind_idx {
                        Style::default().fg(FB_GREEN).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(FB_FG)
                    }),
            );
        }
        lines.push(Line::from(""));
        lines.push(
            Line::from("Mission effort")
                .style(Style::default().fg(FB_INK).add_modifier(Modifier::BOLD)),
        );
        let row: Vec<Span> = crate::mission::Effort::all()
            .iter()
            .enumerate()
            .flat_map(|(i, e)| {
                let sel = i == mp.effort_idx;
                let mut v = vec![Span::styled(
                    format!(" {} ", e.label()),
                    if sel {
                        Style::default()
                            .fg(FB_INK)
                            .bg(FB_GREEN)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(FB_MUTED)
                    },
                )];
                if i + 1 < crate::mission::Effort::all().len() {
                    v.push(Span::styled("·", Style::default().fg(FB_BORDER)));
                }
                v
            })
            .collect();
        lines.push(Line::from(row));
        lines.push(Line::from(""));
        lines.push(
            Line::from("[↑↓] mission  [←→] effort  [Enter] run  [Esc] cancel")
                .style(Style::default().fg(FB_GREEN)),
        );
        let block = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(FB_BORDER))
                    .title(" mission "),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(block, area);
    }

    // M3: inline-edit diff preview overlay (B-15: never over an approval).
    if let Some(pe) = app.pending_edit.as_ref().filter(|_| app.approval.is_none()) {
        let area = centered_rect(80, 70, f.area());
        f.render_widget(Clear, area);
        let mut lines: Vec<Line> = vec![
            Line::from(format!(" edit {}  (first change at line {})", pe.path, pe.line))
                .style(Style::default().fg(FB_GREEN).add_modifier(Modifier::BOLD)),
            Line::from(""),
        ];
        for d in &pe.diff {
            let style = if d.starts_with('+') {
                Style::default().fg(FB_SUCCESS)
            } else if d.starts_with('-') {
                Style::default().fg(FB_ERROR)
            } else {
                Style::default().fg(FB_MUTED)
            };
            lines.push(Line::from(d.clone()).style(style));
        }
        lines.push(Line::from(""));
        lines.push(Line::from("[y] apply   [n/Esc] reject"));
        let block = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(FB_BORDER))
                    .title(" edit preview "),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(block, area);
    }

    // M6: `/help` reference overlay (also F1). Any key closes it.
    if app.help && !covered && app.mission_picker.is_none() && app.sessions.is_none() {
        let area = centered_rect(76, 84, f.area());
        f.render_widget(Clear, area);
        let block = Paragraph::new(help_text())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(FB_BORDER))
                    .title(" help — any key to close "),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(block, area);
    }

    // M6: `/sessions` browser (newest first; ↳ marks a fork of another session).
    if let Some(ov) = app.sessions.as_ref().filter(|_| !covered && app.mission_picker.is_none()) {
        let area = centered_rect(72, 70, f.area());
        f.render_widget(Clear, area);
        let rows = area.height.saturating_sub(4) as usize;
        let mut lines: Vec<Line> = Vec::new();
        if ov.items.is_empty() {
            lines.push(Line::from("no sessions yet").style(Style::default().fg(FB_MUTED)));
        }
        for (i, s) in ov.items.iter().take(rows.max(1)).enumerate() {
            let sel = i == ov.sel;
            let fork = if s.parent_id.is_some() { "↳" } else { " " };
            lines.push(
                Line::from(format!(
                    " {fork} {} {}",
                    truncate_str(&s.id, 20),
                    // B-35: one row per session — line breaks would merge text.
                    truncate_str(&s.title.replace(['\r', '\n'], " "), 44)
                ))
                .style(if sel {
                    Style::default()
                        .fg(FB_INK)
                        .bg(FB_GREEN)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(FB_FG)
                }),
            );
        }
        lines.push(Line::from(""));
        lines.push(
            Line::from("[↑↓] select   [Enter] switch   [Esc] close")
                .style(Style::default().fg(FB_GREEN)),
        );
        let block = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(FB_BORDER))
                    .title(" sessions "),
            )
            .wrap(Wrap { trim: false });
        f.render_widget(block, area);
    }
}

/// Team panel body: header (goal + phase + verdict) with one card per
/// worker underneath. Medium cards, honest status — no fake progress bars.
fn draw_team(f: &mut Frame, app: &App, area: Rect) {
    let Some(team) = &app.team else { return };
    let mut title = format!(" team · {} · {} ", truncate_str(&team.goal, 40), team.phase.label());
    if team.phase == TeamPhase::Reviewing || team.phase == TeamPhase::Done {
        if let Some(v) = &team.verdict {
            title = format!(" team · {} · {} — {} ", truncate_str(&team.goal, 30), team.phase.label(), truncate_str(v, 40));
        }
    }
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(FB_BORDER))
        .title(title);
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    if inner.height < 4 || inner.width < 12 {
        return;
    }
    if team.agents.is_empty() {
        f.render_widget(
            Paragraph::new("spawning agents…").style(Style::default().fg(FB_MUTED)),
            inner,
        );
        return;
    }
    let n = team.agents.len().min(4);
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Ratio(1, n as u32); n])
        .split(inner);
    for (i, card_area) in cards.iter().enumerate() {
        let a = &team.agents[i];
        let accent = TEAM_ACCENTS[i % TEAM_ACCENTS.len()];
        let (mark, border) = match a.status {
            TeamStatus::Working => ("● working", accent),
            TeamStatus::Done => ("✓ done", FB_SUCCESS),
            TeamStatus::Failed => ("✗ failed", FB_ERROR),
        };
        let w = card_area.width.saturating_sub(2) as usize;
        draw_agent_card(
            f,
            *card_area,
            accent,
            border,
            &format!(
                "Agent {} · {}",
                i + 1,
                truncate_str(&a.title, w.saturating_sub(10).max(4))
            ),
            mark,
            &a.task,
        );
    }
}

/// M7: spawned-subagent panel — one medium card per agent with live status,
/// shown while the spawns work in parallel. Same card style as team mode.
fn draw_agents(f: &mut Frame, app: &App, area: Rect) {
    let running = app.agents.iter().filter(|a| a.state == TeamStatus::Working).count();
    let title = if running == 0 {
        format!(" agents · {} finished ", app.agents.len())
    } else {
        format!(" agents · {running} running / {} ", app.agents.len())
    };
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(FB_BORDER))
        .title(title);
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    if inner.height < 4 || inner.width < 12 {
        return;
    }
    // shortcut: the strip fits 4 cards; more agents need paging — upgrade if a run exceeds that
    let n = app.agents.len().min(4);
    let cards = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(vec![Constraint::Ratio(1, n as u32); n])
        .split(inner);
    for (i, card_area) in cards.iter().enumerate() {
        let a = &app.agents[i];
        let accent = TEAM_ACCENTS[i % TEAM_ACCENTS.len()];
        let (mark, border) = match a.state {
            TeamStatus::Working => ("● working", accent),
            TeamStatus::Done => ("✓ done", FB_SUCCESS),
            TeamStatus::Failed => ("✗ failed", FB_ERROR),
        };
        let w = card_area.width.saturating_sub(2) as usize;
        let secs = a.started.elapsed().as_secs();
        draw_agent_card(
            f,
            *card_area,
            accent,
            border,
            &truncate_str(&a.name, w.max(4)),
            &format!(
                "{mark} {secs}s · {}",
                truncate_str(&a.detail, w.saturating_sub(14).max(3))
            ),
            &a.task,
        );
    }
}

/// One medium card, shared by the team and agents panels: accent title,
/// status line, wrapped muted body. The status color does the border too.
fn draw_agent_card(
    f: &mut Frame,
    area: Rect,
    accent: Color,
    border: Color,
    title: &str,
    mark: &str,
    body: &str,
) {
    let w = area.width.saturating_sub(2) as usize;
    let body_rows = area.height.saturating_sub(4).max(1) as usize;
    let mut lines = vec![
        Line::from(title.to_string())
            .style(Style::default().fg(accent).add_modifier(Modifier::BOLD)),
        Line::from(mark.to_string()).style(Style::default().fg(border)),
    ];
    for t in wrap_n(body, w.max(1), body_rows) {
        lines.push(Line::from(t).style(Style::default().fg(FB_MUTED)));
    }
    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(border)),
        ),
        area,
    );
}

/// Cut `s` to at most `max` terminal columns and add `…` when something was
/// cut (B-35: display width, so wide characters count double).
fn truncate_str(s: &str, max: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if s.width() <= max {
        return s.to_string();
    }
    let mut cut = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        cut.push(c);
        used += w;
    }
    format!("{cut}…")
}

fn centered_rect(pct_x: u16, pct_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - pct_y) / 2),
            Constraint::Percentage(pct_y),
            Constraint::Percentage((100 - pct_y) / 2),
        ])
        .split(r);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - pct_x) / 2),
            Constraint::Percentage(pct_x),
            Constraint::Percentage((100 - pct_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

/// B-15: the approval overlay — the WHOLE tool input, scrollable, with the
/// keys in the bottom border so they stay visible.
fn draw_approval(f: &mut Frame, app: &App) {
    let Some(ap) = &app.approval else { return };
    let area = centered_rect(80, 70, f.area());
    f.render_widget(Clear, area);
    let mut text = vec![
        Line::from("Tool approval required")
            .style(Style::default().fg(FB_ERROR).add_modifier(Modifier::BOLD)),
        Line::from(format!("Tool:  {}", ap.tool)).style(Style::default().fg(FB_FG)),
        Line::from("Input:").style(Style::default().fg(FB_FG)),
    ];
    for l in approval_detail_lines(&ap.detail) {
        text.push(Line::from(l).style(Style::default().fg(FB_MUTED)));
    }
    let title = match app.approval_queue.len() {
        0 => " approval ".to_string(),
        n => format!(" approval · {n} more waiting "),
    };
    let keys = " [y] allow once  [a] always (this workspace)  [n/Esc] deny  [↑↓] scroll ";
    let scroll = app.approval_scroll.min(text.len().min(u16::MAX as usize) as u16);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(FB_BORDER))
        .title(title)
        .title_bottom(Line::from(keys).style(Style::default().fg(FB_GREEN)));
    let widget = Paragraph::new(text).block(block).wrap(Wrap { trim: false }).scroll((scroll, 0));
    f.render_widget(widget, area);
}

/// B-15: one row per line of the detail; a tab becomes four spaces and any
/// other control character `\u{..}`, so nothing is hidden or merged.
pub fn approval_detail_lines(detail: &str) -> Vec<String> {
    detail
        .split('\n')
        .map(|line| {
            line.chars()
                .map(|c| match c {
                    '\t' => "    ".to_string(),
                    c if c.is_control() => format!("\\u{{{:x}}}", c as u32),
                    c => c.to_string(),
                })
                .collect()
        })
        .collect()
}
