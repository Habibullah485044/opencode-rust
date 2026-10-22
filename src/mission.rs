//! Missions — assign the agent a mission instead of a bare prompt.
//!
//! The user names a mission kind (Explore / Commit / Merge PR / Custom, like
//! Freebuff's Missions dialog) and an effort level (Low → Max), and the raw
//! goal is expanded into a well-worded briefing: the agent understands the
//! mission, decides its approach, and executes with that framing instead of
//! asking again and again.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissionKind {
    Explore,
    Commit,
    MergePr,
    Custom,
}

impl MissionKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "explore" => Some(Self::Explore),
            "commit" => Some(Self::Commit),
            "merge-pr" | "mergepr" | "merge_pr" | "merge" => Some(Self::MergePr),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }

    /// Picker label (matches Freebuff's Missions dialog).
    pub fn label(&self) -> &'static str {
        match self {
            Self::Explore => "Explore",
            Self::Commit => "Commit",
            Self::MergePr => "Merge PR",
            Self::Custom => "Custom",
        }
    }

    pub fn all() -> [MissionKind; 4] {
        [Self::Explore, Self::Commit, Self::MergePr, Self::Custom]
    }

    /// What this mission means and how to approach it.
    fn playbook(&self) -> &'static str {
        match self {
            Self::Explore => {
                "You are on an EXPLORE mission: understand before you act. Trace how the \
                 relevant code works today — entry points, data flow, invariants — and check \
                 what the repo already has before proposing anything new. Deliverable: a short \
                 map of what you found, the decision you recommend, and the one next step worth \
                 taking. Do not modify files; this mission is about clarity, not change."
            }
            Self::Commit => {
                "You are on a COMMIT mission: get the working tree into a state that can be \
                 committed with pride. Review what changed, run the checks that matter, fix what \
                 the change broke (including callers OUTSIDE the diff), and keep everything the \
                 smallest that fully solves the goal. Deliverable: the commit — with a message \
                 that explains WHY — or the exact message and diff for review if committing is \
                 not appropriate right now."
            }
            Self::MergePr => {
                "You are on a MERGE PR mission: review the branch like the senior dev who gets \
                 paged when it breaks. Verify the change against its callers OUTSIDE the diff, \
                 run the tests covering both the touched code and its neighbors, and watch for \
                 silent behavior changes. Deliverable: a verdict — merge, merge-with-fixes, or \
                 send back — with the evidence, and any fixes applied."
            }
            Self::Custom => {
                "You are on a CUSTOM mission: the goal below is the spec and you own the \
                 approach. Decide what 'done' means, take the smallest complete path to it, and \
                 verify the result yourself before reporting. Deliverable: the goal achieved — \
                 or a crisp status of exactly what stands between here and done."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Default for Effort {
    fn default() -> Self {
        Self::High
    }
}

impl Effort {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Some(Self::Low),
            "medium" | "med" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" | "x-high" | "xl" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    /// Picker label (matches Freebuff's Mission effort row).
    pub fn label(&self) -> &'static str {
        match self {
            Self::Low => "Low",
            Self::Medium => "Medium",
            Self::High => "High",
            Self::Xhigh => "Xhigh",
            Self::Max => "Max",
        }
    }

    pub fn all() -> [Effort; 5] {
        [Self::Low, Self::Medium, Self::High, Self::Xhigh, Self::Max]
    }

    /// How much work the mission asks for.
    fn guidance(&self) -> &'static str {
        match self {
            Self::Low => {
                "EFFORT Low — answer fast: minimal tool use, one focused pass, no ceremony."
            }
            Self::Medium => {
                "EFFORT Medium — steady work: inspect what you need, verify the main path, \
                 skip the exotic cases."
            }
            Self::High => {
                "EFFORT High — thorough: verify claims with real checks, cover the likely edge \
                 cases, and confirm callers still hold."
            }
            Self::Xhigh => {
                "EFFORT Xhigh — deep: run the full test surface that applies, hunt edge cases \
                 and regressions, second-guess every assumption."
            }
            Self::Max => {
                "EFFORT Max — exhaustive: leave no stone unturned; sweep the area (subagents \
                 welcome), verify every claim with tests or commands, cover failure modes, and \
                 do not stop at the first plausible answer."
            }
        }
    }
}

/// A mission: kind + effort + the user's raw goal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mission {
    pub kind: MissionKind,
    pub effort: Effort,
    pub goal: String,
}

impl Mission {
    /// Expand the raw goal into the briefing prompt that runs as the message.
    pub fn briefing(&self) -> String {
        let goal = if self.goal.trim().is_empty() {
            "the task at hand"
        } else {
            self.goal.trim()
        };
        format!(
            "MISSION: {} · effort {}\nGOAL: {}\n\n{}\n\n{}",
            self.kind.label(),
            self.effort.label(),
            goal,
            self.kind.playbook(),
            self.effort.guidance(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_kinds_and_effort() {
        assert_eq!(MissionKind::parse("Explore"), Some(MissionKind::Explore));
        assert_eq!(MissionKind::parse("commit"), Some(MissionKind::Commit));
        assert_eq!(MissionKind::parse("merge-pr"), Some(MissionKind::MergePr));
        assert_eq!(MissionKind::parse("merge"), Some(MissionKind::MergePr));
        assert_eq!(MissionKind::parse("custom"), Some(MissionKind::Custom));
        assert_eq!(MissionKind::parse("deploy"), None);

        assert_eq!(Effort::parse("low"), Some(Effort::Low));
        assert_eq!(Effort::parse("MED"), Some(Effort::Medium));
        assert_eq!(Effort::parse("xhigh"), Some(Effort::Xhigh));
        assert_eq!(Effort::parse("max"), Some(Effort::Max));
        assert_eq!(Effort::parse("ultra"), None);
        assert_eq!(Effort::default(), Effort::High);
    }

    #[test]
    fn briefing_expands_the_goal_into_a_prompt() {
        let m = Mission {
            kind: MissionKind::Explore,
            effort: Effort::Xhigh,
            goal: "the auth flow".into(),
        };
        let b = m.briefing();
        assert!(b.contains("MISSION: Explore · effort Xhigh"), "{b}");
        assert!(b.contains("GOAL: the auth flow"), "{b}");
        assert!(b.contains("EXPLORE mission"), "playbook present: {b}");
        assert!(b.contains("EFFORT Xhigh"), "effort guidance present: {b}");
        // Every kind/effort combination produces a non-trivial briefing.
        for k in MissionKind::all() {
            for e in Effort::all() {
                let b = Mission { kind: k, effort: e, goal: "g".into() }.briefing();
                assert!(b.len() > 200, "{:?}/{:?} briefing too thin", k.label(), e.label());
            }
        }
    }

    #[test]
    fn empty_goal_falls_back_and_labels_are_dialog_exact() {
        let m = Mission { kind: MissionKind::Custom, effort: Effort::Max, goal: "  ".into() };
        assert!(m.briefing().contains("GOAL: the task at hand"));
        let labels: Vec<&str> = MissionKind::all().iter().map(|k| k.label()).collect();
        assert_eq!(labels, vec!["Explore", "Commit", "Merge PR", "Custom"]);
        let efforts: Vec<&str> = Effort::all().iter().map(|e| e.label()).collect();
        assert_eq!(efforts, vec!["Low", "Medium", "High", "Xhigh", "Max"]);
    }
}
