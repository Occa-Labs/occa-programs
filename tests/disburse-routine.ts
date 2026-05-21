/**
 * End-to-end test: disburse_routine happy path.
 *
 * Walks the full flagship payroll flow on devnet:
 *   1. create_company (Company + Treasury + Policy via CPI)
 *   2. set_policy (set routine budget cap for SOL)
 *   3. init_protocol_fee_account (skip if singleton already exists)
 *   4. register_agent_identity
 *   5. create_deployment
 *   6. set_receiving_address (where disburse_routine sends SOL to)
 *   7. register_company_operations (Disbursement kind, whitelist disburse_routine)
 *   8. system.transfer (fund treasury with SOL — disburse pulls from here)
 *   9. disburse_routine (signed by operationsSigner, NOT controlling_authority)
 *
 * Verifies:
 *   • treasury balance decreased by gross (amount + fee)
 *   • destination balance increased by amount
 *   • protocol_fee_account balance increased by fee (3% of amount)
 *   • operations.signatures_this_period == 1
 *   • policy.routine_spent_this_period == [{ mint: SOL, amount: gross }]
 *
 * Prerequisites:
 *   • Both programs deployed on devnet
 *   • Wallet at ~/.config/solana/id.json funded with ≥0.5 SOL
 *   • IDL files at target/idl/{registry,treasury}.json
 *
 * Run: `npm run disburse-routine`
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

// ─── Config ─────────────────────────────────────────────────────────────────

const REGISTRY_PROGRAM_ID = new PublicKey(
  "occaTHMv5eYG5aZ85jimxTvHkBfsDCvndXC6J2k8kxr"
);
const TREASURY_PROGRAM_ID = new PublicKey(
  "occaxyVLnurdjedWCBPrvDCCto8wGYadtTZ3nAmcVzh"
);
const BPF_LOADER_UPGRADEABLE = new PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111"
);

const SOL_PSEUDO_MINT = PublicKey.default;

// Anchor discriminators (sha256("global:<name>")[..8]) — these MUST match
// the IDL. The whitelist passed to register_company_operations references
// disburse_routine's discriminator so the OperationsAccount permits the call.
const DISBURSE_ROUTINE_DISC = Buffer.from([
  45, 152, 225, 130, 133, 73, 62, 202,
]);

// Test parameters.
const TREASURY_FUND_LAMPORTS = 100_000_000n; // 0.1 SOL
const BUDGET_LAMPORTS = 50_000_000n; // 0.05 SOL / month cap
const DISBURSE_AMOUNT_LAMPORTS = 10_000_000n; // 0.01 SOL net to agent
const EXPECTED_FEE_BPS = 300; // 3%

// ─── Helpers ────────────────────────────────────────────────────────────────

function loadKeypair(path: string): Keypair {
  const raw = JSON.parse(readFileSync(path, "utf-8"));
  return Keypair.fromSecretKey(Uint8Array.from(raw));
}

function loadIdl(path: string): anchor.Idl {
  return JSON.parse(readFileSync(path, "utf-8"));
}

function u32Le(value: number): Buffer {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(value, 0);
  return b;
}

function deriveCompanyPda(
  owner: PublicKey,
  nonce: number,
): [PublicKey, number] {
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

function deriveDeploymentPda(
  company: PublicKey,
  index: number,
): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("deployment"), company.toBuffer(), u32Le(index)],
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

function deriveOperationsPda(
  company: PublicKey,
  kindByte: number,
): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("operations"), company.toBuffer(), Buffer.from([kindByte])],
    TREASURY_PROGRAM_ID,
  );
}

function deriveProgramDataPda(programId: PublicKey): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [programId.toBuffer()],
    BPF_LOADER_UPGRADEABLE,
  );
}

function assert(cond: boolean, msg: string): void {
  if (!cond) throw new Error(`✗ ${msg}`);
  console.log(`  ✓ ${msg}`);
}

function lamports(n: bigint): string {
  return `${Number(n) / 1e9} SOL`;
}

// ─── Main ───────────────────────────────────────────────────────────────────

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
    throw new Error("Wallet balance below 0.5 SOL — fund it before running");
  }

  const registry = new anchor.Program(
    loadIdl("target/idl/registry.json"),
    provider,
  );
  const treasury = new anchor.Program(
    loadIdl("target/idl/treasury.json"),
    provider,
  );

  // Per-run keypairs — fresh so we never collide with prior runs.
  const operationsSigner = Keypair.generate();
  const agentIdentityKp = Keypair.generate();
  const destination = Keypair.generate().publicKey;

  // Pick a fresh company nonce.
  const nonce = Math.floor(Math.random() * 0xffffffff);
  const [companyPda] = deriveCompanyPda(wallet.publicKey, nonce);
  const [treasuryPda] = deriveTreasuryPda(companyPda);
  const [policyPda] = derivePolicyPda(companyPda);
  const [agentIdentityPda] = deriveAgentIdentityPda(agentIdentityKp.publicKey);
  const [deploymentPda] = deriveDeploymentPda(companyPda, 0);
  const [protocolFeePda] = deriveProtocolFeePda();
  const [disbursementOpsPda] = deriveOperationsPda(companyPda, 0); // 0 = Disbursement

  console.log(`\n──── PDAs ─────────────────────────────────────────────────────`);
  console.log(`Company:        ${companyPda.toBase58()}`);
  console.log(`Treasury:       ${treasuryPda.toBase58()}`);
  console.log(`Policy:         ${policyPda.toBase58()}`);
  console.log(`AgentIdentity:  ${agentIdentityPda.toBase58()}`);
  console.log(`Deployment:     ${deploymentPda.toBase58()}`);
  console.log(`ProtocolFee:    ${protocolFeePda.toBase58()}`);
  console.log(`Operations(D):  ${disbursementOpsPda.toBase58()}`);
  console.log(`Ops signer:     ${operationsSigner.publicKey.toBase58()}`);
  console.log(`Destination:    ${destination.toBase58()}`);
  console.log(`Nonce:          ${nonce}`);

  // ── 1. create_company ─────────────────────────────────────────────────
  console.log(`\n[1/9] create_company (atomic CPI init_treasury)…`);
  await registry.methods
    .createCompany(
      nonce,
      "Disburse-Routine Test Co",
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

  // ── 2. set_policy ─────────────────────────────────────────────────────
  console.log(`[2/9] set_policy (routine budget = ${lamports(BUDGET_LAMPORTS)})…`);
  await treasury.methods
    .setPolicy({
      routineBudgetPerMonth: [
        { mint: SOL_PSEUDO_MINT, amount: new anchor.BN(BUDGET_LAMPORTS.toString()) },
      ],
      discretionaryBudgetPerMonth: null,
      privilegedThresholdLamports: null,
      privilegedThresholdPerToken: null,
      secondarySigner: null,
      agentOperatingFeeBps: null,
      acceptedAssets: null, // SOL already in default
    })
    .accounts({
      company: companyPda,
      controllingAuthority: wallet.publicKey,
      treasury: treasuryPda,
      policy: policyPda,
    })
    .rpc();

  // ── 3. init_protocol_fee_account (skip if exists) ─────────────────────
  const protocolFeeInfo = await connection.getAccountInfo(protocolFeePda);
  if (protocolFeeInfo === null) {
    console.log(`[3/9] init_protocol_fee_account (singleton)…`);
    const [programDataPda] = deriveProgramDataPda(TREASURY_PROGRAM_ID);
    await treasury.methods
      .initProtocolFeeAccount(wallet.publicKey)
      .accounts({
        protocolFeeAccount: protocolFeePda,
        authority: wallet.publicKey,
        program: TREASURY_PROGRAM_ID,
        programData: programDataPda,
        systemProgram: SystemProgram.programId,
      })
      .rpc();
  } else {
    console.log(`[3/9] init_protocol_fee_account — already exists, skipping`);
  }

  // ── 4. register_agent_identity ───────────────────────────────────────
  // agent_pubkey is an ix ARG (PDA-seed input), not a signer. The keypair
  // is generated client-side just so the pubkey is unique.
  console.log(`[4/9] register_agent_identity…`);
  await registry.methods
    .registerAgentIdentity(
      agentIdentityKp.publicKey,
      "Test Worker",
      "https://example.com/agent.json",
      Array(32).fill(0),
    )
    .accounts({
      identity: agentIdentityPda,
      owner: wallet.publicKey,
      payer: wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  // ── 5. create_deployment ─────────────────────────────────────────────
  // adapter_id is opaque to the chain — any pubkey works. Use a fresh
  // throwaway so we don't accidentally match a real adapter on devnet.
  const adapterId = Keypair.generate().publicKey;
  console.log(`[5/9] create_deployment (index=0)…`);
  await registry.methods
    .createDeployment(
      0,
      "writer",
      null, // parent_deployment_index — Option<u32>, root of hierarchy
      adapterId,
      "https://example.com/dep.json",
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

  // ── 6. set_receiving_address ─────────────────────────────────────────
  console.log(`[6/9] set_receiving_address…`);
  await registry.methods
    .setReceivingAddress(destination)
    .accounts({
      deployment: deploymentPda,
      owner: wallet.publicKey,
    })
    .rpc();

  // ── 7. register_company_operations (Disbursement) ────────────────────
  console.log(`[7/9] register_company_operations (Disbursement)…`);
  await treasury.methods
    .registerCompanyOperations(
      { disbursement: {} }, // OperationsKind::Disbursement
      operationsSigner.publicKey,
      [Array.from(DISBURSE_ROUTINE_DISC)],
      10, // rate_limit_per_period = 10
      new anchor.BN(0), // expiry = 0 (no expiry)
    )
    .accounts({
      company: companyPda,
      controllingAuthority: wallet.publicKey,
      operations: disbursementOpsPda,
      payer: wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  // ── 8. Fund treasury with SOL ────────────────────────────────────────
  console.log(`[8/9] system.transfer ${lamports(TREASURY_FUND_LAMPORTS)} → treasury…`);
  const fundTx = new anchor.web3.Transaction().add(
    SystemProgram.transfer({
      fromPubkey: wallet.publicKey,
      toPubkey: treasuryPda,
      lamports: Number(TREASURY_FUND_LAMPORTS),
    }),
  );
  await provider.sendAndConfirm(fundTx);

  // ── 9. disburse_routine ──────────────────────────────────────────────
  console.log(`[9/9] disburse_routine (signer=opsSigner, amount=${lamports(DISBURSE_AMOUNT_LAMPORTS)})…`);

  const treasuryBefore = await connection.getBalance(treasuryPda);
  const destBefore = await connection.getBalance(destination);
  const feeBefore = await connection.getBalance(protocolFeePda);

  await treasury.methods
    .disburseRoutine(
      SOL_PSEUDO_MINT,
      new anchor.BN(DISBURSE_AMOUNT_LAMPORTS.toString()),
    )
    .accounts({
      company: companyPda,
      treasury: treasuryPda,
      policy: policyPda,
      operations: disbursementOpsPda,
      operationsSigner: operationsSigner.publicKey,
      deployment: deploymentPda,
      destination,
      protocolFeeAccount: protocolFeePda,
    })
    .signers([operationsSigner])
    .rpc();

  // ── Verify ────────────────────────────────────────────────────────────
  console.log(`\n──── Verifying balances ──────────────────────────────────────`);
  const treasuryAfter = await connection.getBalance(treasuryPda);
  const destAfter = await connection.getBalance(destination);
  const feeAfter = await connection.getBalance(protocolFeePda);

  const expectedFee = (DISBURSE_AMOUNT_LAMPORTS * BigInt(EXPECTED_FEE_BPS)) / 10_000n;
  const expectedGross = DISBURSE_AMOUNT_LAMPORTS + expectedFee;

  assert(
    BigInt(treasuryBefore - treasuryAfter) === expectedGross,
    `treasury debited by gross=${expectedGross} (= amount + fee)`,
  );
  assert(
    BigInt(destAfter - destBefore) === DISBURSE_AMOUNT_LAMPORTS,
    `destination credited by amount=${DISBURSE_AMOUNT_LAMPORTS}`,
  );
  assert(
    BigInt(feeAfter - feeBefore) === expectedFee,
    `protocol_fee credited by fee=${expectedFee} (3% of amount)`,
  );

  console.log(`\n──── Verifying account state ─────────────────────────────────`);
  const opsAcc = await (treasury.account as any).operationsAccount.fetch(
    disbursementOpsPda,
  );
  assert(
    opsAcc.signaturesThisPeriod === 1,
    `operations.signatures_this_period == 1`,
  );
  assert(!opsAcc.revoked, `operations.revoked == false`);

  const policyAcc = await (treasury.account as any).policyAccount.fetch(
    policyPda,
  );
  const routineSpent = policyAcc.routineSpentThisPeriod;
  assert(
    routineSpent.length === 1 && routineSpent[0].mint.equals(SOL_PSEUDO_MINT),
    `policy.routine_spent_this_period has SOL entry`,
  );
  assert(
    BigInt(routineSpent[0].amount.toString()) === expectedGross,
    `policy.routine_spent.amount == gross (${expectedGross})`,
  );

  const feeAcc = await (treasury.account as any).protocolFeeAccount.fetch(
    protocolFeePda,
  );
  const feeBalances = feeAcc.balances as Array<{ mint: PublicKey; amount: anchor.BN }>;
  const solEntry = feeBalances.find((b) => b.mint.equals(SOL_PSEUDO_MINT));
  assert(
    solEntry !== undefined,
    `protocol_fee.balances contains SOL entry`,
  );

  console.log(`\n✓ disburse_routine end-to-end PASSED`);
}

main().catch((err) => {
  console.error("\n✗ disburse-routine test FAILED:");
  console.error(err);
  process.exit(1);
});
