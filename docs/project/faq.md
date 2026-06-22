# FAQ

??? question "Do I need a blockchain, wallet, or any setup to use it?"
    No. Public queries are **free and fully off-chain**, and with no peers
    configured `p2p_query` runs locally in a locked-down DuckDB. You only touch
    wallets/economics if you opt into **paid** queries.

??? question "How is this different from DuckDB's Quack / a normal client-server DB?"
    Quack makes DuckDB a single client-server database. Duckton is a
    **decentralized, many-host grid** with a built-in **trust model**: results are
    cross-checked by independent hosts to a **verifiable quorum**, so you can trust
    a result you didn't compute yourself. There's no central broker in the data
    path.

??? question "Can I send a query to exactly one node?"
    Yes — fully configurable. Use `prefer => 'local'` to run it on yourself,
    `replicas => 1, quorum => 1` to dispatch to a single (best-scored) node, or
    `nodes => ['b3:...']` to pin it to a **specific** node id. See
    [Node targeting](../guides/node-targeting.md).

??? question "Is my data private from the host that runs my query?"
    Transport, at-rest, and integrity are protected everywhere. **Confidentiality
    from the host's RAM requires TEE hardware**, so sensitive data is routed only
    to verified attested hosts. See [Security](security.md).

??? question "Which DuckDB version do I need?"
    The one `duckton` is built against (currently **v1.5.4**) — the
    loadable-extension ABI requires an exact match. Upgrade DuckDB if `INSTALL`
    reports a mismatch.

??? question "What happens if a host returns a wrong result, or goes silent?"
    A wrong result fails the **quorum** (and risks a canary catch + reputation
    loss). A silent/slow host is hedged around: the coordinator races `k` workers
    and re-dispatches to a fresh set, so it costs latency, not correctness.

??? question "Can I run a fully private company grid (no public peers)?"
    Yes — [private / enterprise mode](../PRIVATE_MODE.md): mutual allowlist
    pinning, a default-deny roster, and group tokens for a closed pool.

??? question "How are hosts paid, and who pays the platform?"
    A paid job locks the requester's max bid in a per-job escrow; on settle the
    winner gets its base, the treasury a platform fee, and each agreeing verifier a
    commission — all **enforced on-chain** (TON). The remainder refunds to the
    requester. See [Paid queries](../guides/paid-queries.md).

??? question "What's real vs. mocked today?"
    Phases 0–4 are implemented and tested. The mock TEE attestor, local-fake object
    storage, and the `noop` default sandbox are clearly flagged. The
    [Architecture](../ARCHITECTURE.md) "implementation status & deviations" section
    is precise about it.

??? question "Where do I report bugs or contribute?"
    On the [repository](https://github.com/Angelerator/duckton); see
    [Contributing](contributing.md).
