// The server lays out every cell, so nothing here measures or wraps text

const theme = getComputedStyle(document.documentElement);
const BG = theme.getPropertyValue("--bg").trim();
const FG = theme.getPropertyValue("--fg").trim();
const FONT = theme.getPropertyValue("--font").trim();
// The closing codes serve.rs sends
const LEFT = 4000;
const ENDED = 4001;
// Modifier bits of ratatui
const BOLD = 1;
const DIM = 2;
const ITALIC = 4;
const UNDERLINED = 8;
const REVERSED = 64;
const HIDDEN = 128;
const CROSSED_OUT = 256;
const BLANK = [null, null, 0];
// app.rs scrolls this many rows for each scroll event, so each takes as many rows of movement
const SCROLL_ROWS = 3;
const RETRY_WAIT = 1000;
const FIT_WAIT = 50;
// How long a finger rests before it selects instead of scrolling
const HOLD_WAIT = 400;
// A finger that rests this long before it lifts flings nothing
const STILL_WAIT = 50;
// The share of a fling's speed left after each millisecond, as iOS slows one down
const FLING_KEEP = 0.998;
// In pixels per millisecond
const MIN_FLING_SPEED = 0.05;
// How long a release waits for the copy it may cause
const COPY_WAIT = 1000;
const NAMED_KEYS = new Set([
  "Enter", "Backspace", "Tab", "Escape", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight",
  "Home", "End", "PageUp", "PageDown", "Delete", "Insert",
]);
// Option types characters on a Mac, so it is not Alt there
const MAC = /Mac|iPhone|iPad/.test(navigator.userAgent);
// Keeps Backspace firing in an empty field on phone keyboards
const SENTINEL = " ";
// With ctrl or command, the browser's find, save, print, bookmark and open, which act on the page
const PAGE_KEYS = new Set(["f", "g", "s", "p", "d", "o"]);

const view = document.getElementById("view");
const canvas = document.getElementById("screen");
const keys = document.getElementById("keys");
const bar = document.getElementById("bar");
const ctrlButton = document.getElementById("ctrl");
const pasteButton = document.getElementById("paste");
const notice = document.getElementById("notice");
const copied = document.getElementById("copied");
const pasting = document.getElementById("pasting");
const paint = canvas.getContext("2d");

let cell = { width: 9, height: 18, size: 15 };
let screen = blank(0, 0);
let cursor = null;
// The input box as x, y, width and height in cells
let input = [0, 0, 0, 0];
const tiles = new Map();
const images = new Map();
let socket = null;
let committed = null;
// The bar's ctrl, held for the next key
let latched = false;
// A composition that the latched ctrl took its letter from, whose text is dropped
let chorded = false;
// The clipboard write of the last release, which a copy fills
let pending = null;
// A tap on the input box that goes on as the browser's own click
let clicking = false;

function blank(width, height) {
  const rows = [];
  for (let y = 0; y < height; y++) {
    const row = Array.from({ length: width }, () => ({ symbol: " ", style: BLANK }));
    rows.push(row);
  }
  return { width, height, rows };
}

function slot(x, y) {
  return y * screen.width + x;
}

function send(message) {
  if (socket && socket.readyState === WebSocket.OPEN) {
    socket.send(JSON.stringify(message));
  }
}

function show(text) {
  notice.hidden = text === null;
  notice.textContent = text ?? "";
}

// iOS leaves the page under its keyboard and pans what shows of it
function anchor() {
  const visible = window.visualViewport;
  if (visible) {
    view.style.top = `${visible.offsetTop}px`;
    view.style.height = `${visible.height}px`;
  }
}

// The device pixel grid keeps cell edges from blurring into each other
function measure() {
  const ratio = window.devicePixelRatio || 1;
  const width = canvas.clientWidth;
  const height = canvas.clientHeight;
  // A phone keeps the small size on its side too
  const shorter = Math.min(window.screen.width, window.screen.height);
  const size = width < 600 || shorter < 600 ? 12 : 15;
  paint.font = `${size}px ${FONT}`;
  const glyph = paint.measureText("M").width;
  cell = {
    width: Math.round(glyph * ratio) / ratio,
    height: Math.round(size * 1.3 * ratio) / ratio,
    size,
  };

  canvas.width = Math.round(width * ratio);
  canvas.height = Math.round(height * ratio);
  paint.setTransform(ratio, 0, 0, ratio, 0, 0);

  const columns = Math.max(1, Math.floor(width / cell.width));
  const rows = Math.max(1, Math.floor(height / cell.height));
  return { columns, rows };
}

function fit() {
  const { columns, rows } = measure();
  send({ t: "size", w: columns, h: rows });
  repaint();
}

function frame(message) {
  const resized = message.w !== screen.width || message.h !== screen.height;
  const fresh = message.clear || resized;
  if (fresh) {
    screen = blank(message.w, message.h);
    tiles.clear();
  }

  const touched = new Set();
  for (const [x, y, style, symbols] of message.runs) {
    for (const [offset, symbol] of symbols.entries()) {
      const at = x + offset;
      if (y >= screen.height || at >= screen.width) {
        continue;
      }
      screen.rows[y][at] = { symbol, style: message.styles[style] };
      tiles.delete(slot(at, y));
      touched.add(slot(at, y));
    }
  }
  for (const [x, y, image, columns, rows, column, row] of message.tiles) {
    tiles.set(slot(x, y), { image, columns, rows, column, row });
    load(image);
  }
  // A whole screen names every picture still shown
  if (fresh) {
    const shown = new Set();
    for (const tile of tiles.values()) {
      shown.add(tile.image);
    }
    for (const id of images.keys()) {
      if (!shown.has(id)) {
        images.delete(id);
      }
    }
  }

  if (cursor && !fresh) {
    touched.add(slot(...cursor));
  }
  const [cursorX, cursorY] = message.cursor ?? [];
  const inside = message.cursor && cursorX < screen.width && cursorY < screen.height;
  cursor = inside ? message.cursor : null;
  input = message.input;
  if (cursor) {
    touched.add(slot(cursorX, cursorY));
    // The input method's window opens next to the cursor
    keys.style.left = `${cursorX * cell.width}px`;
    keys.style.top = `${cursorY * cell.height}px`;
  }

  if (fresh) {
    repaint();
  } else {
    redraw(touched);
  }
}

function load(id) {
  if (images.has(id)) {
    return;
  }
  const image = new Image();
  image.onload = () => {
    for (const [spot, tile] of tiles) {
      if (tile.image === id) {
        redraw(new Set([spot]));
      }
    }
  };
  // A picture that failed, as during a reconnect, loads again once a frame shows it
  image.onerror = () => images.delete(id);
  image.src = `image/${id}`;
  images.set(id, image);
}

function repaint() {
  paint.fillStyle = BG;
  paint.fillRect(0, 0, canvas.clientWidth, canvas.clientHeight);
  for (let y = 0; y < screen.height; y++) {
    for (let x = 0; x < screen.width; x++) {
      draw(x, y);
    }
  }
}

// A cell covered by a wide symbol is drawn with the symbol, which is drawn whole
function redraw(touched) {
  const owners = new Set();
  for (const spot of touched) {
    let x = spot % screen.width;
    const y = Math.floor(spot / screen.width);
    while (x > 0 && screen.rows[y][x].symbol === null) {
      x--;
    }
    owners.add(slot(x, y));
  }
  for (const spot of [...owners].sort((a, b) => a - b)) {
    const x = spot % screen.width;
    const y = Math.floor(spot / screen.width);
    draw(x, y);
  }
}

function draw(x, y) {
  const here = screen.rows[y][x];
  if (here.symbol === null) {
    return;
  }
  let span = 1;
  while (x + span < screen.width && screen.rows[y][x + span].symbol === null) {
    span++;
  }

  const [foreground, background, modifiers] = here.style;
  let fg = foreground || FG;
  let bg = background || BG;
  if (modifiers & REVERSED) {
    [fg, bg] = [bg, fg];
  }
  const onCursor = cursor && cursor[0] === x && cursor[1] === y;
  if (onCursor) {
    [fg, bg] = [BG, FG];
  }

  const left = x * cell.width;
  const top = y * cell.height;
  const width = span * cell.width;
  paint.fillStyle = bg;
  paint.fillRect(left, top, width, cell.height);

  const tile = tiles.get(slot(x, y));
  if (tile) {
    const image = images.get(tile.image);
    if (image && image.complete && image.naturalWidth > 0) {
      const part = { width: image.naturalWidth / tile.columns, height: image.naturalHeight / tile.rows };
      const from = { x: tile.column * part.width, y: tile.row * part.height };
      paint.drawImage(image, from.x, from.y, part.width, part.height, left, top, cell.width, cell.height);
    }
    return;
  }
  if (modifiers & HIDDEN) {
    return;
  }

  paint.globalAlpha = modifiers & DIM ? 0.6 : 1;
  paint.fillStyle = fg;
  paint.strokeStyle = fg;
  // A space shows only the lines under and through it, which run on across words
  if (here.symbol !== " " && !drawShape(here.symbol, left, top, width)) {
    const weight = modifiers & BOLD ? "bold " : "";
    const slant = modifiers & ITALIC ? "italic " : "";
    paint.font = `${slant}${weight}${cell.size}px ${FONT}`;
    paint.textBaseline = "middle";
    const measured = paint.measureText(here.symbol).width;
    const middle = top + cell.height / 2;

    // Ink past the cell, as fallback fonts draw some symbols, would outlast the next redraw
    paint.save();
    paint.beginPath();
    paint.rect(left, top, width, cell.height);
    paint.clip();
    if (measured > width) {
      paint.translate(left, middle);
      paint.scale(width / measured, 1);
      paint.fillText(here.symbol, 0, 0);
    } else {
      paint.fillText(here.symbol, left + (width - measured) / 2, middle);
    }
    paint.restore();
  }

  if (modifiers & UNDERLINED) {
    paint.fillRect(left, top + cell.height - 2, width, 1);
  }
  if (modifiers & CROSSED_OUT) {
    paint.fillRect(left, top + cell.height / 2, width, 1);
  }
  paint.globalAlpha = 1;
}

// Light and heavy lines from the middle to each side: up, right, down, left
const LINES = {
  "─": [0, 1, 0, 1], "━": [0, 2, 0, 2], "│": [1, 0, 1, 0], "┃": [2, 0, 2, 0],
  "┌": [0, 1, 1, 0], "┐": [0, 0, 1, 1], "└": [1, 1, 0, 0], "┘": [1, 0, 0, 1],
  "├": [1, 1, 1, 0], "┤": [1, 0, 1, 1], "┬": [0, 1, 1, 1], "┴": [1, 1, 0, 1], "┼": [1, 1, 1, 1],
  "╴": [0, 0, 0, 1], "╵": [1, 0, 0, 0], "╶": [0, 1, 0, 0], "╷": [0, 0, 1, 0],
};
// Whether a rounded corner reaches right rather than left, and down rather than up
const CORNERS = { "╭": [true, true], "╮": [false, true], "╰": [true, false], "╯": [false, false] };

// Box lines and blocks drawn by hand meet their neighbors without the font's gaps
function drawShape(symbol, left, top, width) {
  const height = cell.height;
  const code = symbol.codePointAt(0);
  if (symbol.length === 1 && code >= 0x2580 && code <= 0x2595) {
    if (code === 0x2580) {
      paint.fillRect(left, top, width, height / 2);
    } else if (code >= 0x2581 && code <= 0x2588) {
      const part = (code - 0x2580) / 8;
      paint.fillRect(left, top + height * (1 - part), width, height * part);
    } else if (code >= 0x2589 && code <= 0x258f) {
      paint.fillRect(left, top, width * (0x2590 - code) / 8, height);
    } else if (code === 0x2590) {
      paint.fillRect(left + width / 2, top, width / 2, height);
    } else if (code === 0x2594) {
      paint.fillRect(left, top, width, height / 8);
    } else if (code === 0x2595) {
      paint.fillRect(left + width * 7 / 8, top, width / 8, height);
    } else {
      return false;
    }
    return true;
  }

  const middleX = Math.floor(left + width / 2) + 0.5;
  const middleY = Math.floor(top + height / 2) + 0.5;
  const lines = LINES[symbol];
  if (lines) {
    const ends = [[middleX, top], [left + width, middleY], [middleX, top + height], [left, middleY]];
    // Sides stroked apart would each half cover the pixel where they meet and leave a seam
    for (const weight of [1, 2]) {
      paint.lineWidth = weight;
      paint.beginPath();
      for (const [side, drawn] of lines.entries()) {
        if (drawn === weight) {
          paint.moveTo(middleX, middleY);
          paint.lineTo(...ends[side]);
        }
      }
      paint.stroke();
    }
    return true;
  }

  const corner = CORNERS[symbol];
  if (corner) {
    const [right, down] = corner;
    const radius = Math.min(width, height) / 2;
    const toX = right ? left + width : left;
    const toY = down ? top + height : top;
    paint.lineWidth = 1;
    paint.beginPath();
    paint.moveTo(middleX, toY);
    paint.arcTo(middleX, middleY, toX, middleY, radius);
    paint.lineTo(toX, middleY);
    paint.stroke();
    return true;
  }
  return false;
}

function offer(text) {
  const area = copied.querySelector("textarea");
  area.value = text;
  copied.showModal();
  area.select();
}

function copy(text) {
  if (pending) {
    pending.fill(text);
    pending = null;
  } else if (navigator.clipboard && window.isSecureContext) {
    navigator.clipboard.writeText(text).catch(() => offer(text));
  } else {
    offer(text);
  }
}

// Safari lets only the gesture itself write the clipboard, and the copy comes after it
function expectCopy() {
  if (!window.isSecureContext || !window.ClipboardItem) {
    return;
  }
  const waiting = { text: null };
  const blob = new Promise((resolve, reject) => {
    waiting.fill = (text) => {
      waiting.text = text;
      resolve(new Blob([text], { type: "text/plain" }));
    };
    waiting.drop = () => reject(new Error("nothing copied"));
  });
  // A write refused for permission never reads the item, whose rejection would go unhandled
  blob.catch(() => {});
  let item;
  try {
    item = new ClipboardItem({ "text/plain": blob });
  } catch {
    return;
  }

  if (pending) {
    pending.drop();
  }
  pending = waiting;
  setTimeout(() => {
    if (pending === waiting) {
      pending = null;
      waiting.drop();
    }
  }, COPY_WAIT);

  navigator.clipboard.write([item]).catch(() => {
    if (pending === waiting) {
      pending = null;
    }
    if (waiting.text !== null) {
      offer(waiting.text);
    }
  });
}

function openLink(url) {
  const target = new URL(url, location.href);
  if (target.protocol !== "http:" && target.protocol !== "https:") {
    return;
  }
  // A window opened blank keeps no way back to this page, and tells when it was blocked
  const opened = window.open("", "_blank");
  if (opened) {
    opened.opener = null;
    opened.location.href = target.href;
    return;
  }

  // Safari opens a window only while it handles a tap, and this message comes after one
  const link = document.createElement("a");
  link.href = target.href;
  link.target = "_blank";
  link.rel = "noopener";
  link.textContent = `Open ${target.href}`;
  link.addEventListener("click", () => show(null));
  notice.replaceChildren(link);
  notice.hidden = false;
}

function connect() {
  const url = new URL("ws", location.href);
  url.protocol = location.protocol === "https:" ? "wss:" : "ws:";
  socket = new WebSocket(url);
  let opened = false;

  socket.onopen = () => {
    opened = true;
    show(null);
    fit();
  };
  socket.onmessage = (event) => {
    const message = JSON.parse(event.data);
    if (message.t === "frame") {
      frame(message);
    } else if (message.t === "copy") {
      copy(message.text);
    } else if (message.t === "open") {
      openLink(message.url);
    }
  };
  socket.onclose = async (event) => {
    if (event.code === LEFT) {
      return show("You left this session. Reload the page to come back.");
    }
    if (event.code === ENDED) {
      return show("smol web has ended.");
    }
    // A server started again has a new token, which this page's cookie can't match
    if (!opened) {
      const page = await fetch(location.href).catch(() => null);
      if (page && page.status === 403) {
        return show("This address's token is no longer valid. Open the address smol web printed.");
      }
    }
    show("Disconnected, reconnecting…");
    setTimeout(connect, RETRY_WAIT);
  };
}

function held(event) {
  return { ctrl: event.ctrlKey, alt: event.altKey, shift: event.shiftKey };
}

function latch(on) {
  latched = on;
  ctrlButton.setAttribute("aria-pressed", String(on));
}

function press(key, modifiers = {}) {
  send({ t: "key", key, ...modifiers, ctrl: modifiers.ctrl || latched });
  latch(false);
}

function reset() {
  keys.value = SENTINEL;
  keys.setSelectionRange(SENTINEL.length, SENTINEL.length);
}

// Named keys and chords go as keys, while typed text waits for beforeinput, as input methods
// compose it
function keyed(event) {
  committed = null;
  if (event.isComposing || event.keyCode === 229 || event.metaKey) {
    return;
  }
  const named = NAMED_KEYS.has(event.key) || /^F\d{1,2}$/.test(event.key);
  const alt = event.altKey && !MAC;
  const chord = (event.ctrlKey || alt || latched) && event.key.length === 1;
  const pastes = event.ctrlKey && !event.altKey && event.key.toLowerCase() === "v";
  if ((!named && !chord) || pastes) {
    return;
  }
  event.preventDefault();

  // Terminals take a chord's letter from where the key is, whatever the layout types there
  const letter = /^Key([A-Z])$/.exec(event.code);
  const foreign = chord && letter && event.key.charCodeAt(0) > 127;
  const key = foreign ? letter[1].toLowerCase() : event.key;
  const modifiers = held(event);
  press(key, modifiers);
}

keys.addEventListener("keydown", keyed);

keys.addEventListener("beforeinput", (event) => {
  const typed = event.inputType;
  if (event.isComposing) {
    // A composing keyboard holds the letter, so the latched ctrl takes it from the composition
    if (latched && event.data) {
      chorded = true;
      const letter = [...event.data].at(-1);
      press(letter);
    }
    return;
  }

  if (typed === "insertText" && event.data && latched) {
    const [first, ...rest] = event.data;
    press(first);
    if (rest.length > 0) {
      send({ t: "text", text: rest.join("") });
    }
  } else if (typed === "insertText" && /[\r\n]/.test(event.data)) {
    // Typing never inserts a line break, but a keyboard's clipboard does
    send({ t: "paste", text: event.data });
  } else if (typed === "insertText" && event.data) {
    // Some browsers insert the composed text once more after the composition ends
    if (event.data !== committed) {
      send({ t: "text", text: event.data });
    }
    committed = null;
  } else if (typed === "insertLineBreak" || typed === "insertParagraph") {
    press("Enter");
  } else if (typed === "deleteContentBackward") {
    press("Backspace");
  } else if (typed === "deleteContentForward") {
    press("Delete");
  }
  event.preventDefault();
});

// Chromium ends a composition on blur only after its text is in, not while it goes in
keys.addEventListener("input", (event) => {
  if (chorded && event.isComposing) {
    keys.blur();
    keys.focus({ preventScroll: true });
  }
});

keys.addEventListener("compositionend", (event) => {
  if (event.data && !chorded) {
    send({ t: "text", text: event.data });
  }
  chorded = false;
  committed = event.data;
  reset();
});

keys.addEventListener("paste", (event) => {
  event.preventDefault();
  const text = event.clipboardData && event.clipboardData.getData("text/plain");
  if (text) {
    send({ t: "paste", text });
  }
});

// A phone's keyboard has no paste of its own, and the field it types in is hidden
function paste() {
  const ask = () => {
    const area = pasting.querySelector("textarea");
    area.value = "";
    pasting.showModal();
    area.focus();
  };

  if (!window.isSecureContext || !navigator.clipboard || !navigator.clipboard.readText) {
    ask();
    return;
  }
  navigator.clipboard.readText().then((text) => {
    if (text) {
      send({ t: "paste", text });
    }
  }, ask);
}

// Pressing outside the dialogs keeps the focus, and with it the phone's keyboard
document.addEventListener("mousedown", (event) => {
  if (!event.target.closest("dialog")) {
    event.preventDefault();
  }
});

// A key stays lit while the finger is on it, as phones show :active for a tap barely if at all
const lit = (event) => {
  const button = event.target.closest("button");
  if (button) {
    button.classList.toggle("down", event.type === "pointerdown");
  }
};
for (const type of ["pointerdown", "pointerup", "pointercancel"]) {
  bar.addEventListener(type, lit);
}

function activate(button) {
  if (button === ctrlButton) {
    latch(!latched);
  } else if (button === pasteButton) {
    paste();
  } else if (button) {
    press(button.dataset.key);
  }
}

bar.addEventListener("click", (event) => activate(event.target.closest("button")));

// Safari keeps its keyboard up after a tap outside the screen only when the tap's end is canceled
document.addEventListener("touchend", (event) => {
  // The screen handles its own taps, and links and the paste dialog's field need the click
  if (event.target.closest("#screen, dialog, a, #paste")) {
    return;
  }
  // The end of a finger that moved can't be canceled, and clicks nothing anyway
  if (event.cancelable) {
    event.preventDefault();
  }

  const button = event.target.closest("#bar button");
  const finger = event.changedTouches[0];
  const under = document.elementFromPoint(finger.clientX, finger.clientY);
  // A pressed key shrinks, so a finger lifted at its edge may be over the bar
  const landed = bar.contains(under) ? (under.closest("button") ?? button) : null;
  if (button && landed === button) {
    activate(button);
  }
}, { passive: false });

function point(event) {
  const bounds = canvas.getBoundingClientRect();
  const x = Math.floor((event.clientX - bounds.left) / cell.width);
  const y = Math.floor((event.clientY - bounds.top) / cell.height);
  const clamp = (value, size) => Math.min(Math.max(value, 0), Math.max(size - 1, 0));
  return { x: clamp(x, screen.width), y: clamp(y, screen.height) };
}

let pressed = -1;
let dragged = null;

canvas.addEventListener("mousedown", (event) => {
  // A keyboard put away can leave the field focused, and only a new focus brings it back
  if (clicking) {
    clicking = false;
    keys.blur();
  }
  keys.focus({ preventScroll: true });
  pressed = event.button;
  dragged = point(event);
  send({ t: "mouse", kind: "down", button: event.button, ...dragged, ...held(event) });
});

function release(event) {
  const button = pressed;
  pressed = -1;
  const at = point(event);
  send({ t: "mouse", kind: "up", button, ...at, ...held(event) });
}

window.addEventListener("mousemove", (event) => {
  if (pressed < 0) {
    return;
  }
  // The release happened where this page didn't hear it, as in another window
  if (event.buttons === 0) {
    release(event);
    return;
  }
  const at = point(event);
  if (dragged && at.x === dragged.x && at.y === dragged.y) {
    return;
  }
  dragged = at;
  send({ t: "mouse", kind: "drag", button: pressed, ...at, ...held(event) });
});

window.addEventListener("mouseup", (event) => {
  if (pressed < 0) {
    return;
  }
  expectCopy();
  release(event);
});

// Returns the pixels too few for another scroll event
function sendScrolls(pixels, at, modifiers = {}) {
  const step = SCROLL_ROWS * cell.height;
  let left = pixels;
  while (Math.abs(left) >= step) {
    const kind = left < 0 ? "scrollup" : "scrolldown";
    send({ t: "mouse", kind, ...at, ...modifiers });
    left -= Math.sign(left) * step;
  }
  return left;
}

let wheeled = 0;
canvas.addEventListener("wheel", (event) => {
  event.preventDefault();
  // A pinch on a trackpad comes as a wheel with ctrl
  if (event.ctrlKey) {
    return;
  }
  const lines = event.deltaMode === WheelEvent.DOM_DELTA_LINE;
  const pixels = lines ? event.deltaY * cell.height : event.deltaY;
  const at = point(event);
  const modifiers = held(event);
  wheeled = sendScrolls(wheeled + pixels, at, modifiers);
}, { passive: false });

// The animation frame a fling waits for, until it slows to a stop or a touch stops it
let flung = null;

function fling(speed, rest, at) {
  const step = SCROLL_ROWS * cell.height;
  let last = performance.now();
  const glide = (now) => {
    const elapsed = Math.max(0, now - last);
    last = now;
    rest = sendScrolls(rest + speed * elapsed, at);
    speed *= FLING_KEEP ** elapsed;
    // Once the rest of its way can't reach another scroll event, a tap would stop nothing
    const reach = Math.abs(rest + speed / (1 - FLING_KEEP));
    flung = reach >= step ? requestAnimationFrame(glide) : null;
  };
  flung = requestAnimationFrame(glide);
}

let touch = null;

function abandon() {
  clearTimeout(touch.timer);
  if (touch.holding) {
    send({ t: "mouse", kind: "up", button: 0, ...touch.at });
  }
  touch = null;
}

canvas.addEventListener("touchstart", (event) => {
  // A second finger ends what the first began, and does nothing itself
  if (event.touches.length > 1) {
    if (touch) {
      abandon();
    }
    return;
  }
  const stopping = flung !== null;
  cancelAnimationFrame(flung);
  flung = null;

  const finger = event.touches[0];
  const at = point(finger);
  // The screen may change under the finger, as a keyboard coming up shrinks it
  const [x, y, width, height] = input;
  const typing = at.x >= x && at.x < x + width && at.y >= y && at.y < y + height;
  const started = {
    id: finger.identifier,
    x: finger.clientX,
    y: finger.clientY,
    last: finger.clientY,
    time: event.timeStamp,
    speed: 0,
    at,
    typing,
    stopping,
    rest: 0,
    moved: false,
    holding: false,
  };
  started.timer = setTimeout(() => {
    started.holding = true;
    send({ t: "mouse", kind: "down", button: 0, ...started.at });
    if (navigator.vibrate) {
      navigator.vibrate(10);
    }
  }, HOLD_WAIT);
  touch = started;
}, { passive: true });

canvas.addEventListener("touchmove", (event) => {
  event.preventDefault();
  if (!touch) {
    return;
  }
  const finger = [...event.touches].find((one) => one.identifier === touch.id);
  if (!finger) {
    return;
  }
  const at = point(finger);
  if (touch.holding) {
    if (at.x !== touch.at.x || at.y !== touch.at.y) {
      touch.at = at;
      send({ t: "mouse", kind: "drag", button: 0, ...at });
    }
    return;
  }

  const distance = Math.hypot(finger.clientX - touch.x, finger.clientY - touch.y);
  if (distance > cell.height / 2) {
    touch.moved = true;
    clearTimeout(touch.timer);
  }

  // The screen follows the finger, so moving it up scrolls down
  const pixels = touch.last - finger.clientY;
  const elapsed = event.timeStamp - touch.time;
  if (elapsed > 0) {
    touch.speed = pixels / elapsed;
  }
  touch.last = finger.clientY;
  touch.time = event.timeStamp;
  touch.rest = sendScrolls(touch.rest + pixels, at);
}, { passive: false });

const lift = (event) => {
  const finished = touch;
  const finger = finished && [...event.changedTouches].find((one) => one.identifier === finished.id);
  if (!finger) {
    event.preventDefault();
    return;
  }
  touch = null;
  clearTimeout(finished.timer);

  const lifted = event.type === "touchend";
  const tapped = lifted && !finished.moved && !finished.holding && !finished.stopping;
  // Safari brings its keyboard up only for a focus made while it handles a click,
  // so this tap goes on as one and the mouse handlers take it
  if (tapped && finished.typing) {
    clicking = true;
    return;
  }

  event.preventDefault();
  if (finished.holding) {
    expectCopy();
    send({ t: "mouse", kind: "up", button: 0, ...finished.at });
  } else if (tapped) {
    expectCopy();
    send({ t: "mouse", kind: "down", button: 0, ...finished.at });
    send({ t: "mouse", kind: "up", button: 0, ...finished.at });
    keys.blur();
  } else if (lifted && finished.moved) {
    const still = event.timeStamp - finished.time > STILL_WAIT;
    if (!still && Math.abs(finished.speed) >= MIN_FLING_SPEED) {
      const at = point(finger);
      fling(finished.speed, finished.rest, at);
    }
  }
};
canvas.addEventListener("touchend", lift);
canvas.addEventListener("touchcancel", lift);

copied.querySelector("button").addEventListener("click", () => copied.close());

pasting.addEventListener("click", (event) => {
  const button = event.target.closest("button");
  if (!button) {
    return;
  }
  const text = pasting.querySelector("textarea").value;
  if (button.value === "paste" && text) {
    send({ t: "paste", text });
  }
  pasting.close();
});

// Only the text boxes of the dialogs keep the browser's menu, which copying and pasting by hand need
document.addEventListener("contextmenu", (event) => {
  if (!event.target.closest("dialog textarea")) {
    event.preventDefault();
  }
});

// iOS zooms on a pinch whatever the viewport allows
document.addEventListener("gesturestart", (event) => event.preventDefault());

document.addEventListener("wheel", (event) => {
  if (event.ctrlKey) {
    event.preventDefault();
  }
}, { passive: false });

// A file dropped on the page would replace it
document.addEventListener("dragover", (event) => {
  event.preventDefault();
  event.dataTransfer.dropEffect = "none";
});
document.addEventListener("drop", (event) => event.preventDefault());

// The field's own handler still sends the ctrl chords on to the TUI
document.addEventListener("keydown", (event) => {
  const chord = event.ctrlKey || event.metaKey;
  if (chord && PAGE_KEYS.has(event.key.toLowerCase())) {
    event.preventDefault();
  }
});

// A hardware keyboard keeps typing to the screen after a tap elsewhere took the focus away
document.addEventListener("keydown", (event) => {
  if (document.activeElement !== document.body || copied.open || pasting.open) {
    return;
  }
  keys.focus({ preventScroll: true });
  keyed(event);
});

let fitting = null;
const refit = () => {
  clearTimeout(fitting);
  fitting = setTimeout(fit, FIT_WAIT);
};
if (window.visualViewport) {
  window.visualViewport.addEventListener("resize", () => {
    anchor();
    refit();
  });
  window.visualViewport.addEventListener("scroll", anchor);
} else {
  window.addEventListener("resize", refit);
}

// Moving to a screen of another pixel ratio resizes nothing
function watchRatio() {
  const ratio = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
  const changed = () => {
    refit();
    watchRatio();
  };
  ratio.addEventListener("change", changed, { once: true });
}

reset();
anchor();
watchRatio();
// A phone's keyboard waits for a tap on the input box
if (!window.matchMedia("(pointer: coarse)").matches) {
  keys.focus({ preventScroll: true });
}
connect();
