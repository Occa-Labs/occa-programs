/**
 * End-to-end test: disburse_privileged with secondary signer.
 *
 * Validates the third treasury auth class — Privileged-class disbursement
 * requires BOTH the controlling authority (owner) AND a secondary signer
 * registered in policy. This is for over-threshold or externally-destined
 * payouts that bypass normal budget caps.
 *
 * Flow:
 *   1. create_company (atomic CPI init_treasury)
 *   2. set_policy with secondary_signer + accepted_assets + threshold
 *   3. init_protocol_fee_account (skip if exists)
 *   4. register_agent_identity
 *   5. create_deployment
 *   6. set_receiving_address
 *   7. system.transfer (fund treasury)
 *   8. disburse_privileged signed by BOTH owner + secondary signer
 *
 * Verifies:
 *   • policy.secondary_signer == registered secondary pubkey
 *   • treasury balance decreased by gross (amount + fee, since
 *     is_agent_destination=true)
 *   • destination balance increased by amount
 *   • protocol_fee credited (fee applies for in-company agent)
 *
 * Run: `npm run disburse-privileged`
 */

import * as anchor from "@coral-xyz/anchor";
import {
  Connection,
  Keypair,
  PublicKey,
  SystemProgram,
  Transaction,
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
const EXPECTED_FEE_BPS = 300;
const TREASURY_FUND_LAMPORTS = 100_000_000n;
const DISBURSE_AMOUNT_LAMPORTS = 20_000_000n; // 0.02 SOL — above the
                                              // 0.01 SOL threshold below.
const PRIVILEGED_THRESHOLD = 10_000_000n;     // 0.01 SOL

function loadKeypair(p: string): Keypair {
  return Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(readFileSync(p, "utf-8"))),
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

function assert(cond: boolean, msg: string): void {
  if (!cond) throw new Error(`✗ ${msg}`);
  console.log(`  ✓ ${msg}`);
}

function lamports(n: bigint): string {
  return `${Number(n) / 1e9} SOL`;
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
  if (balance < 0.5 * 1e9) throw new Error("Need ≥0.5 SOL");

  const registry = new anchor.Program(
    loadIdl("target/idl/registry.json"),
    provider,
  );
  const treasury = new anchor.Program(
    loadIdl("target/idl/treasury.json"),
    provider,
  );

  // The secondary signer — a fresh keypair we control + co-sign with.
  // In real ops this would be a separate organizational wallet
  // (compliance officer, board signer, etc).
  const secondarySigner = Keypair.generate();
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

  console.log(`\nNonce:        ${nonce}`);
  console.log(`Company:      ${companyPda.toBase58()}`);
  console.log(`Owner:        ${wallet.publicKey.toBase58()}`);
  console.log(`2nd signer:   ${secondarySigner.publicKey.toBase58()}`);
  console.log(`Destination:  ${destination.toBase58()}\n`);

  // ── 1. create_company ──────────────────────────────────────────────
  console.log("[1/7] create_company…");
  await registry.methods
    .createCompany(
      nonce,
      "Privileged Test Co",
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

  // ── 2. set_policy with secondary_signer + threshold ────────────────
  console.log("[2/7] set_policy (secondary_signer + threshold)…");
  await treasury.methods
    .setPolicy({
      routineBudgetPerMonth: null,
      discretionaryBudgetPerMonth: null,
      privilegedThresholdLamports: new anchor.BN(PRIVILEGED_THRESHOLD.toString()),
      privilegedThresholdPerToken: null,
      // Outer-Some, Inner-Some: set to a specific pubkey.
      secondarySigner: secondarySigner.publicKey,
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

  // Verify policy state.
  const policyAfterSet = await (treasury.account as any).policyAccount.fetch(
    policyPda,
  );
  assert(
    policyAfterSet.secondarySigner !== null &&
      policyAfterSet.secondarySigner.equals(secondarySigner.publicKey),
    `policy.secondary_signer == registered secondary pubkey`,
  );
  assert(
    BigInt(policyAfterSet.privilegedThresholdLamports.toString()) ===
      PRIVILEGED_THRESHOLD,
    `policy.privileged_threshold_lamports == ${PRIVILEGED_THRESHOLD}`,
  );

  // ── 3. init_protocol_fee_account ───────────────────────────────────
  const feeExisting = await connection.getAccountInfo(protocolFeePda);
  if (feeExisting === null) {
    console.log("[3/7] init_protocol_fee_account (singleton)…");
    const [programDataPda] = PublicKey.findProgramAddressSync(
      [TREASURY_PROGRAM_ID.toBuffer()],
      new PublicKey("BPFLoaderUpgradeab1e11111111111111111111111"),
    );
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
    console.log("[3/7] init_protocol_fee_account — already exists, skipping");
  }

  // ── 4. register_agent_identity ─────────────────────────────────────
  console.log("[4/7] register_agent_identity…");
  await registry.methods
    .registerAgentIdentity(
      agentIdentityKp.publicKey,
      "Privileged Recipient",
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

  // ── 5. create_deployment ───────────────────────────────────────────
  console.log("[5/7] create_deployment…");
  await registry.methods
    .createDeployment(
      0,
      "executive",
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

  // ── 6. set_receiving_address + fund treasury ───────────────────────
  console.log("[6/7] set_receiving_address + fund treasury…");
  await registry.methods
    .setReceivingAddress(destination)
    .accounts({
      deployment: deploymentPda,
      owner: wallet.publicKey,
    })
    .rpc();
  await provider.sendAndConfirm(
    new Transaction().add(
      SystemProgram.transfer({
        fromPubkey: wallet.publicKey,
        toPubkey: treasuryPda,
        lamports: Number(TREASURY_FUND_LAMPORTS),
      }),
    ),
  );

  // ── 7. disburse_privileged (owner + secondary co-sign) ─────────────
  console.log(
    `[7/7] disburse_privileged (amount=${lamports(DISBURSE_AMOUNT_LAMPORTS)} > threshold=${lamports(PRIVILEGED_THRESHOLD)})…`,
  );

  const treasuryBefore = await connection.getBalance(treasuryPda);
  const destBefore = await connection.getBalance(destination);
  const feeBefore = await connection.getBalance(protocolFeePda);

  // Anchor's `.signers()` only attaches additional signers; the
  // provider wallet (owner) is already known. So we just pass the
  // secondary keypair here — Anchor co-signs both slots.
  await treasury.methods
    .disbursePrivileged(
      SOL_PSEUDO_MINT,
      new anchor.BN(DISBURSE_AMOUNT_LAMPORTS.toString()),
      true, // is_agent_destination — fee applies
    )
    .accounts({
      company: companyPda,
      controllingAuthority: wallet.publicKey,
      secondarySigner: secondarySigner.publicKey,
      treasury: treasuryPda,
      policy: policyPda,
      deployment: deploymentPda,
      destination,
      protocolFeeAccount: protocolFeePda,
    })
    .signers([secondarySigner])
    .rpc();

  // ── Verify ──────────────────────────────────────────────────────────
  console.log("\n──── Verifying balances ──────────────────────────────────────");
  const treasuryAfter = await connection.getBalance(treasuryPda);
  const destAfter = await connection.getBalance(destination);
  const feeAfter = await connection.getBalance(protocolFeePda);

  const expectedFee = (DISBURSE_AMOUNT_LAMPORTS * BigInt(EXPECTED_FEE_BPS)) / 10_000n;
  const expectedGross = DISBURSE_AMOUNT_LAMPORTS + expectedFee;

  assert(
    BigInt(treasuryBefore - treasuryAfter) === expectedGross,
    `treasury debited by gross=${expectedGross} (amount + 3% fee, is_agent=true)`,
  );
  assert(
    BigInt(destAfter - destBefore) === DISBURSE_AMOUNT_LAMPORTS,
    `destination credited by amount=${DISBURSE_AMOUNT_LAMPORTS}`,
  );
  assert(
    BigInt(feeAfter - feeBefore) === expectedFee,
    `protocol_fee credited by ${expectedFee} (3%)`,
  );

  console.log("\n✓ disburse_privileged end-to-end PASSED");
}

main().catch((err) => {
  console.error("\n✗ disburse-privileged FAILED:");
  console.error(err);
  process.exit(1);
});
