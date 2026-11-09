//! The work receipt a hop signs (spec 2) and the bundle a gateway
//! countersigns (spec 4).
//!
//! A receipt never names the gateway: it carries the opaque bundle nonce
//! `bnd` the gateway handed with the session (spec 9). Only the bundle, which
//! goes to the ledger alone, names the gateway.

use crate::chain::LinkChain;
use crate::token;
use anyhow::{bail, Result};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};

fn header(kind: &str) -> String {
    format!(r#"{{"alg":"EdDSA","typ":"{}-{kind}/v1"}}"#, brand::slug())
}

pub fn receipt_header() -> String {
    header("work-receipt")
}

pub fn bundle_header() -> String {
    header("work-bundle")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelRef {
    pub id: String,
    /// Digest of the GGUF or slice manifest served, as the node knows it.
    pub digest: String,
}

/// Hidden-state format on the hop's input and output (`f16`, `f32`, `f8`,
/// `i4g64`, ...), so a verifier reproduces the lossy boundaries (spec 7).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireFormats {
    #[serde(rename = "in", default)]
    pub input: String,
    #[serde(rename = "out", default)]
    pub output: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Computed {
    pub prefill: u32,
    pub decode: u32,
    pub verify_accepted: u32,
    pub verify_rejected: u32,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Drafted {
    pub tokens: u32,
    pub layers: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReceiptClaims {
    pub rid: String,
    pub net: String,
    pub cyc: u64,
    pub bnd: String,
    pub node: String,
    /// The node's public key (hex); `node` must be its id.
    pub node_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtok: Option<String>,
    pub model: ModelRef,
    /// `[first, last)` layers executed, in the whole model's numbering.
    pub range: [u32; 2],
    pub engine: String,
    #[serde(default)]
    pub wire: WireFormats,
    pub computed: Computed,
    #[serde(default)]
    pub drafted: Drafted,
    pub cached: u32,
    pub in_chain: String,
    pub out_chain: String,
    /// Merkle root of the TOPLOC chunk proofs, when the hop kept them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summ: Option<String>,
    /// Number of TOPLOC chunks under `summ`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunks: Option<u32>,
    /// Tensor-parallel shard `[rank, parts]`, when the hop is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shard: Option<[u32; 2]>,
    pub t0: u64,
    pub t1: u64,
    pub ms: u64,
}

pub fn sign_receipt(claims: &ReceiptClaims, key: &SigningKey) -> Result<String> {
    if hex::encode(key.verifying_key().as_bytes()) != claims.node_key {
        bail!("a receipt is signed by the node it names");
    }
    token::sign(&receipt_header(), claims, key)
}

/// Opens a receipt: header, signature by `node_key`, and `node` is that key's
/// id. What the claims mean is the caller's to check.
pub fn open_receipt(token: &str) -> Result<ReceiptClaims> {
    let claims: ReceiptClaims =
        token::open(token, &receipt_header(), |c: &ReceiptClaims| c.node_key.clone())?;
    if crate::node_id_of_pubkey_hex(&claims.node_key).as_deref() != Some(claims.node.as_str()) {
        bail!("the receipt's node is not its key's");
    }
    Ok(claims)
}

/// Who the work was done for (spec 4). The visitor network is the account
/// the visitor charging task bills.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Consumer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visitor_node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visitor_network: Option<String>,
}

/// One step of the plan as the gateway ran it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StepRef {
    pub node: String,
    pub range: [u32; 2],
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HopStatus {
    Metered { receipt: String },
    Unmetered { reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HopEntry {
    pub node: String,
    #[serde(flatten)]
    pub status: HopStatus,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BundleClaims {
    pub bid: String,
    pub net: String,
    pub cyc: u64,
    pub bnd: String,
    pub gw: String,
    pub gw_key: String,
    /// The gateway is a visitor planning for itself (`direct`): its
    /// countersignature pays the hops but never earns it the gateway fee.
    #[serde(default)]
    pub gw_visitor: bool,
    pub plan: Vec<StepRef>,
    pub hops: Vec<HopEntry>,
    /// SHA-256 over the prompt and output token ids (LE i32), the gateway's
    /// commitment to what it keeps for re-execution.
    pub tokens_digest: String,
    pub prompt_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub consumer: Consumer,
    pub t: u64,
}

pub fn sign_bundle(claims: &BundleClaims, key: &SigningKey) -> Result<String> {
    if hex::encode(key.verifying_key().as_bytes()) != claims.gw_key {
        bail!("a bundle is signed by the gateway it names");
    }
    token::sign(&bundle_header(), claims, key)
}

pub fn open_bundle(token: &str) -> Result<BundleClaims> {
    let claims: BundleClaims =
        token::open(token, &bundle_header(), |c: &BundleClaims| c.gw_key.clone())?;
    if crate::node_id_of_pubkey_hex(&claims.gw_key).as_deref() != Some(claims.gw.as_str()) {
        bail!("the bundle's gateway is not its key's");
    }
    Ok(claims)
}

pub fn tokens_digest(prompt: &[i32], output: &[i32]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update((prompt.len() as u64).to_le_bytes());
    for t in prompt.iter().chain(output) {
        h.update(t.to_le_bytes());
    }
    hex::encode(h.finalize())
}

/// The gateway's checks before countersigning (spec 4). `answers` is, per
/// plan step in order, the receipt token the hop returned on release, if
/// any. `prompt_first_pos` and `prompt` are what the gateway sent the head.
/// A failed check marks the hop unmetered with the reason; it never fails the
/// request. A broken link marks both ends.
pub fn assemble_hops(
    plan: &[StepRef],
    bnd: &str,
    answers: &[Option<String>],
    head_input: &LinkChain,
) -> Vec<HopEntry> {
    let mut opened: Vec<Result<ReceiptClaims, String>> = plan
        .iter()
        .zip(answers.iter().chain(std::iter::repeat(&None)))
        .map(|(step, answer)| {
            let Some(token) = answer else {
                return Err("no receipt".into());
            };
            let claims = open_receipt(token).map_err(|e| format!("invalid receipt: {e}"))?;
            if claims.node != step.node {
                return Err("receipt from another node".into());
            }
            if claims.range != step.range {
                return Err("receipt claims another range".into());
            }
            if claims.bnd != bnd {
                return Err("receipt for another bundle".into());
            }
            Ok(claims)
        })
        .collect();
    // Links: the head against the gateway's token chain, then each pair.
    let mut broken = vec![None::<String>; plan.len()];
    if let Some(Ok(head)) = opened.first() {
        if head.in_chain != head_input.digest() {
            broken[0] = Some("head input differs from the prompt sent".into());
        }
    }
    for n in 0..plan.len().saturating_sub(1) {
        if let (Ok(a), Ok(b)) = (&opened[n], &opened[n + 1]) {
            if a.out_chain != b.in_chain {
                broken[n].get_or_insert_with(|| "output link differs from the next hop's input".into());
                broken[n + 1].get_or_insert_with(|| "input link differs from the previous hop's output".into());
            }
        }
    }
    plan.iter()
        .zip(answers.iter().chain(std::iter::repeat(&None)))
        .enumerate()
        .map(|(n, (step, answer))| {
            let status = match (std::mem::replace(&mut opened[n], Err(String::new())), broken[n].take()) {
                (Err(reason), _) => HopStatus::Unmetered { reason },
                (Ok(_), Some(reason)) => HopStatus::Unmetered { reason },
                (Ok(_), None) => HopStatus::Metered {
                    receipt: answer.clone().unwrap_or_default(),
                },
            };
            HopEntry { node: step.node.clone(), status }
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rand::rngs::OsRng;

    pub(crate) fn key() -> SigningKey {
        SigningKey::generate(&mut OsRng)
    }

    pub(crate) fn claims_for(key: &SigningKey, range: [u32; 2], bnd: &str) -> ReceiptClaims {
        let node_key = hex::encode(key.verifying_key().as_bytes());
        ReceiptClaims {
            rid: format!("r-{}", &node_key[..8]),
            net: "net".into(),
            cyc: 3,
            bnd: bnd.into(),
            node: crate::node_id_of_pubkey_hex(&node_key).unwrap(),
            node_key,
            mtok: None,
            model: ModelRef { id: "m".into(), digest: "d".into() },
            range,
            engine: "llama.cpp".into(),
            wire: WireFormats { input: "f16".into(), output: "f16".into() },
            computed: Computed { prefill: 10, decode: 5, ..Default::default() },
            drafted: Drafted::default(),
            cached: 0,
            in_chain: String::new(),
            out_chain: String::new(),
            summ: None,
            chunks: None,
            shard: None,
            t0: 1,
            t1: 2,
            ms: 3,
        }
    }

    #[test]
    fn a_receipt_opens_only_with_its_header_and_its_signer() {
        let k = key();
        let c = claims_for(&k, [0, 10], "b");
        let token = sign_receipt(&c, &k).unwrap();
        assert_eq!(open_receipt(&token).unwrap(), c);
        // Another kind of token from the same key does not open as a receipt.
        let other = token::sign(&bundle_header(), &c, &k).unwrap();
        assert!(open_receipt(&other).is_err());
        let mut forged = token.clone();
        forged.push('A');
        assert!(open_receipt(&forged).is_err());
        // Signing for another node is refused.
        assert!(sign_receipt(&c, &key()).is_err());
    }

    #[test]
    fn a_receipt_whose_node_is_not_its_key_is_refused() {
        let k = key();
        let mut c = claims_for(&k, [0, 10], "b");
        c.node = "someone-else".into();
        let token = token::sign(&receipt_header(), &c, &k).unwrap();
        assert!(open_receipt(&token).is_err());
    }

    #[test]
    fn receipt_claims_name_no_gateway() {
        let json = serde_json::to_value(claims_for(&key(), [0, 1], "b")).unwrap();
        assert!(json.get("gw").is_none());
        assert!(json.get("bnd").is_some());
    }

    fn chain(tag: &[u8]) -> LinkChain {
        let mut c = LinkChain::default();
        c.absorb(0..4, tag);
        c
    }

    fn two_hops(break_link: bool) -> (Vec<StepRef>, Vec<Option<String>>, LinkChain) {
        let (ka, kb) = (key(), key());
        let head_in = LinkChain::of_tokens(0, &[1, 2, 3, 4]);
        let mut a = claims_for(&ka, [0, 10], "b");
        a.in_chain = head_in.digest();
        a.out_chain = chain(b"h").digest();
        let mut b = claims_for(&kb, [10, 20], "b");
        b.in_chain = chain(if break_link { b"x" } else { b"h" }).digest();
        b.out_chain = chain(b"t").digest();
        let plan = vec![
            StepRef { node: a.node.clone(), range: a.range },
            StepRef { node: b.node.clone(), range: b.range },
        ];
        let answers = vec![Some(sign_receipt(&a, &ka).unwrap()), Some(sign_receipt(&b, &kb).unwrap())];
        (plan, answers, head_in)
    }

    fn metered(h: &HopEntry) -> bool {
        matches!(h.status, HopStatus::Metered { .. })
    }

    #[test]
    fn a_consistent_chain_of_receipts_is_accepted() {
        let (plan, answers, head_in) = two_hops(false);
        let hops = assemble_hops(&plan, "b", &answers, &head_in);
        assert!(hops.iter().all(metered), "{hops:?}");
    }

    #[test]
    fn a_broken_link_marks_both_ends_unmetered_with_the_reason() {
        let (plan, answers, head_in) = two_hops(true);
        let hops = assemble_hops(&plan, "b", &answers, &head_in);
        assert!(hops.iter().all(|h| !metered(h)));
    }

    #[test]
    fn a_head_that_did_not_read_the_prompt_sent_is_unmetered() {
        let (plan, answers, _) = two_hops(false);
        let hops = assemble_hops(&plan, "b", &answers, &LinkChain::of_tokens(0, &[9]));
        assert!(!metered(&hops[0]));
        assert!(metered(&hops[1]));
    }

    #[test]
    fn a_receipt_claiming_another_range_or_bundle_is_refused() {
        let (mut plan, answers, head_in) = two_hops(false);
        plan[1].range = [10, 30];
        let hops = assemble_hops(&plan, "b", &answers, &head_in);
        assert!(metered(&hops[0]) && !metered(&hops[1]));
        let (plan, answers, head_in) = two_hops(false);
        let hops = assemble_hops(&plan, "other", &answers, &head_in);
        assert!(hops.iter().all(|h| !metered(h)));
    }

    #[test]
    fn a_missing_receipt_leaves_the_others_metered() {
        let (plan, mut answers, head_in) = two_hops(false);
        answers[1] = None;
        let hops = assemble_hops(&plan, "b", &answers, &head_in);
        assert!(metered(&hops[0]));
        assert_eq!(hops[1].status, HopStatus::Unmetered { reason: "no receipt".into() });
    }

    #[test]
    fn bundle_records_the_visitor_network_and_opens_only_by_its_gateway() {
        let gk = key();
        let gw_key = hex::encode(gk.verifying_key().as_bytes());
        let b = BundleClaims {
            bid: "x".into(),
            net: "net".into(),
            cyc: 1,
            bnd: "b".into(),
            gw: crate::node_id_of_pubkey_hex(&gw_key).unwrap(),
            gw_key,
            gw_visitor: false,
            plan: vec![],
            hops: vec![],
            tokens_digest: tokens_digest(&[1], &[2]),
            prompt_tokens: 1,
            output_tokens: 1,
            consumer: Consumer { visitor_network: Some("their-net".into()), ..Default::default() },
            t: 5,
        };
        let token = sign_bundle(&b, &gk).unwrap();
        let opened = open_bundle(&token).unwrap();
        assert_eq!(opened.consumer.visitor_network.as_deref(), Some("their-net"));
        assert!(sign_bundle(&b, &key()).is_err());
    }

    #[test]
    fn tokens_digest_separates_prompt_from_output() {
        assert_ne!(tokens_digest(&[1, 2], &[3]), tokens_digest(&[1], &[2, 3]));
    }
}
