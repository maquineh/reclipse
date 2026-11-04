//! Literal and regex search used by the editor and project search.

use regex::{Regex, RegexBuilder};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchOptions {
    pub case_sensitive: bool,
    pub whole_word: bool,
    pub regex: bool,
}

/// A compiled search query, reusable across many files.
#[derive(Debug, Clone)]
pub struct SearchMatcher {
    needle: String,
    options: SearchOptions,
    regex: Option<Regex>,
}

impl SearchMatcher {
    pub fn new(needle: &str, options: SearchOptions) -> Result<Self, String> {
        if needle.is_empty() {
            return Ok(Self {
                needle: String::new(),
                options,
                regex: None,
            });
        }
        let regex = if options.regex {
            let pattern = if options.whole_word {
                format!(r"\b(?:{needle})\b")
            } else {
                needle.to_owned()
            };
            let compiled = RegexBuilder::new(&pattern)
                .case_insensitive(!options.case_sensitive)
                .build()
                .map_err(|error| error.to_string())?;
            Some(compiled)
        } else {
            None
        };
        Ok(Self {
            needle: needle.to_owned(),
            options,
            regex,
        })
    }

    pub fn find_all(&self, text: &str) -> Vec<(usize, usize)> {
        if self.needle.is_empty() {
            return Vec::new();
        }
        if let Some(regex) = &self.regex {
            return regex_matches(text, regex, self.options.whole_word);
        }
        literal_matches(text, &self.needle, self.options)
    }

    /// Expand a replacement template for each character range. Regex templates
    /// support capture groups (`$1`, `${name}`); literal mode returns the
    /// template unchanged.
    pub fn expand_replacements(
        &self,
        text: &str,
        ranges: &[(usize, usize)],
        template: &str,
    ) -> Vec<String> {
        let Some(regex) = &self.regex else {
            return vec![template.to_owned(); ranges.len()];
        };
        let mut offsets: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();
        offsets.push(text.len());
        ranges
            .iter()
            .map(|(start, end)| {
                let byte_start = offsets.get(*start).copied().unwrap_or(text.len());
                let byte_end = offsets.get(*end).copied().unwrap_or(text.len());
                let Some(captures) = regex.captures_at(text, byte_start) else {
                    return template.to_owned();
                };
                let Some(found) = captures.get(0) else {
                    return template.to_owned();
                };
                if found.start() != byte_start || found.end() != byte_end {
                    return template.to_owned();
                }
                let mut expanded = String::new();
                captures.expand(template, &mut expanded);
                expanded
            })
            .collect()
    }
}

/// Convenience wrapper for one-off searches.
pub fn find_all_with(
    text: &str,
    needle: &str,
    options: SearchOptions,
) -> Result<Vec<(usize, usize)>, String> {
    Ok(SearchMatcher::new(needle, options)?.find_all(text))
}

/// Find non-overlapping literal matches as character ranges.
pub fn find_all(text: &str, needle: &str, case_sensitive: bool) -> Vec<(usize, usize)> {
    find_all_with(
        text,
        needle,
        SearchOptions {
            case_sensitive,
            whole_word: false,
            regex: false,
        },
    )
    .unwrap_or_default()
}

fn literal_matches(text: &str, needle: &str, options: SearchOptions) -> Vec<(usize, usize)> {
    let hay: Vec<char> = text.chars().collect();
    let needle: Vec<char> = needle.chars().collect();
    if needle.len() > hay.len() {
        return Vec::new();
    }

    let equal = |left: char, right: char| {
        if options.case_sensitive {
            left == right
        } else {
            left.to_lowercase().eq(right.to_lowercase())
        }
    };

    let mut matches = Vec::new();
    let mut index = 0;
    while index + needle.len() <= hay.len() {
        let all_match = equal(hay[index], needle[0])
            && needle
                .iter()
                .enumerate()
                .skip(1)
                .all(|(offset, expected)| equal(hay[index + offset], *expected));
        if all_match
            && (!options.whole_word || whole_word_boundaries(&hay, index, index + needle.len()))
        {
            matches.push((index, index + needle.len()));
            index += needle.len();
        } else {
            index += 1;
        }
    }
    matches
}

fn regex_matches(text: &str, regex: &Regex, whole_word: bool) -> Vec<(usize, usize)> {
    let char_offsets: Vec<usize> = text.char_indices().map(|(byte, _)| byte).collect();
    let chars: Vec<char> = if whole_word {
        text.chars().collect()
    } else {
        Vec::new()
    };

    let mut matches = Vec::new();
    for found in regex.find_iter(text) {
        if found.start() == found.end() {
            continue;
        }
        let start = char_offsets.partition_point(|offset| *offset < found.start());
        let end = char_offsets.partition_point(|offset| *offset < found.end());
        if whole_word && !whole_word_boundaries(&chars, start, end) {
            continue;
        }
        matches.push((start, end));
    }
    matches
}

fn whole_word_boundaries(chars: &[char], start: usize, end: usize) -> bool {
    let before_ok = start == 0 || !is_identifier(chars[start - 1]);
    let after_ok = end >= chars.len() || !is_identifier(chars[end]);
    before_ok && after_ok
}

fn is_identifier(character: char) -> bool {
    character.is_alphanumeric() || character == '_' || character == '$'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_literal_matches() {
        assert_eq!(find_all("one two one", "one", true), vec![(0, 3), (8, 11)]);
        assert_eq!(find_all("One two one", "ONE", false), vec![(0, 3), (8, 11)]);
        assert!(find_all("abc", "ABC", true).is_empty());
        assert!(find_all("abc", "", true).is_empty());
    }

    #[test]
    fn matches_are_non_overlapping() {
        assert_eq!(find_all("aaaa", "aa", true), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn handles_unicode() {
        assert_eq!(find_all("ação ação", "ção", true), vec![(1, 4), (6, 9)]);
        assert_eq!(find_all("AÇÃO", "ação", false), vec![(0, 4)]);
    }

    #[test]
    fn whole_word_filters_substrings() {
        let matches = find_all_with(
            "user userName user_name",
            "user",
            SearchOptions {
                case_sensitive: true,
                whole_word: true,
                regex: false,
            },
        )
        .unwrap();
        assert_eq!(matches, vec![(0, 4)]);
    }

    #[test]
    fn regex_finds_groups() {
        let matches = find_all_with(
            "class App { void run() {} }",
            r"\b(void|class)\s+\w+",
            SearchOptions {
                case_sensitive: true,
                whole_word: false,
                regex: true,
            },
        )
        .unwrap();
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0], (0, 9));
    }

    #[test]
    fn expands_regex_capture_groups() {
        let matcher = SearchMatcher::new(
            r"(\w+)\s*=\s*(\w+)",
            SearchOptions {
                case_sensitive: true,
                whole_word: false,
                regex: true,
            },
        )
        .unwrap();
        let text = "int value = other;";
        let matches = matcher.find_all(text);
        let replacements = matcher.expand_replacements(text, &matches, "$2 = $1");
        assert_eq!(replacements, vec!["other = value"]);
    }

    #[test]
    fn reports_invalid_regex() {
        let error = find_all_with(
            "abc",
            "(",
            SearchOptions {
                case_sensitive: true,
                whole_word: false,
                regex: true,
            },
        )
        .unwrap_err();
        assert!(!error.is_empty());
    }
}
