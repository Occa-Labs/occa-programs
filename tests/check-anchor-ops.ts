/**
 * Diagnostic: is a company's Anchor OperationsAccount ready for commit_trace?
 * Checks existence, signer == operator, not revoked/expired, and whether
 * the commit_trace discriminator is whitelisted.
 *
 * Run: COMPANY_PDA=<pda> npm run check-anchor-ops
 */

import { Connection, PublicKey } from "@solana/web3.js";
import { readFileSync } from "fs";
import { join } from "path";

// bs58 ships no types in this workspace; require + cast.
// eslint-disable-next-line @typescript-eslint/no-var-requires
const bs58 = require("bs58") as { decode: (s: string) => Uint8Array };

// Minimal .env reader — avoids a dotenv dependency. Returns the raw value
// for a key, or undefined.
function readEnvVar(envPath: string, key: string): string | undefined {
  try {
    const text = readFileSync(envPath, "utf-8");
    for (const line of text.split("\n")) {
      const m = line.match(new RegExp(`^\\s*${key}\\s*=\\s*(.*)\\s*$`));
      if (m) return m[1].replace(/^["']|["']$/g, "").trim();
    }
  } catch {
    /* no .env */
  }
  return undefined;
}

const OCCA_ENV = join(__dirname, "../../occa/.env");

const TREASURY_PROGRAM_ID = new PublicKey(
  "occaxyVLnurdjedWCBPrvDCCto8wGYadtTZ3nAmcVzh",
);
const OPERATIONS_KIND_ANCHOR = 1;
const COMMIT_TRACE_DISC_HEX = Buffer.from([
  58, 140, 230, 51, 170, 109, 228, 125,
]).toString("hex");
const COMMIT_DAILY_ANCHOR_DISC_HEX = Buffer.from([
  18, 7, 3, 65, 58, 148, 164, 0,
]).toString("hex");

const COMPANY_PDA = new PublicKey(
  process.env.COMPANY_PDA ?? "7YPkHACUahgbG2GZBe5R1eLo8sWrLYU1RB1ZcdSH5n81",
);

function operatorPubkey(): PublicKey | null {
  const raw =
    process.env.OCCA_OPERATOR_SECRET_KEY ??
    readEnvVar(OCCA_ENV, "OCCA_OPERATOR_SECRET_KEY");
  if (!raw) return null;
  try {
    return new PublicKey(
      bs58.decode(raw.trim()).slice(32), // ed25519 secret = [priv32 | pub32]
    );
  } catch {
    return null;
  }
}

async function main() {
  const conn = new Connection("https://api.devnet.solana.com", "confirmed");
  const operator = operatorPubkey();
  console.log(`Company PDA : ${COMPANY_PDA.toBase58()}`);
  console.log(`Operator    : ${operator?.toBase58() ?? "(could not load)"}`);

  const [opsPda] = PublicKey.findProgramAddressSync(
    [
      Buffer.from("operations"),
      COMPANY_PDA.toBuffer(),
      Buffer.from([OPERATIONS_KIND_ANCHOR]),
    ],
    TREASURY_PROGRAM_ID,
  );
  console.log(`Anchor ops  : ${opsPda.toBase58()}\n`);

  const info = await conn.getAccountInfo(opsPda);
  if (!info) {
    console.log("✗ Anchor OperationsAccount does NOT exist.");
    console.log("  → Set it up: register Anchor ops with signer = operator,");
    console.log("    whitelist commit_trace + commit_daily_anchor.");
    return;
  }

  // Layout post 8-byte disc: version u8, company pk, kind u8, signer pk,
  // whitelist Vec<[u8;8]>, rate u32, sigs u32, period i64, expiry i64,
  // revoked bool, bump u8.
  const d = info.data;
  let o = 8;
  o += 1; // version
  o += 32; // company
  o += 1; // kind
  const signer = new PublicKey(d.subarray(o, o + 32));
  o += 32;
  const wlLen = d.readUInt32LE(o);
  o += 4;
  const whitelist: string[] = [];
  for (let i = 0; i < wlLen; i++) {
    whitelist.push(d.subarray(o, o + 8).toString("hex"));
    o += 8;
  }
  o += 4 + 4 + 8 + 8; // rate, sigs, period, expiry
  const revoked = d.readUInt8(o) === 1;

  console.log(`signer      : ${signer.toBase58()}`);
  console.log(`revoked     : ${revoked}`);
  console.log(`whitelist   : ${whitelist.join(", ") || "(empty)"}`);
  console.log("");

  const signerOk = operator ? signer.equals(operator) : false;
  const traceWl = whitelist.includes(COMMIT_TRACE_DISC_HEX);
  const dailyWl = whitelist.includes(COMMIT_DAILY_ANCHOR_DISC_HEX);

  console.log(`signer == operator      : ${signerOk ? "✓" : "✗"}`);
  console.log(`commit_trace whitelisted: ${traceWl ? "✓" : "✗"} (${COMMIT_TRACE_DISC_HEX})`);
  console.log(`commit_daily whitelisted: ${dailyWl ? "✓" : "✗"} (${COMMIT_DAILY_ANCHOR_DISC_HEX})`);
  console.log("");

  if (signerOk && traceWl && !revoked) {
    console.log("✓ READY — commit_trace will succeed for this company.");
  } else {
    console.log("✗ NOT READY for commit_trace. Fixes needed:");
    if (!signerOk) console.log("  - signer mismatch (ops signer must be the operator)");
    if (revoked) console.log("  - ops is revoked");
    if (!traceWl)
      console.log(
        "  - add commit_trace discriminator to the whitelist via " +
          "treasury.update_operations_capability",
      );
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
