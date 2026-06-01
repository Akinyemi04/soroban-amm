/**
 * PositionManager — composite component for managing a concentrated liquidity
 * or full-range AMM position.
 *
 * Combines RangeSelector, FeeTierComparison, CapitalEfficiencyCalc, and
 * RiskIndicator into a single form-like panel. Designed to be dropped into
 * any React or Next.js / Vite application with no framework-specific
 * dependencies.
 *
 * Accessibility: WCAG 2.1 AA. Uses a single <form> landmark with a visible
 * heading. All interactive controls have visible labels. Live regions
 * announce result changes.
 */

import React, { useCallback, useId, useState } from "react";
import { RangeSelector } from "./RangeSelector.js";
import { FeeTierComparison } from "./FeeTierComparison.js";
import { CapitalEfficiencyCalc } from "./CapitalEfficiencyCalc.js";
import { RiskIndicator, assessPositionRisk } from "./RiskIndicator.js";
import type { BaseProps, FeeTier, PriceRange, Position } from "./types.js";

export interface PositionManagerProps extends BaseProps {
  /** Existing position to edit, or undefined to create a new one. */
  position?: Partial<Position>;
  /** Current spot price of the pool. */
  currentPrice: number;
  /** Available fee tiers. Defaults to standard 0.05/0.30/1.00% tiers. */
  feeTiers?: FeeTier[];
  /** Token symbol A. */
  tokenA?: string;
  /** Token symbol B. */
  tokenB?: string;
  /** Price deviation from TWAP in basis points (for risk assessment). */
  priceDeviationBps?: number;
  /** Pool TVL in USD. */
  poolTvl?: number;
  /** 24-hour pool volume in USD. */
  poolVolume24h?: number;
  /** Called when the user submits the form. */
  onSubmit?: (values: PositionFormValues) => void;
  /** Called when the user cancels. */
  onCancel?: () => void;
}

export interface PositionFormValues {
  feeBps: number;
  priceRange: PriceRange;
  amountA: number;
  amountB: number;
}

const DEFAULT_TIERS: FeeTier[] = [
  {
    feeBps: 5,
    label: "Stable",
    description: "For stable pairs with very low price movement.",
    tvlSharePct: 15,
  },
  {
    feeBps: 30,
    label: "Standard",
    description: "Best for most pairs. Balanced fee income and volume.",
    tvlSharePct: 60,
  },
  {
    feeBps: 100,
    label: "Exotic",
    description: "High-volatility or exotic pairs with low liquidity.",
    tvlSharePct: 25,
  },
];

export function PositionManager({
  position,
  currentPrice,
  feeTiers = DEFAULT_TIERS,
  tokenA = "Token A",
  tokenB = "Token B",
  priceDeviationBps = 0,
  poolTvl = 0,
  poolVolume24h = 0,
  onSubmit,
  onCancel,
  className = "",
  "aria-label": ariaLabel,
}: PositionManagerProps) {
  const formId = useId();

  const defaultRange: PriceRange = {
    lower: position?.priceRange?.lower ?? currentPrice * 0.8,
    upper: position?.priceRange?.upper ?? currentPrice * 1.2,
  };

  const [feeBps, setFeeBps] = useState(position?.feeBps ?? 30);
  const [priceRange, setPriceRange] = useState<PriceRange>(defaultRange);
  const [amountA, setAmountA] = useState(position?.amountA ?? 0);
  const [amountB, setAmountB] = useState(position?.amountB ?? 0);
  const [activeTab, setActiveTab] = useState<"range" | "fee" | "efficiency" | "risk">("range");

  const minPrice = currentPrice * 0.01;
  const maxPrice = currentPrice * 10;

  const isInRange =
    currentPrice >= priceRange.lower && currentPrice <= priceRange.upper;
  const rangePct =
    ((priceRange.upper - priceRange.lower) / currentPrice) * 100;

  const risk = assessPositionRisk({
    priceDeviationBps,
    rangePct,
    tvl: poolTvl,
    volume24h: poolVolume24h,
    isInRange,
    feeBps,
  });

  const handleSubmit = useCallback(
    (e: React.FormEvent) => {
      e.preventDefault();
      onSubmit?.({ feeBps, priceRange, amountA, amountB });
    },
    [feeBps, priceRange, amountA, amountB, onSubmit],
  );

  const tabs: { key: typeof activeTab; label: string }[] = [
    { key: "range", label: "Price Range" },
    { key: "fee", label: "Fee Tier" },
    { key: "efficiency", label: "Efficiency" },
    { key: "risk", label: `Risk ${risk.level !== "low" ? "⚠" : ""}` },
  ];

  return (
    <form
      id={formId}
      onSubmit={handleSubmit}
      aria-label={ariaLabel ?? "Position manager"}
      className={className}
      style={styles.root}
      noValidate
    >
      <div style={styles.header}>
        <h2 style={styles.heading}>
          {position?.id ? "Edit Position" : "New Position"} &mdash; {tokenA}/{tokenB}
        </h2>
        <span style={styles.badge}>
          {feeBps / 100}% fee
        </span>
      </div>

      {/* Tab navigation */}
      <div role="tablist" aria-label="Position configuration tabs" style={styles.tabList}>
        {tabs.map((t) => (
          <button
            key={t.key}
            type="button"
            role="tab"
            aria-selected={activeTab === t.key}
            aria-controls={`${formId}-panel-${t.key}`}
            id={`${formId}-tab-${t.key}`}
            style={{
              ...styles.tab,
              ...(activeTab === t.key ? styles.tabActive : {}),
            }}
            onClick={() => setActiveTab(t.key)}
          >
            {t.label}
          </button>
        ))}
      </div>

      {/* Tab panels */}
      <div style={styles.panelWrap}>
        <div
          id={`${formId}-panel-range`}
          role="tabpanel"
          aria-labelledby={`${formId}-tab-range`}
          hidden={activeTab !== "range"}
          style={styles.panel}
        >
          <RangeSelector
            minPrice={minPrice}
            maxPrice={maxPrice}
            currentPrice={currentPrice}
            value={priceRange}
            onChange={setPriceRange}
            tokenA={tokenA}
            tokenB={tokenB}
          />
        </div>

        <div
          id={`${formId}-panel-fee`}
          role="tabpanel"
          aria-labelledby={`${formId}-tab-fee`}
          hidden={activeTab !== "fee"}
          style={styles.panel}
        >
          <FeeTierComparison
            tiers={feeTiers}
            selected={feeBps}
            onSelect={setFeeBps}
          />
        </div>

        <div
          id={`${formId}-panel-efficiency`}
          role="tabpanel"
          aria-labelledby={`${formId}-tab-efficiency`}
          hidden={activeTab !== "efficiency"}
          style={styles.panel}
        >
          <CapitalEfficiencyCalc
            currentPrice={currentPrice}
            priceRange={priceRange}
            tokenA={tokenA}
            tokenB={tokenB}
            onRangeChange={setPriceRange}
          />
        </div>

        <div
          id={`${formId}-panel-risk`}
          role="tabpanel"
          aria-labelledby={`${formId}-tab-risk`}
          hidden={activeTab !== "risk"}
          style={styles.panel}
        >
          <RiskIndicator assessment={risk} defaultExpanded />
        </div>
      </div>

      {/* Deposit amounts */}
      <fieldset style={styles.fieldset}>
        <legend style={styles.legend}>Deposit amounts</legend>
        <div style={styles.amountGrid}>
          <label style={styles.amountLabel} htmlFor={`${formId}-amountA`}>
            {tokenA}
            <input
              id={`${formId}-amountA`}
              type="number"
              min={0}
              step="any"
              value={amountA}
              onChange={(e) => setAmountA(Number(e.target.value))}
              style={styles.amountInput}
              aria-label={`${tokenA} deposit amount`}
            />
          </label>
          <label style={styles.amountLabel} htmlFor={`${formId}-amountB`}>
            {tokenB}
            <input
              id={`${formId}-amountB`}
              type="number"
              min={0}
              step="any"
              value={amountB}
              onChange={(e) => setAmountB(Number(e.target.value))}
              style={styles.amountInput}
              aria-label={`${tokenB} deposit amount`}
            />
          </label>
        </div>
      </fieldset>

      {/* Actions */}
      <div style={styles.actions}>
        {onCancel && (
          <button
            type="button"
            onClick={onCancel}
            style={styles.btnSecondary}
          >
            Cancel
          </button>
        )}
        <button type="submit" style={styles.btnPrimary}>
          {position?.id ? "Update Position" : "Add Liquidity"}
        </button>
      </div>
    </form>
  );
}

const styles: Record<string, React.CSSProperties> = {
  root: {
    background: "#161b22",
    border: "1px solid #30363d",
    borderRadius: 10,
    padding: 20,
    fontFamily:
      '-apple-system, BlinkMacSystemFont, "Segoe UI", Helvetica, Arial, sans-serif',
    color: "#e6edf3",
    width: "100%",
    maxWidth: 520,
  },
  header: {
    display: "flex",
    alignItems: "center",
    justifyContent: "space-between",
    marginBottom: 16,
    gap: 10,
  },
  heading: {
    fontSize: 16,
    fontWeight: 700,
    margin: 0,
    color: "#e6edf3",
  },
  badge: {
    fontSize: 12,
    fontWeight: 600,
    background: "#1f3d5a",
    color: "#58a6ff",
    padding: "3px 9px",
    borderRadius: 10,
    whiteSpace: "nowrap",
  },
  tabList: {
    display: "flex",
    gap: 2,
    borderBottom: "1px solid #30363d",
    marginBottom: 16,
    overflowX: "auto",
  },
  tab: {
    background: "transparent",
    border: "none",
    borderBottom: "2px solid transparent",
    color: "#8b949e",
    padding: "8px 14px",
    fontSize: 13,
    cursor: "pointer",
    whiteSpace: "nowrap",
    outline: "none",
    transition: "color 0.15s, border-color 0.15s",
  },
  tabActive: {
    color: "#e6edf3",
    borderBottomColor: "#58a6ff",
  },
  panelWrap: { minHeight: 220, marginBottom: 16 },
  panel: { padding: 0 },
  fieldset: {
    border: "1px solid #30363d",
    borderRadius: 8,
    padding: "12px 14px",
    marginBottom: 16,
  },
  legend: {
    fontSize: 12,
    color: "#8b949e",
    textTransform: "uppercase",
    letterSpacing: ".05em",
    padding: "0 4px",
  },
  amountGrid: {
    display: "grid",
    gridTemplateColumns: "1fr 1fr",
    gap: 12,
  },
  amountLabel: {
    display: "flex",
    flexDirection: "column",
    gap: 5,
    fontSize: 13,
    color: "#e6edf3",
  },
  amountInput: {
    width: "100%",
    background: "#0d1117",
    border: "1px solid #30363d",
    color: "#e6edf3",
    borderRadius: 6,
    padding: "7px 10px",
    fontSize: 14,
    outline: "none",
  },
  actions: {
    display: "flex",
    justifyContent: "flex-end",
    gap: 8,
  },
  btnPrimary: {
    background: "#238636",
    color: "#fff",
    border: "none",
    borderRadius: 6,
    padding: "8px 18px",
    fontSize: 14,
    fontWeight: 600,
    cursor: "pointer",
    outline: "none",
    transition: "opacity 0.15s",
  },
  btnSecondary: {
    background: "transparent",
    color: "#e6edf3",
    border: "1px solid #30363d",
    borderRadius: 6,
    padding: "8px 18px",
    fontSize: 14,
    cursor: "pointer",
    outline: "none",
  },
};
