// Same-origin proxy for the real-network feed.
//
// The browser fetches this (on duckton.com / console.duckton.com, served by the
// Cloudflare Worker) instead of hitting the VM's live.duckton.com directly —
// some corporate networks block raw cloud-IP domains with a 403, but allow
// Cloudflare-fronted origins. The Worker fetches the VM server-side (from
// Cloudflare's network, which reaches it fine) and relays the JSON.

const UPSTREAM = "https://live.duckton.com/api/network";

export async function GET(): Promise<Response> {
  try {
    const r = await fetch(UPSTREAM, { cache: "no-store" });
    if (!r.ok) {
      return new Response(JSON.stringify({ error: "upstream", status: r.status }), {
        status: 502,
        headers: { "content-type": "application/json" },
      });
    }
    const body = await r.text();
    return new Response(body, {
      status: 200,
      headers: {
        "content-type": "application/json",
        // Edge-cache briefly so many viewers don't each hit the VM.
        "cache-control": "public, s-maxage=5, stale-while-revalidate=30",
      },
    });
  } catch {
    return new Response(JSON.stringify({ error: "unreachable" }), {
      status: 502,
      headers: { "content-type": "application/json" },
    });
  }
}
