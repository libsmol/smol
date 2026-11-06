use crate::tui::diagram;
use crate::tui::highlight::{self, INLINE_CODE};
use crate::tui::{joined, theme, widened, Spans, BOLD, DIM, HEADING};
use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd, TextMergeWithOffset,
};
use ratatui::style::{Modifier, Style};
use std::mem;
use std::ops::Range;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use url::Url;

const STRONG_TEXT: Style = BOLD.fg(theme::ORANGE);
const EMPHASIS: Style = Style::new()
    .fg(theme::YELLOW)
    .add_modifier(Modifier::ITALIC);
pub(crate) const LINK: Style = Style::new()
    .fg(theme::CYAN)
    .add_modifier(Modifier::UNDERLINED);

#[derive(Debug)]
pub(crate) struct Text {
    pub(crate) marker: String,
    pub(crate) indent: String,
    /// Copying keeps list markers but drops the bars of quotes and code blocks
    pub(crate) copy_marker: bool,
    pub(crate) spans: Spans,
    /// Clickable URLs, with ranges as byte offsets into the joined text of `spans`
    pub(crate) links: Vec<(Range<usize>, String)>,
}

#[derive(Debug)]
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
    /// An image alone on its line, with its address as written
    Image {
        marker: String,
        indent: String,
        alt: String,
        url: String,
    },
}

pub(crate) fn blocks(text: &str) -> Vec<Block> {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut builder = Builder::default();

    let parser = Parser::new_ext(text, options);
    // The parser splits text at entities and stray delimiters, and an address must arrive whole
    let events = TextMergeWithOffset::new(parser.into_offset_iter());
    for (event, range) in events {
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
                Tag::Image { dest_url, .. } => {
                    builder.styles.push(DIM);
                    // An image inside a link or a table cell, or after one without alt text,
                    // stays its alt text
                    let starts = builder.line.is_empty() && builder.alone.is_none();
                    let alone = starts && builder.links.is_empty() && builder.table.is_none();
                    builder.image = alone.then(|| dest_url.into_string());
                    builder.alone = None;
                }
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
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    builder.styles.pop();
                }
                TagEnd::Image => {
                    builder.styles.pop();
                    if let Some(url) = builder.image.take() {
                        let alt = joined(&builder.line);
                        builder.alone = Some((url, alt));
                    }
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

            Event::Text(text) => builder.text(&text),
            Event::InlineHtml(html) => {
                let marks = |c: char| matches!(c, '<' | '>' | '/' | ' ');
                let tag = html.replace(marks, "");
                let broken = tag.eq_ignore_ascii_case("br");
                // Flushing ends the line, which holds no table cell's text, but drops an empty one
                let empty = builder.line.is_empty() && builder.alone.is_none();
                match (broken, builder.table.is_some()) {
                    (true, true) => builder.push("\n", Style::new()),
                    (true, false) if empty => builder.push_block(Vec::new(), false),
                    (true, false) => builder.flush(),
                    (false, _) => builder.text(&html),
                }
            }
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
    /// The address of an image that started its line, until the image ends
    image: Option<String>,
    /// An ended image with nothing after it on its line so far, and its alt text
    alone: Option<(String, String)>,
}

/// Opening a local file link from the model could run a program, so only web pages open
fn web(url: &str) -> bool {
    let parsed = Url::parse(url);
    parsed.is_ok_and(|url| matches!(url.scheme(), "http" | "https"))
}

/// Web addresses written out in `text`, without the punctuation that ends a sentence around them
/// or the Chinese text that may run into them with no space
fn bare_urls(text: &str) -> Vec<Range<usize>> {
    let ends = |c: char| {
        let wide = c.width() == Some(2);
        c.is_whitespace() || matches!(c, '<' | '>' | '"') || wide
    };
    let trailing = ['.', ',', ':', ';', '!', '?', '\'', '*', '_', '~'];

    let mut found = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find("http") {
        let start = from + at;
        let rest = &text[start..];
        let glued = text[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric());
        let scheme = rest.starts_with("http://") || rest.starts_with("https://");
        if glued || !scheme {
            from = start + "http".len();
            continue;
        }

        let length = rest.find(ends).unwrap_or(rest.len());
        let mut url = &rest[..length];
        loop {
            let trimmed = url.trim_end_matches(trailing);
            let open = trimmed.matches('(').count();
            let unbalanced = trimmed.ends_with(')') && trimmed.matches(')').count() > open;
            let trimmed = match unbalanced {
                true => &trimmed[..trimmed.len() - 1],
                false => trimmed,
            };
            if trimmed.len() == url.len() {
                break;
            }
            url = trimmed;
        }
        if web(url) {
            found.push(start..start + url.len());
        }
        from = start + length;
    }
    found
}

impl Builder {
    fn text(&mut self, text: &str) {
        if let Some((_, code)) = &mut self.code {
            return code.push_str(text);
        }

        let mut style = Style::new();
        for inner in &self.styles {
            style = style.patch(*inner);
        }
        // An address inside a link is that link's text
        let urls = match self.links.is_empty() {
            true => bare_urls(text),
            false => Vec::new(),
        };
        if urls.is_empty() {
            return self.push(text, style);
        }

        let mut done = 0;
        for range in urls {
            if range.start > done {
                self.push(&text[done..range.start], style);
            }
            let url = &text[range.start..range.end];
            let start = self.offset();
            self.push(url, style.patch(LINK));
            if self.table.is_none() {
                let end = self.offset();
                self.line_links.push((start..end, url.to_owned()));
            }
            done = range.end;
        }
        if done < text.len() {
            self.push(&text[done..], style);
        }
    }

    fn push(&mut self, text: &str, style: Style) {
        if self.image.is_none() {
            self.alone = None;
        }
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
        // Its alt text may be empty, which leaves the line empty too
        if let Some((url, alt)) = self.alone.take() {
            self.line.clear();
            self.line_links.clear();
            let (marker, indent) = self.prefix();
            self.blocks.push(Block::Image {
                marker,
                indent,
                alt,
                url,
            });
            return;
        }
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
                Block::Image {
                    marker, alt, url, ..
                } => lines.push(format!("{marker}![{alt}]({url})")),
            }
        }
        lines
    }

    #[test]
    fn images_alone() {
        let text = "Before\n\n![Monthly café sales](charts/café.png)\n\n- ![](a.png)\n\nInline ![x](y.png) and ![z](z.png) after\n\n![first](1.png)\n![second](2.png) ends\n\n[![badge](b.svg)](https://x.io)\n\n![](c.png)![](d.png)";
        let expected = [
            "Before",
            "",
            "![Monthly café sales](charts/café.png)",
            "",
            "• ![](a.png)",
            "",
            "Inline x and z after",
            "",
            "![first](1.png)",
            "second ends",
            "",
            "badge (https://x.io)",
            "",
        ];
        assert_eq!(lines(text), expected);
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
    fn table_gaps() {
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

        // The parser hands the text over in pieces at the entity
        let text = "menu at https://café.fr/?a=1&amp;b=2, not `https://a.io` or [https://b.io](https://b.io)";
        let bare = (
            "https://café.fr/?a=1&b=2".to_owned(),
            "https://café.fr/?a=1&b=2".to_owned(),
        );
        let named = ("https://b.io".to_owned(), "https://b.io".to_owned());
        assert_eq!(links(text), [vec![bare, named]]);
        let parsed = blocks(text);
        let Block::Text(first) = &parsed[0] else {
            panic!("{parsed:?}");
        };
        let styled = first
            .spans
            .iter()
            .find(|(span, _)| span.starts_with("https://café"));
        assert_eq!(styled.map(|(_, style)| *style), Some(LINK));

        // One in a table is styled but not clickable, as explicit links there are
        let cells = "| site |\n|---|\n| see https://a.io |\n\nafter https://b.io";
        let after = ("https://b.io".to_owned(), "https://b.io".to_owned());
        assert_eq!(links(cells), [Vec::new(), vec![after]]);
        let parsed = blocks(cells);
        let Block::Table { rows, .. } = &parsed[0] else {
            panic!("{parsed:?}");
        };
        let styled = rows[1][0].iter().find(|(span, _)| span == "https://a.io");
        assert_eq!(styled.map(|(_, style)| *style), Some(LINK));
    }

    #[test]
    fn bare_urls_found() {
        let cases: [(&str, &[&str]); 8] = [
            (
                "menu at https://café.fr/menü, then",
                &["https://café.fr/menü"],
            ),
            ("(see https://a.io/x_(y)).", &["https://a.io/x_(y)"]),
            ("https://a.io。next", &["https://a.io"]),
            ("详见https://a.io", &["https://a.io"]),
            ("https://a.io/x了解", &["https://a.io/x"]),
            (
                "\"https://a.io/?next=http://b.io\"",
                &["https://a.io/?next=http://b.io"],
            ),
            ("xhttps://a.io and http:// alone", &[]),
            (
                "http://a.io https://b.io!",
                &["http://a.io", "https://b.io"],
            ),
        ];
        for (text, expected) in cases {
            let found = bare_urls(text).into_iter().map(|range| &text[range]);
            let found: Vec<&str> = found.collect();
            assert_eq!(found, expected, "{text}");
        }
    }

    #[test]
    fn html_breaks() {
        assert_eq!(lines("Café<br/>naïve<br>olé"), ["Café", "naïve", "olé"]);
        assert_eq!(
            lines("Café<BR />naïve <b>olé</b>"),
            ["Café", "naïve <b>olé</b>"]
        );
        assert_eq!(lines("Café<br><br>naïve"), ["Café", "", "naïve"]);

        let table = blocks("| dish |\n|---|\n| crème<br>brûlée |");
        let Block::Table { rows, .. } = &table[0] else {
            panic!("{table:?}");
        };
        assert_eq!(joined(&rows[1][0]), "crème\nbrûlée");
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
