# occa-programs

Solana Anchor programs for [OCCA](https://github.com/occa-network/occa). Just the on-chain pieces — the TypeScript runtime, server, and web app live in the sibling [`occa`](https://github.com/occa-network/occa) repo and pull the IDL through [`occa-sdk`](https://www.npmjs.com/package/occa-sdk).

## Programs

| Program  | Crate               | Devnet ID                                     | Status        |
| -------- | ------------------- | --------------------------------------------- | ------------- |
| Registry | `programs/registry` | `occakrum1PrVDaXauDuSiwPKfrhdnm7uVWKJxE39kfm` | live (devnet) |

### Registry

Three account types, ten instructions. The owner signs everything; the operator pays.

- **CompanyAccount** — tenant root. Seed: `["company", owner, nonce_le_u32]`.
- **AgentIdentity** — the stable, non-transferable agent identity. Seed: `["agent_identity", agent_pubkey]`.
- **Deployment** — binds an `AgentIdentity` to a `CompanyAccount` with a role and pinned adapter. Seed: `["deployment", company_pda, deployment_index_le_u32]`.

Every state-changing ix is signed by `owner` (the user wallet, immutable). The operator hot wallet only pays fees. It never authorizes anything.

Instructions:

- `create_company`, `update_company_metadata`, `update_company_status`
- `register_agent_identity`, `update_agent_identity_metadata`
- `create_deployment`, `update_deployment_metadata`, `update_deployment_status`, `retire_deployment`
- `set_operating_wallet`

`retire_deployment` is terminal. Once retired, that's it — no reactivation, no transfer, no recovery. Identity transfer doesn't exist as an instruction at all; that's deliberate (whitepaper §15 has the why).

## Layout

```
Anchor.toml              # workspace cluster + program ID config
Cargo.toml               # rust workspace
programs/
  registry/
    Cargo.toml
    Xargo.toml
    src/lib.rs           # all instructions + accounts + errors
    registry-keypair.json  # gitignored
target/                  # build artifacts (gitignored)
  idl/registry.json      # IDL — copied into occa-sdk via `pnpm sync-idl`
  deploy/registry.so     # compiled program
  types/registry.ts      # anchor TS bindings
```

## Prerequisites

- Rust (stable) + `cargo`
- Solana CLI ≥ 1.18
- Anchor CLI ≥ 0.30
- A funded keypair at `~/.config/solana/id.json` (for devnet deploys)

## Build

```bash
anchor build
```

This writes `target/deploy/registry.so` and `target/idl/registry.json`. After every rebuild, sync the IDL into the SDK:

```bash
cd ../occa/packages/occa-sdk && pnpm sync-idl
```

Skip this and the SDK will derive PDAs against a stale schema. Don't skip it.

## Test

```bash
cargo test -p registry --features test
# or, for the full anchor flow (needs a test validator):
anchor test
```

## Deploy

The committed program ID `occakrum1PrVDaXauDuSiwPKfrhdnm7uVWKJxE39kfm` was vanity-grinded. The matching keypair stays out of git — restore it from your backup before deploying.

```bash
solana config set --url devnet
anchor deploy --provider.cluster devnet
```

Going to a fresh program ID (e.g. mainnet)? Generate a new keypair, run `anchor keys sync`, then deploy.

## Versioning

Account schemas carry an explicit `version: u8` byte at offset 8, right after the discriminator. Any breaking layout change has to do three things in lockstep:

1. Bump the version byte.
2. Ship a migration ix (or a one-shot script) that rewrites old accounts in place.
3. Update the SDK borsh offsets to match.

Discriminators stay stable across versions — they're derived from the account name, not the layout — so your old indexer queries keep working.

## License

MIT. See [LICENSE](./LICENSE).
