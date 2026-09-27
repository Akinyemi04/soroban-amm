#!/usr/bin/env bash
# token.sh — end-to-end flow for the token contract every pool depends on
# (LP tokens are instances of it): initialize -> mint -> transfer -> approve
# -> transfer_from -> burn, asserting balances, allowance and total supply
# are read back correctly after each step.
#
# Self-contained: deploys its own token instance and two extra signers, so
# it runs without deploy.sh:
#   bash scripts/e2e/token.sh
set -Eeuo pipefail

run_token_flow() {
  CURRENT_FLOW="token"

  local admin="$SOURCE_PUBLIC_KEY"
  local spender_key="${SOURCE_ACCOUNT}-token-spender"
  local spender recipient
  spender=$(e2e_fund_account token-spender)
  recipient=$(e2e_fund_account token-recipient)

  # ── initialize + metadata ───────────────────────────────────────────────
  local token
  token=$(e2e_deploy token)
  invoke "$token" initialize \
    --admin "$admin" \
    --name "E2E Token" \
    --symbol "E2ETK" \
    --decimals 7 >/dev/null
  pass "token: deployed and initialized $token"

  assert_contains "token: name reads back" "$(invoke "$token" name)" "E2E Token"
  assert_contains "token: symbol reads back" "$(invoke "$token" symbol)" "E2ETK"
  assert_eq "token: decimals" "$(invoke "$token" decimals | parse_i128)" "7"
  assert_eq "token: admin is the initializer" "$(invoke "$token" admin | parse_address)" "$admin"
  assert_eq "token: total_supply starts at zero" "$(invoke "$token" total_supply | parse_i128)" "0"

  expect_fail "token: second initialize is rejected" \
    invoke "$token" initialize --admin "$spender" --name "Hijack" --symbol "HJK" --decimals 7

  # ── mint (admin-only) ───────────────────────────────────────────────────
  local minted=1000000
  invoke "$token" mint --to "$admin" --amount "$minted" >/dev/null
  assert_eq "token: admin balance after mint" "$(balance_of "$token" "$admin")" "$minted"
  assert_eq "token: total_supply after mint" "$(invoke "$token" total_supply | parse_i128)" "$minted"

  expect_fail "token: mint by a non-admin is rejected" \
    invoke_as "$spender_key" "$token" mint --to "$spender" --amount 1

  # ── transfer ────────────────────────────────────────────────────────────
  local sent=250000
  invoke "$token" transfer --from "$admin" --to "$recipient" --amount "$sent" >/dev/null
  assert_eq "token: sender balance after transfer" "$(balance_of "$token" "$admin")" "$(( minted - sent ))"
  assert_eq "token: recipient balance after transfer" "$(balance_of "$token" "$recipient")" "$sent"

  expect_fail "token: transfer beyond balance is rejected" \
    invoke "$token" transfer --from "$admin" --to "$recipient" --amount "$(( minted * 10 ))"

  # ── approve + allowance ─────────────────────────────────────────────────
  # live_until_ledger only has to be >= the current ledger; a far-future
  # value keeps the flow independent of reading the ledger sequence.
  local approved=100000
  invoke "$token" approve \
    --from "$admin" \
    --spender "$spender" \
    --amount "$approved" \
    --live_until_ledger 4000000000 >/dev/null
  assert_eq "token: allowance reads back approved amount" \
    "$(invoke "$token" allowance --from "$admin" --spender "$spender" | json_num amount)" "$approved"

  # ── transfer_from (spender signs) ───────────────────────────────────────
  local pulled=60000
  invoke_as "$spender_key" "$token" transfer_from \
    --spender "$spender" \
    --from "$admin" \
    --to "$recipient" \
    --amount "$pulled" >/dev/null
  assert_eq "token: allowance decremented by transfer_from" \
    "$(invoke "$token" allowance --from "$admin" --spender "$spender" | json_num amount)" "$(( approved - pulled ))"
  assert_eq "token: owner balance after transfer_from" \
    "$(balance_of "$token" "$admin")" "$(( minted - sent - pulled ))"
  assert_eq "token: recipient balance after transfer_from" \
    "$(balance_of "$token" "$recipient")" "$(( sent + pulled ))"

  expect_fail "token: transfer_from beyond the remaining allowance is rejected" \
    invoke_as "$spender_key" "$token" transfer_from \
      --spender "$spender" --from "$admin" --to "$spender" --amount "$approved"

  # ── burn (admin-authorized) ─────────────────────────────────────────────
  local burned=50000
  invoke "$token" burn --from "$recipient" --amount "$burned" >/dev/null
  assert_eq "token: recipient balance after burn" \
    "$(balance_of "$token" "$recipient")" "$(( sent + pulled - burned ))"
  assert_eq "token: total_supply reduced by burn" \
    "$(invoke "$token" total_supply | parse_i128)" "$(( minted - burned ))"

  # ── conservation: supply equals the sum of every holder's balance ───────
  local sum
  sum=$(( $(balance_of "$token" "$admin") + $(balance_of "$token" "$recipient") + $(balance_of "$token" "$spender") ))
  assert_eq "token: total_supply equals sum of balances" "$sum" "$(invoke "$token" total_supply | parse_i128)"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_token_flow
fi
