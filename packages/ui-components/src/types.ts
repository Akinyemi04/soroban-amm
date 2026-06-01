/** Fee tier definition for a pool. */
export interface FeeTier {
  /** Swap fee in basis points (e.g. 5 = 0.05%, 30 = 0.30%, 100 = 1.00%). */
  feeBps: number;
  /** Human-readable label shown in the UI. */
  label: string;
  /** Short description of the tier's typical use-case. */
  description: string;
  /** Expected TVL share across the protocol (for comparison display). */
  tvlSharePct?: number;
}

/** A price range expressed as [lower, upper] in pool price units. */
export interface PriceRange {
  lower: number;
  upper: number;
}

/** Liquidity position details. */
export interface Position {
  id: string;
  poolId: string;
  owner: string;
  tokenA: string;
  tokenB: string;
  /** Amount of tokenA deposited. */
  amountA: number;
  /** Amount of tokenB deposited. */
  amountB: number;
  /** Active price range for concentrated liquidity (undefined for v2 positions). */
  priceRange?: PriceRange;
  /** Fee tier in basis points. */
  feeBps: number;
  /** Current estimated USD value. */
  valueUsd: number;
  /** Uncollected fees in USD. */
  feesEarnedUsd: number;
}

/** Risk level classification. */
export type RiskLevel = "low" | "medium" | "high" | "critical";

/** Risk assessment for a position or pool. */
export interface RiskAssessment {
  level: RiskLevel;
  score: number;
  factors: RiskFactor[];
}

/** An individual risk factor contributing to a risk assessment. */
export interface RiskFactor {
  name: string;
  description: string;
  severity: RiskLevel;
  value?: number;
  threshold?: number;
}

/** Props common to all position management components. */
export interface BaseProps {
  /** Additional CSS class names. */
  className?: string;
  /** Accessible label override. */
  "aria-label"?: string;
}

// ─── Governance types ─────────────────────────────────────────────────────────

export type ProposalStatus = "active" | "passed" | "failed" | "pending";
export type ProposalCategory = "protocol" | "treasury" | "params" | "meta" | "other";
export type VoteChoice = "for" | "against" | "abstain";

export interface OnChainAction {
  contractId: string;
  functionName: string;
  /** Serialised JSON arguments. */
  args: string;
}

export interface Proposal {
  id: string;
  title: string;
  category: ProposalCategory;
  status: ProposalStatus;
  summary: string;
  body: string;
  forVotes: number;
  againstVotes: number;
  /** Minimum participation as % of circulating supply. */
  quorumPct: number;
  /** ISO date string (YYYY-MM-DD). */
  created: string;
  /** ISO date string (YYYY-MM-DD). */
  ends: string;
  author: string;
  actions?: OnChainAction[];
}

export interface Delegate {
  name: string;
  address: string;
  votingPower: string;
  participationRate: string;
  bio: string;
}
