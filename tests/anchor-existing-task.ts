/**
 * Manually anchor ONE already-verified task via commit_trace, using the
 * real company + deployment + operator (Anchor Wallet) — replicates what
 * the server's commit-trace-engine does. Proves the on-chain path works
 * for a real company now that the whitelist includes commit_trace.
 *
 * Env (all have f98a9d70 defaults):
 *   COMPANY_ID, TASK_ID, COMPANY_PDA, DEPLOYMENT_PDA, EVIDENCE_HASH,
 *   QUALITY_SCORE, RUBRIC_VERSION, COMPLETED_AT, DELIVERABLE_FILE, RESULT_URI
 *
 * Run: `npm run anchor-task`
 */

import * as anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey, SystemProgram } from "@solana/web3.js";
import { createHash } from "crypto";
import { readFileSync } from "fs";
import { join } from "path";

// eslint-disable-next-line @typescript-eslint/no-var-requires
const bs58 = require("bs58") as { decode: (s: string) => Uint8Array };

const REGISTRY_PROGRAM_ID = new PublicKey(
  "occaTHMv5eYG5aZ85jimxTvHkBfsDCvndXC6J2k8kxr",
);
const TREASURY_PROGRAM_ID = new PublicKey(
  "occaxyVLnurdjedWCBPrvDCCto8wGYadtTZ3nAmcVzh",
);
const OPERATIONS_KIND_ANCHOR = 1;
const OCCA_ENV = join(__dirname, "../../occa/.env");

function readEnvVar(key: string): string | undefined {
  if (process.env[key]) return process.env[key];
  try {
    const text = readFileSync(OCCA_ENV, "utf-8");
    for (const line of text.split("\n")) {
      const m = line.match(new RegExp(`^\\s*${key}\\s*=\\s*(.*)\\s*$`));
      if (m) return m[1].replace(/^["']|["']$/g, "").trim();
    }
  } catch {
    /* no .env */
  }
  return undefined;
}

function env(key: string, fallback: string): string {
  return process.env[key] ?? fallback;
}

function sha256(buf: Buffer): Buffer {
  return createHash("sha256").update(buf).digest();
}

async function main() {
  const COMPANY_ID = env("COMPANY_ID", "6b3855cb-d0a9-45fc-8e18-8fada01272f3");
  const TASK_ID = env("TASK_ID", "f98a9d70-f13f-41d3-be9e-e1afcfc51e30");
  const companyPda = new PublicKey(
    env("COMPANY_PDA", "7YPkHACUahgbG2GZBe5R1eLo8sWrLYU1RB1ZcdSH5n81"),
  );
  const deploymentPda = new PublicKey(
    env("DEPLOYMENT_PDA", "J8CxLDe4uv22JzRE8vux1rM8DZh7XASV1GRhYvQeepBr"),
  );
  const evidenceHash = Buffer.from(
    env(
      "EVIDENCE_HASH",
      "5ebccde9e0ee0c797ba3722dcbfc583cb6b337858adc448cb85653ddfd2c0e4a",
    ),
    "hex",
  );
  const qualityScore = parseInt(env("QUALITY_SCORE", "100"), 10);
  const rubricVersion = parseInt(env("RUBRIC_VERSION", "1"), 10);
  const completedAt = parseInt(env("COMPLETED_AT", "1780810332"), 10);
  const resultUri = env("RESULT_URI", "");
  const deliverableFile = env(
    "DELIVERABLE_FILE",
    "/tmp/f98a9d70-deliverable.txt",
  );

  const operatorSecret = readEnvVar("OCCA_OPERATOR_SECRET_KEY");
  if (!operatorSecret) throw new Error("OCCA_OPERATOR_SECRET_KEY not found");
  const operator = Keypair.fromSecretKey(bs58.decode(operatorSecret.trim()));

  const connection = new Connection(
    "https://api.devnet.solana.com",
    "confirmed",
  );
  const provider = new anchor.AnchorProvider(
    connection,
    new anchor.Wallet(operator),
    { commitment: "confirmed" },
  );
  anchor.setProvider(provider);
  const registry = new anchor.Program(
    JSON.parse(readFileSync("target/idl/registry.json", "utf-8")),
    provider,
  );

  // task_id = sha256(companyId:taskId) — must match the server engine.
  const taskIdBytes = sha256(Buffer.from(`${COMPANY_ID}:${TASK_ID}`, "utf8"));
  const contentHash = sha256(readFileSync(deliverableFile));

  const [anchorOpsPda] = PublicKey.findProgramAddressSync(
    [
      Buffer.from("operations"),
      companyPda.toBuffer(),
      Buffer.from([OPERATIONS_KIND_ANCHOR]),
    ],
    TREASURY_PROGRAM_ID,
  );
  const [traceAnchorPda] = PublicKey.findProgramAddressSync(
    [Buffer.from("trace"), taskIdBytes],
    REGISTRY_PROGRAM_ID,
  );

  console.log(`Operator      : ${operator.publicKey.toBase58()}`);
  console.log(`Balance       : ${(await connection.getBalance(operator.publicKey)) / 1e9} SOL`);
  console.log(`Company        : ${companyPda.toBase58()}`);
  console.log(`Deployment     : ${deploymentPda.toBase58()}`);
  console.log(`Anchor ops     : ${anchorOpsPda.toBase58()}`);
  console.log(`Trace anchor   : ${traceAnchorPda.toBase58()}`);
  console.log(`content_hash   : ${contentHash.toString("hex")}`);
  console.log(`evidence_hash  : ${evidenceHash.toString("hex")}`);
  console.log(`score / rubric : ${qualityScore} / v${rubricVersion}\n`);

  const existing = await connection.getAccountInfo(traceAnchorPda);
  if (existing) {
    console.log("Already anchored — TraceAnchor PDA exists. Done.");
    return;
  }

  console.log("Sending commit_trace…");
  const sig = await registry.methods
    .commitTrace(
      Array.from(taskIdBytes),
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
      anchorSigner: operator.publicKey,
      operations: anchorOpsPda,
      traceAnchor: traceAnchorPda,
      payer: operator.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log(`\n✓ Anchored.`);
  console.log(`Tx: https://explorer.solana.com/tx/${sig}?cluster=devnet`);
}

main().catch((e) => {
  console.error("\n✗ anchor failed:");
  console.error(e);
  process.exit(1);
});
