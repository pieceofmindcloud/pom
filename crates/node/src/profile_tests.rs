use crate::profile::{NetworkVisibility, NodeProfile, ProfileAction};

#[test]
fn infer_can_request_processing_and_view_only_its_local_network() {
    assert!(NodeProfile::Visitor.allows(ProfileAction::ViewNetwork));
    assert!(!NodeProfile::Visitor.allows(ProfileAction::ViewNetworkPeers));
    assert_eq!(
        NodeProfile::Visitor.network_visibility(),
        NetworkVisibility::Local
    );
}

#[test]
fn only_admin_can_promote_a_node_to_admin() {
    assert!(NodeProfile::Admin.allows(ProfileAction::PromoteNodeToAdmin));
    assert!(crate::profile::can_promote_node_to_admin(
        NodeProfile::Admin,
        NodeProfile::Worker
    ));
    assert!(!crate::profile::can_promote_node_to_admin(
        NodeProfile::Admin,
        NodeProfile::Admin
    ));
    assert!(!NodeProfile::Worker.allows(ProfileAction::PromoteNodeToAdmin));
    assert!(!crate::profile::can_promote_node_to_admin(
        NodeProfile::Worker,
        NodeProfile::Worker
    ));
    assert!(!NodeProfile::Visitor.allows(ProfileAction::PromoteNodeToAdmin));
}

#[test]
fn only_admin_can_manage_deployments() {
    assert!(NodeProfile::Admin.allows(ProfileAction::ManageDeployment));
    assert!(!NodeProfile::Worker.allows(ProfileAction::ManageDeployment));
    assert!(!NodeProfile::Visitor.allows(ProfileAction::ManageDeployment));
}
