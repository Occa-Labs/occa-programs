/**
 * Read test: fetch an AgentIdentity's TraceAnchors via memcmp(agent) and
 * compute reputation — validates the offset (73) + decode layout used by
 * the server's reputation-lookup against REAL on-chain data.
 *
 * Targets the identity created by the commit-trace smoke test. Override
 * with AGENT_IDENTITY_PDA env to point at another identity.
 *
 * Run: `npm run read-reputation`
 */

import * as anchor from "@coral-xyz/anchor";
import { Connection, Keypair, PublicKey } from "@solana/web3.js";
import { readFileSync } from "fs";
import { homedir } from "os";
import { join } from "path";

const REGISTRY_PROGRAM_ID = new PublicKey(
  "occaTHMv5eYG5aZ85jimxTvHkBfsDCvndXC6J2k8kxr",
);

// disc(8) + version(1) + task_id(32) + company(32) = 73
const TRACE_AGENT_OFFSET = 73;

// Default = identity from the commit-trace smoke run.
const AGENT = new PublicKey(
  process.env.AGENT_IDENTITY_PDA ??
    "D5ttqJNzuu6GmxyK97wmZm2Pf2QXrZjxwsudE5uiNntw",
);

function loadKeypair(path: string): Keypair {
  return Keypair.fromSecretKey(
    Uint8Array.from(JSON.parse(readFileSync(path, "utf-8"))),
  );
}
function loadIdl(path: string): anchor.Idl {
  return JSON.parse(readFileSync(path, "utf-8"));
}

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
  const registry = new anchor.Program(
    loadIdl("target/idl/registry.json"),
    provider,
  );

  console.log(`Agent identity: ${AGENT.toBase58()}\n`);

  const accts = await (registry.account as any).traceAnchorAccount.all([
    { memcmp: { offset: TRACE_AGENT_OFFSET, bytes: AGENT.toBase58() } },
  ]);

  console.log(`Traces found: ${accts.length}`);
  if (accts.length === 0) {
    console.log("(no anchored traces for this identity)");
    return;
  }

  const traces = accts
    .map((a: any) => ({
      pda: a.publicKey.toBase58(),
      resultUri: a.account.resultUri as string,
      verdict: a.account.verdict as number,
      qualityScore: a.account.qualityScore as number,
      rubricVersion: a.account.rubricVersion as number,
      completedAt: Number(a.account.completedAt),
    }))
    .sort((x: any, y: any) => x.completedAt - y.completedAt);

  for (const t of traces) {
    console.log(
      `  • ${t.pda.slice(0, 8)}…  score=${t.qualityScore} rubric=v${t.rubricVersion} verdict=${t.verdict} uri="${t.resultUri || "(private)"}"`,
    );
  }

  const total = traces.length;
  const avg =
    Math.round(
      (traces.reduce((s: number, t: any) => s + t.qualityScore, 0) / total) *
        100,
    ) / 100;
  console.log(`\nReputation:`);
  console.log(`  total anchored : ${total}`);
  console.log(`  avg quality    : ${avg}`);
  console.log(`  last quality   : ${traces[total - 1].qualityScore}`);
  console.log(
    `  first anchored : ${new Date(traces[0].completedAt * 1000).toISOString()}`,
  );
  console.log(
    `  last anchored  : ${new Date(traces[total - 1].completedAt * 1000).toISOString()}`,
  );

  console.log("\n✓ reputation read OK — offset + decode validated on-chain.");
}

main().catch((err) => {
  console.error("\n✗ reputation read FAILED:");
  console.error(err);
  process.exit(1);
});
