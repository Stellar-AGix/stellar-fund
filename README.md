# SwiftRamp Swap Contract

A Soroban (Stellar smart contract) that lets users swap between currency-pegged
tokens — NGN, KES, GHS, ZAR, USD, EUR, GBP — at admin-published exchange
rates, with slippage protection and a hook for future transaction-privacy
tooling.

This README covers what the contract actually does on-chain, how the pieces
fit together, how the test suite is organized, and how the CI/CD pipeline
enforces quality on every push.

---

## 1. What this contract does

SwiftRamp is a fixed-rate swap contract, not an AMM. There's no liquidity
curve and no price discovery on-chain — an admin publishes a rate for each
currency, and swaps are settled at whatever rate is currently published,
subject to a slippage floor the caller sets themselves.

At a high level:

1. An **admin** address is set once at deploy time (`initialize`).
2. The admin wires each currency symbol (`"USD"`, `"NGN"`, etc.) to the
   Soroban token contract address that represents it (`set_currency_token`)
   and publishes a scaled exchange rate for that currency
   (`set_rate`).
3. Any **sender** can call `swap`, specifying how much of `from_currency`
   they're sending and the minimum amount of `to_currency` they're willing
   to accept. The contract computes the payout from the two published
   rates, checks it against the sender's floor, and moves real tokens via
   the standard Soroban token interface.
4. The admin can top up the contract's own token balances via
   `fund_liquidity` so it has enough of a given currency to pay out swaps.

Everything here uses **real token transfers** and **real authorization**
(`require_auth()`) — there's no mocking or IOU layer. If the contract
doesn't hold enough of `to_currency`, the transfer simply fails.

## 2. Repo layout

```
.
├── Cargo.toml / Cargo.lock          # workspace root
├── rustfmt.toml                     # shared formatting rules
├── .github/
│   ├── dependabot.yml                # weekly cargo + github-actions PRs
│   └── workflows/
│       ├── ci.yml                    # fmt, clippy, tests, wasm build
│       └── security.yml              # cargo-audit, weekly + on push/PR
└── contracts/
    └── swiftramp-swap/
        ├── Cargo.toml
        ├── Cargo.lock
        └── src/
            └── lib.rs                # the entire contract + test suite
```

The workspace currently has one member crate, `swiftramp-swap`, which
compiles both to a native `rlib` (for running the unit tests on your machine)
and to `wasm32-unknown-unknown` (the actual deployable contract artifact).

## 3. On-chain data model

Everything the contract remembers lives in **instance storage** (small,
cheap, rent-free within the contract's own lifetime) except for spent
commitments, which live in **persistent storage** since they need to survive
independently and be checkable indefinitely.

```rust
pub enum DataKey {
    Admin,                        // the one address allowed to call admin-only fns
    Rate(Symbol),                 // e.g. Rate("NGN") -> scaled i128 rate
    LiquidityToken(Symbol),       // e.g. LiquidityToken("USD") -> token contract Address
    Commitment(BytesN<32>),       // spent-commitment marker, see §6
}
```

### Rate scaling

All rates and amounts are scaled by `RATE_SCALE = 10_000_000` (Stellar's
standard 7-decimal fixed-point convention). A rate of `1580 * RATE_SCALE`
for NGN against a USD rate of `1 * RATE_SCALE` means 1 USD ≈ 1,580 NGN.
`RATE_SCALE` is exported as `pub` from the crate so off-chain callers
(the frontend, scripts, other contracts) can share the same convention
without hardcoding the magic number.

### Errors

```rust
pub enum SwapError {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    NotAdmin = 3,
    UnknownCurrency = 4,
    SlippageExceeded = 5,
    ZeroAmount = 6,
    CommitmentAlreadyUsed = 7,
}
```

Every public function returns `Result<T, SwapError>` — nothing panics on bad
input; callers always get a typed error back.

## 4. Public API

| Function | Caller | What it does |
|---|---|---|
| `initialize(admin)` | anyone, once | Sets the admin address. Fails with `AlreadyInitialized` if called twice. |
| `set_currency_token(admin, currency, token_address)` | admin only | Registers which token contract backs a currency symbol. |
| `set_rate(admin, currency, rate_scaled)` | admin only | Publishes/updates the exchange rate for a currency. Rejects `<= 0`. |
| `get_rate(currency)` | anyone | Read-only lookup. Returns `UnknownCurrency` if never set. |
| `quote(from, to, amount)` | anyone | Pure calculation: `amount * rate_to / rate_from`. No state change. |
| `swap(sender, recipient, from, to, amount, min_receive, commitment)` | sender (auth required) | The main entry point — see §5. |
| `fund_liquidity(admin, currency, amount)` | admin only | Moves tokens from the admin's wallet into the contract's own balance. |

Admin-gated functions all funnel through a shared `require_admin` helper,
which checks the caller against the stored admin **before** calling
`require_auth()` — so an impostor gets a clean `NotAdmin` error rather than
an authorization failure, regardless of whether they could produce a valid
signature for themselves.

## 5. How a swap actually executes

```rust
pub fn swap(env, sender, recipient, from_currency, to_currency, amount, min_receive, commitment)
```

Step by step:

1. `sender.require_auth()` — the transaction must actually be signed by
   `sender`.
2. `amount` must be `> 0`, or the call fails with `ZeroAmount`.
3. `quote()` is called internally to compute `receive_amount` from the two
   published rates.
4. If `receive_amount < min_receive`, the call fails with
   `SlippageExceeded` — this is the sender's own protection against rates
   moving between when they built the transaction and when it lands.
5. **Replay check** (see §6 below) — unless `commitment` is the all-zero
   placeholder, it must not have been used before.
6. Both currencies must have a registered token address, or the call fails
   with `UnknownCurrency`.
7. Real token transfers happen: `from_token.transfer(sender → contract)`,
   then `to_token.transfer(contract → recipient)`.
8. A `swap` event is published with the sender, recipient, payout amount,
   and commitment, so off-chain indexers can track activity.

Note that `sender` and `recipient` are independent parameters — the person
authorizing the swap doesn't have to be the person receiving the funds,
which is the normal shape for a remittance/off-ramp use case.

## 6. The `commitment` parameter — what it is and isn't (yet)

`commitment` is a `BytesN<32>` the caller supplies. Today it does exactly one
thing: if it's non-zero, the contract checks it hasn't been used before and
then marks it as spent. That's it — a **replay guard**, nothing more.

It is **not** currently a real privacy mechanism. The contract does not hide
swap amounts, senders, or recipients — Soroban/Stellar token transfer events
are public by design, same as any contract using the standard token
interface. The field exists so a real shielded-pool or zk-SNARK verifier
could be slotted in later (the caller would supply a commitment derived from
a Pedersen commitment to the real amount, and a future version of the
contract would verify a proof against it) **without changing the public
function signature**. Building that verifier — circuit design, a trusted
setup or transparent proving system, on-chain verification — is a separate,
substantially larger project than what's implemented here.

The all-zero commitment (`BytesN::from_array(&env, &[0u8; 32])`) is treated
as "no commitment supplied" and is explicitly exempt from the replay check,
so callers who don't care about this feature can just pass zeros on every
call.

## 7. Test suite

All 13 tests live in `#[cfg(test)] mod test` inside `lib.rs`, built around a
shared `Fixture` struct that deploys the contract, initializes it with a
generated admin, wires up USD and NGN test tokens via
`register_stellar_asset_contract_v2`, and publishes a rate (1 USD ≈ 1,580
NGN) before each test runs.

| Category | Tests |
|---|---|
| Happy path | `swap_pays_out_at_published_rate`, `quote_matches_manual_rate_math`, `fund_liquidity_moves_tokens_from_admin_to_contract` |
| Amount validation | `swap_rejects_slippage_below_floor`, `swap_rejects_zero_amount`, `swap_rejects_negative_amount` |
| Currency lookups | `get_rate_errors_for_unknown_currency`, `swap_rejects_unpublished_currency`, `set_rate_rejects_zero_or_negative` |
| Admin / auth boundaries | `initialize_twice_errors`, `admin_only_calls_reject_non_admin_caller` |
| Replay protection | `swap_rejects_replayed_commitment`, `zero_commitment_can_be_reused_across_swaps` |

Run them with:

```bash
cargo test --workspace
```

## 8. CI/CD pipeline

Three automated checks run on every push and pull request against `main`:

**`.github/workflows/ci.yml`** — four jobs, the last gated on the first
three passing:
1. `cargo fmt --all -- --check` — formatting must match `rustfmt.toml`.
2. `cargo clippy --all-targets --all-features -- -D warnings` — zero
   tolerance for lints; warnings fail the build.
3. `cargo test --workspace` — the full unit test suite.
4. `cargo build --target wasm32-unknown-unknown --release -p swiftramp-swap`
   — builds the actual deployable contract artifact and uploads it as a
   downloadable GitHub Actions artifact.

**`.github/workflows/security.yml`** — runs `cargo audit` against
`Cargo.lock` on every push/PR and additionally on a weekly cron
(`0 6 * * 1`), so a newly disclosed CVE in a dependency gets caught even in
weeks with no code changes.

**`.github/dependabot.yml`** — opens PRs weekly for outdated `cargo`
dependencies and outdated GitHub Actions versions, capped at 10 open PRs at
a time.

**`rustfmt.toml`** pins `edition = "2021"`, `max_width = 100`, and default
heuristics, so local `cargo fmt` and the CI check never disagree.

## 9. Local development

```bash
# one-time setup
rustup target add wasm32-unknown-unknown
cargo install --locked cargo-audit   # only needed to run the security check locally

# everyday loop
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace

# produce the deployable artifact
cargo build --target wasm32-unknown-unknown --release -p swiftramp-swap
# -> target/wasm32-unknown-unknown/release/swiftramp_swap.wasm
```

Deploying that `.wasm` to testnet or mainnet is done with the Stellar CLI
(`stellar contract deploy --wasm ... --source <account> --network <network>`),
which is outside the scope of this contract's own code but standard for any
Soroban contract.

## 10. Known limitations / roadmap

- **No on-chain privacy yet.** All amounts, senders, and recipients are
  visible on-chain. The `commitment` field is a placeholder for future
  shielded-pool support, not a working privacy feature today.
- **No liquidity incentives.** `fund_liquidity` is admin-only and manual —
  there's no automated market-maker or yield mechanism; the admin is
  responsible for keeping each currency's balance funded.
- **Rates are trusted, not oracled.** `set_rate` is a plain admin-set value
  with no on-chain price feed or staleness check. If the admin key is
  compromised or the admin simply forgets to update a rate, swaps will
  execute at a stale price (bounded only by the caller's own
  `min_receive`).