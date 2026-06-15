//! Bulk result transfer over QUIC (architecture §11 + "Transport performance
//! tuning"). A [`ResultSet`] is serialized once, optionally compressed, and
//! transferred to the requester. Two interchangeable encodings, chosen from
//! config and announced up-front in a [`ResultManifest`]:
//!
//! * **Single stream (`parts == 1`)** — the payload is split into fixed-size
//!   [`ResultChunk`]s on the same control stream. QUIC per-stream flow control
//!   applies **backpressure** automatically. Back-compatible default path.
//! * **Parallel (`parts > 1`)** — the payload is split across `parts`
//!   unidirectional streams (bounded by `transport.quic.max_concurrent_uni_streams`)
//!   to fill a fat pipe. Each uni-stream carries a [`ResultPart`] header followed
//!   by raw bytes (un-framed, so it is not capped by the control-frame size).
//!
//! The codec actually applied is carried in the manifest, so the receiver
//! decodes correctly regardless of its own configuration.

use p2p_proto::{Compression, JobId, ResultChunk, ResultManifest, ResultPart, ResultSet, Wire};
use p2p_transport::endpoint::{read_msg, write_msg};
use p2p_transport::{Conn, RecvStream, SendStream, TransportError};

use crate::compression::{decompress, maybe_compress};

/// Options governing how a result is encoded and transferred. Derived from
/// `[transport.*]` config + per-call overrides; nothing here is hard-coded.
#[derive(Debug, Clone)]
pub struct SendOpts {
    /// Inline chunk size (bytes) for the single-stream path.
    pub chunk_bytes: usize,
    /// Number of concurrent unidirectional streams (>= 1).
    pub parallelism: usize,
    /// Only fan out across streams when the payload is at least this large.
    pub parallel_min_bytes: usize,
    /// Requested wire compression codec.
    pub compression: Compression,
    /// Codec level (zstd).
    pub compression_level: i32,
    /// Only compress payloads at least this large.
    pub compression_min_bytes: usize,
}

impl Default for SendOpts {
    fn default() -> Self {
        Self {
            chunk_bytes: 256 * 1024,
            parallelism: 1,
            parallel_min_bytes: 1024 * 1024,
            compression: Compression::None,
            compression_level: 3,
            compression_min_bytes: 64 * 1024,
        }
    }
}

/// Decide how many streams to split a payload of `total_len` bytes across.
fn decide_parts(total_len: usize, opts: &SendOpts) -> u32 {
    if opts.parallelism <= 1 || total_len < opts.parallel_min_bytes {
        return 1;
    }
    // At least one byte per part; never more parts than bytes.
    let parts = opts.parallelism.min(total_len.max(1));
    parts.max(1) as u32
}

/// Stream a result set to the peer. The `conn` is needed to open unidirectional
/// streams for the parallel path; `control` is the per-job bidi stream the
/// requester is reading.
pub async fn send_result(
    conn: &Conn,
    control: &mut SendStream,
    job_id: &JobId,
    result: &ResultSet,
    opts: &SendOpts,
) -> p2p_transport::Result<()> {
    let raw = p2p_proto::to_bytes(result)?;
    let uncompressed_len = raw.len();
    let (codec, payload) = maybe_compress(
        opts.compression,
        opts.compression_level,
        opts.compression_min_bytes,
        &raw,
    );
    let total_len = payload.len();
    let parts = decide_parts(total_len, opts);

    write_msg(
        control,
        &Wire::Manifest(ResultManifest {
            job_id: job_id.clone(),
            compression: codec,
            uncompressed_len: uncompressed_len as u64,
            total_len: total_len as u64,
            parts,
        }),
    )
    .await?;

    if parts == 1 {
        send_inline(control, job_id, &payload, opts.chunk_bytes).await?;
    } else {
        send_parallel(conn, job_id, &payload, parts).await?;
    }
    let _ = control.finish();
    Ok(())
}

/// Single-stream path: backpressured fixed-size chunks on the control stream.
async fn send_inline(
    control: &mut SendStream,
    job_id: &JobId,
    payload: &[u8],
    chunk_bytes: usize,
) -> p2p_transport::Result<()> {
    let chunk_bytes = chunk_bytes.max(1);
    let total = payload.len();
    let mut seq = 0u32;
    let mut offset = 0usize;
    loop {
        let end = (offset + chunk_bytes).min(total);
        let last = end >= total;
        write_msg(
            control,
            &Wire::Chunk(ResultChunk {
                job_id: job_id.clone(),
                seq,
                last,
                payload: payload[offset..end].to_vec(),
            }),
        )
        .await?;
        offset = end;
        seq += 1;
        if last {
            break;
        }
    }
    Ok(())
}

/// Parallel path: split the payload across `parts` unidirectional streams and
/// send them concurrently (bounded by `parts`, which is itself bounded by the
/// configured uni-stream cap).
async fn send_parallel(
    conn: &Conn,
    job_id: &JobId,
    payload: &[u8],
    parts: u32,
) -> p2p_transport::Result<()> {
    let total = payload.len();
    let parts = parts as usize;
    let base = total / parts;
    let rem = total % parts;

    let mut sends = Vec::with_capacity(parts);
    let mut offset = 0usize;
    for index in 0..parts {
        // Spread the remainder one byte at a time over the first `rem` parts.
        let len = base + if index < rem { 1 } else { 0 };
        let slice = payload[offset..offset + len].to_vec();
        let off = offset;
        offset += len;
        let job_id = job_id.clone();
        sends.push(async move {
            let mut s = conn.open_uni().await?;
            write_msg(
                &mut s,
                &Wire::Part(ResultPart {
                    job_id,
                    index: index as u32,
                    offset: off as u64,
                    len: len as u64,
                }),
            )
            .await?;
            s.write_all(&slice)
                .await
                .map_err(|e| TransportError::Stream(e.to_string()))?;
            let _ = s.finish();
            Ok::<(), TransportError>(())
        });
    }
    futures_util::future::try_join_all(sends).await?;
    Ok(())
}

/// Reassemble a result set from the peer, honoring the manifest's encoding.
///
/// The winning worker is **untrusted**: its [`ResultManifest`] carries
/// attacker-controlled size fields (`total_len` / `uncompressed_len` / `parts`)
/// that would otherwise drive an unbounded reassembly allocation or an unbounded
/// `accept_uni` loop. Before allocating *anything* we validate the manifest
/// against the receiver's configured ceilings (`max_bytes`/`max_parts`, each
/// additionally clamped to the absolute [`p2p_proto::MAX_RESULT_BYTES`] /
/// [`p2p_proto::MAX_RESULT_PARTS`]) and reject oversize / zero-part manifests.
pub async fn recv_result(
    conn: &Conn,
    control: &mut RecvStream,
    max_bytes: u64,
    max_parts: u32,
) -> p2p_transport::Result<ResultSet> {
    let manifest = match read_msg(control).await? {
        Wire::Manifest(m) => m,
        other => {
            return Err(TransportError::Stream(format!(
                "expected result Manifest, got {other:?}"
            )))
        }
    };

    // Defense-in-depth: reject a malicious manifest BEFORE any allocation (the
    // reassembly buffer and the parallel `accept_uni` loop are both sized from
    // these fields).
    manifest
        .validate(max_bytes, max_parts)
        .map_err(|e| TransportError::Stream(format!("result manifest rejected: {e}")))?;

    let payload = if manifest.parts <= 1 {
        recv_inline(control, manifest.total_len).await?
    } else {
        recv_parallel(conn, &manifest).await?
    };

    let bytes = decompress(
        manifest.compression,
        manifest.uncompressed_len as usize,
        &payload,
    )
    .map_err(TransportError::Stream)?;
    Ok(p2p_proto::from_bytes(&bytes)?)
}

/// Single-stream reassembly: read ordered chunks until `last`. `total_len` is the
/// manifest's already-validated declared payload size; the accumulated chunk
/// bytes may not exceed it (an untrusted worker must not be able to grow the
/// buffer past the size it announced and we validated).
async fn recv_inline(control: &mut RecvStream, total_len: u64) -> p2p_transport::Result<Vec<u8>> {
    let total_len = total_len as usize;
    // Pre-size to the (bounded, validated) declared length to avoid reallocs.
    let mut buf: Vec<u8> = Vec::with_capacity(total_len);
    let mut expected_seq = 0u32;
    loop {
        match read_msg(control).await? {
            Wire::Chunk(chunk) => {
                if chunk.seq != expected_seq {
                    return Err(TransportError::Stream(format!(
                        "out-of-order chunk: expected {}, got {}",
                        expected_seq, chunk.seq
                    )));
                }
                if buf.len() + chunk.payload.len() > total_len {
                    return Err(TransportError::Stream(format!(
                        "inline result exceeds declared total_len {total_len}"
                    )));
                }
                buf.extend_from_slice(&chunk.payload);
                expected_seq += 1;
                if chunk.last {
                    break;
                }
            }
            other => {
                return Err(TransportError::Stream(format!(
                    "expected result Chunk, got {other:?}"
                )))
            }
        }
    }
    Ok(buf)
}

/// Parallel reassembly: accept `parts` unidirectional streams and place each
/// part at its declared offset. Streams may arrive in any order.
async fn recv_parallel(conn: &Conn, manifest: &ResultManifest) -> p2p_transport::Result<Vec<u8>> {
    let total = manifest.total_len as usize;
    let parts = manifest.parts as usize;
    let mut buf = vec![0u8; total];

    // Accept the streams (sequential, cheap) then read them concurrently.
    let mut tasks = Vec::with_capacity(parts);
    for _ in 0..parts {
        let mut recv = conn.accept_uni().await?;
        tasks.push(async move {
            let part = match read_msg(&mut recv).await? {
                Wire::Part(p) => p,
                other => {
                    return Err(TransportError::Stream(format!(
                        "expected result Part, got {other:?}"
                    )))
                }
            };
            let raw = recv
                .read_to_end(part.len as usize)
                .await
                .map_err(|e| TransportError::Stream(format!("part read: {e}")))?;
            if raw.len() as u64 != part.len {
                return Err(TransportError::Stream(format!(
                    "part {} length mismatch: header {} vs read {}",
                    part.index,
                    part.len,
                    raw.len()
                )));
            }
            Ok::<(usize, Vec<u8>), TransportError>((part.offset as usize, raw))
        });
    }
    let collected = futures_util::future::try_join_all(tasks).await?;
    for (offset, raw) in collected {
        if offset + raw.len() > buf.len() {
            return Err(TransportError::Stream(
                "result part overruns declared payload length".into(),
            ));
        }
        buf[offset..offset + raw.len()].copy_from_slice(&raw);
    }
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p2p_config::{GridConfig, IdentityConfig, PinningMode};
    use p2p_proto::{JobId, ResultSet, Value};
    use p2p_transport::{NodeIdentity, QuicTransport, Transport};
    use std::sync::Arc;
    use std::time::Duration;

    fn idcfg() -> IdentityConfig {
        IdentityConfig {
            key_path: None,
            pinning_mode: PinningMode::Tofu,
            allowlist: vec![],
        }
    }

    /// Establish a loopback QUIC connection, returning (server_conn, client_conn)
    /// plus the transports (kept alive by the caller).
    async fn conn_pair() -> (Conn, Conn, Arc<QuicTransport>, Arc<QuicTransport>) {
        let net = GridConfig::default().network;
        let server = Arc::new(
            QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap(),
        );
        let client = Arc::new(
            QuicTransport::bind(&net, &idcfg(), NodeIdentity::generate().unwrap()).unwrap(),
        );
        let addr = server.local_addr().unwrap();
        let server_id = server.local_node_id().clone();
        let srv = server.clone();
        let accept = tokio::spawn(async move { srv.accept().await.unwrap().unwrap() });
        let client_conn = client.connect(addr, Some(server_id)).await.unwrap();
        let server_conn = accept.await.unwrap();
        (server_conn, client_conn, server, client)
    }

    #[tokio::test]
    async fn recv_result_rejects_oversize_manifest_before_allocating() {
        let (server_conn, client_conn, _s, _c) = conn_pair().await;
        let job = JobId::new();

        // Malicious "winner": announce an absurd `total_len` (would force a multi-
        // GB reassembly allocation) on an otherwise well-formed manifest.
        let writer = tokio::spawn(async move {
            let (mut send, _recv) = client_conn.open_bi().await.unwrap();
            write_msg(
                &mut send,
                &Wire::Manifest(ResultManifest {
                    job_id: job.clone(),
                    compression: Compression::None,
                    uncompressed_len: u64::MAX,
                    total_len: u64::MAX,
                    parts: 1,
                }),
            )
            .await
            .unwrap();
            // Keep the stream open briefly so the reader sees the manifest.
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let (_send, mut recv) = server_conn.accept_bi().await.unwrap();
        // Small configured caps: the manifest must be rejected outright.
        let res = recv_result(&server_conn, &mut recv, 64 * 1024 * 1024, 64).await;
        assert!(res.is_err(), "oversize manifest must be rejected");
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("manifest rejected"), "unexpected error: {msg}");
        let _ = writer.await;
    }

    #[tokio::test]
    async fn recv_result_rejects_zero_part_manifest() {
        let (server_conn, client_conn, _s, _c) = conn_pair().await;
        let job = JobId::new();
        let writer = tokio::spawn(async move {
            let (mut send, _recv) = client_conn.open_bi().await.unwrap();
            write_msg(
                &mut send,
                &Wire::Manifest(ResultManifest {
                    job_id: job.clone(),
                    compression: Compression::None,
                    uncompressed_len: 0,
                    total_len: 0,
                    parts: 0, // invalid: zero parts
                }),
            )
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let (_send, mut recv) = server_conn.accept_bi().await.unwrap();
        let res = recv_result(&server_conn, &mut recv, 64 * 1024 * 1024, 64).await;
        assert!(res.is_err(), "zero-part manifest must be rejected");
        let _ = writer.await;
    }

    #[tokio::test]
    async fn valid_result_still_roundtrips_within_caps() {
        let (server_conn, client_conn, _s, _c) = conn_pair().await;
        let job = JobId::new();
        let rs = ResultSet::new(
            vec!["k".into(), "v".into()],
            (0..100u8)
                .map(|i| vec![Value::Int(i as i64), Value::Int(7)])
                .collect(),
        );
        let rs_clone = rs.clone();
        let job_w = job.clone();
        let writer = tokio::spawn(async move {
            let (mut send, _recv) = client_conn.open_bi().await.unwrap();
            send_result(
                &client_conn,
                &mut send,
                &job_w,
                &rs_clone,
                &SendOpts::default(),
            )
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
        });

        let (_send, mut recv) = server_conn.accept_bi().await.unwrap();
        let got = recv_result(&server_conn, &mut recv, 64 * 1024 * 1024, 64)
            .await
            .expect("valid result within caps should pass");
        assert_eq!(got.row_count(), rs.row_count());
        assert_eq!(got, rs);
        let _ = writer.await;
    }
}
