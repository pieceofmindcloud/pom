use serde::{Deserialize, Serialize};
use std::fmt;

/// The durable capability profile of one node.
///
/// The profile is persisted in `pom.json`, so the browser cannot choose it.
/// A `Worker` is hardware the admins load models on and run layers with; a
/// `Visitor` is a proxy that only routes inference requests to the workers
/// and never holds a model nor makes an administrative call. `Admin` is a
/// worker whose authority the trust chain confirms - the profile alone
/// grants none (`AdminTrust::effective_authority`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum NodeProfile {
    Admin,
    Worker,
    Visitor,
}

impl fmt::Display for NodeProfile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Admin => "admin",
            Self::Worker => "worker",
            Self::Visitor => "visitor",
        };
        f.write_str(value)
    }
}

impl Default for NodeProfile {
    fn default() -> Self {
        Self::Worker
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
            Self::Visitor => NetworkVisibility::Local,
            Self::Admin | Self::Worker => NetworkVisibility::Cluster,
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
    matches!(actor, NodeProfile::Admin) && matches!(target, NodeProfile::Worker)
}
