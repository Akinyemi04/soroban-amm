#!/usr/bin/env bash
# pol_vesting.sh — end-to-end flow for protocol-owned-liquidity vesting:
# initialize -> fund -> create_vesting -> release -> revoke_vesting,
# asserting the schedule, the released amount and the final split between
# beneficiary and treasury all read back consistently and conserve `total`.
#
# The schedule vests linearly from ledger 0 to u32::MAX, so on any live
# network a strictly partial amount has vested by the time `release` runs.
# That keeps the flow independent of reading the current ledger sequence.
#
# Self-contained: deploys its own LP token and vesting instance, so it runs
# without deploy.sh:
#   bash scripts/e2e/pol_vesting.sh
set -Eeuo pipefail

run_pol_vesting_flow() {
  CURRENT_FLOW="pol_vesting"

  local governance="$SOURCE_PUBLIC_KEY"
  local beneficiary="$SOURCE_PUBLIC_KEY"
  local outsider_key="${SOURCE_ACCOUNT}-vest-outsider"
  local treasury outsider
  treasury=$(e2e_fund_account vest-treasury)
  outsider=$(e2e_fund_account vest-outsider)

  local total="${VEST_TOTAL:-10000000000000}"
  local end_ledger=4294967295

  # ── initialize ──────────────────────────────────────────────────────────
  local lp vest
  lp=$(e2e_new_token VLP)
  vest=$(e2e_deploy pol_vesting)
  invoke "$vest" initialize --governance "$governance" --treasury "$treasury" >/dev/null
  assert_eq "pol_vesting: governance reads back" "$(invoke "$vest" get_governance | parse_address)" "$governance"
  assert_eq "pol_vesting: treasury reads back" "$(invoke "$vest" get_treasury | parse_address)" "$treasury"
  expect_fail "pol_vesting: second initialize is rejected" \
    invoke "$vest" initialize --governance "$outsider" --treasury "$outsider"

  # ── fund the vesting contract (create_vesting expects tokens in place) ──
  invoke "$lp" mint --to "$governance" --amount "$total" >/dev/null
  invoke "$lp" transfer --from "$governance" --to "$vest" --amount "$total" >/dev/null
  assert_eq "pol_vesting: contract holds the vesting total" "$(balance_of "$lp" "$vest")" "$total"

  # ── create_vesting ──────────────────────────────────────────────────────
  expect_fail "pol_vesting: schedule with end <= cliff is rejected" \
    invoke "$vest" create_vesting \
      --governance "$governance" --beneficiary "$beneficiary" --lp_token "$lp" --pool "$lp" \
      --total "$total" --start_ledger 0 --cliff_ledger 100 --end_ledger 100
  expect_fail "pol_vesting: non-governance caller cannot create a schedule" \
    invoke_as "$outsider_key" "$vest" create_vesting \
      --governance "$outsider" --beneficiary "$outsider" --lp_token "$lp" --pool "$lp" \
      --total "$total" --start_ledger 0 --cliff_ledger 0 --end_ledger "$end_ledger"

  # `pool` is recorded metadata only; the LP token address stands in for it.
  local schedule_id
  schedule_id=$(invoke "$vest" create_vesting \
    --governance "$governance" \
    --beneficiary "$beneficiary" \
    --lp_token "$lp" \
    --pool "$lp" \
    --total "$total" \
    --start_ledger 0 \
    --cliff_ledger 0 \
    --end_ledger "$end_ledger" | parse_i128)
  assert_eq "pol_vesting: first schedule id" "$schedule_id" "0"

  local schedule
  schedule=$(invoke "$vest" get_vesting --beneficiary "$beneficiary" --schedule_id "$schedule_id")
  assert_eq "pol_vesting: schedule total reads back" "$(printf '%s\n' "$schedule" | json_num total)" "$total"
  assert_eq "pol_vesting: schedule starts unreleased" "$(printf '%s\n' "$schedule" | json_num released)" "0"
  assert_eq "pol_vesting: schedule end_ledger reads back" "$(printf '%s\n' "$schedule" | json_num end_ledger)" "$end_ledger"

  # ── release: a strictly partial amount has vested ───────────────────────
  local bal0 released
  bal0=$(balance_of "$lp" "$beneficiary")
  released=$(invoke "$vest" release --beneficiary "$beneficiary" --schedule_id "$schedule_id" | parse_i128)
  assert_gt "pol_vesting: release pays out a positive amount" "$released" 0
  if (( released >= total )); then
    die "pol_vesting: release paid out $released, expected strictly less than total $total"
  fi
  pass "pol_vesting: release is partial ($released < $total)"
  assert_eq "pol_vesting: beneficiary credited by release" "$(balance_of "$lp" "$beneficiary")" "$(( bal0 + released ))"
  assert_eq "pol_vesting: schedule records the release" \
    "$(invoke "$vest" get_vesting --beneficiary "$beneficiary" --schedule_id "$schedule_id" | json_num released)" "$released"

  # ── revoke_vesting: remaining tokens split, total conserved ─────────────
  local bal1 treasury0 to_beneficiary to_treasury
  bal1=$(balance_of "$lp" "$beneficiary")
  treasury0=$(balance_of "$lp" "$treasury")
  invoke "$vest" revoke_vesting \
    --governance "$governance" --beneficiary "$beneficiary" --schedule_id "$schedule_id" >/dev/null
  to_beneficiary=$(( $(balance_of "$lp" "$beneficiary") - bal1 ))
  to_treasury=$(( $(balance_of "$lp" "$treasury") - treasury0 ))
  assert_gt "pol_vesting: treasury receives the unvested remainder" "$to_treasury" 0
  assert_eq "pol_vesting: released + vested + unvested equals total" \
    "$(( released + to_beneficiary + to_treasury ))" "$total"
  assert_eq "pol_vesting: contract is emptied by revoke" "$(balance_of "$lp" "$vest")" "0"
  expect_fail "pol_vesting: revoked schedule no longer exists" \
    invoke "$vest" get_vesting --beneficiary "$beneficiary" --schedule_id "$schedule_id"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_pol_vesting_flow
fi
