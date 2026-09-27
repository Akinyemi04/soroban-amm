#!/usr/bin/env bash
# twap_consumer.sh — end-to-end flow for the TWAP consumer against a real
# AMM pool: initialize -> save_snapshot (keeper-gated) -> swap -> read a
# TWAP over the snapshot window, verifying the snapshot index, tracked-pool
# list and retention policy read back.
#
# save_snapshot / get_twap_price are cross-contract calls into the pool's
# get_price_cumulative, so this exercises a real consumer <-> pool
# interaction rather than a mock.
#
# Self-contained: deploys its own tokens, factory, pool and consumer, so it
# runs without deploy.sh:
#   bash scripts/e2e/twap_consumer.sh
set -Eeuo pipefail

run_twap_consumer_flow() {
  CURRENT_FLOW="twap_consumer"

  local keeper="$SOURCE_PUBLIC_KEY"
  local outsider_key="${SOURCE_ACCOUNT}-twap-outsider"
  e2e_fund_account twap-outsider >/dev/null
  local window="${TWAP_WINDOW_SECS:-5}"

  # ── fixtures ────────────────────────────────────────────────────────────
  local ta tb factory pool
  ta=$(e2e_new_token TWA)
  tb=$(e2e_new_token TWB)
  factory=$(e2e_new_factory)
  pool=$(e2e_new_seeded_pool "$factory" "$ta" "$tb" 10000000)
  pass "twap_consumer: seeded pool $pool"

  # ── initialize ──────────────────────────────────────────────────────────
  local twap
  twap=$(e2e_deploy twap_consumer)
  invoke "$twap" initialize --keeper "$keeper" >/dev/null
  assert_eq "twap_consumer: keeper reads back" "$(invoke "$twap" get_keeper | parse_address)" "$keeper"
  expect_fail "twap_consumer: second initialize is rejected" \
    invoke "$twap" initialize --keeper "$keeper"
  assert_eq "twap_consumer: default retention max_age_seconds" \
    "$(invoke "$twap" get_retention_policy | json_num max_age_seconds)" "604800"

  # ── save_snapshot (keeper-only) ─────────────────────────────────────────
  assert_eq "twap_consumer: no snapshots before the first save" \
    "$(invoke "$twap" get_snapshot_count --pool "$pool" | parse_i128)" "0"
  expect_fail "twap_consumer: save_snapshot without keeper auth is rejected" \
    invoke_as "$outsider_key" "$twap" save_snapshot --pool "$pool"

  invoke "$twap" save_snapshot --pool "$pool" >/dev/null
  assert_eq "twap_consumer: snapshot count after save" \
    "$(invoke "$twap" get_snapshot_count --pool "$pool" | parse_i128)" "1"
  assert_eq "twap_consumer: one snapshot timestamp indexed" \
    "$(invoke "$twap" list_snapshot_timestamps --pool "$pool" --offset 0 --limit 10 | count_matches '[0-9]+')" "1"
  assert_contains "twap_consumer: pool is tracked after its first snapshot" \
    "$(invoke "$twap" get_tracked_pools)" "$pool"

  # ── swap so the pool's cumulative price advances past the snapshot ──────
  # Wait out the window first so the snapshot is at or before now - window
  # when the TWAP is read.
  sleep $(( window + 6 ))
  invoke "$ta" mint --to "$keeper" --amount 100000 >/dev/null
  local deadline
  deadline=$(( $(date +%s) + 300 ))
  invoke "$pool" swap \
    --trader "$keeper" \
    --token_in "$ta" \
    --amount_in 100000 \
    --min_out 0 \
    --deadline "$deadline" >/dev/null
  pass "twap_consumer: swapped to advance the pool's price accumulator"

  # ── TWAP read ───────────────────────────────────────────────────────────
  local twap_price
  twap_price=$(invoke "$twap" get_twap_price --pool "$pool" --window_seconds "$window" | parse_i128)
  assert_gt "twap_consumer: TWAP over the snapshot window is positive" "$twap_price" 0
  expect_fail "twap_consumer: zero-length window is rejected" \
    invoke "$twap" get_twap_price --pool "$pool" --window_seconds 0
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_twap_consumer_flow
fi
