use serde::{Deserialize, Serialize};
use std::fmt;

/// The durable capability profile of one node.
///
/// The profile is persisted in `pom.json`, so the browser cannot choose it.
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

impl Default for NodeProfile {
    fn default() -> Self {
        Self::Node
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
    PromoteNodeToAdmin,
    ManageDeployment,
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
            ProfileAction::PromoteNodeToAdmin => matches!(self, Self::Admin),
            ProfileAction::ManageDeployment => matches!(self, Self::Admin),
        }
    }
}

/// Whether a target is in the only state that the administrator promotion can
/// change. Keeping the target check beside the actor policy makes the command
/// reusable by every backend route that exposes profile capabilities.
#[cfg(test)]
pub(crate) const fn can_promote_node_to_admin(actor: NodeProfile, target: NodeProfile) -> bool {
    matches!(actor, NodeProfile::Admin) && matches!(target, NodeProfile::Node)
}
