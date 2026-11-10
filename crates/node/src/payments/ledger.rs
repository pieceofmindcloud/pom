use std::sync::mpsc;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use tracing::error;

use super::book::BalanceBook;
use crate::db::PomDb;

const WRITER_QUEUE_CAPACITY: usize = 4096;

/// One payment event destined for the durable ledger. The in-memory book is
/// the admission truth; `x402_accounts` is the durable mirror that survives a
/// restart, and `x402_ledger` is the audit trail the principal reconciles
/// against.
#[derive(Clone, Debug)]
pub enum PaymentWrite {
    Grant {
        key_id: String,
        delta_micro: i64,
        detail_json: Option<String>,
    },
    Settle {
        key_id: String,
        amount_micro: i64,
        deploy_id: Option<String>,
        detail_json: Option<String>,
    },
    Refund {
        key_id: String,
        amount_micro: i64,
        detail_json: Option<String>,
    },
}

impl PaymentWrite {
    pub fn settle(account: &str, amount_micro: i64, deploy_id: Option<String>, detail_json: Option<String>) -> Self {
        Self::Settle { key_id: account.to_string(), amount_micro, deploy_id, detail_json }
    }

    pub fn grant(account: &str, delta_micro: i64, detail_json: Option<String>) -> Self {
        Self::Grant { key_id: account.to_string(), delta_micro, detail_json }
    }

    fn key_id(&self) -> &str {
        match self {
            Self::Grant { key_id, .. }
            | Self::Settle { key_id, .. }
            | Self::Refund { key_id, .. } => key_id,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::Grant { .. } => "grant",
            Self::Settle { .. } => "settle",
            Self::Refund { .. } => "refund",
        }
    }

    /// The amount the ledger row records, always the magnitude of the event.
    fn amount_micro(&self) -> i64 {
        match self {
            Self::Grant { delta_micro, .. } => *delta_micro,
            Self::Settle { amount_micro, .. } | Self::Refund { amount_micro, .. } => {
                *amount_micro
            }
        }
    }

    /// The effect on the account balance: a grant adds, a settle or a refund
    /// deducts. A settle larger than the hold can drive the balance negative;
    /// the durable mirror records it and the next admission refuses, the same
    /// rule the in-memory book applies.
    fn balance_delta_micro(&self) -> i64 {
        match self {
            Self::Grant { delta_micro, .. } => *delta_micro,
            Self::Settle { amount_micro, .. } | Self::Refund { amount_micro, .. } => {
                -(*amount_micro).max(0)
            }
        }
    }

    fn deploy_id(&self) -> Option<&str> {
        match self {
            Self::Settle { deploy_id, .. } => deploy_id.as_deref(),
            _ => None,
        }
    }

    fn detail_json(&self) -> Option<&str> {
        match self {
            Self::Grant { detail_json, .. }
            | Self::Settle { detail_json, .. }
            | Self::Refund { detail_json, .. } => detail_json.as_deref(),
        }
    }
}

enum LedgerCommand {
    Write(PaymentWrite),
    #[cfg(test)]
    Flush(mpsc::Sender<()>),
}

/// The bounded writer of the payment ledger, mirroring `reqlog.rs`: the
/// request path only enqueues, and a dedicated thread owns the shared `PomDb`
/// connection and batches the inserts. A queue that is full loses the write
/// with a logged error, never a blocked generation.
pub struct LedgerWriter {
    sender: mpsc::SyncSender<LedgerCommand>,
}

impl LedgerWriter {
    /// Start the dedicated writer. A startup failure is logged and deliberately
    /// does not prevent the gateway from serving inference.
    pub fn new(database: Arc<PomDb>) -> Self {
        let (sender, receiver) = mpsc::sync_channel(WRITER_QUEUE_CAPACITY);
        let writer_database = database.clone();
        if let Err(e) = thread::Builder::new()
            .name("pom-payments-writer".into())
            .spawn(move || run_writer(writer_database, receiver))
        {
            error!(error = %e, "could not start the payments writer");
        }
        Self { sender }
    }

    /// Queue one event. Off the hot path: the settle at the terminal `GenSummary`
    /// and the admin grants both come here, and nothing blocks the request.
    pub fn enqueue(&self, write: PaymentWrite) {
        let key_id = write.key_id().to_string();
        if let Err(e) = self.sender.try_send(LedgerCommand::Write(write)) {
            error!(
                operation = "queue payment write",
                key_id = %key_id,
                error = %e,
                "payment write did not enter the ledger queue; operation lost"
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn flush(&self) {
        let (done, barrier) = mpsc::channel();
        self.sender
            .send(LedgerCommand::Flush(done))
            .expect("enqueue the payments barrier");
        barrier
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("wait for the payments writer");
    }
}

fn run_writer(database: Arc<PomDb>, receiver: mpsc::Receiver<LedgerCommand>) {
    while let Ok(command) = receiver.recv() {
        // Drain what is already queued so a burst of settles becomes one
        // transaction, not one per token-billed request.
        let mut batch = match command {
            LedgerCommand::Write(write) => vec![write],
            #[cfg(test)]
            LedgerCommand::Flush(done) => {
                let _ = done.send(());
                continue;
            }
        };
        while let Ok(next) = receiver.try_recv() {
            match next {
                LedgerCommand::Write(write) => batch.push(write),
                #[cfg(test)]
                LedgerCommand::Flush(done) => {
                    // The barrier arrives after every earlier write: committing
                    // this batch first, then answering, is what `flush` relies on.
                    if let Err(e) = write_batch(&database, &batch) {
                        error!(operation = "write payment ledger", writes = batch.len(), error = %e,
                            "failed to write the payment ledger; operation lost");
                    }
                    batch = Vec::new();
                    let _ = done.send(());
                }
            }
        }

        if !batch.is_empty() {
            if let Err(e) = write_batch(&database, &batch) {
                error!(
                    operation = "write payment ledger",
                    writes = batch.len(),
                    error = %e,
                    "failed to write the payment ledger; operation lost"
                );
            }
        }
    }
}

fn write_batch(database: &PomDb, batch: &[PaymentWrite]) -> Result<()> {
    database.with_write("write payment ledger", |connection| {
        let tx = connection.unchecked_transaction()?;
        for write in batch {
            apply_write(&tx, write)?;
        }
        tx.commit()?;
        Ok(())
    })
}

fn apply_write(tx: &Connection, write: &PaymentWrite) -> Result<()> {
    tx.execute(
        "INSERT INTO x402_ledger (key_id, kind, amount_micro, deploy_id, detail_json, created_at)
         VALUES (?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        params![
            write.key_id(),
            write.kind(),
            write.amount_micro(),
            write.deploy_id(),
            write.detail_json(),
        ],
    )?;
    // The insert seeds a new account with the delta; the conflict arm moves the
    // stored balance by the same delta once.
    let delta = write.balance_delta_micro();
    tx.execute(
        "INSERT INTO x402_accounts (key_id, balance_micro, updated_at)
         VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ON CONFLICT(key_id) DO UPDATE SET
            balance_micro = x402_accounts.balance_micro + ?,
            updated_at = excluded.updated_at",
        params![write.key_id(), delta, delta],
    )?;
    Ok(())
}

/// Read the durable balances into the gateway's book at startup. Reserved
/// holds are never persisted: a restart ends every live generation, so the new
/// book starts with the settled balances and nothing reserved.
pub fn load_balances(database: &PomDb, book: &BalanceBook) -> Result<usize> {
    let balances = database.with_write("load payment balances", |connection| {
        let mut statement = connection.prepare("SELECT key_id, balance_micro FROM x402_accounts")?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
        let mut loaded = Vec::new();
        for row in rows {
            loaded.push(row?);
        }
        Ok(loaded)
    })?;
    for (key_id, balance_micro) in &balances {
        book.grant(key_id, *balance_micro);
    }
    Ok(balances.len())
}

/// Read the allowance lease this gateway holds for one account. `None` when the
/// principal granted nothing or the lease expired, which the caller treats as
/// zero: a secondary gateway without a lease cannot admit paid requests.
pub fn load_grant(database: &PomDb, key_id: &str, gateway_node_id: &str) -> Result<Option<i64>> {
    database.with_write("load payment grant", |connection| {
        let allowance: Option<i64> = connection
            .query_row(
                "SELECT allowance_micro FROM x402_grants
                 WHERE key_id = ? AND gateway_node_id = ?
                   AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')",
                params![key_id, gateway_node_id],
                |row| row.get(0),
            )
            .optional()
            .context("read the payment grant")?;
        Ok(allowance)
    })
}