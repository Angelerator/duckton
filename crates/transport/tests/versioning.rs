//! Protocol version negotiation over real QUIC (architecture §5.1):
//!  (a) compatible versions connect and negotiate the common (lower) version,
//!  (b) a peer below `min_supported` is cleanly rejected with the typed
//!      incompatible-version error,
//!  (c) a different major cannot even establish a connection (ALPN gate).

use p2p_config::{GridConfig, IdentityConfig, PinningMode};
use p2p_proto::Version;
use p2p_transport::{NodeIdentity, QuicTransport, Transport, TransportError, VersionInfo};

fn idcfg() -> IdentityConfig {
    IdentityConfig {
        key_path: None,
        pinning_mode: PinningMode::Tofu,
        allowlist: vec![],
    }
}

fn vinfo(version: Version, min: Version) -> VersionInfo {
    VersionInfo {
        version,
        min_supported: min,
        engine_version: "duckdb-test".into(),
        extension_version: "0.1.0".into(),
        require_matching_engine: false,
    }
}

#[tokio::test]
async fn compatible_versions_negotiate_common_lower() {
    let net = GridConfig::default().network;
    let server = QuicTransport::bind_with_version(
        &net,
        &idcfg(),
        NodeIdentity::generate().unwrap(),
        vinfo(Version::new(1, 3, 0), Version::new(1, 0, 0)),
    )
    .unwrap();
    let client = QuicTransport::bind_with_version(
        &net,
        &idcfg(),
        NodeIdentity::generate().unwrap(),
        vinfo(Version::new(1, 1, 0), Version::new(1, 0, 0)),
    )
    .unwrap();
    let server_id = server.local_node_id().clone();
    let addr = server.local_addr().unwrap();

    let server_clone = server.clone();
    let server_task = tokio::spawn(async move {
        let conn = server_clone.accept().await.unwrap().unwrap();
        // Server (v1.3) negotiates down to the common v1.1.
        assert_eq!(conn.negotiated().version, Version::new(1, 1, 0));
        assert_eq!(conn.negotiated().peer_engine_version, "duckdb-test");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    });

    let conn = client.connect(addr, Some(server_id)).await.unwrap();
    assert_eq!(conn.negotiated().version, Version::new(1, 1, 0));
    server_task.await.unwrap();
}

#[tokio::test]
async fn peer_below_min_supported_is_rejected_typed() {
    let net = GridConfig::default().network;
    // Server requires >= 1.5.0 (same major 1 → same ALPN, so the handshake runs).
    let server = QuicTransport::bind_with_version(
        &net,
        &idcfg(),
        NodeIdentity::generate().unwrap(),
        vinfo(Version::new(1, 5, 0), Version::new(1, 5, 0)),
    )
    .unwrap();
    // Client is too old.
    let client = QuicTransport::bind_with_version(
        &net,
        &idcfg(),
        NodeIdentity::generate().unwrap(),
        vinfo(Version::new(1, 2, 0), Version::new(1, 0, 0)),
    )
    .unwrap();
    let server_id = server.local_node_id().clone();
    let addr = server.local_addr().unwrap();

    let server_clone = server.clone();
    tokio::spawn(async move {
        // Server side will produce a typed Err and send a VersionReject.
        let _ = server_clone.accept().await;
    });

    let result = client.connect(addr, Some(server_id)).await;
    assert!(
        matches!(result, Err(TransportError::IncompatibleVersion(_))),
        "expected typed IncompatibleVersion, got {result:?}"
    );
}

#[tokio::test]
async fn different_major_cannot_connect() {
    let net = GridConfig::default().network;
    // Server speaks major 2 → ALPN "duckdb-p2p/2".
    let server = QuicTransport::bind_with_version(
        &net,
        &idcfg(),
        NodeIdentity::generate().unwrap(),
        vinfo(Version::new(2, 0, 0), Version::new(2, 0, 0)),
    )
    .unwrap();
    // Client speaks major 1 → ALPN "duckdb-p2p/1"; no shared protocol.
    let client = QuicTransport::bind_with_version(
        &net,
        &idcfg(),
        NodeIdentity::generate().unwrap(),
        vinfo(Version::new(1, 0, 0), Version::new(1, 0, 0)),
    )
    .unwrap();
    let server_id = server.local_node_id().clone();
    let addr = server.local_addr().unwrap();

    let server_clone = server.clone();
    tokio::spawn(async move {
        let _ = server_clone.accept().await;
    });

    // The ALPN gate prevents the TLS handshake from completing at all.
    let result = client.connect(addr, Some(server_id)).await;
    assert!(result.is_err(), "cross-major connection must fail");
}
