/**
 * CapitalEfficiencyCalc — computes and displays capital efficiency for a
 * concentrated liquidity position versus a full-range (v2-style) position.
 *
 * Capital efficiency = (full_range_capital_needed / concentrated_capital_needed).
 * A tighter range means higher efficiency but more frequent rebalancing risk.
 *
 * Accessibility: all inputs have associated labels; the result region has
 * role="status" so screen readers announce updates.
 */

import React, { useMemo, useId } from "react";
import type { BaseProps, PriceRange } from "./types.js";

export interface CapitalEfficiencyCalcProps extends BaseProps {
  /** Current spot price. */
  currentPrice: number;
  /** Selected concentrated liquidity range. */
  priceRange: PriceRange;
  /** Deposit amount in USD (used to display capital required at each mode). */
  depositUsd?: number;
  /** Token labels for display. */
  tokenA?: string;
  tokenB?: string;
  /** Allow overriding the range from within the component. */
  onRangeChange?: (range: PriceRange) => void;
}

/**
 * Computes the capital efficiency multiplier for a [lower, upper] range
 * relative to [0, ∞) using the Uniswap v3 formula:
 *
 *   efficiency = 1 / (1 - sqrt(lower/upper))
 *
 * when the current price is inside the range.
 */
function computeEfficiency(
  currentPrice: number,
  lower: number,
  upper: number,
): number {
  if (lower <= 0 || upper <= 0 || lower >= upper) return 1;
  if (currentPrice < lower || currentPrice > upper) return 1;
  const sqrtRatio = Math.sqrt(lower / upper);
  return Math.max(1, 1 / (1 - sqrtRatio));
}

export function CapitalEfficiencyCalc({
  currentPrice,
  priceRange,
  depositUsd = 10_000,
  tokenA = "Token A",
  tokenB = "Token B",
  onRangeChange,
  className = "",
  "aria-label": ariaLabel,
}: CapitalEfficiencyCalcProps) {
  const id = useId();

  const efficiency = useMemo(
    () => computeEfficiency(currentPrice, priceRange.lower, priceRange.upper),
    [currentPrice, priceRange],
  );

  const concentratedCapital = depositUsd / efficiency;
  const rangePct =
    ((priceRange.upper - priceRange.lower) / currentPrice) * 100;
  const inRange =
    currentPrice >= priceRange.lower && currentPrice <= priceRange.upper;

  const effStr = efficiency >= 1000 ? ">1000" : efficiency.toFixed(1);
  const barWidth = Math.min(100, ((efficiency - 1) / 99) * 100);

  return (
    <div
      className={className}
      aria-label={ariaLabel ?? "Capital efficiency calculator"}
      style={styles.root}
    >
      <h3 style={styles.heading}>Capital Efficiency</h3>

      {/* Efficiency display */}
      <div role="status" aria-live="polite" style={styles.resultRow}>
        <div style={styles.bigNumber}>
          {effStr}
          <span style={styles.bigUnit}>x</span>
        </div>
        <p style={styles.resultCaption}>
          {inRange
            ? `Your capital works ${effStr}× harder than a full-range position.`
            : "Price is outside range — position earns no fees until re-entered."}
        </p>
      </div>

      {/* Efficiency bar */}
      <div style={styles.barWrap} aria-hidden="true">
        <div style={{ ...styles.barFill, width: `${barWidth}%` }} />
      </div>

      {/* Comparison table */}
      <table style={styles.table} aria-label="Capital comparison">
        <thead>
          <tr>
            <th style={styles.th} scope="col">Strategy</th>
            <th style={styles.th} scope="col">Capital for ${depositUsd.toLocaleString()}</th>
          </tr>
        </thead>
        <tbody>
          <tr>
            <td style={styles.td}>Full range (v2-style)</td>
            <td style={styles.td}>${depositUsd.toLocaleString()}</td>
          </tr>
          <tr style={{ background: "#0d2035" }}>
            <td style={styles.td}>Concentrated ({priceRange.lower.toFixed(4)} – {priceRange.upper.toFixed(4)})</td>
            <td style={{ ...styles.td, color: "#3fb950", fontWeight: 600 }}>
              ${concentratedCapital.toLocaleString(undefined, { maximumFractionDigits: 2 })}
            </td>
          </tr>
        </tbody>
      </table>

      {/* Range info */}
      <div style={styles.infoGrid}>
        <div style={styles.infoCell}>
          <div style={styles.infoLabel}>Range width</div>
          <div style={styles.infoValue}>{rangePct.toFixed(1)}%</div>
        </div>
        <div style={styles.infoCell}>
          <div style={styles.infoLabel}>Lower bound</div>
          <div style={styles.infoValue}>{priceRange.lower.toFixed(6)}</div>
        </div>
        <div style={styles.infoCell}>
          <div style={styles.infoLabel}>Upper bound</div>
          <div style={styles.infoValue}>{priceRange.upper.toFixed(6)}</div>
        </div>
        <div style={styles.infoCell}>
          <div style={styles.infoLabel}>Current price</div>
          <div style={styles.infoValue}>{currentPrice.toFixed(6)}</div>
        </div>
      </div>

      {/* Editable range */}
      {onRangeChange && (
        <div style={styles.editRow}>
          <label style={styles.editLabel} htmlFor={`${id}-lower`}>
            Min price
          </label>
          <input
            id={`${id}-lower`}
            type="number"
            value={priceRange.lower}
            step={currentPrice / 200}
            style={styles.input}
            aria-label={`Minimum price for ${tokenA}/${tokenB}`}
            onChange={(e) =>
              onRangeChange({ lower: Number(e.target.value), upper: priceRange.upper })
            }
          />
          <label style={styles.editLabel} htmlFor={`${id}-upper`}>
            Max price
          </label>
          <input
            id={`${id}-upper`}
            type="number"
            value={priceRange.upper}
            step={currentPrice / 200}
            style={styles.input}
            aria-label={`Maximum price for ${tokenA}/${tokenB}`}
            onChange={(e) =>
              onRangeChange({ lower: priceRange.lower, upper: Number(e.target.value) })
            }
          />
        </div>
      )}

      <p style={styles.footnote}>
        Efficiency calculated using the Uniswap v3 concentrated liquidity
        formula. Higher efficiency requires more frequent range management.
      </p>
    </div>
  );
}

const styles: Record<string, React.CSSProperties> = {
  root: { width: "100%", fontFamily: "inherit" },
  heading: { fontSize: 15, fontWeight: 600, marginBottom: 14, color: "#e6edf3" },
  resultRow: {
    display: "flex",
    alignItems: "center",
    gap: 16,
    marginBottom: 12,
  },
  bigNumber: {
    fontSize: 40,
    fontWeight: 700,
    color: "#58a6ff",
    lineHeight: 1,
    minWidth: 90,
  },
  bigUnit: { fontSize: 20, marginLeft: 2 },
  resultCaption: { fontSize: 13, color: "#8b949e", lineHeight: 1.4 },
  barWrap: {
    height: 6,
    background: "#21262d",
    borderRadius: 3,
    overflow: "hidden",
    marginBottom: 16,
  },
  barFill: {
    height: "100%",
    background: "linear-gradient(90deg, #1a4a2e, #3fb950)",
    borderRadius: 3,
    transition: "width 0.3s",
  },
  table: {
    width: "100%",
    borderCollapse: "collapse",
    fontSize: 13,
    marginBottom: 16,
  },
  th: {
    textAlign: "left",
    padding: "8px 10px",
    fontSize: 11,
    textTransform: "uppercase",
    letterSpacing: ".05em",
    color: "#8b949e",
    borderBottom: "1px solid #30363d",
  },
  td: {
    padding: "10px 10px",
    color: "#e6edf3",
    borderBottom: "1px solid #30363d",
  },
  infoGrid: {
    display: "grid",
    gridTemplateColumns: "repeat(2, 1fr)",
    gap: 10,
    marginBottom: 16,
  },
  infoCell: {
    background: "#161b22",
    border: "1px solid #30363d",
    borderRadius: 6,
    padding: "8px 10px",
  },
  infoLabel: { fontSize: 11, color: "#8b949e", marginBottom: 2 },
  infoValue: { fontSize: 14, fontWeight: 600, color: "#e6edf3" },
  editRow: {
    display: "flex",
    gap: 8,
    alignItems: "center",
    flexWrap: "wrap",
    marginBottom: 12,
  },
  editLabel: { fontSize: 12, color: "#8b949e" },
  input: {
    width: 120,
    background: "#161b22",
    border: "1px solid #30363d",
    color: "#e6edf3",
    borderRadius: 6,
    padding: "5px 8px",
    fontSize: 13,
    outline: "none",
  },
  footnote: { fontSize: 11, color: "#8b949e", lineHeight: 1.4 },
};
