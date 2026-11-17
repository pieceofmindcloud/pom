use super::*;
use crate::admin_trust::AdminTrust;

fn principal_of(node_id: &str, key: &SigningKey) -> AuthorityPrincipal {
    AuthorityPrincipal::from_signing_key(node_id, &format!("peer-{node_id}"), key)
}

struct Chain {
    trust: AdminTrust,
    primary_key: SigningKey,
    primary: AuthorityPrincipal,
    secondary_key: SigningKey,
    secondary: AuthorityPrincipal,
}

fn chain() -> Chain {
    let primary_key = SigningKey::from_bytes(&[3; 32]);
    let secondary_key = SigningKey::from_bytes(&[4; 32]);
    let primary = principal_of("primary", &primary_key);
    let secondary = principal_of("secondary", &secondary_key);
    let trust =
        AdminTrust::bootstrap_primary_in_memory("test-cluster", primary.clone(), &primary_key)
            .unwrap();
    let grant = trust
        .signed_secondary_grant(&primary_key, secondary.clone(), &primary.node_id)
        .unwrap();
    trust.record_secondary_grant(grant, &primary_key).unwrap();
    Chain {
        trust,
        primary_key,
        primary,
        secondary_key,
        secondary,
    }
}

const NOW: i64 = 1_800_000_000;

/// Only an admin revokes, an admin is not revoked this way, and a tampered
/// entry is refused.
#[test]
fn a_revocation_holds_only_from_an_admin_against_a_member() {
    let chain = chain();
    let document = chain.trust.document();
    let outsider_key = SigningKey::from_bytes(&[9; 32]);
    let outsider = principal_of("outsider", &outsider_key);
    assert!(revoke(&outsider_key, &outsider, &document, "worker", NOW).is_err());
    assert!(revoke(
        &chain.secondary_key,
        &chain.secondary,
        &document,
        "primary",
        NOW
    )
    .is_err());
    assert!(revoke(
        &chain.primary_key,
        &chain.primary,
        &document,
        "secondary",
        NOW
    )
    .is_err());

    let mut forged = revoke(
        &chain.secondary_key,
        &chain.secondary,
        &document,
        "worker",
        NOW,
    )
    .unwrap();
    forged.sub = "another-worker".into();
    assert!(verify_revocation(&forged, &document).is_err());
}

/// The lists of several admins merge: a forged entry is dropped, a repeat
/// adds nothing, and the latest ban of a member is the one kept.
#[test]
fn revocation_lists_merge_keeping_the_latest_ban() {
    let chain = chain();
    let document = chain.trust.document();
    let first = revoke(&chain.primary_key, &chain.primary, &document, "worker", NOW).unwrap();
    let later = revoke(
        &chain.secondary_key,
        &chain.secondary,
        &document,
        "worker",
        NOW + 5,
    )
    .unwrap();
    let mut forged = first.clone();
    forged.sub = "another-worker".into();

    assert_eq!(
        chain
            .trust
            .merge_revocations(vec![first.clone(), forged])
            .len(),
        1
    );
    assert!(chain.trust.merge_revocations(vec![first]).is_empty());
    assert_eq!(
        chain.trust.merge_revocations(vec![later.clone()]),
        vec![later]
    );
    assert_eq!(chain.trust.revoked_node_at("worker"), Some(NOW + 5));
    assert_eq!(chain.trust.revoked_node_at("another-worker"), None);
    assert_eq!(chain.trust.revocations().len(), 1);
}

#[test]
fn any_admin_signs_a_vouch_every_node_checks_against_the_chain_alone() {
    let chain = chain();
    let document = chain.trust.document();
    for (key, issuer) in [
        (&chain.primary_key, &chain.primary),
        (&chain.secondary_key, &chain.secondary),
    ] {
        let vouch = sign_vouch(
            key,
            issuer,
            &document,
            "worker",
            EnrollmentRole::Worker,
            NOW - 10,
            NOW,
        )
        .unwrap();
        assert_eq!(vouch.sub, "worker");
        assert_eq!(vouch.role, EnrollmentRole::Worker);
        assert_eq!(vouch.admitted_at, NOW - 10);
        assert_eq!(vouch.issued_at, NOW);
        // The other admin checks it knowing nothing about the member.
        verify_vouch(&vouch, &document, NOW).unwrap();
    }
}

#[test]
fn a_vouch_is_bound_to_its_network_its_signature_its_freshness_and_its_chain() {
    let chain = chain();
    let document = chain.trust.document();
    let vouch = sign_vouch(
        &chain.primary_key,
        &chain.primary,
        &document,
        "worker",
        EnrollmentRole::Visitor,
        NOW - 10,
        NOW,
    )
    .unwrap();
    // A vouch of another network is nothing here.
    let mut foreign = vouch.clone();
    foreign.net = "other-cluster".into();
    assert!(verify_vouch(&foreign, &document, NOW).is_err());
    // A forged signature does not hold.
    let mut forged = vouch.clone();
    forged.signature = "00".repeat(64);
    assert!(verify_vouch(&forged, &document, NOW).is_err());
    // One that stopped being renewed ages out; inside the window it holds.
    assert!(verify_vouch(
        &vouch,
        &document,
        NOW + VOUCH_FRESH_SECS + CLOCK_LEEWAY_SECS + 1
    )
    .is_err());
    assert!(verify_vouch(&vouch, &document, NOW + VOUCH_FRESH_SECS - 60).is_ok());
    // An admin of the chain is never vouched.
    assert!(sign_vouch(
        &chain.primary_key,
        &chain.primary,
        &document,
        &chain.primary.node_id,
        EnrollmentRole::Worker,
        NOW - 10,
        NOW,
    )
    .is_err());
    // A vouch signed by a stranger is refused at once.
    let stranger_key = SigningKey::from_bytes(&[9; 32]);
    let stranger = principal_of("stranger", &stranger_key);
    assert!(sign_vouch(
        &stranger_key,
        &stranger,
        &document,
        "worker",
        EnrollmentRole::Worker,
        NOW - 10,
        NOW,
    )
    .is_err());
}

#[test]
fn a_vouch_from_an_admin_the_chain_demotes_holds_nothing() {
    let chain = chain();
    let document = chain.trust.document();
    let vouch = sign_vouch(
        &chain.secondary_key,
        &chain.secondary,
        &document,
        "worker",
        EnrollmentRole::Worker,
        NOW - 10,
        NOW,
    )
    .unwrap();
    verify_vouch(&vouch, &document, NOW).unwrap();
    chain
        .trust
        .demote_secondary(&chain.primary_key, chain.secondary.clone())
        .unwrap();
    // The check is against the chain as it stands now.
    assert!(verify_vouch(&vouch, &chain.trust.document(), NOW).is_err());
}
