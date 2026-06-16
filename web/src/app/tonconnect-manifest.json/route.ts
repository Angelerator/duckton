// Dynamic TonConnect manifest.
//
// A wallet (often on another device) fetches this before showing the connect
// prompt and REJECTS it ("manifest error") unless it is served over HTTPS, is
// reachable from any origin (permissive CORS), is `application/json`, and its
// `url` matches the actual serving origin (the wallet validates the host).
// See https://docs.ton.org/applications/ton-connect/troubleshooting and
// https://github.com/ton-connect/docs/blob/main/requests-responses.md
//
// A static file with a hardcoded `url` can't satisfy the origin-match rule
// across dev / LAN / tunnel / prod, so we derive `url` + `iconUrl` from the
// request origin here (overridable with NEXT_PUBLIC_TONCONNECT_URL).

export const dynamic = "force-dynamic";

const CORS_HEADERS = {
  "Access-Control-Allow-Origin": "*",
  "Access-Control-Allow-Methods": "GET, OPTIONS",
  "Access-Control-Allow-Headers": "*",
} as const;

/** Absolute origin the manifest should advertise, derived from the request. */
function resolveOrigin(request: Request): string {
  const override = process.env.NEXT_PUBLIC_TONCONNECT_URL;
  if (override) return override.replace(/\/+$/, "");

  const h = request.headers;
  const host = h.get("x-forwarded-host") ?? h.get("host") ?? "localhost:3000";
  const isLocal = /^(localhost|127\.|0\.0\.0\.0|\[::1\]|.*\.local)(:|$)/i.test(host);
  // Honor the proxy/tunnel's scheme; fall back to https except for local hosts.
  const proto = h.get("x-forwarded-proto") ?? (isLocal ? "http" : "https");
  return `${proto}://${host}`;
}

export function OPTIONS(): Response {
  return new Response(null, { status: 204, headers: CORS_HEADERS });
}

export function GET(request: Request): Response {
  const origin = resolveOrigin(request);
  const manifest = {
    url: origin,
    name: "Duckton",
    iconUrl: `${origin}/duckton-icon.png`,
  };
  return new Response(JSON.stringify(manifest, null, 2), {
    status: 200,
    headers: {
      "Content-Type": "application/json",
      // The manifest is origin-specific and cheap to recompute; never cache it.
      "Cache-Control": "no-store",
      ...CORS_HEADERS,
    },
  });
}
