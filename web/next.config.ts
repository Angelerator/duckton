import type { NextConfig } from "next";

const nextConfig: NextConfig = {
  // Wallets fetch the TonConnect manifest at the canonical `.json` URL, but the
  // route handler lives at `/tonconnect-manifest` (a folder named `*.json`
  // breaks the OpenNext/Cloudflare Workers bundler). Map the public URL here.
  async rewrites() {
    return {
      beforeFiles: [
        { source: "/tonconnect-manifest.json", destination: "/tonconnect-manifest" },
      ],
      afterFiles: [],
      fallback: [],
    };
  },
};

export default nextConfig;

// Cloudflare Workers (OpenNext) local dev integration. Lets `next dev` use the
// same bindings/env as the deployed Worker. No-op in production builds.
import { initOpenNextCloudflareForDev } from "@opennextjs/cloudflare";
initOpenNextCloudflareForDev();
