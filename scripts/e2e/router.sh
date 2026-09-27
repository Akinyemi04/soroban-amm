#!/usr/bin/env bash
# router.sh — end-to-end flow for multi-hop routing: a two-hop A -> B -> C
# route through two factory-created pools, i.e. three contracts (router,
# pool, pool) plus the tokens in one transaction — the interaction shape the
# mock environment models least faithfully.
#
# Covers path discovery, quotes, swap_exact_in, swap_exact_out, the slippage
# bound and the pause switch, verifying balances by reading them back.
#
# Self-contained: deploys its own tokens, factory, pools and router, so it
# runs without deploy.sh:
#   bash scripts/e2e/router.sh
set -Eeuo pipefail

run_router_flow() {
  CURRENT_FLOW="router"

  local trader="$SOURCE_PUBLIC_KEY"
  local seed="${ROUTER_POOL_SEED:-10000000}"
  local amount_in="${ROUTER_AMOUNT_IN:-100000}"
  local exact_out="${ROUTER_EXACT_OUT:-20000}"

  # ── fixtures: tokens A/B/C, factory, pools A/B and B/C ──────────────────
  local ta tb tc factory pool_ab pool_bc
  ta=$(e2e_new_token RTA)
  tb=$(e2e_new_token RTB)
  tc=$(e2e_new_token RTC)
  factory=$(e2e_new_factory)
  pool_ab=$(e2e_new_seeded_pool "$factory" "$ta" "$tb" "$seed")
  pool_bc=$(e2e_new_seeded_pool "$factory" "$tb" "$tc" "$seed")
  pass "router: fixtures ready (pools A/B=$pool_ab, B/C=$pool_bc)"

  # ── initialize ──────────────────────────────────────────────────────────
  local router
  router=$(e2e_deploy router)
  invoke "$router" initialize --admin "$trader" --factory "$factory" >/dev/null
  assert_eq "router: get_factory reads back" "$(invoke "$router" get_factory | parse_address)" "$factory"
  assert_eq "router: starts unpaused" "$(invoke "$router" is_paused)" "false"

  expect_fail "router: second initialize is rejected" \
    invoke "$router" initialize --admin "$trader" --factory "$factory"

  # ── path discovery ──────────────────────────────────────────────────────
  local path="[\"$ta\",\"$tb\",\"$tc\"]"
  assert_eq "router: A -> B -> C is routable" "$(invoke "$router" is_path_routable --path "$path")" "true"
  assert_eq "router: direct A -> C (no pool) is not routable" \
    "$(invoke "$router" is_path_routable --path "[\"$ta\",\"$tc\"]")" "false"

  local pools_out
  pools_out=$(invoke "$router" get_pools_for_path --path "$path")
  assert_contains "router: get_pools_for_path resolves hop 0 to pool A/B" "$pools_out" "$pool_ab"
  assert_contains "router: get_pools_for_path resolves hop 1 to pool B/C" "$pools_out" "$pool_bc"

  # ── quotes ──────────────────────────────────────────────────────────────
  # The router quote must equal chaining each pool's own get_amount_out.
  local hop0 hop1 quote amounts_out
  hop0=$(invoke "$pool_ab" get_amount_out --token_in "$ta" --amount_in "$amount_in" | parse_i128)
  hop1=$(invoke "$pool_bc" get_amount_out --token_in "$tb" --amount_in "$hop0" | parse_i128)
  quote=$(invoke "$router" get_amount_out_path --path "$path" --amount_in "$amount_in" | parse_i128)
  assert_gt "router: exact-in quote is positive" "$quote" 0
  assert_eq "router: exact-in quote equals chained pool quotes" "$quote" "$hop1"

  amounts_out=$(invoke "$router" get_amounts_out_path --path "$path" --amount_in "$amount_in")
  assert_eq "router: per-hop breakdown starts at amount_in" "$(printf '%s\n' "$amounts_out" | nth_num 1)" "$amount_in"
  assert_eq "router: per-hop breakdown intermediate amount" "$(printf '%s\n' "$amounts_out" | nth_num 2)" "$hop0"
  assert_eq "router: per-hop breakdown ends at the quote" "$(printf '%s\n' "$amounts_out" | nth_num 3)" "$quote"

  # ── swap_exact_in ───────────────────────────────────────────────────────
  invoke "$ta" mint --to "$trader" --amount "$(( amount_in * 4 ))" >/dev/null

  local a0 b0 c0 deadline out
  a0=$(balance_of "$ta" "$trader")
  b0=$(balance_of "$tb" "$trader")
  c0=$(balance_of "$tc" "$trader")

  deadline=$(( $(date +%s) + 300 ))
  expect_fail "router: swap_exact_in below the slippage floor is rejected" \
    invoke "$router" swap_exact_in \
      --trader "$trader" --path "$path" --amount_in "$amount_in" \
      --min_amount_out "$(( quote * 10 ))" --deadline "$deadline"

  out=$(invoke "$router" swap_exact_in \
    --trader "$trader" \
    --path "$path" \
    --amount_in "$amount_in" \
    --min_amount_out "$quote" \
    --deadline "$deadline" | parse_i128)
  assert_eq "router: swap_exact_in output matches the quote" "$out" "$quote"
  assert_eq "router: token A debited by amount_in" "$(balance_of "$ta" "$trader")" "$(( a0 - amount_in ))"
  assert_eq "router: token C credited by the output" "$(balance_of "$tc" "$trader")" "$(( c0 + out ))"
  assert_eq "router: intermediate token B nets to zero" "$(balance_of "$tb" "$trader")" "$b0"

  # ── swap_exact_out ──────────────────────────────────────────────────────
  local quote_in total_in a1 c1 c2
  quote_in=$(invoke "$router" get_amount_in_path --path "$path" --amount_out "$exact_out" | parse_i128)
  assert_gt "router: exact-out quote is positive" "$quote_in" 0

  a1=$(balance_of "$ta" "$trader")
  c1=$(balance_of "$tc" "$trader")
  deadline=$(( $(date +%s) + 300 ))
  total_in=$(invoke "$router" swap_exact_out \
    --trader "$trader" \
    --path "$path" \
    --amount_out "$exact_out" \
    --max_in "$quote_in" \
    --deadline "$deadline" | parse_i128)
  assert_eq "router: swap_exact_out input matches the quote" "$total_in" "$quote_in"
  assert_eq "router: token A debited by the exact-out input" "$(balance_of "$ta" "$trader")" "$(( a1 - total_in ))"
  c2=$(balance_of "$tc" "$trader")
  if (( c2 - c1 < exact_out )); then
    die "router: swap_exact_out delivered $(( c2 - c1 )) of token C, expected at least $exact_out"
  fi
  pass "router: swap_exact_out delivered at least amount_out ($(( c2 - c1 )) >= $exact_out)"

  # ── pause switch ────────────────────────────────────────────────────────
  invoke "$router" pause --admin "$trader" >/dev/null
  assert_eq "router: is_paused after pause" "$(invoke "$router" is_paused)" "true"
  deadline=$(( $(date +%s) + 300 ))
  expect_fail "router: swaps are rejected while paused" \
    invoke "$router" swap_exact_in \
      --trader "$trader" --path "$path" --amount_in "$amount_in" \
      --min_amount_out 0 --deadline "$deadline"
  invoke "$router" unpause --admin "$trader" >/dev/null
  assert_eq "router: is_paused after unpause" "$(invoke "$router" is_paused)" "false"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_router_flow
fi
