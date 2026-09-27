#!/usr/bin/env bash
# dex_aggregator.sh — end-to-end flow for best-execution routing across
# venues. Builds a market where the direct A/C pool is shallow and the
# two-hop A -> B -> C route through deep pools is better, then checks that
# the aggregator:
#   - quotes the direct pool when limited to one hop,
#   - prefers the better two-hop route when allowed two hops,
#   - quotes exactly what chaining the pools' own quotes produces, and
#   - executes that route with swap_best, moving the quoted amounts.
# Also covers the pause switch (quotes stay live, execution halts).
#
# Self-contained: deploys its own tokens, factory, pools and aggregator, so
# it runs without deploy.sh:
#   bash scripts/e2e/dex_aggregator.sh
set -Eeuo pipefail

run_dex_aggregator_flow() {
  CURRENT_FLOW="dex_aggregator"

  local trader="$SOURCE_PUBLIC_KEY"
  local deep="${AGG_DEEP_SEED:-10000000}"
  local shallow="${AGG_SHALLOW_SEED:-200000}"
  local amount_in="${AGG_AMOUNT_IN:-100000}"

  # ── fixtures ────────────────────────────────────────────────────────────
  local ta tb tc factory pool_ab pool_bc pool_ac
  ta=$(e2e_new_token AGA)
  tb=$(e2e_new_token AGB)
  tc=$(e2e_new_token AGC)
  factory=$(e2e_new_factory)
  pool_ab=$(e2e_new_seeded_pool "$factory" "$ta" "$tb" "$deep")
  pool_bc=$(e2e_new_seeded_pool "$factory" "$tb" "$tc" "$deep")
  pool_ac=$(e2e_new_seeded_pool "$factory" "$ta" "$tc" "$shallow")
  pass "dex_aggregator: fixtures ready (deep A/B, deep B/C, shallow A/C)"

  # ── initialize ──────────────────────────────────────────────────────────
  local agg
  agg=$(e2e_deploy dex_aggregator)
  invoke "$agg" initialize --admin "$trader" --factory "$factory" >/dev/null
  assert_eq "dex_aggregator: starts unpaused" "$(invoke "$agg" is_paused)" "false"
  expect_fail "dex_aggregator: second initialize is rejected" \
    invoke "$agg" initialize --admin "$trader" --factory "$factory"

  # B is the intermediate the BFS may route through.
  invoke "$agg" set_routing_tokens --tokens "[\"$tb\"]" >/dev/null
  pass "dex_aggregator: registered B as a routing token"

  # ── one-hop quote: only the direct pool is eligible ─────────────────────
  local direct_quote direct_out direct_expected
  direct_quote=$(invoke "$agg" get_quote \
    --token_in "$ta" --token_out "$tc" --amount_in "$amount_in" --max_hops 1)
  direct_out=$(printf '%s\n' "$direct_quote" | json_num amount_out)
  direct_expected=$(invoke "$pool_ac" get_amount_out --token_in "$ta" --amount_in "$amount_in" | parse_i128)
  assert_eq "dex_aggregator: one-hop quote equals the direct pool's quote" "$direct_out" "$direct_expected"
  assert_eq "dex_aggregator: one-hop route has one hop" \
    "$(printf '%s\n' "$direct_quote" | count_matches '"pool_kind"')" "1"
  assert_contains "dex_aggregator: one-hop route uses pool A/C" "$direct_quote" "$pool_ac"

  # ── two-hop quote: the deeper route through B must win ──────────────────
  local best_quote best_out hop0 hop1
  best_quote=$(invoke "$agg" get_quote \
    --token_in "$ta" --token_out "$tc" --amount_in "$amount_in" --max_hops 2)
  best_out=$(printf '%s\n' "$best_quote" | json_num amount_out)
  hop0=$(invoke "$pool_ab" get_amount_out --token_in "$ta" --amount_in "$amount_in" | parse_i128)
  hop1=$(invoke "$pool_bc" get_amount_out --token_in "$tb" --amount_in "$hop0" | parse_i128)
  assert_gt "dex_aggregator: two-hop route beats the shallow direct pool" "$best_out" "$direct_out"
  assert_eq "dex_aggregator: best quote equals chained pool quotes" "$best_out" "$hop1"
  assert_eq "dex_aggregator: best route has two hops" \
    "$(printf '%s\n' "$best_quote" | count_matches '"pool_kind"')" "2"
  assert_contains "dex_aggregator: best route enters through pool A/B" "$best_quote" "$pool_ab"
  assert_contains "dex_aggregator: best route exits through pool B/C" "$best_quote" "$pool_bc"
  assert_not_contains "dex_aggregator: best route skips pool A/C" "$best_quote" "$pool_ac"

  # ── swap_best executes the quoted route ─────────────────────────────────
  invoke "$ta" mint --to "$trader" --amount "$(( amount_in * 2 ))" >/dev/null

  local a0 b0 c0 deadline out
  a0=$(balance_of "$ta" "$trader")
  b0=$(balance_of "$tb" "$trader")
  c0=$(balance_of "$tc" "$trader")
  deadline=$(( $(date +%s) + 300 ))

  expect_fail "dex_aggregator: swap_best below min_out is rejected" \
    invoke "$agg" swap_best \
      --trader "$trader" --token_in "$ta" --token_out "$tc" \
      --amount_in "$amount_in" --min_out "$(( best_out * 10 ))" --deadline "$deadline"

  out=$(invoke "$agg" swap_best \
    --trader "$trader" \
    --token_in "$ta" \
    --token_out "$tc" \
    --amount_in "$amount_in" \
    --min_out "$best_out" \
    --deadline "$deadline" | parse_i128)
  assert_eq "dex_aggregator: swap_best output matches the quote" "$out" "$best_out"
  assert_eq "dex_aggregator: token A debited by amount_in" "$(balance_of "$ta" "$trader")" "$(( a0 - amount_in ))"
  assert_eq "dex_aggregator: token C credited by the output" "$(balance_of "$tc" "$trader")" "$(( c0 + out ))"
  assert_eq "dex_aggregator: intermediate token B nets to zero" "$(balance_of "$tb" "$trader")" "$b0"

  # ── pause: quotes stay callable, execution halts ────────────────────────
  invoke "$agg" pause --admin "$trader" >/dev/null
  assert_eq "dex_aggregator: is_paused after pause" "$(invoke "$agg" is_paused)" "true"
  local paused_quote
  paused_quote=$(invoke "$agg" get_quote \
    --token_in "$ta" --token_out "$tc" --amount_in "$amount_in" --max_hops 2 | json_num amount_out)
  assert_gt "dex_aggregator: get_quote still answers while paused" "$paused_quote" 0
  deadline=$(( $(date +%s) + 300 ))
  expect_fail "dex_aggregator: swap_best is rejected while paused" \
    invoke "$agg" swap_best \
      --trader "$trader" --token_in "$ta" --token_out "$tc" \
      --amount_in "$amount_in" --min_out 0 --deadline "$deadline"
  invoke "$agg" unpause --admin "$trader" >/dev/null
  assert_eq "dex_aggregator: is_paused after unpause" "$(invoke "$agg" is_paused)" "false"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_dex_aggregator_flow
fi
