// Meshware — home page behaviour.
// The page is complete without this file. With it: the live mesh, and — when
// GSAP and ScrollTrigger load — the scroll choreography. Reduced motion gets
// every final state and no movement.
(() => {
  "use strict";
  const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;
  const $ = (s, r = document) => r.querySelector(s);
  const $$ = (s, r = document) => [...r.querySelectorAll(s)];
  const hasGsap = () => typeof window.gsap !== "undefined" && typeof window.ScrollTrigger !== "undefined";

  /* ── nav ───────────────────────────────────────── */
  const nav = $("#nav"), menu = $("#nav-menu");
  const setOpen = (open) => {
    open ? nav.setAttribute("data-open", "") : nav.removeAttribute("data-open");
    menu.setAttribute("aria-expanded", String(open));
  };
  menu?.addEventListener("click", () => setOpen(!nav.hasAttribute("data-open")));
  $$("#nav-links a").forEach((a) => a.addEventListener("click", () => setOpen(false)));
  addEventListener("keydown", (e) => { if (e.key === "Escape") setOpen(false); });
  const onScroll = () => nav.toggleAttribute("data-scrolled", scrollY > 8);
  addEventListener("scroll", onScroll, { passive: true }); onScroll();

  /* ── tabs ──────────────────────────────────────── */
  const tabs = $$('[role="tab"]');
  const select = (tab, focus) => {
    tabs.forEach((t) => {
      const on = t === tab;
      t.setAttribute("aria-selected", String(on)); t.tabIndex = on ? 0 : -1;
      document.getElementById(t.getAttribute("aria-controls")).hidden = !on;
    });
    if (focus) tab.focus();
  };
  tabs.forEach((t, i) => {
    t.addEventListener("click", () => select(t));
    t.addEventListener("keydown", (e) => {
      const d = e.key === "ArrowRight" ? 1 : e.key === "ArrowLeft" ? -1 : 0;
      if (d) { e.preventDefault(); select(tabs[(i + d + tabs.length) % tabs.length], true); }
    });
  });

  /* ── copy ──────────────────────────────────────── */
  $$(".copy").forEach((b) => b.addEventListener("click", async () => {
    const text = b.dataset.copy;
    try { await navigator.clipboard.writeText(text); }
    catch {
      const ta = Object.assign(document.createElement("textarea"), { value: text });
      ta.style.cssText = "position:fixed;opacity:0"; document.body.appendChild(ta); ta.select();
      try { document.execCommand("copy"); } catch {} ta.remove();
    }
    b.textContent = "Copied"; b.setAttribute("data-done", "");
    setTimeout(() => { b.textContent = "Copy"; b.removeAttribute("data-done"); }, 1800);
  }));

  /* ── Director screenshots: dark or light ──────── */
  const themeBtns = $$(".theme-switch button");
  themeBtns.forEach((b) => b.addEventListener("click", () => {
    const theme = b.dataset.theme;
    themeBtns.forEach((x) => x.setAttribute("aria-pressed", String(x === b)));
    $$(".shot img").forEach((img) => {
      const next = img.dataset[theme];
      if (!next || img.getAttribute("src") === next) return;
      const pre = new Image();
      pre.onload = () => {
        img.classList.add("is-swapping");
        setTimeout(() => { img.src = next; img.classList.remove("is-swapping"); }, reduce ? 0 : 200);
      };
      pre.src = next;
    });
  }));

  /* ── the mesh ──────────────────────────────────── */
  const mesh = heroMesh($("#mesh"));

  function heroMesh(cv) {
    if (!cv) return null;
    const ctx = cv.getContext("2d");
    const state = { zoom: 1, fade: 1 };
    let W = 0, H = 0, dpr = 1, R = 0, cx = 0, cy = 0, textEdge = { x: 0, y: 0 }, quiet = false;
    // labels stay clear of the headline and copy, bottom-left
    const clear = (x, y) => !quiet && x < W - 150 && y > 70 && (x > textEdge.x || y < textEdge.y - 40);
    let ang = 0.6, tilt = 0.38, px = 0, py = 0, tx = 0, ty = 0;
    let last = performance.now(), running = true, nextJoin = 2600, nextPulse = 0;
    const TEAL = "60,200,183", AMBER = "240,178,84";

    // Points on a flattened sphere (Fibonacci lattice, jittered): the mesh as
    // a body of equals, with no centre.
    const N = 104, VISIBLE = 88;
    let seed = 11;
    const rnd = () => ((seed = (seed * 16807) % 2147483647) / 2147483647);
    const names = ["forge", "anvil", "edge-7", "lab-2", "gpu-a", "build-1", "rack-3", "hq", "edge-12", "gpu-b"];
    const cores = [32, 16, 4, 16, 24, 32, 64, 8, 4, 24];
    const nodes = [];
    for (let i = 0; i < N; i++) {
      const y = 1 - (i / (N - 1)) * 2, r = Math.sqrt(1 - y * y), th = i * 2.399963;
      nodes.push({
        x: Math.cos(th) * r + (rnd() - .5) * .12, y: y * .62 + (rnd() - .5) * .08, z: Math.sin(th) * r + (rnd() - .5) * .12,
        on: i < VISIBLE ? 1 : 0, join: i < VISIBLE ? 1 : 0, flash: 0, sx: 0, sy: 0, d: 0, hub: -1, load: Math.round(8 + rnd() * 70),
      });
    }
    // shuffle which points start visible, so new machines appear all over
    for (let i = N - 1; i > 0; i--) { const j = Math.floor(rnd() * (i + 1)); [nodes[i].on, nodes[j].on] = [nodes[j].on, nodes[i].on]; }
    nodes.forEach((n) => (n.join = n.on));
    names.forEach((nm, k) => { const n = nodes[Math.floor((k + .5) * N / names.length)]; n.hub = k; n.on = n.join = 1; });
    // each node links to its three nearest neighbours
    const edges = [];
    nodes.forEach((a, i) => {
      nodes.map((b, j) => ({ j, d: (a.x - b.x) ** 2 + (a.y - b.y) ** 2 + (a.z - b.z) ** 2 }))
        .filter((o) => o.j !== i).sort((p, q) => p.d - q.d).slice(0, 3)
        .forEach(({ j }) => { if (!edges.some((e) => (e.a === j && e.b === i) || (e.a === i && e.b === j))) edges.push({ a: i, b: j }); });
    });
    let pulses = [];
    let joining = null;

    function resize() {
      dpr = Math.min(devicePixelRatio || 1, 2);
      const r = cv.getBoundingClientRect();
      W = r.width; H = r.height;
      cv.width = Math.round(W * dpr); cv.height = Math.round(H * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      const narrow = W < 760;
      R = Math.min(W, H) * (narrow ? .6 : .6);
      cx = W * (narrow ? .55 : .7); cy = H * (narrow ? .3 : .4);
      textEdge = { x: W * .5, y: H * .5 };
      quiet = narrow; // on phones the copy sits over the mesh: no labels, lower contrast
    }

    function project() {
      const z0 = 3.1 / state.zoom, F = 2.2;
      const ca = Math.cos(ang + tx * .35), sa = Math.sin(ang + tx * .35);
      const t = tilt + ty * .12, ct = Math.cos(t), st = Math.sin(t);
      for (const n of nodes) {
        const x1 = n.x * ca - n.z * sa, z1 = n.x * sa + n.z * ca;
        const y2 = n.y * ct - z1 * st, z2 = n.y * st + z1 * ct;
        const p = F / (z2 + z0);
        n.sx = cx + x1 * p * R; n.sy = cy + y2 * p * R;
        n.d = Math.max(0, Math.min(1, (1 - z2) / 2)); // 1 = nearest
        n.p = p;
      }
    }

    function draw(dt, now) {
      ctx.clearRect(0, 0, W, H);
      ctx.globalAlpha = state.fade * (quiet ? .55 : 1);
      // links
      ctx.lineWidth = 1;
      for (const e of edges) {
        const a = nodes[e.a], b = nodes[e.b];
        const v = Math.min(a.join, b.join);
        if (v <= 0) continue;
        const d = (a.d + b.d) / 2;
        ctx.strokeStyle = `rgba(${TEAL},${(0.05 + 0.34 * d * d) * v})`;
        ctx.beginPath(); ctx.moveTo(a.sx, a.sy); ctx.lineTo(b.sx, b.sy); ctx.stroke();
      }
      // work in flight: a short bright trail along the link
      for (const q of pulses) {
        const a = nodes[q.a], b = nodes[q.b];
        const h = Math.min(1, q.p), t0 = Math.max(0, h - .22);
        const hx = a.sx + (b.sx - a.sx) * h, hy = a.sy + (b.sy - a.sy) * h;
        const sx = a.sx + (b.sx - a.sx) * t0, sy = a.sy + (b.sy - a.sy) * t0;
        const d = (a.d + b.d) / 2;
        const g = ctx.createLinearGradient(sx, sy, hx, hy);
        g.addColorStop(0, `rgba(${TEAL},0)`); g.addColorStop(1, `rgba(${TEAL},${.35 + .6 * d})`);
        ctx.strokeStyle = g; ctx.lineWidth = 1 + d * 1.2;
        ctx.beginPath(); ctx.moveTo(sx, sy); ctx.lineTo(hx, hy); ctx.stroke();
        ctx.fillStyle = `rgba(233,255,251,${.5 + .5 * d})`;
        ctx.beginPath(); ctx.arc(hx, hy, .8 + d * 1.4, 0, 6.283); ctx.fill();
      }
      ctx.lineWidth = 1;
      // machines, far to near
      const order = nodes.map((n, i) => i).sort((i, j) => nodes[i].d - nodes[j].d);
      for (const i of order) {
        const n = nodes[i];
        if (n.join <= 0) continue;
        const d = n.d, r = .9 + 2.4 * d * d * n.join;
        const lum = Math.round(70 + 165 * d);
        ctx.fillStyle = `rgba(${lum - 10},${lum + 8},${lum + 2},${(.45 + .55 * d) * n.join})`;
        ctx.beginPath(); ctx.arc(n.sx, n.sy, r, 0, 6.283); ctx.fill();
        if (n.flash > .02) {
          ctx.strokeStyle = `rgba(${TEAL},${n.flash * .7 * d})`;
          ctx.beginPath(); ctx.arc(n.sx, n.sy, r + 3 + (1 - n.flash) * 8, 0, 6.283); ctx.stroke();
        }
        if (n.hub >= 0 && d > .45 && clear(n.sx, n.sy)) {
          const a = Math.min(1, (d - .45) * 3) * n.join;
          ctx.strokeStyle = `rgba(${TEAL},${.45 * a})`;
          ctx.beginPath(); ctx.arc(n.sx, n.sy, r + 4.5, 0, 6.283); ctx.stroke();
          ctx.font = "500 10.5px 'Geist Mono', ui-monospace, monospace";
          ctx.fillStyle = `rgba(233,242,240,${.85 * a})`;
          ctx.fillText(names[n.hub], n.sx + r + 9, n.sy - 2);
          ctx.fillStyle = `rgba(150,160,166,${.8 * a})`;
          ctx.fillText(`${cores[n.hub]} cores · ${n.load}%`, n.sx + r + 9, n.sy + 11);
        }
      }
      // a machine pairing in
      if (joining) {
        const n = nodes[joining.i], t = joining.t;
        const a = t < .15 ? t / .15 : t > .85 ? (1 - t) / .15 : 1;
        ctx.strokeStyle = `rgba(${AMBER},${.7 * a})`;
        ctx.beginPath(); ctx.arc(n.sx, n.sy, 6 + ((now / 900) % 1) * 12, 0, 6.283); ctx.stroke();
        ctx.fillStyle = `rgba(${AMBER},${a})`;
        ctx.beginPath(); ctx.arc(n.sx, n.sy, 2.6, 0, 6.283); ctx.fill();
        ctx.font = "500 10.5px 'Geist Mono', ui-monospace, monospace";
        ctx.fillStyle = `rgba(240,178,84,${.95 * a})`;
        if (clear(n.sx, n.sy)) ctx.fillText(t < .6 ? `pairing · ${joining.code}` : "joined", n.sx + 12, n.sy + 4);
      }
      ctx.globalAlpha = 1;
    }

    function step(now) {
      if (!running) return;
      const dt = Math.min(48, now - last); last = now;
      ang += dt * .000055;
      tx += (px - tx) * .04; ty += (py - ty) * .04;
      // pulses
      nextPulse -= dt;
      if (nextPulse <= 0 && pulses.length < 14) {
        const live = edges.filter((e) => nodes[e.a].join >= 1 && nodes[e.b].join >= 1);
        const e = live[(Math.random() * live.length) | 0];
        if (e) pulses.push(Math.random() < .5 ? { a: e.a, b: e.b, p: 0, s: .0006 + Math.random() * .0006 } : { a: e.b, b: e.a, p: 0, s: .0006 + Math.random() * .0006 });
        nextPulse = 120 + Math.random() * 260;
      }
      pulses = pulses.filter((q) => {
        q.p += q.s * dt;
        if (q.p >= 1) { const n = nodes[q.b]; n.flash = 1; n.load = Math.min(96, n.load + 3); return false; }
        return true;
      });
      nodes.forEach((n) => { n.flash *= .95; if (Math.random() < .002) n.load = Math.max(4, n.load - 5); });
      // a new machine every few seconds, until the lattice is full
      nextJoin -= dt;
      if (!joining && nextJoin <= 0) {
        const free = nodes.map((n, i) => i).filter((i) => nodes[i].on === 0);
        if (free.length) {
          const i = free[(Math.random() * free.length) | 0];
          nodes[i].on = 1;
          joining = { i, t: 0, code: String(100000 + Math.floor(Math.random() * 899999)).replace(/(\d{3})(\d{3})/, "$1 $2") };
        }
        nextJoin = 4200;
      }
      if (joining) {
        joining.t += dt / 2600;
        nodes[joining.i].join = Math.min(1, joining.t * 1.6);
        if (joining.t >= 1) { nodes[joining.i].join = 1; joining = null; }
      }
      project(); draw(dt, now);
      requestAnimationFrame(step);
    }

    function still() { project(); draw(0, 0); }

    resize();
    let rT; addEventListener("resize", () => { clearTimeout(rT); rT = setTimeout(() => { resize(); if (reduce) still(); }, 120); });
    $(".hero")?.addEventListener("pointermove", (e) => {
      px = (e.clientX / innerWidth - .5); py = (e.clientY / innerHeight - .5);
    });
    if (reduce) { still(); return state; }
    new IntersectionObserver((es) => {
      const on = es[0].isIntersecting;
      if (on && !running) { running = true; last = performance.now(); requestAnimationFrame(step); }
      else if (!on) running = false;
    }).observe(cv);
    requestAnimationFrame(step);
    // the readout under the headline follows the mesh
    setInterval(() => {
      const m = $("#ro-nodes"), j = $("#ro-jobs");
      if (m) m.textContent = String(nodes.filter((n) => n.join >= 1).length);
      if (j) j.textContent = String(pulses.length);
    }, 500);
    state.redraw = still;
    return state;
  }

  /* ── spread: 400 items, two machines ───────────── */
  const spread = makeSpread();

  function makeSpread() {
    const grid = $("#spread-grid");
    if (!grid) return null;
    const N = 400, cells = [];
    for (let i = 0; i < N; i++) { const s = document.createElement("span"); grid.appendChild(s); cells.push(s); }
    const btn = $("#spread-run"), freeze = $("#spread-freeze"), clock = $("#spread-time"), note = $("#spread-note");
    let raf = 0, played = false;

    // Paced to the measured totals: 2.8 s with both machines, 6.8 s when anvil
    // freezes halfway and forge re-runs what it was holding.
    function schedule(frozen) {
      const ev = [], healthy = 2.8 / N;
      if (!frozen) {
        for (let k = 0; k < N; k++) ev.push({ i: k, end: (k + 1) * healthy, who: k % 2 ? "b" : "a" });
        return { ev, total: 2.8 };
      }
      const cut = 200;
      for (let k = 0; k < cut; k++) ev.push({ i: k, end: (k + 1) * healthy, who: k % 2 ? "b" : "a" });
      const stranded = [];
      for (let k = cut; k < cut + 16; k += 2) stranded.push(k + 1);
      const rest = [];
      for (let k = cut; k < N; k++) if (!stranded.includes(k)) rest.push(k);
      const t0 = 1.4, t1 = 6.3, t2 = 6.8;
      rest.forEach((k, n) => ev.push({ i: k, end: t0 + (n + 1) * (t1 - t0) / rest.length, who: "a" }));
      stranded.forEach((k, n) => ev.push({ i: k, end: t1 + (n + 1) * (t2 - t1) / stranded.length, who: "a" }));
      return { ev, total: 6.8, strandedAt: t0, stranded };
    }

    function run() {
      played = true;
      cancelAnimationFrame(raf);
      cells.forEach((c) => (c.className = ""));
      const frozen = freeze.checked, plan = schedule(frozen);
      const byEnd = plan.ev.slice().sort((x, y) => x.end - y.end);
      btn.disabled = true;
      const start = performance.now();
      let n = 0;
      const tick = (now) => {
        const t = (now - start) / 1000;
        clock.textContent = Math.min(t, plan.total).toFixed(1);
        while (n < byEnd.length && byEnd[n].end <= t) { const e = byEnd[n++]; cells[e.i].className = e.who; }
        for (let k = n; k < Math.min(byEnd.length, n + 8); k++) { const e = byEnd[k]; if (!cells[e.i].className) cells[e.i].className = "run"; }
        if (plan.stranded && t >= plan.strandedAt) plan.stranded.forEach((i) => { if (cells[i].className !== "a") cells[i].className = "redo"; });
        if (n < byEnd.length) { raf = requestAnimationFrame(tick); return; }
        clock.textContent = plan.total.toFixed(1);
        btn.disabled = false; btn.textContent = "Run again";
        note.innerHTML = frozen
          ? "anvil stopped responding halfway. forge reassigned its in-flight tasks and completed all 400 in <strong>6.8 s</strong>, matching the measured result. Without reassignment, the same run took <strong>31.6 s</strong>."
          : "Both nodes pulled work as capacity freed up, completing all 400 tasks in <strong>2.8 s</strong>, the measured result on two 16-core machines.";
      };
      raf = requestAnimationFrame(tick);
    }
    btn.addEventListener("click", run);
    freeze.addEventListener("change", () => { btn.textContent = "Run 400 tasks"; });
    if (reduce) { cells.forEach((c, i) => (c.className = i % 2 ? "b" : "a")); clock.textContent = "2.8"; }
    return { run, playOnce: () => { if (!played && !reduce) run(); } };
  }

  /* ── without GSAP (or with reduced motion) ─────── */
  function fallback() {
    if (!spread) return;
    const io = new IntersectionObserver((es) => { if (es.some((e) => e.isIntersecting)) { io.disconnect(); spread.playOnce(); } }, { threshold: .5 });
    io.observe($("#spread-grid"));
  }

  /* ── with GSAP: the choreography ───────────────── */
  function splitWords(el) {
    // wrap every word in a clipping span, keeping inline elements (.dim, code)
    const walk = (node) => {
      [...node.childNodes].forEach((c) => {
        if (c.nodeType === 3) {
          const frag = document.createDocumentFragment();
          c.textContent.split(/(\s+)/).forEach((part) => {
            if (!part) return;
            if (/^\s+$/.test(part)) { frag.appendChild(document.createTextNode(part)); return; }
            const o = document.createElement("span"); o.className = "wd";
            const i = document.createElement("span"); i.textContent = part; o.appendChild(i); frag.appendChild(o);
          });
          node.replaceChild(frag, c);
        } else if (c.nodeType === 1) walk(c);
      });
    };
    walk(el);
    return $$(".wd > span", el);
  }

  function choreograph() {
    const { gsap, ScrollTrigger } = window;
    gsap.registerPlugin(ScrollTrigger);
    const style = document.createElement("style");
    style.textContent = ".wd{display:inline-block;overflow:hidden;vertical-align:top;padding-bottom:.08em;margin-bottom:-.08em}.wd>span{display:inline-block}";
    document.head.appendChild(style);
    const mm = gsap.matchMedia();

    // hero: the headline rises line by line, then everything settles in
    const intro = gsap.timeline({ defaults: { ease: "expo.out" } });
    intro.from(".hero-title .line > span", { yPercent: 108, duration: 1.5, stagger: .12 })
      .from(".hero-lead", { y: 24, opacity: 0, filter: "blur(6px)", duration: 1.2 }, "-=1.05")
      .from(".hero-actions > *", { y: 18, opacity: 0, duration: 1, stagger: .08 }, "-=1")
      .from(".hero-readout > div", { y: 14, opacity: 0, duration: 1, stagger: .06 }, "-=.9")
      .from("#mesh", { opacity: 0, scale: 1.06, duration: 2.4, ease: "power2.out" }, 0);
    // scrolling away: the mesh drifts closer and the copy lifts off
    if (mesh) {
      ScrollTrigger.create({
        trigger: ".hero", start: "top top", end: "bottom top", scrub: true,
        onUpdate: (s) => { mesh.zoom = 1 + s.progress * .9; mesh.fade = 1 - s.progress * .85; },
      });
    }
    gsap.to(".hero-inner", { y: -90, opacity: 0, ease: "none", scrollTrigger: { trigger: ".hero", start: "top top", end: "bottom 25%", scrub: true } });

    // headings: words rise into place
    $$("[data-split]").forEach((el) => {
      const words = splitWords(el);
      gsap.from(words, { yPercent: 110, duration: 1.3, ease: "expo.out", stagger: .035, scrollTrigger: { trigger: el, start: "top 85%" } });
    });
    $$("[data-fade]").forEach((el) => gsap.from(el, { y: 30, opacity: 0, duration: 1.2, ease: "expo.out", scrollTrigger: { trigger: el, start: "top 88%" } }));

    // Director rises out of the page and lays flat as you arrive
    gsap.fromTo(".rise-window",
      { rotateX: 28, scale: .8, y: 60, opacity: .35 },
      { rotateX: 0, scale: 1, y: 0, opacity: 1, ease: "none", scrollTrigger: { trigger: "#rise", start: "top 95%", end: "top 18%", scrub: .6 } });

    // measured: cards arrive, numbers count up
    gsap.from(".stat", { y: 40, opacity: 0, duration: 1.1, ease: "expo.out", stagger: .09, scrollTrigger: { trigger: ".stats", start: "top 82%" } });
    // The figures are shown as measured from the first frame; only their
    // arrival moves. A counter ticking through 2.7 and 48 would show numbers
    // nobody measured under a heading that says "measured".
    gsap.from(".stat-n", { yPercent: 40, opacity: 0, filter: "blur(8px)", duration: 1.4, ease: "expo.out", stagger: .1, scrollTrigger: { trigger: ".stats", start: "top 80%" } });

    // how it works: pinned, one step per screen of scroll
    const steps = $$(".step"), panels = $$(".stage-card");
    const setStep = (k) => {
      steps.forEach((s, i) => s.classList.toggle("is-active", i === k));
      panels.forEach((p, i) => p.classList.toggle("is-active", i === k));
      if (k === 3) spread?.playOnce();
    };
    mm.add("(min-width: 961px)", () => {
      const how = $(".how");
      how.setAttribute("data-pinned", "");
      setStep(0);
      // Held for a screen and a half, not four: long enough to read each
      // step, short enough that scrolling on never feels like a wall.
      const bar = $(".steps-progress i");
      const st = ScrollTrigger.create({
        trigger: how, start: "top top", end: () => "+=" + Math.round(innerHeight * 1.5), pin: true, anticipatePin: 1,
        onUpdate: (s) => {
          setStep(Math.min(3, Math.floor(s.progress * 4)));
          if (bar) bar.style.transform = `scaleY(${s.progress})`;
        },
      });
      // a step is also a link to its own state
      const jump = (k) => scrollTo({ top: st.start + (st.end - st.start) * ((k + .5) / 4), behavior: "smooth" });
      steps.forEach((s, k) => {
        s.tabIndex = 0;
        s.onclick = () => jump(k);
        s.onkeydown = (e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); jump(k); } };
      });
      return () => {
        st.kill(); how.removeAttribute("data-pinned");
        steps.forEach((s) => { s.onclick = s.onkeydown = null; s.removeAttribute("tabindex"); });
      };
    });
    mm.add("(max-width: 960px)", () => {
      $$(".stage-card").forEach((p) => p.classList.add("is-active"));
      const st = ScrollTrigger.create({ trigger: "#spread-grid", start: "top 75%", once: true, onEnter: () => spread?.playOnce() });
      return () => st.kill();
    });

    // the statement lights up word by word as you read it
    const sw = (() => {
      const el = $("[data-words]");
      const words = el.textContent.trim().split(/\s+/);
      el.innerHTML = words.map((w) => `<span class="w">${w}</span>`).join(" ");
      return $$(".w", el);
    })();
    gsap.fromTo(sw, { opacity: .2 }, { opacity: 1, ease: "none", stagger: .1, scrollTrigger: { trigger: ".statement-text", start: "top 78%", end: "bottom 45%", scrub: true } });
    gsap.from(".oh-bar i", { scaleX: 0, duration: 1.4, ease: "expo.out", stagger: .12, scrollTrigger: { trigger: ".overhead", start: "top 80%" } });
    gsap.from(".oh-zero", { scale: 0, duration: .8, ease: "back.out(3)", delay: .5, scrollTrigger: { trigger: ".overhead", start: "top 80%" } });

    // capabilities
    gsap.from(".cap", { y: 36, opacity: 0, duration: 1, ease: "expo.out", stagger: { each: .07, grid: "auto" }, scrollTrigger: { trigger: ".cap-grid", start: "top 82%" } });

    // Director: the screens travel sideways while the page is held
    mm.add("(min-width: 901px)", () => {
      const sec = $(".director"), track = $("#dir-track");
      sec.setAttribute("data-pinned", "");
      const dist = () => track.scrollWidth - innerWidth;
      const tw = gsap.to(track, {
        x: () => -dist(), ease: "none",
        scrollTrigger: { trigger: sec, start: "top top", end: () => "+=" + Math.round(dist() * .6), pin: true, scrub: .5, invalidateOnRefresh: true, anticipatePin: 1 },
      });
      return () => { tw.scrollTrigger?.kill(); tw.kill(); gsap.set(track, { x: 0 }); sec.removeAttribute("data-pinned"); };
    });
    gsap.from(".shot", { y: 50, opacity: 0, duration: 1.2, ease: "expo.out", stagger: .08, scrollTrigger: { trigger: "#dir-track", start: "top 85%" } });

    // yours, install
    gsap.from(".row", { y: 24, opacity: 0, duration: 1, ease: "expo.out", stagger: .08, scrollTrigger: { trigger: ".rows", start: "top 85%" } });

    // the closing wordmark rises with the last of the page
    gsap.from(".footer-mark", { yPercent: 45, ease: "none", scrollTrigger: { trigger: ".footer", start: "top bottom", end: "bottom bottom", scrub: true } });
    gsap.from(".footer-cta > *", { y: 30, opacity: 0, duration: 1.1, ease: "expo.out", stagger: .1, scrollTrigger: { trigger: ".footer", start: "top 80%" } });

    addEventListener("load", () => ScrollTrigger.refresh());
  }

  /* ── start ─────────────────────────────────────── */
  const start = () => { if (hasGsap() && !reduce) choreograph(); else fallback(); };
  if (document.readyState === "complete" || hasGsap()) start();
  else addEventListener("DOMContentLoaded", start, { once: true });
})();
