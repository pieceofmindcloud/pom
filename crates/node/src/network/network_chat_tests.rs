use super::network_chat::{ChatAction, ChatQueue, NetworkChatLimits};
use crate::network::Network;

#[test]
fn action_rejects_text_above_the_protocol_limit() {
    let action = ChatAction::text_message(
        "network",
        "node-a",
        "x".repeat(NetworkChatLimits::TEXT_BYTES + 1),
    );
    assert_eq!(action.validate(), Err("text_too_large"));
}

#[test]
fn bounded_queue_drops_without_waiting() {
    let (queue, mut receiver) = ChatQueue::with_capacity(1);
    let first = ChatAction::text_message("network", "node-a", "first");
    let second = ChatAction::text_message("network", "node-a", "second");

    assert!(queue.try_push(first));
    assert!(!queue.try_push(second));
    assert_eq!(queue.dropped(), 1);
    assert_eq!(receiver.try_recv().unwrap().text(), Some("first"));
}

#[test]
fn action_keeps_node_and_room_identifiers_explicit() {
    let action = ChatAction::text_message("network", "node-a", "hello");
    assert_eq!(action.room_id(), "network");
    assert_eq!(action.sender_node_id(), "node-a");
}

#[test]
fn persisted_history_keeps_sender_and_file_bytes() {
    let path = std::env::temp_dir().join(format!("pom-network-chat-{}.db", uuid::Uuid::new_v4()));
    let db = crate::db::PomDb::open(&path).unwrap();
    let action = ChatAction::File {
        message_id: "message-1".into(),
        room_id: "network".into(),
        sender_node_id: "node-a".into(),
        recipient_node_id: None,
        file_name: "notes.txt".into(),
        mime_type: "text/plain".into(),
        data: b"hello".to_vec(),
        sent_at: chrono::Utc::now(),
    };

    assert!(db.append_network_chat(&action).unwrap());
    let messages = db.network_chat_messages("network", 10).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].sender_node_id, "node-a");
    assert_eq!(messages[0].file_name.as_deref(), Some("notes.txt"));
    assert_eq!(messages[0].file_data, b"hello");
    assert_eq!(
        db.network_chat_file("message-1").unwrap(),
        Some((b"hello".to_vec(), "text/plain".into()))
    );
    assert!(!db.append_network_chat(&action).unwrap());
    let _ = std::fs::remove_file(path);
}

#[test]
fn network_chat_policy_never_adopts_an_unauthenticated_announce() {
    assert_eq!(
        super::network_chat_policy::on_announce(false, true, false, "admin"),
        super::network_chat_policy::PolicyOutcome::Unchanged
    );
    assert_eq!(
        super::network_chat_policy::adopt_or_keep(true, Err(anyhow::anyhow!("offline"))),
        super::network_chat_policy::PolicyOutcome::Unchanged
    );
}

#[tokio::test]
async fn network_chat_probe_only_accepts_the_configured_principal_identity() {
    let network = Network::testkit("local".into(), "pk-local".into(), vec![], None, None)
        .await
        .unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    network.set_network_chat_probe(sender);
    network.set_policy_principal_pubkey("authority-primary");

    network.trigger_network_chat_probe("secondary", "authority-secondary", true, false);
    assert!(receiver.try_recv().is_err());

    network.trigger_network_chat_probe("primary", "authority-primary", true, false);
    assert_eq!(receiver.try_recv().unwrap(), ("primary".into(), false));
}

#[tokio::test]
async fn quic_policy_probe_only_accepts_the_configured_principal_identity() {
    let network = Network::testkit("local".into(), "pk-local".into(), vec![], None, None)
        .await
        .unwrap();
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    network.set_quic_probe(sender);
    network.set_policy_principal_pubkey("authority-primary");

    // A secondary admin may announce the same capability, but cannot
    // revert the preference the principal has just written.
    network.trigger_quic_probe("secondary", "authority-secondary", true, false);
    assert!(receiver.try_recv().is_err());

    network.trigger_quic_probe("primary", "authority-primary", true, false);
    assert_eq!(receiver.try_recv().unwrap(), ("primary".into(), false));
}

#[tokio::test]
async fn principal_rejects_a_spoofed_or_blocked_sender_before_relay() {
    let path =
        std::env::temp_dir().join(format!("pom-network-chat-auth-{}.db", uuid::Uuid::new_v4()));
    let database = std::sync::Arc::new(crate::db::PomDb::open(&path).unwrap());
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "profile": "admin",
        "connection_key": "cluster-key",
    }))
    .unwrap();
    let auth = std::sync::Arc::new(tokio::sync::RwLock::new(config));
    let (chat, _receiver) = super::network_chat::NetworkChat::new(database, auth.clone(), true);
    let action = ChatAction::text_message("network", "node-a", "hello");

    assert!(chat.validate_original(&action, "node-a").await.is_ok());
    // The transport's node is the sender; the JSON cannot name another.
    assert!(chat.validate_original(&action, "node-b").await.is_err());
    auth.write().await.blocked_nodes.push("node-a".into());
    assert!(chat.validate_original(&action, "node-a").await.is_err());
    let _ = std::fs::remove_file(path);
}

#[test]
fn chat_pressure_is_try_send_only_and_does_not_wait() {
    let (queue, _receiver) = ChatQueue::with_capacity(32);
    let started = std::time::Instant::now();
    for _ in 0..50_000 {
        let _ = queue.try_push(ChatAction::text_message("network", "node-a", "load"));
    }
    assert!(
        started.elapsed() < std::time::Duration::from_millis(250),
        "Chat pressure must drop synchronously, not wait for inference or persistence"
    );
    assert!(queue.dropped() > 49_000);
}

#[tokio::test]
async fn plugin_notices_are_validated_kept_in_memory_and_never_persisted() {
    let path = std::env::temp_dir().join(format!(
        "pom-network-chat-notice-{}.db",
        uuid::Uuid::new_v4()
    ));
    let database = std::sync::Arc::new(crate::db::PomDb::open(&path).unwrap());
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "profile": "admin",
        "connection_key": "cluster-key",
    }))
    .unwrap();
    let auth = std::sync::Arc::new(tokio::sync::RwLock::new(config));
    let (chat, _receiver) = super::network_chat::NetworkChat::new(database.clone(), auth, true);
    let notice = |message_id: &str, plugin_code: &str, level: &str| ChatAction::PluginNotice {
        message_id: message_id.into(),
        room_id: "network".into(),
        sender_node_id: "node-a".into(),
        recipient_node_id: None,
        plugin_code: plugin_code.into(),
        title: "Deploy pronto".into(),
        body: "The model was deployed.".into(),
        level: level.into(),
        sent_at: chrono::Utc::now(),
    };

    assert!(notice("n0", "Base", "info").validate().is_err());
    assert!(notice("n0", "base", "loud").validate().is_err());
    assert!(chat.accept(notice("n1", "base", "success")).unwrap());
    // A retransmission of the same notice does not duplicate it.
    assert!(chat.accept(notice("n1", "base", "success")).unwrap());
    let notices = chat.notice_snapshot();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind, "plugin_notice");
    assert_eq!(notices[0].plugin_code.as_deref(), Some("base"));
    assert_eq!(notices[0].title.as_deref(), Some("Deploy pronto"));
    assert!(database
        .network_chat_messages("network", 10)
        .unwrap()
        .is_empty());
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn lobby_presence_is_kept_in_memory_and_never_vouched_for_by_an_api_key() {
    let path = std::env::temp_dir().join(format!(
        "pom-network-chat-lobby-{}.db",
        uuid::Uuid::new_v4()
    ));
    let database = std::sync::Arc::new(crate::db::PomDb::open(&path).unwrap());
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "profile": "admin",
        "connection_key": "cluster-key",
    }))
    .unwrap();
    let auth = std::sync::Arc::new(tokio::sync::RwLock::new(config));
    let (chat, _receiver) =
        super::network_chat::NetworkChat::new(database.clone(), auth.clone(), true);
    let presence = |room: &str| ChatAction::LobbyPresence {
        message_id: "p1".into(),
        room_id: room.into(),
        sender_node_id: "candidate".into(),
        recipient_node_id: None,
        card: crate::network_lobby::LobbyCard {
            hostname: "h".repeat(4_000),
            ram_total_mb: 64_000,
            ..Default::default()
        },
        sent_at: chrono::Utc::now(),
    };

    // Presence only exists in the lobby room.
    assert_eq!(presence("network").validate(), Err("lobby_room_invalid"));
    assert!(chat.accept(presence("lobby")).unwrap());
    let guests = chat.lobby_guests().snapshot();
    assert_eq!(guests.len(), 1);
    assert_eq!(guests[0].0, "candidate");
    // The relayed card is clamped again on arrival.
    assert_eq!(
        guests[0].1.hostname.len(),
        crate::network_lobby::LobbyLimits::CARD_TEXT_BYTES
    );
    assert!(database
        .network_chat_messages("lobby", 10)
        .unwrap()
        .is_empty());

    // A member cannot originate a presence, whatever its key.
    let error = chat
        .validate_original(&presence("lobby"), "candidate")
        .await
        .unwrap_err();
    assert_eq!(error.to_string(), "lobby_presence_not_allowed");

    // The lobby follows discovery, with no switch of its own. A hidden
    // network answers too: it is reached by whoever holds its trust document.
    assert!(!chat.lobby_enabled().await);
    assert!(!chat.auto_approves().await);
    auth.write().await.network.access_policy = crate::config::NetworkAccessPolicy::AutoApprove;
    assert!(chat.auto_approves().await);
    auth.write().await.network.visibility = crate::config::NetworkVisibility::Private;
    assert!(chat.lobby_enabled().await);
    auth.write().await.network.visibility = crate::config::NetworkVisibility::Public;
    assert!(chat.lobby_enabled().await);
    // The network Chat off closes the conversation, not the lobby: it still
    // takes admission requests.
    chat.set_enabled(false);
    assert!(chat.lobby_enabled().await);
    auth.write().await.network.visibility = crate::config::NetworkVisibility::Local;
    assert!(!chat.lobby_enabled().await);
    let _ = std::fs::remove_file(path);
}

#[tokio::test]
async fn the_member_list_takes_admitted_peers_and_keeps_out_one_that_just_left() {
    let path = std::env::temp_dir().join(format!(
        "pom-network-chat-members-{}.db",
        uuid::Uuid::new_v4()
    ));
    let database = std::sync::Arc::new(crate::db::PomDb::open(&path).unwrap());
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "profile": "admin",
        "connection_key": "cluster-key",
    }))
    .unwrap();
    let auth = std::sync::Arc::new(tokio::sync::RwLock::new(config));
    let (chat, _receiver) = super::network_chat::NetworkChat::new(database.clone(), auth, true);
    use crate::config::EnrollmentRole::{Visitor, Worker};
    use crate::network_lobby::AdmissionStatus;

    // A member that joined before admission requests existed has no record.
    assert!(chat.admission_of("old-member").is_none());
    assert!(chat.record_member("old-member", "antigo", Worker));
    let record = chat.admission_of("old-member").unwrap();
    assert_eq!(record.status, AdmissionStatus::Approved);
    assert_eq!(record.card.hostname, "antigo");
    // Recording again changes nothing, and never overrides a decision.
    assert!(!chat.record_member("old-member", "outro-nome", Visitor));
    assert_eq!(chat.admission_of("old-member").unwrap().role, Worker);
    chat.request_admission("candidate", Default::default(), Visitor)
        .unwrap();
    assert!(!chat.record_member("candidate", "x", Worker));
    assert_eq!(
        chat.admission_of("candidate").unwrap().status,
        AdmissionStatus::Pending
    );

    // Leaving takes the node off the list, and the peer table still showing
    // it for a few seconds does not put it back.
    chat.member_departed("old-member");
    assert!(chat.admission_of("old-member").is_none());
    assert!(!chat.record_member("old-member", "antigo", Worker));
    assert!(chat.admission_of("old-member").is_none());

    // The list survives a restart.
    let (reloaded, _receiver) = super::network_chat::NetworkChat::new(
        database,
        std::sync::Arc::new(tokio::sync::RwLock::new(
            serde_json::from_value(serde_json::json!({ "profile": "admin" })).unwrap(),
        )),
        true,
    );
    reloaded.load_admissions();
    assert!(reloaded.admission_of("candidate").is_some());
    let _ = std::fs::remove_file(path);
}

#[test]
fn a_lobby_thread_names_its_candidate() {
    use crate::network_lobby::{is_lobby_room, thread_candidate, thread_room, LOBBY_ROOM};
    assert_eq!(thread_room("cand"), "lobby/cand");
    assert_eq!(thread_candidate("lobby/cand"), Some("cand"));
    assert_eq!(thread_candidate("lobby/"), None);
    assert_eq!(thread_candidate("network"), None);
    assert!(is_lobby_room(LOBBY_ROOM) && is_lobby_room("lobby/cand"));
    assert!(!is_lobby_room("network"));
}

#[tokio::test]
async fn deciding_a_candidate_erases_its_thread_and_queues_it_for_the_admins() {
    let path = std::env::temp_dir().join(format!(
        "pom-network-chat-threads-{}.db",
        uuid::Uuid::new_v4()
    ));
    let database = std::sync::Arc::new(crate::db::PomDb::open(&path).unwrap());
    let config: crate::config::Config = serde_json::from_value(serde_json::json!({
        "profile": "admin",
        "connection_key": "cluster-key",
    }))
    .unwrap();
    let auth = std::sync::Arc::new(tokio::sync::RwLock::new(config));
    let (chat, _receiver) =
        super::network_chat::NetworkChat::new(database.clone(), auth.clone(), true);
    for (candidate, text) in [("cand-a", "hello from a"), ("cand-b", "hello from b")] {
        let action = ChatAction::text_message(
            crate::network_lobby::thread_room(candidate),
            candidate,
            text,
        );
        assert!(database.append_network_chat(&action).unwrap());
    }
    // Each candidate's words stay in its own thread.
    let a = database.network_chat_messages("lobby/cand-a", 10).unwrap();
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].text.as_deref(), Some("hello from a"));
    assert_eq!(chat.lobby_threads(10).unwrap().len(), 2);

    chat.request_admission(
        "cand-a",
        Default::default(),
        crate::config::EnrollmentRole::Visitor,
    )
    .unwrap();
    chat.request_admission(
        "cand-b",
        Default::default(),
        crate::config::EnrollmentRole::Visitor,
    )
    .unwrap();
    // Approved or declined, the thread goes, and the candidate leaves the lobby.
    chat.decide_admission(
        "cand-b",
        Some(crate::network_lobby::AdmissionStatus::Rejected),
    );
    chat.decide_admission(
        "cand-a",
        Some(crate::network_lobby::AdmissionStatus::Approved),
    );
    for room in ["lobby/cand-a", "lobby/cand-b"] {
        assert!(database.network_chat_messages(room, 10).unwrap().is_empty());
    }
    assert_eq!(
        chat.take_closed_threads(),
        vec!["lobby/cand-b".to_string(), "lobby/cand-a".to_string()]
    );
    let decided = chat.decided_candidates();
    assert!(decided.contains("cand-a") && decided.contains("cand-b"));
    assert!(chat.take_closed_threads().is_empty());

    // The closure the principal relays erases the thread on another admin.
    let other = ChatAction::text_message("lobby/cand-b", "cand-b", "still here");
    assert!(database.append_network_chat(&other).unwrap());
    let closed = ChatAction::LobbyClosed {
        message_id: "closed-1".into(),
        room_id: "lobby/cand-b".into(),
        sender_node_id: "principal".into(),
        recipient_node_id: None,
        sent_at: chrono::Utc::now(),
    };
    assert!(chat.accept(closed.clone()).unwrap());
    assert!(chat.lobby_threads(10).unwrap().is_empty());
    // No member originates one, and the bare lobby room takes no text.
    let error = chat.validate_original(&closed, "principal").await.unwrap_err();
    assert_eq!(error.to_string(), "lobby_closed_not_allowed");
    let bare = ChatAction::text_message(crate::network_lobby::LOBBY_ROOM, "member", "hi");
    let error = chat.validate_original(&bare, "member").await.unwrap_err();
    assert_eq!(error.to_string(), "lobby_room_invalid");
    let _ = std::fs::remove_file(path);
}

#[test]
fn threads_nobody_wrote_in_for_a_day_are_purged() {
    let path = std::env::temp_dir().join(format!(
        "pom-network-chat-purge-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db = crate::db::PomDb::open(&path).unwrap();
    for room in ["lobby/old", "lobby/fresh", "lobby", "network"] {
        let action = ChatAction::text_message(room, "someone", "text");
        assert!(db.append_network_chat(&action).unwrap());
    }
    // Every message is fresh: only the shared lobby room goes.
    db.purge_lobby_threads(24 * 60 * 60).unwrap();
    assert!(db.network_chat_messages("lobby", 10).unwrap().is_empty());
    assert_eq!(db.network_chat_lobby_threads(10).unwrap().len(), 2);
    // With no time to live, every thread is stale; the members' room stays.
    std::thread::sleep(std::time::Duration::from_millis(5));
    db.purge_lobby_threads(0).unwrap();
    assert!(db.network_chat_lobby_threads(10).unwrap().is_empty());
    assert_eq!(db.network_chat_messages("network", 10).unwrap().len(), 1);
    let _ = std::fs::remove_file(path);
}
