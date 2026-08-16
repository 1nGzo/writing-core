use pulldown_cmark::{Event, LinkType, Options, Parser, Tag, TagEnd};
use serde::Serialize;
use similar::{ChangeTag, TextDiff};
use unicode_script::{Script, UnicodeScript};

pub const WRITING_COUNT_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WritingCount {
    pub total: i64,
    pub han: i64,
    pub english_words: i64,
    pub numbers: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WritingDelta {
    pub inserted_units: i64,
    pub deleted_units: i64,
    pub net_units: i64,
    pub count_version: i64,
}

#[derive(Debug)]
struct LinkFrame {
    output_start: usize,
    destination: String,
    link_type: LinkType,
}

fn markdown_to_prose(markdown: &str) -> String {
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES
        | Options::ENABLE_GFM
        | Options::ENABLE_YAML_STYLE_METADATA_BLOCKS
        | Options::ENABLE_PLUSES_DELIMITED_METADATA_BLOCKS;
    let parser = Parser::new_ext(markdown, options);
    let mut prose = String::new();
    let mut excluded_depth = 0usize;
    let mut links = Vec::new();

    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(_)) | Event::Start(Tag::MetadataBlock(_)) => {
                excluded_depth += 1;
            }
            Event::End(TagEnd::CodeBlock) | Event::End(TagEnd::MetadataBlock(_)) => {
                excluded_depth = excluded_depth.saturating_sub(1);
            }
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                ..
            }) if excluded_depth == 0 => {
                links.push(LinkFrame {
                    output_start: prose.len(),
                    destination: dest_url.into_string(),
                    link_type,
                });
            }
            Event::End(TagEnd::Link) if excluded_depth == 0 => {
                if let Some(link) = links.pop() {
                    let label = &prose[link.output_start..];
                    let is_autolink =
                        matches!(link.link_type, LinkType::Autolink | LinkType::Email)
                            && label == link.destination;
                    if is_autolink {
                        prose.truncate(link.output_start);
                    }
                }
            }
            Event::Text(text) if excluded_depth == 0 => prose.push_str(&text),
            Event::SoftBreak | Event::HardBreak if excluded_depth == 0 => prose.push('\n'),
            // Inline code, fenced code, HTML, metadata, URL destinations and
            // Markdown punctuation are intentionally not prose.
            Event::Start(_)
            | Event::End(_)
            | Event::Code(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::Html(_)
            | Event::InlineHtml(_)
            | Event::FootnoteReference(_)
            | Event::Rule
            | Event::TaskListMarker(_) => {}
            _ => {}
        }
    }

    prose
}

fn is_han(character: char) -> bool {
    character.script() == Script::Han
}

fn is_english_letter(character: char) -> bool {
    character.is_alphabetic() && character.script() == Script::Latin
}

fn is_number(character: char) -> bool {
    character.to_digit(10).is_some()
}

fn tokenize_prose(prose: &str) -> (Vec<String>, WritingCount) {
    let characters: Vec<char> = prose.chars().collect();
    let mut tokens = Vec::new();
    let mut count = WritingCount::default();
    let mut index = 0usize;

    while index < characters.len() {
        let character = characters[index];
        if is_han(character) {
            tokens.push(character.to_string());
            count.han += 1;
            count.total += 1;
            index += 1;
            continue;
        }

        if is_english_letter(character) {
            tokens.push("\u{0}english".to_string());
            count.english_words += 1;
            count.total += 1;
            index += 1;
            while index < characters.len() {
                if is_english_letter(characters[index]) {
                    index += 1;
                    continue;
                }
                if matches!(characters[index], '\'' | '\u{2019}')
                    && index > 0
                    && index + 1 < characters.len()
                    && is_english_letter(characters[index - 1])
                    && is_english_letter(characters[index + 1])
                {
                    index += 1;
                    continue;
                }
                break;
            }
            continue;
        }

        if is_number(character) {
            tokens.push("\u{0}number".to_string());
            count.numbers += 1;
            count.total += 1;
            index += 1;
            while index < characters.len() && is_number(characters[index]) {
                index += 1;
            }
            continue;
        }

        index += 1;
    }

    (tokens, count)
}

pub fn count_writing_units(markdown: &str) -> WritingCount {
    let prose = markdown_to_prose(markdown);
    tokenize_prose(&prose).1
}

pub fn writing_delta(old_markdown: &str, new_markdown: &str) -> WritingDelta {
    let old_prose = markdown_to_prose(old_markdown);
    let new_prose = markdown_to_prose(new_markdown);
    let old_tokens = tokenize_prose(&old_prose).0;
    let new_tokens = tokenize_prose(&new_prose).0;
    let old_token_refs: Vec<&str> = old_tokens.iter().map(String::as_str).collect();
    let new_token_refs: Vec<&str> = new_tokens.iter().map(String::as_str).collect();
    let diff = TextDiff::from_slices(&old_token_refs, &new_token_refs);
    let mut inserted_units = 0i64;
    let mut deleted_units = 0i64;

    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Equal => {}
            ChangeTag::Insert => inserted_units += 1,
            ChangeTag::Delete => deleted_units += 1,
        }
    }

    WritingDelta {
        inserted_units,
        deleted_units,
        net_units: inserted_units - deleted_units,
        count_version: WRITING_COUNT_VERSION,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_the_requested_mvp_examples() {
        assert_eq!(
            count_writing_units("欧林索斯"),
            WritingCount {
                total: 4,
                han: 4,
                english_words: 0,
                numbers: 0,
            }
        );
        assert_eq!(count_writing_units("“欧林索斯……真的？”").total, 6);
        assert_eq!(count_writing_units("Writing Core").total, 2);
        assert_eq!(count_writing_units("第59章").total, 3);
        assert_eq!(count_writing_units("Tiru飞到了第59章。").total, 7);
    }

    #[test]
    fn markdown_syntax_and_technical_nodes_are_excluded_but_prose_remains() {
        let markdown = "# 第五十九章 星落之夜\n\n**欧林索斯**牵着林祺走进夜花庭。\n\n“真的？”";
        assert_eq!(count_writing_units(markdown).total, 24);
        assert_eq!(count_writing_units("[林祺](https://example.com)").total, 2);
        assert_eq!(count_writing_units("![星灯归庭](image.png)").total, 4);
        assert_eq!(
            count_writing_units("`Writing Core`\n\n```text\ndebug value\n```").total,
            0
        );
        assert_eq!(
            count_writing_units("---\ntitle: internal\n---\n\n正文").total,
            2
        );
    }

    #[test]
    fn apostrophes_keep_common_english_words_together() {
        assert_eq!(
            count_writing_units("don't Tiru's Olynthas Caelorum").english_words,
            4
        );
    }

    #[test]
    fn writing_delta_uses_token_units_and_ignores_markup_and_punctuation() {
        assert_eq!(
            writing_delta("Tir", "Tiru"),
            WritingDelta {
                inserted_units: 0,
                deleted_units: 0,
                net_units: 0,
                count_version: WRITING_COUNT_VERSION,
            }
        );
        assert_eq!(
            writing_delta("5", "59"),
            WritingDelta {
                inserted_units: 0,
                deleted_units: 0,
                net_units: 0,
                count_version: WRITING_COUNT_VERSION,
            }
        );
        assert_eq!(
            writing_delta("林", "林祺"),
            WritingDelta {
                inserted_units: 1,
                deleted_units: 0,
                net_units: 1,
                count_version: WRITING_COUNT_VERSION,
            }
        );
        assert_eq!(
            writing_delta("Writing", "Writing Core"),
            WritingDelta {
                inserted_units: 1,
                deleted_units: 0,
                net_units: 1,
                count_version: WRITING_COUNT_VERSION,
            }
        );
        assert_eq!(
            writing_delta("林祺。", "林祺！"),
            WritingDelta {
                inserted_units: 0,
                deleted_units: 0,
                net_units: 0,
                count_version: WRITING_COUNT_VERSION,
            }
        );
        assert_eq!(writing_delta("林祺", "**林祺**").net_units, 0);
    }

    #[test]
    fn han_script_covers_extensions_and_compatibility_ideographs() {
        assert!(is_han('欧'));
        assert!(is_han('\u{20000}'));
        assert!(is_han('\u{f900}'));
    }

    #[test]
    fn counts_a_100k_markdown_smoke_input() {
        let markdown = format!("# 星落\n\n{}", "欧".repeat(100_000));
        let started = std::time::Instant::now();
        let count = count_writing_units(&markdown);
        eprintln!(
            "writing_count_100k_scalars_ms={}",
            started.elapsed().as_millis()
        );
        assert_eq!(count.total, 100_002);
    }
}
