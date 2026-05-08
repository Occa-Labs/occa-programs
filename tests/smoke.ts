/**
 * Smoke test: registry.create_company → CPI → treasury.init_treasury (atomic).
 *
 * Verifies:
 *   1. Single tx creates Company + Treasury + Policy PDAs
 *   2. company.treasury / .policy back-references match the actual PDAs
 *   3. treasury.accepted_assets == [SOL_PSEUDO_MINT] (default Pubkey)
 *   4. policy fields hold Phase 1 defaults (3% fee bps, u64::MAX threshold)
 *
 * Prerequisites:
 *   • Both programs deployed on devnet
 *   • Wallet at ~/.config/solana/id.json funded
 *   • IDL files at target/idl/{registry,treasury}.json (anchor build output)
 *
 * Run: `npm run smoke`
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

// Default Pubkey = SOL pseudo-mint sentinel inside treasury program.
const SOL_PSEUDO_MINT = PublicKey.default;
const EXPECTED_FEE_BPS = 300;
const U64_MAX = new anchor.BN("18446744073709551615");

// ─── Helpers ────────────────────────────────────────────────────────────────

function loadKeypair(path: string): Keypair {
  const raw = JSON.parse(readFileSync(path, "utf-8"));
  return Keypair.fromSecretKey(Uint8Array.from(raw));
}

function deriveCompanyPda(owner: PublicKey, nonce: number): [PublicKey, number] {
  const nonceLe = Buffer.alloc(4);
  nonceLe.writeUInt32LE(nonce, 0);
  return PublicKey.findProgramAddressSync(
    [Buffer.from("company"), owner.toBuffer(), nonceLe],
    REGISTRY_PROGRAM_ID
  );
}

function deriveTreasuryPda(company: PublicKey): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("treasury"), company.toBuffer()],
    TREASURY_PROGRAM_ID
  );
}

function derivePolicyPda(company: PublicKey): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("policy"), company.toBuffer()],
    TREASURY_PROGRAM_ID
  );
}

function loadIdl(path: string): anchor.Idl {
  return JSON.parse(readFileSync(path, "utf-8"));
}

function assert(cond: boolean, msg: string): void {
  if (!cond) {
    throw new Error(`✗ ${msg}`);
  }
  console.log(`  ✓ ${msg}`);
}

// ─── Main ───────────────────────────────────────────────────────────────────

async function main() {
  const walletPath = join(homedir(), ".config/solana/id.json");
  const wallet = new anchor.Wallet(loadKeypair(walletPath));
  const connection = new Connection(
    "https://api.devnet.solana.com",
    "confirmed"
  );
  const provider = new anchor.AnchorProvider(connection, wallet, {
    commitment: "confirmed",
  });
  anchor.setProvider(provider);

  const balance = await connection.getBalance(wallet.publicKey);
  console.log(`Wallet: ${wallet.publicKey.toBase58()}`);
  console.log(`Balance: ${balance / 1e9} SOL`);
  if (balance < 0.05 * 1e9) {
    throw new Error("Wallet balance below 0.05 SOL — fund it before running");
  }

  const registryIdl = loadIdl("target/idl/registry.json");
  const treasuryIdl = loadIdl("target/idl/treasury.json");
  const registry = new anchor.Program(registryIdl, provider);
  const treasury = new anchor.Program(treasuryIdl, provider);

  // Pick a fresh nonce so we don't collide with prior smoke runs.
  const nonce = Math.floor(Math.random() * 0xffffffff);
  const [companyPda] = deriveCompanyPda(wallet.publicKey, nonce);
  const [treasuryPda] = deriveTreasuryPda(companyPda);
  const [policyPda] = derivePolicyPda(companyPda);

  console.log(`\nNonce: ${nonce}`);
  console.log(`Company PDA:  ${companyPda.toBase58()}`);
  console.log(`Treasury PDA: ${treasuryPda.toBase58()}`);
  console.log(`Policy PDA:   ${policyPda.toBase58()}\n`);

  console.log("Sending create_company tx (atomic with init_treasury CPI)...");
  const sig = await registry.methods
    .createCompany(
      nonce,
      "Smoke Test Co",
      "en",
      "https://example.com/metadata.json",
      Array(32).fill(0)
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

  console.log(`Tx: https://explorer.solana.com/tx/${sig}?cluster=devnet\n`);

  // ─── Verify ────────────────────────────────────────────────────────────
  console.log("Verifying state:");

  const company = await (registry.account as any).companyAccount.fetch(
    companyPda
  );
  assert(
    company.owner.equals(wallet.publicKey),
    `company.owner == wallet (${wallet.publicKey.toBase58().slice(0, 8)}…)`
  );
  assert(
    company.treasury.equals(treasuryPda),
    `company.treasury back-ref correct`
  );
  assert(
    company.policy.equals(policyPda),
    `company.policy back-ref correct`
  );
  assert(company.nonce === nonce, `company.nonce == ${nonce}`);
  assert(company.status === 0, `company.status == ACTIVE (0)`);

  const treasuryAcc = await (treasury.account as any).treasuryAccount.fetch(
    treasuryPda
  );
  assert(
    treasuryAcc.company.equals(companyPda),
    `treasury.company back-ref correct`
  );
  assert(
    treasuryAcc.acceptedAssets.length === 1 &&
      treasuryAcc.acceptedAssets[0].equals(SOL_PSEUDO_MINT),
    `treasury.accepted_assets == [SOL_PSEUDO_MINT]`
  );

  const policyAcc = await (treasury.account as any).policyAccount.fetch(
    policyPda
  );
  assert(
    policyAcc.company.equals(companyPda),
    `policy.company back-ref correct`
  );
  assert(
    policyAcc.agentOperatingFeeBps === EXPECTED_FEE_BPS,
    `policy.agent_operating_fee_bps == ${EXPECTED_FEE_BPS} (3%)`
  );
  assert(
    policyAcc.privilegedThresholdLamports.eq(U64_MAX),
    `policy.privileged_threshold_lamports == u64::MAX (default)`
  );
  assert(
    policyAcc.routineBudgetPerMonth.length === 0,
    `policy.routine_budget_per_month is empty (default)`
  );
  assert(
    policyAcc.discretionaryBudgetPerMonth.length === 0,
    `policy.discretionary_budget_per_month is empty (default)`
  );
  assert(
    policyAcc.secondarySigner === null,
    `policy.secondary_signer is None (default)`
  );
  assert(
    policyAcc.currentPeriodAnchor.toString() === "0",
    `policy.current_period_anchor == 0 (lazy-init pending)`
  );

  console.log("\n✓ Smoke test PASSED — atomic CPI flow verified end-to-end.");
}

main().catch((err) => {
  console.error("\n✗ Smoke test FAILED:");
  console.error(err);
  process.exit(1);
});
