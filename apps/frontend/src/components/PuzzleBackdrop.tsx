/**
 * The POM puzzle in the night sky: four puzzle pieces made of glowing
 * particles drift in from the corners, spiral toward each other and lock into
 * one piece; light runs along the seams, then they drift apart and the loop
 * starts again. Stars, drifting pixel squares and particle clouds sit around
 * them. A dependency-free canvas, ported from the site's hero animation
 * (pieceofmind.cloud) without any other mark: only the pieces. It fills its
 * parent and stays behind the content (`pointer-events: none`). Under reduced
 * motion it draws one still frame; it pauses while the tab is hidden.
 */
import { useEffect, useRef } from "react";

type Dot = { x: number; y: number; r: number; ph: number; w: number };
type Piece = { col: number; row: number; e: number[]; from: [number, number]; path?: Path2D; color?: number[]; dots?: Dot[]; home?: [number, number] };

export function PuzzleBackdrop({ className = "" }: { className?: string }) {
  const ref = useRef<HTMLCanvasElement>(null);

  useEffect(() => {
    const cv = ref.current;
    const ctx = cv?.getContext("2d");
    if (!cv || !ctx) return;
    const reduced = window.matchMedia?.("(prefers-reduced-motion: reduce)").matches === true;
    const DPR = Math.min(window.devicePixelRatio || 1, 2);
    const VW = 1600, VH = 1080, S = 230;
    const CX = VW / 2, CY = VH / 2 - 30;
    const COLORS = [[255, 47, 227], [138, 92, 255], [79, 155, 255], [47, 220, 200]];
    // edges: top, right, bottom, left (1 = knob out, -1 = hole, 0 = flat)
    const PIECES: Piece[] = [
      { col: 0, row: 0, e: [0, 1, -1, 0], from: [-1, -1] },
      { col: 1, row: 0, e: [0, 0, 1, -1], from: [1, -1] },
      { col: 0, row: 1, e: [1, -1, 0, 0], from: [-1, 1] },
      { col: 1, row: 1, e: [-1, 0, 0, 1], from: [1, 1] },
    ];
    const edge = (p: Path2D, x0: number, y0: number, x1: number, y1: number, t: number) => {
      const dx = x1 - x0, dy = y1 - y0, len = Math.hypot(dx, dy);
      const nx = dy / len, ny = -dx / len;
      const q = (a: number, b: number) => [x0 + a * dx + b * len * nx * t, y0 + a * dy + b * len * ny * t];
      if (!t) { p.lineTo(x1, y1); return; }
      const a = q(.34, 0), b1 = q(.40, 0), b2 = q(.28, .24), m = q(.50, .25), c1 = q(.72, .24), c2 = q(.60, 0), z = q(.66, 0);
      p.lineTo(a[0], a[1]);
      p.bezierCurveTo(b1[0], b1[1], b2[0], b2[1], m[0], m[1]);
      p.bezierCurveTo(c1[0], c1[1], c2[0], c2[1], z[0], z[1]);
      p.lineTo(x1, y1);
    };
    const piecePath = (e: number[]) => {
      const p = new Path2D();
      p.moveTo(0, 0);
      edge(p, 0, 0, S, 0, e[0]);
      edge(p, S, 0, S, S, e[1]);
      edge(p, S, S, 0, S, e[2]);
      edge(p, 0, S, 0, 0, e[3]);
      p.closePath();
      return p;
    };
    const probe = document.createElement("canvas").getContext("2d");
    PIECES.forEach((pc, i) => {
      pc.path = piecePath(pc.e);
      pc.color = COLORS[i];
      pc.dots = [];
      let tries = 0;
      while (probe && pc.dots.length < 1400 && tries < 30000) {
        tries += 1;
        const x = -S * .3 + Math.random() * S * 1.6, y = -S * .3 + Math.random() * S * 1.6;
        if (probe.isPointInPath(pc.path, x, y)) {
          pc.dots.push({ x: x - S / 2, y: y - S / 2, r: .7 + Math.random() * 1.6, ph: Math.random() * 6.28, w: Math.random() });
        }
      }
      pc.home = [CX - S + pc.col * S + S / 2, CY - S + pc.row * S + S / 2];
    });
    const stars = Array.from({ length: 160 }, () => ({ x: Math.random() * VW, y: Math.random() * VH * .8, r: Math.random() * 1.3 + .2, ph: Math.random() * 6.28 }));
    const clouds = Array.from({ length: 26 }, (_, c) => {
      const side = c % 2 ? 1 : -1;
      return { x: VW / 2 + side * (250 + Math.random() * 650), y: VH * (.78 + Math.random() * .25), r: 110 + Math.random() * 170, hue: Math.random(), sp: (Math.random() - .5) * 6 };
    });
    const cloudDots = clouds.flatMap((cl) => Array.from({ length: 70 }, () => {
      const a = Math.random() * 6.28, rr = Math.sqrt(Math.random()) * cl.r * .9;
      return { cl, dx: Math.cos(a) * rr, dy: Math.sin(a) * rr * .6, r: .8 + Math.random() * 1.6, ph: Math.random() * 6.28 };
    }));
    const squares = Array.from({ length: 14 }, () => ({ x: Math.random() * VW, y: Math.random() * VH, s: 6 + Math.random() * 12, sp: 4 + Math.random() * 10, a: .15 + Math.random() * .3 }));
    const LOOP = 15;
    const ease = (p: number) => (p < .5 ? 4 * p * p * p : 1 - Math.pow(-2 * p + 2, 3) / 2);
    const clamp = (v: number) => (v < 0 ? 0 : v > 1 ? 1 : v);
    const seg = (t: number, a: number, b: number) => ease(clamp((t - a) / (b - a)));
    const rgba = (c: number[], a: number) => `rgba(${c[0]},${c[1]},${c[2]},${a})`;

    const resize = () => {
      const r = cv.getBoundingClientRect();
      cv.width = Math.max(1, Math.round(r.width * DPR));
      cv.height = Math.max(1, Math.round(r.height * DPR));
    };
    resize();
    const observer = typeof ResizeObserver !== "undefined" ? new ResizeObserver(resize) : null;
    observer?.observe(cv);

    const draw = (time: number) => {
      const t = reduced ? 8 : time % LOOP;
      const k = Math.max(cv.width / VW, cv.height / VH);
      ctx.setTransform(1, 0, 0, 1, 0, 0);
      ctx.clearRect(0, 0, cv.width, cv.height);
      ctx.setTransform(k, 0, 0, k, (cv.width - VW * k) / 2, (cv.height - VH * k) / 2);
      for (const st of stars) {
        ctx.fillStyle = `rgba(200,190,255,${.25 + .35 * Math.sin(time * 1.3 + st.ph) * .5 + .2})`;
        ctx.fillRect(st.x, st.y, st.r, st.r);
      }
      for (const sq of squares) {
        let y = (sq.y - time * sq.sp) % VH; if (y < 0) y += VH;
        ctx.fillStyle = `rgba(150,110,255,${sq.a})`;
        ctx.fillRect(sq.x, y, sq.s, sq.s);
      }
      const gather = seg(t, .6, 5.2);
      const lock = seg(t, 5.1, 5.5) * (1 - seg(t, 5.6, 6.6));
      const apart = seg(t, 12.6, 14.6);
      const fadeIn = seg(t, 0, .9), fadeOut = 1 - seg(t, 13.6, 14.8);
      const bob = Math.sin(time * 1.1) * 10;
      ctx.globalCompositeOperation = "lighter";
      const halo = gather * fadeOut * (1 - apart * .7);
      if (halo > 0) {
        const hg = ctx.createRadialGradient(CX, CY + bob, 0, CX, CY + bob, S * 2.3);
        hg.addColorStop(0, `rgba(160,70,255,${(.30 + lock * .25) * halo})`);
        hg.addColorStop(.5, `rgba(80,90,255,${.10 * halo})`);
        hg.addColorStop(1, "rgba(80,90,255,0)");
        ctx.fillStyle = hg;
        ctx.fillRect(CX - S * 2.4, CY - S * 2.4 + bob, S * 4.8, S * 4.8);
      }
      PIECES.forEach((pc, i) => {
        const R = (1 - gather) * 900 + apart * 160;
        const ang = Math.atan2(pc.from[1], pc.from[0]) + (1 - gather) * Math.PI * 1.6;
        const x = pc.home![0] + Math.cos(ang) * R;
        const y = pc.home![1] + Math.sin(ang) * R * .62 + bob;
        const rot = (1 - gather) * (i % 2 ? 1.4 : -1.4) + apart * (i % 2 ? .3 : -.3);
        const alpha = fadeIn * fadeOut;
        const color = pc.color!;
        ctx.save();
        ctx.translate(x, y);
        ctx.rotate(rot);
        ctx.save();
        ctx.translate(-S / 2, -S / 2);
        ctx.shadowColor = rgba(color, .9);
        ctx.shadowBlur = 26 + lock * 50;
        ctx.strokeStyle = rgba(color, (.55 + lock * .45) * alpha);
        ctx.lineWidth = 2.2;
        ctx.stroke(pc.path!);
        ctx.fillStyle = rgba(color, (.05 + lock * .12) * alpha);
        ctx.fill(pc.path!);
        ctx.restore();
        for (const dot of pc.dots!) {
          const tw = .45 + .55 * Math.sin(time * 2.2 + dot.ph);
          const spread = (1 - gather) * 18 + apart * 26;
          const dx = dot.x + Math.cos(dot.ph + time) * spread, dy = dot.y + Math.sin(dot.ph * 1.3 + time) * spread;
          const mix = dot.w * .6;
          ctx.fillStyle = `rgba(${Math.round(color[0] + (255 - color[0]) * mix)},${Math.round(color[1] + (255 - color[1]) * mix)},${Math.round(color[2] + (255 - color[2]) * mix)},${tw * .8 * alpha})`;
          ctx.fillRect(dx, dy, dot.r, dot.r);
        }
        ctx.restore();
      });
      const run = seg(t, 6, 12.6) * (1 - apart);
      if (run > 0) {
        const seams = [[[CX, CY - S], [CX, CY + S]], [[CX - S, CY], [CX + S, CY]]];
        for (let n = 0; n < 6; n += 1) {
          const sm = seams[n % 2];
          const p = (time * .35 + n / 6) % 1;
          const dir = n % 3 === 0 ? 1 - p : p;
          const px = sm[0][0] + (sm[1][0] - sm[0][0]) * dir, py = sm[0][1] + (sm[1][1] - sm[0][1]) * dir + bob;
          const g = ctx.createRadialGradient(px, py, 0, px, py, 22);
          g.addColorStop(0, `rgba(255,255,255,${.9 * run})`);
          g.addColorStop(.3, `rgba(53,201,240,${.6 * run})`);
          g.addColorStop(1, "rgba(53,201,240,0)");
          ctx.fillStyle = g;
          ctx.fillRect(px - 22, py - 22, 44, 44);
        }
      }
      for (const cl of clouds) {
        const x = cl.x + Math.sin(time * .1 + cl.hue * 6) * 30 + cl.sp * Math.sin(time * .05);
        const col = cl.hue > .5 ? [120, 60, 255] : [70, 40, 200];
        const g2 = ctx.createRadialGradient(x, cl.y, 0, x, cl.y, cl.r);
        g2.addColorStop(0, rgba(col, .22));
        g2.addColorStop(.6, rgba(col, .08));
        g2.addColorStop(1, rgba(col, 0));
        ctx.fillStyle = g2;
        ctx.beginPath(); ctx.arc(x, cl.y, cl.r, 0, Math.PI * 2); ctx.fill();
      }
      for (const cd of cloudDots) {
        const x = cd.cl.x + Math.sin(time * .1 + cd.cl.hue * 6) * 30 + cd.cl.sp * Math.sin(time * .05) + cd.dx;
        ctx.fillStyle = `rgba(220,120,255,${.18 + .25 * (.5 + .5 * Math.sin(time * 1.7 + cd.ph))})`;
        ctx.fillRect(x, cd.cl.y + cd.dy, cd.r, cd.r);
      }
      ctx.globalCompositeOperation = "source-over";
    };

    let frame = 0;
    const loop = (ms: number) => {
      if (!document.hidden) draw(ms / 1000);
      frame = requestAnimationFrame(loop);
    };
    if (reduced) draw(8);
    else frame = requestAnimationFrame(loop);
    return () => {
      cancelAnimationFrame(frame);
      observer?.disconnect();
    };
  }, []);

  return (
    <canvas
      ref={ref}
      aria-hidden="true"
      data-testid="puzzle-backdrop"
      className={`pointer-events-none absolute inset-0 h-full w-full ${className}`}
    />
  );
}
