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
            lz4_flex::decompress_size_prepended(data).map_err(|e| format!("lz4 decompress: {e}"))
        }
        Compression::Zstd => zstd::decode_all(data)
            .map(|mut v| {
                // Defensive: trust the manifest's length as the source of truth.
                if v.len() != uncompressed_len {
                    v.truncate(uncompressed_len);
                }
                v
            })
            .map_err(|e| format!("zstd decompress: {e}")),
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
        let data: Vec<u8> = (0..50_000u32).flat_map(|i| (i % 251) as u8 .. (i % 251) as u8 + 1).collect();
        for codec in [Compression::Lz4, Compression::Zstd] {
            let (applied, out) = maybe_compress(codec, 3, 0, &data);
            assert_eq!(applied, codec);
            let back = decompress(applied, data.len(), &out).unwrap();
            assert_eq!(back, data);
        }
    }
}
