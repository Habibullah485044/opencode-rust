//! The work spec: before a run starts, one plain model call turns the
//! user's request into what they want (goal), what they get (output) and
//! how to prove it is done (done-when checks). The worker sees it next to
//! the request, and the final check proves each item with tools before the
//! run is handed back.
//!
//! Seen live: "create an image using binary 0 and 1" became a `.txt` grid,
//! and the mission's last step reported "Checked the completed result"
//! without opening anything. A spec says "Output: a PNG image"; the final
//! check makes the model show it is one.

/// Text the app sends as a user message during a final check.
pub const FINAL_CHECK_PREFIX: &str = "[final check]";

/// Text the app sends as a user message for the design review.
pub const DESIGN_REVIEW_PREFIX: &str = "[design review]";

/// What makes a page look AI-made. Seen live: five runs on four models
/// all opened with "A little … *happiness*." on cream with a serif headline
/// and a round badge. Named, so the model can see it in its own page.
pub const AI_LOOK: &str = "- a hero headline with one word in coloured italics (\"A little crumb of *happiness*.\")\n\
- a small uppercase label above the headline (\"YOUR WORKSPACE · 00 BOARDS\")\n\
- round or rotated \"stamp\" badges (\"Baked with love\", \"Fresh today\")\n\
- cream or beige background with a serif display font as the default look\n\
- copy like \"Made with love\", \"Crafted with care\", \"Welcome to\", \"Make room for…\"\n\
- three icons in a row with a line of text each; gradient blobs; emoji as icons\n\
- a marketing hero on an app: an app opens on its working screen (the board, the inbox)";

/// The design review request: the screenshots (as `[image: path]`), the
/// brief, and the look to avoid. One round: judge, fix, look again.
pub fn design_review_request(goal: &str, design: Option<&str>, shots: &[std::path::PathBuf]) -> String {
    let pictures: Vec<String> = shots.iter().map(|p| format!("[image: {}]", p.display())).collect();
    format!(
        "{DESIGN_REVIEW_PREFIX} Here are screenshots of what you built (desktop 1440px, phone 390px).\n\
         Goal: {goal}\nDesign brief: {}\n\n\
         Look at them as the client's art director. Does it look made for this business and its \
         people, or like a template? These mark a page as AI-made; count each one you see:\n{AI_LOOK}\n\n\
         Then fix the biggest problems in the code (layout, type, colour, imagery, copy). Keep what \
         already works. After the changes, run check_page with look: true and judge again. Reply \
         with: what you changed, and one line per item above: `SEEN` or `CLEAR`.\n{}",
        design.unwrap_or("none given: choose a direction that fits this business and say it"),
        pictures.join("\n")
    )
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Spec {
    pub goal: String,
    pub output: String,
    pub done_when: Vec<String>,
    pub assuming: Vec<String>,
    /// For something people look at (a site, an app screen): who it is for,
    /// the mood, and a visual direction that fits this business.
    pub design: Option<String>,
    /// The skills that fit this task (names from the skill menu), read and
    /// followed by the worker.
    pub skills: Vec<String>,
}

/// Worth a spec call? Short chatter, slash commands and the app's own
/// messages (`[mission …]`, `[turbo …]`, `[final check]`) are not.
pub fn needs_spec(request: &str) -> bool {
    let t = request.trim();
    if t.starts_with('/') || t.starts_with('[') {
        return false;
    }
    t.split_whitespace().count() >= 4
}

pub const SYSTEM: &str = "You turn a request to a coding agent into a short work spec. You never \
do the work and never ask questions: where the request is unclear, choose what the user most \
likely means and say so under ASSUMING.";

/// The request for the spec call. `context` is what the chat said last (a
/// follow-up like "why is it a txt?" needs it); `files` the project's
/// top-level names (an empty folder means: build from scratch).
pub fn prompt(request: &str, context: Option<&str>, files: &[String], skill_menu: &str) -> String {
    let mut p = String::new();
    if let Some(c) = context.filter(|c| !c.trim().is_empty()) {
        let c: String = c.chars().take(1500).collect();
        p.push_str(&format!("The agent's last reply in this chat:\n{c}\n\n"));
    }
    p.push_str(&format!(
        "Project folder: {}\n\nThe user's request:\n{request}\n\n",
        if files.is_empty() { "empty".to_string() } else { files.join(", ") }
    ));
    if !skill_menu.trim().is_empty() {
        p.push_str(&format!(
            "The agent's skills (a SKILL.md each, with the method for that kind of work):\n{skill_menu}\n\n\
             After DONE WHEN and ASSUMING, add one line: SKILLS: <1 to 3 skill names from this list \
             that fit the task, best first, comma separated, or none>\n\n"
        ));
    }
    p.push_str(
        "Reply in exactly this form:\n\
         KIND: task | question | chat\n\
         GOAL: <one sentence: what the user wants, in their words>\n\
         OUTPUT: <what they get: which files, in which format, where>\n\
         DESIGN: <only for something people look at: who it is for, the mood, and a visual \
         direction that fits this business (palette, type, layout), not a default look>\n\
         DONE WHEN:\n\
         - <a check a tool call can prove: the file exists and opens as a PNG, a command prints X, \
         check_page shows Y at phone and desktop width>\n\
         ASSUMING:\n\
         - <a choice you made where the request is unclear>\n\n\
         Rules:\n\
         - KIND question = they want an answer or an explanation, chat = small talk; then stop \
         after the KIND line.\n\
         - Read the request by its intent: an \"image\" is an image file (PNG or SVG) unless they \
         ask for text; an \"app\" or \"website\" must work, not only look right; a fix must fix the \
         cause the user saw.\n\
         - A website or page: it must look like a professional designer made it for this \
         business (a clear visual direction, real content, not a generic template), work on \
         phones and desktops, and pass check_page.\n\
         - Use the names from the request exactly. Add no features they did not ask for.\n\
         - 2 to 5 DONE WHEN items, 0 to 3 ASSUMING items, each one line.",
    );
    p
}

/// The spec in a reply, or `None` for a question, chat, or a reply that is
/// not in the form (a weak model's chatter must not become a spec).
pub fn parse(reply: &str) -> Option<Spec> {
    let mut spec = Spec::default();
    let mut kind = String::new();
    let mut list: Option<&str> = None;
    for line in reply.lines() {
        let l = line.trim().trim_start_matches(['*', '#', '`']).trim();
        let upper = l.to_ascii_uppercase();
        let value = |key: &str| l[key.len()..].trim_start_matches([':', ' ', '*']).trim().to_string();
        if upper.starts_with("KIND") {
            kind = value("KIND").to_ascii_lowercase();
            list = None;
        } else if upper.starts_with("GOAL") {
            spec.goal = value("GOAL");
            list = None;
        } else if upper.starts_with("OUTPUT") {
            spec.output = value("OUTPUT");
            list = None;
        } else if upper.starts_with("SKILLS") {
            spec.skills = value("SKILLS")
                .split(',')
                .map(|n| n.trim().trim_matches(['`', '*', '"', '.']).to_string())
                .filter(|n| !n.is_empty() && !n.eq_ignore_ascii_case("none") && !n.starts_with('<'))
                .take(3)
                .collect();
            list = None;
        } else if upper.starts_with("DESIGN") {
            spec.design = Some(value("DESIGN")).filter(|d| !d.is_empty() && !d.starts_with('<'));
            list = None;
        } else if upper.starts_with("DONE WHEN") {
            list = Some("done");
        } else if upper.starts_with("ASSUMING") {
            list = Some("assume");
        } else if let Some(item) = l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")).map(str::trim) {
            match list {
                Some("done") if !item.is_empty() => spec.done_when.push(item.to_string()),
                Some("assume") if !item.is_empty() && !item.starts_with('<') => spec.assuming.push(item.to_string()),
                _ => {}
            }
        }
    }
    spec.done_when.truncate(6);
    spec.assuming.truncate(3);
    let task = kind.starts_with("task") || (kind.is_empty() && !spec.done_when.is_empty());
    (task && !spec.goal.is_empty() && !spec.done_when.is_empty()).then_some(spec)
}

/// What the user sees before the work starts.
pub fn for_user(s: &Spec) -> String {
    format!("**Spec** — {}\n{}", s.goal, details(s))
}

/// The goal written with its spec, as a mission keeps it: every step brief
/// and the coordinator see the checks, and a resumed mission still has them.
pub fn with_goal(request: &str, s: &Spec) -> String {
    format!("{}\n\n{}", request.trim(), details(s))
}

/// The spec back from [`with_goal`] text (`None` when it has none).
pub fn from_goal(text: &str) -> Option<Spec> {
    let (goal, rest) = text.split_once("\nOutput:")?;
    parse(&format!("KIND: task\nGOAL: {}\nOutput:{rest}", goal.split_whitespace().collect::<Vec<_>>().join(" ")))
}

fn details(s: &Spec) -> String {
    let mut out = format!("Output: {}\n", s.output);
    if let Some(d) = &s.design {
        out.push_str(&format!("Design: {d}\n"));
    }
    out.push_str("Done when:");
    for d in &s.done_when {
        out.push_str(&format!("\n- {d}"));
    }
    if !s.assuming.is_empty() {
        out.push_str("\nAssuming:");
        for a in &s.assuming {
            out.push_str(&format!("\n- {a}"));
        }
    }
    if !s.skills.is_empty() {
        out.push_str(&format!("\nSkills: {}", s.skills.join(", ")));
    }
    out
}

/// What the worker gets next to the request.
pub fn for_model(s: &Spec) -> String {
    format!(
        "Work spec for this request (written before work started):\n{}\n\n{}Build to this spec. \
         Before you finish, prove every done-when item with a tool call (open or run what you \
         made); the app checks them again after you.",
        for_user(s).replacen("**Spec** — ", "Goal: ", 1),
        if s.skills.is_empty() {
            String::new()
        } else {
            let files: Vec<String> = s.skills.iter().map(|n| format!("`.opencode/skills/{n}/SKILL.md`")).collect();
            format!("First read these skills and follow their method: {}.\n", files.join(", "))
        }
    )
}

/// The first final-check message.
pub fn check_request(s: &Spec) -> String {
    let items: Vec<String> = s.done_when.iter().enumerate().map(|(i, d)| format!("{}. {d}", i + 1)).collect();
    format!(
        "{FINAL_CHECK_PREFIX} Before this is handed back, prove each item below NOW with tool calls \
         (read or run what you made, check_page a page, list the output folder). Do not answer \
         from memory.\nGoal: {}\nOutput: {}\n{}\n\nThen reply with one line per item: \
         `PASS: <item> — <the evidence you saw>` or `FAIL: <item> — <what is wrong>`. If an item \
         fails, fix it and check it again before you reply.",
        s.goal,
        s.output,
        items.join("\n")
    )
}

/// The follow-up when the model checked but wrote no PASS/FAIL lines.
pub fn lines_request(s: &Spec) -> String {
    let items: Vec<String> = s.done_when.iter().enumerate().map(|(i, d)| format!("{}. {d}", i + 1)).collect();
    format!(
        "{FINAL_CHECK_PREFIX} Your reply had no PASS/FAIL lines. From what you just checked, reply \
         with one line per item: `PASS: <item> — <the evidence you saw>` or `FAIL: <item> — <what is \
         wrong>`. Call a tool again only for an item you did not check.\n{}",
        items.join("\n")
    )
}

/// The follow-up when items still fail, or were "passed" without a look.
pub fn retry_request(problem: &str) -> String {
    format!(
        "{FINAL_CHECK_PREFIX} Not done yet: {problem}\nFix it, prove every item again with tool \
         calls, and reply with the PASS/FAIL lines."
    )
}

/// Counts of PASS and FAIL lines, and the failing items.
pub fn verdicts(reply: &str) -> (usize, Vec<String>) {
    let mut pass = 0;
    let mut fails = Vec::new();
    for line in reply.lines() {
        let l = line.trim().trim_start_matches(['*', '-', '`', ' ']).trim_start_matches(|c: char| c.is_ascii_digit() || c == '.').trim();
        let upper = l.to_ascii_uppercase();
        if upper.starts_with("PASS") {
            pass += 1;
        } else if upper.starts_with("FAIL") {
            fails.push(l[4..].trim_start_matches([':', ' ', '*']).trim().to_string());
        }
    }
    (pass, fails)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_spec_reply_is_read() {
        let reply = "KIND: task\nGOAL: Draw a heart from 0s and 1s\n**OUTPUT:** `binary-image/heart.png`, a PNG\n\
                     DONE WHEN:\n- heart.png exists and is a valid PNG\n- it shows a heart\nASSUMING:\n- 16x16 pixels\n";
        let s = parse(reply).unwrap();
        assert_eq!(s.goal, "Draw a heart from 0s and 1s");
        assert!(s.output.contains("PNG"), "{}", s.output);
        assert_eq!(s.done_when.len(), 2);
        assert_eq!(s.assuming, ["16x16 pixels"]);
        assert!(for_user(&s).contains("- it shows a heart"));
        assert!(check_request(&s).starts_with(FINAL_CHECK_PREFIX));
    }

    #[test]
    fn a_design_brief_is_read_and_handed_on() {
        let reply = "KIND: task\nGOAL: A site for a bakery\nOUTPUT: index.html\n\
                     DESIGN: families nearby; warm, playful; tomato red and flour white, chunky sans\n\
                     DONE WHEN:\n- check_page passes";
        let s = parse(reply).unwrap();
        assert_eq!(s.design.as_deref(), Some("families nearby; warm, playful; tomato red and flour white, chunky sans"));
        assert!(for_model(&s).contains("Design: families nearby"));
        assert_eq!(parse("KIND: task\nGOAL: x\nOUTPUT: y\nDESIGN: <only for…>\nDONE WHEN:\n- z").unwrap().design, None);
        let review = design_review_request("A site", s.design.as_deref(), &["d.png".into()]);
        assert!(review.starts_with(DESIGN_REVIEW_PREFIX) && review.contains("[image: d.png]") && review.contains("coloured italics"));
    }

    #[test]
    fn a_mission_goal_keeps_its_spec() {
        let s = Spec {
            goal: "Draw a heart".into(),
            output: "heart.png".into(),
            done_when: vec!["heart.png is a PNG".into()],
            assuming: vec![],
            design: None,
            skills: vec![],
        };
        let stored = with_goal("make a heart\nfrom 0 and 1", &s);
        let back = from_goal(&stored).unwrap();
        assert_eq!(back.goal, "make a heart from 0 and 1");
        assert_eq!((back.output.as_str(), back.done_when.as_slice()), ("heart.png", s.done_when.as_slice()));
        assert_eq!(from_goal("just a goal"), None);
    }

    #[test]
    fn questions_chat_and_chatter_get_no_spec() {
        assert_eq!(parse("KIND: question"), None);
        assert_eq!(parse("KIND: chat\nGOAL: say hi"), None);
        assert_eq!(parse("Sure! I will build it now."), None);
        assert_eq!(parse("KIND: task\nGOAL: x\nDONE WHEN:\n"), None, "no checks, no spec");
    }

    #[test]
    fn only_real_requests_are_specced() {
        assert!(needs_spec("create a folder and start creating an image using binary 0 and 1"));
        assert!(!needs_spec("hi there"));
        assert!(!needs_spec("/mission build a site"));
        assert!(!needs_spec("[turbo] keep going on the todos"));
        assert!(!needs_spec("[final check] prove it"));
    }

    #[test]
    fn verdict_lines_are_counted() {
        let reply = "Checked.\n1. PASS: file exists — read it\n- **FAIL**: shows a heart — it is a text grid\nPASS: menu works";
        let (pass, fails) = verdicts(reply);
        assert_eq!(pass, 2);
        assert_eq!(fails, ["shows a heart — it is a text grid"]);
        assert_eq!(verdicts("All good, everything works."), (0, vec![]));
    }

    #[test]
    fn the_prompt_carries_the_context() {
        let p = prompt("why is it a txt?", Some("Created image.txt"), &["binary-image".into()], "- frontend-design: Distinctive interfaces");
        assert!(p.contains("Created image.txt") && p.contains("binary-image") && p.contains("why is it a txt?"));
        assert!(prompt("x", None, &[], "").contains("Project folder: empty"));
        assert!(p.contains("SKILLS:") && p.contains("frontend-design: Distinctive"), "the skill menu to pick from");
        assert!(!prompt("x", None, &[], "").contains("SKILLS:"), "no skills, no question about them");
    }

    #[test]
    fn the_skills_that_fit_are_picked_and_handed_on() {
        let reply = "KIND: task\nGOAL: A bakery site\nOUTPUT: index.html\nDONE WHEN:\n- check_page passes\n\
                     ASSUMING:\n- one page\nSKILLS: `frontend-design`, build-anything, test-driven-development, extra";
        let s = parse(reply).unwrap();
        assert_eq!(s.skills, ["frontend-design", "build-anything", "test-driven-development"], "three at most");
        let note = for_model(&s);
        assert!(note.contains("First read these skills") && note.contains(".opencode/skills/frontend-design/SKILL.md"), "{note}");
        assert!(for_user(&s).contains("Skills: frontend-design, build-anything"));
        assert!(parse("KIND: task\nGOAL: x\nOUTPUT: y\nDONE WHEN:\n- z\nSKILLS: none").unwrap().skills.is_empty());
        // A mission keeps the pick with its goal.
        let back = from_goal(&with_goal("make a site", &s)).unwrap();
        assert_eq!(back.skills, s.skills);
    }
}
