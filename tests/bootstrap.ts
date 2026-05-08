/**
 * Bootstrap: one-time post-deploy setup for the treasury program.
 *
 * Calls `init_protocol_fee_account(governance)` — singleton ix that creates
 * the protocol-fee accumulator PDA. Must be called by the program's upgrade
 * authority (verified via ProgramData PDA inside the ix).
 *
 * Idempotent: safe to re-run — second call fails with "AccountAlreadyExists"
 * because the singleton PDA seed is fixed.
 *
 * Run: `npm run bootstrap`
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

const TREASURY_PROGRAM_ID = new PublicKey(
  "occaxyVLnurdjedWCBPrvDCCto8wGYadtTZ3nAmcVzh"
);

// BPF Upgradeable Loader — used to derive ProgramData PDA.
const BPF_LOADER_UPGRADEABLE = new PublicKey(
  "BPFLoaderUpgradeab1e11111111111111111111111"
);

function loadKeypair(path: string): Keypair {
  const raw = JSON.parse(readFileSync(path, "utf-8"));
  return Keypair.fromSecretKey(Uint8Array.from(raw));
}

function deriveProtocolFeesPda(): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [Buffer.from("protocol_fees")],
    TREASURY_PROGRAM_ID
  );
}

function deriveProgramDataPda(programId: PublicKey): [PublicKey, number] {
  return PublicKey.findProgramAddressSync(
    [programId.toBuffer()],
    BPF_LOADER_UPGRADEABLE
  );
}

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

  const treasuryIdl = JSON.parse(
    readFileSync("target/idl/treasury.json", "utf-8")
  );
  const treasury = new anchor.Program(treasuryIdl, provider);

  const [protocolFeesPda] = deriveProtocolFeesPda();
  const [programDataPda] = deriveProgramDataPda(TREASURY_PROGRAM_ID);

  console.log(`Authority:        ${wallet.publicKey.toBase58()}`);
  console.log(`ProtocolFees PDA: ${protocolFeesPda.toBase58()}`);
  console.log(`ProgramData PDA:  ${programDataPda.toBase58()}`);

  // Check if already initialized.
  const existing = await connection.getAccountInfo(protocolFeesPda);
  if (existing !== null) {
    console.log(
      `\n✓ Already initialized (account size: ${existing.data.length} bytes). No-op.`
    );
    return;
  }

  // Governance can differ from upgrade authority — for Phase 1 devnet, set
  // it to the same wallet for simplicity. Mainnet would set this to a
  // multisig / DAO key.
  const governance = wallet.publicKey;

  console.log(`\nCalling init_protocol_fee_account(governance=${governance.toBase58()})...`);
  const sig = await treasury.methods
    .initProtocolFeeAccount(governance)
    .accounts({
      protocolFeeAccount: protocolFeesPda,
      authority: wallet.publicKey,
      program: TREASURY_PROGRAM_ID,
      programData: programDataPda,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log(`Tx: https://explorer.solana.com/tx/${sig}?cluster=devnet`);

  const after = await (treasury.account as any).protocolFeeAccount.fetch(
    protocolFeesPda
  );
  console.log(`\n✓ ProtocolFeeAccount initialized:`);
  console.log(`  governance: ${after.governance.toBase58()}`);
  console.log(`  balances: ${after.balances.length} entries (empty)`);
}

main().catch((err) => {
  console.error("\n✗ Bootstrap failed:");
  console.error(err);
  process.exit(1);
});
