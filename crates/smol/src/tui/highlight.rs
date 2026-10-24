use crate::tui::{theme, widened, Spans};
use ratatui::style::{Modifier, Style};
use reaper::terminal_view;
use std::ops::Range;

pub(crate) const INLINE_CODE: Style = Style::new().fg(theme::GREEN);
const KEYWORD: Style = Style::new().fg(theme::PINK);
const STRING: Style = Style::new().fg(theme::YELLOW);
const NUMBER: Style = Style::new().fg(theme::PURPLE);
const COMMENT: Style = Style::new()
    .fg(theme::COMMENT)
    .add_modifier(Modifier::ITALIC);
const DEFINITION: Style = Style::new().fg(theme::GREEN).add_modifier(Modifier::BOLD);
const CALL: Style = Style::new().fg(theme::GREEN);

const KEYWORDS: [&str; 35] = [
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

pub(crate) fn code(language: &str, code: &str) -> Vec<Spans> {
    if !matches!(language, "python" | "py" | "python3") {
        return plain(code, INLINE_CODE);
    }

    let code = terminal_view(code);
    let mut spans = Vec::new();
    let mut at = 0;
    for (range, style) in tokens(&code) {
        let before = code[at..range.start].to_owned();
        spans.push((before, Style::new()));

        let token = code[range.clone()].to_owned();
        spans.push((token, style));
        at = range.end;
    }
    let rest = code[at..].to_owned();
    spans.push((rest, Style::new()));

    lines(spans)
}

pub(crate) fn plain(text: &str, style: Style) -> Vec<Spans> {
    // Escape sequences left in would show up as text and throw off the width
    let text = terminal_view(text);
    let span = (text, style);
    lines(vec![span])
}

/// Not a full tokenizer, but it still splits half-written code
fn tokens(code: &str) -> Vec<(Range<usize>, Style)> {
    let mut found = Vec::new();
    let mut after_def = false;
    let mut at = 0;

    while let Some(c) = code[at..].chars().next() {
        let start = at;
        at += c.len_utf8();
        let style = match c {
            '#' => {
                let end = code[at..].find('\n');
                at = end.map_or(code.len(), |end| at + end);
                COMMENT
            }
            '"' | '\'' => {
                at = string_end(code, start);
                STRING
            }
            c if c.is_ascii_digit() => {
                at = word_end(code, at, true);
                NUMBER
            }
            c if c == '_' || c.is_alphabetic() => {
                at = word_end(code, at, false);
                let word = &code[start..at];
                let rest = &code[at..];
                let prefixed = word.len() <= 2 && word.chars().all(|c| "rRbBfFuU".contains(c));

                if prefixed && rest.starts_with(['"', '\'']) {
                    at = string_end(code, at);
                    STRING
                } else if KEYWORDS.contains(&word) {
                    let style = match word {
                        "True" | "False" | "None" => NUMBER,
                        _ => KEYWORD,
                    };
                    found.push((start..at, style));
                    after_def = matches!(word, "def" | "class");
                    continue;
                } else if after_def {
                    DEFINITION
                } else if rest.trim_start_matches([' ', '\t']).starts_with('(') {
                    CALL
                } else {
                    continue;
                }
            }
            c => {
                after_def &= c.is_whitespace();
                continue;
            }
        };

        after_def = false;
        found.push((start..at, style));
    }

    found
}

fn word_end(code: &str, from: usize, number: bool) -> usize {
    let rest = &code[from..];
    let outside = |c: char| !(c == '_' || c.is_alphanumeric() || (number && c == '.'));
    let end = rest.find(outside);
    from + end.unwrap_or(rest.len())
}

/// An unclosed string ends at the end of its line, or at the end of the code if triple-quoted
fn string_end(code: &str, start: usize) -> usize {
    let quote = &code[start..start + 1];
    let triple = quote.repeat(3);
    let (close, mut at) = match code[start..].starts_with(&triple) {
        true => (triple.as_str(), start + 3),
        false => (quote, start + 1),
    };

    while let Some(c) = code[at..].chars().next() {
        if code[at..].starts_with(close) {
            return at + close.len();
        }
        if c == '\n' && close.len() == 1 {
            return at;
        }
        at += c.len_utf8();
        if c == '\\' {
            let escaped = code[at..].chars().next();
            at += escaped.map_or(0, char::len_utf8);
        }
    }

    code.len()
}

fn lines(spans: Spans) -> Vec<Spans> {
    let mut lines = vec![Spans::new()];
    for (text, style) in spans {
        for (i, part) in text.split('\n').enumerate() {
            if i > 0 {
                lines.push(Spans::new());
            }
            if part.is_empty() {
                continue;
            }

            let line = lines.last_mut().expect("line");
            let part = widened(part);
            line.push((part, style));
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use crate::tui::highlight::*;

    fn classes(code: &str) -> Vec<(&str, Style)> {
        let found = tokens(code).into_iter();
        found.map(|(range, style)| (&code[range], style)).collect()
    }

    #[test]
    fn unfinished_code() {
        let code = "def f(x):\n    return print('hi', 1.5)  # note\ny = type(f\"{x}";
        assert_eq!(
            classes(code),
            [
                ("def", KEYWORD),
                ("f", DEFINITION),
                ("return", KEYWORD),
                ("print", CALL),
                ("'hi'", STRING),
                ("1.5", NUMBER),
                ("# note", COMMENT),
                ("type", CALL),
                ("f\"{x}", STRING),
            ]
        );
        assert_eq!(classes("café = None"), [("None", NUMBER)]);
    }

    #[test]
    fn non_ascii_code() {
        let code = "résumé = 'crème brûlée'  # déjà vu\nécrire(résumé, \"piñata";
        assert_eq!(
            classes(code),
            [
                ("'crème brûlée'", STRING),
                ("# déjà vu", COMMENT),
                ("écrire", CALL),
                ("\"piñata", STRING),
            ]
        );
    }

    #[test]
    fn string_quotes() {
        assert_eq!(classes(r#"s = 'café\'s'"#), [(r#"'café\'s'"#, STRING)]);
        assert_eq!(
            classes("doc = \"\"\"Café\n\"Olé\"\n\"\"\"\nx"),
            [("\"\"\"Café\n\"Olé\"\n\"\"\"", STRING)]
        );
    }

    #[test]
    fn styled_lines() {
        let lines = code("python", "def f():\n\treturn 'olé'  # done");

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0][0], ("def".to_owned(), KEYWORD));
        assert_eq!(lines[0][2], ("f".to_owned(), DEFINITION));
        assert!(lines[1].contains(&("    ".to_owned(), Style::new())));
        assert!(lines[1].contains(&("'olé'".to_owned(), STRING)));
        assert!(lines[1].contains(&("# done".to_owned(), COMMENT)));
    }

    #[test]
    fn terminal_cleaned() {
        let style = Style::new();
        let lines = plain("\x1b[1m10%\rdone\x1b[22m\r\nnext line\r", style);
        let done = vec![("done".to_owned(), style)];
        let next = vec![("next line".to_owned(), style)];
        assert_eq!(lines, [done, next]);

        let python = code("python", "x = 1\r\ny = 'olé'\r\n");
        assert_eq!(python.len(), 3);
        assert_eq!(python[1].last(), Some(&("'olé'".to_owned(), STRING)));
    }
}
