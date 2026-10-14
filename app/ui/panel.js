// The search panel of SpotLite's Windows and Linux shell (tried out on the Mac first). Every keystroke shows keyword
// results at once (the last word as a prefix); once typing pauses for 120 ms the meaning results replace them (the
// query was embedded while you typed). Results far weaker than the best are folded under "N weaker matches". The
// helper (`spotlite search-service`) does the work; DigUp's Mac app's panel is the
// reference for how it behaves.
"use strict";

const tauri = window.__TAURI__;
const invoke = (command, args) => tauri.core.invoke(command, args);
const ask = (request) => invoke("ask", { request });
const fileURL = (path) => tauri.core.convertFileSrc(path);

const TYPING_PAUSE = 120;          // ms after the last keystroke until the meaning results show
const WEB_PAUSE = 600;             // … and until "search:" asks Google (not a request per letter)
const ROWS_PER_GROUP = 6;
const FIELD_HEIGHT = 56;
const PANEL_HEIGHT = 480;
const WEAKER = "\u0000weaker";     // the selectable row that folds and unfolds the weaker matches
const isMac = /Mac/.test(navigator.platform);
const isWindows = /Win/.test(navigator.platform);
const MOD = isMac ? "⌘" : "Ctrl+";
const FILE_MANAGER = isMac ? "Finder" : isWindows ? "Explorer" : "Files";
// Pictures every web view shows as they are; anything else (HEIC, TIFF, RAW…) comes as JPEG from the helper.
const SHOWN_AS_IS = new Set(["jpg", "jpeg", "png", "gif", "webp", "bmp"]);
const GROUP_OF = { screenshot: "Screenshots", image: "Images", pdf: "Documents", doc: "Documents", video: "Video",
                   audio: "Audio", code: "Code" };
const KIND_TITLE = { screenshot: "Screenshot", image: "Image", pdf: "PDF", doc: "Document", audio: "Audio",
                     video: "Video", code: "Code" };
// Each group's dot (panel.css: --k-…).
const GROUP_DOT = { Screenshots: "screenshot", Images: "image", Documents: "pdf", Video: "video", Audio: "audio",
                    Code: "code" };
// Pictures bigger than this show as the helper's JPEG: a 100-megapixel photo decoded whole costs the web view
// hundreds of MB.
const LARGEST_SHOWN = 2400;

const $ = (id) => document.getElementById(id);
const $query = $("query"), $body = $("body"), $list = $("list"), $preview = $("preview");
const $footer = $("footer"), $summary = $("summary"), $keys = $("keys");
const $dig = $("dig"), $digLabel = $("dig-label"), $progress = $("progress");

const state = {
  generation: 0,
  finalGeneration: -1,   // the generation whose meaning results are showing: late keyword results don't replace them
  openOnResults: -1,     // Enter came before this generation's results: open its top one when they arrive
  timer: null,
  phase: "empty",        // empty | words | meaning | wordsOnly | codeHint | web (after "search:")
  text: "",              // what was searched (without "code:" or "search:")
  browser: null,         // the default browser's name, for "search:"
  webShown: false,       // "search:"'s web view is over the results area (the shell's browser.rs)
  webTimer: null,
  codeFolders: null,     // the code folders "code:" searches (none: it can't search code yet)
  codeOff: false,        // code indexing is turned off ("Index Code" in the tray)
  isCode: false,
  strong: [],
  weaker: [],
  groups: [],
  showWeaker: false,
  selection: null,       // a result's path, or WEAKER
  navigated: false,      // the arrow keys moved the selection since the last edit
  encoder: "stopped",
  height: FIELD_HEIGHT,
  home: "",
  error: null,
  indexing: null,        // {done, total} while the index helper works through new files
};
let debugPending = false;

// MARK: Searching

function queryChanged() {
  const generation = ++state.generation;
  clearTimeout(state.timer);
  state.navigated = false;
  state.error = null;
  const typed = $query.value.trim();
  // "search: what" searches the web (quotes around it are fine), not your files.
  clearTimeout(state.webTimer);
  if (/^search:/i.test(typed)) {
    state.isCode = false;
    show({ hits: [], strong: 0 }, typed.slice(7).trim().replace(/^"(.*)"$/s, "$1").trim(), "web");
    state.webTimer = setTimeout(showWeb, WEB_PAUSE);
    return;
  }
  const code = /^code:/i.test(typed);
  const text = code ? typed.slice(5).trim() : typed;
  state.isCode = code;
  if (!typed || (code && !text)) {
    show({ hits: [], strong: 0 }, text, typed ? "codeHint" : "empty");
    return;
  }
  // Words now. The last word is probably half-typed, so it matches as a prefix.
  ask({ op: "search", q: text, code, meaning: false, prefix_last: true, limit: 40 })
    .then((reply) => {
      if (generation === state.generation && state.finalGeneration !== generation) show(reply, text, "words");
    })
    .catch(failed);
  // The meaning is embedded now, so its vector is ready when typing pauses: the pause is the whole wait.
  ask({ op: "embed_ahead", q: text, code }).catch(() => {});
  state.timer = setTimeout(() => {
    ask({ op: "search", q: text, code, meaning: true, limit: 40 })
      .then((reply) => {
        if (generation !== state.generation) return;
        state.finalGeneration = generation;
        show(reply, text, reply.meaning ? "meaning" : "wordsOnly");
        if (state.openOnResults === generation) {
          state.openOnResults = -1;
          openSelected();
        }
        if (debugPending) report();
      })
      .catch(failed);
  }, TYPING_PAUSE);
}

function show(reply, text, phase) {
  const hits = reply.hits || [];
  const strong = Math.min(reply.strong ?? hits.length, hits.length);
  state.text = text;
  state.phase = phase;
  state.strong = hits.slice(0, strong);
  state.weaker = hits.slice(strong);
  // Grouped by kind, the best hit's group first; code is one group with the room of all of them.
  const groups = new Map();
  for (const hit of state.strong) {
    const title = GROUP_OF[hit.kind] || "Files";
    if (!groups.has(title)) groups.set(title, []);
    groups.get(title).push(hit);
  }
  const perGroup = state.isCode ? ROWS_PER_GROUP * 3 : ROWS_PER_GROUP;
  state.groups = [...groups].map(([title, rows]) => ({ title, rows: rows.slice(0, perGroup), total: rows.length }));
  if (!state.navigated) state.showWeaker = false;
  // Follow the top hit until you pick something with the arrow keys.
  const ids = rowIDs();
  if (!state.navigated || !ids.includes(state.selection)) state.selection = ids[0] ?? null;
  render();
}

function failed(error) {
  state.error = String(error);
  renderFooter();
}

function rowIDs() {
  return [
    ...state.groups.flatMap((group) => group.rows.map((hit) => hit.path)),
    ...(state.weaker.length ? [WEAKER] : []),
    ...(state.showWeaker ? state.weaker.map((hit) => hit.path) : []),
  ];
}

function selectedHit() {
  return [...state.strong, ...state.weaker].find((hit) => hit.path === state.selection) ?? null;
}

function move(step) {
  const ids = rowIDs();
  if (!ids.length) return;
  const index = ids.indexOf(state.selection);
  state.selection = ids[Math.max(0, Math.min(ids.length - 1, (index < 0 ? -1 : index) + step))];
  state.navigated = true;
  render();
}

function toggleWeaker() {
  state.showWeaker = !state.showWeaker;
  state.selection = WEAKER;
  render();
}

/// `search:`: the page the panel shows (or the search) in SpotLite's bigger browser window (`here`), or in the default
/// browser.
function openWeb(here) {
  if (state.text) invoke("open_web", { query: state.text, here }).catch(failed);
}

/// `search:`: Google's real results in the results area, a web view of the shell's laid over it, where links work as
/// in a browser. Asked again with the same query, it only moves (the page you went to stays).
function showWeb() {
  clearTimeout(state.webTimer);
  if (state.phase !== "web" || !state.text) return hideWeb();
  const { x, y, width, height } = $body.getBoundingClientRect();
  state.webShown = true;
  invoke("web_in_panel", { query: state.text, area: { x, y, width, height } }).catch(failed);
}

function hideWeb() {
  if (!state.webShown) return;
  state.webShown = false;
  invoke("web_in_panel", { query: null, area: null }).catch(() => {});
}

function openSelected() {
  if (state.phase === "web") return showWeb();
  const hit = selectedHit();
  if (hit) invoke("open_hit", { hit: openable(hit) }).catch(failed);
  // Enter before there's a result (the search service just started, or no file has the words): the top one opens
  // when the meaning results arrive.
  else if (state.finalGeneration !== state.generation) state.openOnResults = state.generation;
}

/// What the shell needs to open a result where it matched: its page, moment or line, and on a PDF page a word of the
/// query seen there, for the viewer to find and highlight (Evince opens at the page and marks it there).
function openable(hit) {
  return { path: hit.path, segment: hit.segment, loc: hit.loc ?? null, focus: hit.focus ?? null, find: wordOnPage(hit) };
}

/// What to find on the page: the query as typed, when the page's excerpt has it; else the longest word keyword search
/// marked («»), else the longest word of the query (4 letters or more) in the excerpt.
function wordOnPage(hit) {
  if (hit.kind !== "pdf" || !hit.excerpt) return null;
  // The whole query when the page has it as typed ("Loan Lens"), as the page writes it.
  const plain = hit.excerpt.replace(/[«»]/g, "");
  const at = plain.toLowerCase().indexOf(state.text.toLowerCase());
  if (/\s/.test(state.text.trim()) && at >= 0) return plain.slice(at, at + state.text.length);
  const marked = [...hit.excerpt.matchAll(/«([^»]+)»/g)].map((match) => match[1].trim());
  const words = marked.length ? marked : state.text.split(/[^\p{L}\p{N}_]+/u).filter((word) => word.length >= 4
    && new RegExp(`(^|[^\\p{L}\\p{N}_])${word}($|[^\\p{L}\\p{N}_])`, "iu").test(hit.excerpt));
  return words.sort((a, b) => b.length - a.length)[0] || null;
}

/// Where "Open at …" opens: "p. 12", "1:05", "line 40".
function openPlace(hit) {
  if (hit.segment === "lines") return `line ${Math.trunc(hit.focus ?? hit.loc)}`;
  if (hit.segment === "frame" || hit.segment === "audio") return clock(hit.loc);
  return `p. ${Math.trunc(hit.loc)}`;
}

function revealSelected() {
  const hit = selectedHit();
  if (hit) invoke("reveal", { path: hit.path }).catch(failed);
}

/// The selected result's full path on the clipboard; its "Copy path" button says so for a moment.
function copySelected() {
  const hit = selectedHit();
  if (!hit) return;
  invoke("copy_path", { path: hit.path }).then(() => {
    const button = document.querySelector(".buttons .copy");
    if (!button) return;
    button.textContent = "Copied";
    setTimeout(() => { button.textContent = "Copy path"; }, 2000);
  }).catch(failed);
}

// MARK: Drawing

function render() {
  const empty = state.phase === "empty";
  const height = empty ? FIELD_HEIGHT : PANEL_HEIGHT;
  if (height !== state.height) {
    state.height = height;
    document.documentElement.style.setProperty("--wanted", `${height}px`);
    invoke("panel_height", { height }).catch(() => {});
  }
  $body.hidden = empty;
  $footer.hidden = empty;
  if (state.phase !== "web") hideWeb();
  if (empty) return;
  renderList();
  renderPreview();
  renderFooter();
}

function renderList() {
  $list.replaceChildren();
  const nothing = !state.strong.length && !state.weaker.length;
  $list.hidden = nothing;
  if (nothing) return;
  for (const group of state.groups) {
    const title = element("div", "group-title", group.title);
    if (GROUP_DOT[group.title]) title.style.setProperty("--dot", `var(--k-${GROUP_DOT[group.title]})`);
    $list.append(title);
    for (const hit of group.rows) $list.append(row(hit));
  }
  if (state.weaker.length) {
    const fold = element("div", "weaker" + (state.showWeaker ? " open" : "") + (state.selection === WEAKER ? " selected" : ""));
    fold.innerHTML = ICONS.chevron;
    const count = state.weaker.length;
    fold.append(state.showWeaker ? "Weaker matches" : `${count} weaker ${count === 1 ? "match" : "matches"}`);
    fold.onclick = toggleWeaker;
    $list.append(fold);
    if (state.showWeaker) for (const hit of state.weaker) $list.append(row(hit));
  }
  $list.querySelector(".selected")?.scrollIntoView({ block: "nearest" });
}

function row(hit) {
  const item = element("div", "row" + (state.selection === hit.path ? " selected" : ""));
  item.setAttribute("role", "option");
  item.append(thumbnail(hit));
  const text = element("div", "row-text");
  const title = element("div", "row-title");
  title.append(element("span", "name", nameOf(hit.path)));
  const place = placeOf(hit);
  if (place) title.append(element("span", "badge", place));
  const sub = element("div", "row-sub");
  // The words found, where keyword search found them; else where the file is.
  if (hit.excerpt && hit.excerpt.includes("«")) sub.innerHTML = marked(hit.excerpt);
  else sub.textContent = folderOf(hit);
  text.append(title, sub);
  item.append(text);
  item.onclick = () => {
    state.selection = hit.path;
    state.navigated = true;
    render();
  };
  item.ondblclick = openSelected;
  return item;
}

/// A picture of the file for its row: the picture itself, a video's frame at the moment, a PDF's page; else its
/// kind's icon. All come small from the helper (a full-size photo per row would cost the web view hundreds of MB).
function thumbnail(hit) {
  const pixels = Math.round(36 * (window.devicePixelRatio || 1));
  const what = hit.kind === "image" || hit.kind === "screenshot" ? ["image", null]
    : hit.kind === "video" ? ["video", hit.loc ?? 0.5]
    : hit.kind === "pdf" ? ["pdf", hit.segment === "page" || hit.segment === "page_image" ? hit.loc : 1]
    : null;
  if (!what) return kindIcon(hit.kind);
  const image = element("img", "thumb");
  image.alt = "";
  rendered(hit.path, what[0], what[1], pixels)
    .then((path) => { image.src = fileURL(path); })
    .catch(() => image.replaceWith(kindIcon(hit.kind)));
  return image;
}

function renderPreview() {
  $preview.replaceChildren();
  if (!state.strong.length && !state.weaker.length) {
    if (state.phase === "web") {
      // What shows until the web view is over it.
      const outside = state.browser || "your browser";
      $preview.append(state.text
        ? message("Search the web", `Google's results for “${state.text}” show here. Shift+Enter opens them in a `
          + `bigger window, ${MOD}Enter in ${outside}.`)
        : message("Search the web", "Type what to search Google for after “search:”."));
    } else if ((state.phase === "codeHint" || state.isCode) && state.codeOff) {
      const box = message("Code search is off", "Turn it on to index the folders your code is in and search them "
        + "with “code:”.");
      const turnOn = element("button", "", "Turn on code search");
      turnOn.onclick = () => invoke("set_code_index", { on: true }).then((folders) => {
        state.codeOff = false;
        state.codeFolders = folders;
        queryChanged();
      }).catch(failed);
      const buttons = element("div", "buttons");
      buttons.append(turnOn);
      box.append(buttons);
      $preview.append(box);
    } else if ((state.phase === "codeHint" || state.isCode) && state.codeFolders && !state.codeFolders.length) {
      const box = message("No code folders yet", "Choose the folders your code is in. SpotLite indexes them for "
        + "“code:” (a repo's git-ignored files and secrets are left out).");
      const choose = element("button", "", "Choose code folders…");
      choose.onclick = () => invoke("choose_code_folders").then((folders) => {
        state.codeFolders = folders;
        queryChanged();
      }).catch(failed);
      const buttons = element("div", "buttons");
      buttons.append(choose);
      box.append(buttons);
      $preview.append(box);
    } else if (state.phase === "codeHint") {
      $preview.append(message("Search your code", "Say what the code does, or type a name that's in it."));
    } else if (state.indexing) {
      const { done, total } = state.indexing;
      $preview.append(message("Nothing yet", `Still indexing (${done} of ${total} files). Until a file is read, `
        + "SpotLite finds it only by its name."));
    } else if (state.isCode) {
      $preview.append(message("No good matches in your code", "Try saying what the code does, or a name that's in it."));
    } else {
      $preview.append(message("No good matches", "Try what you'd see or hear in it, or words you remember from it."));
    }
    return;
  }
  if (state.selection === WEAKER) {
    const count = state.weaker.length;
    $preview.append(message(state.showWeaker ? "Weaker matches" : `${count} weaker ${count === 1 ? "match" : "matches"}`,
      "They match much less well than the results above. Press Return to " + (state.showWeaker ? "fold them away." : "show them.")));
    return;
  }
  const hit = selectedHit();
  if (!hit) return;
  $preview.append(picture(hit), facts(hit));
}

/// The big picture: the photo or screenshot, the frame at the moment (it plays from there), the page, what's heard
/// at the moment, or the passage that matched.
function picture(hit) {
  const frame = element("div", "picture");
  const image = element("img");
  image.alt = "";
  frame.append(image);
  switch (hit.kind) {
    case "image":
    case "screenshot": {
      const info = hit.info || {};
      const large = Math.max(Number(info.width) || 0, Number(info.height) || 0) > LARGEST_SHOWN;
      if (SHOWN_AS_IS.has(extensionOf(hit.path)) && !large) image.src = fileURL(hit.path);
      else rendered(hit.path, "image", null, 1200).then((path) => { image.src = fileURL(path); }).catch(() => {});
      return frame;
    }
    case "pdf":
      rendered(hit.path, "pdf", hit.loc ?? 1, 1024).then((path) => { image.src = fileURL(path); }).catch(() => {});
      return frame;
    case "video": {
      // The frame at the moment, its time on it; a click plays the video from there.
      const at = hit.loc ?? 0;
      rendered(hit.path, "video", at, 1024).then((path) => { image.src = fileURL(path); }).catch(() => {});
      const moment = element("div", "moment");
      const play = element("div", "play");
      play.innerHTML = ICONS.play;
      moment.append(image, play, element("span", "at", clock(at)));
      frame.replaceChildren(moment);
      frame.onclick = () => {
        const video = element("video");
        video.src = `${fileURL(hit.path)}#t=${at}`;
        video.controls = true;
        video.autoplay = true;
        frame.replaceChildren(video);
      };
      return frame;
    }
    case "audio": {
      const audio = element("div", "audio-picture");
      audio.innerHTML = ICONS.audio;
      const player = element("audio");
      player.src = `${fileURL(hit.path)}#t=${hit.loc ?? 0}`;
      player.controls = true;
      player.preload = "metadata";
      audio.append(player);
      frame.replaceChildren(audio);
      return frame;
    }
    default: {
      const passage = element("div", "passage" + (hit.kind === "code" ? " code" : ""));
      passage.textContent = (hit.excerpt ?? "").replace(/[«»]/g, "");
      ask({ op: "text", segment: hit.segment_id, code: state.isCode })
        .then((reply) => { if (reply.text) passage.innerHTML = highlighted(reply.text, state.text); })
        .catch(() => {});
      return passage;
    }
  }
}

function facts(hit) {
  const box = element("div", "facts");
  box.append(element("div", "title", nameOf(hit.path)), element("div", "line", folderOf(hit)),
             element("div", "line", details(hit)));
  const reason = element("div", "reason");
  reason.innerHTML = ICONS.found;
  reason.append(element("span", "", reasonOf(hit)));
  box.append(reason);
  if (hit.excerpt && hit.excerpt.includes("«") && hit.kind !== "doc" && hit.kind !== "code") {
    const words = element("div", "reason-text");
    words.innerHTML = marked(hit.excerpt);
    box.append(words);
  }
  const buttons = element("div", "buttons");
  const open = element("button", "", "Open");
  open.onclick = openSelected;
  invoke("opens_at", { hit: openable(hit) })
    .then((at) => { if (at) open.textContent = `Open at ${openPlace(hit)}`; })
    .catch(() => {});
  const reveal = element("button", "", `Show in ${FILE_MANAGER}`);
  reveal.onclick = revealSelected;
  const copy = element("button", "copy", "Copy path");
  copy.title = `${isMac ? "⇧⌘C" : "Ctrl+Shift+C"}: copy the file's full path`;
  copy.onclick = copySelected;
  buttons.append(open, reveal, copy);
  box.append(buttons);
  return box;
}

function renderFooter() {
  const strong = state.strong.length, weaker = state.weaker.length;
  let summary;
  if (state.error) summary = `SpotLite's helper: ${state.error}`;
  else if (state.phase === "words") {
    summary = state.encoder === "starting" ? "Matching words; meaning search is warming up…" : "Matching words…";
  } else if (state.phase === "wordsOnly") summary = "Words only: meaning search isn't available";
  else if (state.phase === "codeHint") summary = "Searching your code";
  else if (state.phase === "web") summary = "Searching the web, not your files";
  else summary = `${strong} ${strong === 1 ? "result" : "results"}` + (weaker ? ` · ${weaker} weaker` : "");
  // Results while indexing are partial: say so where they're counted.
  if (state.indexing && !state.error) summary += ` · still indexing (${state.indexing.done} of ${state.indexing.total})`;
  $summary.textContent = summary;
  const mod = isMac ? "<kbd>⌘</kbd>" : "<kbd>Ctrl</kbd>";
  $keys.innerHTML = state.phase === "web" ? `<kbd>↩</kbd> Search <span class="gap"></span><kbd>Shift</kbd><kbd>↩</kbd> `
      + `Bigger <span class="gap"></span>${mod}<kbd>↩</kbd> ${escaped(state.browser || "Browser")} `
      + `<span class="gap"></span><kbd>Esc</kbd> Close`
    : `<kbd>↩</kbd> Open <span class="gap"></span>${mod}<kbd>↩</kbd> ${FILE_MANAGER}`
      + ` <span class="gap"></span><kbd>Esc</kbd> Close`;
}

function message(title, text) {
  const box = element("div", "message");
  box.append(element("strong", "", title), element("span", "", text));
  return box;
}

// MARK: What a result says

/// The page or moment that matched, as a badge: "p. 3", "0:06", "♪ 0:25" (a video's sound), "L12–40" (code).
function placeOf(hit) {
  if (hit.loc == null) return null;
  switch (hit.segment) {
    case "page":
    case "page_image": return `p. ${Math.trunc(hit.loc)}`;
    case "frame": return clock(hit.loc);
    case "audio": return hit.kind === "video" ? `♪ ${clock(hit.loc)}` : clock(hit.loc);
    case "lines": {
      const first = Math.trunc(hit.loc), last = Math.trunc(hit.loc_end ?? hit.loc);
      return first === last ? `L${first}` : `L${first}–${last}`;
    }
    case "cells": return span(hit);
    default: return null;
  }
}

function span(hit) {
  if (hit.loc == null) return null;
  const first = Math.trunc(hit.loc), last = Math.trunc(hit.loc_end ?? hit.loc);
  const what = hit.segment === "cells" ? ["cell", "cells"] : ["line", "lines"];
  return first === last ? `${what[0]} ${first}` : `${what[1]} ${first}–${last}`;
}

/// Why it's here, in a few words: "Matches the picture", "Matches the frame at 0:06".
function reasonOf(hit) {
  const meaning = (hit.z ?? 0) >= 2, words = hit.keyword_rank != null;
  if (!meaning) return words ? "Has your words" : "Matches its name";
  const at = hit.loc != null ? clock(hit.loc) : "";
  const page = hit.loc != null ? `page ${Math.trunc(hit.loc)}` : "a page";
  switch (hit.segment) {
    case "image": return hit.kind === "screenshot" ? "Matches what's on screen" : "Matches the picture";
    case "ocr": return "Matches the text in it";
    case "frame": return `Matches the frame at ${at}`;
    case "audio": return hit.kind === "video" ? `Matches the sound at ${at}` : `Matches what's heard at ${at}`;
    case "page": return `Matches the text on ${page}`;
    case "page_image": return `Matches ${page}`;
    case "chunk": return "Matches its text";
    case "lines":
    case "cells": return `Matches ${span(hit) ?? "the code"}`;
    default: return "Matches its name";
  }
}

/// "Screenshot · Oct 7, 2026 · 1440 × 900 · 62 KB", "PDF · … · 463 pages, 120 read so far · 23 MB"
function details(hit) {
  const info = hit.info || {};
  const parts = [KIND_TITLE[hit.kind] || "File", dateOf(hit.modified)];
  if (info.width && info.height) parts.push(`${info.width} × ${info.height}`);
  const readTo = info.read_to != null ? Number(info.read_to) : null;
  if (info.pages) {
    const pages = Number(info.pages);
    parts.push((pages === 1 ? "1 page" : `${pages.toLocaleString()} pages`) + (readTo != null ? `, ${readTo} read so far` : ""));
  } else if (readTo != null) parts.push("partly read so far");
  if (info.lines) parts.push(Number(info.lines) === 1 ? "1 line" : `${Number(info.lines).toLocaleString()} lines`);
  if (info.duration) parts.push(clock(Number(info.duration)));
  if (hit.size > 0) parts.push(bytes(hit.size));
  return parts.join(" · ");
}

function nameOf(path) {
  return path.split(/[\\/]/).pop();
}

function extensionOf(path) {
  const name = nameOf(path);
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : "";
}

/// Where the file is: "~/Downloads/Screenshots"; code says where it is in its repo: "billing/payments".
function folderOf(hit) {
  const folder = hit.path.slice(0, Math.max(0, hit.path.search(/[\\/][^\\/]*$/)));
  const repo = hit.info?.repo;
  if (hit.kind === "code" && repo && hit.path.startsWith(repo + "/")) {
    return folder.slice(repo.slice(0, repo.search(/[\\/][^\\/]*$/)).length + 1);
  }
  const shown = state.home && folder.startsWith(state.home) ? "~" + folder.slice(state.home.length) : folder;
  return isWindows ? shown.replaceAll("/", "\\") : shown;
}

function clock(seconds) {
  const total = Math.trunc(seconds);
  const two = (n) => String(n).padStart(2, "0");
  return total >= 3600 ? `${Math.trunc(total / 3600)}:${two(Math.trunc(total / 60) % 60)}:${two(total % 60)}`
                       : `${Math.trunc(total / 60)}:${two(total % 60)}`;
}

function dateOf(seconds) {
  return new Date(seconds * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}

/// Sizes the way file managers count them (1 KB = 1,000 bytes): "62 KB", "2.1 MB".
function bytes(count) {
  if (count < 1000) return `${count} bytes`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = count / 1000, unit = 0;
  while (value >= 1000 && unit < units.length - 1) { value /= 1000; unit += 1; }
  return `${value >= 10 || unit === 0 ? Math.round(value) : value.toFixed(1)} ${units[unit]}`;
}

/// An excerpt with the words keyword search found (marked « ») in bold.
function marked(excerpt) {
  return escaped(excerpt).replace(/«([^»]*)»/g, "<b>$1</b>");
}

/// A passage with the query's words marked where they start a word.
function highlighted(text, query) {
  const words = [...new Set(query.toLowerCase().split(/[^\p{L}\p{N}_]+/u).filter((word) => word.length > 1))];
  let html = escaped(text);
  if (!words.length) return html;
  const pattern = new RegExp(`(?<![\\p{L}\\p{N}])(${words.map((w) => w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")).join("|")})`, "giu");
  return html.replace(pattern, "<mark>$1</mark>");
}

function escaped(text) {
  return text.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
}

function element(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text != null) node.textContent = text;
  return node;
}

function kindIcon(kind) {
  const icon = element("div", "thumb icon");
  if (kind in KIND_TITLE) icon.style.setProperty("--dot", `var(--k-${kind})`);
  icon.innerHTML = ICONS[kind] || ICONS.doc;
  return icon;
}

/// The helper's JPEG of a page, a frame or a picture, asked for once per file and place.
const renders = new Map();
function rendered(path, kind, loc, pixels) {
  const key = `${path}|${kind}|${loc}|${pixels}`;
  if (!renders.has(key)) renders.set(key, ask({ op: "render", path, kind, loc, pixels }).then((reply) => reply.image));
  return renders.get(key);
}

const ICONS = {
  chevron: '<svg class="chevron" viewBox="0 0 16 16" width="12" height="12"><path d="M6 3.5 L10.5 8 L6 12.5" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"/></svg>',
  play: '<svg viewBox="0 0 20 20"><path d="M5 3 L17 10 L5 17 Z"/></svg>',
  found: '<svg viewBox="0 0 24 24"><circle cx="10" cy="10" r="6.5"/><path d="M15 15 L20.5 20.5 M10 7 V13 M7 10 H13"/></svg>',
  doc: '<svg viewBox="0 0 24 24"><path d="M6 3 H14 L19 8 V21 H6 Z M14 3 V8 H19 M9 12 H16 M9 15 H16 M9 18 H13"/></svg>',
  pdf: '<svg viewBox="0 0 24 24"><path d="M6 3 H14 L19 8 V21 H6 Z M14 3 V8 H19 M9 13 H16 M9 16 H14"/></svg>',
  code: '<svg viewBox="0 0 24 24"><path d="M9 7 L4 12 L9 17 M15 7 L20 12 L15 17"/></svg>',
  audio: '<svg viewBox="0 0 24 24"><path d="M3 12 H5 M7 8 V16 M10 5 V19 M13 9 V15 M16 6 V18 M19 10 V14 M21 12 H22"/></svg>',
  video: '<svg viewBox="0 0 24 24"><path d="M4 6 H15 V18 H4 Z M15 10 L20 7 V17 L15 14"/></svg>',
  image: '<svg viewBox="0 0 24 24"><path d="M4 5 H20 V19 H4 Z M4 16 L9 11 L13 15 L16 12 L20 16"/></svg>',
};
ICONS.screenshot = ICONS.image;

// MARK: Keys and events

$query.addEventListener("input", queryChanged);

document.addEventListener("keydown", (event) => {
  if (event.isComposing) return; // an input method is composing: its keys
  const modifier = isMac ? event.metaKey : event.ctrlKey;
  switch (event.key) {
    case "Escape":
      event.preventDefault();
      invoke("hide_panel").catch(() => {});
      break;
    case "ArrowDown":
    case "ArrowUp":
      if (event.altKey || event.shiftKey || modifier) return;
      event.preventDefault();
      move(event.key === "ArrowDown" ? 1 : -1);
      break;
    case "Enter":
      event.preventDefault();
      if (state.phase === "web" && (modifier || event.shiftKey)) openWeb(!modifier);
      else if (modifier) revealSelected();
      else if (state.selection === WEAKER) toggleWeaker();
      else openSelected();
      break;
    case "C":
    case "c":
      if (!modifier || !event.shiftKey || state.phase === "web") return;
      event.preventDefault();
      copySelected();
      break;
  }
});

function askBrowser() {
  invoke("web_browser").then((name) => { state.browser = name; }).catch(() => {});
  invoke("code_folders").then(({ folders, off }) => {
    state.codeFolders = folders;
    state.codeOff = off;
  }).catch(() => {});
}
askBrowser();

tauri.event.listen("focus-field", () => $query.focus());   // Ctrl+L in the web view
// Closed before the results came: an Enter pressed while searching no longer opens anything.
tauri.event.listen("panel-hidden", () => { state.openOnResults = -1; });
// The results area moved or changed size: the web view follows it.
new ResizeObserver(() => { if (state.webShown) showWeb(); }).observe($body);

tauri.event.listen("panel-shown", () => {
  askBrowser();
  // Like Spotlight: the last query, selected. The window is told its height again: a compositor may have kept another.
  invoke("panel_height", { height: state.height }).catch(() => {});
  $query.focus();
  $query.select();
  ask({ op: "prewarm" }).catch(() => {});
});

tauri.event.listen("helper", ({ payload }) => {
  const { from, event } = payload;
  if (from === "search" && event.event === "encoder") {
    state.encoder = event.state;
    if (state.phase !== "empty") renderFooter();
  }
  if (from === "index") {
    indexEvent(event);
    if (state.phase !== "empty") renderFooter();
  }
});

// MARK: Index new files

/// The pill in the field: "New files" looks for files that are new or changed since the last index and reads only
/// those (the rest keep what was read of them). It also says how indexing is going, whoever started it.
const dig = { asked: false, found: 0, indexed: 0, timer: null };

function showDig(stateName, label, title) {
  clearTimeout(dig.timer);
  $dig.dataset.state = stateName;
  $digLabel.textContent = label;
  $dig.title = title || "Look for files that are new or changed since the last index, and index only those";
}

function showProgress(done, total) {
  $progress.hidden = false;
  $progress.classList.toggle("looking", total == null);
  $progress.style.setProperty("--done", total ? `${Math.round((100 * done) / total)}%` : "0%");
}

function settle(stateName, label) {
  showDig(stateName, label);
  $progress.hidden = true;
  dig.timer = setTimeout(() => showDig("idle", "New files"), 4000);
}

async function indexNewFiles() {
  if ($dig.dataset.state === "looking" || $dig.dataset.state === "indexing") return;
  dig.asked = true;
  dig.found = 0;
  dig.indexed = 0;
  showDig("looking", "Looking for new files…");
  showProgress(0, null);
  try {
    await invoke("index_new_files");
  } catch (error) {
    dig.asked = false;
    settle("error", String(error));
  }
}

function indexEvent(event) {
  switch (event.event) {
    case "sync":
      if (!event.code) dig.found += (event.added || 0) + (event.changed || 0);
      if (dig.asked) showDig("looking", dig.found ? `Found ${dig.found} new` : "Looking for new files…");
      break;
    case "file":
      if (!event.total) break;
      state.indexing = { done: event.done, total: event.total };
      if (dig.asked) dig.indexed += 1;   // the sync just asked for, or one that was already under way
      showDig("indexing", `Indexing ${event.done}/${event.total}`);
      showProgress(event.done, event.total);
      break;
    case "rest":
      showDig("indexing", `Reading long files · ${event.files} left`);
      showProgress(event.done, event.total);
      break;
    case "waiting":
      settle("error", "Waiting for the model");
      break;
    case "error":
      settle("error", "Indexing problem");
      $dig.title = event.message || "";
      break;
    case "idle":
    case "exit":
      state.indexing = null;
      if (dig.asked) {
        dig.asked = false;
        const count = Math.max(dig.found, dig.indexed);
        settle("done", count ? `${count} new indexed` : "No new files");
      } else if ($dig.dataset.state === "indexing" || $dig.dataset.state === "looking") {
        settle("done", "Up to date");
      }
      break;
  }
}

$dig.addEventListener("mousedown", (event) => event.preventDefault());   // the field keeps the keyboard
$dig.addEventListener("click", indexNewFiles);

/// Debug runs (--debug-index-new): clicks "New files" once the model is in, and reports every label the button showed
/// until it settles.
invoke("debug_index_new").then(async (yes) => {
  if (!yes) return;
  const seen = [];
  const watch = new MutationObserver(() => {
    if (seen[seen.length - 1] !== $digLabel.textContent) seen.push($digLabel.textContent);
    if ($dig.dataset.state === "done") {
      watch.disconnect();
      invoke("debug_report", { report: { dig: seen, state: $dig.dataset.state } }).catch(() => {});
    }
  });
  watch.observe($dig, { subtree: true, childList: true, characterData: true, attributes: true });
  for (let tries = 0; tries < 30 && $dig.dataset.state !== "looking" && $dig.dataset.state !== "indexing"; tries++) {
    await indexNewFiles();
    if ($dig.dataset.state === "error") await new Promise((resolve) => setTimeout(resolve, 500));
  }
});

tauri.path.homeDir().then((home) => { state.home = home.replace(/[\\/]$/, ""); }).catch(() => {});
document.documentElement.style.setProperty("--wanted", `${state.height}px`);
invoke("panel_height", { height: state.height }).catch(() => {});   // the field alone, from the start

// Debug runs (--debug-query): type the query in, and once the meaning results and their pictures are in, report
// what the panel shows.
function debugSearch(query) {
  if (!query) return;
  debugPending = true;
  $query.value = query;
  queryChanged();
}
invoke("debug_query").then(debugSearch);
tauri.event.listen("debug-query", ({ payload }) => debugSearch(payload));   // after setup's first index

// Debug runs (--debug-sizes "name=query|…"): the panel's states one after another, each reported with its size and
// what shows, so a test can check (and photograph) every one. An empty query clears the field; "!weaker" opens the
// weaker matches; "!open" opens the selected result (or, after "search:", the web search) as Enter does;
// "!enter:words" types the words and presses Enter before any results came (it opens the top one when they do);
// "!open-outside" searches the web in the default browser, as Ctrl+Enter does; "!open-window" in SpotLite's bigger
// browser window, as Shift+Enter does; "!copy" copies the selected result's path, as Ctrl+Shift+C does; "!reshow"
// hides the panel and shows it again, as the hotkey does.
invoke("debug_sizes").then(async (spec) => {
  if (!spec) return;
  const pause = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
  await pause(800);
  for (const step of spec.split("|")) {
    const [name, ...rest] = step.split("=");
    const query = rest.join("=");
    if (query === "!weaker") {
      if (state.weaker.length) toggleWeaker();
    } else if (query === "!open") {
      openSelected();
    } else if (query.startsWith("!enter:")) {
      $query.value = query.slice(7);   // typed, and Enter pressed at once: before any results
      queryChanged();
      openSelected();
    } else if (query === "!open-outside") {
      openWeb(false);
    } else if (query === "!open-window") {
      openWeb(true);
    } else if (query === "!copy") {
      copySelected();
    } else if (query === "!reshow") {
      await invoke("debug_reshow").catch(() => {});
    } else {
      $query.value = query;
      queryChanged();
    }
    await pause(TYPING_PAUSE + 1400);   // the meaning results in, the window resized
    await invoke("debug_report", { report: measure(name) }).catch(() => {});
    await pause(900);   // time for a screenshot
  }
  await invoke("debug_report", { report: { sizes: "done" } }).catch(() => {});
});

/// What a state looks like, for the size checks: the window against the height the panel asked for, and anything
/// shown that it shouldn't, cut off, sticking out, or overlapping.
function measure(name) {
  const shown = (node) => node && node.getClientRects().length > 0 && getComputedStyle(node).display !== "none"
    && getComputedStyle(node).visibility !== "hidden";
  const describe = (node) => node.tagName.toLowerCase() + (node.id ? `#${node.id}` : "")
    + (node.className && typeof node.className === "string" ? `.${node.className.trim().split(/\s+/).join(".")}` : "");
  const problems = [];
  const panel = $("panel").getBoundingClientRect();
  // The window may be taller than the panel (on Linux it always is: the shell marks the rest as shadow), never shorter.
  if (window.innerHeight < state.height - 2) problems.push(`window ${window.innerHeight}px tall, shorter than ${state.height}`);
  if (Math.abs(window.innerWidth - 780) > 2) problems.push(`window ${window.innerWidth}px wide, wants 780`);
  if (Math.abs(panel.height - state.height) > 1) problems.push(`panel drawn ${panel.height}px tall, wants ${state.height}`);
  const empty = state.phase === "empty";
  if (empty && (shown($body) || shown($footer))) problems.push("results or footer showing with nothing typed");
  if (!empty && (!shown($body) || !shown($footer))) problems.push("results or footer hidden with a query");
  // Everything inside its own area (the list scrolls up and down, so only its sides count).
  for (const [selector, sides] of [[".field", "all"], [".preview", "all"], [".footer", "all"], [".list", "x"]]) {
    const area = document.querySelector(selector);
    if (!shown(area)) continue;
    const box = area.getBoundingClientRect();
    for (const node of area.querySelectorAll("*")) {
      if (!shown(node) || node.closest(".passage") && node !== area.querySelector(".passage")) continue;
      let clipped = false;
      for (let up = node.parentElement; up && up !== area; up = up.parentElement) {
        if (getComputedStyle(up).overflow !== "visible") clipped = true;
      }
      if (clipped) continue;
      const r = node.getBoundingClientRect();
      if (!r.width || !r.height) continue;
      const out = r.left < box.left - 1 || r.right > box.right + 1
        || (sides === "all" && (r.top < box.top - 1 || r.bottom > box.bottom + 1));
      if (out) problems.push(`${describe(node)} sticks out of ${selector}`);
    }
  }
  const q = $query.getBoundingClientRect(), dig = $dig.getBoundingClientRect();
  if (q.right > dig.left + 1) problems.push("the field runs under the New files pill");
  if (shown($footer)) {
    const s = $summary.getBoundingClientRect(), k = $keys.getBoundingClientRect();
    if (s.right > k.left + 1) problems.push("the footer's summary runs into its keys");
  }
  return { case: name, window: [window.innerWidth, window.innerHeight], wants: state.height, phase: state.phase,
           focused: document.hasFocus(), error: state.error,
           open: $preview.querySelector(".buttons button")?.textContent ?? null, top: selectedHit()?.path ?? null,
           copy: $preview.querySelector(".buttons .copy")?.textContent ?? null,
           message: $preview.querySelector(".message strong")?.textContent ?? null,
           web: state.webShown,
           results: state.strong.length + state.weaker.length, problems: [...new Set(problems)].slice(0, 12) };
}

function report() {
  debugPending = false;
  setTimeout(() => {
    const big = $preview.querySelector("img, video, audio, .passage");
    const thumbs = [...$list.querySelectorAll("img.thumb")];
    invoke("debug_report", {
      report: {
        query: $query.value,
        phase: state.phase,
        groups: state.groups.map((group) => ({ title: group.title, rows: group.rows.map((hit) => nameOf(hit.path)) })),
        weaker: state.weaker.length,
        selected: state.selection && nameOf(state.selection),
        preview: big && {
          tag: big.tagName.toLowerCase(),
          loaded: big.tagName === "IMG" ? big.complete && big.naturalWidth > 0 : null,
          size: big.tagName === "IMG" ? `${big.naturalWidth}x${big.naturalHeight}` : null,
          text: big.classList.contains("passage") ? big.textContent.slice(0, 80) : null,
        },
        thumbnails: `${thumbs.filter((image) => image.complete && image.naturalWidth > 0).length} of ${thumbs.length} drawn`,
        facts: [...$preview.querySelectorAll(".facts .title, .facts .line, .reason span")].map((node) => node.textContent),
        footer: [$summary.textContent, $keys.textContent],
        message: $preview.querySelector(".message strong")?.textContent ?? null,
      },
    }).catch(() => {});
  }, 1500);
}
