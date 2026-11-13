// Meshware — User Guide behaviour: contents with scroll-spy and search,
// copy buttons, prompt and comment colouring, reading progress.
// The guide reads in full without this file.
(() => {
  "use strict";
  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => [...r.querySelectorAll(s)];
  const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const esc = (t) => t.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

  /* nav */
  const nav = $("#nav"), menu = $("#nav-menu");
  const setOpen = (o) => { o ? nav.setAttribute("data-open", "") : nav.removeAttribute("data-open"); menu?.setAttribute("aria-expanded", String(o)); };
  menu?.addEventListener("click", () => setOpen(!nav.hasAttribute("data-open")));
  addEventListener("scroll", () => nav.toggleAttribute("data-scrolled", scrollY > 8), { passive: true });

  /* code: dim prompts and comments, add copy buttons that copy commands only */
  $$(".g-code code").forEach((code) => {
    const raw = code.textContent.replace(/\n$/, "");
    const lines = raw.split("\n");
    const shell = lines.some((l) => l.startsWith("$ "));
    const lang = code.parentElement.dataset.lang;
    code.innerHTML = lines.map((l) => {
      if (l.startsWith("$ ")) {
        const at = commentAt(l.slice(2));
        const cmd = at < 0 ? l.slice(2) : l.slice(2, 2 + at);
        return `<span class="p">$ </span>${esc(cmd)}${at < 0 ? "" : `<span class="c">${esc(l.slice(2 + at))}</span>`}`;
      }
      if (/^\s*#/.test(l) && (shell || lang === "toml" || lang === "yaml")) return `<span class="c">${esc(l)}</span>`;
      return shell ? `<span class="o">${esc(l)}</span>` : esc(l);
    }).join("\n");
    const copyText = shell ? lines.filter((l) => l.startsWith("$ ")).map((l) => { const at = commentAt(l.slice(2)); return (at < 0 ? l.slice(2) : l.slice(2, 2 + at)).trimEnd(); }).join("\n") : raw;
    const b = document.createElement("button");
    b.type = "button"; b.className = "copy"; b.textContent = "Copy";
    b.setAttribute("aria-label", "Copy to clipboard");
    b.addEventListener("click", async () => {
      try { await navigator.clipboard.writeText(copyText); }
      catch {
        const ta = Object.assign(document.createElement("textarea"), { value: copyText });
        ta.style.cssText = "position:fixed;opacity:0"; document.body.appendChild(ta); ta.select();
        try { document.execCommand("copy"); } catch {} ta.remove();
      }
      b.textContent = "Copied"; b.setAttribute("data-done", "");
      setTimeout(() => { b.textContent = "Copy"; b.removeAttribute("data-done"); }, 1600);
    });
    code.parentElement.appendChild(b);
  });
  // a trailing shell comment: " # ..." outside quotes
  function commentAt(s) {
    let q = null;
    for (let i = 0; i < s.length; i++) {
      const c = s[i];
      if (q) { if (c === "\\") i++; else if (c === q) q = null; }
      else if (c === "'" || c === '"') q = c;
      else if (c === "#" && i > 0 && /\s/.test(s[i - 1])) return i - 1;
    }
    return -1;
  }

  /* tables scroll inside a frame on narrow screens */
  $$(".g-table").forEach((t) => {
    if (t.parentElement.classList.contains("g-table-wrap")) return;
    const w = document.createElement("div"); w.className = "g-table-wrap";
    t.replaceWith(w); w.appendChild(t);
  });

  /* contents, built from the headings */
  const toc = $("#g-toc"), list = $("#g-toc-list");
  const secs = $$(".g-sec");
  const links = new Map();
  secs.forEach((sec) => {
    const h2 = $("h2", sec);
    const li = document.createElement("li");
    const a = document.createElement("a");
    a.href = "#" + sec.id; a.textContent = h2.textContent; li.appendChild(a);
    links.set(sec.id, a);
    const subs = $$("h3[id]", sec);
    if (subs.length) {
      const ol = document.createElement("ol"); ol.className = "sub";
      subs.forEach((h3) => {
        const sli = document.createElement("li"), sa = document.createElement("a");
        sa.href = "#" + h3.id; sa.textContent = h3.textContent; sli.appendChild(sa); ol.appendChild(sli);
        links.set(h3.id, sa);
      });
      li.appendChild(ol);
    }
    list.appendChild(li);
  });

  /* scroll-spy */
  const targets = [...secs, ...$$(".g-sec h3[id]")];
  let current = null;
  const spy = () => {
    const line = innerHeight * 0.28;
    let best = null;
    for (const t of targets) { const top = t.getBoundingClientRect().top; if (top <= line) { if (!best || top > best.top) best = { el: t, top }; } }
    const id = best ? best.el.id : secs[0]?.id;
    if (id === current) return;
    current = id;
    links.forEach((a) => a.classList.remove("is-current"));
    $$("#g-toc-list > li").forEach((li) => li.classList.remove("is-open"));
    const a = links.get(id);
    if (a) {
      a.classList.add("is-current");
      const top = a.closest("#g-toc-list > li");
      top?.classList.add("is-open");
      top?.firstElementChild?.classList.add("is-current");
    }
  };

  /* reading progress and back to top */
  const bar = $(".progress i"), up = $(".to-top");
  const onScroll = () => {
    const h = document.documentElement.scrollHeight - innerHeight;
    if (bar) bar.style.transform = `scaleX(${h > 0 ? scrollY / h : 0})`;
    up?.classList.toggle("is-shown", scrollY > innerHeight * 1.5);
    spy();
  };
  addEventListener("scroll", () => requestAnimationFrame(onScroll), { passive: true });
  onScroll();
  up?.addEventListener("click", () => scrollTo({ top: 0, behavior: reduce ? "auto" : "smooth" }));

  /* search the contents (and the text under each heading) */
  const input = $("#g-search");
  const index = targets.map((t) => {
    let text = t.textContent;
    if (t.tagName === "H3") { let n = t.nextElementSibling; while (n && n.tagName !== "H3") { text += " " + n.textContent; n = n.nextElementSibling; } }
    else text = $("h2", t).textContent + " " + ($(".g-intro", t)?.textContent || "");
    return { id: t.id, text: text.toLowerCase() };
  });
  const empty = $("#g-empty");
  input?.addEventListener("input", () => {
    const q = input.value.trim().toLowerCase();
    if (!q) {
      toc.removeAttribute("data-searching");
      $$("#g-toc-list li").forEach((li) => li.classList.remove("hide"));
      empty.hidden = true; current = null; spy(); return;
    }
    toc.setAttribute("data-searching", "");
    const words = q.split(/\s+/);
    const hit = new Set(index.filter((e) => words.every((w) => e.text.includes(w))).map((e) => e.id));
    let any = false;
    $$("#g-toc-list > li").forEach((li) => {
      const secId = li.firstElementChild.getAttribute("href").slice(1);
      let show = hit.has(secId);
      $$(".sub li", li).forEach((sli) => {
        const on = hit.has(sli.firstElementChild.getAttribute("href").slice(1));
        sli.classList.toggle("hide", !on); if (on) show = true;
      });
      li.classList.toggle("hide", !show); if (show) any = true;
    });
    empty.hidden = any;
  });
  input?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") { const a = $("#g-toc-list li:not(.hide) a"); if (a) { a.click(); input.blur(); } }
    if (e.key === "Escape") { input.value = ""; input.dispatchEvent(new Event("input")); }
  });
  addEventListener("keydown", (e) => {
    if (e.key === "/" && document.activeElement !== input && !/INPUT|TEXTAREA/.test(document.activeElement?.tagName || "")) {
      e.preventDefault(); if (matchMedia("(max-width: 960px)").matches) openToc(true); input?.focus();
    }
  });

  /* contents drawer on narrow screens */
  const btn = $("#g-toc-btn");
  const openToc = (o) => { o ? toc.setAttribute("data-open", "") : toc.removeAttribute("data-open"); btn?.setAttribute("aria-expanded", String(o)); document.body.style.overflow = o ? "hidden" : ""; };
  btn?.addEventListener("click", () => openToc(!toc.hasAttribute("data-open")));
  list.addEventListener("click", (e) => { if (e.target.closest("a") && matchMedia("(max-width: 960px)").matches) openToc(false); });
  addEventListener("keydown", (e) => { if (e.key === "Escape") { openToc(false); setOpen(false); } });
})();
