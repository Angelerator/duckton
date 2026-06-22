# Hosting & joining a network

Running queries needs no setup. **Becoming a host** (donating compute) and
**joining a specific swarm** are both opt-in.

## Become a host

```sql
CALL p2p_share(
  memory       => '4GB',        -- RAM you donate to others' jobs
  threads      => 2,            -- worker threads
  max_jobs     => 3,            -- concurrent jobs you'll admit
  data_classes => ['public']    -- which sensitivity classes you serve
);
```

Your node now answers `Offer`s under an **admission policy** (budget, data class,
membership, anti-abuse rate limits). A job is only executed after your node bids
`Accept` — so admission gates can never be bypassed.

Pause and resume serving (graceful drain — in-flight jobs finish, new offers are
declined):

```sql
CALL p2p_pause();
CALL p2p_resume();
```

## Join a swarm

```sql
CALL p2p_join(bootstrap => [
  'quic://seed1.example:9494',
  'quic://seed2.example:9494'
]);
```

Without bootstrap seeds a node is **local-first** (runs your own queries; falls
back to local when no grid is reachable). Joining points your fan-out at a real
network.

## Inspect the network

```sql
SELECT node_id, free_mem, attestation_level, trust_score FROM p2p_peers();
SELECT * FROM p2p_status();   -- node/wallet/network/economics state
```

## Admission & abuse resistance

Hosts protect themselves independently:

- **Data-class policy** — a host serves only the classes it advertised.
- **Anti-abuse** — optional deny-list, free-job rate limits, and a cost gate.
- **Membership/scoping** — network, group, and region constraints.
- **Per-node deny-list** — `p2p_block` / `p2p_unblock` / `p2p_blocklist`.

```sql
CALL p2p_block(node_id => 'b3:...', reason => 'spam');
SELECT * FROM p2p_blocklist();
CALL p2p_unblock(node_id => 'b3:...');
```

## Closed/enterprise networks

For a fully private company grid (mutual allowlist, default-deny roster, group
tokens), see [Private / enterprise mode](../PRIVATE_MODE.md).
