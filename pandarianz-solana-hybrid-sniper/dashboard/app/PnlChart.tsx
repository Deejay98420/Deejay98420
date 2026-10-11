"use client";

import { useEffect, useMemo, useRef, useState } from "react";
import type { Position } from "../lib/types";

const H = 180;
const PAD = { l: 52, r: 12, t: 12, b: 22 };

/** Single-series line: cumulative realized P&L after each closed trade. Hover = crosshair + tooltip. */
export function PnlChart({ trades }: { trades: Position[] }) {
  const [hover, setHover] = useState<number | null>(null);
  const box = useRef<HTMLDivElement>(null);
  const [W, setW] = useState(640);
  useEffect(() => {
    const el = box.current;
    if (!el) return;
    const ro = new ResizeObserver(() => setW(Math.max(240, el.clientWidth)));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const pts = useMemo(() => {
    const sorted = [...trades].filter((t) => t.realized_pnl != null).sort((a, b) => (a.closed_us ?? 0) - (b.closed_us ?? 0));
    let cum = 0;
    return [{ i: 0, cum: 0, label: "start", pnl: 0 }, ...sorted.map((t, i) => {
      cum += (t.realized_pnl ?? 0) / 1e9;
      return { i: i + 1, cum, label: t.symbol ?? t.mint.slice(0, 6), pnl: (t.realized_pnl ?? 0) / 1e9 };
    })];
  }, [trades]);
  if (pts.length < 2) return <div className="empty" ref={box}>No closed trades yet.</div>;
  const lo = Math.min(0, ...pts.map((p) => p.cum));
  const hi = Math.max(0, ...pts.map((p) => p.cum));
  const span = hi - lo || 1;
  const x = (i: number) => PAD.l + (i / (pts.length - 1)) * (W - PAD.l - PAD.r);
  const y = (v: number) => PAD.t + (1 - (v - lo) / span) * (H - PAD.t - PAD.b);
  const d = pts.map((p, k) => `${k ? "L" : "M"}${x(p.i).toFixed(1)},${y(p.cum).toFixed(1)}`).join(" ");
  const ticks = [lo, (lo + hi) / 2, hi];
  const h = hover == null ? null : pts[hover];
  return (
    <div className="chart" ref={box}>
      <svg viewBox={`0 0 ${W} ${H}`} width={W} height={H} role="img" aria-label="Cumulative realized P&L in SOL"
        onMouseLeave={() => setHover(null)}
        onMouseMove={(e) => {
          const r = (e.currentTarget as SVGSVGElement).getBoundingClientRect();
          const fx = ((e.clientX - r.left) / r.width) * W;
          const k = Math.round(((fx - PAD.l) / (W - PAD.l - PAD.r)) * (pts.length - 1));
          setHover(Math.max(0, Math.min(pts.length - 1, k)));
        }}>
        {ticks.map((t, k) => (
          <g key={k}>
            <line x1={PAD.l} x2={W - PAD.r} y1={y(t)} y2={y(t)} stroke="var(--border)" strokeWidth={1} vectorEffect="non-scaling-stroke" />
            <text x={PAD.l - 6} y={y(t) + 4} textAnchor="end" fontSize={10} fill="var(--muted)">{t.toFixed(3)}</text>
          </g>
        ))}
        <line x1={PAD.l} x2={W - PAD.r} y1={y(0)} y2={y(0)} stroke="var(--text-2)" strokeWidth={1} vectorEffect="non-scaling-stroke" />
        <path d={d} fill="none" stroke="var(--series-1)" strokeWidth={2} vectorEffect="non-scaling-stroke" strokeLinejoin="round" />
        {h && (
          <>
            <line x1={x(h.i)} x2={x(h.i)} y1={PAD.t} y2={H - PAD.b} stroke="var(--muted)" strokeWidth={1} vectorEffect="non-scaling-stroke" />
            <circle cx={x(h.i)} cy={y(h.cum)} r={4} fill="var(--series-1)" stroke="var(--surface)" strokeWidth={2} vectorEffect="non-scaling-stroke" />
          </>
        )}
        <text x={PAD.l} y={H - 6} fontSize={10} fill="var(--muted)">trade #1</text>
        <text x={W - PAD.r} y={H - 6} fontSize={10} fill="var(--muted)" textAnchor="end">#{pts.length - 1}</text>
      </svg>
      {h && h.i > 0 && (
        <div className="tooltip mono" style={{ left: `${(x(h.i) / W) * 100}%`, top: `${(y(h.cum) / H) * 100}%` }}>
          #{h.i} {h.label}: {h.pnl >= 0 ? "+" : "−"}{Math.abs(h.pnl).toFixed(4)} · cum {h.cum >= 0 ? "+" : "−"}{Math.abs(h.cum).toFixed(4)} SOL
        </div>
      )}
    </div>
  );
}
