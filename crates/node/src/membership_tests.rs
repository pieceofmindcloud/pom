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
    let trust = AdminTrust::bootstrap_primary_in_memory("test-cluster", primary.clone(), &primary_key)
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

#[test]
fn any_admin_signs_a_token_every_member_checks_against_the_chain_alone() {
    let chain = chain();
    let document = chain.trust.document();
    for (key, issuer) in [
        (&chain.primary_key, &chain.primary),
        (&chain.secondary_key, &chain.secondary),
    ] {
        let token = issue(key, issuer, &document, "visitor", "visitor-key", EnrollmentRole::Visitor, NOW)
            .unwrap();
        assert_eq!(
            role_of(Some(&token), "visitor", "visitor-key", &document, NOW + 60),
            Some(EnrollmentRole::Visitor)
        );
    }
}

#[test]
fn a_token_holds_only_for_its_own_key() {
    let chain = chain();
    let document = chain.trust.document();
    let token = issue(&chain.primary_key, &chain.primary, &document, "worker", "worker-key", EnrollmentRole::Worker, NOW)
        .unwrap();
    assert_eq!(role_of(Some(&token), "worker", "another-key", &document, NOW), None);
    assert_eq!(role_of(Some(&token), "another-node", "worker-key", &document, NOW), None);
}

#[test]
fn a_token_expires() {
    let chain = chain();
    let document = chain.trust.document();
    let token = issue(&chain.primary_key, &chain.primary, &document, "worker", "worker-key", EnrollmentRole::Worker, NOW)
        .unwrap();
    assert!(verify(&token, &document, NOW + TOKEN_TTL_SECS + CLOCK_LEEWAY_SECS).is_ok());
    assert!(verify(&token, &document, NOW + TOKEN_TTL_SECS + CLOCK_LEEWAY_SECS + 1).is_err());
    assert!(verify(&token, &document, NOW - CLOCK_LEEWAY_SECS - 1).is_err(), "issued in the future");
    assert_eq!(expires_at(&token), Some(NOW + TOKEN_TTL_SECS));
}

#[test]
fn only_an_admin_issues_and_a_demoted_one_stops_counting() {
    let chain = chain();
    let document = chain.trust.document();
    let stranger_key = SigningKey::from_bytes(&[9; 32]);
    let stranger = principal_of("stranger", &stranger_key);
    assert!(issue(&stranger_key, &stranger, &document, "v", "k", EnrollmentRole::Worker, NOW).is_err());
    assert!(
        issue(&stranger_key, &chain.primary, &document, "v", "k", EnrollmentRole::Worker, NOW).is_err(),
        "the key must be the issuer's"
    );

    let token = issue(&chain.secondary_key, &chain.secondary, &document, "v", "k", EnrollmentRole::Worker, NOW)
        .unwrap();
    chain
        .trust
        .demote_secondary(&chain.primary_key, chain.secondary.clone())
        .unwrap();
    assert!(verify(&token, &chain.trust.document(), NOW).is_err());
}

#[test]
fn a_tampered_token_is_refused() {
    let chain = chain();
    let document = chain.trust.document();
    let token = issue(&chain.primary_key, &chain.primary, &document, "v", "k", EnrollmentRole::Visitor, NOW)
        .unwrap();
    let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
    let mut claims: MembershipClaims =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&parts[1]).unwrap()).unwrap();
    claims.role = EnrollmentRole::Worker;
    parts[1] = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).unwrap());
    assert!(verify(&parts.join("."), &document, NOW).is_err());

    let none_header = format!(
        "{}.{}.{}",
        URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#),
        token.split('.').nth(1).unwrap(),
        token.split('.').nth(2).unwrap()
    );
    assert!(verify(&none_header, &document, NOW).is_err());
}

#[test]
fn a_token_of_another_network_is_nothing() {
    let chain = chain();
    let token = issue(&chain.primary_key, &chain.primary, &chain.trust.document(), "v", "k", EnrollmentRole::Worker, NOW)
        .unwrap();
    let other_key = SigningKey::from_bytes(&[3; 32]);
    let other = AdminTrust::bootstrap_primary_in_memory(
        "other-cluster",
        principal_of("primary", &other_key),
        &other_key,
    )
    .unwrap();
    assert!(verify(&token, &other.document(), NOW).is_err());
}
