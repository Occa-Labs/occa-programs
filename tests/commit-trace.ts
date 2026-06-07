/**
 * Smoke test: registry.commit_trace — per-deliverable provenance anchor.
 *
 * Flow:
 *   1. create_company (+ CPI init_treasury/policy)
 *   2. register_agent_identity
 *   3. create_deployment (index 0, active)
 *   4. register_company_operations (Anchor kind, whitelist commit_trace)
 *   5. commit_trace (signed by the Anchor Wallet)
 *
 * Verifies the TraceAnchorAccount is written with the expected fields:
 *   • verdict == Passed (1), version == 1
 *   • task_id / content_hash / evidence_hash round-trip
 *   • agent == AgentIdentity PDA (reputation aggregates against identity)
 *   • result_uri / quality_score / rubric_version round-trip
 *
 * Prerequisites: registry (with commit_trace) + treasury deployed on
 * devnet, wallet at ~/.config/solana/id.json funded.
 *
 * Run: `npm run commit-trace`
 */

import * as anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey, SystemProgram } from "@solana/web3.js";
import { createHash, randomBytes } from "crypto";
import { readFileSync } from "fs";
import { homedir } from "os";
import { join } from "path";

// ─── Config ───────────────────────────────────────────────────────────────
const REGISTRY_PROGRAM_ID = new PublicKey(
  "occaTHMv5eYG5aZ85jimxTvHkBfsDCvndXC6J2k8kxr",
);
const TREASURY_PROGRAM_ID = new PublicKey(
  "occaxyVLnurdjedWCBPrvDCCto8wGYadtTZ3nAmcVzh",
);

// commit_trace discriminator (sha256("global:commit_trace")[..8]) — MUST
// match the IDL. Whitelisted on the Anchor-kind OperationsAccount.
const COMMIT_TRACE_DISC = Buffer.from([58, 140, 230, 51, 170, 109, 228, 125]);

const TRACE_VERDICT_PASSED = 1;
const OPERATIONS_KIND_ANCHOR = 1;

// ─── Helpers ────────────────────────────────────────────────────────────────
function loadKeypair(path: string): Keypair {
  return Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(readFileSync(path, "utf-8"))),
  );
}

function loadIdl(path: string): anchor.Idl {
  return JSON.parse(readFileSync(path, "utf-8"));
}

function u32Le(value: number): Buffer {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(value, 0);
  return b;
}

function sha256(s: string): Buffer {
  return createHash("sha256").update(s).digest();
}

function deriveCompanyPda(owner: PublicKey, nonce: number): PublicKey {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("company"), owner.toBuffer(), u32Le(nonce)],
    REGISTRY_PROGRAM_ID,
  )[0];
}
function deriveAgentIdentityPda(agent: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("agent_identity"), agent.toBuffer()],
    REGISTRY_PROGRAM_ID,
  )[0];
}
function deriveDeploymentPda(company: PublicKey, index: number): PublicKey {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("deployment"), company.toBuffer(), u32Le(index)],
    REGISTRY_PROGRAM_ID,
  )[0];
}
function deriveTreasuryPda(company: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("treasury"), company.toBuffer()],
    TREASURY_PROGRAM_ID,
  )[0];
}
function derivePolicyPda(company: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("policy"), company.toBuffer()],
    TREASURY_PROGRAM_ID,
  )[0];
}
function deriveOperationsPda(company: PublicKey, kindByte: number): PublicKey {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("operations"), company.toBuffer(), Buffer.from([kindByte])],
    TREASURY_PROGRAM_ID,
  )[0];
}
function deriveTraceAnchorPda(taskId: Buffer): PublicKey {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("trace"), taskId],
    REGISTRY_PROGRAM_ID,
  )[0];
}

function assert(cond: boolean, msg: string): void {
  if (!cond) throw new Error(`✗ ${msg}`);
  console.log(`  ✓ ${msg}`);
}

// ─── Main ───────────────────────────────────────────────────────────────────
async function main() {
  const wallet = new anchor.Wallet(
    loadKeypair(join(homedir(), ".config/solana/id.json")),
  );
  const connection = new Connection(
    "https://api.devnet.solana.com",
    "confirmed",
  );
  const provider = new anchor.AnchorProvider(connection, wallet, {
    commitment: "confirmed",
  });
  anchor.setProvider(provider);

  const registry = new anchor.Program(loadIdl("target/idl/registry.json"), provider);
  const treasury = new anchor.Program(loadIdl("target/idl/treasury.json"), provider);

  const balance = await connection.getBalance(wallet.publicKey);
  console.log(`Wallet:  ${wallet.publicKey.toBase58()}`);
  console.log(`Balance: ${balance / 1e9} SOL`);
  if (balance < 0.05 * 1e9) throw new Error("fund wallet ≥ 0.05 SOL first");

  const nonce = Math.floor(Math.random() * 0xffffffff);
  const agentKp = Keypair.generate();
  const anchorSigner = Keypair.generate();

  const companyPda = deriveCompanyPda(wallet.publicKey, nonce);
  const treasuryPda = deriveTreasuryPda(companyPda);
  const policyPda = derivePolicyPda(companyPda);
  const agentIdentityPda = deriveAgentIdentityPda(agentKp.publicKey);
  const deploymentPda = deriveDeploymentPda(companyPda, 0);
  const anchorOpsPda = deriveOperationsPda(companyPda, OPERATIONS_KIND_ANCHOR);

  // Deliverable under test.
  const taskId = randomBytes(32);
  const resultUri = "https://crypoch.com/news/smoke-test-article";
  const contentHash = sha256("the article content at completion");
  const evidenceHash = sha256("verification report: claims + sources");
  const qualityScore = 87;
  const rubricVersion = 1;
  const completedAt = Math.floor(Date.now() / 1000) - 60;
  const traceAnchorPda = deriveTraceAnchorPda(taskId);

  console.log(`\nNonce:        ${nonce}`);
  console.log(`Company:      ${companyPda.toBase58()}`);
  console.log(`Identity:     ${agentIdentityPda.toBase58()}`);
  console.log(`Deployment:   ${deploymentPda.toBase58()}`);
  console.log(`Anchor ops:   ${anchorOpsPda.toBase58()}`);
  console.log(`Trace anchor: ${traceAnchorPda.toBase58()}\n`);

  console.log("[1/5] create_company…");
  await registry.methods
    .createCompany(nonce, "Trace Smoke Co", "en", "https://example.com/c.json", Array(32).fill(0))
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

  console.log("[2/5] register_agent_identity…");
  await registry.methods
    .registerAgentIdentity(agentKp.publicKey, "Trace Writer", "https://example.com/a.json", Array(32).fill(0))
    .accounts({
      identity: agentIdentityPda,
      owner: wallet.publicKey,
      payer: wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log("[3/5] create_deployment (index=0)…");
  await registry.methods
    .createDeployment(0, "news_writer", null, Keypair.generate().publicKey, "https://example.com/d.json", Array(32).fill(0))
    .accounts({
      company: companyPda,
      identity: agentIdentityPda,
      owner: wallet.publicKey,
      deployment: deploymentPda,
      payer: wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log("[4/5] register_company_operations (Anchor, whitelist commit_trace)…");
  await treasury.methods
    .registerCompanyOperations(
      { anchor: {} }, // OperationsKind::Anchor
      anchorSigner.publicKey,
      [Array.from(COMMIT_TRACE_DISC)],
      100, // rate_limit_per_period
      new anchor.BN(0), // no expiry
    )
    .accounts({
      company: companyPda,
      controllingAuthority: wallet.publicKey,
      operations: anchorOpsPda,
      payer: wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log("[5/5] commit_trace (signed by Anchor Wallet)…");
  const sig = await registry.methods
    .commitTrace(
      Array.from(taskId),
      resultUri,
      Array.from(contentHash),
      qualityScore,
      rubricVersion,
      Array.from(evidenceHash),
      new anchor.BN(completedAt),
    )
    .accounts({
      deployment: deploymentPda,
      company: companyPda,
      anchorSigner: anchorSigner.publicKey,
      operations: anchorOpsPda,
      traceAnchor: traceAnchorPda,
      payer: wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .signers([anchorSigner])
    .rpc();
  console.log(`Tx: https://explorer.solana.com/tx/${sig}?cluster=devnet\n`);

  // ─── Verify ────────────────────────────────────────────────────────────
  console.log("Verifying TraceAnchorAccount:");
  const t = await (registry.account as any).traceAnchorAccount.fetch(traceAnchorPda);

  assert(t.version === 1, "version == 1");
  assert(Buffer.from(t.taskId).equals(taskId), "task_id round-trips");
  assert(t.company.equals(companyPda), "company back-ref correct");
  assert(t.agent.equals(agentIdentityPda), "agent == AgentIdentity PDA (not deployment)");
  assert(t.deployment.equals(deploymentPda), "deployment back-ref correct");
  assert(t.resultUri === resultUri, "result_uri round-trips");
  assert(Buffer.from(t.contentHash).equals(contentHash), "content_hash round-trips");
  assert(t.verdict === TRACE_VERDICT_PASSED, "verdict == Passed (1)");
  assert(t.qualityScore === qualityScore, `quality_score == ${qualityScore}`);
  assert(t.rubricVersion === rubricVersion, `rubric_version == ${rubricVersion}`);
  assert(Buffer.from(t.evidenceHash).equals(evidenceHash), "evidence_hash round-trips");
  assert(t.completedAt.toString() === completedAt.toString(), "completed_at round-trips");
  assert(t.committedBy.equals(anchorSigner.publicKey), "committed_by == Anchor Wallet");

  console.log("\n✓ commit_trace smoke test PASSED — provenance anchored end-to-end.");
}

main().catch((err) => {
  console.error("\n✗ commit_trace smoke test FAILED:");
  console.error(err);
  process.exit(1);
});
