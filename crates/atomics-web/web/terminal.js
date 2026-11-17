// The terminal's input on the page (design §4.4, the decided `<textarea>` overlay; docs/daemon.md, "The
// browser client and its embedding"). A transparent textarea lies over the terminal the wasm app draws, so
// the browser owns the keyboard, composition and dictation; it forwards committed text, composition as
// preedit, named keys, pastes, its focus, and every pointer and wheel event over the terminal. The key bar
// sits on the keyboard (it follows visualViewport) and the Copy chip offers a selection. The app decides
// everything; this file only reports and places. Events go to the app as plain objects, in CSS pixels.

const NAMED = new Set([
  "Escape", "Tab", "Enter", "Backspace", "Delete", "Insert", "ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight",
  "Home", "End", "PageUp", "PageDown", "F1", "F2", "F3", "F4", "F5", "F6", "F7", "F8", "F9", "F10", "F11", "F12",
]);
// How far a touch may move and still be a tap: a finger wobbles on the glass (`gesture::TAP_SLOP`). How long a tap may
// take (the app's gesture rules), and how long egui still takes a press for a click, which is what focuses a text box.
const SLOP = 12;
const TAP_MS = 450;
const CLICK_MS = 800;
// With less of the screen than this in view, on a phone held sideways, the key bar keeps to one row that scrolls.
const SHORT = 420;
// The field is never empty, because a soft keyboard's Backspace needs something to delete: some raise no event at all
// in an empty field. This character is never typed, and stays before the caret.
const KEEP = "\u200b";
const VIEW_KEY = "atomics.view";
const MODE_KEY = "atomics.mode";

let report = null;
let commandChords = false;
// A key with a modifier was pressed: a real keyboard is there, even on a touch screen.
let hardKeyboard = false;
let ta = null;
let bar = null;
let chip = null;
let strip = null;
let probe = null;
let writable = false;
let copyText = null;
let composing = false;
// Where the app draws a text box, or a line that opens one, in CSS pixels: a tap there takes the keyboard.
let textBoxes = [];
// The input's focus moves away and back while Copy uses a scratch field; the app is not told.
let quiet = false;
// The pointers over the terminal the app was told are down.
const down = new Set();
const coarse = matchMedia("(pointer: coarse)");
const calm = matchMedia("(prefers-reduced-motion: reduce)");
const vv = window.visualViewport;

// A microtask, so an event raised while the app runs never re-enters it.
const send = (event) => queueMicrotask(() => report && report(event));
const mods = (e) => (e.shiftKey ? 1 : 0) | (e.altKey ? 2 : 0) | (e.ctrlKey ? 4 : 0) | (e.metaKey ? 8 : 0);
const px = (value) => parseFloat(value) || 0;

// The app's agent chords: Ctrl+1 to 9, Command on a Mac (`atomics_ui::window_keys`), by the physical key.
function agentDigit(e) {
  if (e.shiftKey || e.altKey || (commandChords ? !e.metaKey || e.ctrlKey : !e.ctrlKey || e.metaKey)) return null;
  const digit = /^(?:Digit|Numpad)([1-9])$/.exec(e.code || "Digit" + e.key);
  return digit && digit[1];
}

// The keyboard is on nothing on this page, as when it has just opened: the app reads the canvas's keys from here on.
// A hosting page that has the keyboard keeps it.
function toCanvas() {
  if (document.activeElement !== document.body || !document.hasFocus()) return;
  const canvas = document.getElementById("atomics");
  if (canvas) canvas.focus({ preventScroll: true });
}

// The window's keys, wherever the keyboard is on the page: the canvas, a text box or this input. A page that takes
// Ctrl+1 to 9 keeps them from the browser's tabs. Capture comes first, so nothing on the page sees them too. The app
// decides what they do.
function windowKeys(e) {
  if ((e.ctrlKey || e.metaKey || e.altKey) && !hardKeyboard) {
    // It has these keys, and the key bar has no more to give.
    hardKeyboard = true;
    if (bar) showBar(false);
  }
  if (composing || e.isComposing) return;
  const digit = agentDigit(e);
  if (digit) {
    e.preventDefault();
    e.stopPropagation();
    toCanvas();
    if (!e.repeat) send({ kind: "key", text: digit, mods: mods(e) });
  } else if (e.target === document.body && e.shiftKey && (e.key === "ArrowDown" || e.key === "ArrowUp")) {
    e.preventDefault();
    toCanvas();
    send({ kind: "key", text: e.key, mods: mods(e) });
  }
}

// What a finger does on an element, as far as a tap goes. Fed its touch events, `end` says whether they made a tap.
function tapTracker() {
  let at = null;
  return {
    start(e) {
      const t = e.touches[0];
      at = e.touches.length === 1 ? { x: t.clientX, y: t.clientY, time: e.timeStamp } : null;
    },
    move(e) {
      const t = e.touches[0];
      if (at && (e.touches.length > 1 || Math.hypot(t.clientX - at.x, t.clientY - at.y) > SLOP)) at = null;
    },
    end(e, ms) {
      const tapped = at !== null && e.timeStamp - at.time < ms;
      at = null;
      return tapped;
    },
  };
}

// The field holds only what keeps it from being empty. Moving the caret focuses a field in some browsers, so only one
// that has the focus gets that.
function emptyField() {
  ta.value = KEEP;
  if (document.activeElement === ta) ta.setSelectionRange(1, 1);
}

// The tallest the page has been at its width. In a frame the hosting page ends the frame above a soft keyboard, so the
// frame's own visual viewport never shows it: the frame grows shorter instead.
let tallest = { width: 0, height: 0 };

// Whether a soft keyboard plainly takes part of the screen: the visual viewport is short of the page, or the page is
// short of what it was at this width.
function softKeyboardUp() {
  if (innerWidth !== tallest.width) tallest = { width: innerWidth, height: innerHeight };
  tallest.height = Math.max(tallest.height, innerHeight);
  const view = vv ? vv.height : innerHeight;
  return innerHeight - view > 120 || tallest.height - view > 120;
}

// Composed text without the character that keeps the field from being empty: a keyboard that composes the word before
// the caret again takes it in.
const plain = (text) => (text || "").split(KEEP).join("");

// A finger asks to type: this has to run in the touch's own handler, or iOS opens no keyboard. A field that kept the
// focus while the keyboard was put away takes it again, quietly: focus alone would do nothing, and neither the lease
// nor the key bar changes. Where the keyboard is plainly up, nothing moves, and a word under way stays.
function openKeyboard() {
  if (!writable || ta.hidden) return;
  if (document.activeElement !== ta) {
    ta.focus({ preventScroll: true });
    return;
  }
  if (composing || softKeyboardUp()) return;
  quiet = true;
  try {
    ta.blur();
    ta.focus({ preventScroll: true });
  } finally {
    quiet = false;
  }
}

// A tap on a text box the app draws takes eframe's hidden input in the touch itself, or iOS opens no keyboard until a
// second tap. The app's own click then finds the keyboard where it wants it. This sees the touch before eframe does.
function watchCanvas() {
  const canvas = document.getElementById("atomics");
  const tap = tapTracker();
  let over = false;
  const on = (type, handler) => addEventListener(type, handler, { capture: true, passive: true });
  on("touchstart", (e) => {
    over = e.target === canvas;
    if (over) tap.start(e);
  });
  on("touchmove", (e) => {
    if (over) tap.move(e);
  });
  on("touchend", (e) => {
    if (!over) return;
    const t = e.changedTouches[0];
    const hit = textBoxes.some(([x, y, width, height]) =>
      t.clientX >= x && t.clientX <= x + width && t.clientY >= y && t.clientY <= y + height);
    if (!tap.end(e, CLICK_MS) || !hit) return;
    const agent = document.querySelector("#atomics + input");
    if (agent) agent.focus({ preventScroll: true });
  });
}

// The page never zooms, wherever a finger lands: the canvas, the input, the key bar or a gap between its keys. Safari
// leaves out the viewport's own limits (and in a frame they are the hosting page's), but not a pinch's gesture events
// refused, nor two fingers' moves, nor a double tap's dblclick; the style sheet's touch-action does the rest. A pinch
// still reaches the app as pointers, which on a phone zoom nothing either (`term_pane`).
function lockZoom() {
  const refuse = (e) => {
    if (e.cancelable) e.preventDefault();
  };
  const options = { capture: true, passive: false };
  for (const type of ["gesturestart", "gesturechange", "gestureend", "dblclick"]) addEventListener(type, refuse, options);
  addEventListener("touchmove", (e) => {
    if (e.touches.length > 1) refuse(e);
  }, options);
}

// Letting go of the input leaves the keyboard on the canvas, so the app keeps reading keys there.
function leaveInput() {
  ta.blur();
  toCanvas();
}

function viewport() {
  const insets = probe ? getComputedStyle(probe) : null;
  const keybar = bar && !bar.hidden ? bar.offsetHeight : 0;
  const view = vv
    ? { x: vv.offsetLeft, y: vv.offsetTop, w: vv.width, h: vv.height, scale: vv.scale }
    : { x: 0, y: 0, w: innerWidth, h: innerHeight, scale: 1 };
  send({
    kind: "viewport", ...view, coarse: coarse.matches, keybar, screenShort: Math.min(screen.width, screen.height),
    calm: calm.matches,
    insetTop: insets ? px(insets.paddingTop) : 0, insetBottom: insets ? px(insets.paddingBottom) : 0,
    insetLeft: insets ? px(insets.paddingLeft) : 0, insetRight: insets ? px(insets.paddingRight) : 0,
  });
}

// The key bar on the visual viewport's bottom edge, at a constant size even when the page is zoomed.
function placeBar() {
  if (!bar || bar.hidden) return;
  bar.classList.toggle("one-row", (vv ? vv.height : innerHeight) < SHORT);
  const s = vv ? vv.scale : 1;
  const [left, top, width, height] = vv ? [vv.offsetLeft, vv.offsetTop, vv.width, vv.height] : [0, 0, innerWidth, innerHeight];
  bar.style.width = width * s + "px";
  const y = top + height - bar.offsetHeight / s;
  bar.style.transform = `translate(${left}px, ${y}px) scale(${1 / s})`;
}

function showBar(on) {
  const show = on && writable && coarse.matches && !hardKeyboard;
  if (bar.hidden === !show) return;
  bar.hidden = !show;
  placeBar();
  viewport();
}

// Pressing a key, the chip or a gap in the bar must not move the focus, or the soft keyboard closes: no press of
// theirs makes the mouse events that move it, and no touch a click or a long press's menu.
function keepFocus(el) {
  el.addEventListener("pointerdown", (e) => e.preventDefault());
  el.addEventListener("mousedown", (e) => e.preventDefault());
  el.addEventListener("touchend", (e) => { if (e.cancelable) e.preventDefault(); }, { passive: false });
  el.addEventListener("contextmenu", (e) => e.preventDefault());
}

// How far the key bar's rows have scrolled, together.
const scrolled = () => (bar ? [bar, ...bar.children].reduce((sum, el) => sum + el.scrollLeft, 0) : 0);

// A key or the chip acts on a tap's end, the event that lets a page use the clipboard. A finger that moves on, or a
// row that scrolls under it, is a swipe: the browser scrolls the row (its touch-action), and nothing is typed. A
// mouse clicks; a touch makes no click (keepFocus).
function onTap(el, act) {
  const tap = tapTracker();
  let from = 0;
  el.addEventListener("touchstart", (e) => { tap.start(e); from = scrolled(); }, { passive: true });
  el.addEventListener("touchmove", (e) => tap.move(e), { passive: true });
  el.addEventListener("touchcancel", (e) => tap.end(e, 0));
  el.addEventListener("touchend", (e) => {
    if (tap.end(e, Infinity) && scrolled() === from) act();
  });
  el.addEventListener("click", act);
}

function paste() {
  if (!navigator.clipboard || !navigator.clipboard.readText) {
    return send({ kind: "note", text: "This browser does not let the page read the clipboard. Paste with the keyboard instead." });
  }
  navigator.clipboard.readText().then(
    (text) => text && send({ kind: "paste", text }),
    () => send({ kind: "note", text: "The clipboard could not be read. Paste with the keyboard instead." }),
  );
}

function copy() {
  const text = copyText;
  if (!text) return;
  const done = () => send({ kind: "copied" });
  const fallback = () => {
    // Older browsers, and a frame its page does not allow to write the clipboard (`allow="clipboard-write"`):
    // copy from a selected scratch field. The focus goes back only where it was, quietly, so copying neither
    // asks for the terminal's lease nor opens the keyboard.
    const typing = document.activeElement === ta;
    const scratch = document.createElement("textarea");
    scratch.value = text;
    scratch.setAttribute("readonly", "");
    scratch.className = "atomics-scratch";
    quiet = true;
    try {
      document.body.append(scratch);
      scratch.select();
      try { document.execCommand("copy"); } catch (_) { /* nothing more to try */ }
      scratch.remove();
      if (typing && !ta.hidden) ta.focus({ preventScroll: true });
    } finally {
      quiet = false;
    }
    done();
  };
  if (navigator.clipboard && navigator.clipboard.writeText) navigator.clipboard.writeText(text).then(done, fallback);
  else fallback();
}

function buildBar(rows) {
  bar = document.createElement("div");
  bar.id = "atomics-keybar";
  bar.setAttribute("role", "toolbar");
  bar.setAttribute("aria-label", "Terminal keys");
  bar.hidden = true;
  keepFocus(bar);
  for (const row of rows) {
    const line = document.createElement("div");
    line.className = "atomics-keyrow";
    for (const key of row) {
      const button = document.createElement("button");
      button.type = "button";
      button.tabIndex = -1;
      button.textContent = key.label;
      button.setAttribute("aria-label", key.name);
      button.dataset.kind = key.kind;
      button.dataset.value = key.value;
      if (key.wide) button.classList.add("wide");
      if (key.value === "Ctrl" || key.value === "Alt") button.setAttribute("aria-pressed", "false");
      onTap(button, () => {
        if (key.kind === "key") send({ kind: "key", text: key.value, mods: 0 });
        else if (key.kind === "text") send({ kind: "text", text: key.value });
        else paste();
      });
      line.append(button);
    }
    bar.append(line);
  }
  document.body.append(bar);
}

// A tap on `el` opens the keyboard, and nothing else happens to its touches.
function tapOpensKeyboard(el) {
  const tap = tapTracker();
  el.addEventListener("touchstart", (e) => { e.preventDefault(); tap.start(e); }, { passive: false });
  el.addEventListener("touchmove", (e) => { e.preventDefault(); tap.move(e); }, { passive: false });
  el.addEventListener("touchend", (e) => {
    e.preventDefault();
    if (tap.end(e, TAP_MS)) openKeyboard();
  }, { passive: false });
}

// The strip over the terminal says to tap the terminal, so a tap on the words themselves counts too. It is a button
// for a screen reader, a mouse and the keyboard as well: activated, it types in the terminal.
function buildStrip() {
  strip = document.createElement("div");
  strip.id = "atomics-strip";
  strip.hidden = true;
  strip.tabIndex = 0;
  strip.setAttribute("role", "button");
  strip.setAttribute("aria-label", "Type in this terminal");
  tapOpensKeyboard(strip);
  strip.addEventListener("click", openKeyboard);
  strip.addEventListener("keydown", (e) => {
    if (e.key !== "Enter" && e.key !== " ") return;
    e.preventDefault();
    openKeyboard();
  });
  document.body.append(strip);
}

function buildOverlay() {
  ta = document.createElement("textarea");
  ta.id = "atomics-input";
  ta.hidden = true;
  ta.readOnly = true;
  for (const [name, value] of [["autocapitalize", "off"], ["autocorrect", "off"], ["autocomplete", "off"],
    ["spellcheck", "false"], ["enterkeyhint", "enter"], ["aria-label", "Terminal input"]]) {
    ta.setAttribute(name, value);
  }
  ta.value = KEEP;
  document.body.append(ta);

  ta.addEventListener("focus", () => {
    emptyField();
    if (quiet) return;
    send({ kind: "focus", on: true });
    showBar(true);
  });
  ta.addEventListener("blur", () => {
    composing = false;
    emptyField();
    if (quiet) return;
    // The caret stays in a field that lost the focus, and text a keyboard or IME commits after that would
    // focus the field again, be typed and ask for the lease: it goes with the focus. Typing again is a tap.
    const selection = getSelection();
    if (selection) selection.removeAllRanges();
    send({ kind: "focus", on: false });
    showBar(false);
  });
  ta.addEventListener("compositionstart", () => { composing = true; });
  ta.addEventListener("compositionupdate", (e) => send({ kind: "preedit", text: plain(e.data) }));
  ta.addEventListener("compositionend", (e) => {
    composing = false;
    send({ kind: "preedit", text: "" });
    const text = plain(e.data);
    if (text) send({ kind: "text", text });
    emptyField();
  });
  ta.addEventListener("beforeinput", (e) => {
    if (composing || e.isComposing) return;
    const type = e.inputType;
    let event = null;
    if (type.startsWith("delete") && (ta.value === KEEP || ta.value === "")) event = { kind: "key", text: "Backspace", mods: 0 };
    else if (type === "insertLineBreak" || type === "insertParagraph") event = { kind: "key", text: "Enter", mods: 0 };
    else if ((type === "insertFromPaste" || type === "insertFromDrop") && e.dataTransfer) {
      event = { kind: "paste", text: e.dataTransfer.getData("text/plain") };
    } else if (type.startsWith("insert") && e.data) event = { kind: "text", text: e.data };
    if (event && e.cancelable) {
      e.preventDefault();
      send(event);
    }
  });
  // What a browser changed without a cancelable beforeinput: forward it and start over. With nothing left, not even
  // what keeps the field from being empty, that was a Backspace.
  ta.addEventListener("input", () => {
    if (composing || ta.value === KEEP) return;
    const text = ta.value.split(KEEP).join("");
    send(text ? { kind: "text", text } : { kind: "key", text: "Backspace", mods: 0 });
    emptyField();
  });
  // The caret stays after that character, so a Backspace always has it to delete.
  document.addEventListener("selectionchange", () => {
    if (document.activeElement === ta && !composing && ta.selectionStart !== ta.value.length) {
      ta.setSelectionRange(ta.value.length, ta.value.length);
    }
  });
  ta.addEventListener("keydown", (e) => {
    // A soft keyboard that names no key (229) is read from beforeinput (above). A key named here is sent
    // from here, Return and Backspace too: a Backspace in the empty field raises no beforeinput in some
    // browsers. Preventing the key prevents its beforeinput, so nothing is sent twice.
    if (composing || e.isComposing || e.keyCode === 229 || e.metaKey) return;
    if (NAMED.has(e.key)) {
      e.preventDefault();
      send({ kind: "key", text: e.key, mods: mods(e) });
    } else if (e.ctrlKey && /^(?:Digit|Numpad)[0-9]$/.test(e.code)) {
      // Never a terminal's, whatever the layout types there (`windowKeys` has the agent chords).
    } else if (e.ctrlKey && !e.shiftKey && !e.altKey && [...e.key].length === 1 && e.key.toLowerCase() !== "v") {
      // Ctrl with a character is the terminal's (Ctrl+V stays the browser's paste).
      e.preventDefault();
      send({ kind: "key", text: e.key, mods: mods(e) });
    }
  });
  ta.addEventListener("copy", (e) => {
    if (!copyText || !e.clipboardData) return;
    e.clipboardData.setData("text/plain", copyText);
    e.preventDefault();
    send({ kind: "copied" });
  });

  // Every pointer over the terminal goes to the app, which tells taps, drags, long presses and pinches.
  const pointer = (phase) => (e) => {
    const event = (step, id) =>
      ({ kind: "pointer", phase: step, pointer: e.pointerType, id, x: e.clientX, y: e.clientY, t: e.timeStamp });
    if (phase === "down") {
      // A primary pointer is a new touch's first finger, or a mouse: one still down here lost its lift on the
      // way. It is cancelled, or the app would take the new touch for a second finger.
      if (e.isPrimary) {
        for (const id of down) if (id !== e.pointerId) send(event("cancel", id));
        down.clear();
      }
      down.add(e.pointerId);
      try { ta.setPointerCapture(e.pointerId); } catch (_) { /* a pointer the page cannot capture */ }
    } else if (phase !== "move") {
      down.delete(e.pointerId);
    }
    send(event(phase, e.pointerId));
  };
  ta.addEventListener("pointerdown", pointer("down"));
  ta.addEventListener("pointermove", pointer("move"));
  ta.addEventListener("pointerup", pointer("up"));
  ta.addEventListener("pointercancel", pointer("cancel"));
  ta.addEventListener("wheel", (e) => {
    e.preventDefault();
    const unit = e.deltaMode === 1 ? 16 : e.deltaMode === 2 ? innerHeight : 1;
    send({ kind: "wheel", dx: e.deltaX * unit, dy: e.deltaY * unit, x: e.clientX, y: e.clientY, zoom: e.ctrlKey });
  }, { passive: false });
  // A touch is the app's: no loupe, callout, page zoom or scroll. A tap opens the keyboard.
  tapOpensKeyboard(ta);
  for (const type of ["gesturestart", "gesturechange", "gestureend"]) ta.addEventListener(type, (e) => e.preventDefault());
}

// The drawer is one step in the tab's history while it is open, so Back (a browser's, a phone's or a tablet's) closes it.
// Closed some other way, it takes its step back itself, so Back then goes where it went before. In a frame the step is in
// the hosting page's history too, and its Back closes the drawer the same way.
let drawerStep = false;
const onDrawerStep = () => history.state !== null && typeof history.state === "object" && history.state.atomicsDrawer === true;

function watchBack() {
  try {
    // A reload while the drawer was open comes back to its step, with no drawer: it is an ordinary place again.
    if (onDrawerStep()) history.replaceState(null, "");
  } catch (_) { /* a page whose history cannot be written */ }
  addEventListener("popstate", () => {
    if (!drawerStep || onDrawerStep()) return;
    drawerStep = false;
    send({ kind: "back" });
  });
}

/** The drawer opened or closed: give it its step in the tab's history, or take that step back. */
export function setDrawer(open) {
  if (open === drawerStep) return;
  drawerStep = open;
  try {
    if (open) history.pushState({ atomicsDrawer: true }, "");
    else if (onDrawerStep()) history.back();
  } catch (_) { /* a page whose history cannot be written */ }
}

/** Build the overlay and the key bar (`keys`: the app's rows, as JSON) and report to `callback`. The agent chords use
    Command when `command`, else Ctrl. */
export function install(keys, command, callback) {
  report = callback;
  commandChords = command;
  addEventListener("keydown", windowKeys, true);
  lockZoom();
  watchBack();
  probe = document.createElement("div");
  probe.id = "atomics-insets";
  document.body.append(probe);
  buildOverlay();
  buildStrip();
  watchCanvas();
  buildBar(JSON.parse(keys));
  chip = document.createElement("button");
  chip.id = "atomics-copy";
  chip.type = "button";
  chip.tabIndex = -1;
  chip.textContent = "Copy";
  chip.hidden = true;
  keepFocus(chip);
  onTap(chip, copy);
  document.body.append(chip);
  const changed = () => {
    // The soft keyboard came up after all, so its bar is wanted again. Asked every time, so the page's height at its
    // width is known before a keyboard comes.
    const soft = softKeyboardUp();
    if (hardKeyboard && soft) {
      hardKeyboard = false;
      if (document.activeElement === ta) showBar(true);
    }
    placeBar();
    viewport();
  };
  if (vv) {
    vv.addEventListener("resize", changed);
    vv.addEventListener("scroll", changed);
  }
  addEventListener("resize", changed);
  coarse.addEventListener("change", changed);
  calm.addEventListener("change", changed);
  softKeyboardUp();
  viewport();
}

/** Lay the overlay over the terminal's cells. */
export function place(x, y, width, height) {
  Object.assign(ta.style, { left: x + "px", top: y + "px", width: width + "px", height: height + "px" });
  ta.hidden = false;
}

/** Lay the tappable part of the strip over the terminal; nothing there while `width` is 0. */
export function placeStrip(x, y, width, height) {
  Object.assign(strip.style, { left: x + "px", top: y + "px", width: width + "px", height: height + "px" });
  strip.hidden = width <= 0;
}

/** Where a tap takes the keyboard for a text box the app draws: `[[x, y, width, height]]` in CSS pixels, as JSON. */
export function setTextBoxes(boxes) {
  textBoxes = JSON.parse(boxes);
  // The canvas says nothing else about what it draws, and this is how a test finds them.
  const canvas = document.getElementById("atomics");
  if (canvas) canvas.dataset.textBoxes = boxes;
}

/** Leave out of the overlay what the app draws over it (a CSS clip-path, empty for nothing): the browser gives
    the pointer over a clipped part to the canvas under it. */
export function setClip(clip) {
  ta.style.clipPath = clip;
}

/** No terminal shows. */
export function hide() {
  if (document.activeElement === ta) leaveInput();
  ta.hidden = true;
}

/** Whether typing may start: a device that may type, connected, and nobody else typing. */
export function setWritable(on) {
  writable = on;
  ta.readOnly = !on;
  if (!on && document.activeElement === ta) leaveInput();
}

export function blurInput() {
  if (document.activeElement === ta) leaveInput();
}

/** A click picked the terminal under the input, or the keyboard came back to it: type there. Only for a mouse, a pen
    or a real keyboard; a finger taps the input itself, in its own touch handler, which is what opens the soft one. */
export function focusInput() {
  const keyboard = !coarse.matches || hardKeyboard;
  if (keyboard && writable && !ta.hidden && document.activeElement !== ta) ta.focus({ preventScroll: true });
}

/** Light the key bar's sticky Ctrl and Alt. */
export function setSticky(ctrl, alt) {
  for (const button of bar.querySelectorAll("button[aria-pressed]")) {
    const on = button.dataset.value === "Ctrl" ? ctrl : alt;
    button.classList.toggle("on", on);
    button.setAttribute("aria-pressed", String(on));
  }
}

/** Offer Copy for `text` around x: above `y`, or below `below` where that would leave the terminal and cover
    the strip over it. `null` takes it away. */
export function setCopy(text, x, y, below) {
  copyText = text;
  chip.hidden = !text;
  if (!text) {
    chip.removeAttribute("title");
    return;
  }
  // What a click copies, as a pointer's tooltip shows it.
  chip.title = `Copy “${text.length > 80 ? text.slice(0, 79) + "…" : text}”`;
  const width = chip.offsetWidth || 64;
  const height = chip.offsetHeight || 44;
  const left = Math.max(8, Math.min(innerWidth - width - 8, x - width / 2));
  const above = y - height - 8;
  const top = above >= px(ta.style.top) ? above : Math.min(innerHeight - height - 8, below + 8);
  Object.assign(chip.style, { left: left + "px", top: Math.max(8, top) + "px" });
}

/** The app's theme for the page's own controls: `{name: "rgba(…)"}` as CSS custom properties. */
export function setTheme(colors) {
  for (const [name, value] of Object.entries(JSON.parse(colors))) {
    document.documentElement.style.setProperty(`--atomics-${name}`, value);
  }
}

/** The view (board or diff) the switcher last showed on this browser, if any. */
export function loadView() {
  try { return localStorage.getItem(VIEW_KEY) || ""; } catch (_) { return ""; }
}

export function saveView(name) {
  try { localStorage.setItem(VIEW_KEY, name); } catch (_) { /* a browser that keeps nothing */ }
}

/** This browser's own choice of simple or full layout; empty follows the host's `[ui] mode`. */
export function loadMode() {
  try { return localStorage.getItem(MODE_KEY) || ""; } catch (_) { return ""; }
}

export function saveMode(name) {
  try {
    if (name) { localStorage.setItem(MODE_KEY, name); } else { localStorage.removeItem(MODE_KEY); }
  } catch (_) { /* a browser that keeps nothing */ }
}
