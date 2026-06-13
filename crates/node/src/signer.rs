//! Bridges the transport's [`NodeIdentity`] to the trust crate's [`Signer`]
//! trait, so trust stays independent of the networking layer.

use p2p_proto::NodeId;
use p2p_transport::NodeIdentity;
use p2p_trust::Signer;

/// Newtype wrapper implementing [`p2p_trust::Signer`] for a [`NodeIdentity`].
pub struct IdentitySigner<'a>(pub &'a NodeIdentity);

impl Signer for IdentitySigner<'_> {
    fn sign_bytes(&self, msg: &[u8]) -> [u8; 64] {
        self.0.sign(msg)
    }
    fn public_key(&self) -> [u8; 32] {
        self.0.public_key_bytes()
    }
    fn node_id(&self) -> NodeId {
        self.0.node_id().clone()
    }
}
