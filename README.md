# occa-programs

Solana Anchor programs for [OCCA](https://github.com/occa-network/occa) — the on-chain settlement layer for AI-agent-run companies. This repo holds **only** the on-chain pieces. The TypeScript runtime, REST server, worker, and web OS live in the sibling [`occa`](https://github.com/occa-network/occa) repo and reach the chain through the published [`occa-sdk`](https://www.npmjs.com/package/occa-sdk), which is generated from the IDL produced here.

The chain is the source of truth for identity, ownership, treasury, and provenance. Postgres in the `occa` repo is a hot cache that can be rebuilt from these accounts at any time.

## Programs

| Program      | Crate               | Devnet ID                                       | Status        |
| ------------ | ------------------- | ----------------------------------------------- | ------------- |
| **Registry** | `programs/registry` | `occaTHMv5eYG5aZ85jimxTvHkBfsDCvndXC6J2k8kxr`   | live (devnet) |
| **Treasury** | `programs/treasury` | `occaxyVLnurdjedWCBPrvDCCto8wGYadtTZ3nAmcVzh`   | live (devnet) |

The Registry owns *who exists* (companies, agents, deployments) and *what was produced* (daily anchors, per-deliverable traces). The Treasury owns *the money* (custody, policy, delegated signers, disbursement). They are deployed separately and reference each other by PDA, not by Cargo dependency.

## Authority model

Two roles run everything, and they are deliberately unequal:

- **Owner** — the user's own wallet. Signs every state-changing instruction that touches their company or agents. Immutable: an account's `owner` can never be reassigned. There is no transfer, sell, or rotate-authority instruction by design (Whitepaper §15). Losing the owner wallet means the company/agent is permanently inaccessible — back up the seed.
- **Operator** — OCCA's hot wallet. Pays rent and fees so the user never needs SOL to onboard. It **authorizes nothing** on its own.

For background actions that must run without the owner online (anchoring provenance, batched payouts), the Treasury registers **OperationsAccounts** — capability-bounded delegated signers. Each is scoped to one company, one kind, an instruction-discriminator allow-list, a rate limit, and an expiry. This is how the server signs on the company's behalf without ever holding owner authority.

## Registry program

Five account types. Seeds in brackets.

| Account                  | Seeds                                                  | Purpose                                                                                  |
| ------------------------ | ----------------------------------------------------- | ---------------------------------------------------------------------------------------- |
| **CompanyAccount**       | `["company", owner, nonce_le_u32]`                    | Tenant root. One per company.                                                            |
| **AgentIdentity**        | `["agent_identity", agent_pubkey]`                    | Stable, non-transferable agent identity. Survives across redeployments.                  |
| **Deployment**           | `["deployment", company, deployment_index_le_u32]`   | Binds an `AgentIdentity` to a `CompanyAccount` with a role + pinned adapter.             |
| **DailyAnchorAccount**   | `["daily_anchor", deployment, day_unix_le_i64]`      | One Merkle root per deployment per UTC day — tamper-evident summary of that day's tasks. |
| **TraceAnchorAccount**   | `["trace", task_id]`                                  | One per completed, verified deliverable. The provenance record (see below).              |

Instructions:

- **Identity & company** — `create_company`, `update_company_metadata`, `update_company_status`, `register_agent_identity`, `update_agent_identity_metadata`, `set_agent_receiving_address`
- **Deployment** — `create_deployment`, `update_deployment_metadata`, `update_deployment_status`, `set_receiving_address`, `retire_deployment`
- **Provenance** — `commit_daily_anchor`, `commit_trace`

`retire_deployment` is terminal: no reactivation, no transfer, no recovery. Identity transfer is not an instruction at all — that's deliberate.

### Provenance & reputation

This is what makes agent work auditable and attributable on-chain. The deliverable itself stays off-chain (an article, a PR, a report); the chain locks its *identity and verified quality* so neither the agent nor the operator can fake it after the fact.

`commit_trace` writes one `TraceAnchorAccount` per verified deliverable, carrying:

- `task_id` — hash of the task params; also the PDA seed, so at most one anchor per task (re-commits fail naturally).
- `agent` — the **AgentIdentity** PDA (reputation aggregates here, stable across redeployments), plus the `deployment` for company context.
- `result_uri` — off-chain link to the deliverable.
- `content_hash` — SHA-256 of the deliverable; re-hash the artifact to prove it wasn't altered.
- `quality_score` (0–100) + `rubric_version` — produced by OCCA's deterministic verification gate, **not** self-asserted by the agent.
- `evidence_hash` — SHA-256 of the verification report (claims checked + sources), so anyone can audit *why* the score was assigned.
- `verdict` — always "passed"; only verified work is anchored.

There is no on-chain reputation account. **Reputation is a derived view**: fold over a company's (or agent's) `TraceAnchorAccount`s off-chain — count, average score, recency. The chain holds the tamper-evident inputs; the SDK/server computes the reputation.

`commit_daily_anchor` is the coarser companion: one Merkle root over a deployment's task hashes per UTC day, for cheap day-level integrity proofs.

Both provenance instructions are signed by the **Anchor Wallet** registered as the company's `OperationsAccount[Anchor]` in the Treasury program — resolved via a cross-program PDA lookup (`seeds::program = treasury::ID`). The instruction's discriminator must be on that account's allow-list. PDA-per-task dedup + rent cost cap any griefing.

## Treasury program

Four account types. The treasury holds funds and enforces *who can move how much, to where*.

| Account                | Seeds                                            | Purpose                                                              |
| ---------------------- | ------------------------------------------------ | ------------------------------------------------------------------- |
| **TreasuryAccount**    | `["treasury", company]`                          | Funds custody + accepted-asset allow-list.                          |
| **PolicyAccount**      | `["policy", company]`                            | Per-period budgets, thresholds, fee bps, secondary-signer rule.     |
| **OperationsAccount**  | `["operations", company, kind_byte]`             | One delegated signer per kind (see below).                          |
| **ProtocolFeeAccount** | `["protocol_fees"]`                              | Singleton. Protocol fee collection.                                 |

`OperationsKind` has two members:

- **Disbursement** — operator-held only (OCCA never holds the key). Signs batched routine payouts.
- **Anchor** — shared session key (operator + OCCA both hold). Signs the provenance instructions in the Registry.

Instructions:

- **Setup** — `init_treasury` (atomically creates Treasury + Policy), `set_policy`, `init_protocol_fee_account`
- **Operations lifecycle** — `register_company_operations`, `update_operations_capability`, `revoke_operations`, `close_operations`
- **Disbursement** — `disburse_routine`, `disburse_discretionary`, `disburse_privileged`

### Three disbursement classes

Money leaves the treasury through exactly three authorization paths, in increasing privilege:

1. **Routine** — recurring batched payouts to agent receiving addresses. Signed by the registered Disbursement Wallet, bounded by the per-month routine budget in policy and the operation's rate limit + allow-list.
2. **Discretionary** — ad-hoc payouts to an agent's receiving address, signed by the owner. Bounded by the discretionary budget.
3. **Privileged** — unrestricted destination (agent address *or* any external pubkey), signed by the owner. Above the policy threshold it additionally requires the registered **secondary signer** (two-of-two).

Disbursements to an agent receiving address charge the **Agent Operating Fee** (default `300` bps = 3%, capped at `10_000` bps), routed to the `ProtocolFeeAccount`. Payouts to external pubkeys via the privileged path charge no fee. Budgets roll over lazily by calendar month (`current_period_anchor` + civil-date arithmetic).

## How it ties into the off-chain stack

1. Owner onboards in the `occa` web OS → server submits `create_company` / `register_agent_identity` / `create_deployment` (owner signs, operator pays).
2. An agent completes a task → the deterministic verification gate scores it → the server anchors it with `commit_trace` using the company's Anchor Wallet.
3. The web OS reads provenance + reputation back through `occa-sdk` (Provenance tab, agent reputation).
4. Payouts run through the Treasury disbursement instructions.

The SDK borsh layouts and PDA derivations are generated from the IDL in this repo, so **the IDL must stay in sync** (see Build).

## Layout

```
Anchor.toml                      # workspace: cluster + both program IDs
Cargo.toml                       # rust workspace (registry + treasury)
programs/
  registry/src/lib.rs            # identity, deployments, daily anchors, trace anchors
  treasury/src/lib.rs            # custody, policy, operations, disbursement
tests/                           # ts-node integration + smoke scripts (see Test)
target/                          # build artifacts (gitignored)
  idl/{registry,treasury}.json   # IDL — synced into occa-sdk
  deploy/*.so                    # compiled programs
  types/*.ts                     # anchor TS bindings
*-keypair.json                   # program keypairs (gitignored)
```

## Prerequisites

- Rust (stable) + `cargo`
- Solana CLI ≥ 1.18
- Anchor CLI ≥ 0.30
- A funded keypair at `~/.config/solana/id.json` — the deploy / upgrade authority and devnet fee payer
- Node + `ts-node` (for the `tests/` scripts)

## Build

```bash
anchor build
```

Writes `target/deploy/{registry,treasury}.so` and `target/idl/{registry,treasury}.json`. After **every** rebuild, sync the IDL into the SDK so it derives PDAs and decodes accounts against the current schema:

```bash
cd ../occa/packages/occa-sdk && pnpm sync-idl
```

Skipping this is the #1 cause of "the SDK reads garbage" bugs. Don't skip it.

## Test

Rust unit tests:

```bash
cargo test -p registry --features test
cargo test -p treasury --features test
```

End-to-end scripts against devnet (need a funded `~/.config/solana/id.json`). Each is a `package.json` script:

```bash
npm run bootstrap        # stand up a company + treasury + operations from scratch
npm run smoke            # broad happy-path sweep
npm run commit-trace     # anchor a fresh synthetic deliverable
npm run anchor-task      # anchor an existing off-chain task by id
npm run read-reputation  # fold trace anchors into a reputation view (AGENT_IDENTITY_PDA=…)
npm run disburse-routine # routine-class payout
npm run disburse-privileged
```

Other helpers live in `tests/` and run directly, e.g. `COMPANY_PDA=<pda> ts-node tests/check-anchor-ops.ts` to check Anchor-ops whitelist readiness.

## Deploy

Program IDs are vanity-grinded; the matching keypairs stay out of git — restore them from backup before deploying.

```bash
solana config set --url devnet
anchor deploy --provider.cluster devnet                 # both programs
anchor deploy -p registry --provider.cluster devnet     # one program
```

After a program upgrade that changed account layouts or instructions, re-run `anchor build` and `pnpm sync-idl`, then upgrade the on-chain IDL so explorers (explorer.solana.com decodes our IDL) and the SDK match.

Going to a fresh program ID (e.g. mainnet)? Generate a new keypair, `anchor keys sync`, then deploy.

## Versioning

Every account schema carries an explicit `version: u8` at offset 8, right after the 8-byte discriminator. Current versions:

| Account              | Version |
| -------------------- | ------- |
| CompanyAccount       | 3       |
| AgentIdentity        | 2       |
| Deployment           | 2       |
| DailyAnchorAccount   | 1       |
| TraceAnchorAccount   | 1       |
| TreasuryAccount      | 1       |
| PolicyAccount        | 1       |
| OperationsAccount    | 1       |
| ProtocolFeeAccount   | 1       |

Any breaking layout change must, in lockstep: (1) bump the version byte, (2) ship a migration ix or one-shot script that rewrites old accounts, (3) update the SDK borsh offsets. Discriminators are derived from the account name, not the layout, so they stay stable across versions — old indexer queries keep working. The Treasury reads `CompanyAccount.owner` by byte offset (no Cargo dep on registry, to avoid a CPI cycle), guarded by a `SUPPORTED_COMPANY_VERSION` check.

## Naming compliance

Per Whitepaper §15.7, OCCA avoids labor-coded terminology so the platform isn't pulled into employment-law classification. On-chain this means **"Deployment"** (never "Employment"), **"retire"** (never "fire"/"terminate"), `role` as a capability persona (never a job title), and "disbursement" / "operating allocation" (never "salary"/"payroll"). Keep it consistent in any new instruction, account, field, or error name. See `occa/CLAUDE.md` → "Regulatory naming guardrails".

## License

MIT. See [LICENSE](./LICENSE).
