# Event schema versioning (#302)

Every event emitted by the AMM, CL, governance, factory (and any future
contract that adopts the pattern) carries a `schema_version: u32`
field. Off-chain consumers (GraphQL indexer, health dashboard, any
WebSocket subscriber) read this field BEFORE decoding the rest of the
payload so an unexpected version can be quarantined rather than
silently misinterpreted.

## How it's emitted

Every emit site uses the `soroban_amm_sdk::emit_versioned_event!` macro
instead of `env.events().publish(...)`:

```rust
// Before:
env.events().publish(
    (Symbol::new(&env, "swap"), trader.clone()),
    (token_in, amount_in, token_out, amount_out),
);

// After (#302):
soroban_amm_sdk::emit_versioned_event!(
    env,
    (Symbol::new(&env, "swap"), trader.clone()),
    (token_in, amount_in, token_out, amount_out),
);
```

The macro expands to:

```rust
env.events().publish(
    /* topic */ (Symbol::new(&env, "swap"), trader.clone()),
    /* data  */ (soroban_amm_sdk::EVENT_SCHEMA_VERSION, (token_in, amount_in, token_out, amount_out)),
);
```

So the on-wire shape is `(version: u32, original_payload)`. **Topic is
unchanged** — consumers can keep filtering by event name + author the
way they always have.

## How consumers decode

```rust
let (version, payload): (u32, (Address, i128, Address, i128)) =
    event.data.try_into_val(env)?;

match version {
    1 => decode_v1(payload),
    2 => decode_v2(payload),  // future
    other => {
        log::warn!("unknown event schema version {other}; skipping");
        return Ok(());
    }
}
```

## When to bump `EVENT_SCHEMA_VERSION`

`pub const EVENT_SCHEMA_VERSION: u32 = 1;` lives in
`contracts/amm-sdk/src/lib.rs`.

Bump it (single integer, +1 per release) when ANY event's payload
shape changes:

- Field added
- Field removed
- Field type changed
- Field order changed (Soroban encodes tuples positionally)
- Field renamed (no runtime effect, but indexers that key on names
  will break)

Don't bump for:

- Adding a new event type (consumers can ignore unknown topics)
- Changing event topic content (topic is separate from payload — a
  topic change is independently observable)

## Versioning is global, not per-event

One `EVENT_SCHEMA_VERSION` covers every contract event in the
workspace. The alternative — per-event version — was rejected because:

1. Consumer state machines would have to track N independent version
   sequences, one per event type.
2. The contracts ship together as a single workspace release; if any
   event payload changes, the deployment cycle re-bumps every consumer
   anyway.
3. A global version is one branch in the consumer's decoder, not N.

The cost is that bumping any event's payload bumps the "version" for
every event, even unchanged ones. That's fine: consumers see the
unchanged payload as the same bytes, just with a different `version`
prefix — easy to validate during the upgrade window.

## Affected contracts

This PR migrates every existing emit site in:

- `contracts/amm/src/lib.rs` (14 sites)
- `contracts/concentrated_liquidity/src/lib.rs` (5 sites)
- `contracts/governance/src/lib.rs` (5 sites)
- `contracts/factory/src/lib.rs` (6 sites)

Total: **30 sites migrated**.

`contracts/staking/src/lib.rs` was inspected but has no event emissions
yet — once it starts emitting it should adopt the macro from day one.

Test files in each of those crates were updated to decode the
versioned payload shape; see `__ver_N` locals + `assert_eq!(version,
EVENT_SCHEMA_VERSION)` assertions added by `migrate_tests.py`.
