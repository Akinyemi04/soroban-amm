/**
 * RangeSelector — interactive price range picker for concentrated liquidity.
 *
 * Renders a dual-thumb slider that lets users select a [lower, upper] price
 * range. A mini bar-chart visualises the current liquidity distribution so
 * users can see where active positions are concentrated.
 *
 * Accessibility: WCAG 2.1 AA compliant. Each thumb is a focusable element
 * with role="slider", aria-valuenow, aria-valuemin, aria-valuemax, and
 * aria-label. Keyboard: arrow keys (fine), Page Up/Down (coarse), Home/End.
 */

import React, { useCallback, useId, useRef, useState } from "react";
import type { BaseProps, PriceRange } from "./types.js";

export interface LiquidityBin {
  price: number;
  liquidity: number;
}

export interface RangeSelectorProps extends BaseProps {
  /** Minimum selectable price. */
  minPrice: number;
  /** Maximum selectable price. */
  maxPrice: number;
  /** Current spot price (drawn as a vertical line). */
  currentPrice: number;
  /** Controlled value — the selected range. */
  value: PriceRange;
  /** Called whenever the range changes. */
  onChange: (range: PriceRange) => void;
  /** Optional liquidity distribution data for the background chart. */
  liquidityBins?: LiquidityBin[];
  /** Decimal precision for displayed prices. Default 6. */
  decimals?: number;
  /** Token symbols for axis labels. */
  tokenA?: string;
  tokenB?: string;
}

const clamp = (v: number, lo: number, hi: number) =>
  Math.min(hi, Math.max(lo, v));

const pct = (v: number, min: number, max: number) =>
  ((v - min) / (max - min)) * 100;

export function RangeSelector({
  minPrice,
  maxPrice,
  currentPrice,
  value,
  onChange,
  liquidityBins = [],
  decimals = 6,
  tokenA = "Token A",
  tokenB = "Token B",
  className = "",
  "aria-label": ariaLabel,
}: RangeSelectorProps) {
  const id = useId();
  const trackRef = useRef<HTMLDivElement>(null);
  const [dragging, setDragging] = useState<"lower" | "upper" | null>(null);

  const priceAtPct = (p: number) => minPrice + (p / 100) * (maxPrice - minPrice);

  const getPosPct = useCallback(
    (clientX: number): number => {
      const rect = trackRef.current?.getBoundingClientRect();
      if (!rect) return 0;
      return clamp(((clientX - rect.left) / rect.width) * 100, 0, 100);
    },
    [],
  );

  const handleMouseMove = useCallback(
    (e: MouseEvent) => {
      if (!dragging) return;
      const p = priceAtPct(getPosPct(e.clientX));
      if (dragging === "lower") {
        onChange({ lower: clamp(p, minPrice, value.upper - 0.000001), upper: value.upper });
      } else {
        onChange({ lower: value.lower, upper: clamp(p, value.lower + 0.000001, maxPrice) });
      }
    },
    [dragging, getPosPct, maxPrice, minPrice, onChange, value],
  );

  const handleMouseUp = useCallback(() => {
    setDragging(null);
    window.removeEventListener("mousemove", handleMouseMove);
    window.removeEventListener("mouseup", handleMouseUp);
  }, [handleMouseMove]);

  const startDrag = (thumb: "lower" | "upper") => (e: React.MouseEvent) => {
    e.preventDefault();
    setDragging(thumb);
    window.addEventListener("mousemove", handleMouseMove);
    window.addEventListener("mouseup", handleMouseUp);
  };

  const handleKeyDown =
    (thumb: "lower" | "upper") =>
    (e: React.KeyboardEvent) => {
      const step = (maxPrice - minPrice) / 200;
      const coarse = (maxPrice - minPrice) / 20;
      const delta =
        e.key === "ArrowRight" || e.key === "ArrowUp"
          ? step
          : e.key === "ArrowLeft" || e.key === "ArrowDown"
            ? -step
            : e.key === "PageUp"
              ? coarse
              : e.key === "PageDown"
                ? -coarse
                : e.key === "Home"
                  ? -(maxPrice - minPrice)
                  : e.key === "End"
                    ? maxPrice - minPrice
                    : 0;
      if (delta === 0) return;
      e.preventDefault();
      if (thumb === "lower") {
        onChange({
          lower: clamp(value.lower + delta, minPrice, value.upper - 0.000001),
          upper: value.upper,
        });
      } else {
        onChange({
          lower: value.lower,
          upper: clamp(value.upper + delta, value.lower + 0.000001, maxPrice),
        });
      }
    };

  const maxLiq = Math.max(...liquidityBins.map((b) => b.liquidity), 1);
  const spotPct = pct(currentPrice, minPrice, maxPrice);
  const lowerPct = pct(value.lower, minPrice, maxPrice);
  const upperPct = pct(value.upper, minPrice, maxPrice);

  const fmt = (n: number) => n.toFixed(decimals);

  return (
    <div
      className={`rs-root ${className}`}
      aria-label={ariaLabel ?? "Price range selector"}
      style={styles.root}
    >
      <div style={styles.labels}>
        <span style={styles.muted}>{tokenB} per {tokenA}</span>
        <span style={styles.muted}>
          {fmt(value.lower)} &ndash; {fmt(value.upper)}
        </span>
      </div>

      {/* Liquidity bars */}
      <div style={styles.chartWrap} aria-hidden="true">
        {liquidityBins.map((bin, i) => {
          const binPct = pct(bin.price, minPrice, maxPrice);
          const inRange = bin.price >= value.lower && bin.price <= value.upper;
          return (
            <div
              key={i}
              style={{
                ...styles.bin,
                left: `${binPct}%`,
                height: `${(bin.liquidity / maxLiq) * 100}%`,
                background: inRange ? "#3fb950" : "#30363d",
              }}
            />
          );
        })}
      </div>

      {/* Track */}
      <div ref={trackRef} style={styles.track}>
        {/* Selected range highlight */}
        <div
          aria-hidden="true"
          style={{
            ...styles.selection,
            left: `${lowerPct}%`,
            width: `${upperPct - lowerPct}%`,
          }}
        />

        {/* Spot price line */}
        <div
          aria-hidden="true"
          style={{ ...styles.spotLine, left: `${clamp(spotPct, 0, 100)}%` }}
          title={`Current price: ${fmt(currentPrice)}`}
        />

        {/* Lower thumb */}
        <div
          id={`${id}-lower`}
          role="slider"
          tabIndex={0}
          aria-label={`Lower bound: ${fmt(value.lower)}`}
          aria-valuenow={value.lower}
          aria-valuemin={minPrice}
          aria-valuemax={value.upper}
          style={{ ...styles.thumb, left: `${lowerPct}%` }}
          onMouseDown={startDrag("lower")}
          onKeyDown={handleKeyDown("lower")}
        />

        {/* Upper thumb */}
        <div
          id={`${id}-upper`}
          role="slider"
          tabIndex={0}
          aria-label={`Upper bound: ${fmt(value.upper)}`}
          aria-valuenow={value.upper}
          aria-valuemin={value.lower}
          aria-valuemax={maxPrice}
          style={{ ...styles.thumb, left: `${upperPct}%` }}
          onMouseDown={startDrag("upper")}
          onKeyDown={handleKeyDown("upper")}
        />
      </div>

      {/* Numeric inputs */}
      <div style={styles.inputs}>
        <label style={styles.inputWrap}>
          <span style={styles.muted}>Min price</span>
          <input
            type="number"
            value={value.lower}
            step={(maxPrice - minPrice) / 200}
            min={minPrice}
            max={value.upper}
            style={styles.input}
            aria-label="Minimum price"
            onChange={(e) =>
              onChange({
                lower: clamp(Number(e.target.value), minPrice, value.upper - 0.000001),
                upper: value.upper,
              })
            }
          />
        </label>
        <label style={styles.inputWrap}>
          <span style={styles.muted}>Max price</span>
          <input
            type="number"
            value={value.upper}
            step={(maxPrice - minPrice) / 200}
            min={value.lower}
            max={maxPrice}
            style={styles.input}
            aria-label="Maximum price"
            onChange={(e) =>
              onChange({
                lower: value.lower,
                upper: clamp(Number(e.target.value), value.lower + 0.000001, maxPrice),
              })
            }
          />
        </label>
      </div>

      {spotPct < lowerPct || spotPct > upperPct ? (
        <p role="alert" style={styles.warning}>
          Current price is outside the selected range. Position will be
          inactive until the price re-enters the range.
        </p>
      ) : null}
    </div>
  );
}

const styles: Record<string, React.CSSProperties> = {
  root: {
    fontFamily: "inherit",
    userSelect: "none",
    width: "100%",
  },
  labels: {
    display: "flex",
    justifyContent: "space-between",
    fontSize: 12,
    marginBottom: 8,
  },
  muted: { color: "#8b949e", fontSize: 12 },
  chartWrap: {
    position: "relative",
    height: 48,
    marginBottom: 4,
    overflow: "hidden",
  },
  bin: {
    position: "absolute",
    bottom: 0,
    width: 3,
    borderRadius: "1px 1px 0 0",
    transform: "translateX(-50%)",
    transition: "background 0.2s",
  },
  track: {
    position: "relative",
    height: 6,
    background: "#30363d",
    borderRadius: 3,
    marginBottom: 16,
    cursor: "pointer",
  },
  selection: {
    position: "absolute",
    top: 0,
    height: "100%",
    background: "#1f4a2e",
    borderRadius: 3,
  },
  spotLine: {
    position: "absolute",
    top: -6,
    bottom: -6,
    width: 2,
    background: "#58a6ff",
    borderRadius: 1,
    transform: "translateX(-50%)",
  },
  thumb: {
    position: "absolute",
    top: "50%",
    width: 18,
    height: 18,
    background: "#e6edf3",
    border: "2px solid #58a6ff",
    borderRadius: "50%",
    transform: "translate(-50%, -50%)",
    cursor: "grab",
    outline: "none",
    transition: "box-shadow 0.15s",
  },
  inputs: {
    display: "flex",
    gap: 12,
  },
  inputWrap: {
    flex: 1,
    display: "flex",
    flexDirection: "column",
    gap: 4,
  },
  input: {
    width: "100%",
    background: "#161b22",
    border: "1px solid #30363d",
    color: "#e6edf3",
    borderRadius: 6,
    padding: "6px 10px",
    fontSize: 13,
    outline: "none",
  },
  warning: {
    marginTop: 10,
    fontSize: 12,
    color: "#d29922",
    background: "#3d2e00",
    border: "1px solid #d29922",
    borderRadius: 6,
    padding: "6px 10px",
  },
};
