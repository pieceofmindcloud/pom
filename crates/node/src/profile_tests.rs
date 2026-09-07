use crate::profile::{NetworkVisibility, NodeProfile, ProfileAction};

#[test]
fn infer_can_self_promote_and_view_only_its_local_network() {
    assert!(NodeProfile::Infer.allows(ProfileAction::SelfPromoteToNode));
    assert!(NodeProfile::Infer.allows(ProfileAction::ViewNetwork));
    assert!(!NodeProfile::Infer.allows(ProfileAction::ViewNetworkPeers));
    assert_eq!(
        NodeProfile::Infer.network_visibility(),
        NetworkVisibility::Local
    );
}

#[test]
fn only_admin_can_promote_a_node_to_admin() {
    assert!(NodeProfile::Admin.allows(ProfileAction::PromoteNodeToAdmin));
    assert!(crate::profile::can_promote_node_to_admin(
        NodeProfile::Admin,
        NodeProfile::Node
    ));
    assert!(!crate::profile::can_promote_node_to_admin(
        NodeProfile::Admin,
        NodeProfile::Admin
    ));
    assert!(!NodeProfile::Node.allows(ProfileAction::PromoteNodeToAdmin));
    assert!(!crate::profile::can_promote_node_to_admin(
        NodeProfile::Node,
        NodeProfile::Node
    ));
    assert!(!NodeProfile::Infer.allows(ProfileAction::PromoteNodeToAdmin));
}
