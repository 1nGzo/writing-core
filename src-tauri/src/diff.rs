use similar::{ChangeTag, TextDiff};

use crate::writing::{writing_delta, WritingDelta};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSummary {
    pub inserted_chars: i64,
    pub deleted_chars: i64,
    pub change_count: i64,
    pub writing: WritingDelta,
}

pub fn summarize(old: &str, new: &str) -> DiffSummary {
    let diff = TextDiff::from_chars(old, new);
    let mut summary = DiffSummary {
        inserted_chars: 0,
        deleted_chars: 0,
        change_count: 0,
        writing: writing_delta(old, new),
    };

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {}
            ChangeTag::Insert => {
                summary.inserted_chars += change.value().chars().count() as i64;
                summary.change_count += 1;
            }
            ChangeTag::Delete => {
                summary.deleted_chars += change.value().chars().count() as i64;
                summary.change_count += 1;
            }
        }
    }

    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_unicode_scalars_for_insert_delete_and_replace() {
        assert_eq!(
            summarize("欧林索斯走进花园。", "欧林索斯牵着林祺走进夜花庭。"),
            DiffSummary {
                inserted_chars: 6,
                deleted_chars: 1,
                change_count: 7,
                writing: writing_delta("欧林索斯走进花园。", "欧林索斯牵着林祺走进夜花庭。"),
            }
        );
        assert_eq!(
            summarize("abc", "ab🙂"),
            DiffSummary {
                inserted_chars: 1,
                deleted_chars: 1,
                change_count: 2,
                writing: writing_delta("abc", "ab🙂"),
            }
        );
    }

    #[test]
    fn same_text_has_no_event_payload() {
        assert_eq!(
            summarize("相同正文", "相同正文"),
            DiffSummary {
                inserted_chars: 0,
                deleted_chars: 0,
                change_count: 0,
                writing: writing_delta("相同正文", "相同正文"),
            }
        );
    }
}
