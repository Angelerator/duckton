//! Protocol messages exchanged between nodes.
//!
//! Protocol flow (architecture §3 / §11):
//!   Requester --Offer-->  Worker
//!   Worker    --Bid-->    Requester     (accept w/ ETA + attestation + receipts, or reject)
//!   Requester --Dispatch--> Worker      (full SQL + scoped credential, to top-k workers)
//!   Worker    --Commit-->  Requester    (result_hash first, "commit-first")
//!   Worker    --Chunk*-->  Requester    (bulk result stream; winner only)
//!   Requester --Cancel-->  Worker       (RESET losers)
//!
//! All of these are carried in the [`Wire`] tagged envelope so the transport
//! layer can use one uniform framed read/write path.

use serde::{Deserialize, Serialize};

use crate::attestation::Attestation;
use crate::ids::{JobId, NodeId, QueryHash};
use crate::input::InputSnapshot;
use crate::value::ResultSet;
use crate::version::Version;

/// Sensitivity class of the data a query touches (architecture §7.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum DataClass {
    #[default]
    Public,
    Internal,
    Sensitive,
}

/// Verification mode chosen by the requester (architecture §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum VerifyMode {
    /// Return the fastest result; verify hashes in the background.
    Fast,
    /// Wait for `quorum` matching hashes before returning.
    #[default]
    Quorum,
}

/// Step 1: Requester probes a candidate worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Offer {
    pub job_id: JobId,
    pub requester_id: NodeId,
    pub query_hash: QueryHash,
    /// Optional cost hint (estimated rows scanned) so workers can admission-check.
    pub cost_hint_rows: Option<u64>,
    /// Optional pre-flight estimate of the SCANNED input bytes for this job (the
    /// estimator's `scanned_uncompressed_bytes`). A worker uses it to size its
    /// metered `estimated_seconds`/`cap_seconds` bid (bytes ÷ measured throughput).
    /// `None` (default / an older requester) ⇒ no hint: the worker falls back to a
    /// conservative cold-start estimate.
    #[serde(default)]
    pub cost_hint_bytes: Option<u64>,
    pub data_class: DataClass,
    /// Fresh random nonce to bind the exchange and prevent replay.
    pub nonce: u64,
    // --- Request-scoping / routing constraints (additive; `#[serde(default)]` so an
    //     Offer from an older peer parses with no constraints = today's behavior). ---
    /// Logical grid partition this query targets (NOT the TON chain). `None` ⇒ the
    /// requester does not constrain by network (matches any host).
    #[serde(default)]
    pub network: Option<String>,
    /// The requester's claimed group memberships. A host that has groups
    /// configured admits the offer only if `host.groups ∩ this != ∅`; an ungrouped
    /// (public) host ignores it. Empty ⇒ the requester claims no groups.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Regions the requester will accept a host in. Non-empty ⇒ the host's region
    /// must be one of these (fail-closed for a host with no region). Empty ⇒ no
    /// region constraint.
    #[serde(default)]
    pub regions: Vec<String>,
    /// Optional cryptographic group-membership proof (a JSON-encoded
    /// `p2p_trust::CapabilityToken`) presented under `group_enforcement = token`.
    /// `None` under the default soft tier. Kept as an opaque string so the wire
    /// crate stays free of the trust-crate dependency.
    #[serde(default)]
    pub group_proof: Option<String>,
    /// Optional hint of the input-snapshot fingerprint the requester intends to
    /// pin for this job (deterministic-input verification). Lets a worker
    /// early-decline if it already knows it cannot read that exact snapshot.
    /// `None` ⇒ no hint (the authoritative pin is the [`Dispatch::input_snapshot`]).
    #[serde(default)]
    pub input_fingerprint_hint: Option<String>,
}

/// A worker's decision on an [`Offer`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BidDecision {
    Accept,
    Reject { reason: String },
}

/// Step 2: Worker bids on an offer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bid {
    pub job_id: JobId,
    pub worker_id: NodeId,
    pub decision: BidDecision,
    /// Estimated time to completion (ms).
    pub eta_ms: u64,
    /// Price hint (abstract units; 0 for free donation).
    pub price: u64,
    /// Attestation evidence (stubbed to L0 in Phase 0).
    pub attestation: Attestation,
    /// A bundle of recent signed receipts the worker presents as reputation.
    pub recent_receipts: Vec<Receipt>,
    /// Currently-free memory the worker advertises (bytes).
    pub free_mem_bytes: u64,
    /// Currently-free worker threads.
    pub free_threads: u32,
    /// Optional cryptographic region-attestation proof (a JSON-encoded
    /// `p2p_trust::CapabilityToken`) the host presents so a requester running the
    /// attested region tier can verify residency. `None` under the default
    /// declared tier. Opaque string to keep the wire crate trust-free.
    #[serde(default)]
    pub region_proof: Option<String>,
    // --- Time-based (usage) pricing terms (additive; `#[serde(default)]` so a Bid
    //     from an older peer parses with zeros = no metered terms ⇒ the requester
    //     falls back to fixed/estimate pricing — today's behavior). ---
    /// Provider's advertised per-second rate in **nanoton/second**. `0` ⇒ this bid
    /// carries no metered terms (fixed-price / free).
    #[serde(default)]
    pub rate_per_second: u64,
    /// Optional provider per-GiB rate in **nanoton/GiB** of scanned input. `0` ⇒
    /// no byte term (pure time-based).
    #[serde(default)]
    pub rate_per_gb: u64,
    /// The provider's ESTIMATE of the processing time (seconds) for this job, from
    /// the data-size hint ÷ its measured throughput. Drives `cap_seconds`. `0` ⇒
    /// no estimate (older peer / fixed bid).
    #[serde(default)]
    pub estimated_seconds: u64,
    /// The billing-ceiling AND hard execution deadline (seconds):
    /// `cap_seconds = ceil(estimated_seconds × cap_multiplier)`. The job is billed
    /// `rate × min(actual, cap_seconds)` and hard-aborted past `cap_seconds`. `0` ⇒
    /// no cap (older peer / fixed bid).
    #[serde(default)]
    pub cap_seconds: u64,
}

/// A scoped, short-lived storage credential delivered inside a [`Dispatch`]
/// (architecture §9.2). In tests this points at a local fake object store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScopedCredential {
    /// e.g. "s3", "az", "gcs", or "local-fake".
    pub provider: String,
    /// Opaque token (STS session / SAS / downscoped token / local path token).
    pub token: String,
    /// Object prefix the credential is scoped to (read-only).
    pub prefix: String,
    /// Unix-seconds expiry.
    pub expires_at: u64,
}

/// Step 3: Requester dispatches the full job to a chosen worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dispatch {
    pub job_id: JobId,
    /// The full SQL text to execute.
    pub sql: String,
    pub query_hash: QueryHash,
    /// Per-job scoped credential (optional; absent for purely local test data).
    pub credential: Option<ScopedCredential>,
    /// Memory lease for the execution connection (bytes).
    pub memory_limit_bytes: u64,
    /// Thread lease for the execution connection.
    pub threads: u32,
    pub verify_mode: VerifyMode,
    /// Phase 4: a data key sealed to the worker's (enclave) key. `None` outside
    /// the confidential tier.
    pub sealed_key: Option<SealedKey>,
    /// Per-call result-stream parallelism (number of concurrent uni-streams the
    /// winner should use). `None` ⇒ worker uses its configured default.
    #[serde(default)]
    pub result_parallelism: Option<u32>,
    /// Per-call wire compression for the result. `None` ⇒ worker default.
    #[serde(default)]
    pub compression: Option<Compression>,
    /// The pinned, version-identified manifest of the external inputs this job
    /// reads (deterministic-input verification). The worker reads the pinned
    /// versions and echoes [`InputSnapshot::fingerprint`] in its [`ResultCommit`]
    /// so the requester can tell "data changed between replicas" (benign drift)
    /// apart from "node returned a wrong result" (fault). `None` (default / an
    /// older requester) ⇒ no pin: verification falls back to result-hash quorum,
    /// exactly today's behavior.
    #[serde(default)]
    pub input_snapshot: Option<InputSnapshot>,
    /// Hard per-attempt execution deadline (milliseconds) derived from the metered
    /// `cap_seconds` of the worker's own bid: the worker ABORTS exactly at this cap
    /// (no grace window) and reports a resource/job fault (no provider penalty, no
    /// slash). `None` (default / fixed-price / an older requester) ⇒ the worker's
    /// own `job_timeout` governs, exactly today's behavior.
    #[serde(default)]
    pub cap_deadline_ms: Option<u64>,
}

/// A symmetric data key sealed (encrypted) to a worker/enclave public key
/// (architecture §9.3 — attestation-gated key release). Phase 4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SealedKey {
    /// Recipient key fingerprint the blob is sealed to.
    pub recipient: String,
    /// Sealed ciphertext (hex). Opaque to the proto layer.
    pub ciphertext_hex: String,
}

/// A streamed progress / heartbeat update sent by the worker **during** job
/// execution, before the [`ResultCommit`] (architecture §11 resilience). It is
/// both an observability signal (stage / rows / pct, surfaced to a future SQL
/// status view) and — crucially — the **liveness signal**: if the requester
/// sees no `Progress` (nor a `Commit`) within its stall timeout it declares the
/// attempt stalled and re-dispatches to a fresh candidate set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub job_id: JobId,
    pub worker_id: NodeId,
    /// Coarse execution stage (e.g. "executing", "scanning", "finalizing").
    pub stage: String,
    /// Rows processed so far (best-effort; `0` when unknown).
    pub rows_processed: u64,
    /// Percent complete `0..=100` (best-effort estimate; `0` when unknown).
    pub pct: u8,
    /// Monotonic heartbeat sequence number for this job (starts at 1).
    pub seq: u32,
    /// Unix-millis timestamp the update was emitted.
    pub ts_ms: u64,
}

/// Step 4: Worker commits its result hash *before* streaming data ("commit-first").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultCommit {
    pub job_id: JobId,
    pub worker_id: NodeId,
    /// Canonical BLAKE3 hash of the result (hex). See `p2p-trust`.
    pub result_hash: String,
    pub row_count: u64,
    pub latency_ms: u64,
    /// The fingerprint of the input snapshot this worker actually read
    /// (deterministic-input verification). Echoes [`Dispatch::input_snapshot`]'s
    /// fingerprint when the worker honored the pin; differs when the worker read
    /// a different (e.g. newer) version of the source data — which the requester
    /// classifies as benign drift, NOT a fault. Empty (default / an older worker)
    /// ⇒ unknown: treated as "on the pinned snapshot", never a false drift.
    #[serde(default)]
    pub input_fingerprint: String,
}

/// Wire compression codec for result payloads (mirrors `p2p_config::CompressionAlgo`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Compression {
    #[default]
    None,
    Lz4,
    Zstd,
}

/// Sent on the control stream *before* any bulk bytes, describing how the result
/// is encoded and split. The receiver uses it to allocate, decompress, and know
/// how many unidirectional streams (`parts`) to expect.
///
/// * `parts == 1` ⇒ the payload follows inline as [`ResultChunk`] messages on the
///   same control stream (back-compatible single-stream path).
/// * `parts > 1`  ⇒ the payload is split across `parts` unidirectional streams,
///   each carrying one [`ResultPart`] header followed by raw bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultManifest {
    pub job_id: JobId,
    /// Compression applied to the serialized [`ResultSet`].
    pub compression: Compression,
    /// Length (bytes) of the serialized result *before* compression.
    pub uncompressed_len: u64,
    /// Length (bytes) of the (possibly compressed) payload that is transferred.
    pub total_len: u64,
    /// Number of streams the payload is split across (>= 1).
    pub parts: u32,
}

impl ResultManifest {
    /// Defense-in-depth sanity check on the **attacker-controlled** size fields
    /// before the receiver allocates any reassembly buffer or enters its
    /// `accept_uni` loop. The winning worker is untrusted, so a malicious manifest
    /// declaring a huge `total_len`/`uncompressed_len`/`parts` must be rejected
    /// *before* it can drive an unbounded allocation (OOM).
    ///
    /// `max_bytes`/`max_parts` are the receiver's configured ceilings; both are
    /// additionally clamped to the absolute [`crate::MAX_RESULT_BYTES`] /
    /// [`crate::MAX_RESULT_PARTS`].
    pub fn validate(&self, max_bytes: u64, max_parts: u32) -> Result<(), crate::ProtoError> {
        let bytes_ceiling = max_bytes.min(crate::MAX_RESULT_BYTES);
        let parts_ceiling = max_parts.min(crate::MAX_RESULT_PARTS);
        if self.parts == 0 {
            return Err(crate::ProtoError::InvalidFrame(
                "result manifest declares 0 parts".into(),
            ));
        }
        if self.parts > parts_ceiling {
            return Err(crate::ProtoError::InvalidFrame(format!(
                "result parts {} exceeds limit {}",
                self.parts, parts_ceiling
            )));
        }
        if self.total_len > bytes_ceiling {
            return Err(crate::ProtoError::InvalidFrame(format!(
                "result total_len {} exceeds limit {}",
                self.total_len, bytes_ceiling
            )));
        }
        if self.uncompressed_len > bytes_ceiling {
            return Err(crate::ProtoError::InvalidFrame(format!(
                "result uncompressed_len {} exceeds limit {}",
                self.uncompressed_len, bytes_ceiling
            )));
        }
        Ok(())
    }
}

/// Step 5: a chunk of the inline bulk result stream (winner only, `parts == 1`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultChunk {
    pub job_id: JobId,
    pub seq: u32,
    pub last: bool,
    /// A slice of the (possibly compressed) serialized [`ResultSet`].
    pub payload: Vec<u8>,
}

/// Header for one part of a parallel result transfer (`parts > 1`). Sent as the
/// first framed message on a dedicated unidirectional stream; the `len` raw
/// payload bytes follow immediately (un-framed) and are placed at `offset` in the
/// reassembled payload. Keeping the bulk bytes un-framed avoids the control-frame
/// size cap and an extra copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultPart {
    pub job_id: JobId,
    /// 0-based part index.
    pub index: u32,
    /// Byte offset of this part within the reassembled payload.
    pub offset: u64,
    /// Number of raw payload bytes that follow this header on the stream.
    pub len: u64,
}

/// Requester cancels a (losing or failed) job, triggering a stream RESET.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cancel {
    pub job_id: JobId,
    pub reason: String,
}

/// Generic acknowledgement / error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ack {
    pub job_id: JobId,
    pub ok: bool,
    pub detail: String,
}

/// Verdict recorded in a [`Receipt`] (architecture §7.3, "Abuse resistance").
///
/// Verdicts split into three fault classes (see [`Verdict::is_provider_fault`]):
///  * **Provider fault** — `Incorrect` / `Timeout` / `Malformed`: the provider
///    returned a wrong result, went silent, or produced a corrupt frame. These
///    count against the provider's reputation and may incur a penalty.
///  * **Requester / job fault** — `ResourceExceeded` / `Infeasible`: the *job*
///    was too expensive (OOM / over budget) or impossible (missing data, query
///    infeasible). The provider is blameless; **zero** provider penalty.
///  * **Non-attributable** — `Inconclusive`: a non-verifiable (non-deterministic)
///    query, a job-consensus failure (most/all providers failed the same way), or
///    an admission rejection. Neutral: no reputation effect either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Correct,
    Incorrect,
    Timeout,
    Malformed,
    /// The job exceeded a resource budget (OOM / memory limit / too expensive).
    /// Requester/job fault — never penalizes the provider.
    ResourceExceeded,
    /// The query was infeasible (missing data, unsatisfiable, malformed *input*).
    /// Requester/job fault — never penalizes the provider.
    Infeasible,
    /// The outcome is non-attributable (non-verifiable/non-deterministic query,
    /// job-consensus failure, or admission rejection). Neutral — no score effect.
    Inconclusive,
}

impl Verdict {
    /// A success.
    pub fn is_correct(self) -> bool {
        matches!(self, Verdict::Correct)
    }

    /// Whether this verdict is **provable provider fault** (the only class that
    /// may reduce a provider's reputation / incur a penalty). Requester/job-caused
    /// and non-attributable verdicts return `false`.
    pub fn is_provider_fault(self) -> bool {
        matches!(
            self,
            Verdict::Incorrect | Verdict::Timeout | Verdict::Malformed
        )
    }

    /// Whether this verdict should be recorded as an observation against a
    /// provider's reputation at all. `Correct` and provider-fault verdicts are
    /// recorded; requester/job-caused and non-attributable verdicts are neutral
    /// and recorded as nothing (so they cannot be used to grief a provider).
    pub fn affects_reputation(self) -> bool {
        self.is_correct() || self.is_provider_fault()
    }
}

/// A signed statement about a completed job's outcome (architecture §7.3).
///
/// The `sig` is an Ed25519 signature by `requester_id`'s key over the canonical
/// signing bytes (see `p2p-trust::receipt`). The struct lives here (it appears
/// inside [`Bid`]); signing/verification logic lives in `p2p-trust`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub job_id: JobId,
    pub worker_id: NodeId,
    pub requester_id: NodeId,
    pub query_hash: QueryHash,
    pub result_hash: String,
    pub verdict: Verdict,
    pub latency_ms: u64,
    /// Unix-seconds timestamp.
    pub ts: u64,
    /// Requester-MEASURED workload magnitude for this job (the grid-wide
    /// measured-capability signal): input bytes dispatched, and the result the
    /// requester actually received. `0` = unknown. These are signature-covered,
    /// so they cannot be edited after the requester attests them. `#[serde(default)]`
    /// keeps older receipts (without these fields) readable as zeros.
    #[serde(default)]
    pub observed_input_bytes: u64,
    #[serde(default)]
    pub observed_result_rows: u64,
    #[serde(default)]
    pub observed_result_bytes: u64,
    /// The input-snapshot fingerprint this job was verified against
    /// (deterministic-input verification). Signature-covered and bound into the
    /// anchored `JobRecord` for auditability. Empty (default / an older receipt)
    /// ⇒ unknown (unpinned job), readable as before.
    #[serde(default)]
    pub input_fingerprint: String,
    /// Hex Ed25519 public key of the requester (so verifiers can check `sig`).
    pub requester_pubkey: String,
    /// Hex Ed25519 signature over the canonical signing bytes.
    pub sig: String,
}

/// A signed statement that a node/wallet is an abusive actor, gossiped so each
/// node can **independently** decide to refuse the flagged actor (ARCHITECTURE
/// "Abuse resistance"). There is no central authority: a node honors a signal
/// only if it verifies and only if its policy opts in.
///
/// The `sig` is an Ed25519 signature by `reporter_id`'s key over the canonical
/// signing bytes (see `p2p-trust::abuse`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbuseSignal {
    /// The accused node identity.
    pub subject_id: NodeId,
    /// The accused wallet address, if the report is wallet-keyed.
    pub subject_wallet: Option<String>,
    /// Machine-readable reason (`wrong_result` | `equivocation` | `slashed` |
    /// `downtime` | `exfiltration` | …).
    pub reason: String,
    /// Unix-seconds timestamp the signal was issued.
    pub ts: u64,
    /// The reporter's node identity (must hash to `reporter_pubkey`).
    pub reporter_id: NodeId,
    /// Hex Ed25519 public key of the reporter.
    pub reporter_pubkey: String,
    /// Hex Ed25519 signature over the canonical signing bytes.
    pub sig: String,
}

/// Handshake hello exchanged once per connection before any application
/// messages (architecture §5.1). Carries the full semver, the minimum version
/// the sender will accept, and engine/extension build versions for
/// result-determinism policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    /// Wire schema tag the sender speaks.
    pub schema_version: u16,
    /// The sender's current protocol version.
    pub protocol_version: Version,
    /// The minimum protocol version the sender will accept from a peer.
    pub min_supported: Version,
    pub node_id: NodeId,
    /// DuckDB engine version (for result-determinism / quorum policy).
    pub engine_version: String,
    /// This extension/build version.
    pub extension_version: String,
}

/// A typed rejection sent when a peer is version-incompatible, so the other side
/// gets a clear reason instead of a silent drop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionReject {
    pub reason: String,
    pub our_version: Version,
    pub min_supported: Version,
}

/// The uniform tagged envelope sent over a framed QUIC stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Wire {
    /// Connection handshake.
    Hello(Hello),
    /// Typed version-incompatibility rejection.
    VersionReject(VersionReject),
    Offer(Offer),
    Bid(Bid),
    Dispatch(Dispatch),
    /// Streamed progress / heartbeat (liveness signal) sent during execution.
    Progress(Progress),
    Commit(ResultCommit),
    /// Describes the encoding/splitting of the bulk result (sent before bytes).
    Manifest(ResultManifest),
    Chunk(ResultChunk),
    /// Header for one part of a parallel (multi-stream) result transfer.
    Part(ResultPart),
    Cancel(Cancel),
    Ack(Ack),
    /// A whole result set delivered in one message (small results / tests).
    Result(JobId, ResultSet),
    /// A signed abuse signal (gossiped; each node decides independently).
    Abuse(AbuseSignal),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attestation::Attestation;

    #[test]
    fn wire_roundtrips() {
        let offer = Wire::Offer(Offer {
            job_id: JobId::new(),
            requester_id: NodeId("b3:req".into()),
            query_hash: QueryHash::compute("SELECT 1", "1.0"),
            cost_hint_rows: Some(100),
            cost_hint_bytes: None,
            data_class: DataClass::Public,
            nonce: 42,
            network: None,
            groups: vec![],
            regions: vec![],
            group_proof: None,
            input_fingerprint_hint: None,
        });
        let bytes = crate::to_bytes(&offer).unwrap();
        let back: Wire = crate::from_bytes(&bytes).unwrap();
        assert_eq!(offer, back);
    }

    #[test]
    fn unknown_minor_field_is_tolerated() {
        // A newer minor adds a field to Offer; an older-minor peer (this struct)
        // must still parse the message, ignoring the unknown field. This is the
        // forward-compat hygiene that lets minors differ within a major.
        let json = r#"{
            "Offer": {
                "job_id": "abc",
                "requester_id": "b3:req",
                "query_hash": "h",
                "cost_hint_rows": null,
                "data_class": "Public",
                "nonce": 1,
                "future_field_added_in_v1_1": {"anything": [1,2,3]}
            }
        }"#;
        let parsed: Wire = serde_json::from_str(json).expect("unknown field tolerated");
        match parsed {
            Wire::Offer(o) => assert_eq!(o.nonce, 1),
            other => panic!("expected Offer, got {other:?}"),
        }
    }

    #[test]
    fn result_manifest_validate_rejects_oversize_and_zero_parts() {
        let base = ResultManifest {
            job_id: JobId::new(),
            compression: Compression::None,
            uncompressed_len: 1024,
            total_len: 1024,
            parts: 1,
        };
        // Well-formed manifest within limits is accepted.
        assert!(base.validate(1 << 20, 16).is_ok());
        // Zero parts is rejected (would skip the accept loop / be malformed).
        let mut zero = base.clone();
        zero.parts = 0;
        assert!(zero.validate(1 << 20, 16).is_err());
        // A huge total_len (attacker forcing a pre-allocation) is rejected.
        let mut huge = base.clone();
        huge.total_len = u64::MAX;
        assert!(huge.validate(1 << 20, 16).is_err());
        // A huge uncompressed_len (decompression bomb) is rejected.
        let mut bomb = base.clone();
        bomb.uncompressed_len = u64::MAX;
        assert!(bomb.validate(1 << 20, 16).is_err());
        // Too many parts (unbounded accept_uni loop) is rejected.
        let mut many = base.clone();
        many.parts = 100_000;
        assert!(many.validate(1 << 20, 16).is_err());
        // The configured ceiling is clamped to the absolute MAX_RESULT_BYTES.
        assert!(base.validate(u64::MAX, u32::MAX).is_ok());
    }

    #[test]
    fn bid_carries_attestation_stub_and_receipts() {
        let bid = Bid {
            job_id: JobId::new(),
            worker_id: NodeId("b3:w".into()),
            decision: BidDecision::Accept,
            eta_ms: 10,
            price: 0,
            attestation: Attestation::stub_l0(),
            recent_receipts: vec![],
            free_mem_bytes: 1 << 30,
            free_threads: 4,
            region_proof: None,
            rate_per_second: 0,
            rate_per_gb: 0,
            estimated_seconds: 0,
            cap_seconds: 0,
        };
        assert_eq!(bid.attestation.level, crate::AttestationLevel::L0);
        let w = Wire::Bid(bid.clone());
        let back: Wire = crate::from_bytes(&crate::to_bytes(&w).unwrap()).unwrap();
        assert_eq!(Wire::Bid(bid), back);
    }
}
