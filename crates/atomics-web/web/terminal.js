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
// How far a touch may move and still be a tap, and how long a tap may take (the app's gesture rules).
const SLOP = 8;
const TAP_MS = 450;
const LAYOUT_KEY = "atomics.layout";

let report = null;
let ta = null;
let bar = null;
let chip = null;
let probe = null;
let writable = false;
let copyText = null;
let composing = false;
let touch = null;
// The input's focus moves away and back while Copy uses a scratch field; the app is not told.
let quiet = false;
// The pointers over the terminal the app was told are down.
const down = new Set();
const coarse = matchMedia("(pointer: coarse)");
const vv = window.visualViewport;

// A microtask, so an event raised while the app runs never re-enters it.
const send = (event) => queueMicrotask(() => report && report(event));
const mods = (e) => (e.shiftKey ? 1 : 0) | (e.altKey ? 2 : 0) | (e.ctrlKey ? 4 : 0) | (e.metaKey ? 8 : 0);
const px = (value) => parseFloat(value) || 0;

function viewport() {
  const insets = probe ? getComputedStyle(probe) : null;
  const keybar = bar && !bar.hidden ? bar.offsetHeight : 0;
  const view = vv
    ? { x: vv.offsetLeft, y: vv.offsetTop, w: vv.width, h: vv.height, scale: vv.scale }
    : { x: 0, y: 0, w: innerWidth, h: innerHeight, scale: 1 };
  send({
    kind: "viewport", ...view, coarse: coarse.matches, keybar,
    insetTop: insets ? px(insets.paddingTop) : 0, insetBottom: insets ? px(insets.paddingBottom) : 0,
    insetLeft: insets ? px(insets.paddingLeft) : 0, insetRight: insets ? px(insets.paddingRight) : 0,
  });
}

// The key bar on the visual viewport's bottom edge, at a constant size even when the page is zoomed.
function placeBar() {
  if (!bar || bar.hidden) return;
  const s = vv ? vv.scale : 1;
  const [left, top, width, height] = vv ? [vv.offsetLeft, vv.offsetTop, vv.width, vv.height] : [0, 0, innerWidth, innerHeight];
  bar.style.width = width * s + "px";
  const y = top + height - bar.offsetHeight / s;
  bar.style.transform = `translate(${left}px, ${y}px) scale(${1 / s})`;
}

function showBar(on) {
  const show = on && writable && coarse.matches;
  if (bar.hidden === !show) return;
  bar.hidden = !show;
  placeBar();
  viewport();
}

// Pressing a key, the chip or the bar must not move the focus, or the soft keyboard closes. A touch acts
// on touchend: the event that lets a page use the clipboard.
function noFocus(el, act) {
  el.addEventListener("pointerdown", (e) => e.preventDefault());
  el.addEventListener("mousedown", (e) => e.preventDefault());
  el.addEventListener("touchstart", (e) => e.preventDefault(), { passive: false });
  el.addEventListener("touchend", (e) => { e.preventDefault(); act(); }, { passive: false });
  el.addEventListener("click", act); // a mouse; a prevented touch makes no click
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
      noFocus(button, () => {
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

function buildOverlay() {
  ta = document.createElement("textarea");
  ta.id = "atomics-input";
  ta.hidden = true;
  ta.readOnly = true;
  for (const [name, value] of [["autocapitalize", "off"], ["autocorrect", "off"], ["autocomplete", "off"],
    ["spellcheck", "false"], ["enterkeyhint", "enter"], ["aria-label", "Terminal input"]]) {
    ta.setAttribute(name, value);
  }
  document.body.append(ta);

  ta.addEventListener("focus", () => {
    if (quiet) return;
    send({ kind: "focus", on: true });
    showBar(true);
  });
  ta.addEventListener("blur", () => {
    composing = false;
    ta.value = "";
    if (quiet) return;
    // The caret stays in a field that lost the focus, and text a keyboard or IME commits after that would
    // focus the field again, be typed and ask for the lease: it goes with the focus. Typing again is a tap.
    const selection = getSelection();
    if (selection) selection.removeAllRanges();
    send({ kind: "focus", on: false });
    showBar(false);
  });
  ta.addEventListener("compositionstart", () => { composing = true; });
  ta.addEventListener("compositionupdate", (e) => send({ kind: "preedit", text: e.data || "" }));
  ta.addEventListener("compositionend", (e) => {
    composing = false;
    send({ kind: "preedit", text: "" });
    if (e.data) send({ kind: "text", text: e.data });
    ta.value = "";
  });
  ta.addEventListener("beforeinput", (e) => {
    if (composing || e.isComposing) return;
    const type = e.inputType;
    let event = null;
    if (type.startsWith("delete") && ta.value === "") event = { kind: "key", text: "Backspace", mods: 0 };
    else if (type === "insertLineBreak" || type === "insertParagraph") event = { kind: "key", text: "Enter", mods: 0 };
    else if ((type === "insertFromPaste" || type === "insertFromDrop") && e.dataTransfer) {
      event = { kind: "paste", text: e.dataTransfer.getData("text/plain") };
    } else if (type.startsWith("insert") && e.data) event = { kind: "text", text: e.data };
    if (event && e.cancelable) {
      e.preventDefault();
      send(event);
    }
  });
  // What a browser inserted without a cancelable beforeinput: forward it and keep the field empty.
  ta.addEventListener("input", () => {
    if (composing || !ta.value) return;
    send({ kind: "text", text: ta.value });
    ta.value = "";
  });
  ta.addEventListener("keydown", (e) => {
    // A soft keyboard that names no key (229) is read from beforeinput (above). A key named here is sent
    // from here, Return and Backspace too: a Backspace in the empty field raises no beforeinput in some
    // browsers. Preventing the key prevents its beforeinput, so nothing is sent twice.
    if (composing || e.isComposing || e.keyCode === 229 || e.metaKey) return;
    if (NAMED.has(e.key)) {
      e.preventDefault();
      send({ kind: "key", text: e.key, mods: mods(e) });
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
  // A touch is the app's: no loupe, callout, page zoom or scroll. A tap focuses the field, which opens the
  // keyboard; it has to happen here, in the touch's own handler.
  ta.addEventListener("touchstart", (e) => {
    e.preventDefault();
    const t = e.touches[0];
    touch = e.touches.length === 1 ? { x: t.clientX, y: t.clientY, at: e.timeStamp } : null;
  }, { passive: false });
  ta.addEventListener("touchmove", (e) => {
    e.preventDefault();
    const t = e.touches[0];
    if (touch && (e.touches.length > 1 || Math.hypot(t.clientX - touch.x, t.clientY - touch.y) > SLOP)) touch = null;
  }, { passive: false });
  ta.addEventListener("touchend", (e) => {
    e.preventDefault();
    const tap = touch && e.timeStamp - touch.at < TAP_MS;
    touch = null;
    if (tap && writable && document.activeElement !== ta) ta.focus({ preventScroll: true });
  }, { passive: false });
  for (const type of ["gesturestart", "gesturechange", "gestureend"]) ta.addEventListener(type, (e) => e.preventDefault());
}

/** Build the overlay and the key bar (`keys`: the app's rows, as JSON) and report to `callback`. */
export function install(keys, callback) {
  report = callback;
  probe = document.createElement("div");
  probe.id = "atomics-insets";
  document.body.append(probe);
  buildOverlay();
  buildBar(JSON.parse(keys));
  chip = document.createElement("button");
  chip.id = "atomics-copy";
  chip.type = "button";
  chip.tabIndex = -1;
  chip.textContent = "Copy";
  chip.hidden = true;
  noFocus(chip, copy);
  document.body.append(chip);
  const changed = () => { placeBar(); viewport(); };
  if (vv) {
    vv.addEventListener("resize", changed);
    vv.addEventListener("scroll", changed);
  }
  addEventListener("resize", changed);
  coarse.addEventListener("change", changed);
  viewport();
}

/** Lay the overlay over the terminal's cells. */
export function place(x, y, width, height) {
  Object.assign(ta.style, { left: x + "px", top: y + "px", width: width + "px", height: height + "px" });
  ta.hidden = false;
}

/** Leave out of the overlay what the app draws over it (a CSS clip-path, empty for nothing): the browser gives
    the pointer over a clipped part to the canvas under it. */
export function setClip(clip) {
  ta.style.clipPath = clip;
}

/** No terminal shows. */
export function hide() {
  if (document.activeElement === ta) ta.blur();
  ta.hidden = true;
}

/** Whether typing may start: a device that may type, connected, and nobody else typing. */
export function setWritable(on) {
  writable = on;
  ta.readOnly = !on;
  if (!on && document.activeElement === ta) ta.blur();
}

export function blurInput() {
  if (document.activeElement === ta) ta.blur();
}

/** A click picked the terminal under the input: type there. Only for a mouse or a pen; a finger taps the
    input itself, in its own touch handler, which is what opens the keyboard. */
export function focusInput() {
  if (!coarse.matches && writable && !ta.hidden && document.activeElement !== ta) ta.focus({ preventScroll: true });
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

/** The layout the user chose on this browser, if any. */
export function loadLayout() {
  try { return localStorage.getItem(LAYOUT_KEY) || ""; } catch (_) { return ""; }
}

export function saveLayout(name) {
  try { localStorage.setItem(LAYOUT_KEY, name); } catch (_) { /* a browser that keeps nothing */ }
}
