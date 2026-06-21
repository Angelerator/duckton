//! Optional wire compression for result payloads (architecture — "Transport
//! performance tuning"). Selectable codec (none / lz4 / zstd) with a size
//! threshold and level, all driven by `[transport.compression]` config; default
//! off so loopback/LAN transfers pay no CPU cost. See the docs for WAN guidance.
//!
//! The codec actually applied to a transfer is carried on the wire in
//! [`p2p_proto::ResultManifest`] so the receiver always knows how to decode,
//! independent of its own config.

use p2p_config::CompressionAlgo;
use p2p_proto::Compression;

/// Map the config algorithm enum to the wire codec enum.
pub fn algo_to_wire(algo: CompressionAlgo) -> Compression {
    match algo {
        CompressionAlgo::None => Compression::None,
        CompressionAlgo::Lz4 => Compression::Lz4,
        CompressionAlgo::Zstd => Compression::Zstd,
    }
}

/// Compress `data` with `codec`, but only when it meets `min_size_bytes`
/// (smaller payloads skip compression and report [`Compression::None`]).
///
/// Returns the codec actually applied and the (possibly compressed) bytes.
pub fn maybe_compress(
    codec: Compression,
    level: i32,
    min_size_bytes: usize,
    data: &[u8],
) -> (Compression, Vec<u8>) {
    if matches!(codec, Compression::None) || data.len() < min_size_bytes {
        return (Compression::None, data.to_vec());
    }
    match codec {
        Compression::None => (Compression::None, data.to_vec()),
        Compression::Lz4 => (Compression::Lz4, lz4_flex::compress_prepend_size(data)),
        Compression::Zstd => match zstd::encode_all(data, level) {
            Ok(out) => (Compression::Zstd, out),
            // On the unlikely encode error, fall back to uncompressed rather
            // than failing the transfer.
            Err(_) => (Compression::None, data.to_vec()),
        },
    }
}

/// Decompress `data` that was encoded with `codec`, given the expected
/// uncompressed length (used as a bound to cap allocation).
pub fn decompress(
    codec: Compression,
    uncompressed_len: usize,
    data: &[u8],
) -> Result<Vec<u8>, String> {
    match codec {
        Compression::None => Ok(data.to_vec()),
        Compression::Lz4 => {
            // `lz4_flex::decompress_size_prepended` trusts the attacker-prepended
            // 4-byte size and pre-allocates that many bytes — an OOM vector when
            // the sender is untrusted. Mirror the Zstd clamp: read the prepended
            // size, REJECT it if it exceeds the manifest's `uncompressed_len`
            // (the source of truth), then decompress with the bounded size.
            if data.len() < 4 {
                return Err("lz4 decompress: truncated size prefix".into());
            }
            let prepended = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
            if prepended > uncompressed_len {
                return Err(format!(
                    "lz4 prepended size {prepended} exceeds declared uncompressed_len {uncompressed_len}"
                ));
            }
            lz4_flex::decompress(&data[4..], prepended).map_err(|e| format!("lz4 decompress: {e}"))
        }
        Compression::Zstd => {
            // `zstd::decode_all` inflates the WHOLE frame before we could
            // truncate, so a zstd bomb (tiny compressed, huge decompressed) OOMs
            // the receiver. Stream-decode with a hard read limit of
            // `uncompressed_len + 1` instead: allocation is bounded by the
            // manifest's declared length, and a frame that decompresses larger is
            // rejected rather than fully materialized.
            use std::io::Read;
            let decoder =
                zstd::stream::read::Decoder::new(data).map_err(|e| format!("zstd decoder: {e}"))?;
            let limit = uncompressed_len.saturating_add(1);
            let mut out = Vec::new();
            decoder
                .take(limit as u64)
                .read_to_end(&mut out)
                .map_err(|e| format!("zstd decompress: {e}"))?;
            if out.len() > uncompressed_len {
                return Err(format!(
                    "zstd output exceeds declared uncompressed_len {uncompressed_len}"
                ));
            }
            Ok(out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_below_threshold_roundtrips() {
        let data = b"hello world".to_vec();
        let (codec, out) = maybe_compress(Compression::Zstd, 3, 1024, &data);
        assert_eq!(codec, Compression::None); // below threshold
        assert_eq!(decompress(codec, data.len(), &out).unwrap(), data);
    }

    #[test]
    fn lz4_and_zstd_roundtrip() {
        let data: Vec<u8> = (0..50_000u32)
            .flat_map(|i| (i % 251) as u8..(i % 251) as u8 + 1)
            .collect();
        for codec in [Compression::Lz4, Compression::Zstd] {
            let (applied, out) = maybe_compress(codec, 3, 0, &data);
            assert_eq!(applied, codec);
            let back = decompress(applied, data.len(), &out).unwrap();
            assert_eq!(back, data);
        }
    }

    #[test]
    fn lz4_rejects_oversized_prepended_size() {
        // A malicious LZ4 payload that prepends a huge size must be rejected
        // against the declared `uncompressed_len` BEFORE allocating (mirrors the
        // zstd clamp). Craft a valid compression of a small payload, then forge
        // the 4-byte LE prefix to claim a gigantic uncompressed length.
        let data = vec![7u8; 4096];
        let (_, mut out) = maybe_compress(Compression::Lz4, 3, 0, &data);
        // Forge the prepended size to ~4 GiB (u32::MAX), far above the declared
        // uncompressed_len of 4096.
        out[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        let err = decompress(Compression::Lz4, data.len(), &out).unwrap_err();
        assert!(
            err.contains("exceeds declared uncompressed_len"),
            "got: {err}"
        );
    }

    #[test]
    fn lz4_truncated_prefix_is_rejected() {
        let err = decompress(Compression::Lz4, 1024, &[0u8; 2]).unwrap_err();
        assert!(err.contains("truncated size prefix"), "got: {err}");
    }

    #[test]
    fn zstd_bomb_rejected_against_declared_len() {
        // A highly compressible payload zips tiny but inflates large. If the
        // manifest under-declares `uncompressed_len`, decode must stop at the
        // bound and reject instead of allocating the full (bomb) output.
        let big = vec![0u8; 4 * 1024 * 1024]; // 4 MiB of zeros → tiny zstd frame
        let (codec, out) = maybe_compress(Compression::Zstd, 3, 0, &big);
        assert_eq!(codec, Compression::Zstd);
        assert!(out.len() < big.len(), "zstd should shrink a zero buffer");
        // Honest length still round-trips.
        assert_eq!(decompress(codec, big.len(), &out).unwrap(), big);
        // A forged small declared length must be rejected, not inflated.
        let err = decompress(codec, 1024, &out).unwrap_err();
        assert!(
            err.contains("exceeds declared uncompressed_len"),
            "got: {err}"
        );
    }
}
