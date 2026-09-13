use super::windows_firewall::{
    desired_rules, should_auto_request, FirewallProtocol, SetupDecision, SetupRecord,
};

#[test]
fn regras_de_firewall_usam_a_porta_efetiva_e_os_quatro_protocolos() {
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
            (FirewallProtocol::Tcp, 52778),
        ]
    );
}

#[test]
fn regra_de_firewall_recusa_offset_que_estoura_a_porta() {
    assert!(desired_rules(u16::MAX).is_err());
}

#[test]
fn recusa_persistida_nao_dispara_novo_uac_automaticamente() {
    let declined = SetupRecord {
        p2p_port: 42777,
        attempted: true,
        decision: Some(SetupDecision::Declined),
        last_error: Some("cancelado".into()),
    };
    assert!(!should_auto_request(Some(&declined), false));
    assert!(should_auto_request(Some(&declined), true));
}
