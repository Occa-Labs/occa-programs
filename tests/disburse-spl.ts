/**
 * End-to-end test: SPL (USDC) disbursement + withdrawal happy paths.
 *
 * SPL sibling of disburse-routine.ts. Creates a fresh company, a throwaway
 * 6-decimal test mint (stand-in for USDC), funds the treasury's ATA, then
 * exercises all four SPL instructions in one run:
 *   • disburse_routine_spl       (operations-signer, budgeted, fee)
 *   • disburse_discretionary_spl (owner-signed, budgeted, fee)
 *   • disburse_privileged_spl    (owner-signed, agent dest, fee, below threshold)
 *   • withdraw_protocol_fees_spl (governance-signed, drains accrued fee)
 *
 * Verifies, per disbursement: destination ATA += amount, fee ATA += fee,
 * treasury ATA -= gross. For withdraw: fee ATA drained, balances decremented.
 *
 * Prereqs: both programs deployed (with SPL ix), wallet funded ≥0.5 SOL,
 * target/idl/{registry,treasury}.json current. Run: `npm run disburse-spl`.
 */

import * as anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey, SystemProgram } from "@solana/web3.js";
import {
  ASSOCIATED_TOKEN_PROGRAM_ID,
  TOKEN_PROGRAM_ID,
  createMint,
  getAssociatedTokenAddressSync,
  getOrCreateAssociatedTokenAccount,
  getAccount,
  mintTo,
} from "@solana/spl-token";
import { readFileSync } from "fs";
import { homedir } from "os";
import { join } from "path";

const REGISTRY_PROGRAM_ID = new PublicKey(
  "occaTHMv5eYG5aZ85jimxTvHkBfsDCvndXC6J2k8kxr",
);
const TREASURY_PROGRAM_ID = new PublicKey(
  "occaxyVLnurdjedWCBPrvDCCto8wGYadtTZ3nAmcVzh",
);
const BPF_LOADER_UPGRADEABLE = new PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111",
);

// Discriminators the OperationsAccount whitelist must reference for the
// routine SPL path (sha256("global:disburse_routine_spl")[..8]).
const DISBURSE_ROUTINE_SPL_DISC = [62, 246, 42, 181, 243, 213, 199, 222];

const DECIMALS = 6; // USDC
const ONE = 10n ** BigInt(DECIMALS);
const FUND = 100n * ONE; // 100 USDC into treasury ATA
const BUDGET = 50n * ONE; // 50 USDC/month cap (routine + discretionary each)
const AMOUNT = 10n * ONE; // 10 USDC net per disburse
const FEE_BPS = 300n; // 3%

function loadKeypair(path: string): Keypair {
  return Keypair.fromSecretKey(Uint8Array.from(JSON.parse(readFileSync(path, "utf-8"))));
}
function loadIdl(path: string): anchor.Idl {
  return JSON.parse(readFileSync(path, "utf-8"));
}
function u32Le(v: number): Buffer {
  const b = Buffer.alloc(4);
  b.writeUInt32LE(v, 0);
  return b;
}
function pda(seeds: (Buffer | Uint8Array)[], programId: PublicKey): PublicKey {
  return PublicKey.findProgramAddressSync(seeds, programId)[0];
}
function assert(cond: boolean, msg: string): void {
  if (!cond) throw new Error(`✗ ${msg}`);
  console.log(`  ✓ ${msg}`);
}
const bn = (n: bigint) => new anchor.BN(n.toString());
const usdc = (n: bigint) => `${Number(n) / Number(ONE)} USDC`;

async function ataBalance(connection: Connection, ata: PublicKey): Promise<bigint> {
  try {
    return (await getAccount(connection, ata)).amount;
  } catch {
    return 0n;
  }
}

async function main() {
  const walletKp = loadKeypair(join(homedir(), ".config/solana/id.json"));
  const wallet = new anchor.Wallet(walletKp);
  const heliusKey = (readFileSync(
    join(__dirname, "../../occa/apps/web/.env.local"),
    "utf-8",
  ).match(/api-key=([A-Za-z0-9-]+)/) || [])[1];
  const url = heliusKey
    ? `https://devnet.helius-rpc.com/?api-key=${heliusKey}`
    : "https://api.devnet.solana.com";
  const connection = new Connection(url, "confirmed");
  const provider = new anchor.AnchorProvider(connection, wallet, { commitment: "confirmed" });
  anchor.setProvider(provider);

  console.log(`Wallet: ${wallet.publicKey.toBase58()}`);
  console.log(`RPC:    ${heliusKey ? "Helius devnet" : "public devnet"}`);

  const registry = new anchor.Program(loadIdl("target/idl/registry.json"), provider);
  const treasury = new anchor.Program(loadIdl("target/idl/treasury.json"), provider);

  const operationsSigner = Keypair.generate();
  const agentKp = Keypair.generate();
  const destinationWallet = Keypair.generate().publicKey; // agent receiving wallet

  const nonce = Math.floor(Math.random() * 0xffffffff);
  const companyPda = pda([Buffer.from("company"), wallet.publicKey.toBuffer(), u32Le(nonce)], REGISTRY_PROGRAM_ID);
  const treasuryPda = pda([Buffer.from("treasury"), companyPda.toBuffer()], TREASURY_PROGRAM_ID);
  const policyPda = pda([Buffer.from("policy"), companyPda.toBuffer()], TREASURY_PROGRAM_ID);
  const agentIdentityPda = pda([Buffer.from("agent_identity"), agentKp.publicKey.toBuffer()], REGISTRY_PROGRAM_ID);
  const deploymentPda = pda([Buffer.from("deployment"), companyPda.toBuffer(), u32Le(0)], REGISTRY_PROGRAM_ID);
  const protocolFeePda = pda([Buffer.from("protocol_fees")], TREASURY_PROGRAM_ID);
  const opsPda = pda([Buffer.from("operations"), companyPda.toBuffer(), Buffer.from([0])], TREASURY_PROGRAM_ID);

  console.log(`\nCompany:   ${companyPda.toBase58()}`);
  console.log(`Treasury:  ${treasuryPda.toBase58()}`);

  // ── 1. create_company (atomic treasury+policy via CPI) ────────────────
  console.log(`\n[1] create_company…`);
  await registry.methods
    .createCompany(nonce, "SPL Disburse Test Co", "en", "https://example.com/m.json", Array(32).fill(0))
    .accounts({
      company: companyPda, owner: wallet.publicKey, payer: wallet.publicKey,
      treasury: treasuryPda, policy: policyPda,
      treasuryProgram: TREASURY_PROGRAM_ID, systemProgram: SystemProgram.programId,
    })
    .rpc();

  // ── 2. create test mint (6 dec), mint authority = wallet ──────────────
  console.log(`[2] create test USDC-style mint (6 decimals)…`);
  const mint = await createMint(connection, walletKp, wallet.publicKey, null, DECIMALS);
  console.log(`    mint: ${mint.toBase58()}`);

  // ── 3. set_policy: accept mint + routine/discretionary budgets +
  //       a high per-token privileged threshold (so below-threshold
  //       privileged needs no secondary signer) ─────────────────────────
  console.log(`[3] set_policy (accept mint, budgets=${usdc(BUDGET)})…`);
  await treasury.methods
    .setPolicy({
      routineBudgetPerMonth: [{ mint, amount: bn(BUDGET) }],
      discretionaryBudgetPerMonth: [{ mint, amount: bn(BUDGET) }],
      privilegedThresholdLamports: null,
      privilegedThresholdPerToken: [{ mint, amount: bn(1_000_000n * ONE) }],
      secondarySigner: null,
      agentOperatingFeeBps: null,
      acceptedAssets: [PublicKey.default, mint], // keep SOL, add mint
    })
    .accounts({ company: companyPda, controllingAuthority: wallet.publicKey, treasury: treasuryPda, policy: policyPda })
    .rpc();

  // ── 4. init_protocol_fee_account (skip if singleton exists) ───────────
  if ((await connection.getAccountInfo(protocolFeePda)) === null) {
    console.log(`[4] init_protocol_fee_account…`);
    const programDataPda = pda([TREASURY_PROGRAM_ID.toBuffer()], BPF_LOADER_UPGRADEABLE);
    await treasury.methods
      .initProtocolFeeAccount(wallet.publicKey)
      .accounts({
        protocolFeeAccount: protocolFeePda, authority: wallet.publicKey,
        program: TREASURY_PROGRAM_ID, programData: programDataPda, systemProgram: SystemProgram.programId,
      })
      .rpc();
  } else {
    console.log(`[4] protocol_fee_account exists — skip`);
  }

  // ── 5. agent identity + deployment + receiving address ────────────────
  console.log(`[5] register_agent_identity + create_deployment + set_receiving_address…`);
  await registry.methods
    .registerAgentIdentity(agentKp.publicKey, "SPL Worker", "https://example.com/agent.json", Array(32).fill(0))
    .accounts({ identity: agentIdentityPda, owner: wallet.publicKey, payer: wallet.publicKey, systemProgram: SystemProgram.programId })
    .rpc();
  await registry.methods
    .createDeployment(0, "writer", null, Keypair.generate().publicKey, "https://example.com/dep.json", Array(32).fill(0))
    .accounts({ company: companyPda, identity: agentIdentityPda, owner: wallet.publicKey, deployment: deploymentPda, payer: wallet.publicKey, systemProgram: SystemProgram.programId })
    .rpc();
  await registry.methods
    .setReceivingAddress(destinationWallet)
    .accounts({ deployment: deploymentPda, owner: wallet.publicKey })
    .rpc();

  // ── 6. register Disbursement operations (whitelist routine_spl) ───────
  console.log(`[6] register_company_operations (whitelist disburse_routine_spl)…`);
  await treasury.methods
    .registerCompanyOperations({ disbursement: {} }, operationsSigner.publicKey, [DISBURSE_ROUTINE_SPL_DISC], 10, new anchor.BN(0))
    .accounts({ company: companyPda, controllingAuthority: wallet.publicKey, operations: opsPda, payer: wallet.publicKey, systemProgram: SystemProgram.programId })
    .rpc();

  // ── 7. create + fund treasury ATA with test USDC ──────────────────────
  console.log(`[7] fund treasury ATA with ${usdc(FUND)}…`);
  const treasuryAta = getAssociatedTokenAddressSync(mint, treasuryPda, true);
  const destAta = getAssociatedTokenAddressSync(mint, destinationWallet, true);
  const feeAta = getAssociatedTokenAddressSync(mint, protocolFeePda, true);
  await getOrCreateAssociatedTokenAccount(connection, walletKp, mint, treasuryPda, true);
  await mintTo(connection, walletKp, mint, treasuryAta, wallet.publicKey, FUND);

  // Fund the operations signer: the routine_spl path makes it the rent payer
  // for the destination + fee ATAs created on first payout (mirrors the
  // funded operator hot wallet in production).
  console.log(`    funding operations signer 0.02 SOL for ATA rent…`);
  await provider.sendAndConfirm(
    new anchor.web3.Transaction().add(
      SystemProgram.transfer({ fromPubkey: wallet.publicKey, toPubkey: operationsSigner.publicKey, lamports: 20_000_000 }),
    ),
  );

  const expectedFee = (AMOUNT * FEE_BPS) / 10_000n;
  const expectedGross = AMOUNT + expectedFee;

  const splAccounts = {
    company: companyPda, treasury: treasuryPda, policy: policyPda,
    deployment: deploymentPda, destination: destinationWallet, mint,
    treasuryTokenAccount: treasuryAta, destinationTokenAccount: destAta,
    protocolFeeAccount: protocolFeePda, feeTokenAccount: feeAta,
    tokenProgram: TOKEN_PROGRAM_ID, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
  };

  // ── 8. disburse_routine_spl ───────────────────────────────────────────
  console.log(`\n[8] disburse_routine_spl…`);
  let tB = await ataBalance(connection, treasuryAta), dB = await ataBalance(connection, destAta), fB = await ataBalance(connection, feeAta);
  await treasury.methods.disburseRoutineSpl(mint, bn(AMOUNT))
    .accountsPartial({ ...splAccounts, operations: opsPda, operationsSigner: operationsSigner.publicKey })
    .signers([operationsSigner]).rpc();
  let tA = await ataBalance(connection, treasuryAta), dA = await ataBalance(connection, destAta), fA = await ataBalance(connection, feeAta);
  assert(tB - tA === expectedGross, `routine_spl: treasury -= gross`);
  assert(dA - dB === AMOUNT, `routine_spl: dest += amount`);
  assert(fA - fB === expectedFee, `routine_spl: fee += fee`);

  // ── 9. disburse_discretionary_spl ─────────────────────────────────────
  console.log(`[9] disburse_discretionary_spl…`);
  tB = tA; dB = dA; fB = fA;
  await treasury.methods.disburseDiscretionarySpl(mint, bn(AMOUNT))
    .accountsPartial({ ...splAccounts, controllingAuthority: wallet.publicKey })
    .rpc();
  tA = await ataBalance(connection, treasuryAta); dA = await ataBalance(connection, destAta); fA = await ataBalance(connection, feeAta);
  assert(tB - tA === expectedGross, `discretionary_spl: treasury -= gross`);
  assert(dA - dB === AMOUNT, `discretionary_spl: dest += amount`);
  assert(fA - fB === expectedFee, `discretionary_spl: fee += fee`);

  // ── 10. disburse_privileged_spl (agent dest, below threshold) ─────────
  console.log(`[10] disburse_privileged_spl (agent dest, below threshold → no secondary)…`);
  tB = tA; dB = dA; fB = fA;
  await treasury.methods.disbursePrivilegedSpl(mint, bn(AMOUNT), true)
    .accountsPartial({ ...splAccounts, controllingAuthority: wallet.publicKey })
    .rpc();
  tA = await ataBalance(connection, treasuryAta); dA = await ataBalance(connection, destAta); fA = await ataBalance(connection, feeAta);
  assert(tB - tA === expectedGross, `privileged_spl: treasury -= gross`);
  assert(dA - dB === AMOUNT, `privileged_spl: dest += amount`);
  assert(fA - fB === expectedFee, `privileged_spl: fee += fee`);

  // ── 11. withdraw_protocol_fees_spl ────────────────────────────────────
  console.log(`[11] withdraw_protocol_fees_spl (governance drains accrued fee)…`);
  const govDestWallet = wallet.publicKey;
  const govDestAta = getAssociatedTokenAddressSync(mint, govDestWallet, true);
  const feeAccrued = await ataBalance(connection, feeAta);
  const govBefore = await ataBalance(connection, govDestAta);
  await treasury.methods.withdrawProtocolFeesSpl(mint, bn(feeAccrued))
    .accountsPartial({
      protocolFeeAccount: protocolFeePda, governance: wallet.publicKey, destination: govDestWallet, mint,
      feeTokenAccount: feeAta, destinationTokenAccount: govDestAta,
      tokenProgram: TOKEN_PROGRAM_ID, associatedTokenProgram: ASSOCIATED_TOKEN_PROGRAM_ID, systemProgram: SystemProgram.programId,
    })
    .rpc();
  const feeAfterWd = await ataBalance(connection, feeAta);
  const govAfter = await ataBalance(connection, govDestAta);
  assert(feeAfterWd === 0n, `withdraw_spl: fee ATA drained to 0`);
  assert(govAfter - govBefore === feeAccrued, `withdraw_spl: governance dest ATA += ${usdc(feeAccrued)}`);
  const feeAcc = await (treasury.account as any).protocolFeeAccount.fetch(protocolFeePda);
  const entry = (feeAcc.balances as any[]).find((b) => b.mint.equals(mint));
  assert(!entry || BigInt(entry.amount.toString()) === 0n, `withdraw_spl: tracked balance for mint == 0`);

  console.log(`\n✓ ALL FOUR SPL PATHS PASSED on devnet`);
  console.log(`  company:  ${companyPda.toBase58()}`);
  console.log(`  mint:     ${mint.toBase58()}`);
}

main().catch((err) => {
  console.error("\n✗ disburse-spl test FAILED:");
  console.error(err);
  process.exit(1);
});
