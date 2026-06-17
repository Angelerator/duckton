//! Two in-process nodes complete a mutual-TLS QUIC handshake over loopback and
//! exchange protocol messages. This is the transport half of the Phase 0
//! "walking skeleton".

use std::time::Duration;

use p2p_config::{GridConfig, IdentityConfig, PinningMode};
use p2p_proto::{Ack, BidDecision, DataClass, JobId, NodeId, Offer, QueryHash, Wire};
use p2p_transport::endpoint::{read_msg, request_response, write_msg};
use p2p_transport::{NodeIdentity, QuicTransport, Transport};

fn tofu_identity_cfg() -> IdentityConfig {
    IdentityConfig {
        key_path: None,
        pinning_mode: PinningMode::Tofu,
        allowlist: vec![],
    }
}

fn make_offer(requester: &NodeId) -> Offer {
    Offer {
        job_id: JobId::new(),
        requester_id: requester.clone(),
        query_hash: QueryHash::compute("SELECT 1", "test"),
        cost_hint_rows: Some(1),
        cost_hint_bytes: None,
        data_class: DataClass::Public,
        nonce: 7,
        network: None,
        groups: Vec::new(),
        regions: Vec::new(),
        group_proof: None,
        input_fingerprint_hint: None,
    }
}

#[tokio::test]
async fn mtls_handshake_and_message_roundtrip() {
    let net = GridConfig::default().network;
    let idcfg = tofu_identity_cfg();

    let server = QuicTransport::bind(&net, &idcfg, NodeIdentity::generate().unwrap()).unwrap();
    let client = QuicTransport::bind(&net, &idcfg, NodeIdentity::generate().unwrap()).unwrap();

    let server_id = server.local_node_id().clone();
    let client_id = client.local_node_id().clone();
    let server_addr = server.local_addr().unwrap();

    // Server: accept one connection, read an Offer, reply with an Ack.
    let server_clone = server.clone();
    let expected_client = client_id.clone();
    let server_task = tokio::spawn(async move {
        let conn = server_clone.accept().await.unwrap().unwrap();
        // The peer identity is authenticated via the pinned mTLS cert.
        assert_eq!(conn.peer_node_id(), &expected_client);
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        let msg = read_msg(&mut recv).await.unwrap();
        let job_id = match msg {
            Wire::Offer(o) => {
                assert_eq!(o.requester_id, expected_client);
                o.job_id
            }
            other => panic!("expected Offer, got {other:?}"),
        };
        write_msg(
            &mut send,
            &Wire::Ack(Ack {
                job_id,
                ok: true,
                detail: "accepted".into(),
            }),
        )
        .await
        .unwrap();
        send.finish().unwrap();
        // keep the connection alive briefly so the client can read the reply
        tokio::time::sleep(Duration::from_millis(50)).await;
    });

    // Client: connect (pinning the server id), send an Offer, read the Ack.
    let conn = client
        .connect(server_addr, Some(server_id.clone()))
        .await
        .unwrap();
    assert_eq!(conn.peer_node_id(), &server_id);

    let reply = request_response(&conn, &Wire::Offer(make_offer(&client_id)))
        .await
        .unwrap();
    match reply {
        Wire::Ack(ack) => assert!(ack.ok),
        other => panic!("expected Ack, got {other:?}"),
    }

    server_task.await.unwrap();
}

#[tokio::test]
async fn pinning_mismatch_is_rejected() {
    let net = GridConfig::default().network;
    let idcfg = tofu_identity_cfg();

    let server = QuicTransport::bind(&net, &idcfg, NodeIdentity::generate().unwrap()).unwrap();
    let client = QuicTransport::bind(&net, &idcfg, NodeIdentity::generate().unwrap()).unwrap();
    let server_addr = server.local_addr().unwrap();

    let server_clone = server.clone();
    tokio::spawn(async move {
        let _ = server_clone.accept().await;
    });

    // Pin to the WRONG node id -> connect must fail with a pinning error.
    let wrong = NodeId("b3:deadbeef".into());
    let result = client.connect(server_addr, Some(wrong)).await;
    assert!(matches!(
        result,
        Err(p2p_transport::TransportError::Pinning { .. })
    ));
}

#[tokio::test]
async fn allowlist_blocks_unknown_peer() {
    let net = GridConfig::default().network;

    // Server only allows a bogus id, so the real client must be rejected during
    // the handshake (client-cert verification fails server-side).
    let server_idcfg = IdentityConfig {
        key_path: None,
        pinning_mode: PinningMode::Allowlist,
        allowlist: vec!["b3:not-the-client".to_string()],
    };
    let server =
        QuicTransport::bind(&net, &server_idcfg, NodeIdentity::generate().unwrap()).unwrap();
    let client = QuicTransport::bind(
        &net,
        &tofu_identity_cfg(),
        NodeIdentity::generate().unwrap(),
    )
    .unwrap();
    let server_addr = server.local_addr().unwrap();
    let server_id = server.local_node_id().clone();

    let server_clone = server.clone();
    tokio::spawn(async move {
        // Accepting will fail the handshake because the client is not allowlisted.
        let _ = server_clone.accept().await;
    });

    // The client may briefly believe the TLS 1.3 handshake completed (it
    // processes the server Finished before the server validates the client
    // cert). A full request/response surfaces the server-side rejection: the
    // server resets the connection, so the read fails.
    let outcome = async {
        let conn = client.connect(server_addr, Some(server_id)).await?;
        let _reply = request_response(
            &conn,
            &Wire::Cancel(p2p_proto::Cancel {
                job_id: JobId::new(),
                reason: "probe".into(),
            }),
        )
        .await?;
        Ok::<(), p2p_transport::TransportError>(())
    }
    .await;

    assert!(
        outcome.is_err(),
        "non-allowlisted client should not be able to communicate"
    );
    // sanity: ensure BidDecision import compiles (used elsewhere in proto)
    let _ = BidDecision::Accept;
}
