use super::local_discovery::{
    LocalAutoDiscoveryMessage, LocalDiscoveryResult, LocalDiscoveryRuntime,
};
use std::net::SocketAddr;
use std::time::Duration;

fn endpoint(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

#[tokio::test]
async fn complete_handshake_delivers_trust_and_connection_key() {
    let admin_socket = tokio::net::UdpSocket::bind(endpoint(0)).await.unwrap();
    let node_socket = tokio::net::UdpSocket::bind(endpoint(0)).await.unwrap();
    let admin = LocalDiscoveryRuntime::new(std::sync::Arc::new(admin_socket), endpoint(0));
    let node = LocalDiscoveryRuntime::new(std::sync::Arc::new(node_socket), endpoint(0));
    admin
        .set_admin_material(true, "{\"anchor\":{}}", "secret-key")
        .await
        .unwrap();

    let request_id = node.begin_search().await.unwrap();
    let probe = node.take_outbound_for_test().await.unwrap();
    let admin_address = admin.local_addr().await;
    admin
        .handle_for_test(probe, endpoint(node.port().await))
        .await;
    let offer = admin.take_outbound_for_test().await.unwrap();
    node.handle_for_test(offer, admin_address).await;
    let proof = node.take_outbound_for_test().await.unwrap();
    admin
        .handle_for_test(proof, endpoint(node.port().await))
        .await;
    let secret = admin.take_outbound_for_test().await.unwrap();
    let admin_status = admin.status().await;
    assert_eq!(admin_status.peers.len(), 1);
    assert_eq!(
        admin_status.peers[0].address,
        endpoint(node.port().await).to_string()
    );
    assert_eq!(admin_status.peers[0].request_id, request_id);
    node.handle_for_test(secret, admin_address).await;

    let result = node.peek_result().await.unwrap();
    assert_eq!(result.request_id, request_id);
    assert_eq!(result.connection_key, "secret-key");
    assert_eq!(result.trust_document, "{\"anchor\":{}}");
}

#[tokio::test]
async fn large_private_trust_documents_are_fragmented_and_reassembled() {
    let admin_socket = tokio::net::UdpSocket::bind(endpoint(0)).await.unwrap();
    let node_socket = tokio::net::UdpSocket::bind(endpoint(0)).await.unwrap();
    let admin = LocalDiscoveryRuntime::new(std::sync::Arc::new(admin_socket), endpoint(0));
    let node = LocalDiscoveryRuntime::new(std::sync::Arc::new(node_socket), endpoint(0));
    let trust_document = format!("{{\"private_setup\":\"{}\"}}", "x".repeat(40_000));

    admin
        .set_admin_material(true, trust_document.clone(), "secret-key")
        .await
        .expect("documentos privados maiores que um datagrama devem ser aceitos");

    let request_id = node.begin_search().await.unwrap();
    let probe = node.take_outbound_for_test().await.unwrap();
    let admin_address = admin.local_addr().await;
    admin
        .handle_for_test(probe, endpoint(node.port().await))
        .await;
    let offer = admin.take_outbound_for_test().await.unwrap();
    assert!(serialized_message_size(&offer) <= 1200);
    node.handle_for_test(offer, admin_address).await;
    let proof = node.take_outbound_for_test().await.unwrap();
    admin
        .handle_for_test(proof, endpoint(node.port().await))
        .await;

    let mut secret_messages = Vec::new();
    while let Some(message) = admin.take_outbound_for_test().await {
        assert!(serialized_message_size(&message) <= 1200);
        secret_messages.push(message);
    }
    assert!(secret_messages.len() > 1);
    for message in secret_messages {
        node.handle_for_test(message, admin_address).await;
    }

    let result = node.peek_result().await.expect("handshake completo");
    assert_eq!(result.request_id, request_id);
    assert_eq!(result.connection_key, "secret-key");
    assert_eq!(result.trust_document, trust_document);
}

#[tokio::test]
async fn public_messages_never_contain_connection_key() {
    let admin = LocalDiscoveryRuntime::test_memory();
    admin
        .set_admin_material(true, "trust-document", "do-not-leak")
        .await
        .unwrap();
    let _ = admin.begin_search().await.unwrap();
    let probe = admin.take_outbound_for_test().await.unwrap();
    let encoded = serde_json::to_string(&probe).unwrap();
    assert!(!encoded.contains("do-not-leak"));

    let passive = LocalDiscoveryRuntime::test_memory();
    passive.handle_for_test(probe, endpoint(42777)).await;
    assert!(passive.take_outbound_for_test().await.is_none());
}

#[tokio::test]
async fn admin_requires_proof_before_emitting_secret() {
    let admin = LocalDiscoveryRuntime::test_memory();
    admin
        .set_admin_material(true, "trust-document", "secret-key")
        .await
        .unwrap();
    let node = LocalDiscoveryRuntime::test_memory();
    node.begin_search().await.unwrap();
    let probe = node.take_outbound_for_test().await.unwrap();
    admin.handle_for_test(probe, endpoint(42777)).await;
    let offer = admin.take_outbound_for_test().await.unwrap();
    let LocalAutoDiscoveryMessage::Offer {
        request_id,
        admin_ephemeral_public,
        admin_nonce,
        ..
    } = offer
    else {
        panic!("expected offer");
    };
    let forged = LocalAutoDiscoveryMessage::Proof {
        version: 1,
        request_id,
        node_ephemeral_public: "not-a-key".into(),
        node_nonce: "not-a-nonce".into(),
        admin_ephemeral_public,
        admin_nonce,
        proof: "forged".into(),
    };
    admin.handle_for_test(forged, endpoint(42777)).await;
    assert!(admin.take_outbound_for_test().await.is_none());
    assert!(admin.status().await.peers.is_empty());
}

#[tokio::test]
async fn expired_handshake_does_not_deliver_secret() {
    let runtime = LocalDiscoveryRuntime::test_memory();
    runtime.set_session_ttl(Duration::ZERO).await;
    runtime
        .set_admin_material(true, "trust-document", "secret-key")
        .await
        .unwrap();
    let node = LocalDiscoveryRuntime::test_memory();
    node.begin_search().await.unwrap();
    let probe = node.take_outbound_for_test().await.unwrap();
    runtime.handle_for_test(probe, endpoint(42777)).await;
    let _offer = runtime.take_outbound_for_test().await;
    assert!(runtime.take_outbound_for_test().await.is_none());
}

#[allow(dead_code)]
fn assert_result_is_not_secretless(result: LocalDiscoveryResult) {
    assert!(!result.connection_key.is_empty());
}

fn serialized_message_size(message: &LocalAutoDiscoveryMessage) -> usize {
    serde_json::to_vec(&crate::network::P2PMessage::LocalAutoDiscovery {
        message: message.clone(),
    })
    .unwrap()
    .len()
}
