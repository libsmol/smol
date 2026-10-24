use crate::tui::diagram;
use crate::tui::highlight::{self, INLINE_CODE};
use crate::tui::{joined, theme, widened, Spans, BOLD, DIM, HEADING};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use std::mem;
use std::ops::Range;
use unicode_width::UnicodeWidthStr;
use url::Url;

const STRONG_TEXT: Style = BOLD.fg(theme::ORANGE);
const EMPHASIS: Style = Style::new()
    .fg(theme::YELLOW)
    .add_modifier(Modifier::ITALIC);
pub(crate) const LINK: Style = Style::new()
    .fg(theme::CYAN)
    .add_modifier(Modifier::UNDERLINED);

#[derive(Debug, PartialEq)]
pub(crate) struct Text {
    pub(crate) marker: String,
    pub(crate) indent: String,
    /// Copying keeps list markers but drops the bars of quotes and code blocks
    pub(crate) copy_marker: bool,
    pub(crate) spans: Spans,
    /// Clickable URLs, with ranges as byte offsets into the joined text of `spans`
    pub(crate) links: Vec<(Range<usize>, String)>,
}

#[derive(Debug, PartialEq)]
pub(crate) enum Block {
    Text(Text),
    /// The first row is the header
    Table {
        marker: String,
        indent: String,
        aligns: Vec<Alignment>,
        rows: Vec<Vec<Spans>>,
    },
    /// `code` is the Mermaid source and `source` is the code block showing it
    Diagram {
        marker: String,
        indent: String,
        lines: Vec<String>,
        code: String,
        source: Vec<Text>,
    },
}

pub(crate) fn blocks(text: &str) -> Vec<Block> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut builder = Builder::default();

    let parser = Parser::new_ext(text, options);
    for (event, range) in parser.into_offset_iter() {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => builder.gap(),
                Tag::Heading { .. } => {
                    builder.gap();
                    builder.styles.push(HEADING);
                }
                Tag::BlockQuote(_) => {
                    builder.gap();
                    builder.prefixes.push(Prefix::Quote);
                }
                Tag::CodeBlock(kind) => {
                    builder.gap();
                    let language = match kind {
                        CodeBlockKind::Fenced(info) => {
                            let first = info.split_whitespace().next();
                            first.unwrap_or_default().to_owned()
                        }
                        CodeBlockKind::Indented => String::new(),
                    };
                    builder.code = Some((language, String::new()));
                }
                Tag::List(start) => {
                    builder.flush();
                    if builder.lists.is_empty() {
                        builder.gap();
                    }
                    builder.lists.push(start);
                }
                Tag::Item => {
                    builder.flush();
                    let marker = match builder.lists.last_mut() {
                        Some(Some(number)) => {
                            let marker = format!("{number}. ");
                            *number += 1;
                            marker
                        }
                        _ => "• ".to_owned(),
                    };
                    builder.prefixes.push(Prefix::Marker(marker));
                }
                Tag::Emphasis => builder.styles.push(EMPHASIS),
                Tag::Strong => builder.styles.push(STRONG_TEXT),
                Tag::Strikethrough => {
                    let crossed = Style::new().add_modifier(Modifier::CROSSED_OUT);
                    builder.styles.push(crossed);
                }
                Tag::Link {
                    link_type,
                    dest_url,
                    ..
                } => {
                    builder.styles.push(LINK);
                    let bare = matches!(link_type, LinkType::Autolink | LinkType::Email);
                    let start = builder.offset();
                    builder.links.push(Linking {
                        url: dest_url.into_string(),
                        bare,
                        start,
                    });
                }
                Tag::Image { .. } => builder.styles.push(DIM),
                Tag::HtmlBlock => builder.gap(),
                Tag::Table(aligns) => {
                    builder.gap();
                    builder.table = Some(Vec::new());
                    builder.aligns = aligns;
                }
                Tag::TableHead | Tag::TableRow => {
                    if let Some(rows) = &mut builder.table {
                        rows.push(Vec::new());
                    }
                }
                Tag::TableCell => {
                    let row = builder.table.as_mut().and_then(|rows| rows.last_mut());
                    if let Some(row) = row {
                        row.push(Vec::new());
                    }
                }
                _ => {}
            },

            Event::End(tag) => match tag {
                TagEnd::Paragraph => builder.flush(),
                TagEnd::Heading(_) => {
                    builder.flush();
                    builder.styles.pop();
                }
                TagEnd::BlockQuote(_) => {
                    builder.flush();
                    builder.prefixes.pop();
                }
                TagEnd::CodeBlock => {
                    let (language, code) = builder.code.take().unwrap_or_default();
                    let code = code.trim_end_matches('\n');
                    let (marker, indent) = builder.prefix();
                    let highlighted = highlight::code(&language, code);
                    let mut source = Vec::new();
                    for (i, spans) in highlighted.into_iter().enumerate() {
                        let lead = if i == 0 { &marker } else { &indent };
                        source.push(Text {
                            marker: format!("{lead}│ "),
                            indent: format!("{indent}│ "),
                            copy_marker: false,
                            spans,
                            links: Vec::new(),
                        });
                    }

                    // A diagram still streaming has no closing fence yet and shows as source
                    let block = text[range].trim_end();
                    let fence = block.lines().skip(1).last().unwrap_or_default();
                    // Lines inside a quote carry a > prefix
                    let fence = fence.trim_start_matches(|c: char| c == '>' || c.is_whitespace());
                    let closed = fence.starts_with("```") || fence.starts_with("~~~");
                    let drawn = match language == "mermaid" && closed {
                        true => diagram::drawn(code),
                        false => None,
                    };
                    match drawn {
                        Some(lines) => builder.blocks.push(Block::Diagram {
                            marker,
                            indent,
                            lines,
                            code: code.to_owned(),
                            source,
                        }),
                        None => {
                            let texts = source.into_iter().map(Block::Text);
                            builder.blocks.extend(texts);
                        }
                    }
                }
                TagEnd::List(_) => {
                    builder.flush();
                    builder.lists.pop();
                }
                TagEnd::Item => {
                    builder.flush();
                    builder.prefixes.pop();
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Image => {
                    builder.styles.pop();
                }
                TagEnd::Link => {
                    builder.styles.pop();
                    let Some(link) = builder.links.pop() else {
                        continue;
                    };
                    let target = builder.target();
                    let text = joined(target);
                    if !link.bare && !text.ends_with(&link.url) {
                        let shown = format!(" ({})", link.url);
                        builder.push(&shown, DIM);
                    }

                    if web(&link.url) && builder.table.is_none() {
                        let end = builder.offset();
                        builder.line_links.push((link.start..end, link.url));
                    }
                }
                TagEnd::Table => {
                    let rows = builder.table.take().unwrap_or_default();
                    let aligns = mem::take(&mut builder.aligns);
                    let (marker, indent) = builder.prefix();
                    builder.blocks.push(Block::Table {
                        marker,
                        indent,
                        aligns,
                        rows,
                    });
                }
                _ => {}
            },

            Event::Text(text) | Event::InlineHtml(text) => builder.text(&text),
            Event::Html(html) => {
                let html = html.trim_end_matches('\n');
                builder.text(html);
                builder.flush();
            }
            Event::Code(code) => builder.push(&code, INLINE_CODE),
            Event::SoftBreak | Event::HardBreak => builder.flush(),
            Event::Rule => {
                builder.gap();
                builder.push("───", DIM);
                builder.flush();
            }
            Event::TaskListMarker(done) => {
                let marker = if done { "[x] " } else { "[ ] " };
                builder.push(marker, DIM);
            }
            _ => {}
        }
    }

    builder.flush();
    builder.blocks
}

enum Prefix {
    Quote,
    Indent(usize),
    Marker(String),
}

struct Linking {
    url: String,
    /// An autolink's text is already the address, so it is not written out again
    bare: bool,
    /// Byte offset where it starts in the current line's text
    start: usize,
}

#[derive(Default)]
struct Builder {
    blocks: Vec<Block>,
    line: Spans,
    line_links: Vec<(Range<usize>, String)>,
    styles: Vec<Style>,
    prefixes: Vec<Prefix>,
    lists: Vec<Option<u64>>,
    code: Option<(String, String)>,
    links: Vec<Linking>,
    table: Option<Vec<Vec<Spans>>>,
    aligns: Vec<Alignment>,
}

/// Opening a local file link from the model could run a program, so only web pages open
fn web(url: &str) -> bool {
    let parsed = Url::parse(url);
    parsed.is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

impl Builder {
    fn text(&mut self, text: &str) {
        match &mut self.code {
            Some((_, code)) => code.push_str(text),
            None => {
                let mut style = Style::new();
                for inner in &self.styles {
                    style = style.patch(*inner);
                }
                self.push(text, style);
            }
        }
    }

    fn push(&mut self, text: &str, style: Style) {
        let text = widened(text);
        self.target().push((text, style));
    }

    fn target(&mut self) -> &mut Spans {
        let row = self.table.as_mut().and_then(|rows| rows.last_mut());
        let cell = row.and_then(|row| row.last_mut());
        cell.unwrap_or(&mut self.line)
    }

    fn offset(&self) -> usize {
        self.line.iter().map(|(text, _)| text.len()).sum()
    }

    /// Call it once per block because it turns list markers into indentation
    fn prefix(&mut self) -> (String, String) {
        let mut marker = String::new();
        let mut indent = String::new();
        for kind in &mut self.prefixes {
            match kind {
                Prefix::Quote => {
                    marker.push_str("▏ ");
                    indent.push_str("▏ ");
                }
                Prefix::Indent(width) => {
                    let spaces = " ".repeat(*width);
                    marker.push_str(&spaces);
                    indent.push_str(&spaces);
                }
                Prefix::Marker(text) => {
                    let width = text.width();
                    let spaces = " ".repeat(width);
                    marker.push_str(text);
                    indent.push_str(&spaces);
                    *kind = Prefix::Indent(width);
                }
            }
        }
        (marker, indent)
    }

    /// The first paragraph of a list item follows its marker with no blank line
    fn gap(&mut self) {
        self.flush();

        let blank = |block: &Block| matches!(block, Block::Text(text) if text.spans.is_empty());
        let after_content = self.blocks.last().is_some_and(|block| !blank(block));
        let marking = self
            .prefixes
            .iter()
            .any(|kind| matches!(kind, Prefix::Marker(_)));
        if after_content && !marking {
            self.push_block(Vec::new(), false);
        }
    }

    fn flush(&mut self) {
        if self.line.is_empty() {
            return;
        }

        // A link that spans lines records the part on this line first
        let end = self.offset();
        for link in &mut self.links {
            if web(&link.url) {
                let part = (link.start..end, link.url.clone());
                self.line_links.push(part);
            }
            link.start = 0;
        }

        let quote = |kind: &Prefix| matches!(kind, Prefix::Quote);
        let quoted = self.prefixes.iter().any(quote);
        let spans = mem::take(&mut self.line);
        self.push_block(spans, !quoted);
    }

    fn push_block(&mut self, spans: Spans, copy_marker: bool) {
        let (marker, indent) = self.prefix();
        let links = mem::take(&mut self.line_links);
        self.blocks.push(Block::Text(Text {
            marker,
            indent,
            copy_marker,
            spans,
            links,
        }));
    }
}

#[cfg(test)]
mod tests {
    use crate::tui::markdown::*;

    fn lines(text: &str) -> Vec<String> {
        let mut lines = Vec::new();
        for block in blocks(text) {
            match block {
                Block::Text(text) => {
                    let joined = joined(&text.spans);
                    lines.push(text.marker + &joined);
                }
                Block::Table { marker, rows, .. } => {
                    for (i, row) in rows.iter().enumerate() {
                        let lead = if i == 0 { marker.as_str() } else { "" };
                        let cells = row.iter().map(Vec::as_slice).map(joined);
                        let cells: Vec<String> = cells.collect();
                        let cells = cells.join(" | ");
                        lines.push(lead.to_owned() + &cells);
                    }
                }
                Block::Diagram {
                    marker,
                    lines: drawn,
                    ..
                } => {
                    for (i, line) in drawn.into_iter().enumerate() {
                        let lead = if i == 0 { marker.as_str() } else { "" };
                        lines.push(lead.to_owned() + &line);
                    }
                }
            }
        }
        lines
    }

    #[test]
    fn markers_rendered() {
        let text = "# Title\n\nSome **bold** and `code`.\n\n- one\n- two\n  1. nested\n\n> quote";
        let expected = [
            "Title",
            "",
            "Some bold and code.",
            "",
            "• one",
            "• two",
            "  1. nested",
            "",
            "▏ quote",
        ];
        assert_eq!(lines(text), expected);
    }

    #[test]
    fn inline_styles() {
        let parsed = blocks("plain **bold** *italic* `code`");
        let Block::Text(first) = &parsed[0] else {
            panic!("{parsed:?}");
        };
        let style = |text: &str| {
            let found = first.spans.iter().find(|(span, _)| span == text);
            found.map(|(_, style)| *style)
        };

        assert_eq!(style("bold"), Some(STRONG_TEXT));
        assert_eq!(style("italic"), Some(EMPHASIS));
        assert_eq!(style("code"), Some(INLINE_CODE));
    }

    #[test]
    fn code_and_tables() {
        let text = "```python\nx = 'café'\nprint(x)\n```\n\nafter";
        let expected = ["│ x = 'café'", "│ print(x)", "", "after"];
        assert_eq!(lines(text), expected);

        let text = "Body\n\n| name | n |\n|---|---|\n| **José** | 1 |\n| Zoë | 22 |\n\nafter";
        let expected = ["Body", "", "name | n", "José | 1", "Zoë | 22", "", "after"];
        assert_eq!(lines(text), expected);
    }

    #[test]
    fn link_suffixes() {
        let text = "**bold** text [a\\\nb](http://x) [http://y](http://y) <me@x.io>";
        assert_eq!(
            lines(text),
            ["bold text a", "b (http://x) http://y me@x.io"]
        );

        let table = "| url | v |\n|---|---|\n| <https://a.io> | [docs](http://d) |";
        assert_eq!(lines(table), ["url | v", "https://a.io | docs (http://d)"]);
    }

    #[test]
    fn web_links_kept() {
        let links = |text: &str| -> Vec<Vec<(String, String)>> {
            let mut found = Vec::new();
            for block in blocks(text) {
                let Block::Text(text) = block else {
                    continue;
                };
                let joined = joined(&text.spans);
                let picked = text
                    .links
                    .iter()
                    .map(|(range, url)| (joined[range.clone()].to_owned(), url.clone()));
                found.push(picked.collect());
            }
            found
        };

        let text = "see the café [docs](https://café.fr/menü) and <https://a.io>, [local](src/main.rs) [mail](mailto:a@b.io)";
        let doc = (
            "docs (https://café.fr/menü)".to_owned(),
            "https://café.fr/menü".to_owned(),
        );
        let bare = ("https://a.io".to_owned(), "https://a.io".to_owned());
        assert_eq!(links(text), [vec![doc, bare]]);

        let split = links("see [the café\\\nmenu](http://x.io)");
        let first = ("the café".to_owned(), "http://x.io".to_owned());
        let second = ("menu (http://x.io)".to_owned(), "http://x.io".to_owned());
        assert_eq!(split, [vec![first], vec![second]]);

        let table = links("| site |\n|---|\n| [docs](https://a.io) |\n\nafter");
        assert_eq!(table, vec![Vec::new(); 2]);
    }

    #[test]
    fn html_block_gaps() {
        let text = "Body\n\n<div>\nhello\n</div>\n\nafter";
        let expected = ["Body", "", "<div>", "hello", "</div>", "", "after"];
        assert_eq!(lines(text), expected);
    }

    #[test]
    fn soft_breaks_kept() {
        let text =
            "The conclusion comes first.\nThe details follow.\n\n- one\n  two\n> quote\n> more";
        let expected = [
            "The conclusion comes first.",
            "The details follow.",
            "",
            "• one",
            "  two",
            "",
            "▏ quote",
            "▏ more",
        ];
        assert_eq!(lines(text), expected);
    }

    #[test]
    fn tabs_widened() {
        assert_eq!(lines("name\tvalue `a\tb`"), ["name    value a    b"]);
    }

    #[test]
    fn nested_item_markers() {
        let text = "- - one\n- > quote\n- | x | y |\n  |---|---|\n  | one | two |";
        let expected = ["• • one", "• ▏ quote", "• x | y", "one | two"];
        assert_eq!(lines(text), expected);
    }

    #[test]
    fn quote_gap_hidden() {
        let gap = Block::Text(Text {
            marker: "▏ ".to_owned(),
            indent: "▏ ".to_owned(),
            copy_marker: false,
            spans: Vec::new(),
            links: Vec::new(),
        });
        assert_eq!(blocks("> first\n>\n> second")[1], gap);
    }

    #[test]
    fn mermaid_drawn() {
        let diagram = "graph LR\n    A[read 🐍] --> B[write 🚀]";
        let closed = format!("```mermaid\n{diagram}\n```");
        let parsed = blocks(&closed);
        let Block::Diagram {
            lines: drawn,
            source,
            ..
        } = &parsed[0]
        else {
            panic!("{parsed:?}");
        };
        assert!(
            drawn.iter().any(|line| line.contains("│ read 🐍 │")),
            "{drawn:?}"
        );
        assert_eq!(source.len(), 2);

        let open = format!("```mermaid\n{diagram}");
        assert_eq!(
            lines(&open),
            ["│ graph LR", "│     A[read 🐍] --> B[write 🚀]"]
        );
        let broken = "```mermaid\ngraph TD\n    A[unclosed --> B\n```";
        assert_eq!(lines(broken)[0], "│ graph TD");

        let listed = format!("- Flow:\n\n  ```mermaid\n  {diagram}\n  ```");
        let parsed = blocks(&listed);
        let Some(Block::Diagram { marker, .. }) = parsed.last() else {
            panic!("{parsed:?}");
        };
        assert_eq!(marker, "  ");

        let quoted = "> ```mermaid\n> graph LR\n>     A[read 🐍] --> B[write 🚀]\n> ```";
        let parsed = blocks(quoted);
        let Some(Block::Diagram { marker, .. }) = parsed.first() else {
            panic!("{parsed:?}");
        };
        assert_eq!(marker, "▏ ");
    }

    #[test]
    fn python_highlighted() {
        let code = |text: &str| match &blocks(text)[0] {
            Block::Text(text) => text.spans.clone(),
            table => panic!("{table:?}"),
        };

        assert_eq!(code("```py\nimport os  # system\n```")[0].0, "import");

        let shell = [("ls -l docs".to_owned(), INLINE_CODE)];
        assert_eq!(code("```sh\nls -l docs\n```"), shell);
    }
}
