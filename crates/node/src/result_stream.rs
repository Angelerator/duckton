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

use p2p_proto::{
    Compression, JobId, ResultChunk, ResultManifest, ResultPart, ResultSet, Wire,
};
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
