use super::*;

fn test_pricing() -> DeploymentPricing {
    DeploymentPricing {
        price_in_micro_per_1k: 1000,
        price_out_micro_per_1k: 2000,
        min_funding_micro: 0,
    }
}

#[test]
fn hold_micro_exact_thousands() {
    assert_eq!(hold_micro(&test_pricing(), 1000, 1000), 3000);
}

#[test]
fn hold_micro_ceil_partial_input() {
    assert_eq!(hold_micro(&test_pricing(), 1, 0), 1);
    assert_eq!(hold_micro(&test_pricing(), 1001, 0), 1001);
}

#[test]
fn hold_micro_uses_effective_not_leg() {
    let pricing = test_pricing();
    assert_eq!(hold_micro(&pricing, 0, 1000), 2000);
    assert_eq!(hold_micro(&pricing, 0, 100), 200);
}

#[test]
fn bill_micro_cached_prefix_floor_one_token() {
    assert_eq!(bill_micro(&test_pricing(), 1000, 0, 1000), 3000);
    assert_eq!(bill_micro(&test_pricing(), 1000, 1000, 1000), 2001);
}

#[test]
fn min_funding_micro_max_session_and_admin_floor() {
    let mut pricing = test_pricing();
    assert_eq!(min_funding_micro(&pricing, [(1000, 1000), (1, 1)]), 3000);
    pricing.min_funding_micro = 5000;
    assert_eq!(min_funding_micro(&pricing, [(1, 1)]), 5000);
}

#[test]
fn balance_book_admission_rule() {
    let book = BalanceBook::default();
    book.grant("k", 1000);
    assert_eq!(book.try_reserve("k", 1000), Some(true));
    assert_eq!(book.reserved("k"), 1000);
    assert_eq!(book.try_reserve("k", 1), Some(false));
    book.release("k", 1000);
    assert_eq!(book.try_reserve("k", 1000), Some(true));
}

#[test]
fn balance_book_settle_moves_reserved_to_spent() {
    let book = BalanceBook::default();
    book.grant("k", 1000);
    assert_eq!(book.try_reserve("k", 1000), Some(true));
    book.settle("k", 1000, 700);
    assert_eq!(book.balance("k"), 300);
    assert_eq!(book.reserved("k"), 0);
}

#[test]
fn balance_book_settle_larger_than_hold_records_negative() {
    let book = BalanceBook::default();
    book.grant("k", 1000);
    assert_eq!(book.try_reserve("k", 1000), Some(true));
    book.settle("k", 1000, 1200);
    assert_eq!(book.balance("k"), -200);
    assert_eq!(book.reserved("k"), 0);
}

#[test]
fn balance_book_allowance_limits_admission() {
    let book = BalanceBook::default();
    book.set_allowance(500);
    book.grant("k", 1000);
    assert_eq!(book.try_reserve("k", 600), Some(false));
    assert_eq!(book.try_reserve("k", 500), Some(true));
}

#[test]
fn balance_book_allowance_getter_reports_the_lease() {
    let book = BalanceBook::default();
    assert_eq!(book.allowance(), i64::MAX);
    book.set_allowance(500);
    assert_eq!(book.allowance(), 500);
}

#[test]
fn payment_state_gate_disabled_is_noop() {
    let state = PaymentState::new(false, "principal");
    let reservation = state.gate("k", Some(&test_pricing()), 1000, 1000).unwrap();
    assert_eq!(reservation.hold_micro, 0);
    assert_eq!(state.book().reserved("k"), 0);
}

#[test]
fn payment_state_gate_free_deployment_is_noop() {
    let state = PaymentState::new(true, "principal");
    let reservation = state.gate("k", None, 1000, 1000).unwrap();
    assert_eq!(reservation.hold_micro, 0);
    assert_eq!(state.book().reserved("k"), 0);
}

#[test]
fn payment_state_gate_underfunded_returns_402_accepts() {
    let state = PaymentState::new(true, "principal");
    let err = state
        .gate("k", Some(&test_pricing()), 1000, 1000)
        .expect_err("under-funded key must be challenged");

    match err {
        GateError::Challenge(challenge) => {
            assert_eq!(challenge.hold_micro, 3000);
            assert_eq!(challenge.balance_micro, 0);
            assert_eq!(challenge.body["accepts"][0]["scheme"].as_str(), Some("pom-credit"));
            assert_eq!(challenge.body["accepts"][0]["payTo"].as_str(), Some("pk:principal"));
            assert_eq!(
                challenge.body["accepts"][0]["maxAmountRequired"].as_i64(),
                Some(3000)
            );
            assert_eq!(
                challenge.body["accepts"][0]["metadata"]["balance_micro"].as_i64(),
                Some(0)
            );
        }
        GateError::Unavailable => panic!("expected a 402 challenge, not unavailable state"),
    }
}

#[test]
fn payment_state_gate_funded_key_reserves_hold() {
    let state = PaymentState::new(true, "principal");
    state.grant("k", 3000);
    let reservation = state
        .gate("k", Some(&test_pricing()), 1000, 1000)
        .unwrap();
    assert_eq!(reservation.hold_micro, 3000);
    assert_eq!(state.book().reserved("k"), 3000);
    assert_eq!(state.book().balance("k"), 3000);
}

#[test]
fn payment_state_settle_releases_hold_and_deducts_bill() {
    let state = PaymentState::new(true, "principal");
    state.grant("k", 3000);
    let reservation = state
        .gate("k", Some(&test_pricing()), 1000, 1000)
        .unwrap();
    state.settle("k", reservation.hold_micro, 2500);
    assert_eq!(state.book().reserved("k"), 0);
    assert_eq!(state.book().balance("k"), 500);
}

#[test]
fn payment_state_release_unspent_hold() {
    let state = PaymentState::new(true, "principal");
    state.grant("k", 1000);
    let reservation = state.gate("k", Some(&test_pricing()), 0, 500).unwrap();
    assert_eq!(reservation.hold_micro, 1000);
    state.release("k", reservation.hold_micro);
    assert_eq!(state.book().reserved("k"), 0);
    assert_eq!(state.book().balance("k"), 1000);
}

#[test]
fn payment_state_gate_deployment_uses_price_table() {
    let state = PaymentState::new(true, "principal");
    state.pricing().set("d1", test_pricing());
    state.grant("k", 3000);
    let reservation = state.gate_deployment("k", "d1", 1000, 1000).unwrap();
    assert_eq!(reservation.hold_micro, 3000);
    assert_eq!(state.book().reserved("k"), 3000);
}

#[test]
fn payment_state_set_allowance_limits_secondary_gateway() {
    let state = PaymentState::new(true, "principal");
    state.set_allowance(500);
    state.grant("k", 1000);
    let err = state
        .gate("k", Some(&test_pricing()), 1000, 1000)
        .expect_err("gateway allowance must cap admission");
    assert!(matches!(err, GateError::Challenge(_)));
}

#[test]
fn payment_state_enable_flag_and_principal_identity() {
    let mut state = PaymentState::new(false, "principal");
    assert!(!state.is_enabled());
    assert_eq!(state.principal_node_id(), "principal");
    state.set_enabled(true);
    assert!(state.is_enabled());
    assert_eq!(state.book().allowance(), i64::MAX);
}

#[test]
fn price_table_remove_makes_the_deployment_free_again() {
    let state = PaymentState::new(true, "principal");
    state.pricing().set("d1", test_pricing());
    state.grant("k", 3000);
    assert_eq!(
        state.gate_deployment("k", "d1", 1000, 1000).unwrap().hold_micro,
        3000
    );
    state.pricing().remove("d1");
    let reservation = state.gate_deployment("k", "d1", 1000, 1000).unwrap();
    assert_eq!(reservation.hold_micro, 0);
}

#[test]
fn reservation_carries_the_key_id() {
    let state = PaymentState::new(true, "principal");
    state.grant("k1", 3000);
    let reservation = state.gate("k1", Some(&test_pricing()), 1000, 1000).unwrap();
    assert_eq!(reservation.key_id, "k1");
    assert_eq!(reservation.hold_micro, 3000);
}

#[test]
fn payment_state_gate_unavailable_when_book_poisoned() {
    let state = PaymentState::new(true, "principal");
    state.grant("k", 10_000);
    state.book().poison_allowance_lock_for_test();
    let err = state
        .gate("k", Some(&test_pricing()), 1000, 1000)
        .expect_err("a poisoned book must fail closed");
    assert!(matches!(err, GateError::Unavailable));
}