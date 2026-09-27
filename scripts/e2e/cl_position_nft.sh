#!/usr/bin/env bash
# cl_position_nft.sh — end-to-end flow for concentrated-liquidity position
# NFTs: initialize -> pool-gated mint -> metadata/ownership reads -> approve
# -> approved transfer -> pool-gated burn, reading ownership, balances and
# the id counter back after each step.
#
# mint/burn require the auth of the registered `cl_pool`. To exercise the
# NFT contract on its own, the flow registers a dedicated funded account as
# the pool and signs mint/burn with it — the same require_auth check a CL
# pool contract satisfies when it calls in.
#
# Self-contained: deploys its own instance, so it runs without deploy.sh:
#   bash scripts/e2e/cl_position_nft.sh
set -Eeuo pipefail

run_cl_position_nft_flow() {
  CURRENT_FLOW="cl_position_nft"

  local admin="$SOURCE_PUBLIC_KEY"
  local owner="$SOURCE_PUBLIC_KEY"
  local pool_key="${SOURCE_ACCOUNT}-nft-pool"
  local buyer_key="${SOURCE_ACCOUNT}-nft-buyer"
  local pool buyer
  pool=$(e2e_fund_account nft-pool)
  buyer=$(e2e_fund_account nft-buyer)

  local lower="${NFT_LOWER_TICK:--600}"
  local upper="${NFT_UPPER_TICK:-600}"

  # ── initialize ──────────────────────────────────────────────────────────
  local nft
  nft=$(e2e_deploy cl_position_nft)
  invoke "$nft" initialize --admin "$admin" --cl_pool "$pool" >/dev/null
  assert_eq "cl_position_nft: admin reads back" "$(invoke "$nft" admin | parse_address)" "$admin"
  assert_eq "cl_position_nft: cl_pool reads back" "$(invoke "$nft" cl_pool | parse_address)" "$pool"
  assert_eq "cl_position_nft: next_token_id starts at 0" "$(invoke "$nft" next_token_id | parse_i128)" "0"
  expect_fail "cl_position_nft: second initialize is rejected" \
    invoke "$nft" initialize --admin "$buyer" --cl_pool "$buyer"

  # ── mint (pool-only) ────────────────────────────────────────────────────
  expect_fail "cl_position_nft: mint without the pool's auth is rejected" \
    invoke "$nft" mint --to "$owner" --pool "$pool" --lower_tick="$lower" --upper_tick="$upper"

  local id0 id1
  id0=$(invoke_as "$pool_key" "$nft" mint \
    --to "$owner" --pool "$pool" --lower_tick="$lower" --upper_tick="$upper" | parse_i128)
  id1=$(invoke_as "$pool_key" "$nft" mint \
    --to "$owner" --pool "$pool" --lower_tick="$(( lower * 2 ))" --upper_tick="$(( upper * 2 ))" | parse_i128)
  assert_eq "cl_position_nft: first token id" "$id0" "0"
  assert_eq "cl_position_nft: ids are sequential" "$id1" "1"
  assert_eq "cl_position_nft: next_token_id after two mints" "$(invoke "$nft" next_token_id | parse_i128)" "2"
  assert_eq "cl_position_nft: total_supply after two mints" "$(invoke "$nft" total_supply | parse_i128)" "2"

  # ── ownership + metadata reads ──────────────────────────────────────────
  assert_eq "cl_position_nft: owner_of token 0" "$(invoke "$nft" owner_of --token_id "$id0" | parse_address)" "$owner"
  local meta
  meta=$(invoke "$nft" position_meta --token_id "$id0")
  assert_eq "cl_position_nft: position_meta lower_tick" "$(printf '%s\n' "$meta" | json_num lower_tick)" "$lower"
  assert_eq "cl_position_nft: position_meta upper_tick" "$(printf '%s\n' "$meta" | json_num upper_tick)" "$upper"
  assert_contains "cl_position_nft: position_meta records the pool" "$meta" "$pool"
  assert_eq "cl_position_nft: balance_of owner" "$(invoke "$nft" balance_of --owner "$owner" | parse_i128)" "2"
  local owned
  owned=$(invoke "$nft" tokens_of --owner "$owner")
  assert_eq "cl_position_nft: tokens_of lists two ids" "$(printf '%s\n' "$owned" | count_matches '[0-9]+')" "2"

  # ── approve + approved transfer ─────────────────────────────────────────
  invoke "$nft" approve --caller "$owner" --approved "$buyer" --token_id "$id0" >/dev/null
  assert_eq "cl_position_nft: get_approved reads back" \
    "$(invoke "$nft" get_approved --token_id "$id0" | parse_address)" "$buyer"

  invoke_as "$buyer_key" "$nft" transfer \
    --caller "$buyer" --from "$owner" --to "$buyer" --token_id "$id0" >/dev/null
  assert_eq "cl_position_nft: approved transfer moves ownership" \
    "$(invoke "$nft" owner_of --token_id "$id0" | parse_address)" "$buyer"
  assert_not_contains "cl_position_nft: approval cleared on transfer" \
    "$(invoke "$nft" get_approved --token_id "$id0")" "$buyer"
  assert_eq "cl_position_nft: balance_of seller after transfer" "$(invoke "$nft" balance_of --owner "$owner" | parse_i128)" "1"
  assert_eq "cl_position_nft: balance_of buyer after transfer" "$(invoke "$nft" balance_of --owner "$buyer" | parse_i128)" "1"

  expect_fail "cl_position_nft: transfer by a non-owner, non-approved caller is rejected" \
    invoke "$nft" transfer --caller "$owner" --from "$buyer" --to "$owner" --token_id "$id0"

  # ── burn (pool-only) ────────────────────────────────────────────────────
  expect_fail "cl_position_nft: burn without the pool's auth is rejected" \
    invoke "$nft" burn --token_id "$id1"
  invoke_as "$pool_key" "$nft" burn --token_id "$id1" >/dev/null
  expect_fail "cl_position_nft: owner_of a burned token fails" \
    invoke "$nft" owner_of --token_id "$id1"
  assert_eq "cl_position_nft: balance_of owner after burn" "$(invoke "$nft" balance_of --owner "$owner" | parse_i128)" "0"
  assert_eq "cl_position_nft: total_supply is cumulative (unchanged by burn)" \
    "$(invoke "$nft" total_supply | parse_i128)" "2"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_cl_position_nft_flow
fi
