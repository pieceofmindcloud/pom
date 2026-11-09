//! HMAC selection (spec 6, 7). Selection is keyed by a secret only the ledger
//! admin holds and drawn after the work is submitted, so no hop knows at
//! compute time which of its work will be checked.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// A uniform value in `[0, 1)` for `item` in `cycle`.
pub fn score(secret: &[u8], item: &str, cycle: u64) -> f64 {
    let mut mac = HmacSha256::new_from_slice(secret).expect("any key length");
    mac.update(item.as_bytes());
    mac.update(&cycle.to_le_bytes());
    let out = mac.finalize().into_bytes();
    let n = u64::from_be_bytes(out[..8].try_into().unwrap());
    (n >> 11) as f64 / (1u64 << 53) as f64
}

/// The `n` items with the lowest scores, in score order. Fewer items than
/// `n`: all of them.
pub fn draw<'a>(secret: &[u8], cycle: u64, items: &'a [String], n: usize) -> Vec<&'a String> {
    let mut scored: Vec<(f64, &String)> = items.iter().map(|i| (score(secret, i, cycle), i)).collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(b.1)));
    scored.into_iter().take(n).map(|(_, i)| i).collect()
}

/// Whether `item` is selected at `rate`.
pub fn selected(secret: &[u8], item: &str, cycle: u64, rate: f64) -> bool {
    score(secret, item, cycle) < rate
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("rid-{i}")).collect()
    }

    #[test]
    fn the_draw_is_deterministic_for_a_secret_and_differs_across_secrets() {
        let all = items(200);
        assert_eq!(draw(b"s", 4, &all, 30), draw(b"s", 4, &all, 30));
        assert_ne!(draw(b"s", 4, &all, 30), draw(b"t", 4, &all, 30));
        assert_ne!(draw(b"s", 4, &all, 30), draw(b"s", 5, &all, 30));
        assert_eq!(draw(b"s", 4, &all, 30).len(), 30);
    }

    #[test]
    fn fewer_items_than_samples_checks_all() {
        assert_eq!(draw(b"s", 1, &items(7), 30).len(), 7);
    }

    #[test]
    fn selection_rate_is_roughly_honored() {
        let hits = items(10_000).iter().filter(|i| selected(b"k", i, 1, 0.1)).count();
        assert!((800..1200).contains(&hits), "{hits}");
    }
}
