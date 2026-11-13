/* Meshware documentation: search, theme, the drawer, "on this page",
   copy buttons, and — in the single-file build — which page is showing.
   No dependencies; works from a file:// URL. */
(function () {
  "use strict";
  var doc = document.documentElement;
  var SINGLE = document.body.hasAttribute("data-single");

  function store(key, value) {
    try {
      if (value === undefined) return window.localStorage.getItem(key);
      if (value === null) window.localStorage.removeItem(key);
      else window.localStorage.setItem(key, value);
    } catch (e) { /* storage unavailable: the page works without it */ }
    return null;
  }

  /* ---------- theme ---------- */
  var saved = store("mw-docs-theme");
  if (saved === "light" || saved === "dark") doc.setAttribute("data-theme", saved);
  function currentTheme() {
    var set = doc.getAttribute("data-theme");
    if (set) return set;
    return window.matchMedia && window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
  }
  var themeBtn = document.getElementById("themeToggle");
  function labelTheme() {
    if (!themeBtn) return;
    var next = currentTheme() === "dark" ? "light" : "dark";
    themeBtn.setAttribute("aria-label", "Switch to " + next + " theme");
    themeBtn.setAttribute("title", "Switch to " + next + " theme");
  }
  if (themeBtn) {
    labelTheme();
    themeBtn.addEventListener("click", function () {
      var next = currentTheme() === "dark" ? "light" : "dark";
      doc.setAttribute("data-theme", next);
      store("mw-docs-theme", next);
      labelTheme();
    });
  }

  /* ---------- the drawer, on narrow screens ---------- */
  var menuBtn = document.getElementById("menuToggle");
  var scrim = null;
  function closeNav() {
    document.body.classList.remove("is-nav-open");
    if (menuBtn) menuBtn.setAttribute("aria-expanded", "false");
    if (scrim) { scrim.remove(); scrim = null; }
  }
  if (menuBtn) {
    menuBtn.addEventListener("click", function () {
      var open = !document.body.classList.contains("is-nav-open");
      if (!open) { closeNav(); return; }
      document.body.classList.add("is-nav-open");
      menuBtn.setAttribute("aria-expanded", "true");
      scrim = document.createElement("div");
      scrim.className = "scrim";
      scrim.addEventListener("click", closeNav);
      document.body.appendChild(scrim);
    });
  }
  document.addEventListener("keydown", function (e) { if (e.key === "Escape") closeNav(); });

  /* ---------- copy buttons ---------- */
  document.addEventListener("click", function (e) {
    var btn = e.target.closest && e.target.closest(".code__copy");
    if (!btn) return;
    var pre = btn.closest(".code").querySelector("pre");
    var text = pre.innerText;
    // A console block copies the commands, not the prompt or the output.
    if (btn.closest(".code").getAttribute("data-lang") === "console") {
      var cmds = text.split("\n").filter(function (l) { return /^\$ /.test(l); })
        .map(function (l) { return l.replace(/^\$ /, "").replace(/\s+#\s.*$/, ""); });
      if (cmds.length) text = cmds.join("\n");
    }
    var done = function () {
      btn.textContent = "Copied";
      btn.classList.add("is-done");
      setTimeout(function () { btn.textContent = "Copy"; btn.classList.remove("is-done"); }, 1600);
    };
    if (navigator.clipboard && navigator.clipboard.writeText) {
      navigator.clipboard.writeText(text).then(done, function () { fallbackCopy(text); done(); });
    } else { fallbackCopy(text); done(); }
  });
  function fallbackCopy(text) {
    var ta = document.createElement("textarea");
    ta.value = text; ta.setAttribute("readonly", ""); ta.style.position = "fixed"; ta.style.opacity = "0";
    document.body.appendChild(ta); ta.select();
    try { document.execCommand("copy"); } catch (e) { /* nothing more to try */ }
    ta.remove();
  }

  /* ---------- on this page ---------- */
  var observer = null;
  function watchToc(root) {
    if (observer) { observer.disconnect(); observer = null; }
    var toc = root.querySelector(".toc");
    if (!toc || !("IntersectionObserver" in window)) return;
    var links = Array.prototype.slice.call(toc.querySelectorAll("a"));
    var byId = {};
    links.forEach(function (a) { byId[decodeURIComponent(a.getAttribute("href").slice(1))] = a; });
    var visible = {};
    observer = new IntersectionObserver(function (entries) {
      entries.forEach(function (en) { visible[en.target.id] = en.isIntersecting; });
      var first = null;
      links.forEach(function (a) {
        var id = decodeURIComponent(a.getAttribute("href").slice(1));
        if (!first && visible[id]) first = a;
      });
      if (first) {
        links.forEach(function (a) { a.classList.toggle("is-on", a === first); });
      }
    }, { rootMargin: "-70px 0px -60% 0px" });
    Object.keys(byId).forEach(function (id) {
      var el = document.getElementById(id);
      if (el) observer.observe(el);
    });
  }

  /* ---------- the single-file build: one page showing at a time ---------- */
  var pages = SINGLE ? Array.prototype.slice.call(document.querySelectorAll("[data-page]")) : [];
  function show(page, target) {
    pages.forEach(function (p) { p.hidden = p !== page; });
    var slug = page.getAttribute("data-page");
    document.querySelectorAll(".side a").forEach(function (a) {
      if (a.getAttribute("data-slug") === slug) a.setAttribute("aria-current", "page");
      else a.removeAttribute("aria-current");
    });
    document.querySelectorAll(".side__group").forEach(function (g) {
      g.classList.toggle("is-current", !!g.querySelector('a[data-slug="' + slug + '"]'));
    });
    var home = slug === "home";
    document.querySelector(".main").classList.toggle("home", home);
    document.title = page.getAttribute("data-title");
    watchToc(page);
    if (target && target !== page) target.scrollIntoView();
    else window.scrollTo(0, 0);
  }
  function route() {
    var id = decodeURIComponent(location.hash.slice(1));
    var el = id ? document.getElementById(id) : null;
    var page = el ? el.closest("[data-page]") : null;
    show(page || pages[0], el);
    closeNav();
  }
  if (SINGLE) {
    window.addEventListener("hashchange", route);
    route();
  } else {
    watchToc(document);
  }

  /* ---------- search ---------- */
  var INDEX = window.MW_SEARCH || [];
  var BASE = document.body.getAttribute("data-base") || "";
  function words(s) { return s.toLowerCase().split(/[^a-z0-9_.\-]+/).filter(Boolean); }
  function esc(s) { return s.replace(/[&<>"]/g, function (c) { return { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]; }); }
  function mark(text, terms) {
    var out = esc(text);
    terms.forEach(function (t) {
      if (t.length < 2) return;
      var re = new RegExp("(" + t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&") + ")", "ig");
      out = out.replace(re, "<mark>$1</mark>");
    });
    return out;
  }
  function score(entry, terms) {
    var total = 0;
    for (var i = 0; i < terms.length; i++) {
      var t = terms[i], s = 0;
      if (entry.tl.indexOf(t) >= 0) s += entry.h ? 6 : 10;
      if (entry.hl.indexOf(t) >= 0) s += 8;
      if (entry.xl.indexOf(t) >= 0) s += 2;
      if (!s) return 0;
      total += s;
    }
    if (!entry.h) total += 1;
    return total;
  }
  INDEX.forEach(function (e) { e.tl = e.t.toLowerCase(); e.hl = (e.h || "").toLowerCase(); e.xl = e.x.toLowerCase(); });

  function bindSearch(box) {
    var input = box.querySelector(".search__input");
    var list = box.querySelector(".search__results");
    var on = -1;
    function hits() { return Array.prototype.slice.call(list.querySelectorAll(".search__hit")); }
    function highlight(n) {
      var hs = hits();
      if (!hs.length) return;
      on = (n + hs.length) % hs.length;
      hs.forEach(function (h, i) { h.classList.toggle("is-on", i === on); });
      hs[on].scrollIntoView({ block: "nearest" });
    }
    function run() {
      var q = input.value.trim();
      on = -1;
      if (!q) { list.hidden = true; list.innerHTML = ""; return; }
      var terms = words(q);
      var found = INDEX.map(function (e) { return { e: e, s: score(e, terms) }; })
        .filter(function (r) { return r.s > 0; })
        .sort(function (a, b) { return b.s - a.s; })
        .slice(0, 12);
      if (!found.length) {
        list.innerHTML = '<div class="search__empty">Nothing matches “' + esc(q) + '”.</div>';
      } else {
        list.innerHTML = "<ol>" + found.map(function (r) {
          var e = r.e;
          return '<li class="search__hit"><a href="' + BASE + e.u + '"><span>' + esc(e.g) + "</span><b>" +
            mark(e.h ? e.t + " › " + e.h : e.t, terms) + "</b><p>" + mark(e.x, terms) + "</p></a></li>";
        }).join("") + "</ol>";
      }
      list.hidden = false;
    }
    input.addEventListener("input", run);
    input.addEventListener("focus", function () { if (input.value.trim()) run(); });
    input.addEventListener("keydown", function (e) {
      if (e.key === "ArrowDown") { e.preventDefault(); highlight(on + 1); }
      else if (e.key === "ArrowUp") { e.preventDefault(); highlight(on - 1); }
      else if (e.key === "Enter") {
        var hs = hits();
        var pick = hs[on >= 0 ? on : 0];
        if (pick) { e.preventDefault(); pick.querySelector("a").click(); }
      } else if (e.key === "Escape") { input.value = ""; run(); input.blur(); }
    });
    list.addEventListener("click", function (e) {
      if (e.target.closest("a")) { input.value = ""; list.hidden = true; input.blur(); }
    });
    document.addEventListener("click", function (e) { if (!box.contains(e.target)) list.hidden = true; });
  }
  document.querySelectorAll("[data-search]").forEach(bindSearch);
  document.addEventListener("keydown", function (e) {
    if (e.key === "/" && !/^(INPUT|TEXTAREA)$/.test(document.activeElement.tagName)) {
      var inputs = Array.prototype.slice.call(document.querySelectorAll(".search__input"))
        .filter(function (i) { return i.offsetParent !== null; });
      var input = inputs[inputs.length - 1];
      if (input) { e.preventDefault(); input.focus(); }
    }
  });
})();
