// Live on-chain stats for the deployed mainnet GlobalParams contract.
//
// Read straight from TON mainnet via tonapi.io (keyless, dApp-grade) — NOT from
// the baked snapshot — so the homepage can show real, verifiable chain state
// (balance, status, params version, on-chain fee φ / commission κ). Runs on the
// Worker and is edge-cached briefly to stay well within provider limits.

const GP =
  process.env.NEXT_PUBLIC_MAINNET_GLOBALPARAMS?.trim() ||
  "EQCV59kSoDDgmE8cheBNYwl2oYL9h5nkbyCqn99tN-N1w9Gg";

const API = "https://tonapi.io/v2";
const API_KEY = process.env.TONAPI_API_KEY;

function headers(): Record<string, string> {
  return API_KEY ? { Authorization: `Bearer ${API_KEY}` } : {};
}

/** Run a no-arg getter and return its first stack item as a bigint, or null. */
async function getMethodInt(method: string): Promise<bigint | null> {
  try {
    const r = await fetch(`${API}/blockchain/accounts/${GP}/methods/${method}`, {
      headers: headers(),
      cache: "no-store",
    });
    if (!r.ok) return null;
    const j = (await r.json()) as {
      success?: boolean;
      exit_code?: number;
      stack?: { type: string; num?: string }[];
    };
    if (!j.success || j.exit_code !== 0) return null;
    const top = j.stack?.[0];
    if (!top || top.type !== "num" || !top.num) return null;
    return BigInt(top.num);
  } catch {
    return null;
  }
}

async function getAccount(): Promise<{ balanceNano: bigint | null; status: string | null }> {
  try {
    const r = await fetch(`${API}/accounts/${GP}`, { headers: headers(), cache: "no-store" });
    if (!r.ok) return { balanceNano: null, status: null };
    const j = (await r.json()) as { balance?: number | string; status?: string };
    return {
      balanceNano: j.balance != null ? BigInt(j.balance) : null,
      status: j.status ?? null,
    };
  } catch {
    return { balanceNano: null, status: null };
  }
}

export async function GET(): Promise<Response> {
  const [{ balanceNano, status }, version, feeBps, commissionBps] = await Promise.all([
    getAccount(),
    getMethodInt("get_params_version"),
    getMethodInt("get_platform_fee_bps"),
    getMethodInt("get_participation_commission_bps"),
  ]);

  const body = {
    network: "mainnet",
    address: GP,
    explorer: `https://tonviewer.com/${GP}`,
    status, // "active" once deployed
    balanceTon: balanceNano === null ? null : Number(balanceNano) / 1e9,
    paramsVersion: version === null ? null : Number(version),
    platformFeeBps: feeBps === null ? null : Number(feeBps),
    participationBps: commissionBps === null ? null : Number(commissionBps),
    fetchedAt: Date.now(),
  };

  return new Response(JSON.stringify(body), {
    status: 200,
    headers: {
      "content-type": "application/json",
      // Edge-cache for 30s (serve stale up to 5m) so we don't hammer the API.
      "cache-control": "public, s-maxage=30, stale-while-revalidate=300",
    },
  });
}
