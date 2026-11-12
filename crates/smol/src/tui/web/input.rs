use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use serde::Deserialize;

/// Columns or rows past any real screen, which a device can't make the shared one take
const MAX_SIDE: u16 = 1000;

pub(crate) enum Input {
    Size(u16, u16),
    /// Whether the user looks at the device
    Focus(bool),
    Events(Vec<Event>),
    /// Pasted or dropped on the page
    Image(Vec<u8>),
}

/// What app.js sends, where `key` is a `KeyboardEvent.key`
#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum Message {
    Size {
        w: u16,
        h: u16,
    },
    Focus {
        on: bool,
    },
    Key {
        key: String,
        #[serde(flatten)]
        held: Held,
    },
    /// Typed or composed text, which goes in as keys since the TUI folds a long paste
    Text {
        text: String,
    },
    Paste {
        text: String,
    },
    /// In base64
    Image {
        data: String,
    },
    Mouse {
        kind: Kind,
        #[serde(default)]
        button: u8,
        x: u16,
        y: u16,
        #[serde(flatten)]
        held: Held,
    },
}

#[derive(Deserialize)]
struct Held {
    #[serde(default)]
    ctrl: bool,
    #[serde(default)]
    alt: bool,
    #[serde(default)]
    shift: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    Down,
    Up,
    Drag,
    #[serde(rename = "scrollup")]
    ScrollUp,
    #[serde(rename = "scrolldown")]
    ScrollDown,
}

impl Held {
    fn modifiers(&self) -> KeyModifiers {
        let mut modifiers = KeyModifiers::empty();
        modifiers.set(KeyModifiers::CONTROL, self.ctrl);
        modifiers.set(KeyModifiers::ALT, self.alt);
        modifiers.set(KeyModifiers::SHIFT, self.shift);
        modifiers
    }
}

/// None for a message that isn't one, a key the TUI has no name for, a size out of range or an
/// image that isn't base64
pub(crate) fn decode(text: &str) -> Option<Input> {
    let message: Message = serde_json::from_str(text).ok()?;
    let events = match message {
        Message::Size { w, h } => {
            let sized = (1..=MAX_SIDE).contains(&w) && (1..=MAX_SIDE).contains(&h);
            return sized.then_some(Input::Size(w, h));
        }

        Message::Focus { on } => return Some(Input::Focus(on)),

        Message::Key { key, held } => {
            let code = code(&key, held.shift)?;
            let event = KeyEvent::new(code, held.modifiers());
            vec![Event::Key(event)]
        }

        Message::Text { text } => {
            let mut events = Vec::new();
            for c in text.chars().filter(|c| !c.is_control()) {
                let event = KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty());
                events.push(Event::Key(event));
            }
            events
        }

        Message::Paste { text } => vec![Event::Paste(text)],

        Message::Image { data } => {
            let data = STANDARD.decode(data).ok()?;
            return Some(Input::Image(data));
        }

        Message::Mouse {
            kind,
            button,
            x,
            y,
            held,
        } => {
            let button = match button {
                1 => MouseButton::Middle,
                2 => MouseButton::Right,
                _ => MouseButton::Left,
            };
            let kind = match kind {
                Kind::Down => MouseEventKind::Down(button),
                Kind::Up => MouseEventKind::Up(button),
                Kind::Drag => MouseEventKind::Drag(button),
                Kind::ScrollUp => MouseEventKind::ScrollUp,
                Kind::ScrollDown => MouseEventKind::ScrollDown,
            };
            let event = MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: held.modifiers(),
            };
            vec![Event::Mouse(event)]
        }
    };
    Some(Input::Events(events))
}

fn code(key: &str, shift: bool) -> Option<KeyCode> {
    let named = match key {
        "Enter" => KeyCode::Enter,
        "Backspace" => KeyCode::Backspace,
        "Tab" if shift => KeyCode::BackTab,
        "Tab" => KeyCode::Tab,
        "Escape" => KeyCode::Esc,
        "ArrowUp" => KeyCode::Up,
        "ArrowDown" => KeyCode::Down,
        "ArrowLeft" => KeyCode::Left,
        "ArrowRight" => KeyCode::Right,
        "Home" => KeyCode::Home,
        "End" => KeyCode::End,
        "PageUp" => KeyCode::PageUp,
        "PageDown" => KeyCode::PageDown,
        "Delete" => KeyCode::Delete,
        "Insert" => KeyCode::Insert,
        _ => {
            let mut chars = key.chars();
            let first = chars.next()?;
            if let Some(number) = key.strip_prefix('F').and_then(|n| n.parse().ok()) {
                return Some(KeyCode::F(number));
            }
            if chars.next().is_some() {
                return None;
            }
            KeyCode::Char(first)
        }
    };
    Some(named)
}

#[cfg(test)]
mod tests {
    use crate::tui::web::input::{decode, Input};
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEventKind,
    };

    fn events(text: &str) -> Vec<Event> {
        match decode(text) {
            Some(Input::Events(events)) => events,
            _ => panic!("{text}"),
        }
    }

    #[test]
    fn keys_decoded() {
        let key = |code, modifiers| Event::Key(KeyEvent::new(code, modifiers));
        let cases = [
            (
                r#"{"t":"key","key":"Enter"}"#,
                key(KeyCode::Enter, KeyModifiers::empty()),
            ),
            (
                r#"{"t":"key","key":"Enter","alt":true}"#,
                key(KeyCode::Enter, KeyModifiers::ALT),
            ),
            (
                r#"{"t":"key","key":"Tab","shift":true}"#,
                key(KeyCode::BackTab, KeyModifiers::SHIFT),
            ),
            (
                r#"{"t":"key","key":"d","ctrl":true}"#,
                key(KeyCode::Char('d'), KeyModifiers::CONTROL),
            ),
            (
                r#"{"t":"key","key":"é"}"#,
                key(KeyCode::Char('é'), KeyModifiers::empty()),
            ),
            (
                r#"{"t":"key","key":"F5"}"#,
                key(KeyCode::F(5), KeyModifiers::empty()),
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(events(text), [expected], "{text}");
        }

        assert!(decode(r#"{"t":"key","key":"Unidentified"}"#).is_none());
        assert!(decode(r#"{"t":"size","w":0,"h":24}"#).is_none());
        assert!(decode("café").is_none());

        let focus = decode(r#"{"t":"focus","on":false}"#);
        assert!(matches!(focus, Some(Input::Focus(false))));
        assert!(decode(r#"{"t":"focus"}"#).is_none());
    }

    #[test]
    fn text_typed() {
        let mut chars = Vec::new();
        for event in events(r#"{"t":"text","text":"naïve 🐍\n"}"#) {
            let Event::Key(KeyEvent {
                code: KeyCode::Char(c),
                ..
            }) = event
            else {
                panic!("{event:?}");
            };
            chars.push(c);
        }
        assert_eq!(chars, ['n', 'a', 'ï', 'v', 'e', ' ', '🐍']);
        let pasted = events(r#"{"t":"paste","text":"résumé\nline 2"}"#);
        assert_eq!(pasted, [Event::Paste("résumé\nline 2".to_owned())]);

        let image = decode(r#"{"t":"image","data":"iVBORw0KGgo="}"#);
        assert!(matches!(image, Some(Input::Image(data)) if data.starts_with(b"\x89PNG")));
        assert!(decode(r#"{"t":"image","data":"café"}"#).is_none());
    }

    #[test]
    fn mouse_decoded() {
        let [Event::Mouse(down)] =
            &events(r#"{"t":"mouse","kind":"down","button":0,"x":3,"y":7}"#)[..]
        else {
            panic!("down");
        };
        assert_eq!(down.kind, MouseEventKind::Down(MouseButton::Left));
        assert_eq!((down.column, down.row), (3, 7));
        let [Event::Mouse(scroll)] =
            &events(r#"{"t":"mouse","kind":"scrollup","x":0,"y":1,"ctrl":true}"#)[..]
        else {
            panic!("scroll");
        };
        assert_eq!(scroll.kind, MouseEventKind::ScrollUp);
        assert_eq!(scroll.modifiers, KeyModifiers::CONTROL);
    }
}
