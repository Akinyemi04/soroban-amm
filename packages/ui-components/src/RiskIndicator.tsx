/**
 * RiskIndicator — displays a risk assessment for a position or pool.
 *
 * Shows an overall risk level, a numeric score, and a breakdown of
 * individual risk factors with descriptions and severity badges.
 *
 * Accessibility: WCAG 2.1 AA. Risk level is announced via aria-live
 * region. Severity badges use color + text to convey meaning (not color
 * alone). Expandable details use aria-expanded / aria-controls.
 */

import React, { useState, useId } from "react";
import type { BaseProps, RiskAssessment, RiskFactor, RiskLevel } from "./types.js";

export interface RiskIndicatorProps extends BaseProps {
  /** The risk assessment to display. */
  assessment: RiskAssessment;
  /** Show the factor breakdown by default. Default false. */
  defaultExpanded?: boolean;
}

const levelColor: Record<RiskLevel, string> = {
  low:      "#3fb950",
  medium:   "#d29922",
  high:     "#f0883e",
  critical: "#f85149",
};

const levelBg: Record<RiskLevel, string> = {
  low:      "#1f4a2e",
  medium:   "#3d2e00",
  high:     "#3d1f00",
  critical: "#3d1a1a",
};

const levelLabel: Record<RiskLevel, string> = {
  low:      "Low Risk",
  medium:   "Medium Risk",
  high:     "High Risk",
  critical: "Critical Risk",
};

function SeverityBadge({ level }: { level: RiskLevel }) {
  return (
    <span
      aria-label={`Severity: ${levelLabel[level]}`}
      style={{
        display: "inline-block",
        fontSize: 10,
        fontWeight: 700,
        padding: "2px 7px",
        borderRadius: 10,
        textTransform: "uppercase",
        letterSpacing: ".04em",
        background: levelBg[level],
        color: levelColor[level],
      }}
    >
      {level}
    </span>
  );
}

function ScoreGauge({ score, level }: { score: number; level: RiskLevel }) {
  const radius = 28;
  const circ = 2 * Math.PI * radius;
  const offset = circ * (1 - score / 100);
  const color = levelColor[level];

  return (
    <svg
      viewBox="0 0 70 70"
      width={70}
      height={70}
      aria-hidden="true"
      role="img"
      style={{ flexShrink: 0 }}
    >
      <circle cx="35" cy="35" r={radius} fill="none" stroke="#21262d" strokeWidth="7" />
      <circle
        cx="35"
        cy="35"
        r={radius}
        fill="none"
        stroke={color}
        strokeWidth="7"
        strokeLinecap="round"
        strokeDasharray={circ}
        strokeDashoffset={offset}
        transform="rotate(-90 35 35)"
      />
      <text
        x="35"
        y="35"
        textAnchor="middle"
        dominantBaseline="central"
        fontSize="14"
        fontWeight="700"
        fill={color}
      >
        {score}
      </text>
    </svg>
  );
}

function FactorRow({ factor }: { factor: RiskFactor }) {
  return (
    <li style={styles.factorRow}>
      <div style={styles.factorHeader}>
        <span style={styles.factorName}>{factor.name}</span>
        <SeverityBadge level={factor.severity} />
      </div>
      <p style={styles.factorDesc}>{factor.description}</p>
      {factor.value !== undefined && factor.threshold !== undefined && (
        <div style={styles.factorBar} aria-hidden="true">
          <div
            style={{
              ...styles.factorBarFill,
              width: `${Math.min(100, (factor.value / factor.threshold) * 100)}%`,
              background: levelColor[factor.severity],
            }}
          />
        </div>
      )}
    </li>
  );
}

export function RiskIndicator({
  assessment,
  defaultExpanded = false,
  className = "",
  "aria-label": ariaLabel,
}: RiskIndicatorProps) {
  const [expanded, setExpanded] = useState(defaultExpanded);
  const id = useId();
  const detailsId = `${id}-details`;

  return (
    <div
      className={className}
      style={{
        ...styles.root,
        borderColor: levelColor[assessment.level],
      }}
      aria-label={ariaLabel ?? "Risk indicator"}
    >
      {/* Header row */}
      <div style={styles.headerRow}>
        <ScoreGauge score={assessment.score} level={assessment.level} />

        <div style={{ flex: 1 }}>
          <div
            role="status"
            aria-live="polite"
            aria-label={`Overall risk: ${levelLabel[assessment.level]}, score ${assessment.score} out of 100`}
            style={{ display: "flex", alignItems: "center", gap: 8, marginBottom: 6 }}
          >
            <span style={{ ...styles.levelText, color: levelColor[assessment.level] }}>
              {levelLabel[assessment.level]}
            </span>
            <span style={styles.scoreLabel}>Score: {assessment.score}/100</span>
          </div>

          <p style={styles.factorSummary}>
            {assessment.factors.length} risk factor
            {assessment.factors.length !== 1 ? "s" : ""} identified
            {assessment.factors.filter((f) => f.severity === "critical").length > 0
              ? ` — ${assessment.factors.filter((f) => f.severity === "critical").length} critical`
              : ""}
          </p>
        </div>

        {assessment.factors.length > 0 && (
          <button
            type="button"
            aria-expanded={expanded}
            aria-controls={detailsId}
            onClick={() => setExpanded((e) => !e)}
            style={styles.toggleBtn}
          >
            {expanded ? "Hide details" : "Show details"}
          </button>
        )}
      </div>

      {/* Factor breakdown */}
      {expanded && (
        <div id={detailsId}>
          <hr style={styles.divider} />
          <ul style={styles.factorList} aria-label="Risk factors">
            {assessment.factors.map((f, i) => (
              <FactorRow key={i} factor={f} />
            ))}
          </ul>
        </div>
      )}
    </div>
  );
}

/** Derives a RiskAssessment from common position metrics. */
export function assessPositionRisk(params: {
  priceDeviationBps: number;
  rangePct: number;
  tvl: number;
  volume24h: number;
  isInRange: boolean;
  feeBps: number;
}): RiskAssessment {
  const factors: RiskFactor[] = [];
  let score = 100;

  if (!params.isInRange) {
    factors.push({
      name: "Out of range",
      description: "Current price is outside your selected range. The position is inactive and earning no fees.",
      severity: "critical",
    });
    score -= 40;
  }

  if (params.priceDeviationBps > 200) {
    const sev: RiskLevel = params.priceDeviationBps > 500 ? "high" : "medium";
    factors.push({
      name: "High price deviation",
      description: `Price has deviated ${(params.priceDeviationBps / 100).toFixed(1)}% from TWAP, indicating unusual volatility.`,
      severity: sev,
      value: params.priceDeviationBps,
      threshold: 500,
    });
    score -= params.priceDeviationBps > 500 ? 25 : 10;
  }

  if (params.rangePct < 5) {
    factors.push({
      name: "Very narrow range",
      description: `Range spans only ${params.rangePct.toFixed(1)}% around current price. High efficiency but high rebalancing risk.`,
      severity: "medium",
      value: params.rangePct,
      threshold: 5,
    });
    score -= 15;
  }

  if (params.tvl < 10_000) {
    factors.push({
      name: "Low TVL",
      description: "Pool TVL below $10,000. Thin liquidity may cause higher slippage.",
      severity: params.tvl < 1_000 ? "high" : "low",
      value: params.tvl,
      threshold: 10_000,
    });
    score -= params.tvl < 1_000 ? 20 : 5;
  }

  score = Math.max(0, Math.min(100, score));
  const level: RiskLevel =
    score >= 80 ? "low" : score >= 55 ? "medium" : score >= 30 ? "high" : "critical";

  return { level, score, factors };
}

const styles: Record<string, React.CSSProperties> = {
  root: {
    background: "#161b22",
    border: "1px solid",
    borderRadius: 8,
    padding: 16,
    fontFamily: "inherit",
    width: "100%",
  },
  headerRow: {
    display: "flex",
    alignItems: "center",
    gap: 14,
  },
  levelText: {
    fontSize: 15,
    fontWeight: 700,
  },
  scoreLabel: {
    fontSize: 12,
    color: "#8b949e",
  },
  factorSummary: {
    fontSize: 12,
    color: "#8b949e",
  },
  toggleBtn: {
    background: "transparent",
    border: "1px solid #30363d",
    color: "#8b949e",
    borderRadius: 6,
    padding: "4px 10px",
    fontSize: 12,
    cursor: "pointer",
    outline: "none",
    whiteSpace: "nowrap",
  },
  divider: {
    border: "none",
    borderTop: "1px solid #30363d",
    margin: "12px 0",
  },
  factorList: {
    listStyle: "none",
    padding: 0,
    margin: 0,
    display: "flex",
    flexDirection: "column",
    gap: 10,
  },
  factorRow: {
    background: "#0d1117",
    border: "1px solid #30363d",
    borderRadius: 6,
    padding: "10px 12px",
  },
  factorHeader: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    marginBottom: 4,
  },
  factorName: { fontSize: 13, fontWeight: 600, color: "#e6edf3" },
  factorDesc: { fontSize: 12, color: "#8b949e", marginBottom: 6, lineHeight: 1.4 },
  factorBar: {
    height: 4,
    background: "#21262d",
    borderRadius: 2,
    overflow: "hidden",
  },
  factorBarFill: {
    height: "100%",
    borderRadius: 2,
    transition: "width 0.3s",
  },
};
