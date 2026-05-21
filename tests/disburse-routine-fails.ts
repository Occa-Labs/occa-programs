/**
 * Fail-path tests: disburse_routine on-chain guards.
 *
 * Sets up one company + treasury + policy + deployment + ops, then runs
 * three fail scenarios sharing that infra (state mutated between via
 * update_operations_capability / revoke_operations). Each scenario must
 * fail with the expected on-chain error.
 *
 * Scenarios:
 *   A. Rate limit exceeded — set rate_limit=1, do one ok call, second fails
 *   B. Whitelist miss — empty whitelist, attempt fails with DiscriminatorNotWhitelisted
 *   C. Revoked — revoke_operations, attempt fails with OperationsRevoked
 *
 * Scenarios run in order; C terminates the ops account (no resurrection).
 *
 * Run: `npm run disburse-routine-fails`
 */

import * as anchor from "@coral-xyz/anchor";
import {
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
} from "@solana/web3.js";
import { readFileSync } from "fs";
import { homedir } from "os";
import { join } from "path";

const REGISTRY_PROGRAM_ID = new PublicKey(
  "occaTHMv5eYG5aZ85jimxTvHkBfsDCvndXC6J2k8kxr",
);
const TREASURY_PROGRAM_ID = new PublicKey(
  "occaxyVLnurdjedWCBPrvDCCto8wGYadtTZ3nAmcVzh",
);

const SOL_PSEUDO_MINT = PublicKey.default;
const DISBURSE_ROUTINE_DISC = Buffer.from([
  45, 152, 225, 130, 133, 73, 62, 202,
]);
// Used in Scenario B to populate the whitelist with a NON-matching action
// (program rejects an outright empty whitelist via EmptyWhitelist).
const COMMIT_DAILY_ANCHOR_DISC = Buffer.from([
  18, 7, 3, 65, 58, 148, 164, 0,
]);

const TREASURY_FUND_LAMPORTS = 100_000_000n;
const DISBURSE_AMOUNT_LAMPORTS = 5_000_000n;

function loadKeypair(path: string): Keypair {
  return Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(readFileSync(path, "utf-8"))),
  );
}

function loadIdl(p: string): anchor.Idl {
  return JSON.parse(readFileSync(p, "utf-8"));
}

function u32Le(value: number): Buffer {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(value, 0);
  return b;
}

function deriveCompanyPda(owner: PublicKey, nonce: number): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("company"), owner.toBuffer(), u32Le(nonce)],
    REGISTRY_PROGRAM_ID,
  );
}

function deriveAgentIdentityPda(agent: PublicKey): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("agent_identity"), agent.toBuffer()],
    REGISTRY_PROGRAM_ID,
  );
}

function deriveDeploymentPda(company: PublicKey, idx: number): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("deployment"), company.toBuffer(), u32Le(idx)],
    REGISTRY_PROGRAM_ID,
  );
}

function deriveTreasuryPda(company: PublicKey): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("treasury"), company.toBuffer()],
    TREASURY_PROGRAM_ID,
  );
}

function derivePolicyPda(company: PublicKey): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("policy"), company.toBuffer()],
    TREASURY_PROGRAM_ID,
  );
}

function deriveProtocolFeePda(): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("protocol_fees")],
    TREASURY_PROGRAM_ID,
  );
}

function deriveOperationsPda(company: PublicKey, kindByte: number): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("operations"), company.toBuffer(), Buffer.from([kindByte])],
    TREASURY_PROGRAM_ID,
  );
}

function assertOk(cond: boolean, msg: string): void {
  if (!cond) throw new Error(`✗ ${msg}`);
  console.log(`  ✓ ${msg}`);
}

// Inspect an Anchor RPC failure for a specific named error. Anchor wraps
// program errors in `SendTransactionError` with logs containing
// "Error Message: <ErrorVariant>" or a numeric code — we match by name
// since the names are stable across builds.
async function expectError(
  promise: Promise<unknown>,
  errorName: string,
): Promise<void> {
  try {
    await promise;
  } catch (err) {
    const msg = err instanceof Error ? err.message : String(err);
    const logs = (err as { logs?: string[] }).logs?.join("\n") ?? "";
    const haystack = `${msg}\n${logs}`;
    if (haystack.includes(errorName)) {
      console.log(`  ✓ failed with ${errorName} (expected)`);
      return;
    }
    throw new Error(
      `expected ${errorName}, got different error:\n${msg}\nlogs:\n${logs}`,
    );
  }
  throw new Error(`expected ${errorName} but call SUCCEEDED`);
}

async function main() {
  const walletPath = join(homedir(), ".config/solana/id.json");
  const wallet = new anchor.Wallet(loadKeypair(walletPath));
  const connection = new Connection(
    "https://api.devnet.solana.com",
    "confirmed",
  );
  const provider = new anchor.AnchorProvider(connection, wallet, {
    commitment: "confirmed",
  });
  anchor.setProvider(provider);

  const balance = await connection.getBalance(wallet.publicKey);
  console.log(`Wallet:  ${wallet.publicKey.toBase58()}`);
  console.log(`Balance: ${balance / 1e9} SOL`);
  if (balance < 0.5 * 1e9) {
    throw new Error("Need ≥0.5 SOL to run");
  }

  const registry = new anchor.Program(
    loadIdl("target/idl/registry.json"),
    provider,
  );
  const treasury = new anchor.Program(
    loadIdl("target/idl/treasury.json"),
    provider,
  );

  // Per-run keypairs.
  const operationsSigner = Keypair.generate();
  const agentIdentityKp = Keypair.generate();
  const destination = Keypair.generate().publicKey;
  const adapterId = Keypair.generate().publicKey;

  const nonce = Math.floor(Math.random() * 0xffffffff);
  const [companyPda] = deriveCompanyPda(wallet.publicKey, nonce);
  const [treasuryPda] = deriveTreasuryPda(companyPda);
  const [policyPda] = derivePolicyPda(companyPda);
  const [agentIdentityPda] = deriveAgentIdentityPda(agentIdentityKp.publicKey);
  const [deploymentPda] = deriveDeploymentPda(companyPda, 0);
  const [protocolFeePda] = deriveProtocolFeePda();
  const [opsPda] = deriveOperationsPda(companyPda, 0);

  console.log(`\nNonce:        ${nonce}`);
  console.log(`Company:      ${companyPda.toBase58()}`);
  console.log(`Operations:   ${opsPda.toBase58()}`);
  console.log(`Ops signer:   ${operationsSigner.publicKey.toBase58()}\n`);

  // ── Setup (shared) ─────────────────────────────────────────────────────
  console.log("─── Setup ──────────────────────────────────────────────────");
  console.log("create_company…");
  await registry.methods
    .createCompany(
      nonce,
      "Disburse-Fails Test Co",
      "en",
      "https://example.com/m.json",
      Array(32).fill(0),
    )
    .accounts({
      company: companyPda,
      owner: wallet.publicKey,
      payer: wallet.publicKey,
      treasury: treasuryPda,
      policy: policyPda,
      treasuryProgram: TREASURY_PROGRAM_ID,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log("set_policy (routine budget = 0.05 SOL)…");
  await treasury.methods
    .setPolicy({
      routineBudgetPerMonth: [
        { mint: SOL_PSEUDO_MINT, amount: new anchor.BN(50_000_000) },
      ],
      discretionaryBudgetPerMonth: null,
      privilegedThresholdLamports: null,
      privilegedThresholdPerToken: null,
      secondarySigner: null,
      agentOperatingFeeBps: null,
      acceptedAssets: null,
    })
    .accounts({
      company: companyPda,
      controllingAuthority: wallet.publicKey,
      treasury: treasuryPda,
      policy: policyPda,
    })
    .rpc();

  console.log("register_agent_identity…");
  await registry.methods
    .registerAgentIdentity(
      agentIdentityKp.publicKey,
      "Test",
      "https://example.com/a.json",
      Array(32).fill(0),
    )
    .accounts({
      identity: agentIdentityPda,
      owner: wallet.publicKey,
      payer: wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log("create_deployment…");
  await registry.methods
    .createDeployment(
      0,
      "writer",
      null,
      adapterId,
      "https://example.com/d.json",
      Array(32).fill(0),
    )
    .accounts({
      company: companyPda,
      identity: agentIdentityPda,
      owner: wallet.publicKey,
      deployment: deploymentPda,
      payer: wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log("set_receiving_address…");
  await registry.methods
    .setReceivingAddress(destination)
    .accounts({
      deployment: deploymentPda,
      owner: wallet.publicKey,
    })
    .rpc();

  console.log("register_company_operations (rate_limit=1, whitelist=[disburse_routine])…");
  await treasury.methods
    .registerCompanyOperations(
      { disbursement: {} },
      operationsSigner.publicKey,
      [Array.from(DISBURSE_ROUTINE_DISC)],
      1, // rate_limit = 1 — first call OK, second triggers RateLimitExceeded
      new anchor.BN(0),
    )
    .accounts({
      company: companyPda,
      controllingAuthority: wallet.publicKey,
      operations: opsPda,
      payer: wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log("fund treasury…");
  await provider.sendAndConfirm(
    new anchor.web3.Transaction().add(
      SystemProgram.transfer({
        fromPubkey: wallet.publicKey,
        toPubkey: treasuryPda,
        lamports: Number(TREASURY_FUND_LAMPORTS),
      }),
    ),
  );

  // Helper: build a disburse_routine call with current setup.
  const disburse = () =>
    treasury.methods
      .disburseRoutine(
        SOL_PSEUDO_MINT,
        new anchor.BN(DISBURSE_AMOUNT_LAMPORTS.toString()),
      )
      .accounts({
        company: companyPda,
        treasury: treasuryPda,
        policy: policyPda,
        operations: opsPda,
        operationsSigner: operationsSigner.publicKey,
        deployment: deploymentPda,
        destination,
        protocolFeeAccount: protocolFeePda,
      })
      .signers([operationsSigner])
      .rpc();

  // ── Scenario A: rate limit ─────────────────────────────────────────────
  console.log("\n─── Scenario A: rate limit exceeded ─────────────────────────");
  console.log("first disburse (consumes the 1-allowed quota)…");
  await disburse();
  const opsAfterOne = await (treasury.account as any).operationsAccount.fetch(opsPda);
  assertOk(opsAfterOne.signaturesThisPeriod === 1, "ops.signatures_this_period == 1");
  console.log("second disburse (should fail)…");
  await expectError(disburse(), "RateLimitExceeded");

  // ── Scenario B: whitelist miss ────────────────────────────────────────
  // Program rejects an outright empty whitelist via EmptyWhitelist, so
  // we populate with a non-matching discriminator instead.
  console.log("\n─── Scenario B: whitelist miss ──────────────────────────────");
  console.log("update_operations_capability: bump rate limit + replace whitelist with commit_daily_anchor disc…");
  await treasury.methods
    .updateOperationsCapability({
      actionWhitelist: [Array.from(COMMIT_DAILY_ANCHOR_DISC)],
      rateLimitPerPeriod: 10, // raise so rate-limit isn't the cause
      expiryUnix: null,
    })
    .accounts({
      company: companyPda,
      controllingAuthority: wallet.publicKey,
      operations: opsPda,
    })
    .rpc();
  console.log("disburse (should fail — disburse_routine disc not in whitelist)…");
  await expectError(disburse(), "DiscriminatorNotWhitelisted");

  // ── Scenario C: revoked ───────────────────────────────────────────────
  console.log("\n─── Scenario C: revoked operations ──────────────────────────");
  console.log("update_operations_capability: restore whitelist (so revoke is the only block)…");
  await treasury.methods
    .updateOperationsCapability({
      actionWhitelist: [Array.from(DISBURSE_ROUTINE_DISC)],
      rateLimitPerPeriod: null,
      expiryUnix: null,
    })
    .accounts({
      company: companyPda,
      controllingAuthority: wallet.publicKey,
      operations: opsPda,
    })
    .rpc();
  console.log("revoke_operations…");
  await treasury.methods
    .revokeOperations()
    .accounts({
      company: companyPda,
      controllingAuthority: wallet.publicKey,
      operations: opsPda,
    })
    .rpc();
  const opsAfterRevoke = await (treasury.account as any).operationsAccount.fetch(opsPda);
  assertOk(opsAfterRevoke.revoked === true, "ops.revoked == true");
  console.log("disburse (should fail — revoked)…");
  await expectError(disburse(), "OperationsRevoked");

  console.log("\n✓ All 3 fail-path scenarios PASSED");
}

main().catch((err) => {
  console.error("\n✗ disburse-routine-fails FAILED:");
  console.error(err);
  process.exit(1);
});
