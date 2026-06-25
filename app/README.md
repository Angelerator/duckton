# Duckton Node (desktop)

A cross-platform desktop app that runs your machine as a **node** on the Duckton
peer-to-peer DuckDB compute grid. Donate a slice of RAM/CPU, serve verified
queries over QUIC, and optionally **earn — settled on TON** (testnet or mainnet).
No central broker.

The UI mirrors the [duckton.com](https://duckton.com) homepage: dark `#0a0a0b`
canvas, the `#FFD400` accent, Geist typography — built with **SvelteKit +
Tailwind v4** and shadcn-style components.

## Architecture

```
app/
├─ src/                    SvelteKit (SPA, static adapter) frontend
│  ├─ routes/              Overview · Configuration · Payments · Logs
│  └─ lib/                 ipc.ts (typed Tauri commands), UI components, state
└─ src-tauri/              Rust backend (Tauri v2)
   └─ src/
      ├─ node_manager.rs   embeds `p2p-node` as a host (bundled DuckDB engine,
      │                    libp2p discovery), lifecycle + on-chain stake
      ├─ config_store.rs   GridConfig ⇄ config.json/p2p.toml + 0600 secret files
      ├─ commands.rs       Tauri commands the UI calls
      └─ dto.rs            serde DTOs across the JS↔Rust boundary
```

The backend embeds the real grid core (`p2p-node` with the `duckdb-engine` and
`discovery-libp2p` features, plus `p2p-settlement` with `ton-live`), reusing the
same host bring-up the `console-server` demo uses. It is a **standalone Cargo
package** (excluded from the workspace) so it never affects the lean
extension/registry builds.

### Where state lives

Config + secrets are stored in the OS app-config dir (`com.duckton.node`):

- `config.json` — the canonical node config (a full `GridConfig`).
- `p2p.toml` — a best-effort mirror reusable by the CLI / `duckton` extension.
- `node.key` — the stable Ed25519 node identity (`0600`).
- `secrets/<network>.mnemonic`, `secrets/<network>.api_key` — wallet/RPC secrets
  written as `0600` files; only their **paths** are referenced from the config.
  Mnemonics are never stored in the config or the webview.

## Develop

Prerequisites: Rust (stable), Node 20+, and the
[Tauri v2 system prerequisites](https://v2.tauri.app/start/prerequisites/) for
your OS (on Linux: `webkit2gtk-4.1`, `libappindicator3`, `librsvg2`, `patchelf`).

```bash
cd app
npm install
npm run tauri dev     # hot-reloading dev build
```

## Build installers locally

```bash
cd app
npm run tauri build   # produces a bundle for the current OS under src-tauri/target/release/bundle
```

## Release (all OSes, via GitHub Actions)

Push a tag like `app-v0.1.0` (or run the **release-app** workflow manually). The
[`.github/workflows/release-app.yml`](../.github/workflows/release-app.yml)
matrix builds and uploads installers to a **draft GitHub Release**:

| OS      | Artifacts                          | Signing                          |
| ------- | ---------------------------------- | -------------------------------- |
| macOS   | `.dmg`, `.app` (universal)         | Developer ID + notarization      |
| Windows | `.msi`, `.exe` (NSIS)              | Authenticode                     |
| Linux   | `.deb`, `.AppImage`, `.rpm` (x64)  | unsigned (standard for Linux)    |

### Required repository secrets

**macOS** (Developer ID Application cert + notarization):

| Secret                        | Description                                             |
| ----------------------------- | ------------------------------------------------------- |
| `APPLE_CERTIFICATE`           | base64 of the `.p12` Developer ID Application cert      |
| `APPLE_CERTIFICATE_PASSWORD`  | password for the `.p12`                                 |
| `APPLE_SIGNING_IDENTITY`      | e.g. `Developer ID Application: Acme (TEAMID)`          |
| `APPLE_ID`                    | Apple ID email used for notarization                    |
| `APPLE_PASSWORD`              | an app-specific password for that Apple ID              |
| `APPLE_TEAM_ID`               | your Apple Developer Team ID                            |

**Windows** (Authenticode):

| Secret                          | Description                                  |
| ------------------------------- | -------------------------------------------- |
| `WINDOWS_CERTIFICATE`           | base64 of the code-signing `.pfx`            |
| `WINDOWS_CERTIFICATE_PASSWORD`  | password for the `.pfx`                      |

If a platform's secrets are absent the workflow still builds, just **unsigned**
(users will see Gatekeeper / SmartScreen warnings).

## Notes / current limitations

- The app introduces the machine **only as a node** (host): it configures and
  serves the grid and manages payments. It does not ship a requester query
  console.
- Auto-discovery on the public swarm depends on the core's libp2p advertisement;
  on a fresh build the node serves on its QUIC port and joins via the configured
  bootstrap seed (`seed.duckton.com:9494` by default). Set an
  **advertised address** under Configuration if you are behind NAT.
