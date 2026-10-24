// The admin interface: one page, four views, all talking to the WordMaker
// REST API through this server, which adds the token on the way.

import initEditor, { mount } from "/vendor/asciidoc_editor.js";

const $ = (selector) => document.querySelector(selector);

// ---- Talking to the server ------------------------------------------------

/** Calls the REST API; resolves to its JSON (or text), rejects with its error. */
async function api(method, path, body) {
  // The server refuses API calls without this header; see API_GUARD_HEADER.
  const init = { method, headers: { "x-wordmaker-admin": "1" } };
  if (body instanceof FormData) {
    init.body = body;
  } else if (body !== undefined) {
    init.headers["content-type"] = "application/json";
    init.body = JSON.stringify(body);
  }
  const response = await fetch(`/api/v1${path}`, init);
  const text = await response.text();
  if (!response.ok) {
    let message = text.trim() || `${response.status} ${response.statusText}`;
    try {
      message = JSON.parse(text).error ?? message;
    } catch {
      // Not JSON: axum's own rejections are plain text.
    }
    throw Object.assign(new Error(message), { status: response.status });
  }
  if (!text) return null;
  return (response.headers.get("content-type") ?? "").includes("json") ? JSON.parse(text) : text;
}

const pagePath = (slug) => `/pages/${encodeURIComponent(slug)}`;

// ---- Small helpers --------------------------------------------------------

/** Builds an element; text children are inserted as text, never as HTML. */
function h(tag, attributes = {}, ...children) {
  const element = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (value === false || value == null) continue;
    if (name.startsWith("on")) element.addEventListener(name.slice(2), value);
    else if (name === "class") element.className = value;
    else element.setAttribute(name, value === true ? "" : value);
  }
  element.append(...children.flat().filter((child) => child != null));
  return element;
}

function toast(message, { error = false } = {}) {
  const item = h("div", { class: error ? "toast error" : "toast" }, message);
  $("#toasts").append(item);
  setTimeout(() => item.remove(), error ? 6000 : 3000);
}

/** Runs `action`, reporting a failure instead of throwing it. */
async function attempt(action) {
  try {
    return await action();
  } catch (error) {
    toast(error.message, { error: true });
    return undefined;
  }
}

const relative = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });

function ago(iso) {
  const seconds = (new Date(iso) - Date.now()) / 1000;
  for (const [unit, size] of [
    ["year", 31536000],
    ["month", 2592000],
    ["day", 86400],
    ["hour", 3600],
    ["minute", 60],
  ]) {
    if (Math.abs(seconds) >= size) return relative.format(Math.round(seconds / size), unit);
  }
  return "just now";
}

function size(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(0)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/**
 * Asks something in a modal. Resolves to the field values (an empty object
 * when there are no fields), or null when cancelled.
 */
function ask({ title, message = "", fields = [], confirm = "OK", danger = false }) {
  const dialog = $("#dialog");
  $("#dialog-title").textContent = title;
  $("#dialog-message").textContent = message;
  const inputs = fields.map((field) =>
    h("input", {
      name: field.name,
      value: field.value ?? "",
      placeholder: field.placeholder,
      required: field.required,
      spellcheck: field.spellcheck,
    }),
  );
  $("#dialog-fields").replaceChildren(
    ...fields.map((field, i) => h("label", { class: "stacked" }, h("span", {}, field.label), inputs[i])),
  );
  const button = $("#dialog-confirm");
  button.textContent = confirm;
  button.className = danger ? "primary danger" : "primary";

  answerDialog(null);
  dialog.showModal();
  (inputs[0] ?? button).focus();

  return new Promise((resolve) => {
    // Answered from the submit and cancel handlers rather than the dialog's
    // `close` event, which Chrome holds back while the tab is not rendering.
    pendingAnswer = (confirmed) =>
      resolve(confirmed ? Object.fromEntries(inputs.map((input) => [input.name, input.value.trim()])) : null);
  });
}

let pendingAnswer = null;

/** Settles the open question, if any, and closes the dialog. */
function answerDialog(confirmed) {
  const answer = pendingAnswer;
  pendingAnswer = null;
  answer?.(confirmed);
  if ($("#dialog").open) $("#dialog").close();
}

// Submitting passes the form's own validation first, so required fields hold.
$("#dialog-form").addEventListener("submit", (event) => {
  event.preventDefault();
  answerDialog(true);
});
$("#dialog-cancel").addEventListener("click", () => answerDialog(false));
// Escape.
$("#dialog").addEventListener("cancel", (event) => {
  event.preventDefault();
  answerDialog(false);
});

// ---- State ----------------------------------------------------------------

const state = {
  /** Page summaries, as listed by the API. */
  pages: [],
  settings: null,
  /** The page being edited, as last saved. */
  current: null,
  /** The mounted editor, once the wasm has loaded. */
  editor: null,
  /** A revision shown in the preview instead of the draft. */
  revision: null,
  layout: readLayout(),
  saving: false,
  /** The public site, for links to its pages. */
  siteUrl: null,
};

function readLayout() {
  try {
    return localStorage.getItem("wordmaker-admin-layout") ?? "split";
  } catch {
    return "split";
  }
}

// ---- Routing --------------------------------------------------------------

const views = ["pages", "media", "settings", "dashboard"];

async function route() {
  const [, view = "pages", ...rest] = location.hash.split("/");
  const name = views.includes(view) ? view : "pages";
  for (const other of views) $(`#view-${other}`).hidden = other !== name;
  // Leaving the dashboard stops its auto-refresh; coming back loads it anew.
  scheduleRefresh();
  for (const link of document.querySelectorAll(".tabs a")) {
    if (link.dataset.view === name) link.setAttribute("aria-current", "page");
    else link.removeAttribute("aria-current");
  }

  if (name === "media") return loadMedia();
  if (name === "settings") return loadSettings();
  if (name === "dashboard") return loadDashboard();

  const slug = rest.length ? decodeURIComponent(rest.join("/")) : null;
  if (!slug) {
    // Back to the pages view: keep showing what was open.
    if (state.current) history.replaceState(null, "", `#/pages/${state.current.slug}`);
    return;
  }
  if (slug === state.current?.slug) return;
  if (state.current && isDirty()) {
    const discard = await ask({
      title: "Discard changes?",
      message: `“${state.current.title}” has unsaved changes.`,
      confirm: "Discard",
      danger: true,
    });
    if (!discard) {
      history.replaceState(null, "", `#/pages/${state.current.slug}`);
      return;
    }
  }
  await openPage(slug);
}

window.addEventListener("hashchange", () => {
  openMenu(false);
  route();
});

// ---- The phone menu -------------------------------------------------------

function openMenu(open) {
  $(".topbar").classList.toggle("menu-open", open);
  $("#menu-toggle").setAttribute("aria-expanded", String(open));
}

$("#menu-toggle").addEventListener("click", () => {
  openMenu($("#menu-toggle").getAttribute("aria-expanded") !== "true");
});
// Picking the current section again changes no hash, so close on any pick.
$("#menu").addEventListener("click", (event) => {
  if (event.target.closest("a")) openMenu(false);
});
document.addEventListener("click", (event) => {
  if (!event.target.closest(".topbar")) openMenu(false);
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && $(".topbar").classList.contains("menu-open")) {
    openMenu(false);
    $("#menu-toggle").focus();
  }
});

// ---- Page list ------------------------------------------------------------

async function loadPages() {
  const pages = await attempt(() => api("GET", "/pages"));
  if (!pages) return;
  state.pages = pages.sort((a, b) => a.title.localeCompare(b.title));
  renderPageList();
}

function renderPageList() {
  const filter = $("#page-filter").value.trim().toLowerCase();
  const pages = state.pages.filter(
    (page) => !filter || page.title.toLowerCase().includes(filter) || page.slug.includes(filter),
  );
  const home = state.settings?.home_slug;
  $("#page-list").replaceChildren(
    ...pages.map((page) =>
      h(
        "li",
        {},
        h(
          "a",
          {
            href: `#/pages/${page.slug}`,
            "aria-current": page.slug === state.current?.slug ? "page" : null,
          },
          h(
            "span",
            { class: "page-title" },
            h("span", {
              class: `dot ${page.status}${page.has_draft ? " pending" : ""}`,
              title: statusLabel(page.status === "published", page.has_draft),
            }),
            page.title,
            page.slug === home ? h("span", { class: "home" }, "· home") : null,
          ),
          h(
            "span",
            { class: "page-sub" },
            `/${page.slug} · ${page.has_draft ? "changes pending" : ago(page.updated_at)}`,
          ),
        ),
      ),
    ),
  );
  if (!pages.length) {
    $("#page-list").append(
      h("li", { class: "none" }, state.pages.length ? "No page matches." : "No pages yet."),
    );
  }
}

$("#page-filter").addEventListener("input", renderPageList);

$("#new-page").addEventListener("click", async () => {
  const answer = await ask({
    title: "New page",
    fields: [
      { name: "title", label: "Title", required: true },
      { name: "slug", label: "Slug (optional)", placeholder: "from the title", spellcheck: "false" },
    ],
    confirm: "Create",
  });
  if (!answer) return;
  const page = await attempt(() =>
    api("POST", "/pages", { title: answer.title, slug: answer.slug || undefined }),
  );
  if (!page) return;
  await loadPages();
  location.hash = `#/pages/${page.slug}`;
});

// ---- The page being edited ------------------------------------------------

const fields = {
  title: $("#page-title"),
  slug: $("#page-slug"),
  inNav: $("#page-in-nav"),
  navOrder: $("#page-nav-order"),
};

let pageRequest = 0;

async function openPage(slug) {
  // Answers can come back out of order; only the last page asked for shows.
  const request = ++pageRequest;
  const page = await attempt(() => api("GET", pagePath(slug)));
  if (request !== pageRequest) return;
  if (!page) {
    state.current = null;
    showPageEditor(false);
    renderPageList();
    return;
  }
  closeRevision();
  showPageEditor(true);
  show(page);
  renderPageList();
  if (!$("#history").hidden) loadRevisions();
  await ensureEditor();
  // Another page may have been opened while the editor loaded.
  if (request !== pageRequest) return;
  state.editor.setValue(savedContent(page).body);
  updateDirty();
  refreshPreview(0);
}

function showPageEditor(visible) {
  $("#page-editor").hidden = !visible;
  $("#no-page").hidden = visible;
  // Without a page open the list is all there is; never leave it hidden then.
  applySidebar();
}

// ---- Sidebar --------------------------------------------------------------

let sidebarHidden = false;
try {
  sidebarHidden = localStorage.getItem("wordmaker-admin-sidebar") === "hidden";
} catch {
  // Shown, then.
}

function applySidebar() {
  const hidden = sidebarHidden && !$("#page-editor").hidden;
  $("#view-pages").classList.toggle("sidebar-hidden", hidden);
  const button = $("#toggle-sidebar");
  button.setAttribute("aria-pressed", String(!hidden));
  button.title = hidden ? "Show the page list (Ctrl+\\)" : "Hide the page list (Ctrl+\\)";
}

function toggleSidebar() {
  sidebarHidden = !sidebarHidden;
  try {
    localStorage.setItem("wordmaker-admin-sidebar", sidebarHidden ? "hidden" : "shown");
  } catch {
    // Not remembered, then.
  }
  applySidebar();
  // The panes changed width; the split preview may want a new layout.
  refreshPreview(0);
}

$("#toggle-sidebar").addEventListener("click", toggleSidebar);

// ---- Drafts ---------------------------------------------------------------
//
// A published page is not changed by saving: its title and body go into a
// draft, and the site keeps the published version until "Publish changes".
// Slug and navigation are not drafted; they change at once.

const hasDraft = (page) => page.draft_body != null;

/** Whether `page` is still the one open, after waiting on the server. */
const stillOpen = (page) => state.current?.id === page.id;

/** Whether saving keeps title and body as a draft rather than changing the page. */
function savesDraft(page) {
  // Servers from before drafts send no draft fields; they change the page.
  if (!("draft_body" in page)) return false;
  return page.status === "published" || hasDraft(page);
}

/** Title and body as last saved: the draft's, when there is one. */
const savedContent = (page) => ({
  title: page.draft_title ?? page.title,
  body: page.draft_body ?? page.body,
});

function statusLabel(published, pending) {
  if (!published) return "Not published";
  return pending ? "Published · changes not yet published" : "Published";
}

/** Shows `page` as the saved state; `body` also puts it in the editor. */
function show(page, { body = false } = {}) {
  state.current = page;
  const content = savedContent(page);
  fields.title.value = content.title;
  fields.slug.value = page.slug;
  fields.inNav.checked = page.in_nav;
  fields.navOrder.value = page.nav_order;

  const published = page.status === "published";
  const pending = hasDraft(page);
  const drafts = savesDraft(page);
  const status = $("#page-status");
  status.textContent = statusLabel(published, pending);
  status.className = `badge ${pending ? "pending" : page.status}`;
  $("#save").textContent = drafts ? "Save draft" : "Save";
  $("#save").title = drafts
    ? "Keep the changes without publishing them (Ctrl+S)"
    : "Save (Ctrl+S)";
  // Before drafts, the one button published and unpublished.
  const legacy = published && !("draft_body" in page);
  $("#publish").textContent = !published ? "Publish" : legacy ? "Unpublish" : "Publish changes";
  $("#unpublish").hidden = !published || legacy;
  $("#discard-draft").hidden = !pending;

  const view = $("#page-view");
  view.href = page.url;
  // Unpublished pages are not on the site; the preview is where they show.
  view.textContent = pending ? "View published ↗" : "View ↗";
  view.hidden = !published;
  if (body && state.editor) state.editor.setValue(content.body);
  updateDirty();
}

/** The page as it stands in the form and editor. */
function draft() {
  const current = state.current;
  return {
    title: fields.title.value.trim(),
    slug: fields.slug.value.trim(),
    body: state.editor ? state.editor.value() : current.body,
    in_nav: fields.inNav.checked,
    nav_order: Number.parseInt(fields.navOrder.value, 10) || 0,
  };
}

/** The fields that differ from what was last saved. */
function changes() {
  if (!state.current) return {};
  const saved = { ...state.current, ...savedContent(state.current) };
  return Object.fromEntries(Object.entries(draft()).filter(([key, value]) => value !== saved[key]));
}

const isDirty = () => Object.keys(changes()).length > 0;

function updateDirty() {
  const label = $("#save-state");
  if (state.saving) {
    label.textContent = "Saving…";
    label.className = "save-state";
  } else if (isDirty()) {
    label.textContent = "Unsaved changes";
    label.className = "save-state dirty";
  } else if (state.current && hasDraft(state.current)) {
    label.textContent = `Draft saved ${ago(state.current.draft_updated_at)}`;
    label.className = "save-state";
  } else {
    label.textContent = state.current ? `Saved ${ago(state.current.updated_at)}` : "";
    label.className = "save-state";
  }
  // Nothing to publish on a published page without changes, and nothing
  // to send twice while a save is under way.
  const current = state.current;
  $("#save").disabled = state.saving;
  $("#publish").disabled =
    state.saving ||
    (!!current && savesDraft(current) && current.status === "published" && !hasDraft(current) && !isDirty());
}

for (const input of Object.values(fields)) {
  input.addEventListener("input", () => {
    updateDirty();
    if (input === fields.title) refreshPreview();
  });
}

/** Saves the draft; resolves to whether everything is saved now. */
async function save() {
  // A second Ctrl+S would send the same changes again, to a slug that may
  // be gone by then.
  if (state.saving) return false;
  const update = changes();
  if (!Object.keys(update).length) return true;
  if (update.title === "") {
    toast("A page needs a title.", { error: true });
    return false;
  }
  state.saving = true;
  updateDirty();
  const target = state.current;
  const before = target.slug;
  const drafting = savesDraft(target);
  const content = Object.fromEntries(
    Object.entries(update).filter(([key]) => drafting && (key === "title" || key === "body")),
  );
  const direct = Object.fromEntries(Object.entries(update).filter(([key]) => !(key in content)));

  let page = target;
  let ok = true;
  if (Object.keys(direct).length) {
    page = (await attempt(() => api("PATCH", pagePath(before), direct))) ?? page;
    ok = page !== target;
  }
  if (ok && Object.keys(content).length) {
    const drafted = await attempt(() => api("PUT", `${pagePath(page.slug)}/draft`, content));
    ok = !!drafted;
    page = drafted ?? page;
  }
  state.saving = false;
  if (!stillOpen(target)) {
    // Another page was opened meanwhile; the form is that page's now.
    updateDirty();
    if (ok) {
      loadPages();
      toast(`Saved “${page.title}”`);
    }
    return false;
  }
  // The form and editor keep what is in them: anything typed while saving,
  // or not saved, stays a change.
  const typed = {
    title: fields.title.value,
    slug: fields.slug.value,
    inNav: fields.inNav.checked,
    navOrder: fields.navOrder.value,
  };
  if (page !== state.current) show(page);
  fields.title.value = typed.title;
  fields.slug.value = typed.slug;
  fields.inNav.checked = typed.inNav;
  fields.navOrder.value = typed.navOrder;
  updateDirty();
  if (page.slug !== before) history.replaceState(null, "", `#/pages/${page.slug}`);
  if (!ok) return false;
  if (!$("#history").hidden) loadRevisions();
  loadPages();
  toast(
    Object.keys(content).length && page.status === "published"
      ? "Draft saved. The site still shows the published version."
      : "Saved",
  );
  return true;
}

$("#save").addEventListener("click", save);

/** Publishes (a published page: its draft) or, before drafts, unpublishes. */
$("#publish").addEventListener("click", async () => {
  const current = state.current;
  if (current.status === "published" && !("draft_body" in current)) return unpublish();
  if (!(await save())) return;
  const saved = state.current;
  const wasPublished = saved.status === "published";
  const page = await attempt(() => api("POST", `${pagePath(saved.slug)}/publish`));
  if (!page) return;
  if (!stillOpen(saved)) {
    loadPages();
    toast(`Published “${page.title}”`);
    return;
  }
  show(page);
  if (!$("#history").hidden) loadRevisions();
  loadPages();
  toast(wasPublished ? "Changes published" : "Published");
});

async function unpublish() {
  const page = state.current;
  const sure = await ask({
    title: `Unpublish “${page.title}”?`,
    message: "It disappears from the site until it is published again.",
    confirm: "Unpublish",
    danger: true,
  });
  if (!sure) return;
  const updated = await attempt(() => api("POST", `${pagePath(page.slug)}/unpublish`));
  if (!updated) return;
  if (stillOpen(page)) show(updated);
  loadPages();
  toast("Unpublished");
}

$("#unpublish").addEventListener("click", unpublish);

$("#discard-draft").addEventListener("click", async () => {
  const page = state.current;
  const sure = await ask({
    title: "Discard the draft?",
    message: "The editor goes back to the published version; the draft's changes are lost.",
    confirm: "Discard",
    danger: true,
  });
  if (!sure) return;
  const updated = await attempt(() => api("DELETE", `${pagePath(page.slug)}/draft`));
  if (!updated) return;
  if (stillOpen(page)) {
    show(updated, { body: true });
    refreshPreview(0);
  }
  loadPages();
  toast("Draft discarded");
});

$("#delete-page").addEventListener("click", async () => {
  const page = state.current;
  const sure = await ask({
    title: `Delete “${page.title}”?`,
    message: "The page and its whole history are removed. This cannot be undone.",
    confirm: "Delete",
    danger: true,
  });
  if (!sure) return;
  const done = await attempt(() => api("DELETE", pagePath(page.slug)));
  if (done === undefined) return;
  if (stillOpen(page)) {
    state.current = null;
    showPageEditor(false);
    history.replaceState(null, "", "#/pages");
  }
  await loadPages();
  toast(`Deleted “${page.title}”`);
});

// Ctrl+S saves and Ctrl+\ toggles the page list (Cmd on a Mac), wherever
// the focus is, including inside the editor's own frames.
function onKey(event) {
  if ((event.ctrlKey || event.metaKey) && event.key === "s") {
    event.preventDefault();
    if (state.current) save();
  } else if ((event.ctrlKey || event.metaKey) && event.key === "\\") {
    event.preventDefault();
    toggleSidebar();
  }
}

document.addEventListener("keydown", onKey);

function hookFrame(frame) {
  try {
    const target = frame.contentDocument;
    if (target && !target.wordmakerHooked) {
      target.wordmakerHooked = true;
      target.addEventListener("keydown", onKey);
    }
  } catch {
    // A frame of another origin: nothing to hook.
  }
}

// `load` does not bubble, but it can be caught on the way down.
$("#editor").addEventListener("load", (event) => {
  if (event.target instanceof HTMLIFrameElement) hookFrame(event.target);
}, true);

window.addEventListener("beforeunload", (event) => {
  if (state.current && isDirty()) event.preventDefault();
});

// ---- The editor -----------------------------------------------------------

let editorLoading;

/** Loads the wasm and mounts the editor, once. */
function ensureEditor() {
  editorLoading ??= (async () => {
    await initEditor();
    // An empty value would open the editor's sample document instead.
    const editor = mount("#editor", { value: "\n", richText: true });
    editor.onChange(() => {
      updateDirty();
      refreshPreview();
    });
    state.editor = editor;
    $("#editor-loading").hidden = true;
    for (const frame of document.querySelectorAll("#editor iframe")) hookFrame(frame);
  })().catch((error) => {
    $("#editor-loading").textContent = `The editor failed to load: ${error.message ?? error}`;
    throw error;
  });
  return editorLoading;
}

// ---- Layout and preview ---------------------------------------------------

function setLayout(layout) {
  state.layout = layout;
  $("#panes").dataset.mode = layout;
  for (const button of document.querySelectorAll(".segmented button")) {
    button.setAttribute("aria-pressed", String(button.dataset.mode === layout));
  }
  try {
    localStorage.setItem("wordmaker-admin-layout", layout);
  } catch {
    // Not remembered, then.
  }
  refreshPreview(0);
}

for (const button of document.querySelectorAll(".segmented button")) {
  button.addEventListener("click", () => setLayout(button.dataset.mode));
}

const previewVisible = () =>
  state.layout === "preview" ||
  (state.layout === "split" && !window.matchMedia("(max-width: 760px)").matches);

let previewTimer;
let previewRequest = 0;

/** Renders the draft (or the revision being looked at) after `delay` ms. */
function refreshPreview(delay = 400) {
  clearTimeout(previewTimer);
  if (!state.current || !previewVisible()) return;
  previewTimer = setTimeout(renderPreview, delay);
}

/** Set once the server turns out to predate `POST /preview`. */
let previewFallback = false;
/** Changes when the theme does, so the fallback's stylesheet is fetched anew. */
let styleVersion = Date.now();

async function previewHtml(title, slug, body) {
  if (!previewFallback) {
    try {
      return await api("POST", "/preview", { title, slug, body });
    } catch (error) {
      if (error.status !== 404) throw error;
      previewFallback = true;
      toast(
        "This server predates the full-page preview; showing the content without the site's header and footer. Update it to see the whole page.",
      );
    }
  }
  const rendered = await api("POST", "/render", { body });
  return fallbackPage(title, rendered.html);
}

const escapeHtml = (text) =>
  text.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);

/** The content part of the site's page frame, around `html`. */
function fallbackPage(title, html) {
  const language = escapeHtml(state.settings?.site_language ?? "en");
  return `<!doctype html>
<html lang="${language}"><head><meta charset="utf-8">
<link rel="stylesheet" href="/_wm/style.css?v=${styleVersion}"></head>
<body><div class="wp-site-blocks">
<main class="wp-block-group has-global-padding is-layout-constrained" style="margin-top:var(--wp--preset--spacing--60, 2.5rem)">
<div class="wp-block-group alignfull has-global-padding is-layout-constrained" style="padding-top:var(--wp--preset--spacing--60, 2.5rem);padding-bottom:var(--wp--preset--spacing--60, 2.5rem)">
<h1 class="wp-block-post-title">${escapeHtml(title)}</h1>
<div class="entry-content alignfull wp-block-post-content has-global-padding is-layout-constrained wm-adoc">
${html}
</div></div></main></div></body></html>`;
}

async function renderPreview() {
  const current = state.current;
  if (!current) return;
  const source = state.revision ?? { ...draft(), title: draft().title || current.title };
  const request = ++previewRequest;
  try {
    const html = await previewHtml(source.title, current.slug, source.body);
    // A later keystroke has already asked for a newer one.
    if (request === previewRequest) showPreview(html);
  } catch (error) {
    if (request === previewRequest) {
      showPreview(
        `<!doctype html><body style="font:14px system-ui;color:#c62f2f;padding:1rem">
         Preview failed: ${escapeHtml(error.message)}</body>`,
      );
    }
  }
}

/** Swaps the preview, keeping the reader where they were in it. */
function showPreview(html) {
  const frame = $("#preview");
  let scroll = 0;
  try {
    scroll = frame.contentWindow?.scrollY ?? 0;
  } catch {
    // Nothing to keep.
  }
  frame.addEventListener(
    "load",
    () => {
      try {
        frame.contentWindow.scrollTo(0, scroll);
      } catch {
        // Stays at the top.
      }
    },
    { once: true },
  );
  frame.srcdoc = html;
}

window.matchMedia("(max-width: 760px)").addEventListener("change", () => refreshPreview(0));

// ---- History --------------------------------------------------------------

$("#toggle-history").addEventListener("click", () => {
  const panel = $("#history");
  panel.hidden = !panel.hidden;
  $("#toggle-history").setAttribute("aria-pressed", String(!panel.hidden));
  if (panel.hidden) closeRevision();
  else loadRevisions();
});

async function loadRevisions() {
  const slug = state.current.slug;
  const revisions = await attempt(() => api("GET", `${pagePath(slug)}/revisions`));
  if (!revisions || slug !== state.current?.slug) return;
  $("#revision-list").replaceChildren(
    ...revisions.map((revision, index) =>
      h(
        "li",
        {},
        h(
          "button",
          {
            type: "button",
            "aria-pressed": String(state.revision?.revision === revision.revision),
            onclick: () => openRevision(revision.revision),
          },
          `Revision ${revision.revision}${index === 0 ? " (current)" : ""}`,
          h("small", { title: new Date(revision.created_at).toLocaleString() }, `${ago(revision.created_at)} · ${revision.title}`),
        ),
      ),
    ),
  );
}

async function openRevision(number) {
  const revision = await attempt(() => api("GET", `${pagePath(state.current.slug)}/revisions/${number}`));
  if (!revision) return;
  state.revision = revision;
  $("#revision-label").textContent = `Revision ${revision.revision}, ${new Date(revision.created_at).toLocaleString()}`;
  $("#revision-banner").hidden = false;
  if (state.layout === "edit") setLayout("split");
  else refreshPreview(0);
  loadRevisions();
}

function closeRevision() {
  if (!state.revision) return;
  state.revision = null;
  $("#revision-banner").hidden = true;
  refreshPreview(0);
  if (!$("#history").hidden) loadRevisions();
}

$("#close-revision").addEventListener("click", closeRevision);

/** Puts the revision in the editor, to be saved (and published) like any edit. */
$("#restore-revision").addEventListener("click", async () => {
  const revision = state.revision;
  if (isDirty()) {
    const discard = await ask({
      title: "Discard changes?",
      message: "Restoring replaces the unsaved changes in the editor.",
      confirm: "Restore anyway",
      danger: true,
    });
    if (!discard) return;
  }
  fields.title.value = revision.title;
  state.editor.setValue(revision.body);
  closeRevision();
  updateDirty();
  toast(
    savesDraft(state.current)
      ? `Revision ${revision.revision} is in the editor. Save the draft, then publish the changes.`
      : `Revision ${revision.revision} is in the editor. Save to keep it.`,
  );
});

// ---- Media ----------------------------------------------------------------

async function loadMedia() {
  const media = await attempt(() => api("GET", "/media"));
  if (!media) return;
  const grid = $("#media-grid");
  grid.replaceChildren(...media.map(mediaCard));
  if (!media.length) grid.append(h("li", { class: "media-empty" }, "Nothing uploaded yet."));
}

/** What to write in a page to show or link the file. */
function snippet(item) {
  const name = item.original_name.replace(/\.[^.]+$/, "").replace(/[[\]]/g, "");
  return item.content_type.startsWith("image/")
    ? `image::${item.path}[${name}]`
    : `link:${item.path}[${item.original_name.replace(/[[\]]/g, "")}]`;
}

function mediaCard(item) {
  const image = item.content_type.startsWith("image/");
  return h(
    "li",
    { class: "media-card" },
    h(
      "a",
      { class: "media-thumb", href: item.path, target: "_blank", rel: "noopener" },
      image
        ? h("img", { src: item.path, alt: item.original_name, loading: "lazy" })
        : item.original_name.split(".").pop(),
    ),
    h(
      "div",
      { class: "media-info" },
      h("strong", { title: item.original_name }, item.original_name),
      h("small", {}, `${size(item.size)} · ${ago(item.created_at)}`),
    ),
    h(
      "div",
      { class: "media-actions" },
      h("button", { type: "button", onclick: () => copySnippet(item) }, "Copy AsciiDoc"),
      h("button", { type: "button", class: "ghost danger", onclick: () => deleteMedia(item) }, "Delete"),
    ),
  );
}

async function copySnippet(item) {
  const text = snippet(item);
  try {
    await navigator.clipboard.writeText(text);
    toast(`Copied ${text}`);
  } catch {
    await ask({
      title: "Copy this into a page",
      fields: [{ name: "snippet", label: "AsciiDoc", value: text, spellcheck: "false" }],
      confirm: "Done",
    });
  }
}

async function deleteMedia(item) {
  const sure = await ask({
    title: `Delete ${item.original_name}?`,
    message: "Pages that show or link it will point at nothing.",
    confirm: "Delete",
    danger: true,
  });
  if (!sure) return;
  const done = await attempt(() => api("DELETE", `/media/${encodeURIComponent(item.filename)}`));
  if (done === undefined) return;
  toast(`Deleted ${item.original_name}`);
  loadMedia();
}

async function upload(files) {
  if (!files.length) return;
  const form = new FormData();
  for (const file of files) form.append("file", file, file.name);
  toast(files.length === 1 ? `Uploading ${files[0].name}…` : `Uploading ${files.length} files…`);
  const stored = await attempt(() => api("POST", "/media", form));
  if (!stored) return;
  toast(stored.length === 1 ? `Uploaded ${stored[0].original_name}` : `Uploaded ${stored.length} files`);
  loadMedia();
}

$("#media-input").addEventListener("change", (event) => {
  upload([...event.target.files]);
  event.target.value = "";
});

const dropzone = $("#dropzone");
for (const type of ["dragenter", "dragover"]) {
  dropzone.addEventListener(type, (event) => {
    event.preventDefault();
    dropzone.classList.add("over");
  });
}
dropzone.addEventListener("dragleave", () => dropzone.classList.remove("over"));
dropzone.addEventListener("drop", (event) => {
  event.preventDefault();
  dropzone.classList.remove("over");
  upload([...event.dataTransfer.files]);
});

// ---- Settings -------------------------------------------------------------

let themes = [];

async function loadSettings() {
  const [settings, installed] = await Promise.all([
    attempt(() => api("GET", "/settings")),
    attempt(() => api("GET", "/themes")),
    loadPages(),
  ]);
  if (settings) {
    state.settings = settings;
    const form = $("#settings-form");
    form.site_title.value = settings.site_title;
    form.site_tagline.value = settings.site_tagline ?? "";
    form.site_language.value = settings.site_language;
    form.footer_text.value = settings.footer_text ?? "";
    $("#home-slug").replaceChildren(
      h("option", { value: "" }, "List of published pages"),
      ...state.pages.map((page) =>
        h("option", { value: page.slug }, `${page.title}${page.status === "draft" ? " (not published)" : ""}`),
      ),
    );
    form.home_slug.value = settings.home_slug ?? "";
  }
  if (installed) {
    themes = installed;
    const select = $("#theme-select");
    select.replaceChildren(
      h("option", { value: "" }, "Built-in look"),
      ...themes.map((theme) =>
        h("option", { value: theme.slug }, `${theme.name}${theme.version ? ` ${theme.version}` : ""}`),
      ),
    );
    select.value = state.settings?.theme ?? "";
    renderVariations();
  }
}

function renderVariations() {
  const slug = $("#theme-select").value;
  const theme = themes.find((theme) => theme.slug === slug);
  const active = slug === state.settings?.theme ? state.settings.theme_variations : [];
  const box = $("#theme-variations");
  box.replaceChildren();
  $("#delete-theme").hidden = !theme;
  if (!theme?.variations.length) return;
  box.append(
    h("legend", {}, "Style variations"),
    ...theme.variations.map((variation) =>
      h(
        "label",
        { class: "check" },
        h("input", {
          type: "checkbox",
          name: "variation",
          value: variation.id,
          checked: active.includes(variation.id),
        }),
        variation.kind === "style" ? variation.title : `${variation.title} (${variation.kind})`,
      ),
    ),
  );
}

$("#theme-select").addEventListener("change", renderVariations);

$("#settings-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = event.target;
  const update = {
    site_title: form.site_title.value.trim(),
    site_tagline: form.site_tagline.value.trim(),
    footer_text: form.footer_text.value,
    home_slug: form.home_slug.value,
  };
  const language = form.site_language.value.trim();
  if (language) update.site_language = language;
  const settings = await attempt(() => api("PUT", "/settings", update));
  if (!settings) return;
  state.settings = settings;
  renderPageList();
  refreshPreview(0);
  toast("Settings saved");
});

$("#theme-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const theme = $("#theme-select").value || null;
  const variations = [...document.querySelectorAll("#theme-variations input:checked")].map((input) => input.value);
  const settings = await attempt(() => api("PUT", "/themes/active", { theme, variations }));
  if (!settings) return;
  state.settings = settings;
  styleVersion = Date.now();
  refreshPreview(0);
  toast(theme ? "Theme applied" : "Back to the built-in look");
});

$("#delete-theme").addEventListener("click", async () => {
  const theme = themes.find((theme) => theme.slug === $("#theme-select").value);
  if (!theme) return;
  const sure = await ask({
    title: `Remove ${theme.name}?`,
    message: "Its files are deleted. It can be installed again later.",
    confirm: "Remove",
    danger: true,
  });
  if (!sure) return;
  const done = await attempt(() => api("DELETE", `/themes/${encodeURIComponent(theme.slug)}`));
  if (done === undefined) return;
  toast(`Removed ${theme.name}`);
  loadSettings();
});

$("#install-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const form = event.target;
  const button = form.querySelector("button[type=submit]");
  const request = { wordpress_org: form.wordpress_org.value.trim() };
  if (form.version.value.trim()) request.version = form.version.value.trim();
  button.disabled = true;
  button.textContent = "Installing…";
  const theme = await attempt(() => api("POST", "/themes/install", request));
  button.disabled = false;
  button.textContent = "Install";
  if (!theme) return;
  form.reset();
  toast(`Installed ${theme.name}`);
  await loadSettings();
  $("#theme-select").value = theme.slug;
  renderVariations();
});

// ---- Dashboard ------------------------------------------------------------
//
// Page views recorded in ClickHouse, as summed up by GET /analytics. Bots are
// left out of every number except their own tile.

let dashboardDays = readDays();
let dashboardRequest = 0;
/** The report on screen, kept to redraw the chart when the view resizes. */
let report = null;

function readDays() {
  try {
    return Number(localStorage.getItem("wordmaker-admin-days")) || 30;
  } catch {
    return 30;
  }
}

const count = new Intl.NumberFormat(undefined, { notation: "compact", maximumFractionDigits: 1 });
const exact = new Intl.NumberFormat();
const dayLabel = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", timeZone: "UTC" });
const dayLong = new Intl.DateTimeFormat(undefined, {
  weekday: "short",
  month: "short",
  day: "numeric",
  year: "numeric",
  timeZone: "UTC",
});
const utcDay = (iso) => new Date(`${iso}T00:00:00Z`);

async function loadDashboard() {
  for (const button of document.querySelectorAll("#period button")) {
    button.setAttribute("aria-pressed", String(Number(button.dataset.days) === dashboardDays));
  }
  clearTimeout(refreshTimer);
  const request = ++dashboardRequest;
  const message = $("#dashboard-message");
  const status = $("#refresh-status");
  $("#refresh-now").classList.add("spinning");
  try {
    const loaded = await api("GET", `/analytics?days=${dashboardDays}`);
    if (request !== dashboardRequest) return;
    report = loaded;
    message.hidden = true;
    $("#dashboard").hidden = false;
    status.classList.remove("failed");
    status.title = "";
    status.textContent = `Updated ${clock.format(new Date())}`;
    renderDashboard();
  } catch (error) {
    if (request !== dashboardRequest) return;
    if (report?.days === dashboardDays && error.status !== 404) {
      // A refresh that failed: keep showing the last numbers.
      status.classList.add("failed");
      status.textContent = `Update failed at ${clock.format(new Date())}`;
      status.title = error.message;
    } else {
      report = null;
      status.textContent = "";
      $("#dashboard").hidden = true;
      message.hidden = false;
      message.textContent =
        error.status === 404
          ? "Analytics is off. Set WORDMAKER_CLICKHOUSE_URL on the server to start recording page views."
          : `Cannot load the numbers: ${error.message}`;
    }
  } finally {
    if (request === dashboardRequest) {
      $("#refresh-now").classList.remove("spinning");
      lastLoad = Date.now();
      scheduleRefresh();
    }
  }
}

// ---- Auto-refresh ---------------------------------------------------------
//
// Like Grafana: reload every so often, but only while someone can see the
// dashboard, so a forgotten background tab does not keep querying ClickHouse.

let refreshEvery = readRefresh();
let refreshTimer;
/** When the last load finished, in milliseconds. */
let lastLoad = 0;

const clock = new Intl.DateTimeFormat(undefined, { hour: "2-digit", minute: "2-digit", second: "2-digit" });

function readRefresh() {
  try {
    return Number(localStorage.getItem("wordmaker-admin-refresh")) || 0;
  } catch {
    return 0;
  }
}

/** Plans the next load, at once when one is overdue; or stops planning. */
function scheduleRefresh() {
  clearTimeout(refreshTimer);
  const watched = !$("#view-dashboard").hidden && document.visibilityState === "visible";
  if (!refreshEvery || !watched || $("#refresh-now").classList.contains("spinning")) return;
  const wait = Math.max(0, lastLoad + refreshEvery * 1000 - Date.now());
  refreshTimer = setTimeout(loadDashboard, wait);
}

$("#refresh-every").value = String(refreshEvery);
$("#refresh-every").addEventListener("change", (event) => {
  refreshEvery = Number(event.target.value);
  try {
    localStorage.setItem("wordmaker-admin-refresh", String(refreshEvery));
  } catch {
    // Remembering the interval is only a convenience.
  }
  scheduleRefresh();
});
$("#refresh-now").addEventListener("click", loadDashboard);
// Hidden tab: stop. Back in view: catch up at once if a refresh is due.
document.addEventListener("visibilitychange", scheduleRefresh);

for (const button of document.querySelectorAll("#period button")) {
  button.addEventListener("click", () => {
    dashboardDays = Number(button.dataset.days);
    try {
      localStorage.setItem("wordmaker-admin-days", String(dashboardDays));
    } catch {
      // Remembering the period is only a convenience.
    }
    loadDashboard();
  });
}

function renderDashboard() {
  const perDay = report.visitor_days / report.days;
  $("#stats").replaceChildren(
    stat("Page views", count.format(report.views), exact.format(report.views)),
    stat(
      "Visitors per day",
      perDay < 10 ? perDay.toFixed(1) : count.format(perDay),
      `on average over ${report.days} days`,
    ),
    stat("Bot views", count.format(report.bot_views), "not counted elsewhere; see Bots below"),
  );
  $("#chart-range").textContent = `${dayLong.format(utcDay(report.from))} – ${dayLong.format(utcDay(report.to))} (UTC)`;
  drawDaily();
  $("#daily-table").replaceChildren(
    h("thead", {}, h("tr", {}, h("th", {}, "Day"), h("th", {}, "Page views"), h("th", {}, "Visitors"))),
    h(
      "tbody",
      {},
      report.daily.map((day) =>
        h(
          "tr",
          {},
          h("td", {}, dayLong.format(utcDay(day.date))),
          h("td", {}, exact.format(day.views)),
          h("td", {}, exact.format(day.visitors)),
        ),
      ),
    ),
  );

  const pageLink = (name) => (state.siteUrl ? new URL(name, state.siteUrl).href : null);
  $("#tops").replaceChildren(
    topList("Pages", report.pages, { link: pageLink }),
    topList("Referrers", report.referrers, { link: (name) => `https://${name}`, empty: "No visits from other sites yet." }),
    topList("Countries", report.countries),
    topList("Browsers", report.browsers, { label: known }),
    topList("Operating systems", report.systems, { label: known }),
    topList("Devices", report.devices, { label: deviceName }),
    topList("Not found", report.not_found, {
      link: pageLink,
      empty: "No broken links.",
      hint: "Pages visitors asked for that do not exist.",
    }),
    topList("Bots", report.bots, {
      empty: "No bots yet.",
      hint: "Crawlers, AI scrapers and other programs, named after their user agent.",
    }),
  );
  $("#dashboard-notes").replaceChildren(...notes());
  drawMap();
}

function stat(label, value, detail) {
  return h(
    "div",
    { class: "stat" },
    h("span", { class: "stat-label" }, label),
    h("strong", { class: "stat-value" }, value),
    h("small", {}, detail),
  );
}

const known = (name) => (name === "UNKNOWN" ? "Unknown" : name);

const DEVICES = {
  pc: "Desktop",
  smartphone: "Phone",
  mobilephone: "Feature phone",
  appliance: "Console or TV",
  misc: "Other",
};
const deviceName = (name) => DEVICES[name] ?? known(name);

/** A ranked list with a bar per entry, scaled to the first. */
function topList(title, rows, { label = (name) => name, link, empty = "Nothing yet.", hint } = {}) {
  const most = Math.max(1, ...rows.map((row) => row.views));
  return h(
    "section",
    { class: "panel top" },
    h("h2", {}, title),
    hint ? h("p", { class: "hint" }, hint) : null,
    rows.length
      ? h(
          "ol",
          { class: "top-list" },
          rows.map((row) => {
            const href = link?.(row.name);
            const name = label(row.name);
            return h(
              "li",
              { style: `--share: ${row.views / most}` },
              href
                ? h("a", { class: "top-name", href, target: "_blank", rel: "noopener", title: name }, name)
                : h("span", { class: "top-name", title: name }, name),
              h("span", { class: "top-value", title: `${exact.format(row.visitors)} visitors` }, exact.format(row.views)),
            );
          }),
        )
      : h("p", { class: "hint" }, empty),
  );
}

/** How to read the numbers, and the credit the GeoIP data's license asks for. */
function notes() {
  const items = [
    "Visitors are counted per day: someone who comes back on another day counts again. No IP addresses are stored.",
  ];
  const types = report.geo_databases.join(" ");
  const credit = [];
  if (/DBIP/i.test(types)) {
    credit.push(h("a", { href: "https://db-ip.com", target: "_blank", rel: "noopener" }, "IP Geolocation by DB-IP"));
  } else if (/GeoLite2|GeoIP2/i.test(types)) {
    credit.push(
      "This product includes GeoLite2 data created by MaxMind, available from ",
      h("a", { href: "https://www.maxmind.com", target: "_blank", rel: "noopener" }, "https://www.maxmind.com"),
      ".",
    );
  } else {
    credit.push("No GeoIP database is loaded, so countries are unknown.");
  }
  return [h("span", {}, items), " ", h("span", {}, credit)];
}

// ---- The world map --------------------------------------------------------
//
// Countries shaded by page views in one hue, light to dark, on a log scale:
// one country usually dwarfs the rest. The shapes (vendor/world.js) are only
// fetched once the dashboard has numbers to show.

const SHADES = 5;
const regionName = new Intl.DisplayNames(undefined, { type: "region" });
let world;

/** The country's name in the reader's language, or its code if unknown. */
function countryName(code) {
  try {
    return regionName.of(code) ?? code;
  } catch {
    return code;
  }
}

async function drawMap() {
  const rows = report.country_codes;
  $("#map-panel").hidden = !rows.length;
  if (!rows.length) return;
  world ??= await import("/vendor/world.js");

  const box = $("#world-map");
  let map = box.querySelector("svg");
  if (!map) {
    map = svg("svg", {
      viewBox: `0 0 ${world.width} ${world.height}`,
      role: "img",
      "aria-label": "Page views by country; the table below has the numbers",
    });
    for (const [code, d] of Object.entries(world.countries)) {
      map.append(svg("path", { class: "country", d, "data-code": code }));
    }
    box.prepend(map);
  }

  const byCode = new Map(rows.map((row) => [row.name, row]));
  const most = Math.max(...rows.map((row) => row.views));
  const shade = (views) =>
    Math.max(1, Math.ceil((SHADES * Math.log1p(views)) / Math.log1p(most)));
  for (const path of map.querySelectorAll(".country")) {
    const row = byCode.get(path.dataset.code);
    path.setAttribute("class", row ? `country s${shade(row.views)}` : "country");
    const label = `${countryName(path.dataset.code)}: ${exact.format(row?.views ?? 0)} page views`;
    path.setAttribute("aria-label", label);
    if (row) path.setAttribute("tabindex", "0");
    else path.removeAttribute("tabindex");
    path.onpointerenter = path.onfocus = (event) => showMapTip(path, row, event);
    path.onpointermove = (event) => showMapTip(path, row, event);
    path.onpointerleave = path.onblur = () => ($("#map-tooltip").hidden = true);
  }

  // Lower bound of each shade, back from the log scale.
  const bound = (step) => Math.max(1, Math.ceil(Math.expm1(((step - 1) / SHADES) * Math.log1p(most))));
  $("#map-legend").replaceChildren(
    h("span", {}, exact.format(1)),
    ...Array.from({ length: SHADES }, (_, i) =>
      h("span", { class: `swatch s${i + 1}`, title: `from ${exact.format(bound(i + 1))}` }),
    ),
    h("span", {}, exact.format(most)),
  );
  $("#country-table").replaceChildren(
    h("thead", {}, h("tr", {}, h("th", {}, "Country"), h("th", {}, "Page views"), h("th", {}, "Visitors"))),
    h(
      "tbody",
      {},
      rows.map((row) =>
        h(
          "tr",
          {},
          h("td", {}, countryName(row.name)),
          h("td", {}, exact.format(row.views)),
          h("td", {}, exact.format(row.visitors)),
        ),
      ),
    ),
  );
}

function showMapTip(path, row, event) {
  const tip = $("#map-tooltip");
  tip.replaceChildren(
    h("strong", {}, countryName(path.dataset.code)),
    h("span", {}, row ? `${exact.format(row.views)} page views` : "No page views"),
    row ? h("span", {}, `${exact.format(row.visitors)} visitors`) : null,
  );
  tip.hidden = false;
  const box = $("#world-map").getBoundingClientRect();
  // At the pointer, or above the country's middle when reached by keyboard.
  const shape = path.getBoundingClientRect();
  const x = (event.clientX || shape.left + shape.width / 2) - box.left;
  const y = (event.clientY || shape.top + shape.height / 2) - box.top;
  tip.style.left = `${Math.min(Math.max(x - tip.offsetWidth / 2, 0), box.width - tip.offsetWidth)}px`;
  tip.style.top = `${Math.max(0, y - tip.offsetHeight - 12)}px`;
}

// ---- The daily chart ------------------------------------------------------

const SVG = "http://www.w3.org/2000/svg";

function svg(tag, attributes = {}, ...children) {
  const element = document.createElementNS(SVG, tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (name.startsWith("on")) element.addEventListener(name.slice(2), value);
    else element.setAttribute(name, value);
  }
  element.append(...children);
  return element;
}

/** 0 and evenly spaced round numbers covering `max`, at most five steps. */
function ticks(max) {
  let step = 10 ** Math.floor(Math.log10(Math.max(max, 1) / 4));
  for (const factor of [1, 2, 5, 10]) {
    if (max / (step * factor) <= 4) {
      step *= factor;
      break;
    }
  }
  step = Math.max(1, step);
  const values = [];
  for (let value = 0; value < max + step; value += step) values.push(value);
  return values;
}

function drawDaily() {
  const box = $("#daily-chart");
  box.querySelector("svg")?.remove();
  if (!report || !box.clientWidth) return;
  const daily = report.daily;
  const width = box.clientWidth;
  const height = 220;
  const pad = { top: 12, right: 4, bottom: 24, left: 40 };
  const plotWidth = width - pad.left - pad.right;
  const plotHeight = height - pad.top - pad.bottom;
  const scale = ticks(Math.max(...daily.map((day) => day.views)));
  const top = scale.at(-1);
  const y = (value) => pad.top + plotHeight - (value / top) * plotHeight;
  const slot = plotWidth / daily.length;
  const barWidth = Math.max(1, Math.min(24, slot - 2));
  const base = y(0);

  const chart = svg("svg", {
    width,
    height,
    viewBox: `0 0 ${width} ${height}`,
    role: "img",
    "aria-label": `Page views per day, ${report.days} days; the table below has the numbers`,
  });
  for (const value of scale) {
    chart.append(
      svg("line", { class: "grid", x1: pad.left, x2: width - pad.right, y1: y(value), y2: y(value) }),
      svg("text", { class: "axis", x: pad.left - 6, y: y(value), "text-anchor": "end", "dominant-baseline": "middle" }, count.format(value)),
    );
  }
  // A label every few days, so they never crowd: at most about one per 70px.
  const every = Math.max(1, Math.ceil(daily.length / Math.max(1, Math.floor(plotWidth / 70))));
  daily.forEach((day, i) => {
    const center = pad.left + slot * (i + 0.5);
    const x = center - barWidth / 2;
    const barTop = y(day.views);
    const radius = Math.min(4, barWidth / 2, base - barTop);
    if (day.views > 0) {
      chart.append(
        svg("path", {
          class: "bar",
          d: `M${x},${base}V${barTop + radius}Q${x},${barTop} ${x + radius},${barTop}H${x + barWidth - radius}Q${x + barWidth},${barTop} ${x + barWidth},${barTop + radius}V${base}Z`,
        }),
      );
    }
    if ((daily.length - 1 - i) % every === 0) {
      // The last label ends at the plot's edge instead of running past it.
      const last = i === daily.length - 1;
      chart.append(
        svg(
          "text",
          {
            class: "axis",
            x: last ? Math.min(center + 16, width - pad.right) : center,
            y: height - 6,
            "text-anchor": last ? "end" : "middle",
          },
          dayLabel.format(utcDay(day.date)),
        ),
      );
    }
    // The whole column answers to the pointer, not just the bar.
    chart.append(
      svg("rect", {
        class: "hit",
        x: pad.left + slot * i,
        y: pad.top,
        width: slot,
        height: plotHeight,
        tabindex: 0,
        "aria-label": `${dayLong.format(utcDay(day.date))}: ${exact.format(day.views)} page views`,
        onpointerenter: () => showTip(day, center, barTop),
        onfocus: () => showTip(day, center, barTop),
        onpointerleave: hideTip,
        onblur: hideTip,
      }),
    );
  });
  chart.append(svg("line", { class: "baseline", x1: pad.left, x2: width - pad.right, y1: base, y2: base }));
  box.prepend(chart);
}

function showTip(day, x, barTop) {
  const tip = $("#chart-tooltip");
  tip.replaceChildren(
    h("strong", {}, dayLong.format(utcDay(day.date))),
    h("span", {}, `${exact.format(day.views)} page views`),
    h("span", {}, `${exact.format(day.visitors)} visitors`),
  );
  tip.hidden = false;
  const box = $("#daily-chart").clientWidth;
  const left = Math.min(Math.max(x - tip.offsetWidth / 2, 0), box - tip.offsetWidth);
  tip.style.left = `${left}px`;
  tip.style.top = `${Math.max(0, barTop - tip.offsetHeight - 8)}px`;
}

function hideTip() {
  $("#chart-tooltip").hidden = true;
}

new ResizeObserver(() => {
  if (!$("#view-dashboard").hidden) drawDaily();
}).observe($("#daily-chart"));

// ---- Signing out ----------------------------------------------------------

$("#sign-out").addEventListener("click", async () => {
  const signedOut = await attempt(async () => {
    // Like API calls, signing out needs the guard header.
    const response = await fetch("/auth/logout", {
      method: "POST",
      headers: { "x-wordmaker-admin": "1" },
    });
    if (!response.ok) throw new Error(`cannot sign out: ${response.status}`);
    return true;
  });
  if (signedOut) location.assign("/auth/signed-out");
});

// ---- Start ----------------------------------------------------------------

async function start() {
  setLayout(state.layout);
  applySidebar();
  const config = await attempt(async () => (await fetch("/config.json")).json());
  if (config) {
    $("#site-link").href = config.site_url;
    state.siteUrl = config.site_url;
    if (config.user) {
      $("#account-user").textContent = config.user;
      $("#account").hidden = false;
    }
  }
  state.settings = await attempt(() => api("GET", "/settings"));
  await loadPages();
  // Fetch the wasm while the first page loads, not after.
  ensureEditor().catch(() => {});
  await route();
}

start();
