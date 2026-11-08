//! Unified diffs between two config texts.

use similar::{ChangeTag, TextDiff};

/// Counts of added and removed lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DiffStat {
    pub added: usize,
    pub removed: usize,
}

impl DiffStat {
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

/// A unified diff (like `diff -u`) with `context` lines around each change.
/// Returns an empty string when the texts are equal.
pub fn unified(old: &str, new: &str, old_label: &str, new_label: &str, context: usize) -> String {
    if old == new {
        return String::new();
    }
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(context)
        .header(old_label, new_label)
        .to_string()
}

pub fn stat(old: &str, new: &str) -> DiffStat {
    let mut s = DiffStat::default();
    for change in TextDiff::from_lines(old, new).iter_all_changes() {
        match change.tag() {
            ChangeTag::Insert => s.added += 1,
            ChangeTag::Delete => s.removed += 1,
            ChangeTag::Equal => {}
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn equal_texts_give_empty_diff() {
        assert_eq!(unified("a\n", "a\n", "x", "y", 3), "");
        assert!(stat("a\n", "a\n").is_empty());
    }

    #[test]
    fn shows_changed_line() {
        let old = "hostname r1\ninterface Gi0/0\n shutdown\nend\n";
        let new = "hostname r1\ninterface Gi0/0\n no shutdown\nend\n";
        let d = unified(old, new, "old", "new", 1);
        assert!(d.starts_with("--- old\n+++ new\n"), "{d}");
        assert!(d.contains("\n- shutdown\n"), "{d}");
        assert!(d.contains("\n+ no shutdown\n"), "{d}");
        assert_eq!(
            stat(old, new),
            DiffStat {
                added: 1,
                removed: 1
            }
        );
    }
}
