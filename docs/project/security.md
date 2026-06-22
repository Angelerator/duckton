# Security

Duckton is built to be **honest about what it can and cannot protect**. This page
summarizes the model; the full reasoning is in the [Architecture deep
dive](../ARCHITECTURE.md) (§9 Security model) and the threat model in [How it
works](../HOW_IT_WORKS.md) (§5).

## What is protected on any machine

- **Transport** — QUIC with **TLS 1.3, mutual authentication**; the peer
  certificate is **pinned to its Ed25519 node identity**. Nothing is readable on
  the wire.
- **At rest** — data lives in cloud object storage, encrypted (Parquet Modular
  Encryption). Hosts are pure compute with per-job **scoped, short-lived**
  credentials.
- **Integrity** — results are reduced to canonical BLAKE3 hashes and accepted only
  on **quorum agreement**, with randomized **canary audits**. A single broken or
  dishonest host can't slip a wrong answer past the quorum.
- **Identity / Sybil** — Ed25519 identities, proof-of-work minting + vouching, and
  signed receipts. Stake/Sybil gates are **fail-closed** (only a cryptographically
  attributable id can satisfy them).

## The honest boundary

**Confidentiality from a malicious host operator's RAM is only achievable on
confidential-computing hardware (TEEs).** A commodity machine that executes your
query can, in principle, read its inputs in memory. Therefore:

- Sensitive data is routed only to hosts that present a **verified attested tier**.
- A bid claiming a tier `> L0` is honored **only** if its evidence verifies against
  a wired verifier (trusted-authority signature over an allowlisted measurement +
  the offer nonce, with the level bound into the signed evidence). Absent or
  invalid evidence is treated as **L0** — the gate **fails closed**.
- **Production nodes ship no attestor and no verifier by default**: real L1/L2
  attestation needs TPM/TEE hardware that is not yet shipped, so a production node
  emits L0 and an L2 (sensitive) policy admits *nobody* until that hardware lands.
  The demo exercises the gate honestly with a software mock attestor + real
  verification.

## Admission & isolation

- A **`Dispatch` is only executed for a job the host bid `Accept` on**, from the
  same authenticated peer — so the offer-phase admission gates (serving block,
  membership/roster, anti-abuse, data class) can never be skipped.
- Untrusted SQL runs under a **DuckDB configuration lockdown** (no external access,
  filesystem disabled or scoped, configuration locked). Real **OS-level isolation**
  (rlimits / Seatbelt / cgroups / Job Object) is opt-in via
  `[sandbox].process_per_job` + the `p2p-job-exec` child binary; the default
  backend is `noop` (in-process under the lockdown).
- On-disk spill is capped and (when a crypto provider is loaded) encrypted at rest.

## On-chain safety

The TON settlement enforces the economic split on-chain: a settle is rejected for
a wrong platform fee (`FEE_MISMATCH`), a shaved verifier commission
(`COMMISSION_MISMATCH`), or an under-funded escrow (`PAYOUT_EXCEEDS_ESCROW`). The
treasury is bound to the authoritative `GlobalParams.fee_recipient`; a local
mismatch is refused. Governance code upgrades (params, stake vault, record anchor)
are **timelocked** and hash-committed.

## Secrets

Wallet mnemonics and API keys are never written to the config file or echoed.
Inline secrets are moved to a `0600` file outside the repo and only the path is
persisted; reads redact them. Prefer `*_file` references.

## Reporting a vulnerability

Please report security issues privately to the maintainers via the
[repository](https://github.com/Angelerator/duckton) (a security advisory or a
direct contact), rather than opening a public issue. Include reproduction steps
and the affected component.
