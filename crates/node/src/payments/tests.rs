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

fn temporary_database(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "pom-payments-{label}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}

fn remove_database(path: &std::path::Path) {
    // Only this test's own directory: the parent is the shared temp folder,
    // and removing it would delete the other tests' databases mid-run.
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
    }
}

#[test]
fn ledger_writer_batches_writes_and_moves_balances() {
    let path = temporary_database("ledger");
    let database = std::sync::Arc::new(crate::db::PomDb::open(&path).expect("open a database"));
    let writer = LedgerWriter::new(database.clone());

    writer.enqueue(PaymentWrite::Grant {
        key_id: "k1".into(),
        delta_micro: 5000,
        detail_json: None,
    });
    writer.enqueue(PaymentWrite::Settle {
        key_id: "k1".into(),
        amount_micro: 1200,
        deploy_id: Some("d1".into()),
        detail_json: Some("{\"prompt_tokens\":1000}".into()),
    });
    writer.flush();

    let rows = database
        .with_write("read ledger", |connection| {
            let mut statement = connection
                .prepare("SELECT kind, amount_micro, deploy_id FROM x402_ledger ORDER BY id")?;
            let mapped = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in mapped {
                out.push(row?);
            }
            Ok(out)
        })
        .expect("read the ledger");

    assert_eq!(
        rows,
        vec![
            ("grant".to_string(), 5000, None),
            ("settle".to_string(), 1200, Some("d1".to_string())),
        ]
    );

    let balance = database
        .with_write("read balance", |connection| {
            Ok(connection.query_row(
                "SELECT balance_micro FROM x402_accounts WHERE key_id = 'k1'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .expect("read the balance");
    assert_eq!(balance, 3800);

    remove_database(&path);
}

#[test]
fn ledger_settle_larger_than_the_grant_records_a_negative_balance() {
    let path = temporary_database("negative");
    let database = std::sync::Arc::new(crate::db::PomDb::open(&path).expect("open a database"));
    let writer = LedgerWriter::new(database.clone());

    writer.enqueue(PaymentWrite::Grant {
        key_id: "k1".into(),
        delta_micro: 1000,
        detail_json: None,
    });
    writer.enqueue(PaymentWrite::Settle {
        key_id: "k1".into(),
        amount_micro: 1200,
        deploy_id: None,
        detail_json: None,
    });
    writer.flush();

    let balance = database
        .with_write("read balance", |connection| {
            Ok(connection.query_row(
                "SELECT balance_micro FROM x402_accounts WHERE key_id = 'k1'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .expect("read the balance");
    assert_eq!(balance, -200);

    remove_database(&path);
}

#[test]
fn load_balances_restores_the_book_after_a_restart() {
    let path = temporary_database("restart");
    let database = std::sync::Arc::new(crate::db::PomDb::open(&path).expect("open a database"));
    let writer = LedgerWriter::new(database.clone());
    writer.enqueue(PaymentWrite::Grant {
        key_id: "k1".into(),
        delta_micro: 5000,
        detail_json: None,
    });
    writer.enqueue(PaymentWrite::Settle {
        key_id: "k1".into(),
        amount_micro: 1200,
        deploy_id: None,
        detail_json: None,
    });
    writer.flush();
    drop(database);

    // A fresh process: a new connection and a new book, loaded from the file.
    let reopened = crate::db::PomDb::open(&path).expect("reopen the database");
    let book = BalanceBook::default();
    let loaded = ledger::load_balances(&reopened, &book).expect("load balances");
    assert_eq!(loaded, 1);
    assert_eq!(book.balance("k1"), 3800);
    // A restart ends every live generation, so nothing stays reserved.
    assert_eq!(book.reserved("k1"), 0);

    remove_database(&path);
}

#[test]
fn load_grant_reads_only_an_unexpired_lease() {
    let path = temporary_database("grant");
    let database = crate::db::PomDb::open(&path).expect("open a database");

    database
        .with_write("seed grants", |connection| {
            connection.execute(
                "INSERT INTO x402_grants (key_id, gateway_node_id, allowance_micro, granted_at, expires_at)
                 VALUES ('k1', 'gw', 4000, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+1 hour')),
                        ('k1', 'other', 9000, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '+1 hour')),
                        ('k1', 'expired', 7000, strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-2 hour'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now', '-1 hour'))",
                [],
            )?;
            Ok(())
        })
        .expect("seed grants");

    assert_eq!(
        ledger::load_grant(&database, "k1", "gw").expect("read the lease"),
        Some(4000)
    );
    assert_eq!(
        ledger::load_grant(&database, "k1", "expired").expect("read the lease"),
        None
    );
    assert_eq!(
        ledger::load_grant(&database, "k1", "absent").expect("read the lease"),
        None
    );

    remove_database(&path);
}