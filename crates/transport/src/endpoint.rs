//! Quinn QUIC endpoint with mutual-TLS pinned to node identities.
//!
//! A single [`QuicTransport`] both dials (client role) and listens (server
//! role), mirroring the symmetric-node model (every node can be requester or
//! worker). All operational parameters come from [`p2p_config::NetworkConfig`]
//! — nothing is hard-coded here.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use p2p_config::{CongestionAlgo, IdentityConfig, NetworkConfig, QuicTuningConfig};
use p2p_proto::{NodeId, VersionReject, Wire, MAX_FRAME_BYTES};
use quinn::congestion::{BbrConfig, CubicConfig, NewRenoConfig};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{ClientConfig, Endpoint, RecvStream, SendStream, ServerConfig, TransportConfig, VarInt};
use rustls_pki_types::CertificateDer;
use tokio::sync::{mpsc, Mutex, Semaphore};

use crate::error::{Result, TransportError};
use crate::identity::NodeIdentity;
use crate::verifier::{node_id_from_cert, PinPolicy, PinnedClientVerifier, PinnedServerVerifier};
use crate::version::{Negotiated, VersionInfo};

/// An abstract transport so the rest of the system can be tested against fakes
/// and so alternative transports (e.g. a future libp2p QUIC) can be swapped in.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// This node's identity.
    fn local_node_id(&self) -> &NodeId;
    /// The bound local address.
    fn local_addr(&self) -> Result<SocketAddr>;
    /// Dial a peer. If `expected` is set, the established peer id must match.
    async fn connect(&self, addr: SocketAddr, expected: Option<NodeId>) -> Result<Conn>;
    /// Accept the next inbound connection; `None` once the endpoint is closed.
    async fn accept(&self) -> Option<Result<Conn>>;
}

/// Channel capacity for completed inbound connections awaiting `accept()`.
const ACCEPT_CHANNEL_CAP: usize = 256;
/// Upper bound on concurrently in-flight inbound handshakes — a resource guard so
/// a connection flood cannot spawn unbounded handshake tasks.
const MAX_INFLIGHT_HANDSHAKES: usize = 512;

/// A concrete Quinn-backed transport.
#[derive(Clone)]
pub struct QuicTransport {
    endpoint: Endpoint,
    identity: NodeIdentity,
    version: VersionInfo,
    /// Completed inbound connections produced by the background accept loop. Each
    /// connection's QUIC+TLS+version handshake runs concurrently in its own
    /// (bounded, timed-out) task, so one slow/stalled peer cannot head-of-line
    /// block the acceptance of other connections.
    incoming: Arc<Mutex<mpsc::Receiver<Result<Conn>>>>,
    /// Dial + handshake deadline, from `network.connect_timeout_ms`.
    connect_timeout: Duration,
}

impl QuicTransport {
    /// Build and bind a transport from configuration + identity, using the
    /// default version info (current build version) and default QUIC tuning.
    /// Convenience for tests.
    pub fn bind(net: &NetworkConfig, id: &IdentityConfig, identity: NodeIdentity) -> Result<Self> {
        Self::bind_with_version(net, id, identity, VersionInfo::default())
    }

    /// Build and bind a transport, advertising the given protocol/version info,
    /// with default QUIC performance tuning.
    pub fn bind_with_version(
        net: &NetworkConfig,
        id: &IdentityConfig,
        identity: NodeIdentity,
        version: VersionInfo,
    ) -> Result<Self> {
        Self::bind_tuned(net, &QuicTuningConfig::default(), id, identity, version)
    }

    /// Build and bind a transport with explicit QUIC performance tuning
    /// (`[transport.quic]`): UDP offload, congestion control, flow-control
    /// windows, uni-stream cap, and 0-RTT. The ALPN is derived from the protocol
    /// major version. Nothing here is hard-coded — every value comes from config.
    pub fn bind_tuned(
        net: &NetworkConfig,
        quic: &QuicTuningConfig,
        id: &IdentityConfig,
        identity: NodeIdentity,
        version: VersionInfo,
    ) -> Result<Self> {
        install_crypto_provider();

        let alpn = p2p_proto::alpn_for_major(version.version.major);
        let policy = PinPolicy::new(id.pinning_mode, id.allowlist.clone());
        let transport_config = Arc::new(build_transport_config(net, quic)?);

        // ---- server side (validates inbound client certs) ----
        let mut server_crypto = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| TransportError::Tls(e.to_string()))?
        .with_client_cert_verifier(PinnedClientVerifier::new(policy.clone()))
        .with_single_cert(identity.cert_chain(), identity.private_key_der().into())
        .map_err(|e| TransportError::Tls(e.to_string()))?;
        server_crypto.alpn_protocols = vec![alpn.clone()];
        if quic.enable_0rtt {
            // Accept TLS 1.3 early data (0-RTT) from resuming peers. CAVEAT: 0-RTT
            // early data is replayable by a network attacker. This is only safe if
            // the application sends nothing non-idempotent before the handshake is
            // confirmed — the version handshake itself is idempotent, but a higher
            // layer MUST NOT push effectful control messages (Dispatch/Offer/…) on
            // a resumed connection until `Connection::accepted_0rtt`/handshake
            // completion. Keep `enable_0rtt = false` unless that holds.
            server_crypto.max_early_data_size = u32::MAX;
        }

        let mut server_config = ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(server_crypto)
                .map_err(|e| TransportError::Tls(e.to_string()))?,
        ));
        server_config.transport_config(transport_config.clone());

        let bind_addr: SocketAddr = net
            .bind_addr
            .parse()
            .map_err(|e| TransportError::Endpoint(format!("bad bind_addr {}: {e}", net.bind_addr)))?;
        let mut endpoint = Endpoint::server(server_config, bind_addr)
            .map_err(|e| TransportError::Endpoint(e.to_string()))?;

        // ---- client side (validates server certs) ----
        let mut client_crypto = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|e| TransportError::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(PinnedServerVerifier::new(policy))
        .with_client_auth_cert(identity.cert_chain(), identity.private_key_der().into())
        .map_err(|e| TransportError::Tls(e.to_string()))?;
        client_crypto.alpn_protocols = vec![alpn];
        if quic.enable_0rtt {
            // rustls keeps an in-memory session cache by default; opting into
            // early data lets a resuming connection send 0-RTT data.
            client_crypto.enable_early_data = true;
        }

        let mut client_config = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(client_crypto)
                .map_err(|e| TransportError::Tls(e.to_string()))?,
        ));
        client_config.transport_config(transport_config);
        endpoint.set_default_client_config(client_config);

        // Drive inbound handshakes concurrently in a background task (each bounded
        // by `connect_timeout`), so a single slow/stalled peer cannot head-of-line
        // block acceptance, and so a never-completing handshake cannot hang forever.
        let connect_timeout = Duration::from_millis(net.connect_timeout_ms.max(1));
        let (tx, rx) = mpsc::channel::<Result<Conn>>(ACCEPT_CHANNEL_CAP);
        tokio::spawn(server_accept_loop(
            endpoint.clone(),
            version.clone(),
            identity.node_id().clone(),
            tx,
            connect_timeout,
        ));

        Ok(Self {
            endpoint,
            identity,
            version,
            incoming: Arc::new(Mutex::new(rx)),
            connect_timeout,
        })
    }

    /// The local node's version info.
    pub fn version_info(&self) -> &VersionInfo {
        &self.version
    }

    /// Access the node identity (for signing in higher layers).
    pub fn identity(&self) -> &NodeIdentity {
        &self.identity
    }

    /// Gracefully wait for the endpoint to go idle (used in shutdown/tests).
    pub async fn wait_idle(&self) {
        self.endpoint.wait_idle().await;
    }

    pub fn close(&self) {
        self.endpoint.close(VarInt::from_u32(0), b"shutdown");
    }
}

#[async_trait]
impl Transport for QuicTransport {
    fn local_node_id(&self) -> &NodeId {
        self.identity.node_id()
    }

    fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint
            .local_addr()
            .map_err(|e| TransportError::Endpoint(e.to_string()))
    }

    async fn connect(&self, addr: SocketAddr, expected: Option<NodeId>) -> Result<Conn> {
        let connecting = self
            .endpoint
            .connect(addr, "duckdb-p2p")
            .map_err(|e| TransportError::Connection(e.to_string()))?;
        // Bound the dial so a black-hole / unresponsive peer cannot hang forever.
        let connection = tokio::time::timeout(self.connect_timeout, connecting)
            .await
            .map_err(|_| {
                TransportError::Connection(format!(
                    "connect to {addr} timed out after {:?}",
                    self.connect_timeout
                ))
            })?
            .map_err(|e| TransportError::Connection(e.to_string()))?;
        let peer = peer_node_id(&connection)?;
        if let Some(exp) = expected {
            if peer != exp {
                connection.close(VarInt::from_u32(1), b"pin-mismatch");
                return Err(TransportError::Pinning {
                    expected: exp.to_string(),
                    actual: peer.to_string(),
                });
            }
        }
        // Version handshake (client role): we open the hello stream first, bounded
        // by the same deadline.
        let hs = tokio::time::timeout(
            self.connect_timeout,
            handshake_client(&connection, &self.version, self.identity.node_id().clone()),
        )
        .await;
        match hs {
            Ok(Ok(negotiated)) => Ok(Conn {
                connection,
                peer,
                negotiated,
            }),
            Ok(Err(e)) => {
                // If the peer rejected us with the incompatible-version close
                // code, surface a typed error even if the stream-level
                // VersionReject raced the connection close.
                let mapped = map_version_close(&connection).unwrap_or(e);
                connection.close(VarInt::from_u32(2), b"incompatible-version");
                Err(mapped)
            }
            Err(_elapsed) => {
                connection.close(VarInt::from_u32(3), b"handshake-timeout");
                Err(TransportError::Connection(
                    "outbound version handshake timed out".into(),
                ))
            }
        }
    }

    async fn accept(&self) -> Option<Result<Conn>> {
        // The background accept loop performs the handshakes; we just hand back the
        // next completed result. `None` once the endpoint closes (loop ends, the
        // sender drops, and the channel returns `None`).
        self.incoming.lock().await.recv().await
    }
}

/// Background loop: pull raw inbound connection attempts off the endpoint and
/// drive each one's QUIC+TLS+version handshake in its own bounded, timed task,
/// forwarding the (possibly failed) result to the `accept()` channel. Running
/// handshakes concurrently means one slow/stalled peer cannot head-of-line-block
/// the acceptance of others.
async fn server_accept_loop(
    endpoint: Endpoint,
    version: VersionInfo,
    my_id: NodeId,
    tx: mpsc::Sender<Result<Conn>>,
    handshake_timeout: Duration,
) {
    let sem = Arc::new(Semaphore::new(MAX_INFLIGHT_HANDSHAKES));
    while let Some(incoming) = endpoint.accept().await {
        if tx.is_closed() {
            break;
        }
        // Bound concurrent in-flight handshakes.
        let permit = match Arc::clone(&sem).acquire_owned().await {
            Ok(p) => p,
            Err(_) => break,
        };
        let tx = tx.clone();
        let version = version.clone();
        let my_id = my_id.clone();
        tokio::spawn(async move {
            let _permit = permit; // released when the handshake finishes
            let result = accept_one(incoming, &version, my_id, handshake_timeout).await;
            let _ = tx.send(result).await;
        });
    }
}

/// Complete one inbound connection: finish the QUIC/TLS handshake, then run the
/// application version handshake under `handshake_timeout`.
async fn accept_one(
    incoming: quinn::Incoming,
    version: &VersionInfo,
    my_id: NodeId,
    handshake_timeout: Duration,
) -> Result<Conn> {
    let connection = incoming
        .accept()
        .map_err(|e| TransportError::Connection(e.to_string()))?
        .await
        .map_err(|e| TransportError::Connection(e.to_string()))?;
    let peer = peer_node_id(&connection)?;
    // Version handshake (server role): accept the hello stream and reply with our
    // Hello, or a typed VersionReject — bounded so a peer that opens a connection
    // and then never speaks cannot hold a handshake slot forever.
    match tokio::time::timeout(handshake_timeout, handshake_server(&connection, version, my_id))
        .await
    {
        Ok(Ok(negotiated)) => Ok(Conn {
            connection,
            peer,
            negotiated,
        }),
        Ok(Err(e)) => {
            connection.close(VarInt::from_u32(2), b"incompatible-version");
            Err(e)
        }
        Err(_elapsed) => {
            connection.close(VarInt::from_u32(3), b"handshake-timeout");
            Err(TransportError::Connection(
                "inbound version handshake timed out".into(),
            ))
        }
    }
}

/// If the connection was closed by the peer with the incompatible-version
/// application code (2), return a typed [`TransportError::IncompatibleVersion`].
fn map_version_close(connection: &quinn::Connection) -> Option<TransportError> {
    match connection.close_reason()? {
        quinn::ConnectionError::ApplicationClosed(ac) if ac.error_code == VarInt::from_u32(2) => {
            Some(TransportError::IncompatibleVersion(format!(
                "peer closed connection as version-incompatible: {}",
                String::from_utf8_lossy(&ac.reason)
            )))
        }
        _ => None,
    }
}

/// Client-side version handshake: open a stream, send our Hello, await the peer.
async fn handshake_client(
    connection: &quinn::Connection,
    info: &VersionInfo,
    my_node_id: NodeId,
) -> Result<Negotiated> {
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|e| TransportError::Stream(e.to_string()))?;
    write_msg(&mut send, &Wire::Hello(info.hello(my_node_id))).await?;
    let reply = read_msg(&mut recv).await?;
    let _ = send.finish();
    match reply {
        Wire::Hello(peer) => info.negotiate(&peer),
        Wire::VersionReject(rej) => Err(TransportError::IncompatibleVersion(format!(
            "peer rejected us: {} (peer version {}, min {})",
            rej.reason, rej.our_version, rej.min_supported
        ))),
        other => Err(TransportError::Connection(format!(
            "expected Hello, got {other:?}"
        ))),
    }
}

/// Server-side version handshake: accept the hello stream, reply Hello or
/// VersionReject.
async fn handshake_server(
    connection: &quinn::Connection,
    info: &VersionInfo,
    my_node_id: NodeId,
) -> Result<Negotiated> {
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(|e| TransportError::Stream(e.to_string()))?;
    let peer = match read_msg(&mut recv).await? {
        Wire::Hello(h) => h,
        other => {
            return Err(TransportError::Connection(format!(
                "expected Hello, got {other:?}"
            )))
        }
    };
    match info.negotiate(&peer) {
        Ok(neg) => {
            write_msg(&mut send, &Wire::Hello(info.hello(my_node_id))).await?;
            let _ = send.finish();
            Ok(neg)
        }
        Err(e) => {
            let rej = Wire::VersionReject(VersionReject {
                reason: e.to_string(),
                our_version: info.version,
                min_supported: info.min_supported,
            });
            let _ = write_msg(&mut send, &rej).await;
            let _ = send.finish();
            Err(e)
        }
    }
}

/// A live, authenticated, version-negotiated connection to a peer.
#[derive(Clone, Debug)]
pub struct Conn {
    connection: quinn::Connection,
    peer: NodeId,
    negotiated: Negotiated,
}

impl Conn {
    /// The authenticated node id of the peer (derived from its pinned cert).
    pub fn peer_node_id(&self) -> &NodeId {
        &self.peer
    }

    /// The negotiated protocol version + the peer's engine/extension versions.
    pub fn negotiated(&self) -> &Negotiated {
        &self.negotiated
    }

    /// Open a new bidirectional stream (control or bulk).
    pub async fn open_bi(&self) -> Result<(SendStream, RecvStream)> {
        self.connection
            .open_bi()
            .await
            .map_err(|e| TransportError::Stream(e.to_string()))
    }

    /// Accept the next inbound bidirectional stream from the peer.
    pub async fn accept_bi(&self) -> Result<(SendStream, RecvStream)> {
        self.connection
            .accept_bi()
            .await
            .map_err(|e| TransportError::Stream(e.to_string()))
    }

    /// Open a new unidirectional stream (used for parallel bulk result transfer).
    pub async fn open_uni(&self) -> Result<SendStream> {
        self.connection
            .open_uni()
            .await
            .map_err(|e| TransportError::Stream(e.to_string()))
    }

    /// Accept the next inbound unidirectional stream from the peer.
    pub async fn accept_uni(&self) -> Result<RecvStream> {
        self.connection
            .accept_uni()
            .await
            .map_err(|e| TransportError::Stream(e.to_string()))
    }

    /// Send a datagram (cheap, unreliable — used for gossip/heartbeats).
    pub fn send_datagram(&self, data: Vec<u8>) -> Result<()> {
        self.connection
            .send_datagram(data.into())
            .map_err(|e| TransportError::Stream(e.to_string()))
    }

    pub async fn read_datagram(&self) -> Result<Vec<u8>> {
        self.connection
            .read_datagram()
            .await
            .map(|b| b.to_vec())
            .map_err(|e| TransportError::Stream(e.to_string()))
    }

    /// Close the whole connection (e.g. fatal error).
    pub fn close(&self, code: u32, reason: &[u8]) {
        self.connection.close(VarInt::from_u32(code), reason);
    }
}

/// Build a Quinn `TransportConfig` from `NetworkConfig` + `[transport.quic]`
/// tuning. Every knob is configured here; no magic numbers.
fn build_transport_config(net: &NetworkConfig, quic: &QuicTuningConfig) -> Result<TransportConfig> {
    let mut tc = TransportConfig::default();
    let idle = Duration::from_millis(net.idle_timeout_ms)
        .try_into()
        .map_err(|e| TransportError::Endpoint(format!("idle timeout: {e}")))?;
    tc.max_idle_timeout(Some(idle));
    tc.keep_alive_interval(Some(Duration::from_millis(net.keepalive_ms)));
    tc.max_concurrent_bidi_streams(VarInt::from_u32(net.max_concurrent_bidi_streams));
    tc.max_concurrent_uni_streams(VarInt::from_u32(quic.max_concurrent_uni_streams));

    // Flow-control windows: BDP target (if enabled) > explicit override > network.
    let (stream_rwnd, conn_rwnd, send_wnd) = quic.effective_windows(net);
    tc.stream_receive_window(
        VarInt::from_u64(stream_rwnd)
            .map_err(|e| TransportError::Endpoint(format!("stream window: {e}")))?,
    );
    tc.receive_window(
        VarInt::from_u64(conn_rwnd)
            .map_err(|e| TransportError::Endpoint(format!("recv window: {e}")))?,
    );
    tc.send_window(send_wnd);

    // UDP Generic Segmentation Offload (GSO): the biggest TX throughput lever.
    // quinn-udp auto-disables it when the platform lacks support. GRO on receive
    // is auto-detected by quinn-udp and has no per-endpoint toggle in this
    // version, so `quic.gro` is advisory only.
    tc.enable_segmentation_offload(quic.gso);

    // Congestion control. Pacing is always applied internally by Quinn for the
    // chosen controller; `quic.pacing` is advisory in this version.
    match quic.congestion {
        CongestionAlgo::Bbr => {
            tc.congestion_controller_factory(Arc::new(BbrConfig::default()));
        }
        CongestionAlgo::Cubic => {
            tc.congestion_controller_factory(Arc::new(CubicConfig::default()));
        }
        CongestionAlgo::NewReno => {
            tc.congestion_controller_factory(Arc::new(NewRenoConfig::default()));
        }
    }
    Ok(tc)
}

/// Extract the authenticated peer node id from a connection's pinned cert chain.
fn peer_node_id(connection: &quinn::Connection) -> Result<NodeId> {
    let identity = connection
        .peer_identity()
        .ok_or_else(|| TransportError::Connection("peer presented no identity".into()))?;
    let chain = identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map_err(|_| TransportError::Connection("unexpected peer identity type".into()))?;
    let end_entity = chain
        .first()
        .ok_or_else(|| TransportError::Connection("empty peer cert chain".into()))?;
    node_id_from_cert(end_entity).map_err(TransportError::Connection)
}

/// Install the ring crypto provider as the process default exactly once. Safe to
/// call repeatedly; ignores the error if another provider is already installed.
fn install_crypto_provider() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

// ---------------------------------------------------------------------------
// Length-prefixed framing of `Wire` messages over a QUIC stream.
// ---------------------------------------------------------------------------

/// Write one framed [`Wire`] message: 4-byte big-endian length + 2-byte schema
/// tag + JSON payload. The schema tag versions the wire format on every message.
pub async fn write_msg(send: &mut SendStream, msg: &Wire) -> Result<()> {
    let payload = p2p_proto::to_bytes(msg)?;
    let body_len = payload.len() + 2;
    if body_len > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge(body_len, MAX_FRAME_BYTES));
    }
    send.write_all(&(body_len as u32).to_be_bytes())
        .await
        .map_err(|e| TransportError::Stream(e.to_string()))?;
    send.write_all(&p2p_proto::SCHEMA_VERSION.to_be_bytes())
        .await
        .map_err(|e| TransportError::Stream(e.to_string()))?;
    send.write_all(&payload)
        .await
        .map_err(|e| TransportError::Stream(e.to_string()))?;
    Ok(())
}

/// Read one framed [`Wire`] message written by [`write_msg`], verifying the
/// schema tag (defense-in-depth against an incompatible peer that slipped past
/// ALPN/handshake).
pub async fn read_msg(recv: &mut RecvStream) -> Result<Wire> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf)
        .await
        .map_err(|e| TransportError::Stream(e.to_string()))?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(TransportError::FrameTooLarge(len, MAX_FRAME_BYTES));
    }
    if len < 2 {
        return Err(TransportError::Stream("frame too short for schema tag".into()));
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body)
        .await
        .map_err(|e| TransportError::Stream(e.to_string()))?;
    let schema = u16::from_be_bytes([body[0], body[1]]);
    if schema != p2p_proto::SCHEMA_VERSION {
        return Err(TransportError::SchemaMismatch {
            got: schema,
            expected: p2p_proto::SCHEMA_VERSION,
        });
    }
    Ok(p2p_proto::from_bytes(&body[2..])?)
}

/// Convenience: send one request and read one response on a fresh bi-stream.
pub async fn request_response(conn: &Conn, request: &Wire) -> Result<Wire> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_msg(&mut send, request).await?;
    send.finish().map_err(|e| TransportError::Stream(e.to_string()))?;
    read_msg(&mut recv).await
}
