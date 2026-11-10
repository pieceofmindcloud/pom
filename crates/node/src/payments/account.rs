//! Who pays: an account id, the key of the prepaid credit tables and of the
//! balance book.
//!
//! - `net:<pubkey>`: a visitor network, by its principal's public key (hex).
//!   Every visitor of that network draws on it
//!   (`docs/superpowers/specs/2026-11-09-visitor-charging-design.md`).
//! - `key:<id>`: an API key minted by a member node (the x402 core's original
//!   accounts; not charged by visitor charging).
//!
//! A bare id, as the x402 core wrote before accounts had a kind, reads as a key.

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AccountId {
    Network(String),
    Key(String),
}

pub fn network_account(principal_pubkey_hex: &str) -> String {
    format!("net:{principal_pubkey_hex}")
}

pub fn key_account(key_id: &str) -> String {
    format!("key:{key_id}")
}

impl AccountId {
    pub fn parse(id: &str) -> Self {
        match id.split_once(':') {
            Some(("net", key)) => AccountId::Network(key.to_string()),
            Some(("key", key)) => AccountId::Key(key.to_string()),
            _ => AccountId::Key(id.to_string()),
        }
    }

    pub fn as_string(&self) -> String {
        match self {
            AccountId::Network(key) => network_account(key),
            AccountId::Key(id) => key_account(id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_ids_round_trip_and_bare_ids_are_keys() {
        assert_eq!(AccountId::parse(&network_account("ab12")), AccountId::Network("ab12".into()));
        assert_eq!(AccountId::parse(&key_account("k1")), AccountId::Key("k1".into()));
        assert_eq!(AccountId::parse("k1"), AccountId::Key("k1".into()));
        assert_eq!(AccountId::Network("ab12".into()).as_string(), "net:ab12");
    }
}
