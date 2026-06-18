# Private / Enterprise mode — running a fully closed company grid

`duckdb-p2p` is zero-config and public by default. **Private mode** turns a set
of nodes into a *closed* grid: outsiders cannot connect or impersonate a member,
and members cannot serve or be served by outsiders.

Flip one switch — `[security].mode = "private"` (env `P2P_SECURITY_MODE=private`)
— and the node **fails to start** unless the closure invariants below are all
satisfied. Nothing is silently relaxed; a misconfigured private node never leaks.

## What private mode enforces

Always-on (both modes — it is a correctness/security fix, not a preset):

- **Application↔transport identity binding.** An `Offer`'s self-claimed
  `requester_id` must equal the authenticated mTLS peer (`conn.peer_node_id()`).
  The honest requester dials the host directly and stamps its own node id, so
  they always match; a mismatch is impersonation and is rejected. This also makes
  a *stolen* group token useless: the token is bound to its holder's node id, and
  the holder must be the authenticated peer.

Activated by `mode = "private"`:

- **Allowlist mTLS (the perimeter).** Requires `identity.pinning_mode =
  "allowlist"` with a **non-empty** `identity.allowlist`. Outsiders are refused
  during the TLS handshake — they can't even open a connection.
- **Cryptographic group membership.** Requires `membership.group_enforcement =
  "token"`: a host admits a requester only if it presents a `group_proof`
  (a `CapabilityToken`) that verifies against a configured `group_issuers` key
  for one of the host's groups **and** is bound to the requester's id. Soft
  declared labels are rejected.
- **Require-grouped-host.** An *ungrouped* host serves everyone — forbidden in a
  closed grid. A private host with no `membership.groups` refuses every offer.
- **Default-deny requester roster.** A host serves only requesters on its roster
  (the `identity.allowlist`), not merely those absent from a reactive blocklist.
- **Non-default network.** Requires an explicit, non-`"default"` `membership.networks`
  name (e.g. `["acme-internal"]`).
- **Fail-closed discovery.** Candidates whose network/group labels are *unknown*
  or don't match are **dropped** (in public mode they're kept on the soft
  assumption the host re-checks at admission).

## Company recipe

### 1. Generate a stable node key per machine

Each node needs a persistent identity (PKCS#8 Ed25519 PEM):

```bash
openssl genpkey -algorithm ed25519 -out /etc/duckdb-p2p/node.key
chmod 600 /etc/duckdb-p2p/node.key
```

Point the node at it with `identity.key_path` (or `P2P_IDENTITY_KEY_PATH`).
Without a key the node generates an *ephemeral* identity whose node id changes on
every restart — fine for a public node, useless for an allowlist.

### 2. Collect the `b3:` roster

Each node's id is the BLAKE3 of its public key, formatted `b3:<64 hex>`. Read it
from any started node:

```sql
CALL p2p_share();   -- the "node_id" row is this node's b3: id
```

Collect every member's `b3:` id into one roster list.

### 3. Mint group tokens (one issuer per company / department)

Pick an **issuer** keypair (kept by ops, offline). Mint a group-membership
`CapabilityToken` for each member, bound to that member's public key, carrying a
`Group("finance")` caveat and an `ExpiresAt` (always time-box). Distribute the
issuer's **public** key (hex) to every node as a `group_issuers` entry, and give
each member its own token as `membership.group_token`.

### 4. Configure each node

```toml
[security]
mode = "private"

[identity]
key_path     = "/etc/duckdb-p2p/node.key"
pinning_mode = "allowlist"
allowlist    = [                       # the company roster (every member b3: id)
  "b3:1111...", "b3:2222...", "b3:3333...",
]

[membership]
networks          = ["acme-internal"]  # explicit, non-"default"
groups            = ["finance"]        # the group(s) THIS node serves
group_enforcement = "token"
group_token       = "<this node's JSON CapabilityToken>"   # presented as a requester

[membership.group_issuers]
finance = "<issuer ed25519 public key, hex>"

[discovery]
bootstrap = ["/ip4/10.0.0.10/tcp/9595/p2p/12D3Koo..."]     # a PRIVATE bootstrap/relay you run
```

Equivalent env overrides (highest precedence): `P2P_SECURITY_MODE`,
`P2P_IDENTITY_PINNING_MODE`, `P2P_IDENTITY_ALLOWLIST` (comma list),
`P2P_IDENTITY_KEY_PATH`, `P2P_MEMBERSHIP_NETWORKS`, `P2P_MEMBERSHIP_GROUPS`,
`P2P_MEMBERSHIP_GROUP_ENFORCEMENT`, `P2P_MEMBERSHIP_GROUP_TOKEN`,
`P2P_MEMBERSHIP_GROUP_ISSUERS` (`finance=<hex>,ops=<hex>`).

### 5. Scope queries

A requester stamps its `[membership]` network/group claims into every offer
automatically. Scoped, in-network queries match only company hosts; the
fail-closed discovery + token group check drop everything else.

### Perimeter note (still required)

Private mode closes the **application** grid: identity, membership, and serving.
It does not replace your **network perimeter**. Run the data-plane QUIC port and
the discovery bootstrap/relay inside your VPN / behind a firewall so the closed
grid isn't even reachable from the public internet. The two layers are
complementary: the firewall stops packets, allowlist mTLS stops connections, and
the group tokens + roster stop serving.

## Honest limitations

- **Node-id roster, not a company CA/PKI.** Membership is a flat list of `b3:`
  node ids plus per-group issuer keys. There is no certificate authority,
  hierarchy, or automatic enrolment — adding/removing a member means editing the
  `allowlist` (and minting/expiring a token) on the nodes.
- **Key rotation & revocation are ops.** Rotating a node key or revoking a member
  is a config change (update the allowlist / let the token expire / re-issue),
  not an online protocol. Keep token `ExpiresAt` windows short.
- **Perimeter still required.** See above — private mode is not a substitute for
  a VPN/firewall.
- **The identity binding does not add a CA either** — it binds the offer to the
  pinned self-signed cert's node id; trust in that id still comes from the
  allowlist.
