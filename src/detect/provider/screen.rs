pub(super) struct VisibleScreen<'a> {
    lines: Vec<&'a str>,
}

impl<'a> VisibleScreen<'a> {
    pub(super) fn new(content: &'a str) -> Self {
        Self {
            lines: content.lines().collect(),
        }
    }

    pub(super) fn all(&self) -> Lines<'_> {
        Lines { lines: &self.lines }
    }

    pub(super) fn following_last<F>(&self, predicate: F) -> Lines<'_>
    where
        F: Fn(&str) -> bool,
    {
        self.window_from_last(predicate, false)
    }

    pub(super) fn at_last<F>(&self, predicate: F) -> Lines<'_>
    where
        F: Fn(&str) -> bool,
    {
        self.window_from_last(predicate, true)
    }

    fn window_from_last<F>(&self, predicate: F, include_match: bool) -> Lines<'_>
    where
        F: Fn(&str) -> bool,
    {
        let matched = self.lines.iter().rposition(|line| predicate(line));
        let start = match (matched, include_match) {
            (Some(index), true) => index,
            (Some(index), false) => index + 1,
            (None, _) => 0,
        };
        Lines {
            lines: &self.lines[start..],
        }
    }

    pub(super) fn after_last_divider(&self) -> Lines<'_> {
        self.following_last(is_divider)
    }

    pub(super) fn recent_non_empty(&self, count: usize) -> Lines<'_> {
        if count == 0 {
            return Lines {
                lines: &self.lines[self.lines.len()..],
            };
        }
        let mut remaining = count;
        let mut start = self.lines.len();
        for index in (0..self.lines.len()).rev() {
            if self.lines[index].trim().is_empty() {
                continue;
            }
            start = index;
            remaining = remaining.saturating_sub(1);
            if remaining == 0 {
                break;
            }
        }
        Lines {
            lines: &self.lines[start..],
        }
    }

    pub(super) fn latest_prompt_box(&self) -> Option<Lines<'_>> {
        let mut lower_border = None;
        for index in (0..self.lines.len()).rev() {
            if !is_divider(self.lines[index]) {
                continue;
            }
            if let Some(bottom) = lower_border {
                return Some(Lines {
                    lines: &self.lines[index + 1..bottom],
                });
            }
            lower_border = Some(index);
        }
        None
    }
}

#[derive(Clone, Copy)]
pub(super) struct Lines<'a> {
    lines: &'a [&'a str],
}

impl Lines<'_> {
    pub(super) fn contains(&self, needle: &str) -> bool {
        let needle = needle.to_lowercase();
        self.lines
            .iter()
            .any(|line| line.to_lowercase().contains(&needle))
    }

    pub(super) fn contains_all(&self, needles: &[&str]) -> bool {
        needles.iter().all(|needle| self.contains(needle))
    }

    pub(super) fn contains_any(&self, needles: &[&str]) -> bool {
        needles.iter().any(|needle| self.contains(needle))
    }

    pub(super) fn any_line<F>(&self, predicate: F) -> bool
    where
        F: Fn(&str) -> bool,
    {
        self.lines.iter().any(|line| predicate(line))
    }
}

pub(super) fn codex_prompt(line: &str) -> bool {
    let line = line.trim_start();
    line == "›" || line.starts_with("› ")
}

pub(super) fn opencode_prompt(line: &str) -> bool {
    line.to_lowercase().contains("ask anything")
}

pub(super) fn is_divider(line: &str) -> bool {
    let trimmed = line.trim();
    let mut chars = trimmed.chars();
    let mut count = 0;
    while chars.next().is_some_and(|character| character == '─') {
        count += 1;
    }
    count >= 3
}

pub(super) fn title_has_braille_activity(title: &str) -> bool {
    title.split_whitespace().any(|word| {
        let mut characters = word.chars();
        characters
            .next()
            .is_some_and(|character| ('\u{2801}'..='\u{28ff}').contains(&character))
            && characters.next().is_none()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_windows_follow_the_latest_provider_prompt() {
        let screen = VisibleScreen::new("old approval\n› continue\nnew output");
        let current = screen.following_last(codex_prompt);
        assert!(!current.contains("approval"));
        assert!(current.contains("new output"));
    }

    #[test]
    fn recent_non_empty_ignores_blank_padding() {
        let screen = VisibleScreen::new("old\n\nworking\n\nready\n\n");
        let bottom = screen.recent_non_empty(2);
        assert!(!bottom.contains("old"));
        assert!(bottom.contains_all(&["working", "ready"]));
    }

    #[test]
    fn zero_recent_lines_returns_an_empty_window() {
        let screen = VisibleScreen::new("working\nready");
        let bottom = screen.recent_non_empty(0);
        assert!(!bottom.contains("working"));
        assert!(!bottom.contains("ready"));
    }

    #[test]
    fn prompt_box_returns_the_latest_bordered_body() {
        let screen = VisibleScreen::new("history\n────\n❯ message\n────\nfooter");
        let body = screen.latest_prompt_box().unwrap();
        assert!(body.contains("❯ message"));
        assert!(!body.contains("footer"));
    }

    #[test]
    fn braille_activity_requires_a_standalone_title_word() {
        assert!(title_has_braille_activity("⠸ project"));
        assert!(!title_has_braille_activity("project⠸"));
        assert!(!title_has_braille_activity("project"));
    }
}
