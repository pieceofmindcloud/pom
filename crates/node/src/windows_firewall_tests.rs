use super::windows_firewall::{
    desired_rules, should_auto_request, FirewallProtocol, SetupDecision, SetupRecord,
};

#[test]
fn firewall_rules_use_the_effective_port_and_both_protocols_of_the_p2p_port() {
    let rules = desired_rules(42778).unwrap();

    assert_eq!(
        rules
            .iter()
            .map(|rule| (rule.protocol, rule.port))
            .collect::<Vec<_>>(),
        vec![
            (FirewallProtocol::Udp, 42778),
            (FirewallProtocol::Udp, 43778),
            (FirewallProtocol::Udp, 44778),
            (FirewallProtocol::Udp, 52778),
            (FirewallProtocol::Tcp, 52778),
        ]
    );
}

#[test]
fn a_firewall_rule_refuses_an_offset_that_overflows_the_port() {
    assert!(desired_rules(u16::MAX).is_err());
}

#[test]
fn a_persisted_refusal_does_not_trigger_a_new_uac_automatically() {
    let declined = SetupRecord {
        p2p_port: 42777,
        attempted: true,
        decision: Some(SetupDecision::Declined),
        last_error: Some("cancelado".into()),
    };
    assert!(!should_auto_request(Some(&declined), false));
    assert!(should_auto_request(Some(&declined), true));
}
