// Jarvis Dashboard browser glue (spec D9, D12, §5.4). Everything else is
// server-rendered HTML driven by htmx attributes. This file only handles
// shortcuts, pane switching, turn_id minting, SSE filtering and reloads.
(function () {
  "use strict";
  var body = document.body;
  var phone = window.matchMedia("(max-width: 767px)");

  // Swap error fragments too: they retarget themselves to the toast. A 401
  // means the Access session expired; reloading goes through the login.
  htmx.config.responseHandling = [
    { code: "204", swap: false },
    { code: "401", swap: false, error: true },
    { code: "[23]..", swap: true },
    { code: "[45]..", swap: true, error: true },
  ];
  body.addEventListener("htmx:responseError", function (e) {
    if (e.detail.xhr && e.detail.xhr.status === 401) location.reload();
  });

  // UUIDv7 (D12): 48-bit ms timestamp, version 7, RFC 4122 variant, random rest.
  function uuidv7() {
    var b = new Uint8Array(16);
    crypto.getRandomValues(b);
    var ms = Date.now();
    for (var i = 5; i >= 0; i--) { b[i] = ms % 256; ms = Math.floor(ms / 256); }
    b[6] = (b[6] & 0x0f) | 0x70;
    b[8] = (b[8] & 0x3f) | 0x80;
    var h = Array.prototype.map.call(b, function (x) { return (x + 256).toString(16).slice(1); }).join("");
    return h.slice(0, 8) + "-" + h.slice(8, 12) + "-" + h.slice(12, 16) + "-" + h.slice(16, 20) + "-" + h.slice(20);
  }
  window.jarvis = { uuidv7: uuidv7 };

  // Keep the layout the height of the visual viewport, so the bottom of the
  // app (composer, bottom bar) stays above the on-screen keyboard.
  var vv = window.visualViewport;
  function fit() {
    document.documentElement.style.setProperty("--app-h", (vv ? vv.height : window.innerHeight) + "px");
    if (vv && vv.offsetTop) window.scrollTo(0, 0);
  }
  if (vv) { vv.addEventListener("resize", fit); vv.addEventListener("scroll", fit); }
  fit();

  // Panes (phones): one at a time; hidden panes are inert but stay mounted.
  function showPane(name) {
    body.dataset.pane = name;
    document.querySelectorAll(".j-pane").forEach(function (p) {
      p.inert = phone.matches && p.dataset.paneName !== name;
    });
    document.querySelectorAll("[data-pane-target]").forEach(function (b) {
      if (b.dataset.paneTarget === name) b.setAttribute("aria-current", "page");
      else b.removeAttribute("aria-current");
    });
  }
  phone.addEventListener("change", function () { showPane(body.dataset.pane); });
  showPane(body.dataset.pane);

  function railVisible() {
    var rail = document.querySelector(".j-rail");
    return rail && getComputedStyle(rail).display !== "none";
  }
  try { if (body.dataset.rail === "auto" && localStorage.getItem("jarvis.rail")) body.dataset.rail = localStorage.getItem("jarvis.rail"); } catch (e) {}

  // Live updates: refetch the sidebar and the open thread on every SSE (re)open,
  // the open thread on a matching `changed`, and reload after 3 errors in a row.
  var sseErrors = 0;
  function refresh(selector) {
    document.querySelectorAll(selector).forEach(function (el) { htmx.trigger(el, "jarvis:refresh"); });
  }
  body.addEventListener("htmx:sseOpen", function () { sseErrors = 0; refresh("#threads, #log"); });
  body.addEventListener("htmx:sseError", function () { if (++sseErrors >= 3) location.reload(); });
  body.addEventListener("sse:changed", function (e) {
    var id = null;
    try { id = JSON.parse(e.detail.data).thread_id; } catch (err) { return; }
    if (id && id === body.dataset.thread) refresh("#log");
  });

  // A created thread opens at its pushed URL.
  body.addEventListener("htmx:afterRequest", function (e) {
    var xhr = e.detail.xhr;
    if (e.detail.elt.id === "new-thread" && e.detail.successful && xhr.getResponseHeader("HX-Push-Url")) {
      location.assign(xhr.getResponseHeader("HX-Push-Url"));
    }
    // A submitted dialog form (rename) closes its dialog on success.
    var dialog = e.detail.elt.tagName === "FORM" && e.detail.elt.closest("dialog");
    if (dialog && e.detail.successful) dialog.close();
  });

  // Thread search filters the loaded list; reapplied after each refetch.
  var search = document.getElementById("thread-search");
  function filterThreads() {
    var q = search.value.trim().toLowerCase();
    var shown = 0;
    document.querySelectorAll("#threads .j-group").forEach(function (g) {
      var any = false;
      g.querySelectorAll(".j-thread").forEach(function (a) {
        var hit = !q || a.dataset.title.indexOf(q) >= 0;
        a.hidden = !hit;
        any = any || hit;
      });
      g.hidden = !any;
      if (any) shown++;
    });
    var none = document.querySelector("#threads .j-no-match");
    if (none) none.hidden = !q || shown > 0;
  }
  search.addEventListener("input", filterThreads);
  body.addEventListener("htmx:afterSwap", function (e) {
    if (e.detail.target.id === "threads" || document.getElementById("threads") === e.detail.elt) filterThreads();
    if (e.detail.target.id === "j-toast") {
      clearTimeout(filterThreads.toast);
      filterThreads.toast = setTimeout(function () { e.detail.target.innerHTML = ""; }, 6000);
    }
  });

  // Command palette.
  var palette = document.getElementById("palette");
  var paletteInput = palette.querySelector("input");
  function openPalette() {
    if (palette.open) return;
    paletteInput.value = "";
    palette.showModal();
    paletteInput.focus();
    htmx.trigger(paletteInput, "jarvis:open");
  }
  function options() { return Array.prototype.slice.call(palette.querySelectorAll("[role=option]")); }
  function move(delta) {
    var opts = options();
    if (!opts.length) return;
    var i = opts.findIndex(function (o) { return o.classList.contains("is-active"); });
    opts.forEach(function (o) { o.classList.remove("is-active"); });
    var next = opts[(i + delta + opts.length) % opts.length];
    next.classList.add("is-active");
    next.scrollIntoView({ block: "nearest" });
  }
  paletteInput.addEventListener("keydown", function (e) {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); move(e.key === "ArrowDown" ? 1 : -1); }
    if (e.key === "Enter") {
      var active = palette.querySelector(".is-active");
      if (active) { e.preventDefault(); active.click(); }
    }
  });
  palette.addEventListener("click", function (e) { if (e.target === palette) palette.close(); });

  function newThread() {
    if (palette.open) palette.close();
    document.getElementById("new-thread").click();
  }

  // Clicks on data-action buttons, pane tabs, and closing open menus.
  document.addEventListener("click", function (e) {
    var el = e.target.closest("[data-action], [data-pane-target]");
    document.querySelectorAll("details.j-menu[open]").forEach(function (d) {
      if (!d.contains(e.target) || (el && d.contains(el))) d.open = false;
    });
    if (!el) return;
    if (el.dataset.paneTarget) return showPane(el.dataset.paneTarget);
    switch (el.dataset.action) {
      case "palette": return openPalette();
      case "new-thread": return newThread();
      case "toggle-rail":
        body.dataset.rail = railVisible() ? "closed" : "open";
        try { localStorage.setItem("jarvis.rail", body.dataset.rail); } catch (err) {}
        return;
      case "rename": {
        var d = document.getElementById("rename-dialog");
        if (d) { d.showModal(); d.querySelector("input").select(); }
        return;
      }
      case "close-dialog": return el.closest("dialog").close();
      case "dismiss-toast": document.getElementById("j-toast").innerHTML = ""; return;
    }
  });

  document.addEventListener("keydown", function (e) {
    var mod = e.metaKey || e.ctrlKey;
    if (mod && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "k") { e.preventDefault(); openPalette(); }
    else if (mod && !e.shiftKey && !e.altKey && e.key.toLowerCase() === "n") { e.preventDefault(); newThread(); }
  });
})();
