# polyleverage

Pairwise-matched leverage contracts on prediction markets, on Solana.
See `docs/POLYLEVERAGE_SOLANA_SPEC.md` in the
[Polylayer monorepo][monorepo] for the full protocol specification.

## Build & test

```bash
# Native unit + property tests
cargo test --features no-entrypoint

# Solana SBF deployable artifact
cargo build-sbf
# → target/deploy/polyleverage.so
```

## Program ID

`6Fvi3dGdQkBP8HHZFt3e42RJUKtzmfM4wHkCXVjiYyqv`

The keypair lives in the parent monorepo under `solana/keys/` (gitignored).

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
| `src/attestation.rs` | Ed25519Program-introspected attestations |
| `src/math/` | Fixed-point helpers + leverage math |
| `src/pod.rs` | POD account helpers |
| `tests/` | proptest property tests over state transitions |

## License

Apache-2.0. See [LICENSE](./LICENSE).

[monorepo]: https://github.com/numinousmuses/polylayer
