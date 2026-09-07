use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

/// The durable capability profile of one node.
///
/// `Role` remains the cluster topology and authentication concept used by the
/// existing connection-key protocol. This enum is the product-facing profile
/// and is persisted in `pom.json`, so the browser cannot choose it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeProfile {
    Admin,
    Node,
    Infer,
}

impl fmt::Display for NodeProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Admin => "admin",
            Self::Node => "node",
            Self::Infer => "infer",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NetworkVisibility {
    Local,
    Cluster,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProfileAction {
    ViewNetwork,
    ViewNetworkPeers,
    SelfPromoteToNode,
    PromoteNodeToAdmin,
    ManageDeployment,
}

/// Signed command sent by an administrator node to the node being promoted.
/// The shared connection key authenticates the transport, while this
/// signature proves that the sender is the principal peer known by the target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PromoteNodeToAdminCommand {
    pub(crate) actor_node_id: String,
    pub(crate) actor_pubkey: String,
    pub(crate) target_node_id: String,
    pub(crate) signature: String,
}

fn promotion_message(actor_node_id: &str, target_node_id: &str) -> Vec<u8> {
    format!("pom/profile/promote-node-to-admin/v1\n{actor_node_id}\n{target_node_id}").into_bytes()
}

pub(crate) fn signed_promotion_command(
    signing_key: &SigningKey,
    actor_node_id: &str,
    target_node_id: &str,
) -> PromoteNodeToAdminCommand {
    let signature = signing_key.sign(&promotion_message(actor_node_id, target_node_id));
    PromoteNodeToAdminCommand {
        actor_node_id: actor_node_id.to_string(),
        actor_pubkey: hex::encode(signing_key.verifying_key().as_bytes()),
        target_node_id: target_node_id.to_string(),
        signature: hex::encode(signature.to_bytes()),
    }
}

pub(crate) fn verifies_promotion_command(command: &PromoteNodeToAdminCommand) -> bool {
    let Ok(pubkey_bytes) = <[u8; 32]>::try_from(
        hex::decode(&command.actor_pubkey)
            .unwrap_or_default()
            .as_slice(),
    ) else {
        return false;
    };
    let Ok(signature_bytes) = hex::decode(&command.signature) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature_bytes) else {
        return false;
    };
    let Ok(pubkey) = VerifyingKey::from_bytes(&pubkey_bytes) else {
        return false;
    };
    let expected_node_id = hex::encode(Sha256::digest(pubkey.as_bytes()));
    expected_node_id == command.actor_node_id
        && pubkey
            .verify(
                &promotion_message(&command.actor_node_id, &command.target_node_id),
                &signature,
            )
            .is_ok()
}

impl NodeProfile {
    pub(crate) const fn network_visibility(self) -> NetworkVisibility {
        match self {
            Self::Infer => NetworkVisibility::Local,
            Self::Admin | Self::Node => NetworkVisibility::Cluster,
        }
    }

    pub(crate) const fn allows(self, action: ProfileAction) -> bool {
        match action {
            ProfileAction::ViewNetwork => true,
            ProfileAction::ViewNetworkPeers => {
                matches!(self.network_visibility(), NetworkVisibility::Cluster)
            }
            ProfileAction::SelfPromoteToNode => matches!(self, Self::Infer),
            ProfileAction::PromoteNodeToAdmin => matches!(self, Self::Admin),
            ProfileAction::ManageDeployment => matches!(self, Self::Admin),
        }
    }
}

/// Whether a target is in the only state that the administrator promotion can
/// change. Keeping the target check beside the actor policy makes the command
/// reusable by every backend route that exposes profile capabilities.
pub(crate) const fn can_promote_node_to_admin(actor: NodeProfile, target: NodeProfile) -> bool {
    matches!(actor, NodeProfile::Admin) && matches!(target, NodeProfile::Node)
}
