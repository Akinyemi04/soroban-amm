#!/usr/bin/env bash
# oracle_aggregator.sh — end-to-end flow for the oracle aggregator's
# configuration surface: initialize -> register sources -> tune weight,
# staleness and deviation band -> pause -> remove source -> two-step admin
# handover, reading every setting back after it is written.
#
# The workspace ships no deployable `quote` adapter (the TWAP consumers are
# snapshot stores, not adapters), so sources here are plain contract
# instances with no `quote` entrypoint. That makes the pricing assertion a
# negative one: with no source able to quote, get_price must refuse to
# return a price rather than report a zero-confidence value.
#
# Self-contained: deploys its own instances, so it runs without deploy.sh:
#   bash scripts/e2e/oracle_aggregator.sh
set -Eeuo pipefail

run_oracle_aggregator_flow() {
  CURRENT_FLOW="oracle_aggregator"

  local admin="$SOURCE_PUBLIC_KEY"
  local new_admin_key="${SOURCE_ACCOUNT}-oracle-admin"
  local new_admin
  new_admin=$(e2e_fund_account oracle-admin)

  # Stand-in source addresses: deployed contracts that expose no `quote`.
  local src1 src2
  src1=$(e2e_deploy token)
  src2=$(e2e_deploy token)

  # ── initialize ──────────────────────────────────────────────────────────
  local oracle
  oracle=$(e2e_deploy oracle_aggregator)
  expect_fail "oracle_aggregator: zero max_staleness is rejected" \
    invoke "$oracle" initialize --admin "$admin" --max_staleness_seconds 0
  invoke "$oracle" initialize --admin "$admin" --max_staleness_seconds 3600 >/dev/null
  expect_fail "oracle_aggregator: second initialize is rejected" \
    invoke "$oracle" initialize --admin "$admin" --max_staleness_seconds 3600

  assert_eq "oracle_aggregator: get_admin reads back" "$(invoke "$oracle" get_admin | parse_address)" "$admin"
  assert_eq "oracle_aggregator: max staleness reads back" "$(invoke "$oracle" get_max_staleness | parse_i128)" "3600"
  assert_eq "oracle_aggregator: default deviation band is 500 bps" \
    "$(invoke "$oracle" get_max_deviation_bps | parse_i128)" "500"
  assert_eq "oracle_aggregator: starts unpaused" "$(invoke "$oracle" is_paused)" "false"
  assert_eq "oracle_aggregator: starts with no sources" \
    "$(invoke "$oracle" list_sources | count_matches '"source_contract"')" "0"

  # ── register_source ─────────────────────────────────────────────────────
  # source_type 2 = OracleSourceType::External.
  invoke "$oracle" register_source \
    --admin "$admin" --source_contract "$src1" --source_type 2 --weight 10000 >/dev/null
  invoke "$oracle" register_source \
    --admin "$admin" --source_contract "$src2" --source_type 2 --weight 10000 >/dev/null

  local sources
  sources=$(invoke "$oracle" list_sources)
  assert_eq "oracle_aggregator: two sources registered" \
    "$(printf '%s\n' "$sources" | count_matches '"source_contract"')" "2"
  assert_contains "oracle_aggregator: source 1 listed" "$sources" "$src1"
  assert_contains "oracle_aggregator: source 2 listed" "$sources" "$src2"

  expect_fail "oracle_aggregator: duplicate source is rejected" \
    invoke "$oracle" register_source \
      --admin "$admin" --source_contract "$src1" --source_type 2 --weight 10000
  expect_fail "oracle_aggregator: zero weight is rejected" \
    invoke "$oracle" register_source \
      --admin "$admin" --source_contract "$oracle" --source_type 2 --weight 0
  expect_fail "oracle_aggregator: non-admin cannot register a source" \
    invoke_as "$new_admin_key" "$oracle" register_source \
      --admin "$new_admin" --source_contract "$oracle" --source_type 2 --weight 10000

  # ── tune weight, staleness, deviation band ──────────────────────────────
  invoke "$oracle" set_source_weight --admin "$admin" --source_contract "$src2" --weight 25000 >/dev/null
  assert_eq "oracle_aggregator: updated weight reads back" \
    "$(invoke "$oracle" list_sources | count_matches '"weight":[[:space:]]*25000')" "1"

  invoke "$oracle" set_max_staleness --admin "$admin" --max_staleness_seconds 600 >/dev/null
  assert_eq "oracle_aggregator: updated staleness reads back" "$(invoke "$oracle" get_max_staleness | parse_i128)" "600"

  invoke "$oracle" set_max_deviation_bps --admin "$admin" --max_deviation_bps 250 >/dev/null
  assert_eq "oracle_aggregator: updated deviation band reads back" \
    "$(invoke "$oracle" get_max_deviation_bps | parse_i128)" "250"
  expect_fail "oracle_aggregator: zero deviation band is rejected" \
    invoke "$oracle" set_max_deviation_bps --admin "$admin" --max_deviation_bps 0

  # ── pricing: no source can quote, so no price is returned ───────────────
  expect_fail "oracle_aggregator: get_price refuses without a quoting source" \
    invoke "$oracle" get_price --token_a "$src1" --token_b "$src2"

  # ── pause ───────────────────────────────────────────────────────────────
  invoke "$oracle" pause --admin "$admin" >/dev/null
  assert_eq "oracle_aggregator: is_paused after pause" "$(invoke "$oracle" is_paused)" "true"
  expect_fail "oracle_aggregator: get_price is rejected while paused" \
    invoke "$oracle" get_price --token_a "$src1" --token_b "$src2"
  invoke "$oracle" unpause --admin "$admin" >/dev/null
  assert_eq "oracle_aggregator: is_paused after unpause" "$(invoke "$oracle" is_paused)" "false"

  # ── remove_source ───────────────────────────────────────────────────────
  invoke "$oracle" remove_source --admin "$admin" --source_contract "$src1" >/dev/null
  sources=$(invoke "$oracle" list_sources)
  assert_eq "oracle_aggregator: one source left after removal" \
    "$(printf '%s\n' "$sources" | count_matches '"source_contract"')" "1"
  assert_not_contains "oracle_aggregator: removed source is gone" "$sources" "$src1"
  expect_fail "oracle_aggregator: removing the last source is rejected" \
    invoke "$oracle" remove_source --admin "$admin" --source_contract "$src2"

  # ── two-step admin handover ─────────────────────────────────────────────
  invoke "$oracle" propose_admin --current_admin "$admin" --new_admin "$new_admin" >/dev/null
  assert_eq "oracle_aggregator: admin unchanged until accepted" "$(invoke "$oracle" get_admin | parse_address)" "$admin"
  invoke_as "$new_admin_key" "$oracle" accept_admin --new_admin "$new_admin" >/dev/null
  assert_eq "oracle_aggregator: admin rotated after accept" "$(invoke "$oracle" get_admin | parse_address)" "$new_admin"
  expect_fail "oracle_aggregator: former admin loses admin rights" \
    invoke "$oracle" set_max_staleness --admin "$admin" --max_staleness_seconds 900
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_oracle_aggregator_flow
fi
