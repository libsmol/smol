use crate::prompt;
use crate::tools::ToolError;
use crate::utils::{content_type, excerpt, http_client, read_cause};
use encoding_rs::{Encoding, UTF_8};
use html5ever::driver::{parse_fragment, ParseOpts};
use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{local_name, ns, QualName};
use reqwest::header::LOCATION;
use reqwest::redirect::Policy;
use reqwest::{Client, Url};
use scraper::{ElementRef, Html, HtmlTreeSink, Node};
use std::str;
use std::sync::LazyLock;
use std::time::Duration;

/// Limits the connect and each read separately, not the whole request
const FETCH_WAIT: Duration = Duration::from_secs(20);
const MAX_PAGE_BYTES: u64 = 32 * 1024 * 1024;
/// Error pages often put their message after a long block of styles
const MAX_EXCERPT_BYTES: usize = 64 * 1024;
const SNIFF_BYTES: usize = 8192;
/// Where a page that declares its charset does so
const CHARSET_BYTES: usize = 1024;
/// Past this depth the recursive walk only takes text, as deep nesting would exhaust the stack
const MAX_DEPTH: usize = 256;
/// HTTP redirects and in-page refreshes count together
const MAX_REDIRECTS: usize = 10;

static CLIENT: LazyLock<Result<Client, String>> = LazyLock::new(|| {
    // When reqwest follows a redirect and cannot connect it reports the original host
    let builder = Client::builder()
        .user_agent("Mozilla/5.0")
        .redirect(Policy::none())
        .read_timeout(FETCH_WAIT);
    http_client(builder)
});

pub(crate) async fn fetch(url: &str) -> Result<String, ToolError> {
    let client = match &*CLIENT {
        Ok(client) => client,
        Err(reason) => {
            let message = prompt::failure("fetch", url, reason);
            return Err(ToolError::Failed(message));
        }
    };

    let mut target = url.to_owned();
    let mut hops = 0;
    loop {
        if hops > MAX_REDIRECTS {
            let message = prompt::failure("fetch", url, prompt::TOO_MANY_REDIRECTS);
            return Err(ToolError::Failed(message));
        }

        let sent = client.get(&target).send().await;
        let mut response = sent.map_err(|e| failed(url, e))?;
        let base = response.url().clone();

        let status = response.status();
        let moved = matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308);
        let location = response.headers().get(LOCATION);
        let location = location.and_then(|value| value.to_str().ok());
        let next = location.and_then(|location| web_url(&base, location));
        if let Some(next) = next.filter(|_| moved) {
            hops += 1;
            target = next.into();
            continue;
        }

        let (mime, label) = content_type(response.headers());
        let encoding = label.and_then(|label| Encoding::for_label(label.as_bytes()));
        let html = matches!(mime.as_str(), "" | "text/html" | "application/xhtml+xml");

        if !status.is_success() {
            // An error body may stream forever and only its start is read
            let mut body = Vec::new();
            while body.len() < MAX_EXCERPT_BYTES {
                let Ok(Some(chunk)) = response.chunk().await else {
                    break;
                };
                body.extend_from_slice(&chunk);
            }

            let body = decoded(&body, encoding, html);
            let body = match html {
                true => {
                    let page = parsed(&body);
                    markdown(&page, &base)
                }
                false => body,
            };
            let said = excerpt(&body);

            let code = status.as_u16();
            let reason = prompt::http_status(code, &said);
            let message = prompt::failure("fetch", url, &reason);
            return Err(ToolError::Failed(message));
        }

        let textual = ["json", "xml", "javascript"]
            .iter()
            .any(|suffix| mime.ends_with(suffix));
        // nginx sends .md, .yaml and the like as application/octet-stream, so check the content
        let mut sniffing = !html && !textual && !mime.starts_with("text/");

        let mut bytes = Vec::new();
        while bytes.len() as u64 <= MAX_PAGE_BYTES {
            let read = response.chunk().await;
            let Some(chunk) = read.map_err(|e| failed(url, e))? else {
                break;
            };
            bytes.extend_from_slice(&chunk);
            if !sniffing {
                continue;
            }

            // A character cut off at the end of the head doesn't count
            let end = bytes.len().min(SNIFF_BYTES);
            let head = &bytes[..end];
            let undecodable = match str::from_utf8(head) {
                Ok(_) => false,
                Err(e) => e.error_len().is_some(),
            };
            if head.contains(&0) || undecodable {
                let message = prompt::unfetchable(url, &mime);
                return Err(ToolError::Failed(message));
            }
            sniffing = end < SNIFF_BYTES;
        }
        let cut = bytes.len() as u64 > MAX_PAGE_BYTES;
        bytes.truncate(MAX_PAGE_BYTES as usize);

        let text = decoded(&bytes, encoding, html);
        let text = match html {
            true => {
                let page = parsed(&text);
                if let Some(next) = refresh(&page, &base) {
                    hops += 1;
                    target = next.into();
                    continue;
                }
                markdown(&page, &base)
            }
            false => text,
        };
        if !cut {
            return Ok(text);
        }

        let limit = MAX_PAGE_BYTES >> 20;
        let marker = prompt::cut_at(limit);
        return Ok(format!("{text}\n{marker}"));
    }
}

fn parsed(text: &str) -> Html {
    // The default parses as if scripting were on, which turns noscript content into plain text
    let opts = ParseOpts {
        tree_builder: TreeBuilderOpts {
            scripting_enabled: false,
            ..TreeBuilderOpts::default()
        },
        ..ParseOpts::default()
    };
    let body = QualName::new(None, ns!(html), local_name!("body"));
    let sink = HtmlTreeSink::new(Html::new_fragment());
    let parser = parse_fragment(sink, opts, body, Vec::new(), false);
    parser.one(text)
}

fn failed(url: &str, error: reqwest::Error) -> ToolError {
    let reason = read_cause(&error, FETCH_WAIT);
    let message = prompt::failure("fetch", url, &reason);
    ToolError::Failed(message)
}

/// In the encoding the response names, or for a page without one, the one the page declares
fn decoded(bytes: &[u8], encoding: Option<&'static Encoding>, html: bool) -> String {
    let encoding = match encoding {
        Some(encoding) => encoding,
        None if html => declared(bytes).unwrap_or(UTF_8),
        None => UTF_8,
    };
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

fn declared(bytes: &[u8]) -> Option<&'static Encoding> {
    let end = bytes.len().min(CHARSET_BYTES);
    let head = String::from_utf8_lossy(&bytes[..end]).to_ascii_lowercase();
    let (_, rest) = head.split_once("charset=")?;
    let rest = rest.trim_start_matches(['"', '\'']);
    let label = rest.split(['"', '\'', ';', '>', ' ', '/']).next()?;

    let encoding = Encoding::for_label(label.as_bytes())?;
    Some(encoding.output_encoding())
}

/// Only follows refreshes with a zero delay
fn refresh(page: &Html, base: &Url) -> Option<Url> {
    // Browsers only honor the first one
    let mut content = None;
    for node in page.root_element().descendants() {
        let Some(el) = ElementRef::wrap(node) else {
            continue;
        };
        let equiv = el.attr("http-equiv").unwrap_or_default();
        if el.value().name() == "meta" && equiv.eq_ignore_ascii_case("refresh") {
            content = el.attr("content");
            break;
        }
    }
    let content = content?.trim_start();

    // Only the whole seconds count, followed by an optional `;` or `,`, `url=` and quotes
    let end = content.find(|c: char| !c.is_ascii_digit() && c != '.');
    let end = end.unwrap_or(content.len());
    let (delay, rest) = content.split_at(end);
    let (whole, _) = delay.split_once('.').unwrap_or((delay, ""));
    if delay.is_empty() || !whole.trim_start_matches('0').is_empty() {
        return None;
    }
    let rest = rest.trim_start();
    let rest = rest.strip_prefix([';', ',']).unwrap_or(rest).trim_start();
    let named = rest
        .get(..3)
        .is_some_and(|word| word.eq_ignore_ascii_case("url"));
    let rest = match named {
        true => {
            let after = rest[3..].trim_start();
            after.strip_prefix('=').map_or(rest, str::trim_start)
        }
        false => rest,
    };
    let link = match rest.strip_prefix(['\'', '"']) {
        Some(quoted) => {
            let quote = &rest[..1];
            let (link, _) = quoted.split_once(quote).unwrap_or((quoted, ""));
            link
        }
        None => rest.trim_end(),
    };
    if link.is_empty() {
        return None;
    }

    let next = web_url(base, link)?;
    (next != *base).then_some(next)
}

/// `link` read against `base`, when it leads to a web page
fn web_url(base: &Url, link: &str) -> Option<Url> {
    let url = base.join(link).ok()?;
    let web = matches!(url.scheme(), "http" | "https");
    web.then_some(url)
}

fn markdown(page: &Html, base: &Url) -> String {
    let mut writer = Markdown {
        base,
        out: String::new(),
        in_pre: false,
        opened: false,
        lists: Vec::new(),
        depth: 0,
    };
    let root = page.root_element();
    writer.walk(root);

    let mut text = String::new();
    let mut newlines = 0;
    for ch in writer.out.trim().chars() {
        newlines = if ch == '\n' { newlines + 1 } else { 0 };
        if newlines <= 2 {
            text.push(ch);
        }
    }

    text
}

struct Markdown<'a> {
    base: &'a Url,
    out: String,
    in_pre: bool,
    /// A mark such as `- ` or `[` was just written and no content has followed yet
    opened: bool,
    /// One per nesting level, holding the count for ordered lists
    lists: Vec<Option<usize>>,
    depth: usize,
}

impl Markdown<'_> {
    fn walk(&mut self, el: ElementRef<'_>) {
        if el.attr("hidden").is_some() {
            return;
        }
        let style = el.attr("style").unwrap_or_default();
        for declaration in style.split(';') {
            let (property, value) = declaration.split_once(':').unwrap_or_default();
            let value = value.trim().to_ascii_lowercase();
            if property.trim().eq_ignore_ascii_case("display") && value.starts_with("none") {
                return;
            }
        }

        let tag = el.value().name();
        let (open, close) = match tag {
            "script" | "style" | "template" => return,
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let level = tag[1..].parse().unwrap_or(1);
                let marks = "#".repeat(level);
                (format!("\n{marks} "), "\n".into())
            }
            "p" | "div" | "section" | "article" | "header" | "footer" | "nav" | "main"
            | "aside" | "figure" | "figcaption" | "table" | "dl" | "dt" | "dd" | "form" | "hr" => {
                ("\n".into(), "\n".into())
            }
            "br" | "tr" => ("\n".into(), String::new()),
            "blockquote" => ("\n> ".into(), "\n".into()),
            "td" | "th" => (" | ".into(), String::new()),
            "pre" => ("\n```\n".into(), "\n```\n".into()),
            "code" if !self.in_pre => ("`".into(), "`".into()),
            "ul" | "ol" => {
                let ordered = tag == "ol";
                let count = ordered.then_some(0);
                self.lists.push(count);
                (String::new(), "\n".into())
            }
            "li" => {
                let levels = self.lists.len().saturating_sub(1);
                let indent = "  ".repeat(levels);
                match self.lists.last_mut() {
                    Some(Some(count)) => {
                        *count += 1;
                        (format!("\n{indent}{count}. "), String::new())
                    }
                    _ => (format!("\n{indent}- "), String::new()),
                }
            }
            "a" => {
                let href = el.attr("href").filter(|href| !href.starts_with('#'));
                let url = href.and_then(|href| web_url(self.base, href));
                match url {
                    Some(url) => ("[".into(), format!("]({url})")),
                    None => Default::default(),
                }
            }
            _ => Default::default(),
        };

        // Items, rows and fences still start a line of their own
        let joins = self.opened && open.starts_with('\n') && !matches!(tag, "li" | "tr" | "pre");
        let open = match joins {
            true => open.trim_start_matches('\n'),
            false => open.as_str(),
        };
        if open.starts_with(char::is_whitespace) {
            let kept = self.out.trim_end_matches(' ').len();
            self.out.truncate(kept);
        }
        self.out.push_str(open);
        // A new line ends the mark, which text in a fence never does
        if open.starts_with('\n') {
            self.opened = false;
        }
        self.opened |= open.ends_with(|c: char| c != '\n');
        let in_pre = self.in_pre;
        self.in_pre |= tag == "pre";

        for child in el.children() {
            match child.value() {
                Node::Text(text) => self.write(text),
                Node::Element(_) => {
                    let child = ElementRef::wrap(child).expect("element node");
                    if self.depth >= MAX_DEPTH {
                        for text in child.text() {
                            self.write(text);
                        }
                        continue;
                    }
                    self.depth += 1;
                    self.walk(child);
                    self.depth -= 1;
                }
                _ => {}
            }
        }

        self.in_pre = in_pre;
        if matches!(tag, "ul" | "ol") {
            self.lists.pop();
        }

        if joins {
            self.write(" ");
        } else if !close.is_empty() {
            let kept = self.out.trim_end_matches(' ').len();
            let spaced = kept < self.out.len();
            self.out.truncate(kept);
            self.out.push_str(&close);
            self.opened = false;
            if spaced && !close.ends_with('\n') {
                self.out.push(' ');
            }
        }
    }

    fn write(&mut self, text: &str) {
        if self.in_pre {
            self.out.push_str(text);
            return;
        }

        for ch in text.chars() {
            if !ch.is_whitespace() {
                self.out.push(ch);
                self.opened = false;
                continue;
            }
            let last = self.out.chars().next_back();
            if !self.opened && last.is_some_and(|last| !last.is_whitespace()) {
                self.out.push(' ');
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::tools::web::fetch;
    use crate::utils::block_on;
    use encoding_rs::WINDOWS_1252;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::thread;
    use std::time::Duration;

    /// Serves `head` and `body` once and keeps the connection open when `endless`
    fn served(head: String, body: Vec<u8>, endless: bool) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let address = listener.local_addr().expect("address");
        thread::spawn(move || {
            let (mut connection, _) = listener.accept().expect("accept");
            let mut buffer = [0; 4096];
            let read = connection.read(&mut buffer).unwrap_or(0);
            assert!(read > 0);
            connection.write_all(head.as_bytes()).ok();
            connection.write_all(&body).ok();
            if endless {
                thread::sleep(Duration::from_secs(60));
            }
        });
        address
    }

    /// Returns the URL, the fetched page and the error
    fn fetched(kind: &str, body: Vec<u8>) -> (String, String, String) {
        let length = body.len();
        let head =
            format!("HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {length}\r\n\r\n");
        let address = served(head, body, false);
        let url = format!("http://{address}/docs/page.html");
        let fetching = fetch(&url);
        match block_on(fetching) {
            Ok(page) => (url, page, String::new()),
            Err(error) => (url, String::new(), error.to_string()),
        }
    }

    #[test]
    fn fetch_charset() {
        let (body, _, _) = WINDOWS_1252.encode("<p>café</p>");
        let meta = [br#"<meta charset="windows-1252">"#.as_slice(), &body].concat();
        assert_eq!(fetched("text/html", meta).1, "café");

        let (text, _, _) = WINDOWS_1252.encode("crème brûlée");
        let named = fetched("text/plain; Charset=ISO-8859-1", text.to_vec());
        assert_eq!(named.1, "crème brûlée");
    }
}
