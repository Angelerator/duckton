import { NextResponse, type NextRequest } from "next/server";

// The same Worker serves the marketing landing (duckton.com) and the console
// (console.duckton.com). The landing lives at "/", so on the console subdomain
// we send the root to the console home instead of showing the landing again.
export function middleware(req: NextRequest) {
  const host = req.headers.get("host") ?? "";
  if (host.startsWith("console.") && req.nextUrl.pathname === "/") {
    const url = req.nextUrl.clone();
    url.pathname = "/overview";
    return NextResponse.redirect(url);
  }
  return NextResponse.next();
}

// Only run on the root path; every other route is served as-is.
export const config = {
  matcher: "/",
};
