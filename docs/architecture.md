back

# The Polyleverage Protocol Architecture

rust
solana
defi
leverage
prediction markets
pyth
oracles
attestation
testing
litesvm

Polylayer is a trading protocol and platform for crypto, real-world
assets, and prediction markets. Polyleverage is the part of it that
offers leverage. It is a native Solana program that lets a trader
take a leveraged position on a market's price without the protocol
ever holding more risk than the collateral the trader posted.

That last clause is the whole design. Most leverage venues are
structured so that the protocol, or a shared liquidity pool, is the
counterparty of last resort. When a position moves far enough and
fast enough, the protocol absorbs the gap between the liquidation
price and the price actually achieved, and a cascade of liquidations
during a sharp move can drain a shared pool faster than keepers can
close positions. Offering high leverage on that structure means
underwriting a tail risk that is difficult to bound.

Polyleverage removes the shared pool. Every position is a contract
between exactly two traders, each of whom has posted equal
collateral, and the contract is fully collateralized for its entire
life. This post describes how that works, how positions are matched
and settled, how the design was extended from prediction markets to
equities and commodities, and how the program is tested.

## Bounded loss by construction

A polyleverage position is called a PMLC, a pairwise matched leverage
contract. It has exactly two owners, a long and a short. At the
moment of matching, each side locks the same amount of collateral,
call it `c`. Nothing else is at stake. The long's maximum loss is
`c`; the short's maximum loss is `c`; and whatever one side loses,
the other side gains.

The settlement math enforces this directly. Each side's equity is
computed and then clamped to the range `[0, 2c]`:

```
equity_long  = clamp(c + pnl_long,  0, 2c)
equity_short = 2c - equity_long
```

The clamp is the safety property. However extreme the price move,
however high the leverage, a side's equity can never fall below zero
and never rise above `2c`, and the two equities always sum to exactly
`2c`. No value is created or destroyed at settlement, and there is no
amount the protocol can be asked to cover. Because the loss is bounded
by construction rather than by a liquidation engine winning a race,
leverage as high as 1000x is safe to offer. At 1000x a price move of
a tenth of a percent against a position consumes the entire `c`, but
that is the trader's `c` and nobody else's.

Leverage, then, is not a property of a margin account. It is a
property baked into the contract at match time, and the contract is a
closed system from that point forward.

## The objects

Four kinds of account carry the protocol's state.

A **ProgramConfig** is the singleton root. It records the admin
authority, the public key the program accepts attestations from, and
a global pause flag.

An **InstrumentConfig** describes one tradeable market at one leverage
and one collateral size. It is keyed by a tuple: a data source byte,
a 32-byte market identifier, the leverage, the collateral bucket, and
a TWAP window. The program treats the market identifier as an opaque
blob; what it identifies is the off-chain attestor's concern, not the
program's.

A **MarginAccount** is one trader's collateral ledger for one
collateral mint. It separates three balances: `free` collateral the
trader can withdraw or commit, `reserved` collateral committed to
resting intents, and `locked` collateral committed to live positions.
Every state transition moves value between these three buckets, and
the program's arithmetic helpers are the only way to do so.

A **PMLC** is a live position: two owners, an entry price, a
collateral amount per side, a leverage, a derived position size, and
a status.

```
ProgramConfig (singleton)
      │
      ├── InstrumentConfig ──── IntentBook (order book)
      │         │
      │         └── PMLC, PMLC, PMLC, ...   (matched positions)
      │
      └── MarginAccount per (trader, mint)
```

## Intents and the order book

A trader does not take a position directly. A trader posts an
**intent**: a statement of the side they want, a price range they are
willing to enter at, a number of contracts, and an expiration slot.
Posting an intent moves `contracts × collateral_bucket` from the
trader's `free` balance to `reserved`. The collateral is committed but
not yet at risk.

Intents rest in an **IntentBook**, one expandable account per
instrument. The book is a single contiguous account holding a header
and a pool of fixed-size 96-byte nodes. A node is one of three
things: an intent, a trader's seat, or a free-list slot. Intents are
held in two red-black trees, one per side, augmented into interval
trees so that the matcher can ask "which resting shorts have a range
overlapping this long" in logarithmic time rather than by scanning.

```
IntentBook account
┌────────────────────────────────────────────────┐
│ header: tree roots, free-list head, counters    │
├────────────────────────────────────────────────┤
│ node 1 │ node 2 │ node 3 │ ... │ node N         │
│  each node = intent | seat | free slot (96 B)   │
└────────────────────────────────────────────────┘
   long tree ─┐         ┌─ short tree
              ▼         ▼
        (red-black + interval-augmented)
```

Keeping the book in one account, rather than one account per order,
keeps matching within a single transaction's account budget and makes
the book's rent a fixed, expandable cost rather than a per-order one.

## Matching

Two intents on opposite sides match when their price ranges overlap.
The matcher computes the overlap of the long's range and the short's
range, and the position's **entry price** is the midpoint of that
overlap. A long willing to enter at `[0.58, 0.61]` and a short
willing to enter at `[0.60, 0.63]` overlap on `[0.60, 0.61]` and
match at `0.605`.

```
long  intent     [0.58 ──────── 0.61]
short intent          [0.60 ──────── 0.63]
                       └ overlap ┘
                        [0.60,0.61]
                     entry = 0.605
```

Matching can happen two ways. A keeper can name an explicit pair, or
anyone can call the permissionless matcher, which scans the book for
the first valid pair. Both paths run the same core logic, so the
economics are identical regardless of who triggered the match. At
match time each side's `reserved` collateral becomes `locked`, a PMLC
account is created, and its fields are filled in: the two owners, the
entry price, the collateral per side, the leverage, and a position
size derived as `notional × 1e18 / entry` where `notional` is
`collateral × leverage`.

A fee applies to the taker, the intent that arrived later. The fee is
volume-tiered: each trader's rolling 30-day volume is tracked in a
small per-trader account, and a fee schedule maps volume to a fee in
basis points. Posters reserve a fee buffer when they post; the buffer
is consumed if they end up the taker and released back to `free` if
they end up the maker.

## Settlement

A PMLC ends in one of three ways, and all three move locked
collateral, so all three are gated by an attestation.

**Liquidation** closes a position whose price has moved far enough
that one side's equity has reached the instrument's liquidation
threshold. The price used is not the price at the moment the
liquidation transaction lands. It is a historical mark: the price at
the moment the position first became liquidatable, computed off-chain
by the attestor against the market's price history. Settling at the
historical mark removes a path dependency. If settlement used the
current price, a position that breached the threshold and then
recovered before a keeper got around to it would un-liquidate, and a
slow keeper would change the outcome. Binding settlement to the
historical breach mark makes the result independent of submission
timing.

**Resolution** closes a position when the underlying market resolves
to an outcome. For a prediction market this is the binary settlement:
the market resolved yes, no, or fifty-fifty.

**Mutual close** closes a position by agreement.

In every case the program does not trust the caller for the price. It
trusts an **attestation**: a fixed 104-byte message, signed by the
key registered in ProgramConfig, carrying a magic tag, a type, the
market identifier, a timestamp, a nonce, and a type-specific payload.
A liquidation attestation additionally binds itself to one specific
PMLC by public key, so the same signed price cannot be replayed
against a different position.

The signature is checked in an unusual but deliberate way. Solana
exposes an Ed25519 signature-verification primitive as a precompiled
program. The caller places an Ed25519-verify instruction immediately
before the settlement instruction in the same transaction. The
settlement instruction then reads the preceding instruction back out
of the instructions sysvar, confirms it targeted the Ed25519
precompile, and confirms the public key it verified against is the
registered attestor and the message is the attestation being acted
on.

```
settlement transaction
┌──────────────────────────────────────────────┐
│  ix[0]  Ed25519 precompile                    │
│         verifies sig over the attestation     │
├──────────────────────────────────────────────┤
│  ix[1]  Liquidate / Resolve                   │
│         reads ix[0] from the instructions     │
│         sysvar, confirms signer == attestor   │
│         and message == this attestation       │
└──────────────────────────────────────────────┘
```

Freshness and replay are handled by a per-market nonce account. Each
market keeps the last accepted attestation nonce; an attestation must
carry a strictly greater nonce, and a timestamp within a staleness
bound. An old signed attestation cannot be replayed, and an
attestation for one market cannot be used on another.

The attestor's signing key is itself rotatable, behind a timelock.
Changing it is a two-step governance action: a proposal, then an
execution that is rejected until a fixed delay has elapsed, with a
cancellation path in between. The delay gives the system's users a
window to observe a pending change to the most security-sensitive
parameter in the program.

## Exiting a position

A PMLC has no fixed expiry. Once matched it stays live until it
settles, and the settlement paths above (liquidation, resolution,
mutual close) are one way it ends. But a trader rarely wants to wait
for the underlying market to resolve. Three mechanisms let an owner
leave a live position early.

The simplest is **mutual close**: both owners of a PMLC agree to tear
it down, and each takes back collateral adjusted by the position's
current mark.

**Novation** transfers one side of a PMLC to a named new owner. The
exiting owner's collateral unlocks, the new owner's collateral locks
in, and the new owner inherits the position at its original entry
price. Novation needs a specific counterparty who has agreed to take
the position over.

The most important of the three, because it does not need a named
counterparty, is **substitution**. It is how a trader exits into the
open market, and it reuses machinery the trader already understands.
To leave a long position, the owner does exactly what they would do
to open a short from scratch: they post a short intent. The intent
rests in the same order book as every other intent. When the matcher
finds a new trader whose long intent overlaps it, the substitution
instruction fires. The crucial point is that it does not open a
second PMLC. It swaps the exiting owner out of their existing PMLC
and swaps the new trader in.

```
before:  PMLC = (long: Alice, short: Bob)

         Alice posts a short intent ─┐
                                     ├─ matched in the order book
         Carol posts a long intent ─┘
                                     ▼
after:   PMLC = (long: Carol, short: Bob)

         Alice's collateral c unlocks back to free
         Carol's collateral c locks in
         Bob, the untouched side, is unaffected
```

Substitution can settle the exiting owner's profit on-chain. The
position has an entry price `e1`; the substituting match has its own
midpoint `e2`. In the settling variant, the new trader pays the
exiting owner a premium equal to the owner's mark-to-market profit
between `e1` and `e2`. The exiting owner walks away with their
collateral plus realized profit, or minus realized loss; the new
trader holds a position whose on-chain entry is still `e1` but whose
economic entry, after the premium, is `e2`. The counterparty on the
other side of the PMLC is not touched: only owners change, and the
bilateral collateral sum is preserved throughout.

Because an exit is just an ordinary intent in the same book as an
entry, exiting and entering are the same action viewed from two
sides, and a position can be rolled rather than simply unwound. An
intent can additionally carry a reentry flag, which is recorded on
the PMLC it matches into; when that PMLC later closes, the flagged
side can re-post an intent at the saved range without a fresh signed
action, so a closing position can roll directly into the next one.

## From prediction markets to equities and commodities

Polyleverage began on prediction markets, where a price is a
probability and lives in the open interval `(0, 1)`. Extending it to
equities, commodities, and crypto majors meant settling positions on
assets quoted in dollars: a share at two hundred dollars, gold at
several thousand, bitcoin in the tens of thousands.

The program's price math turned out to be ready for this in one
respect and not in another. It was ready because the math is
ratio-based. A position's profit is `notional × (mark - entry) /
entry`, which is `notional` times a return. Multiplying every price
in an instrument by a constant leaves the return unchanged. So a
dollar price can be normalized into the program's `(0, 1)`
fixed-point by dividing by a per-instrument reference ceiling, and
the normalization is economically exact: it scales out of every
number that matters.

It was not ready in another respect. The program validated that every
price was strictly inside `(0, 1)`, and it gated instrument creation
on two hardcoded allowlists, one of accepted data sources and one of
accepted leverage values. A dollar price normalizes cleanly into
`(0, 1)`, so the price bound was satisfied by the normalization
convention with no code change. The allowlists were not. Admitting a
Pyth-sourced instrument, and admitting the higher leverage buckets,
required extending both lists. That was the on-chain change the
multi-asset work needed, and it was a small one.

Prices for the non-prediction-market assets come from Pyth. The
attestor sources a price from Pyth, normalizes it against the
instrument's reference ceiling, and signs the same attestation format
the program already understood. The on-chain verifier did not change.
The launch asset set is three crypto majors, two metals, and eleven
tokenized equities, the xStocks issued by Backed Finance, each carried
by Pyth as a continuously-priced feed.

## Testing a program that moves money

A program that custodies collateral and moves it on settlement has to
be tested to a higher standard than an ordinary application. A wrong
state transition is not a bug report; it is a loss of funds. The
question is how to exercise the real program, not a model of it,
against the full range of inputs an adversary would try.

The approach taken is to run the real compiled program in an
in-process virtual machine. The program is built to SBF bytecode with
the same toolchain that produces a deployable artifact. That bytecode
is loaded into litesvm, a Solana virtual machine that runs inside the
test process with no validator daemon and no network. Every test
sends real transactions: real instruction encoding, real program
derived addresses, real cross-program invocations into the SPL token
program and the Ed25519 precompile, and real compute-unit metering.

The choice of an in-process VM over a local validator is deliberate.
A validator advances its clock in real time, which makes the
governance timelock, a delay measured in hours, impractical to
exercise in a test. litesvm allows the clock to be set directly, so a
timelock is warped forward instantly and deterministically. A full
end-to-end test completes in milliseconds.

What the harness simulates, by design, is the off-chain operators.
The attestor, in production a key held inside a trusted execution
environment, is represented by a local Ed25519 keypair. This is a
genuine simulation rather than a mock: the keypair produces real
Ed25519 signatures over the exact attestation byte layout, and the
on-chain verifier checks them for real. A forged attestation fails
because the signature genuinely does not verify against the
registered key, not because a mock returned false. Oracle prices come
from a small Pyth feeder. The program, the VM, and every transaction
are real; only the actors around the program stand in for their
production counterparts.

## The harness

The test harness, `polyleverage-simulator`, is a small Rust crate
with four parts.

A driver loads the program, derives addresses, funds accounts,
frames instructions, and submits transactions. It exposes one typed
helper per instruction, so a test reads as a sequence of intent
("post a long, match it, liquidate it") rather than as account-list
plumbing.

An attestor module is the simulated signer. It frames and signs the
attestation types and builds the Ed25519 precompile instruction,
using the program crate's own layout constants so the harness cannot
silently drift from the on-chain wire format.

A scenario builder assembles the substrate every settlement test
needs: a configured program, a fee schedule, a collateral mint and
vault, an instrument, and two funded traders. It can drive a long and
a short into a matched position, so each test writes only the part
that is actually under test.

A pricing module performs the off-chain price normalization, the same
integer formula the Pyth feeder applies, so the multi-asset tests can
open positions at a normalized real price.

## What the suite covers

The suite is thirty-six test functions in four layers.

The **end-to-end** layer walks the protocol's lifecycle: deposit and
withdrawal, instrument creation, intent posting and matching,
liquidation, resolution, position close, novation of a position side
to a new owner, substitution, the governance timelock, and the
emergency pause. Each flow is checked on its success path and on at
least one rejection path.

The **adversarial** layer encodes the threat model directly.
Settlement is the trust boundary, so it receives the most attention.
A separate test confirms the rejection of each forgery vector: an
attestation signed by an unregistered key, an attestation of the
wrong type, an attestation bound to a different position, a replayed
nonce, an attestation for the wrong market, and a settlement
transaction carrying no attestation at all. The permissionless intent
surface is covered the same way: zero-contract intents, invalid side
bytes, expired intents, inverted price ranges, posting without
collateral, and matching two same-side intents. Each adversarial test
differs from a known-good call in exactly one way, so a rejection
isolates the vector under test rather than passing for an unrelated
reason.

The **performance** layer meters every instruction for compute units
and asserts a hard ceiling, so a compute regression fails the suite.

The **multi-asset** layer drives the program at the leverage and
margin bucket extremes and runs a full lifecycle on a normalized real
Pyth price.

## What the testing found

A test suite earns its keep by finding things, and this one found two
that thinner coverage would have missed.

The first was a reachability bug in settlement. Liquidation,
resolution, and mutual close all require a per-market nonce account,
which they load and expect to be initialized. The function that
initialized that account existed but had no callers anywhere in the
program. No instruction created it, and settlement did no lazy
initialization. Because a program derived address can only be created
by its own program, no external client could create it either. The
consequence was that all three settlement paths were unreachable on a
fresh deployment. The bug surfaced the first time a liquidation test
ran against the real program: the transaction simply failed. The fix
was to have instrument creation also create the nonce account,
idempotently, since the account is shared across every instrument on
a market.

The second was a design error caught by an executing test. The
multi-asset expansion was first analyzed as needing no on-chain change
at all. The first multi-asset test disproved that immediately:
instrument creation rejected a Pyth-sourced instrument because of the
two hardcoded allowlists described earlier. The analysis was wrong,
the test said so before any of it shipped, and the design and the
code were corrected together.

Neither finding is dramatic. That is the point. Both are the kind of
gap that a specification reads past and that only an execution of the
real program against a real transaction surfaces.

## Performance

Every instruction sits well within Solana's per-instruction compute
budget. The heaviest is matching, at roughly thirty-eight thousand
compute units against a two-hundred-thousand limit, because a match
does the most work in one transaction: it resolves both intents, runs
the matching math, lazily creates the fee and volume accounts on
first use, and allocates the position account. Everything else is
under seventeen thousand. The protocol has substantial headroom.

## Scope

The test suite verifies the functional correctness of the program's
instructions, the rejection of the modeled attacks, and the
compute-unit budget. It does not replace an external security audit,
which remains a prerequisite for a mainnet deployment, and it does
not exercise the program on a live cluster under real validator
scheduling. The program should be treated as pre-production until
those steps are complete.

What the design does provide, and what the testing confirms, is the
property the protocol is built around: a polyleverage position is a
closed system between two traders, its losses are bounded to posted
collateral by construction, and no price move, at any leverage, asks
the protocol to cover a gap.
