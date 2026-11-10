// Demo server for the two-way USDC netting escrow on Stellar testnet.
//
// Holds four throwaway testnet wallets (two in Canada, two in India) so you can
// act as any of them from the browser, plus the settler key that submits the
// matches. Every few seconds it:
//   1. reads all orders from the contract,
//   2. asks the Python netting engine which opposite-direction orders to pair,
//   3. submits those pairs with settle_batch (the contract re-checks each one),
//   4. calls release on any order whose wait has run out.
//
// Custodial by design for the demo: the keys live in app/state.json. A real
// app would have senders sign in their own wallet (e.g. Freighter).
//
// Run: npm run setup (once), then npm start, then open http://localhost:5173
import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";
import {
  Address, BASE_FEE, Contract, Keypair, TransactionBuilder, contract, rpc, scValToNative,
} from "@stellar/stellar-sdk";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "..");
const STATE_FILE = path.join(HERE, "state.json");
const PORT = Number(process.env.PORT || 5173);
const TICK_MS = 6000;
const SAFETY_S = 15; // don't propose a match this close to a deadline: it may not land in time
const U = 10_000_000n;

if (!fs.existsSync(STATE_FILE)) {
  console.error("No app/state.json yet. Run `npm run setup` first.");
  process.exit(1);
}
const S = JSON.parse(fs.readFileSync(STATE_FILE, "utf8"));
const save = () => fs.writeFileSync(STATE_FILE, JSON.stringify(S, null, 2));
const server = new rpc.Server(S.rpcUrl);
const person = (key) => S.people.find((p) => p.key === key);
const explorer = (hash) => `https://stellar.expert/explorer/${S.network}/tx/${hash}`;
const nowS = () => Math.floor(Date.now() / 1000);

// ---- activity log --------------------------------------------------------
const activity = [];
function log(text, hash, kind = "info") {
  activity.unshift({ t: nowS(), text, link: hash ? explorer(hash) : null, kind });
  activity.length = Math.min(activity.length, 80);
  console.log(`[${kind}] ${text}${hash ? " " + hash : ""}`);
}

// ---- contract clients ----------------------------------------------------
// The contract's interface (spec) is fetched from the chain once; fetching it
// takes ~15s on testnet, so every per-person client reuses it.
let specPromise = null;
const clients = new Map();
async function escrowAs(secret) {
  if (!clients.has(secret)) {
    specPromise ??= contract.Client.from({
      contractId: S.escrow, rpcUrl: S.rpcUrl, networkPassphrase: S.passphrase,
    }).then((c) => c.spec);
    const kp = Keypair.fromSecret(secret);
    clients.set(secret, specPromise.then((spec) => new contract.Client(spec, {
      contractId: S.escrow,
      rpcUrl: S.rpcUrl,
      networkPassphrase: S.passphrase,
      publicKey: kp.publicKey(),
      ...contract.basicNodeSigner(kp, S.passphrase),
    })));
  }
  return clients.get(secret);
}

// One write at a time: keeps account sequence numbers simple.
let queue = Promise.resolve();
const serial = (fn) => {
  const p = queue.then(fn, fn);
  queue = p.catch(() => {});
  return p;
};

function unwrap(result, what) {
  if (result && typeof result.isErr === "function") {
    if (result.isErr()) {
      const e = result.unwrapErr();
      throw new Error(`${what}: contract error ${e?.message ?? JSON.stringify(e)}`);
    }
    return result.unwrap();
  }
  return result;
}

async function send(tx, what) {
  unwrap(tx.result, what); // simulation result: fail early with the contract's own error
  const sent = await tx.signAndSend();
  const hash = sent.sendTransactionResponse?.hash ?? sent.getTransactionResponse?.txHash;
  return { value: unwrap(sent.result, what), hash };
}

async function sacBalance(addr) {
  const acct = await server.getAccount(S.settler.address);
  const tx = new TransactionBuilder(acct, { fee: BASE_FEE, networkPassphrase: S.passphrase })
    .addOperation(new Contract(S.usdc).call("balance", new Address(addr).toScVal()))
    .setTimeout(30)
    .build();
  const sim = await server.simulateTransaction(tx);
  if (rpc.Api.isSimulationError(sim)) throw new Error(sim.error);
  return BigInt(scValToNative(sim.result.retval));
}

// ---- snapshot of on-chain state -----------------------------------------
let snapshot = { ready: false };

async function refresh() {
  const reader = await escrowAs(S.settler.secret);
  const orders = await Promise.all(S.orders.map(async (o) => {
    const tx = await reader.get_order({ order_id: BigInt(o.id) });
    const chain = unwrap(tx.result, "get_order");
    return {
      ...o,
      remaining: chain.remaining.toString(),
      status: chain.status.tag,
    };
  }));
  const addrs = [...S.people.map((p) => p.address), S.escrow, S.treasury];
  const bals = await Promise.all(addrs.map(sacBalance));
  snapshot = {
    ready: true,
    at: nowS(),
    network: S.network,
    escrow: S.escrow,
    usdc: S.usdc,
    feeBps: S.feeBps,
    people: S.people.map((p, i) => ({
      key: p.key, name: p.name, region: p.region, address: p.address, balance: bals[i].toString(),
    })),
    escrowBalance: bals[S.people.length].toString(),
    treasuryBalance: bals[S.people.length + 1].toString(),
    orders,
  };
  return snapshot;
}

// ---- matching ------------------------------------------------------------
function runEngine(input) {
  return new Promise((resolve, reject) => {
    const p = spawn("python3", ["-I", path.join(ROOT, "engine", "match_cli.py")]);
    let out = "", err = "";
    p.stdout.on("data", (d) => (out += d));
    p.stderr.on("data", (d) => (err += d));
    p.on("close", (code) => (code === 0 ? resolve(JSON.parse(out)) : reject(new Error(err))));
    p.stdin.end(JSON.stringify(input));
  });
}

async function tick() {
  const snap = await refresh();
  const now = nowS();
  const open = snap.orders.filter((o) => o.status === "Open" && BigInt(o.remaining) > 0n);
  const settler = await escrowAs(S.settler.secret);

  const matchable = open.filter((o) => now <= o.deadline - SAFETY_S);
  if (matchable.length > 1) {
    const { fills } = await runEngine({
      fee_bps: S.feeBps,
      now,
      orders: matchable.map((o) => ({
        id: o.id, from_region: o.from, to_region: o.to, remaining: Number(o.remaining),
        created_at: o.createdAt, deadline: o.deadline, full: o.full,
      })),
    });
    if (fills.length) {
      const tx = await settler.settle_batch({
        fills: fills.map((f) => ({ x_id: BigInt(f.x_id), y_id: BigInt(f.y_id), amount: BigInt(f.amount) })),
      });
      const { hash } = await serial(() => send(tx, "settle_batch"));
      for (const f of fills) {
        for (const id of [f.x_id, f.y_id]) {
          const o = S.orders.find((x) => x.id === id);
          o.matched = (BigInt(o.matched || 0) + BigInt(f.amount)).toString();
        }
        const x = S.orders.find((o) => o.id === f.x_id), y = S.orders.find((o) => o.id === f.y_id);
        log(`Matched ${usd(f.amount)} USDC: #${x.id} ${x.from}→${x.to} with #${y.id} ${y.from}→${y.to}. ` +
            `${person(x.recipient).name} and ${person(y.recipient).name} paid now.`, hash, "match");
      }
      save();
    }
  }

  for (const o of open.filter((o) => now >= o.deadline)) {
    const tx = await settler.release({ order_id: BigInt(o.id) });
    const { value, hash } = await serial(() => send(tx, "release"));
    const to = o.onExpiry === "Refund" ? person(o.owner).name : person(o.recipient).name;
    log(`Wait over for #${o.id}: ${usd(value)} USDC ${o.onExpiry === "Refund" ? "refunded to" : "sent across to"} ${to}.`,
        hash, "release");
  }
  if (open.length) await refresh();
}

let ticking = false;
async function loop() {
  if (ticking) return;
  ticking = true;
  try { await tick(); } catch (e) { console.error(e.stack); log(`Matcher: ${e.message}`, null, "error"); }
  finally { ticking = false; }
}

// ---- actions -------------------------------------------------------------
const usd = (stroops) => {
  const v = BigInt(stroops);
  return `${v / U}.${(v % U).toString().padStart(7, "0").replace(/0+$/, "").padEnd(2, "0")}`;
};
const toStroops = (s) => {
  const m = String(s).trim().match(/^(\d+)(?:\.(\d{1,7}))?$/);
  if (!m) throw new Error("amount must be a number with up to 7 decimals");
  return BigInt(m[1]) * U + BigInt((m[2] || "").padEnd(7, "0"));
};

async function sendMoney({ from, to, amount, waitSeconds, onExpiry, full }) {
  const sender = person(from), recipient = person(to);
  if (!sender || !recipient) throw new Error("unknown person");
  if (sender.region === recipient.region) throw new Error("pick a recipient in the other country");
  const stroops = toStroops(amount);
  if (stroops <= 0n) throw new Error("amount must be positive");
  const wait = Math.max(30, Math.min(7 * 86400, Number(waitSeconds) || 0)); // "instant" still needs ~30s to land
  const createdAt = nowS();
  const deadline = createdAt + wait;
  const kind = onExpiry === "Refund" ? "Refund" : "SendRemainder";

  const c = await escrowAs(sender.secret);
  const tx = await c.deposit({
    owner: sender.address, recipient: recipient.address,
    from_region: sender.region, to_region: recipient.region,
    amount: stroops, deadline: BigInt(deadline), full: Boolean(full),
    on_expiry: { tag: kind, values: undefined },
  });
  const { value, hash } = await serial(() => send(tx, "deposit"));
  const id = value.toString();
  S.orders.push({
    id, owner: from, recipient: to, from: sender.region, to: recipient.region,
    amount: stroops.toString(), createdAt, deadline, onExpiry: kind, full: Boolean(full), matched: "0", hash,
  });
  save();
  log(`${sender.name} locked ${usd(stroops)} USDC for ${recipient.name} (${sender.region}→${recipient.region}), ` +
      `waiting up to ${Math.round(wait / 60)} min.`, hash, "deposit");
  setTimeout(loop, 500);
  return { id, hash };
}

async function cancelOrder({ id }) {
  const o = S.orders.find((x) => x.id === String(id));
  if (!o) throw new Error("unknown order");
  const c = await escrowAs(person(o.owner).secret);
  const tx = await c.cancel({ order_id: BigInt(o.id) });
  const { value, hash } = await serial(() => send(tx, "cancel"));
  log(`${person(o.owner).name} cancelled #${o.id}; ${usd(value)} USDC refunded.`, hash, "release");
  await refresh();
  return { refunded: value.toString(), hash };
}

// ---- http ----------------------------------------------------------------
function body(req) {
  return new Promise((resolve, reject) => {
    let b = "";
    req.on("data", (d) => { b += d; if (b.length > 1e5) req.destroy(); });
    req.on("end", () => { try { resolve(b ? JSON.parse(b) : {}); } catch (e) { reject(e); } });
  });
}
function json(res, code, data) {
  res.writeHead(code, { "content-type": "application/json" });
  res.end(JSON.stringify(data));
}

http.createServer(async (req, res) => {
  try {
    if (req.method === "GET" && (req.url === "/" || req.url === "/index.html")) {
      res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
      return res.end(fs.readFileSync(path.join(HERE, "public", "index.html")));
    }
    if (req.method === "GET" && req.url === "/api/state") {
      return json(res, 200, { ...snapshot, activity, now: nowS() });
    }
    if (req.method === "POST" && req.url === "/api/send") return json(res, 200, await sendMoney(await body(req)));
    if (req.method === "POST" && req.url === "/api/cancel") return json(res, 200, await cancelOrder(await body(req)));
    if (req.method === "POST" && req.url === "/api/match") { await loop(); return json(res, 200, { ok: true }); }
    json(res, 404, { error: "not found" });
  } catch (e) {
    console.error(`${req.method} ${req.url}:`, e.stack);
    log(e.message, null, "error");
    json(res, 400, { error: e.message });
  }
}).listen(PORT, "127.0.0.1", async () => {
  console.log(`Velocity demo on http://localhost:${PORT}  (escrow ${S.escrow})`);
  await loop();
  setInterval(loop, TICK_MS);
});
