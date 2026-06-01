/**
 * GovernanceForum — decentralized governance forum & voting UI for Soroban AMM.
 *
 * Features:
 *   - Proposal creation wizard (4 steps)
 *   - Vote casting (For / Against / Abstain) without requiring a pre-loaded wallet
 *   - Vote delegation UI with recommended delegate list
 *   - Voting power calculator with lock-duration multiplier
 *   - Proposal history & analytics (participation bars, outcome/category breakdown)
 *
 * Accessibility: WCAG 2.1 AA compliant.
 *   - Full keyboard navigation; all interactive elements have unique IDs and labels.
 *   - aria-live regions for vote feedback and toast notifications.
 *   - Color is never the sole conveyor of meaning (text labels accompany all badges).
 *   - Focus management on modal open/close.
 *
 * Works without a web3 wallet pre-loaded — read-only mode is fully functional.
 * Wallet connection toggles additional write operations.
 */

import React, {
  useState,
  useEffect,
  useRef,
  useCallback,
  useId,
  type KeyboardEvent,
} from "react";
import type { BaseProps } from "./types.js";

// ─── Domain types ────────────────────────────────────────────────────────────

export type ProposalStatus = "active" | "passed" | "failed" | "pending";
export type ProposalCategory = "protocol" | "treasury" | "params" | "meta" | "other";
export type VoteChoice = "for" | "against" | "abstain";

export interface OnChainAction {
  contractId: string;
  functionName: string;
  args: string; // JSON string
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
  /** Quorum as percentage of circulating supply required for the vote to be valid. */
  quorumPct: number;
  /** ISO date string */
  created: string;
  /** ISO date string */
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

export interface GovernanceForumProps extends BaseProps {
  /** Initial proposals to display. */
  proposals?: Proposal[];
  /** Initial delegate list. */
  delegates?: Delegate[];
  /** Total circulating supply for voting-power % calculations. */
  totalSupply?: number;
  /** Called when a new proposal is submitted via the wizard. */
  onProposalSubmit?: (proposal: Omit<Proposal, "id" | "forVotes" | "againstVotes">) => void;
  /** Called when the user casts a vote. */
  onVote?: (proposalId: string, choice: VoteChoice) => void;
  /** Called when the user delegates to an address. */
  onDelegate?: (address: string) => void;
  /** Called when "Connect Wallet" is pressed. */
  onConnectWallet?: () => void;
  /** Whether a wallet is currently connected. */
  walletConnected?: boolean;
  /** Connected wallet address (shown in header when connected). */
  walletAddress?: string;
  /** Voting power of the connected wallet. */
  walletVotingPower?: number;
}

// ─── Default seed data ────────────────────────────────────────────────────────

const DEFAULT_PROPOSALS: Proposal[] = [
  {
    id: "GIP-012",
    title: "Increase max pool fee to 1.5%",
    category: "params",
    status: "active",
    summary:
      "Raise the ceiling on swap fees from 1.0% to 1.5% to allow high-volatility pair pools to compete.",
    body: "Currently the maximum swap fee is capped at 1.0%, which creates pricing pressure issues for highly volatile asset pairs. Raising the ceiling allows pool owners to set appropriate fees while keeping defaults unchanged.",
    forVotes: 8_200_000,
    againstVotes: 3_100_000,
    quorumPct: 10,
    created: "2026-05-25",
    ends: "2026-06-05",
    author: "GAXYZ…1234",
  },
  {
    id: "GIP-011",
    title: "Deploy liquidity mining incentives Season 4",
    category: "treasury",
    status: "active",
    summary:
      "Allocate 2.4M AMM tokens from treasury to fund the next three-month liquidity mining program.",
    body: "Season 3 concluded with TVL growth of 43%. This proposal requests continued incentive funding for key pairs: XLM/USDC, BTC/XLM, and ETH/XLM.",
    forVotes: 12_500_000,
    againstVotes: 900_000,
    quorumPct: 10,
    created: "2026-05-22",
    ends: "2026-06-03",
    author: "GBWXY…5678",
  },
  {
    id: "GIP-010",
    title: "Upgrade AMM core to v2.1 (concentrated liquidity)",
    category: "protocol",
    status: "pending",
    summary: "Adopt the Soroban SDK v21 concentrated liquidity primitives across all pool types.",
    body: "Full spec in forum post #2894. Timeline: testnet deploy June 15, mainnet vote June 30.",
    forVotes: 0,
    againstVotes: 0,
    quorumPct: 15,
    created: "2026-06-01",
    ends: "2026-06-14",
    author: "GCABC…9012",
  },
  {
    id: "GIP-009",
    title: "Reduce protocol fee to 0.05%",
    category: "params",
    status: "passed",
    summary: "Lower the base protocol fee from 0.10% to 0.05% to improve competitiveness.",
    body: "Analysis shows fee reduction increases volume by ~30% based on elasticity modelling.",
    forVotes: 16_800_000,
    againstVotes: 2_200_000,
    quorumPct: 10,
    created: "2026-04-10",
    ends: "2026-04-17",
    author: "GDUVW…3456",
  },
  {
    id: "GIP-008",
    title: "Emergency pause of XYZ/USDC pool",
    category: "protocol",
    status: "failed",
    summary: "Temporarily pause the XYZ/USDC pool due to oracle manipulation risk.",
    body: "Insufficient consensus was reached before the risk window closed.",
    forVotes: 4_500_000,
    againstVotes: 9_200_000,
    quorumPct: 10,
    created: "2026-03-02",
    ends: "2026-03-05",
    author: "GFQRS…7890",
  },
];

const DEFAULT_DELEGATES: Delegate[] = [
  {
    name: "StellarDev",
    address: "GAAA…1111",
    votingPower: "2.4M VP",
    participationRate: "98%",
    bio: "Protocol contributor, 3y governance veteran",
  },
  {
    name: "AMMResearch",
    address: "GBBB…2222",
    votingPower: "1.1M VP",
    participationRate: "94%",
    bio: "Independent researcher, focus on tokenomics",
  },
  {
    name: "LiquidityDAO",
    address: "GCCC…3333",
    votingPower: "3.8M VP",
    participationRate: "88%",
    bio: "Representing 42 community delegates",
  },
  {
    name: "SorobanLabs",
    address: "GDDD…4444",
    votingPower: "0.8M VP",
    participationRate: "100%",
    bio: "Core team delegate (recused on team proposals)",
  },
];

// ─── Helpers ─────────────────────────────────────────────────────────────────

function fmtNum(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + "M";
  if (n >= 1_000) return (n / 1_000).toFixed(1) + "K";
  return n.toLocaleString();
}

// ─── Styles (inline, zero-dependency) ────────────────────────────────────────

const T = {
  bg: "#0b0f1a",
  surface: "#111827",
  card: "#161d2f",
  border: "rgba(255,255,255,.08)",
  accent: "#6c63ff",
  accent2: "#00d4aa",
  danger: "#f04e4e",
  warn: "#f4a93b",
  text: "#e2e8f0",
  muted: "#64748b",
  radius: "12px",
} as const;

const S: Record<string, React.CSSProperties> = {
  root: { fontFamily: "'Inter',system-ui,sans-serif", background: T.bg, color: T.text, minHeight: "100vh" },
  header: { position: "sticky", top: 0, zIndex: 100, background: "rgba(11,15,26,.9)", backdropFilter: "blur(16px)", borderBottom: `1px solid ${T.border}`, padding: "0 1.5rem", display: "flex", alignItems: "center", justifyContent: "space-between", height: 64 },
  main: { maxWidth: 1180, margin: "0 auto", padding: "2rem 1.25rem 4rem" },
  statsRow: { display: "grid", gridTemplateColumns: "repeat(auto-fit,minmax(170px,1fr))", gap: "1rem", marginBottom: "2rem" },
  statCard: { background: T.card, border: `1px solid ${T.border}`, borderRadius: T.radius, padding: "1.25rem 1.5rem" },
  statLabel: { fontSize: ".75rem", color: T.muted, textTransform: "uppercase" as const, letterSpacing: ".05em", marginBottom: ".4rem" },
  statVal: { fontSize: "1.6rem", fontWeight: 700 },
  statSub: { fontSize: ".75rem", color: T.muted, marginTop: ".2rem" },
  tabsWrap: { display: "flex", gap: ".25rem", background: T.surface, borderRadius: 10, padding: ".25rem", marginBottom: "2rem", flexWrap: "wrap" as const, width: "fit-content" },
  proposalCard: { background: T.card, border: `1px solid ${T.border}`, borderRadius: T.radius, padding: "1.4rem 1.6rem", cursor: "pointer", position: "relative" as const, overflow: "hidden" as const, marginBottom: "1rem", transition: "border-color .2s,transform .2s" },
  badge: { fontSize: ".7rem", fontWeight: 600, padding: ".2rem .6rem", borderRadius: 20, textTransform: "uppercase" as const, letterSpacing: ".05em", display: "inline-block" },
  btn: { display: "inline-flex", alignItems: "center", gap: ".4rem", padding: ".5rem 1.1rem", borderRadius: 8, fontSize: ".875rem", fontWeight: 500, border: "none", cursor: "pointer", fontFamily: "inherit", transition: "all .2s" },
  input: { width: "100%", background: T.card, border: `1px solid ${T.border}`, borderRadius: 8, color: T.text, fontFamily: "inherit", fontSize: ".9rem", padding: ".65rem .9rem", outline: "none" },
  overlay: { position: "fixed" as const, inset: 0, background: "rgba(0,0,0,.7)", backdropFilter: "blur(4px)", zIndex: 200, display: "flex", alignItems: "center", justifyContent: "center", padding: "1rem" },
  modal: { background: T.surface, border: `1px solid ${T.border}`, borderRadius: 16, maxWidth: 700, width: "100%", maxHeight: "90vh", overflowY: "auto" as const, padding: "2rem", boxShadow: "0 4px 24px rgba(0,0,0,.4)" },
  delegateCard: { background: T.card, border: `1px solid ${T.border}`, borderRadius: T.radius, padding: "1.25rem", display: "flex", alignItems: "center", gap: "1rem", marginBottom: ".75rem" },
  avatar: { width: 42, height: 42, borderRadius: "50%", background: `linear-gradient(135deg,${T.accent},${T.accent2})`, display: "flex", alignItems: "center", justifyContent: "center", fontWeight: 700, color: "#fff", fontSize: ".9rem", flexShrink: 0 },
  chartCard: { background: T.card, border: `1px solid ${T.border}`, borderRadius: T.radius, padding: "1.25rem", marginBottom: "1rem" },
};

// ─── Sub-components ───────────────────────────────────────────────────────────

function VoteBar({ forVotes, againstVotes }: { forVotes: number; againstVotes: number }) {
  const total = forVotes + againstVotes;
  const forPct = total ? Math.round((forVotes / total) * 100) : 0;
  const againstPct = 100 - forPct;
  return (
    <div>
      <div style={{ display: "flex", justifyContent: "space-between", fontSize: ".75rem", marginBottom: ".3rem" }}>
        <span style={{ color: T.accent2 }}>✓ {fmtNum(forVotes)} ({forPct}%)</span>
        <span style={{ color: T.danger }}>✗ {fmtNum(againstVotes)} ({againstPct}%)</span>
      </div>
      <div style={{ height: 6, borderRadius: 3, background: "rgba(255,255,255,.08)", position: "relative", overflow: "hidden" }}
           role="img" aria-label={`Voting: ${forPct}% for, ${againstPct}% against`}>
        <div style={{ position: "absolute", left: 0, top: 0, height: "100%", width: `${forPct}%`, background: `linear-gradient(90deg,${T.accent2},${T.accent})`, borderRadius: 3 }} />
        <div style={{ position: "absolute", right: 0, top: 0, height: "100%", width: `${againstPct}%`, background: T.danger, borderRadius: 3 }} />
      </div>
    </div>
  );
}

function StatusBadge({ status }: { status: ProposalStatus }) {
  const map: Record<ProposalStatus, { bg: string; color: string }> = {
    active:  { bg: "rgba(0,212,170,.15)",   color: T.accent2 },
    passed:  { bg: "rgba(108,99,255,.15)",  color: T.accent  },
    failed:  { bg: "rgba(240,78,78,.15)",   color: T.danger  },
    pending: { bg: "rgba(244,169,59,.15)",  color: T.warn    },
  };
  const { bg, color } = map[status];
  return <span style={{ ...S.badge, background: bg, color }}>{status}</span>;
}

// ─── Toast ────────────────────────────────────────────────────────────────────

interface ToastItem { id: number; msg: string; type: "success" | "error" }

function Toasts({ items, onRemove }: { items: ToastItem[]; onRemove: (id: number) => void }) {
  return (
    <div style={{ position: "fixed", bottom: "1.5rem", right: "1.5rem", display: "flex", flexDirection: "column", gap: ".5rem", zIndex: 999 }}
         aria-live="polite" aria-atomic="true">
      {items.map(t => (
        <div key={t.id} role="alert"
          style={{ background: T.surface, border: `1px solid ${T.border}`, borderLeft: `3px solid ${t.type === "success" ? T.accent2 : T.danger}`, borderRadius: 10, padding: ".75rem 1.1rem", fontSize: ".875rem", boxShadow: "0 4px 24px rgba(0,0,0,.4)", display: "flex", alignItems: "center", gap: ".5rem", maxWidth: 320 }}>
          {t.type === "success" ? "✓ " : "✕ "}
          <span dangerouslySetInnerHTML={{ __html: t.msg }} />
        </div>
      ))}
    </div>
  );
}

// ─── Proposal Detail Modal ────────────────────────────────────────────────────

function ProposalModal({
  proposal,
  userVote,
  onClose,
  onVote,
}: {
  proposal: Proposal | null;
  userVote?: VoteChoice;
  onClose: () => void;
  onVote: (id: string, choice: VoteChoice) => void;
}) {
  const headingRef = useRef<HTMLHeadingElement>(null);
  useEffect(() => { if (proposal) headingRef.current?.focus(); }, [proposal]);

  const handleOverlayKey = (e: KeyboardEvent<HTMLDivElement>) => { if (e.key === "Escape") onClose(); };
  const handleOverlayClick = (e: React.MouseEvent<HTMLDivElement>) => { if (e.target === e.currentTarget) onClose(); };

  if (!proposal) return null;
  const total = proposal.forVotes + proposal.againstVotes;
  const forPct = total ? Math.round((proposal.forVotes / total) * 100) : 0;

  return (
    <div style={S.overlay} role="dialog" aria-modal="true" aria-labelledby="modal-title"
         onClick={handleOverlayClick} onKeyDown={handleOverlayKey}>
      <div style={S.modal}>
        <div style={{ display: "flex", alignItems: "flex-start", justifyContent: "space-between", gap: "1rem", marginBottom: "1.5rem" }}>
          <div>
            <div style={{ display: "flex", gap: ".5rem", flexWrap: "wrap", marginBottom: ".5rem" }}>
              <StatusBadge status={proposal.status} />
              <span style={{ ...S.badge, background: "rgba(255,255,255,.06)", color: T.muted }}>{proposal.category}</span>
              <span style={{ fontSize: ".75rem", color: T.muted }}>{proposal.id}</span>
            </div>
            <h2 id="modal-title" ref={headingRef} tabIndex={-1} style={{ fontSize: "1.15rem", fontWeight: 700, outline: "none" }}>{proposal.title}</h2>
          </div>
          <button onClick={onClose} aria-label="Close proposal detail"
            style={{ background: "none", border: "none", color: T.muted, fontSize: "1.5rem", cursor: "pointer", lineHeight: 1, padding: ".2rem" }}>✕</button>
        </div>

        <section style={{ marginBottom: "1.5rem" }}>
          <h3 style={{ fontSize: ".8rem", textTransform: "uppercase", letterSpacing: ".08em", color: T.muted, marginBottom: ".75rem" }}>Description</h3>
          <p style={{ fontSize: ".875rem", color: T.muted, lineHeight: 1.7 }}>{proposal.body}</p>
        </section>

        <section style={{ marginBottom: "1.5rem" }}>
          <h3 style={{ fontSize: ".8rem", textTransform: "uppercase", letterSpacing: ".08em", color: T.muted, marginBottom: ".75rem" }}>Voting Progress</h3>
          <div style={{ height: 10, borderRadius: 5, background: "rgba(255,255,255,.08)", overflow: "hidden", marginBottom: ".5rem", position: "relative" }}>
            <div style={{ position: "absolute", left: 0, top: 0, height: "100%", width: `${forPct}%`, background: `linear-gradient(90deg,${T.accent2},${T.accent})`, borderRadius: 5 }} />
            <div style={{ position: "absolute", right: 0, top: 0, height: "100%", width: `${100 - forPct}%`, background: T.danger, borderRadius: 5 }} />
          </div>
          <div style={{ display: "flex", justifyContent: "space-between", fontSize: ".8rem" }}>
            <span style={{ color: T.accent2 }}>✓ For: <strong>{fmtNum(proposal.forVotes)}</strong></span>
            <span>Quorum: <strong>{proposal.quorumPct}%</strong></span>
            <span style={{ color: T.danger }}>✗ Against: <strong>{fmtNum(proposal.againstVotes)}</strong></span>
          </div>
        </section>

        {proposal.status === "active" && (
          <section style={{ marginBottom: "1.5rem" }} aria-label="Cast your vote">
            <h3 style={{ fontSize: ".8rem", textTransform: "uppercase", letterSpacing: ".08em", color: T.muted, marginBottom: ".75rem" }}>Cast Your Vote</h3>
            {userVote && (
              <p style={{ fontSize: ".8rem", color: T.accent, marginBottom: ".75rem" }}>
                ✓ You already voted <strong>{userVote}</strong> on this proposal.
              </p>
            )}
            <div style={{ display: "flex", gap: ".75rem", flexWrap: "wrap" }}>
              {(["for", "against", "abstain"] as VoteChoice[]).map(c => (
                <button key={c}
                  id={`vote-${proposal.id}-${c}`}
                  onClick={() => { onVote(proposal.id, c); onClose(); }}
                  aria-label={`Vote ${c} on ${proposal.id}`}
                  disabled={userVote === c}
                  style={{ ...S.btn, background: c === "for" ? T.accent2 : c === "against" ? T.danger : "transparent", color: c === "abstain" ? T.text : (c === "for" ? "#0b0f1a" : "#fff"), border: c === "abstain" ? `1px solid ${T.border}` : "none", opacity: userVote === c ? 0.5 : 1 }}>
                  {c === "for" ? "👍 Vote For" : c === "against" ? "👎 Vote Against" : "◦ Abstain"}
                </button>
              ))}
            </div>
          </section>
        )}

        <section>
          <h3 style={{ fontSize: ".8rem", textTransform: "uppercase", letterSpacing: ".08em", color: T.muted, marginBottom: ".75rem" }}>Timeline</h3>
          <div style={{ fontSize: ".85rem", color: T.muted, display: "flex", flexDirection: "column", gap: ".3rem" }}>
            <span>Created: {proposal.created}</span>
            <span>{proposal.status === "active" ? "Ends" : "Closed"}: {proposal.ends}</span>
            <span>Author: {proposal.author}</span>
          </div>
        </section>
      </div>
    </div>
  );
}

// ─── Voting Power Calculator ─────────────────────────────────────────────────

function VotingPowerCalc({ totalSupply }: { totalSupply: number }) {
  const [tokens, setTokens] = useState(10000);
  const [lockedPct, setLockedPct] = useState(30);
  const [delegated, setDelegated] = useState(0);
  const [lockMult, setLockMult] = useState(1.5);

  const locked  = Math.round(tokens * lockedPct / 100);
  const base    = (tokens - locked) + locked * lockMult;
  const bonus   = locked * (lockMult - 1);
  const total   = Math.round(base + delegated);
  const share   = ((total / totalSupply) * 100).toFixed(3);
  const pct     = Math.min(total / (totalSupply * 0.05), 1);
  const circ    = 314;
  const offset  = circ * (1 - pct);

  return (
    <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: "1.5rem" }}>
      <div>
        {[
          { label: "AMM Tokens held", id: "calc-tokens", type: "number", value: tokens, onChange: (v: number) => setTokens(v) },
          { label: "Delegated to you", id: "calc-delegated", type: "number", value: delegated, onChange: (v: number) => setDelegated(v) },
        ].map(f => (
          <div key={f.id} style={{ marginBottom: "1.25rem" }}>
            <label htmlFor={f.id} style={{ display: "block", fontSize: ".85rem", fontWeight: 500, marginBottom: ".4rem" }}>{f.label}</label>
            <input id={f.id} type="number" min={0} value={f.value} onChange={e => f.onChange(+e.target.value)} style={S.input} />
          </div>
        ))}

        <div style={{ marginBottom: "1.25rem" }}>
          <label htmlFor="calc-locked-pct" style={{ display: "block", fontSize: ".85rem", fontWeight: 500, marginBottom: ".4rem" }}>
            Tokens locked (multiplier) — <strong>{lockedPct}%</strong>
          </label>
          <input id="calc-locked-pct" type="range" min={0} max={100} value={lockedPct}
            onChange={e => setLockedPct(+e.target.value)}
            style={{ width: "100%", accentColor: T.accent }}
            aria-label={`Percentage of tokens locked: ${lockedPct}%`} />
        </div>

        <div style={{ marginBottom: "1.25rem" }}>
          <label htmlFor="calc-lock-dur" style={{ display: "block", fontSize: ".85rem", fontWeight: 500, marginBottom: ".4rem" }}>Lock duration</label>
          <select id="calc-lock-dur" value={lockMult} onChange={e => setLockMult(+e.target.value)}
            style={{ ...S.input, appearance: "none" as const }}>
            <option value={1}>1 week (1.0×)</option>
            <option value={1.25}>1 month (1.25×)</option>
            <option value={1.5}>3 months (1.5×)</option>
            <option value={2}>1 year (2.0×)</option>
          </select>
        </div>
      </div>

      <div style={{ background: T.card, border: `1px solid ${T.border}`, borderRadius: T.radius, padding: "1.5rem", display: "flex", flexDirection: "column", alignItems: "center", gap: ".5rem", textAlign: "center" }}
           aria-live="polite" aria-label="Calculated voting power">
        <div style={{ width: 120, height: 120, position: "relative", marginBottom: ".5rem" }}>
          <svg viewBox="0 0 120 120" style={{ width: "100%", height: "100%", transform: "rotate(-90deg)" }} aria-hidden="true">
            <circle cx="60" cy="60" r="50" fill="none" stroke="rgba(255,255,255,.06)" strokeWidth="12" />
            <circle cx="60" cy="60" r="50" fill="none" stroke="url(#vpg)" strokeWidth="12"
              strokeDasharray={circ} strokeDashoffset={offset} strokeLinecap="round" style={{ transition: "stroke-dashoffset .5s" }} />
            <defs>
              <linearGradient id="vpg" x1="0%" y1="0%" x2="100%" y2="0%">
                <stop stopColor={T.accent} /><stop offset="1" stopColor={T.accent2} />
              </linearGradient>
            </defs>
          </svg>
          <div style={{ position: "absolute", inset: 0, display: "flex", alignItems: "center", justifyContent: "center", flexDirection: "column" }}>
            <span style={{ fontSize: "1.4rem", fontWeight: 700 }}>{fmtNum(total)}</span>
            <span style={{ fontSize: ".7rem", color: T.muted }}>VP</span>
          </div>
        </div>

        <div style={{ fontSize: ".85rem", color: T.muted }}>Your Voting Power</div>
        <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: ".4rem .75rem", width: "100%", textAlign: "left", marginTop: ".25rem" }}>
          {[
            ["Base", fmtNum(Math.round(base))],
            ["Lock bonus", `+${fmtNum(Math.round(bonus))}`],
            ["Delegated", `+${fmtNum(delegated)}`],
            ["Total VP", fmtNum(total)],
          ].map(([l, v]) => (
            <React.Fragment key={l}>
              <span style={{ fontSize: ".8rem", color: T.muted }}>{l}</span>
              <span style={{ fontSize: l === "Total VP" ? ".85rem" : ".8rem", fontWeight: l === "Total VP" ? 700 : 400, textAlign: "right", color: l === "Total VP" ? T.accent2 : T.text }}>{v}</span>
            </React.Fragment>
          ))}
        </div>
        <div style={{ width: "100%", marginTop: ".75rem", padding: ".75rem", background: "rgba(255,255,255,.03)", borderRadius: 8, fontSize: ".78rem", color: T.muted, textAlign: "left" }}>
          Supply: <strong style={{ color: T.text }}>{fmtNum(totalSupply)}</strong>&nbsp;&nbsp;
          Your share: <strong style={{ color: T.accent2 }}>{share}%</strong>
        </div>
      </div>
    </div>
  );
}

// ─── Analytics ───────────────────────────────────────────────────────────────

const participationData = [8.2, 14.1, 11.6, 9.4, 17.3, 12.8, 10.1, 15.9];

function Analytics({ proposals }: { proposals: Proposal[] }) {
  const maxP = Math.max(...participationData);

  const outcomeCounts = { passed: 0, failed: 0, active: 0, pending: 0 } as Record<ProposalStatus, number>;
  const catCounts: Record<string, number> = {};
  proposals.forEach(p => {
    outcomeCounts[p.status] = (outcomeCounts[p.status] || 0) + 1;
    catCounts[p.category] = (catCounts[p.category] || 0) + 1;
  });
  const total = proposals.length || 1;

  const catColors: Record<string, string> = { protocol: T.accent, treasury: T.accent2, params: T.warn, meta: T.muted, other: T.muted };

  return (
    <div>
      <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit,minmax(280px,1fr))", gap: "1.25rem", marginBottom: "1.5rem" }}>
        {/* Participation */}
        <div style={S.chartCard}>
          <h4 style={{ fontSize: ".8rem", textTransform: "uppercase", letterSpacing: ".06em", color: T.muted, marginBottom: "1rem" }}>Participation Rate (last 8)</h4>
          <div style={{ display: "flex", alignItems: "flex-end", gap: 4, height: 80 }}
               role="img" aria-label="Participation rate bar chart">
            {participationData.map((v, i) => (
              <div key={i} style={{ flex: 1, minWidth: 8, borderRadius: "3px 3px 0 0", background: `linear-gradient(to top,${T.accent},${T.accent2})`, height: `${Math.round((v / maxP) * 76)}px`, transition: "height .4s" }}
                   title={`P${i + 1}: ${v}%`} aria-label={`Proposal ${i + 1}: ${v}% participation`} />
            ))}
          </div>
          <div style={{ display: "flex", gap: 4, marginTop: ".3rem" }}>
            {participationData.map((_, i) => (
              <div key={i} style={{ flex: 1, fontSize: ".65rem", color: T.muted, textAlign: "center" }}>P{i + 1}</div>
            ))}
          </div>
        </div>

        {/* Outcomes */}
        <div style={S.chartCard}>
          <h4 style={{ fontSize: ".8rem", textTransform: "uppercase", letterSpacing: ".06em", color: T.muted, marginBottom: "1rem" }}>Outcome Breakdown</h4>
          <div style={{ display: "flex", flexDirection: "column", gap: ".6rem" }}>
            {(["passed", "failed", "active", "pending"] as ProposalStatus[]).map(s => {
              const colors: Record<ProposalStatus, string> = { passed: T.accent2, failed: T.danger, active: T.warn, pending: T.muted };
              const n = outcomeCounts[s] || 0;
              return (
                <div key={s} style={{ display: "flex", alignItems: "center", gap: ".75rem", fontSize: ".85rem" }}>
                  <span style={{ width: 70, color: T.muted, textTransform: "capitalize" }}>{s}</span>
                  <div style={{ flex: 1, height: 8, borderRadius: 4, background: "rgba(255,255,255,.06)", overflow: "hidden" }}>
                    <div style={{ height: "100%", background: colors[s], borderRadius: 4, width: `${Math.round((n / total) * 100)}%`, transition: "width .6s" }} />
                  </div>
                  <span style={{ width: 20, textAlign: "right", fontWeight: 600 }}>{n}</span>
                </div>
              );
            })}
          </div>
        </div>

        {/* Category */}
        <div style={S.chartCard}>
          <h4 style={{ fontSize: ".8rem", textTransform: "uppercase", letterSpacing: ".06em", color: T.muted, marginBottom: "1rem" }}>Category Distribution</h4>
          <div style={{ display: "flex", flexDirection: "column", gap: ".6rem" }}>
            {Object.entries(catCounts).map(([cat, n]) => (
              <div key={cat} style={{ display: "flex", alignItems: "center", gap: ".75rem", fontSize: ".85rem" }}>
                <span style={{ width: 70, color: T.muted, textTransform: "capitalize" }}>{cat}</span>
                <div style={{ flex: 1, height: 8, borderRadius: 4, background: "rgba(255,255,255,.06)", overflow: "hidden" }}>
                  <div style={{ height: "100%", background: catColors[cat] || T.muted, borderRadius: 4, width: `${Math.round((n / total) * 100)}%`, transition: "width .6s" }} />
                </div>
                <span style={{ width: 20, textAlign: "right", fontWeight: 600 }}>{n}</span>
              </div>
            ))}
          </div>
        </div>

        {/* Speed */}
        <div style={S.chartCard}>
          <h4 style={{ fontSize: ".8rem", textTransform: "uppercase", letterSpacing: ".06em", color: T.muted, marginBottom: "1rem" }}>Avg. Decision Speed</h4>
          {[["Avg. time to quorum", "2.3 days"], ["Avg. voting period", "7 days"], ["Fastest proposal", "18h"], ["Largest turnout", "17.3%"]].map(([l, v]) => (
            <div key={l} style={{ display: "flex", justifyContent: "space-between", fontSize: ".875rem", padding: ".4rem 0", borderBottom: `1px solid ${T.border}` }}>
              <span style={{ color: T.muted }}>{l}</span><strong>{v}</strong>
            </div>
          ))}
        </div>
      </div>

      {/* History table */}
      <div style={{ ...S.chartCard, overflowX: "auto" }}>
        <h4 style={{ fontSize: ".8rem", textTransform: "uppercase", letterSpacing: ".06em", color: T.muted, marginBottom: "1rem" }}>Full Proposal History</h4>
        <table style={{ width: "100%", borderCollapse: "collapse", fontSize: ".85rem" }} role="table" aria-label="Full proposal history">
          <thead>
            <tr style={{ borderBottom: `1px solid ${T.border}` }}>
              {["#", "Title", "Status", "For", "Against", "Participation"].map((h, i) => (
                <th key={h} scope="col" style={{ padding: ".5rem .75rem", color: T.muted, fontWeight: 500, textAlign: i >= 3 ? "right" : "left" }}>{h}</th>
              ))}
            </tr>
          </thead>
          <tbody>
            {[...proposals].reverse().map(p => {
              const tot = p.forVotes + p.againstVotes;
              const fp = tot ? Math.round((p.forVotes / tot) * 100) : 0;
              const par = tot ? ((tot / 184_000_000) * 100).toFixed(1) + "%" : "—";
              return (
                <tr key={p.id} style={{ borderBottom: `1px solid ${T.border}` }}>
                  <td style={{ padding: ".5rem .75rem", color: T.muted }}>{p.id}</td>
                  <td style={{ padding: ".5rem .75rem" }}>{p.title}</td>
                  <td style={{ padding: ".5rem .75rem" }}><StatusBadge status={p.status} /></td>
                  <td style={{ padding: ".5rem .75rem", textAlign: "right", color: T.accent2 }}>{fp}%</td>
                  <td style={{ padding: ".5rem .75rem", textAlign: "right", color: T.danger }}>{100 - fp}%</td>
                  <td style={{ padding: ".5rem .75rem", textAlign: "right" }}>{par}</td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
    </div>
  );
}

// ─── Proposal Creation Wizard ─────────────────────────────────────────────────

interface WizardState {
  title: string; category: string; summary: string; body: string;
  votingPeriod: number; quorum: number; threshold: number;
  actions: OnChainAction[];
}

function ProposalWizard({ onSubmit, onToast }: {
  onSubmit: (data: WizardState) => void;
  onToast: (msg: string, type: "success" | "error") => void;
}) {
  const [step, setStep] = useState(0);
  const [form, setForm] = useState<WizardState>({ title: "", category: "", summary: "", body: "", votingPeriod: 7, quorum: 10, threshold: 66, actions: [] });

  const set = (k: keyof WizardState, v: unknown) => setForm(f => ({ ...f, [k]: v }));

  const next = () => {
    if (step === 0) {
      if (!form.title.trim())   { onToast("Title is required", "error"); return; }
      if (!form.category)       { onToast("Category is required", "error"); return; }
      if (!form.summary.trim()) { onToast("Summary is required", "error"); return; }
    }
    setStep(s => s + 1);
  };

  const addAction = () => set("actions", [...form.actions, { contractId: "", functionName: "", args: "" }]);
  const removeAction = (i: number) => set("actions", form.actions.filter((_, idx) => idx !== i));

  const stepLabels = ["Details", "Parameters", "Actions", "Review"];
  const inputStyle = S.input;
  const ta = { ...inputStyle, resize: "vertical" as const, minHeight: 110 };

  return (
    <div>
      {/* Step indicator */}
      <div style={{ display: "flex", gap: 0, marginBottom: "2rem" }} role="list" aria-label="Wizard steps">
        {stepLabels.map((l, i) => (
          <div key={l} role="listitem" style={{ flex: 1, display: "flex", flexDirection: "column", alignItems: "center", gap: ".3rem", fontSize: ".75rem", color: T.muted, position: "relative" }}>
            {i < stepLabels.length - 1 && (
              <div style={{ position: "absolute", top: 18, left: "50%", width: "100%", height: 2, background: i < step ? T.accent2 : T.border }} />
            )}
            <div aria-current={i === step ? "step" : undefined}
              style={{ width: 36, height: 36, borderRadius: "50%", display: "flex", alignItems: "center", justifyContent: "center", fontWeight: 600, fontSize: ".85rem", zIndex: 1,
                background: i < step ? T.accent2 : i === step ? T.accent : T.card,
                border: `2px solid ${i < step ? T.accent2 : i === step ? T.accent : T.border}`,
                color: i < step ? "#0b0f1a" : i === step ? "#fff" : T.muted }}>
              {i < step ? "✓" : i + 1}
            </div>
            <span>{l}</span>
          </div>
        ))}
      </div>

      {/* Step 0 — Details */}
      {step === 0 && (
        <div>
          <div style={{ marginBottom: "1.25rem" }}>
            <label htmlFor="wiz-title" style={{ display: "block", fontSize: ".85rem", fontWeight: 500, marginBottom: ".4rem" }}>Title <span style={{ color: T.muted }}>*</span></label>
            <input id="wiz-title" type="text" maxLength={100} value={form.title} onChange={e => set("title", e.target.value)} style={inputStyle} placeholder="Concise proposal title" aria-required="true" />
            <div style={{ fontSize: ".75rem", color: T.muted, textAlign: "right", marginTop: ".25rem" }}>{form.title.length}/100</div>
          </div>
          <div style={{ marginBottom: "1.25rem" }}>
            <label htmlFor="wiz-cat" style={{ display: "block", fontSize: ".85rem", fontWeight: 500, marginBottom: ".4rem" }}>Category <span style={{ color: T.muted }}>*</span></label>
            <select id="wiz-cat" value={form.category} onChange={e => set("category", e.target.value)} style={{ ...inputStyle, appearance: "none" as const }} aria-required="true">
              <option value="">Select category…</option>
              {[["protocol", "Protocol Upgrade"], ["treasury", "Treasury"], ["params", "Parameter Change"], ["meta", "Meta-Governance"], ["other", "Other"]].map(([v, l]) => (
                <option key={v} value={v}>{l}</option>
              ))}
            </select>
          </div>
          <div style={{ marginBottom: "1.25rem" }}>
            <label htmlFor="wiz-summary" style={{ display: "block", fontSize: ".85rem", fontWeight: 500, marginBottom: ".4rem" }}>Summary <span style={{ color: T.muted }}>*</span></label>
            <textarea id="wiz-summary" maxLength={500} value={form.summary} onChange={e => set("summary", e.target.value)} style={ta} placeholder="Brief description visible in the proposal list…" aria-required="true" />
            <div style={{ fontSize: ".75rem", color: T.muted, textAlign: "right", marginTop: ".25rem" }}>{form.summary.length}/500</div>
          </div>
          <div style={{ marginBottom: "1.25rem" }}>
            <label htmlFor="wiz-body" style={{ display: "block", fontSize: ".85rem", fontWeight: 500, marginBottom: ".4rem" }}>Full Description <span style={{ color: T.muted, fontWeight: 400 }}>(Markdown supported)</span></label>
            <textarea id="wiz-body" value={form.body} onChange={e => set("body", e.target.value)} style={{ ...ta, minHeight: 160 }} placeholder="Motivation, specification, implementation plan…" />
          </div>
        </div>
      )}

      {/* Step 1 — Parameters */}
      {step === 1 && (
        <div>
          {[
            { id: "wiz-vp", label: "Voting Period (days)", type: "select", value: form.votingPeriod, options: [[3, "3 days"], [7, "7 days (recommended)"], [14, "14 days"]], onChange: (v: number) => set("votingPeriod", v) },
          ].map(f => (
            <div key={f.id} style={{ marginBottom: "1.25rem" }}>
              <label htmlFor={f.id} style={{ display: "block", fontSize: ".85rem", fontWeight: 500, marginBottom: ".4rem" }}>{f.label}</label>
              <select id={f.id} value={f.value} onChange={e => f.onChange(+e.target.value)} style={{ ...inputStyle, appearance: "none" as const }}>
                {f.options!.map(([v, l]) => <option key={v} value={v}>{l}</option>)}
              </select>
            </div>
          ))}
          {[
            { id: "wiz-quorum", label: "Quorum (% of supply)", key: "quorum" as const, min: 1, max: 50, help: "Minimum participation required." },
            { id: "wiz-thresh", label: "Approval Threshold (%)", key: "threshold" as const, min: 50, max: 100, help: "% of For votes needed to pass." },
          ].map(f => (
            <div key={f.id} style={{ marginBottom: "1.25rem" }}>
              <label htmlFor={f.id} style={{ display: "block", fontSize: ".85rem", fontWeight: 500, marginBottom: ".4rem" }}>{f.label}</label>
              <input id={f.id} type="number" min={f.min} max={f.max} value={form[f.key]} onChange={e => set(f.key, +e.target.value)} style={inputStyle} />
              <p style={{ fontSize: ".78rem", color: T.muted, marginTop: ".25rem" }}>{f.help}</p>
            </div>
          ))}
        </div>
      )}

      {/* Step 2 — Actions */}
      {step === 2 && (
        <div>
          <p style={{ fontSize: ".875rem", color: T.muted, marginBottom: "1rem" }}>Attach executable on-chain actions, or leave blank for a signaling-only proposal.</p>
          {form.actions.map((a, i) => (
            <div key={i} style={{ background: T.card, border: `1px solid ${T.border}`, borderRadius: 8, padding: "1rem", marginBottom: ".75rem" }}>
              <div style={{ display: "flex", justifyContent: "space-between", marginBottom: ".5rem" }}>
                <strong style={{ fontSize: ".85rem" }}>Action {i + 1}</strong>
                <button onClick={() => removeAction(i)} aria-label={`Remove action ${i + 1}`}
                  style={{ ...S.btn, background: "transparent", border: `1px solid ${T.border}`, color: T.muted, padding: ".2rem .5rem", fontSize: ".8rem" }}>✕</button>
              </div>
              {[["Contract ID", "contractId", "C..."], ["Function", "functionName", "set_fee_rate"], ["Arguments (JSON)", "args", '{"bps": 150}']].map(([l, k, ph]) => (
                <div key={k} style={{ marginBottom: ".5rem" }}>
                  <label htmlFor={`action-${i}-${k}`} style={{ display: "block", fontSize: ".82rem", marginBottom: ".25rem", color: T.muted }}>{l}</label>
                  <input id={`action-${i}-${k}`} type="text" placeholder={ph}
                    value={(a as Record<string, string>)[k]}
                    onChange={e => set("actions", form.actions.map((x, xi) => xi === i ? { ...x, [k]: e.target.value } : x))}
                    style={inputStyle} />
                </div>
              ))}
            </div>
          ))}
          <button onClick={addAction} id="add-action-btn"
            style={{ ...S.btn, background: "transparent", border: `1px solid ${T.border}`, color: T.text, fontSize: ".85rem" }}>+ Add Action</button>
        </div>
      )}

      {/* Step 3 — Review */}
      {step === 3 && (
        <div>
          <div style={{ background: T.card, border: `1px solid ${T.border}`, borderRadius: T.radius, padding: "1.25rem", marginBottom: "1.5rem" }}>
            <h3 style={{ fontSize: "1rem", fontWeight: 700, marginBottom: "1rem" }}>{form.title || "—"}</h3>
            <div style={{ display: "grid", gridTemplateColumns: "1fr 1fr", gap: ".5rem 1rem", fontSize: ".85rem", marginBottom: "1rem" }}>
              {[["Category", form.category || "—"], ["Voting period", `${form.votingPeriod} days`], ["Quorum", `${form.quorum}%`], ["Approval threshold", `${form.threshold}%`]].map(([l, v]) => (
                <React.Fragment key={l}><span style={{ color: T.muted }}>{l}</span><span>{v}</span></React.Fragment>
              ))}
            </div>
            <p style={{ fontSize: ".85rem", color: T.muted }}>{form.summary || "—"}</p>
          </div>
        </div>
      )}

      {/* Navigation */}
      <div style={{ display: "flex", justifyContent: "space-between", marginTop: "2rem" }}>
        <div>
          {step > 0 && (
            <button onClick={() => setStep(s => s - 1)} id="wiz-back"
              style={{ ...S.btn, background: "transparent", border: `1px solid ${T.border}`, color: T.text }}>← Back</button>
          )}
        </div>
        {step < 3 ? (
          <button onClick={next} id="wiz-next" style={{ ...S.btn, background: T.accent, color: "#fff" }}>Next →</button>
        ) : (
          <button id="wiz-submit" onClick={() => onSubmit(form)}
            style={{ ...S.btn, background: T.accent2, color: "#0b0f1a" }}>🚀 Submit Proposal</button>
        )}
      </div>
    </div>
  );
}

// ─── Main component ───────────────────────────────────────────────────────────

type Tab = "proposals" | "create" | "delegate" | "calculator" | "analytics";

export function GovernanceForum({
  proposals: propProposals = DEFAULT_PROPOSALS,
  delegates = DEFAULT_DELEGATES,
  totalSupply = 184_000_000,
  onProposalSubmit,
  onVote,
  onDelegate,
  onConnectWallet,
  walletConnected = false,
  walletAddress,
  walletVotingPower,
  className = "",
  "aria-label": ariaLabel,
}: GovernanceForumProps) {
  const [tab, setTab] = useState<Tab>("proposals");
  const [proposals, setProposals] = useState<Proposal[]>(propProposals);
  const [votes, setVotes] = useState<Record<string, VoteChoice>>({});
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [filterStatus, setFilterStatus] = useState("");
  const [filterCat, setFilterCat] = useState("");
  const [delegateAddr, setDelegateAddr] = useState("");
  const [toasts, setToasts] = useState<ToastItem[]>([]);
  const toastCounter = useRef(0);

  const addToast = useCallback((msg: string, type: "success" | "error" = "success") => {
    const id = ++toastCounter.current;
    setToasts(t => [...t, { id, msg, type }]);
    setTimeout(() => setToasts(t => t.filter(x => x.id !== id)), 4000);
  }, []);

  const castVote = useCallback((proposalId: string, choice: VoteChoice) => {
    if (votes[proposalId] === choice) { addToast(`Already voted ${choice}`, "error"); return; }
    setVotes(v => ({ ...v, [proposalId]: choice }));
    setProposals(ps => ps.map(p => p.id !== proposalId ? p : {
      ...p,
      forVotes: p.forVotes + (choice === "for" ? 50_000 : 0),
      againstVotes: p.againstVotes + (choice === "against" ? 50_000 : 0),
    }));
    addToast(`Vote cast: <strong>${choice}</strong> on ${proposalId}`);
    onVote?.(proposalId, choice);
  }, [votes, addToast, onVote]);

  const handleDelegate = useCallback((addr?: string) => {
    const a = addr || delegateAddr.trim();
    if (!a) { addToast("Enter a valid Stellar address", "error"); return; }
    addToast(`Delegated to <strong>${a.slice(0, 12)}…</strong>`);
    onDelegate?.(a);
  }, [delegateAddr, addToast, onDelegate]);

  const handleProposalSubmit = useCallback((data: WizardState) => {
    const newId = `GIP-0${proposals.length + 10}`;
    const newProposal: Proposal = {
      id: newId, title: data.title, category: data.category as ProposalCategory,
      status: "pending", summary: data.summary, body: data.body || data.summary,
      forVotes: 0, againstVotes: 0, quorumPct: data.quorum,
      created: new Date().toISOString().slice(0, 10),
      ends: new Date(Date.now() + data.votingPeriod * 86400000).toISOString().slice(0, 10),
      author: walletAddress || "You", actions: data.actions,
    };
    setProposals(ps => [newProposal, ...ps]);
    addToast(`Proposal <strong>${newId}</strong> submitted!`);
    onProposalSubmit?.(newProposal);
    setTab("proposals");
  }, [proposals.length, walletAddress, addToast, onProposalSubmit]);

  const filteredProposals = proposals.filter(p =>
    (!filterStatus || p.status === filterStatus) &&
    (!filterCat    || p.category === filterCat)
  );

  const selectedProposal = selectedId ? proposals.find(p => p.id === selectedId) ?? null : null;

  const tabs: { key: Tab; label: string }[] = [
    { key: "proposals",  label: "Proposals" },
    { key: "create",     label: "+ New Proposal" },
    { key: "delegate",   label: "Delegation" },
    { key: "calculator", label: "Power Calc" },
    { key: "analytics",  label: "Analytics" },
  ];

  const activeStats = proposals.filter(p => p.status === "active").length;
  const totalVotes = proposals.reduce((a, p) => a + p.forVotes + p.againstVotes, 0);

  return (
    <div style={S.root} className={className} aria-label={ariaLabel ?? "Soroban AMM Governance"}>
      {/* Header */}
      <header style={S.header} role="banner">
        <div style={{ display: "flex", alignItems: "center", gap: ".6rem", fontWeight: 700, fontSize: "1.1rem", background: `linear-gradient(135deg,${T.accent},${T.accent2})`, WebkitBackgroundClip: "text", WebkitTextFillColor: "transparent" }}
             aria-label="Soroban AMM Governance">
          <svg width="28" height="28" viewBox="0 0 28 28" fill="none" aria-hidden="true">
            <circle cx="14" cy="14" r="13" stroke="url(#hg1)" strokeWidth="2"/>
            <path d="M8 14 L14 8 L20 14 L14 20 Z" fill="url(#hg2)" opacity=".9"/>
            <defs>
              <linearGradient id="hg1" x1="0" y1="0" x2="28" y2="28"><stop stopColor={T.accent}/><stop offset="1" stopColor={T.accent2}/></linearGradient>
              <linearGradient id="hg2" x1="8" y1="8" x2="20" y2="20"><stop stopColor={T.accent}/><stop offset="1" stopColor={T.accent2}/></linearGradient>
            </defs>
          </svg>
          Soroban AMM · Governance
        </div>
        <div style={{ display: "flex", alignItems: "center", gap: ".75rem" }}>
          <div style={{ background: T.card, border: `1px solid ${T.border}`, borderRadius: 20, padding: ".3rem .8rem", fontSize: ".8rem", color: T.muted, display: "flex", alignItems: "center", gap: ".4rem" }}
               aria-label={walletConnected ? "Wallet connected" : "Read-only mode, no wallet required"}>
            <span style={{ width: 8, height: 8, borderRadius: "50%", background: walletConnected ? T.accent2 : T.muted, display: "inline-block" }} aria-hidden="true" />
            {walletConnected ? (walletAddress ?? "Connected") : "Read-only mode"}
          </div>
          <button id="connect-wallet-btn" onClick={onConnectWallet} aria-label={walletConnected ? "Disconnect wallet" : "Connect wallet"}
            style={{ ...S.btn, background: walletConnected ? T.accent2 : T.accent, color: walletConnected ? "#0b0f1a" : "#fff" }}>
            {walletConnected ? "✓ Connected" : "Connect Wallet"}
          </button>
        </div>
      </header>

      {/* Main */}
      <main style={S.main} id="main-content">
        {/* Stats */}
        <div style={S.statsRow} role="region" aria-label="Governance statistics">
          {[
            { label: "Active Proposals", val: activeStats, sub: `${proposals.filter(p => p.status === "active" && p.ends <= new Date(Date.now() + 7*86400000).toISOString().slice(0,10)).length} ending this week` },
            { label: "Total Votes Cast",  val: fmtNum(totalVotes), sub: "across all proposals" },
            { label: "Quorum Threshold",  val: "10%",  sub: "of circulating supply" },
            { label: "Your Voting Power", val: walletVotingPower ? fmtNum(walletVotingPower) : "—", sub: walletConnected ? "connected" : "Connect wallet to view" },
          ].map(s => (
            <div key={s.label} style={S.statCard}>
              <div style={S.statLabel}>{s.label}</div>
              <div style={S.statVal}>{s.val}</div>
              <div style={S.statSub}>{s.sub}</div>
            </div>
          ))}
        </div>

        {/* Tabs */}
        <nav role="navigation" aria-label="Governance sections">
          <div style={S.tabsWrap} role="tablist">
            {tabs.map(t => (
              <button key={t.key} id={`tab-${t.key}`} role="tab"
                aria-selected={tab === t.key} aria-controls={`sec-${t.key}`}
                onClick={() => setTab(t.key)}
                style={{ padding: ".45rem 1rem", borderRadius: 7, border: "none", cursor: "pointer", fontFamily: "inherit", fontSize: ".875rem", fontWeight: 500, transition: "all .2s",
                  background: tab === t.key ? T.accent : "transparent", color: tab === t.key ? "#fff" : T.muted }}>
                {t.label}
              </button>
            ))}
          </div>
        </nav>

        {/* ── Proposals tab ── */}
        {tab === "proposals" && (
          <section id="sec-proposals" role="tabpanel" aria-labelledby="tab-proposals">
            <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", flexWrap: "wrap", gap: ".75rem", marginBottom: "1.25rem" }}>
              <h1 style={{ fontSize: "1.25rem", fontWeight: 700 }}>Governance Proposals</h1>
              <div style={{ display: "flex", gap: ".5rem", flexWrap: "wrap" }}>
                {[
                  { id: "filter-status", value: filterStatus, onChange: setFilterStatus, label: "Filter by status", options: [["", "All Status"], ["active", "Active"], ["passed", "Passed"], ["failed", "Failed"], ["pending", "Pending"]] },
                  { id: "filter-cat",    value: filterCat,    onChange: setFilterCat,    label: "Filter by category", options: [["", "All Categories"], ["protocol", "Protocol"], ["treasury", "Treasury"], ["params", "Parameters"]] },
                ].map(f => (
                  <select key={f.id} id={f.id} value={f.value} onChange={e => f.onChange(e.target.value)}
                    aria-label={f.label} style={{ ...S.input, width: "auto", fontSize: ".82rem", appearance: "none" as const }}>
                    {f.options.map(([v, l]) => <option key={v} value={v}>{l}</option>)}
                  </select>
                ))}
              </div>
            </div>
            <div role="list" aria-label="Proposals list">
              {filteredProposals.length === 0 && (
                <p style={{ color: T.muted, fontSize: ".9rem", padding: "2rem 0" }}>No proposals match the current filters.</p>
              )}
              {filteredProposals.map(p => {
                const myVote = votes[p.id];
                const hasVotes = p.forVotes + p.againstVotes > 0;
                return (
                  <article key={p.id} role="listitem"
                    style={S.proposalCard}
                    onClick={() => setSelectedId(p.id)}
                    tabIndex={0}
                    onKeyDown={(e: KeyboardEvent<HTMLElement>) => { if (e.key === "Enter" || e.key === " ") setSelectedId(p.id); }}
                    aria-label={`Proposal ${p.id}: ${p.title}`}>
                    <div style={{ position: "absolute", left: 0, top: 0, bottom: 0, width: 3, background: `linear-gradient(to bottom,${T.accent},${T.accent2})`, borderRadius: "3px 0 0 3px" }} aria-hidden="true" />
                    <div style={{ display: "flex", alignItems: "center", gap: ".6rem", flexWrap: "wrap", marginBottom: ".6rem" }}>
                      <StatusBadge status={p.status} />
                      <span style={{ ...S.badge, background: "rgba(255,255,255,.06)", color: T.muted }}>{p.category}</span>
                      <span style={{ fontSize: ".75rem", color: T.muted }}>{p.id}</span>
                      {myVote && <span style={{ ...S.badge, background: "rgba(108,99,255,.2)", color: T.accent }}>You voted: {myVote}</span>}
                    </div>
                    <div style={{ fontSize: "1.05rem", fontWeight: 600, marginBottom: ".4rem" }}>{p.title}</div>
                    <div style={{ fontSize: ".875rem", color: T.muted, marginBottom: "1rem", display: "-webkit-box", WebkitLineClamp: 2, WebkitBoxOrient: "vertical" as const, overflow: "hidden" }}>{p.summary}</div>
                    {hasVotes ? <VoteBar forVotes={p.forVotes} againstVotes={p.againstVotes} /> : (
                      <div style={{ fontSize: ".8rem", color: T.muted, margin: ".4rem 0" }}>No votes yet</div>
                    )}
                    <div style={{ display: "flex", alignItems: "center", justifyContent: "space-between", flexWrap: "wrap", gap: ".5rem", fontSize: ".8rem", color: T.muted, marginTop: ".8rem" }}>
                      <span>By {p.author} · Created {p.created}</span>
                      <span style={{ color: p.status === "active" ? T.warn : T.muted }}>
                        {p.status === "active" ? `⏱ Ends ${p.ends}` : p.status === "pending" ? `📅 Opens ${p.ends}` : `✓ Closed ${p.ends}`}
                      </span>
                      {p.status === "active" && (
                        <div style={{ display: "flex", gap: ".5rem" }} onClick={e => e.stopPropagation()}>
                          {(["for", "against", "abstain"] as VoteChoice[]).map(c => (
                            <button key={c} id={`vote-list-${p.id}-${c}`}
                              onClick={() => castVote(p.id, c)}
                              aria-label={`Vote ${c} on ${p.id}`}
                              style={{ ...S.btn, padding: ".3rem .55rem", fontSize: ".75rem", background: c === "for" ? T.accent2 : c === "against" ? T.danger : "transparent", color: c === "abstain" ? T.text : (c === "for" ? "#0b0f1a" : "#fff"), border: c === "abstain" ? `1px solid ${T.border}` : "none", opacity: myVote === c ? 0.5 : 1 }}>
                              {c === "for" ? "👍 For" : c === "against" ? "👎 Against" : "◦ Abstain"}
                            </button>
                          ))}
                        </div>
                      )}
                    </div>
                  </article>
                );
              })}
            </div>
          </section>
        )}

        {/* ── Create tab ── */}
        {tab === "create" && (
          <section id="sec-create" role="tabpanel" aria-labelledby="tab-create">
            <h2 style={{ fontSize: "1.2rem", fontWeight: 700, marginBottom: "1.5rem" }}>New Proposal Wizard</h2>
            <ProposalWizard onSubmit={handleProposalSubmit} onToast={addToast} />
          </section>
        )}

        {/* ── Delegation tab ── */}
        {tab === "delegate" && (
          <section id="sec-delegate" role="tabpanel" aria-labelledby="tab-delegate">
            <h2 style={{ fontSize: "1.2rem", fontWeight: 700, marginBottom: "1.5rem" }}>Vote Delegation</h2>
            <p style={{ fontSize: ".875rem", color: T.muted, marginBottom: "1.5rem" }}>Delegate your voting power to a trusted representative. You retain token ownership and can revoke at any time.</p>
            <div style={{ background: T.card, border: `1px solid ${T.border}`, borderRadius: T.radius, padding: "1.25rem", marginBottom: "1.5rem" }}>
              <h3 style={{ fontSize: ".9rem", fontWeight: 600, marginBottom: "1rem" }}>Delegate to custom address</h3>
              <div style={{ display: "flex", gap: ".75rem", flexWrap: "wrap" }}>
                <input id="delegate-addr-input" type="text" value={delegateAddr} onChange={e => setDelegateAddr(e.target.value)}
                  style={{ ...S.input, flex: 1, minWidth: 200 }} placeholder="G… (Stellar address)" aria-label="Custom delegate address" />
                <button id="delegate-submit-btn" onClick={() => handleDelegate()} style={{ ...S.btn, background: T.accent, color: "#fff" }} aria-label="Delegate voting power">Delegate</button>
              </div>
            </div>
            <h3 style={{ fontSize: ".9rem", fontWeight: 600, marginBottom: "1rem", color: T.muted, textTransform: "uppercase", letterSpacing: ".06em" }}>Recommended Delegates</h3>
            <div role="list" aria-label="Recommended delegates">
              {delegates.map(d => (
                <div key={d.address} style={S.delegateCard} role="listitem">
                  <div style={S.avatar} aria-hidden="true">{d.name.slice(0, 2).toUpperCase()}</div>
                  <div style={{ flex: 1 }}>
                    <div style={{ fontWeight: 600, fontSize: ".95rem" }}>{d.name}</div>
                    <div style={{ fontSize: ".78rem", color: T.muted }}>{d.address} · {d.participationRate} participation · {d.bio}</div>
                  </div>
                  <span style={{ fontSize: ".85rem", fontWeight: 600, color: T.accent2, whiteSpace: "nowrap" }}>{d.votingPower}</span>
                  <button id={`delegate-to-${d.address.replace(/…/g, "")}`} onClick={() => handleDelegate(d.address)}
                    style={{ ...S.btn, background: T.accent, color: "#fff", padding: ".3rem .7rem", fontSize: ".8rem" }}
                    aria-label={`Delegate to ${d.name}`}>Delegate</button>
                </div>
              ))}
            </div>
          </section>
        )}

        {/* ── Calculator tab ── */}
        {tab === "calculator" && (
          <section id="sec-calculator" role="tabpanel" aria-labelledby="tab-calculator">
            <h2 style={{ fontSize: "1.2rem", fontWeight: 700, marginBottom: "1.5rem" }}>Voting Power Calculator</h2>
            <VotingPowerCalc totalSupply={totalSupply} />
          </section>
        )}

        {/* ── Analytics tab ── */}
        {tab === "analytics" && (
          <section id="sec-analytics" role="tabpanel" aria-labelledby="tab-analytics">
            <h2 style={{ fontSize: "1.2rem", fontWeight: 700, marginBottom: "1.5rem" }}>Proposal History & Analytics</h2>
            <Analytics proposals={proposals} />
          </section>
        )}
      </main>

      {/* Proposal detail modal */}
      <ProposalModal
        proposal={selectedProposal}
        userVote={selectedId ? votes[selectedId] : undefined}
        onClose={() => setSelectedId(null)}
        onVote={(id, choice) => { castVote(id, choice); setSelectedId(null); }}
      />

      {/* Toasts */}
      <Toasts items={toasts} onRemove={id => setToasts(t => t.filter(x => x.id !== id))} />
    </div>
  );
}
