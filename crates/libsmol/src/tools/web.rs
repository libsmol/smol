use crate::prompt;
use crate::tools::ToolError;
use crate::utils::{excerpt, http_cause, http_client};
use encoding_rs::{Encoding, UTF_8};
use html5ever::driver::{parse_fragment, ParseOpts};
use html5ever::tendril::TendrilSink;
use html5ever::tree_builder::TreeBuilderOpts;
use html5ever::{local_name, ns, QualName};
use reqwest::header::{CONTENT_TYPE, LOCATION};
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

pub async fn fetch(url: &str) -> Result<String, ToolError> {
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
        let next = location.and_then(|location| base.join(location).ok());
        let next = next.filter(|next| matches!(next.scheme(), "http" | "https"));
        if let Some(next) = next.filter(|_| moved) {
            hops += 1;
            target = next.into();
            continue;
        }

        let content_type = response.headers().get(CONTENT_TYPE);
        let content_type = content_type.and_then(|value| value.to_str().ok());
        let mut parts = content_type.unwrap_or_default().split(';');
        let mime = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        let label = parts.find_map(charset);
        let encoding = label.and_then(|label| Encoding::for_label(label.as_bytes()));
        let html = matches!(mime.as_str(), "" | "text/html" | "application/xhtml+xml");

        if !status.is_success() {
            let code = status.as_u16();
            // An error body may stream forever and only its start is read
            let mut body = Vec::new();
            while body.len() < MAX_EXCERPT_BYTES {
                let Ok(Some(chunk)) = response.chunk().await else {
                    break;
                };
                body.extend_from_slice(&chunk);
            }

            let decoder = encoding.unwrap_or(UTF_8);
            let (body, _, _) = decoder.decode(&body);
            let body = match html {
                true => {
                    let page = parsed(&body);
                    markdown(&page, &base)
                }
                false => body.into_owned(),
            };
            let said = excerpt(&body);

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

        let encoding = match encoding {
            Some(encoding) => encoding,
            None if html => declared(&bytes).unwrap_or(UTF_8),
            None => UTF_8,
        };
        let (text, _, _) = encoding.decode(&bytes);
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
            false => text.into_owned(),
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

/// Old sites send `Charset=gb2312`
fn charset(part: &str) -> Option<&str> {
    let (key, value) = part.split_once('=')?;
    let named = key.trim().eq_ignore_ascii_case("charset");
    let value = value.trim().trim_matches('"');
    named.then_some(value)
}

fn failed(url: &str, error: reqwest::Error) -> ToolError {
    // `http_cause` reports a timeout while connecting as a failure to connect
    let reason = match error.is_timeout() && !error.is_connect() {
        true => {
            let seconds = FETCH_WAIT.as_secs();
            prompt::timed_out(seconds)
        }
        false => http_cause(&error),
    };

    let message = prompt::failure("fetch", url, &reason);
    ToolError::Failed(message)
}

fn declared(bytes: &[u8]) -> Option<&'static Encoding> {
    let end = bytes.len().min(1024);
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

    let next = base.join(link).ok()?;
    let web = matches!(next.scheme(), "http" | "https");
    (web && next != *base).then_some(next)
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
    /// An opening mark was just written and no content has followed yet
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
            "blockquote" => ("\n> ".into(), String::new()),
            "td" | "th" => (" | ".into(), String::new()),
            "pre" => ("\n```\n".into(), "\n```\n".into()),
            "code" if !self.in_pre => ("`".into(), "`".into()),
            "ul" | "ol" => {
                let ordered = tag == "ol";
                let count = ordered.then_some(0);
                self.lists.push(count);
                (String::new(), "\n".into())
            }
            "li" => match self.lists.last_mut() {
                Some(Some(count)) => {
                    *count += 1;
                    (format!("\n{count}. "), String::new())
                }
                _ => ("\n- ".into(), String::new()),
            },
            "a" => {
                let href = el.attr("href").filter(|href| !href.starts_with('#'));
                let url = href.and_then(|href| self.base.join(href).ok());
                match url {
                    Some(url) if matches!(url.scheme(), "http" | "https") => {
                        ("[".into(), format!("]({url})"))
                    }
                    _ => Default::default(),
                }
            }
            _ => Default::default(),
        };

        if open.starts_with(char::is_whitespace) {
            let kept = self.out.trim_end_matches(' ').len();
            self.out.truncate(kept);
        }
        self.out.push_str(&open);
        self.opened |= !open.is_empty();
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

        if !close.is_empty() {
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
    use crate::tools::web::{fetch, FETCH_WAIT, MAX_REDIRECTS};
    use crate::utils::block_on;
    use encoding_rs::WINDOWS_1252;
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpListener};
    use std::thread;
    use std::time::{Duration, Instant};

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

    fn failure(url: &str) -> String {
        let fetching = fetch(url);
        let failed = block_on(fetching).expect_err("error");
        failed.to_string()
    }

    #[test]
    fn fetch_markdown() {
        let html = |body: &str| fetched("text/html", body.as_bytes().to_vec()).1;
        let page = "<h2>Title</h2><script>x()</script><ol><li>one</li><li>two</li></ol>\
            <p>Use <code>f</code></p>\n\n\n\n<pre>let x;</pre>";
        let expected = "## Title\n\n1. one\n2. two\n\nUse `f`\n\n```\nlet x;\n```";
        assert_eq!(html(page), expected);

        let links = r##"<a href="/wiki/Rust">Rust language</a> <a href="next.html">Next page</a> <a name="top">top</a> <a href="#s1">s1</a> <a href="javascript:void(0)">js</a>"##;
        let (url, shown, _) = fetched("text/html", links.as_bytes().to_vec());
        let root = url.trim_end_matches("/docs/page.html");
        let expected = format!(
            "[Rust language]({root}/wiki/Rust) [Next page]({root}/docs/next.html) top s1 js"
        );
        assert_eq!(shown, expected);

        let spaced = "<div>\n  <p>crème\n    brûlée </p>\n  \n  <p><a href=\"/x\">\n    link\n  </a> after</p>\n  <pre>  keep\n    this</pre>\n  <p><code>[ 1 ]</code></p>\n</div>";
        let (url, shown, _) = fetched("text/html", spaced.as_bytes().to_vec());
        let root = url.trim_end_matches("/docs/page.html");
        let expected = format!(
            "crème brûlée\n\n[link]({root}/x) after\n\n```\n  keep\n    this\n```\n\n`[ 1 ]`"
        );
        assert_eq!(shown, expected);

        let depth = 10_000;
        let opened = "<span>".repeat(depth);
        let closed = "</span>".repeat(depth);
        let deep = format!("<p>Start</p>{opened}deep down\nhere{closed}");
        assert_eq!(html(&deep), "Start\ndeep down here");

        let hidden = "<noscript><p>Please enable JavaScript</p></noscript>\
            <textarea style=\"color: red; DISPLAY : none\">&lt;style&gt;.a{}&lt;/style&gt;</textarea>\
            <div hidden>hidden</div><template><p>template</p></template><p style=\"color: red\">visible</p>";
        assert_eq!(html(hidden), "Please enable JavaScript\n\nvisible");
    }

    #[test]
    fn fetch_charset() {
        let (body, _, _) = WINDOWS_1252.encode("<p>café</p>");
        let declared = [br#"<meta charset="windows-1252">"#.as_slice(), &body].concat();
        assert_eq!(fetched("text/html", declared).1, "café");
        let old = br#"<meta http-equiv="Content-Type" content="text/html; charset=ISO-8859-1">"#;
        let old = [old.as_slice(), &body].concat();
        assert_eq!(fetched("text/html", old).1, "café");

        let (text, _, _) = WINDOWS_1252.encode("crème brûlée");
        let named = fetched("text/plain; Charset=ISO-8859-1", text.to_vec());
        assert_eq!(named.1, "crème brûlée");
        let wide = "<meta charset='utf-16'><p>café</p>".as_bytes().to_vec();
        assert_eq!(fetched("text/html", wide).1, "café");
    }

    #[test]
    fn fetch_errors() {
        let body = "{\"error\":\n  \"requests too frequent\"}";
        let length = body.len();
        let head = format!("HTTP/1.1 429 Too Many Requests\r\ncontent-length: {length}\r\n\r\n");
        let address = served(head, body.into(), false);
        let url = format!("http://{address}/résumé");
        let message = failure(&url);
        let expected = format!("fetch: {url}: http 429: {{\"error\": \"requests too frequent\"}}");
        assert_eq!(message, expected);

        let head = "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 100000000\r\n\r\n";
        let body = "server error — ".repeat(10_000);
        let address = served(head.to_owned(), body.into_bytes(), true);
        let url = format!("http://{address}/");
        let started = Instant::now();
        let message = failure(&url);
        let elapsed = started.elapsed();
        let start = format!("fetch: {url}: http 500: server error — server error —");
        assert!(message.starts_with(&start), "{message}");
        assert!(message.ends_with('…'), "{message}");
        // Reading on to the end would only stop at the read timeout
        assert!(elapsed < FETCH_WAIT / 4, "{elapsed:?}");

        // The error names the host redirected to
        let head =
            "HTTP/1.1 302 Found\r\nlocation: http://127.0.0.1:1/\r\ncontent-length: 0\r\n\r\n";
        let address = served(head.to_owned(), Vec::new(), false);
        let url = format!("http://{address}/old-café");
        let message = failure(&url);
        let expected = format!("fetch: {url}: cannot connect to 127.0.0.1:1: ");
        assert!(message.starts_with(&expected), "{message}");

        let style = format!("<style>{}</style>", ".a{color:red}".repeat(500));
        let body = format!("<html><head>{style}</head><body><h1>Access denied</h1></body></html>");
        let length = body.len();
        let head = format!(
            "HTTP/1.1 403 Forbidden\r\ncontent-type: text/html\r\ncontent-length: {length}\r\n\r\n"
        );
        let address = served(head, body.into_bytes(), false);
        let url = format!("http://{address}/");
        let message = failure(&url);
        let expected = format!("fetch: {url}: http 403: # Access denied");
        assert_eq!(message, expected);
    }

    #[test]
    fn fetch_sniffed() {
        let text = "# Café menu\n\nCrème brûlée".as_bytes().to_vec();
        let (_, shown, _) = fetched("application/octet-stream", text);
        assert_eq!(shown, "# Café menu\n\nCrème brûlée");

        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.resize(16 * 1024, 0);
        let (url, _, failed) = fetched("application/octet-stream", png);
        let expected = format!(
            "fetch: {url}: application/octet-stream, only HTML and text pages can be fetched"
        );
        assert_eq!(failed, expected);
    }

    #[test]
    fn fetch_refresh() {
        let page = |body: &str| {
            let length = body.len();
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {length}\r\n\r\n"
            );
            let address = served(head, body.as_bytes().to_vec(), false);
            format!("http://{address}/")
        };

        let full = page("<p>full page</p>");
        let script = format!("<script>location.replace('{full}')</script><noscript><meta http-equiv=\"refresh\" content=\"0;url={full}\"></noscript>");
        assert_eq!(fetched("text/html", script.into_bytes()).1, "full page");
        let quoted = page("<p>quoted</p>");
        let quoted = format!("<meta http-equiv=\"Refresh\" content=\" 0.5 , URL = '{quoted}' \">");
        assert_eq!(fetched("text/html", quoted.into_bytes()).1, "quoted");

        // Following it would fail to connect to port 1
        let later =
            "<meta http-equiv=\"refresh\" content=\"5;url=http://127.0.0.1:1/\"><p>redirecting in 5s</p>";
        assert_eq!(fetched("text/html", later.into()).1, "redirecting in 5s");
        let itself =
            "<meta http-equiv=\"refresh\" content=\"0; url=page.html\"><p>refreshes itself</p>";
        assert_eq!(fetched("text/html", itself.into()).1, "refreshes itself");

        let chained = |end: String, hops: usize| {
            let mut next = end;
            for _ in 0..hops {
                let body = format!("<meta http-equiv=\"refresh\" content=\"0;url={next}\">");
                next = page(&body);
            }
            next
        };
        let arrived = page("<p>arrived</p>");
        let first = chained(arrived, MAX_REDIRECTS);
        assert_eq!(block_on(fetch(&first)).expect("fetch"), "arrived");

        let refused = "http://127.0.0.1:1/".to_owned();
        let first = chained(refused, MAX_REDIRECTS + 1);
        let message = failure(&first);
        let expected = format!("fetch: {first}: too many redirects");
        assert_eq!(message, expected);
    }
}
