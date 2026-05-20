# polyleverage

Pairwise-matched, fully collateralized leverage contracts on Solana.

Two traders post opposing intents over a price range; the program
matches them into a position (a PMLC) in which each side locks equal
collateral. The position settles against a price signed by an
off-chain attestor. Because matching is pairwise and fully
collateralized, a trader's maximum loss is the collateral they
posted: there is no liquidation cascade and no protocol risk, which
is what makes leverage as high as 1000x safe to offer.

polyleverage is a native Solana program (not Anchor), written against
`solana-program`, with `bytemuck` POD account layouts. It is part of
the Polylayer protocol and supports prediction markets (Polymarket)
as well as equities, commodities, and crypto majors priced off Pyth.

## Architecture

The protocol design, the settlement model, and the testing approach
are described in **[The Polyleverage Protocol
Architecture](docs/the-polyleverage-protocol-architecture.md)**.

## Build and test

```sh
# native unit + property tests
cargo test --features no-entrypoint

# Solana SBF deployable artifact
cargo build-sbf
# -> target/deploy/polyleverage.so
```

The end-to-end, adversarial, and benchmark suite lives in a separate
harness, [`polyleverage-simulator`][sim], which loads the compiled
program into an in-process VM and drives it with a simulated attestor.

## Program ID

`6Fvi3dGdQkBP8HHZFt3e42RJUKtzmfM4wHkCXVjiYyqv`

The program keypair is not contained in this repository.

## Repo layout

| Path | Purpose |
|---|---|
| `src/lib.rs` | Crate root, `declare_id!` |
| `src/entrypoint.rs` | SBF entrypoint (skipped under `no-entrypoint`) |
| `src/processor/` | Per-instruction handlers |
| `src/instruction.rs` | Instruction tags + Borsh args |
| `src/state/` | On-chain account layouts |
| `src/seeds.rs` | PDA seed constants |
| `src/error.rs` | `PolyleverageError` numeric variants (public ABI) |
| `src/attestation.rs` | Ed25519-precompile-introspected attestations |
| `src/math/` | Fixed-point price + leverage math |
| `src/pod.rs` | POD account helpers |
| `tests/` | proptest property tests over state transitions |

## Status

This program has not yet completed an external security audit and is
not deployed to mainnet. It should be treated as pre-production.

## License

Apache-2.0. See [LICENSE](./LICENSE).

[sim]: https://github.com/polylayer/polyleverage-simulator
