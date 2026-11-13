<script lang="ts">
  // The first-run wave — a mesh, drawn as one.
  //
  // The only decorative element on the only screen every customer sees
  // exactly once, at the exact moment they decide whether the thing they
  // paid for looks like what they paid for. A regular grid of dots, bent by
  // travelling swells and seen nearly edge-on, so the eye recovers a
  // surface from how the rows bunch and splay.
  //
  // **Canvas 2D, on purpose (2026-09).** This was a WebGL point cloud with
  // a CSS mask over it, and in the shipped webview it stuttered the moment
  // the pointer moved over the window and went blank when the window was
  // resized. Both are the accelerated-canvas path in WebKitGTK on Wayland:
  // a masked GL layer is re-composited through a slow path on every hover
  // repaint, and a resize reallocates the drawing buffer. It also went
  // dark on its own for seconds at a time because the height field was
  // value noise alone (see `height()`). The 2D canvas is the most-used
  // drawing path in every webview and has none of those failure modes:
  // there is no context to lose, a resize is a redraw, and the vignette
  // that used to be a CSS mask is an alpha term per dot. The lattice, the
  // camera, the swells and the colour ramp are the same picture; only the
  // renderer changed. Fewer points (~6.6k against 25k) at 30 frames a
  // second — a background does not need 120 — and it drops to a lighter
  // grid on its own if a frame ever runs long, so a machine that cannot
  // keep up gets a slower wave, never a broken one.
  //
  // The colours come from --wave-trough / --wave-crest in app.css — on
  // light the crest is the *darker* value, on dark the brighter, so it is
  // the one that reads against the canvas in either theme.
  import { theme } from "../theme.svelte";
  import { motion } from "../motion";

  let {
    /**
     * A form is open, so the decoration gets out of the way: the field
     * drops to 55% and runs at half speed. The moment somebody has
     * something to type, this stops being the thing on screen.
     */
    quiet = false,
  }: { quiet?: boolean } = $props();

  /** The tuned constants. */
  const WAVE = {
    /** field */
    alpha: 1.0,
    size: 4.2,
    cols: 220,
    rows: 30,
    crest: 1.9,
    /** the ribbon: how wide across, how soft its edge, how flat it enters */
    width: 0.32,
    band: 2.2,
    taper: 0.45,
    /** height -> luminosity contrast */
    gain: 2.4,
    /** swell 1 — along the ribbon: amplitude, wavenumber, radians/second */
    amp1: 1.0,
    freq1: 0.62,
    spd1: 0.42,
    /** swell 2 — across it, run backwards so the crests interfere */
    amp2: 0.7,
    freq2: 0.95,
    spd2: 0.55,
    /** the roughening noise on top of both */
    amp3: 0.9,
    /** camera */
    tilt: 19,
    dist: 13,
    yaw: 11,
    horizon: 0.35,
    drift: 5,
    /** the clear area around the card, in CSS px, and the fade in from the top */
    ringW: 560,
    ringH: 600,
    clear: 0.28,
    topFade: 0.06,
    /** frames per second; a background does not need more */
    fps: 30,
  } as const;

  /** Half the ribbon's length in world units; its depth is `width` of this. */
  const HALF = 16;
  const FOV = (30 * Math.PI) / 180;
  /** The still frame reduced motion lands on — picked for having structure. */
  const STILL = 30;
  /** Colour buckets in the sprite ramp. */
  const RAMP = 16;

  // -- the surface ----------------------------------------------------------

  const fract = (v: number) => v - Math.floor(v);
  const smooth = (a: number, b: number, x: number) => {
    const t = Math.min(1, Math.max(0, (x - a) / (b - a)));
    return t * t * (3 - 2 * t);
  };
  const random = (x: number, y: number) => fract(Math.sin(x * 12.9898 + y * 78.233) * 43758.5453123);
  /** Value noise — the same function the reference shader used. */
  function noise(x: number, y: number): number {
    const ix = Math.floor(x), iy = Math.floor(y);
    const fx = x - ix, fy = y - iy;
    const a = random(ix, iy), b = random(ix + 1, iy), c = random(ix, iy + 1), d = random(ix + 1, iy + 1);
    const ux = fx * fx * (3 - 2 * fx), uy = fy * fy * (3 - 2 * fy);
    return a + (b - a) * ux + (c - a) * uy * (1 - ux) + (d - b) * ux * uy;
  }

  /**
   * Two travelling swells and one layer of noise. The swells are what keep
   * the field ALIVE: a sine has a crest every wavelength, so whatever the
   * moment, some part of the ribbon is near its brightest. The first build
   * was noise alone, and value noise at this frequency has three or four
   * cells across the whole strip — as the pattern translated with time the
   * entire visible field slid into one trough together, every dot dimmed at
   * once, and for seconds at a stretch the wave was simply not there. The
   * noise now only roughens the swells; it no longer decides whether
   * anything is visible.
   */
  function height(px: number, pz: number, t: number): number {
    let h = Math.sin((px * 0.989 + pz * 0.148) * WAVE.freq1 + t * WAVE.spd1) * WAVE.amp1;
    h += Math.sin((px * 0.33 + pz * 0.944) * WAVE.freq2 - t * WAVE.spd2) * WAVE.amp2;
    h += (noise(px * 0.24 + t * 0.1, pz * 0.24 + t * 0.1) - 0.5) * WAVE.amp3;
    // the second noise layer, rotated π/4 and run backwards
    const rx = (pz - px) * 0.70710678, ry = (pz + px) * 0.70710678;
    h += (noise(rx * 0.55 - t * 0.16, ry * 0.55 - t * 0.16) - 0.5) * WAVE.amp3 * 0.45;
    return h;
  }
  const SPAN = (WAVE.amp1 + WAVE.amp2 + WAVE.amp3) * 1.45;

  // -- the camera (4x4, column-major) -----------------------------------------

  const m4 = {
    mul(o: Float32Array, a: Float32Array, b: Float32Array) {
      for (let c = 0; c < 4; c++) {
        const b0 = b[c * 4], b1 = b[c * 4 + 1], b2 = b[c * 4 + 2], b3 = b[c * 4 + 3];
        o[c * 4] = a[0] * b0 + a[4] * b1 + a[8] * b2 + a[12] * b3;
        o[c * 4 + 1] = a[1] * b0 + a[5] * b1 + a[9] * b2 + a[13] * b3;
        o[c * 4 + 2] = a[2] * b0 + a[6] * b1 + a[10] * b2 + a[14] * b3;
        o[c * 4 + 3] = a[3] * b0 + a[7] * b1 + a[11] * b2 + a[15] * b3;
      }
    },
    perspective(o: Float32Array, fovy: number, aspect: number, near: number, far: number) {
      const f = 1 / Math.tan(fovy / 2), nf = 1 / (near - far);
      o.fill(0);
      o[0] = f / aspect; o[5] = f; o[10] = (far + near) * nf; o[11] = -1; o[14] = 2 * far * near * nf;
    },
    translation(o: Float32Array, x: number, y: number, z: number) {
      o.set([1, 0, 0, 0, 0, 1, 0, 0, 0, 0, 1, 0, x, y, z, 1]);
    },
    rotX(o: Float32Array, r: number) {
      const c = Math.cos(r), s = Math.sin(r);
      o.set([1, 0, 0, 0, 0, c, s, 0, 0, -s, c, 0, 0, 0, 0, 1]);
    },
    rotY(o: Float32Array, r: number) {
      const c = Math.cos(r), s = Math.sin(r);
      o.set([c, 0, -s, 0, 0, 1, 0, 0, s, 0, c, 0, 0, 0, 0, 1]);
    },
  };

  // -- the renderer -----------------------------------------------------------

  type Handle = {
    resize(): void;
    repalette(): void;
    /** Draw one frame at wave-time `t`; returns how long it took, in ms. */
    draw(t: number): number;
    /** Halve the point count. Returns false when there is nothing left to drop. */
    lighter(): boolean;
  };

  let canvas = $state<HTMLCanvasElement | null>(null);
  /** A 2D context is not quite a given either, so a background never assumes one. */
  let supported = $state(true);
  let wave: Handle | null = null;
  let calm = $state(false);
  $effect(() => {
    calm = quiet;
  });

  function build(cv: HTMLCanvasElement): Handle | null {
    const ctx = cv.getContext("2d", { alpha: true });
    if (!ctx) return null;
    const g2 = ctx;

    let W = 1, H = 1, dpr = 1, aspect = 1, extent = HALF;
    /** Every other column and row is skipped at stride 2; three in four at 4. */
    let stride = 1;

    // The lattice, with 12% of a cell of jitter: enough to stop the far rows
    // moiréing against each other, far too little to lose the weave.
    const cols = WAVE.cols, rows = WAVE.rows;
    const grid = new Float32Array(cols * rows * 2);
    {
      const jx = 0.24 / cols, jz = 0.18 / rows;
      let i = 0, seed = 1;
      const rnd = () => ((seed = (seed * 1103515245 + 12345) & 0x7fffffff) / 0x7fffffff - 0.5);
      for (let r = 0; r < rows; r++) {
        const z = (r / (rows - 1)) * 2 - 1;
        for (let c = 0; c < cols; c++) {
          const x = (c / (cols - 1)) * 2 - 1;
          grid[i++] = x + rnd() * jx;
          grid[i++] = z + rnd() * jz;
        }
      }
    }

    // Per-frame scratch: screen x, y, size, alpha and colour step per point;
    // per row, where its points sit in those arrays and how far away it is,
    // so rows can be painted far to near.
    const N = cols * rows;
    const sx = new Float32Array(N), sy = new Float32Array(N), ss = new Float32Array(N), sa = new Float32Array(N);
    const sb = new Uint8Array(N);
    const rowFrom = new Int32Array(rows), rowTo = new Int32Array(rows), rowDepth = new Float32Array(rows);

    const mvp = new Float32Array(16);
    const t0 = new Float32Array(16), t1 = new Float32Array(16);
    const t2 = new Float32Array(16), t3 = new Float32Array(16);

    // The dot itself, pre-rendered once per colour step: a disc with a soft
    // rim, in the ramp between trough and crest. `drawImage` of a sprite is
    // the cheapest way to put six thousand anti-aliased circles on a 2D
    // canvas thirty times a second.
    const SPRITE = 32;
    let sprites: HTMLCanvasElement[] = [];
    let low = [169, 198, 193], high = [0, 86, 75];
    function ramp() {
      sprites = [];
      for (let b = 0; b < RAMP; b++) {
        const lum = b / (RAMP - 1);
        const col = low.map((l, i) => Math.round(l + (high[i] - l) * lum));
        const s = document.createElement("canvas");
        s.width = s.height = SPRITE;
        const c = s.getContext("2d");
        if (!c) continue;
        const grad = c.createRadialGradient(SPRITE / 2, SPRITE / 2, 0, SPRITE / 2, SPRITE / 2, SPRITE / 2);
        const rgb = `${col[0]},${col[1]},${col[2]}`;
        grad.addColorStop(0, `rgba(${rgb},1)`);
        grad.addColorStop(0.68, `rgba(${rgb},1)`);
        grad.addColorStop(1, `rgba(${rgb},0)`);
        c.fillStyle = grad;
        c.fillRect(0, 0, SPRITE, SPRITE);
        sprites.push(s);
      }
    }

    function repalette() {
      const css = getComputedStyle(document.documentElement);
      const hex = (name: string, fallback: string) => {
        const v = (css.getPropertyValue(name).trim() || fallback).replace("#", "");
        return [0, 2, 4].map((i) => parseInt(v.slice(i, i + 2), 16));
      };
      low = hex("--wave-trough", "a9c6c1");
      high = hex("--wave-crest", "00564b");
      ramp();
    }

    function resize() {
      // 1.5, not 2: the dots are soft-edged sprites and the difference is
      // invisible, while the fill cost is not.
      dpr = Math.min(window.devicePixelRatio || 1, 1.5);
      const w = Math.max(1, Math.round(cv.clientWidth * dpr));
      const h = Math.max(1, Math.round(cv.clientHeight * dpr));
      if (cv.width !== w || cv.height !== h) {
        cv.width = w;
        cv.height = h;
      }
      W = w;
      H = h;
      aspect = w / h;
      // Long enough to run off both sides of any window it is given.
      extent = Math.min(26, Math.max(12, HALF * Math.max(1, aspect / 1.55)));
    }

    function draw(time: number): number {
      const began = performance.now();
      const pitch = (WAVE.tilt * Math.PI) / 180;
      // Not an orbit: a 140-second breath. A constant yaw reads as a
      // screensaver, and the wave is already the moving thing.
      const yaw = ((WAVE.yaw + Math.sin(time * 0.045) * WAVE.drift) * Math.PI) / 180;

      m4.perspective(t0, FOV, aspect, 0.1, WAVE.dist + extent * 3 + 10);
      m4.translation(t1, 0, -WAVE.horizon, -WAVE.dist);
      m4.mul(t2, t0, t1);
      m4.rotX(t1, pitch);
      m4.mul(t3, t2, t1);
      m4.rotY(t1, yaw);
      m4.mul(mvp, t3, t1);

      const ex = extent, ez = extent * WAVE.width;
      const fog0 = WAVE.dist * 0.9, fog1 = WAVE.dist + extent * 1.6;
      const cx = W / 2, cy = H / 2;
      const rw = WAVE.ringW * dpr, rh = WAVE.ringH * dpr;
      const top = Math.max(1, WAVE.topFade * H);
      const maxSize = 14 * dpr;

      let n = 0;
      const drawn: number[] = [];
      for (let r = 0; r < rows; r += stride) {
        rowFrom[r] = n;
        let depth = 0, counted = 0;
        for (let c = 0; c < cols; c += stride) {
          const gi = (r * cols + c) * 2;
          const gx = grid[gi], gz = grid[gi + 1];
          const px = gx * ex, pz = gz * ez;

          const across = Math.exp(-gz * gz * WAVE.band);
          const along = WAVE.taper + (1 - WAVE.taper) * smooth(-0.95, 0.85, gx);
          const hc = height(px, pz, time) * across * along;

          // clip = mvp * (px, hc, pz, 1)
          const cw = mvp[3] * px + mvp[7] * hc + mvp[11] * pz + mvp[15];
          if (cw <= 0.001) continue;
          const x = (((mvp[0] * px + mvp[4] * hc + mvp[8] * pz + mvp[12]) / cw) * 0.5 + 0.5) * W;
          const y = (0.5 - ((mvp[1] * px + mvp[5] * hc + mvp[9] * pz + mvp[13]) / cw) * 0.5) * H;
          if (x < -maxSize || x > W + maxSize || y < -maxSize || y > H + maxSize) continue;

          const vh = Math.min(1, Math.max(0, (hc / (SPAN * 0.5)) * WAVE.gain * 0.5 + 0.5));
          const lum = smooth(0.08, 0.9, vh);
          // The vignette that used to be a CSS mask: clear over the card,
          // full at the edges, faded in from the top.
          const dx = (x - cx) / rw, dy = (y - cy) / rh;
          const ring = Math.min(1, Math.max(0, (Math.sqrt(dx * dx + dy * dy) - WAVE.clear) / (1 - WAVE.clear)));
          const fadeTop = Math.min(1, y / top);
          const fog = 1 - smooth(fog0, fog1, cw);
          const edge = 1 - smooth(0.78, 1.0, Math.abs(gx));
          // A trough is dim, never gone — the floor is the difference
          // between a trough and a hole.
          const a = fog * edge * across * WAVE.alpha * (0.34 + 0.66 * lum) * ring * fadeTop;
          if (a < 0.015) continue;

          sx[n] = x;
          sy[n] = y;
          // Crests are nearer the eye AND bigger. Size is what turns a flat
          // stipple into a surface you can read the relief of.
          ss[n] = Math.min(maxSize, Math.max(1, WAVE.size * dpr * (WAVE.dist / cw) * (1 + WAVE.crest * smooth(0.42, 1, vh))));
          sa[n] = Math.min(1, a);
          sb[n] = Math.round(lum * (RAMP - 1));
          n++;
          depth += cw;
          counted++;
        }
        rowTo[r] = n;
        rowDepth[r] = counted ? depth / counted : 0;
        if (counted) drawn.push(r);
      }
      // far rows first, so a near crest paints over the distance
      drawn.sort((p, q) => rowDepth[q] - rowDepth[p]);

      g2.clearRect(0, 0, W, H);
      let prevAlpha = -1;
      for (const r of drawn) {
        for (let i = rowFrom[r]; i < rowTo[r]; i++) {
          const s = ss[i];
          if (sa[i] !== prevAlpha) {
            g2.globalAlpha = sa[i];
            prevAlpha = sa[i];
          }
          g2.drawImage(sprites[sb[i]], sx[i] - s / 2, sy[i] - s / 2, s, s);
        }
      }
      g2.globalAlpha = 1;
      return performance.now() - began;
    }

    function lighter(): boolean {
      if (stride >= 4) return false;
      stride *= 2;
      return true;
    }

    resize();
    repalette();
    return { resize, repalette, draw, lighter };
  }

  $effect(() => {
    const cv = canvas;
    if (!cv) return;

    const handle = build(cv);
    if (!handle) {
      supported = false;
      return;
    }
    wave = handle;

    let raf = 0;
    // Scaled time, accumulated: a speed change must not jump the phase.
    let clock = 0;
    let last = performance.now();
    /** When the last frame was drawn — the pacer and the watchdog read it. */
    let drawnAt = last;
    const interval = 1000 / WAVE.fps;
    /** A rolling cost of the last few frames, for the quality governor. */
    let cost = 0;
    let slow = 0;
    let still = false;

    const frame = (now: number) => {
      raf = requestAnimationFrame(frame);
      if (now - drawnAt < interval - 1) return;
      clock += ((now - last) / 1000) * (calm ? 0.5 : 1);
      last = now;
      drawnAt = now;
      const ms = handle.draw(clock);
      // The governor. A frame over budget is fine; a run of them means this
      // machine cannot draw this many dots this often, so draw fewer. If the
      // lightest grid is still over budget, stop animating and leave one
      // good frame — a still wave is a picture, a stuttering one is a bug.
      cost = cost * 0.8 + ms * 0.2;
      if (cost > interval * 0.7) {
        if (++slow > 12) {
          slow = 0;
          cost = 0;
          if (!handle.lighter()) {
            cancelAnimationFrame(raf);
            still = true;
          }
        }
      } else {
        slow = 0;
      }
    };
    const play = () => {
      cancelAnimationFrame(raf);
      if (!motion.on || still) {
        // A still field, not a frozen animation: one frame with structure
        // in it, drawn once.
        handle.draw(STILL);
        return;
      }
      last = performance.now();
      drawnAt = last;
      raf = requestAnimationFrame(frame);
    };
    const onVisibility = () => (document.hidden ? cancelAnimationFrame(raf) : play());

    // A webview pauses requestAnimationFrame when it decides the page is
    // not visible, and does not always say so through `visibilitychange`.
    // A loop that has silently stopped while the document says it is
    // visible is a frozen background, so it is restarted rather than
    // trusted.
    const watchdog = setInterval(() => {
      if (!motion.on || still || document.hidden) return;
      if (performance.now() - drawnAt > 1500) play();
    }, 1000);

    // A resize is a redraw, immediately — not a blank canvas until the
    // next frame comes round.
    const ro = new ResizeObserver(() => {
      handle.resize();
      handle.draw(motion.on && !still ? clock : STILL);
    });
    ro.observe(cv);
    document.addEventListener("visibilitychange", onVisibility);
    play();

    return () => {
      cancelAnimationFrame(raf);
      clearInterval(watchdog);
      ro.disconnect();
      document.removeEventListener("visibilitychange", onVisibility);
      wave = null;
    };
  });

  // Re-read the ramp in place on a theme change rather than tearing the
  // canvas down: the colours moved, the mesh did not.
  $effect(() => {
    void theme.mode;
    void theme.resolved;
    wave?.repalette();
    wave?.draw(STILL);
  });

  // The fallback, and what this screen used to be: 28 breathing circles on
  // a phyllotactic spiral. Kept because a background must never be a hard
  // dependency on anything.
  const DOTS = Array.from({ length: 28 }, (_, i) => {
    const a = i * 2.399963;
    const t = (i + 0.5) / 28;
    const r = Math.sqrt(t) * 0.94;
    return {
      cx: 600 + Math.cos(a) * r * 580,
      cy: 300 + Math.sin(a) * r * 280,
      r: 2 + (i % 3) * 0.8,
      o: 0.35 + (i % 4) * 0.12,
      i,
    };
  });
</script>

{#if supported}
  <canvas bind:this={canvas} class="mesh-wave" class:quiet={calm} aria-hidden="true"></canvas>
{:else}
  <svg
    class="pointer-events-none absolute inset-0 h-full w-full"
    viewBox="0 0 1200 600"
    preserveAspectRatio="xMidYMid slice"
    aria-hidden="true"
    style="color: var(--accent); opacity: .22;"
  >
    {#each DOTS as d (d.i)}
      <circle cx={d.cx} cy={d.cy} r={d.r} fill="currentColor" opacity={d.o}>
        <animate
          attributeName="opacity"
          values="{d.o};{Math.min(1, d.o + 0.45)};{d.o}"
          dur="{3.6 + (d.i % 5) * 0.5}s"
          repeatCount="indefinite"
        />
      </circle>
    {/each}
  </svg>
{/if}

<style>
  .mesh-wave {
    position: absolute;
    inset: 0;
    display: block;
    width: 100%;
    height: 100%;
    pointer-events: none;
    /* The ribbon is a background; the card is the page. */
    z-index: 0;
    /* Opacity, so the step-back costs a composite rather than a redraw. */
    opacity: 1;
    transition: opacity var(--dur-slower) var(--ease);
  }

  .mesh-wave.quiet {
    opacity: 0.55;
  }
</style>
