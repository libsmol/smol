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
pub(crate) struct Block {
    /// Before its first line, such as a list marker or the bars of quotes
    pub(crate) marker: String,
    /// Before its other lines
    pub(crate) indent: String,
    pub(crate) kind: Kind,
}

#[derive(Debug)]
pub(crate) enum Kind {
    Text(Text),
    /// The first row is the header
    Table {
        aligns: Vec<Alignment>,
        rows: Vec<Vec<Spans>>,
    },
    /// `code` is the Mermaid source and `source` is the code block showing it
    Diagram {
        lines: Vec<String>,
        code: String,
        source: Vec<Block>,
    },
    /// An image alone on its line, with its address as written
    Image {
        alt: String,
        url: String,
    },
}

#[derive(Debug)]
pub(crate) struct Text {
    /// Copying keeps list markers but drops the bars of quotes and code blocks
    pub(crate) copy_marker: bool,
    pub(crate) heading: bool,
    pub(crate) spans: Spans,
    /// Clickable URLs, with ranges as byte offsets into the joined text of `spans`
    pub(crate) links: Vec<(Range<usize>, String)>,
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
                    builder.heading = true;
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
                    builder.heading = false;
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
                        let text = Text {
                            copy_marker: false,
                            heading: false,
                            spans,
                            links: Vec::new(),
                        };
                        source.push(Block {
                            marker: format!("{lead}│ "),
                            indent: format!("{indent}│ "),
                            kind: Kind::Text(text),
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
                        Some(lines) => builder.blocks.push(Block {
                            marker,
                            indent,
                            kind: Kind::Diagram {
                                lines,
                                code: code.to_owned(),
                                source,
                            },
                        }),
                        None => builder.blocks.extend(source),
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
                    builder.blocks.push(Block {
                        marker,
                        indent,
                        kind: Kind::Table { aligns, rows },
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

/// The first line of text that `text` shows below its headings, without its Markdown,
/// or its first heading when nothing else has text
pub(crate) fn first_line(text: &str) -> String {
    let mut first_heading = None;
    for block in blocks(text) {
        let (shown, heading) = match block.kind {
            Kind::Text(text) => (joined(&text.spans), text.heading),
            Kind::Image { alt, .. } => (alt, false),
            Kind::Table { .. } | Kind::Diagram { .. } => continue,
        };

        let shown = shown.trim();
        if shown.is_empty() {
            continue;
        }
        if !heading {
            return shown.to_owned();
        }
        if first_heading.is_none() {
            first_heading = Some(shown.to_owned());
        }
    }
    first_heading.unwrap_or_default()
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
    heading: bool,
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

        let blank =
            |block: &Block| matches!(&block.kind, Kind::Text(text) if text.spans.is_empty());
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
            self.blocks.push(Block {
                marker,
                indent,
                kind: Kind::Image { alt, url },
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
        let text = Text {
            copy_marker,
            heading: self.heading,
            spans,
            links: mem::take(&mut self.line_links),
        };
        self.blocks.push(Block {
            marker,
            indent,
            kind: Kind::Text(text),
        });
    }
}
