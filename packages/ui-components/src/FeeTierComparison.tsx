/**
 * FeeTierComparison — side-by-side comparison of available fee tiers.
 *
 * Shows fee bps, expected annual yield estimate, TVL share, and a short
 * description for each tier. The selected tier is highlighted with a focus
 * ring and check mark.
 *
 * Accessibility: WCAG 2.1 AA. Uses radiogroup / radio role pattern so
 * keyboard users can navigate with arrow keys and select with Space/Enter.
 */

import React, { useId } from "react";
import type { BaseProps, FeeTier } from "./types.js";

export interface FeeTierComparisonProps extends BaseProps {
  /** Available fee tiers to display. */
  tiers: FeeTier[];
  /** The currently selected fee tier (by feeBps value). */
  selected: number;
  /** Called when the user selects a tier. */
  onSelect: (feeBps: number) => void;
  /** Current spot volatility hint — used to highlight the recommended tier. */
  volatilityHint?: "stable" | "moderate" | "volatile";
}

function recommendedFor(
  tier: FeeTier,
  volatility: "stable" | "moderate" | "volatile" | undefined,
): boolean {
  if (!volatility) return false;
  if (volatility === "stable" && tier.feeBps <= 5) return true;
  if (volatility === "moderate" && tier.feeBps === 30) return true;
  if (volatility === "volatile" && tier.feeBps >= 100) return true;
  return false;
}

export function FeeTierComparison({
  tiers,
  selected,
  onSelect,
  volatilityHint,
  className = "",
  "aria-label": ariaLabel,
}: FeeTierComparisonProps) {
  const groupId = useId();

  const handleKeyDown = (e: React.KeyboardEvent, idx: number) => {
    if (e.key === "ArrowRight" || e.key === "ArrowDown") {
      e.preventDefault();
      const next = tiers[(idx + 1) % tiers.length];
      if (next) onSelect(next.feeBps);
    } else if (e.key === "ArrowLeft" || e.key === "ArrowUp") {
      e.preventDefault();
      const prev = tiers[(idx - 1 + tiers.length) % tiers.length];
      if (prev) onSelect(prev.feeBps);
    }
  };

  return (
    <div
      role="radiogroup"
      aria-label={ariaLabel ?? "Fee tier comparison"}
      aria-required="true"
      className={className}
      style={styles.root}
    >
      {volatilityHint && (
        <p style={styles.hint}>
          For{" "}
          <strong>{volatilityHint}</strong> pairs, we recommend the highlighted
          tier.
        </p>
      )}

      <div style={styles.grid}>
        {tiers.map((tier, idx) => {
          const isSelected = tier.feeBps === selected;
          const isRecommended = recommendedFor(tier, volatilityHint);
          return (
            <div
              key={tier.feeBps}
              role="radio"
              aria-checked={isSelected}
              aria-label={`${tier.label} — ${tier.feeBps / 100}% fee${isRecommended ? ", recommended" : ""}`}
              tabIndex={isSelected ? 0 : -1}
              id={`${groupId}-tier-${tier.feeBps}`}
              style={{
                ...styles.tile,
                ...(isSelected ? styles.tileSelected : {}),
                ...(isRecommended ? styles.tileRecommended : {}),
              }}
              onClick={() => onSelect(tier.feeBps)}
              onKeyDown={(e) => {
                if (e.key === " " || e.key === "Enter") {
                  e.preventDefault();
                  onSelect(tier.feeBps);
                }
                handleKeyDown(e, idx);
              }}
            >
              {isRecommended && (
                <span style={styles.badge} aria-hidden="true">
                  Recommended
                </span>
              )}
              {isSelected && (
                <span style={styles.check} aria-hidden="true">
                  ✓
                </span>
              )}

              <div style={styles.feeLabel}>{tier.feeBps / 100}%</div>
              <div style={styles.tierName}>{tier.label}</div>
              <p style={styles.desc}>{tier.description}</p>

              {tier.tvlSharePct !== undefined && (
                <div style={styles.tvlRow}>
                  <span style={styles.muted}>TVL share</span>
                  <span>{tier.tvlSharePct.toFixed(0)}%</span>
                </div>
              )}

              <div style={styles.feeBar} aria-hidden="true">
                <div
                  style={{
                    ...styles.feeBarFill,
                    width: `${Math.min(100, (tier.feeBps / 300) * 100)}%`,
                    background: isSelected ? "#58a6ff" : "#30363d",
                  }}
                />
              </div>
            </div>
          );
        })}
      </div>
    </div>
  );
}

const styles: Record<string, React.CSSProperties> = {
  root: { width: "100%" },
  hint: {
    fontSize: 13,
    color: "#8b949e",
    marginBottom: 12,
  },
  grid: {
    display: "grid",
    gridTemplateColumns: "repeat(auto-fill, minmax(160px, 1fr))",
    gap: 10,
  },
  tile: {
    position: "relative",
    background: "#161b22",
    border: "2px solid #30363d",
    borderRadius: 8,
    padding: "14px 12px",
    cursor: "pointer",
    outline: "none",
    transition: "border-color 0.15s, background 0.15s",
  },
  tileSelected: {
    borderColor: "#58a6ff",
    background: "#0d2035",
  },
  tileRecommended: {
    borderColor: "#3fb950",
  },
  badge: {
    position: "absolute",
    top: -1,
    right: -1,
    fontSize: 10,
    fontWeight: 700,
    background: "#1f4a2e",
    color: "#3fb950",
    padding: "2px 6px",
    borderRadius: "0 6px 0 4px",
    textTransform: "uppercase",
    letterSpacing: ".04em",
  },
  check: {
    position: "absolute",
    top: 10,
    right: 10,
    color: "#58a6ff",
    fontWeight: 700,
    fontSize: 14,
  },
  feeLabel: {
    fontSize: 24,
    fontWeight: 700,
    color: "#e6edf3",
    marginBottom: 2,
  },
  tierName: {
    fontSize: 11,
    fontWeight: 600,
    textTransform: "uppercase",
    letterSpacing: ".05em",
    color: "#8b949e",
    marginBottom: 8,
  },
  desc: {
    fontSize: 12,
    color: "#8b949e",
    marginBottom: 10,
    lineHeight: 1.4,
  },
  tvlRow: {
    display: "flex",
    justifyContent: "space-between",
    fontSize: 12,
    marginBottom: 8,
    color: "#e6edf3",
  },
  muted: { color: "#8b949e" },
  feeBar: {
    height: 4,
    background: "#21262d",
    borderRadius: 2,
    overflow: "hidden",
  },
  feeBarFill: {
    height: "100%",
    borderRadius: 2,
    transition: "width 0.3s, background 0.3s",
  },
};
