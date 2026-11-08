use crate::tui::picture::Picture;
use crate::tui::web::input::decode;
use crate::tui::web::{Arrival, Who};
use httparse::{Request, Status, EMPTY_HEADER};
use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::str;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tungstenite::handshake::derive_accept_key;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, Role, WebSocketConfig};
use tungstenite::{Error, Message, WebSocket};

const INDEX: &str = include_str!("page/index.html");
const SCRIPT: &str = include_str!("page/app.js");
const STYLE: &str = include_str!("page/style.css");

/// Cookies don't keep to a port, so the port goes in the name to keep two smol webs apart
const COOKIE: &str = "smol";
/// In seconds, the most Chrome keeps a cookie for
const COOKIE_MAX_AGE: u32 = 400 * 24 * 60 * 60;
const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;
/// For a request's head as a whole, and for each frame to go out before the browser counts as gone
const REQUEST_WAIT: Duration = Duration::from_secs(10);
/// Accepting fails again at once while no file descriptor is free
const ACCEPT_WAIT: Duration = Duration::from_millis(100);
/// How long a socket's read blocks before it looks whether the sending thread ended, as shutting
/// the socket doesn't wake a read on Windows
const READ_WAIT: Duration = Duration::from_secs(1);
/// Frames a browser can fall behind by, after which it gets the whole screen instead
const MAX_QUEUED: usize = 64;
const MAX_OPENED: usize = 16;

/// Closing codes that app.js tells apart
pub(crate) const LEFT: u16 = 4000;
pub(crate) const ENDED: u16 = 4001;

const NO_ENTRY: &str = "<!doctype html>\n<meta charset=\"utf-8\">\n<title>smol</title>\n<p>Open the address <code>smol web</code> printed, with its token.</p>\n";
/// The page loads only its own script and style, and no other page may frame it
const PAGE_POLICY: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; connect-src 'self' ws: wss:; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
/// Diagrams and pictures the user opens run no script
const OPENED_POLICY: &str = "sandbox; default-src 'none'; img-src data:; style-src 'unsafe-inline'";

/// What the server hands out besides the page, which the TUI's thread puts here
#[derive(Default)]
pub(crate) struct Served {
    pub(crate) images: HashMap<u32, Arc<Picture>>,
    opened: VecDeque<(u64, Arc<[u8]>, &'static str)>,
    last: u64,
}

impl Served {
    /// Returns the number its address ends in
    pub(crate) fn open(&mut self, bytes: Vec<u8>, kind: &'static str) -> u64 {
        self.last += 1;
        if self.opened.len() == MAX_OPENED {
            self.opened.pop_front();
        }
        self.opened.push_back((self.last, Arc::from(bytes), kind));
        self.last
    }
}

struct Server {
    token: String,
    cookie: String,
    served: Arc<Mutex<Served>>,
    arrivals: Sender<Arrival>,
    next_id: AtomicU64,
}

/// The socket as the reading thread holds it, where tungstenite's own replies to pings and
/// closes are dropped, since bytes from two threads would mix in the stream
struct Reading(TcpStream);

/// Starts answering on `address`, and returns the address it got
pub(crate) fn listen(
    address: SocketAddr,
    token: String,
    served: Arc<Mutex<Served>>,
    arrivals: Sender<Arrival>,
) -> io::Result<SocketAddr> {
    let listener = TcpListener::bind(address)?;
    let bound = listener.local_addr()?;
    let cookie = format!("{COOKIE}-{}", bound.port());
    let server = Arc::new(Server {
        token,
        cookie,
        served,
        arrivals,
        next_id: AtomicU64::new(1),
    });

    thread::spawn(move || loop {
        let Ok((stream, _)) = listener.accept() else {
            thread::sleep(ACCEPT_WAIT);
            continue;
        };
        // A connection the system has no thread for goes unanswered
        let server = Arc::clone(&server);
        thread::Builder::new()
            .spawn(move || server.answer(stream))
            .ok();
    });
    Ok(bound)
}

impl Server {
    fn answer(&self, mut stream: TcpStream) {
        stream.set_read_timeout(Some(REQUEST_WAIT)).ok();
        stream.set_write_timeout(Some(REQUEST_WAIT)).ok();

        // A client sending a byte at a time keeps each read within its timeout
        let started = Instant::now();
        let mut head = Vec::new();
        let mut chunk = [0; 4096];
        let rest = loop {
            let Ok(read) = stream.read(&mut chunk) else {
                return;
            };
            if read == 0 || started.elapsed() > REQUEST_WAIT {
                return;
            }
            head.extend_from_slice(&chunk[..read]);

            if let Some(end) = head.windows(4).position(|four| four == b"\r\n\r\n") {
                break head.split_off(end + 4);
            }
            if head.len() > MAX_HEAD_BYTES {
                return;
            }
        };

        let mut headers = [EMPTY_HEADER; 64];
        let mut request = Request::new(&mut headers);
        let Ok(Status::Complete(_)) = request.parse(&head) else {
            reply(
                &mut stream,
                "400 Bad Request",
                "text/plain",
                b"bad request",
                "",
            );
            return;
        };
        if request.method != Some("GET") {
            reply(
                &mut stream,
                "405 Method Not Allowed",
                "text/plain",
                b"GET only",
                "",
            );
            return;
        }
        let header = |name: &str| {
            let found = request
                .headers
                .iter()
                .find(|h| h.name.eq_ignore_ascii_case(name));
            found.and_then(|h| str::from_utf8(h.value).ok())
        };

        let target = request.path.unwrap_or("/");
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        let given = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("token="));
        let entering = given.filter(|given| path == "/" && same(given, &self.token));
        if let Some(given) = entering {
            // The page goes on without the token in its address, which would end up in history
            let name = &self.cookie;
            let cookie = format!(
                "Set-Cookie: {name}={given}; Path=/; Max-Age={COOKIE_MAX_AGE}; HttpOnly; SameSite=Strict\r\nLocation: ./\r\n"
            );
            reply(&mut stream, "303 See Other", "text/plain", b"", &cookie);
            return;
        }

        let cookies = header("cookie").unwrap_or_default();
        let named = format!("{}=", self.cookie);
        let mut pairs = cookies.split(';').map(str::trim);
        let presented = pairs.find_map(|pair| pair.strip_prefix(named.as_str()));
        if !presented.is_some_and(|presented| same(presented, &self.token)) {
            let page = NO_ENTRY.as_bytes();
            reply(&mut stream, "403 Forbidden", "text/html", page, "");
            return;
        }

        match path {
            "/" => {
                let policy = format!("Content-Security-Policy: {PAGE_POLICY}\r\n");
                let page = INDEX.as_bytes();
                reply(
                    &mut stream,
                    "200 OK",
                    "text/html; charset=utf-8",
                    page,
                    &policy,
                );
            }
            "/app.js" => reply(
                &mut stream,
                "200 OK",
                "text/javascript",
                SCRIPT.as_bytes(),
                "",
            ),
            "/style.css" => reply(&mut stream, "200 OK", "text/css", STYLE.as_bytes(), ""),

            "/ws" => {
                // SameSite still sends the cookie from a page of this site on another port
                let host = header("x-forwarded-host").or(header("host"));
                let origin = header("origin").and_then(|origin| origin.split_once("://"));
                if origin.is_some_and(|(_, origin)| Some(origin) != host) {
                    reply(
                        &mut stream,
                        "403 Forbidden",
                        "text/plain",
                        b"another site",
                        "",
                    );
                    return;
                }
                let Some(key) = header("sec-websocket-key") else {
                    reply(
                        &mut stream,
                        "400 Bad Request",
                        "text/plain",
                        b"websocket only",
                        "",
                    );
                    return;
                };
                let accept = derive_accept_key(key.as_bytes());
                let switched = format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n");
                if stream.write_all(switched.as_bytes()).is_ok() {
                    self.converse(stream, rest);
                }
            }

            _ => {
                let image: Option<u32> =
                    path.strip_prefix("/image/").and_then(|id| id.parse().ok());
                let number: Option<u64> = path.strip_prefix("/open/").and_then(|n| n.parse().ok());

                // Taken out under the lock, which the TUI's thread needs for every frame
                let served = self.served.lock().expect("served");
                let picture = image.and_then(|id| served.images.get(&id).cloned());
                let opened = served.opened.iter().find(|(n, ..)| Some(*n) == number);
                let opened = opened.cloned();
                drop(served);

                let policy = format!("Content-Security-Policy: {OPENED_POLICY}\r\n");
                let png = picture.as_ref().and_then(|picture| picture.png());
                match (png, opened) {
                    (Some(png), _) => reply(&mut stream, "200 OK", "image/png", png, &policy),
                    (_, Some((_, bytes, kind))) => {
                        reply(&mut stream, "200 OK", kind, &bytes, &policy)
                    }
                    _ => reply(&mut stream, "404 Not Found", "text/plain", b"not found", ""),
                }
            }
        }
    }

    /// Passes the browser's messages on while another thread sends it what was queued for it,
    /// and the side that ends first ends the other
    fn converse(&self, stream: TcpStream, rest: Vec<u8>) {
        let Ok(writing) = stream.try_clone() else {
            return;
        };
        let (outbox, outgoing) = mpsc::sync_channel(MAX_QUEUED);
        let parting = Arc::new(AtomicU16::new(ENDED));
        let finished = Arc::new(AtomicBool::new(false));
        let closing = Arc::clone(&parting);
        let ending = Arc::clone(&finished);
        let sending =
            thread::Builder::new().spawn(move || send_queued(writing, outgoing, closing, ending));
        if sending.is_err() {
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        if self
            .arrivals
            .send(Arrival::Joined(id, outbox, parting))
            .is_err()
        {
            return;
        }

        stream.set_read_timeout(Some(READ_WAIT)).ok();
        let config = WebSocketConfig::default().max_message_size(Some(MAX_MESSAGE_BYTES));
        let reading = Reading(stream);
        let mut socket = WebSocket::from_partially_read(reading, rest, Role::Server, Some(config));
        let who = Who::Browser(id);
        loop {
            match socket.read() {
                Ok(Message::Text(text)) => {
                    let Some(input) = decode(&text) else {
                        continue;
                    };
                    if self.arrivals.send(Arrival::Input(who, input)).is_err() {
                        break;
                    }
                }
                Ok(Message::Close(_)) => break,
                Ok(_) => {}
                Err(Error::Io(e))
                    if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    if finished.load(Ordering::Relaxed) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }

        let Reading(stream) = socket.get_ref();
        stream.shutdown(Shutdown::Both).ok();
        self.arrivals.send(Arrival::Left(id)).ok();
    }
}

impl Read for Reading {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for Reading {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Sends each frame as soon as it is queued, and once the host drops the outbox closes with the
/// code it left in `parting`, unless the browser is gone
fn send_queued(
    stream: TcpStream,
    outgoing: Receiver<String>,
    parting: Arc<AtomicU16>,
    finished: Arc<AtomicBool>,
) {
    let mut socket = WebSocket::from_raw_socket(stream, Role::Server, None);
    let mut sent = |text: String| socket.send(Message::text(text)).is_ok();
    if outgoing.iter().all(&mut sent) {
        let code = parting.load(Ordering::Relaxed);
        let frame = CloseFrame {
            code: CloseCode::from(code),
            reason: "".into(),
        };
        socket.close(Some(frame)).ok();
        socket.flush().ok();
    }
    finished.store(true, Ordering::Relaxed);
    socket.get_ref().shutdown(Shutdown::Both).ok();
}

/// `extra` is whole header lines, each ending in CRLF
fn reply(stream: &mut TcpStream, status: &str, kind: &str, body: &[u8], extra: &str) {
    let length = body.len();
    let head = format!("HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {length}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n{extra}\r\n");
    let written = stream.write_all(head.as_bytes());
    if written.is_ok() {
        stream.write_all(body).ok();
    }
}

/// Takes as long for any token of the right length, so timing tells nothing of how much matched
fn same(given: &str, token: &str) -> bool {
    let (given, token) = (given.as_bytes(), token.as_bytes());
    if given.len() != token.len() {
        return false;
    }
    let differs = given.iter().zip(token).fold(0, |all, (a, b)| all | (a ^ b));
    differs == 0
}

#[cfg(test)]
mod tests {
    use crate::tui::web::input::Input;
    use crate::tui::web::serve::{listen, Served, LEFT};
    use crate::tui::web::{Arrival, Who};
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::sync::atomic::Ordering;
    use std::sync::mpsc::{self, Receiver};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tungstenite::client::IntoClientRequest;
    use tungstenite::http::HeaderValue;
    use tungstenite::protocol::frame::coding::CloseCode;
    use tungstenite::Message;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn started() -> (SocketAddr, Arc<Mutex<Served>>, Receiver<Arrival>) {
        let served = Arc::default();
        let (arrive, arrivals) = mpsc::channel();
        let address = "127.0.0.1:0".parse().expect("address");
        let bound = listen(address, TOKEN.to_owned(), Arc::clone(&served), arrive);
        (bound.expect("listen"), served, arrivals)
    }

    /// The whole response to a GET with these extra header lines
    fn get(address: SocketAddr, path: &str, headers: &str) -> String {
        let mut stream = TcpStream::connect(address).expect("connect");
        let request = format!("GET {path} HTTP/1.1\r\nHost: {address}\r\n{headers}\r\n");
        stream.write_all(request.as_bytes()).expect("write");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read");
        response
    }

    #[test]
    fn entry_guarded() {
        let (address, served, _arrivals) = started();
        let name = format!("smol-{}", address.port());
        let cookie = format!("Cookie: theme=dark; {name}={TOKEN}\r\n");

        assert!(get(address, "/", "").starts_with("HTTP/1.1 403"));
        let wrong = get(address, "/?token=0123", "");
        assert!(wrong.starts_with("HTTP/1.1 403"), "{wrong}");
        let entered = get(address, &format!("/?token={TOKEN}"), "");
        assert!(entered.starts_with("HTTP/1.1 303"), "{entered}");
        let set = format!(
            "Set-Cookie: {name}={TOKEN}; Path=/; Max-Age=34560000; HttpOnly; SameSite=Strict\r\n"
        );
        assert!(
            entered.contains(&set) && entered.contains("Location: ./\r\n"),
            "{entered}"
        );

        // Another smol web's cookie, on another port of the same host
        let elsewhere = format!("Cookie: smol-1={TOKEN}\r\n");
        assert!(get(address, "/", &elsewhere).starts_with("HTTP/1.1 403"));
        let page = get(address, "/", &cookie);
        assert!(
            page.starts_with("HTTP/1.1 200") && page.contains("frame-ancestors 'none'"),
            "{page}"
        );
        let foreign = format!("{cookie}Origin: http://café.example\r\nSec-WebSocket-Key: a2V5\r\n");
        assert!(get(address, "/ws", &foreign).starts_with("HTTP/1.1 403"));

        assert!(get(address, "/image/1", &cookie).starts_with("HTTP/1.1 404"));
        let page = "<p>naïve</p>".as_bytes().to_vec();
        let number = served.lock().expect("served").open(page, "text/html");
        let opened = get(address, &format!("/open/{number}"), &cookie);
        assert!(
            opened.contains("sandbox") && opened.ends_with("<p>naïve</p>"),
            "{opened}"
        );
    }

    #[test]
    fn socket_relayed() {
        let (address, _served, arrivals) = started();
        let wait = Duration::from_secs(5);
        let mut request = format!("ws://{address}/ws")
            .into_client_request()
            .expect("request");
        let port = address.port();
        let cookie = HeaderValue::from_str(&format!("smol-{port}={TOKEN}")).expect("cookie");
        request.headers_mut().insert("cookie", cookie);
        let stream = TcpStream::connect(address).expect("connect");
        let (mut socket, _) = tungstenite::client(request, stream).expect("handshake");

        let Ok(Arrival::Joined(id, outbox, parting)) = arrivals.recv_timeout(wait) else {
            panic!("joined");
        };
        // Only the sending thread writes, so the ping read before the size goes unanswered
        socket.send(Message::Ping("naïve".into())).expect("ping");
        socket
            .send(Message::text(r#"{"t":"size","w":90,"h":30}"#))
            .expect("send");
        let Ok(Arrival::Input(who, Input::Size(90, 30))) = arrivals.recv_timeout(wait) else {
            panic!("size");
        };
        assert_eq!(who, Who::Browser(id));

        outbox.send("café".to_owned()).expect("queue");
        assert_eq!(socket.read().expect("read"), Message::text("café"));
        parting.store(LEFT, Ordering::Relaxed);
        drop(outbox);
        let Message::Close(Some(frame)) = socket.read().expect("close") else {
            panic!("close");
        };
        assert_eq!(frame.code, CloseCode::from(LEFT));
        assert!(matches!(arrivals.recv_timeout(wait), Ok(Arrival::Left(left)) if left == id));
    }
}
