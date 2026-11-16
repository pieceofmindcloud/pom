//! SHA-256 binary Merkle tree with leaf/node domain separation. Used over a
//! receipt's TOPLOC chunk proofs (`summ`, spec 3) and over a cycle's accepted
//! receipts (the statement root, spec 5). An odd node is carried up as is.

use sha2::{Digest, Sha256};

fn leaf(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([0u8]);
    h.update(data);
    h.finalize().into()
}

fn node(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update([1u8]);
    h.update(a);
    h.update(b);
    h.finalize().into()
}

fn levels(leaves: &[&[u8]]) -> Vec<Vec<[u8; 32]>> {
    let mut levels = vec![leaves.iter().map(|l| leaf(l)).collect::<Vec<_>>()];
    while levels.last().unwrap().len() > 1 {
        let prev = levels.last().unwrap();
        let next = prev
            .chunks(2)
            .map(|pair| if pair.len() == 2 { node(&pair[0], &pair[1]) } else { pair[0] })
            .collect();
        levels.push(next);
    }
    levels
}

/// Root of the tree, hex. The root of no leaves is the hash of nothing.
pub fn root(leaves: &[&[u8]]) -> String {
    if leaves.is_empty() {
        return hex::encode(Sha256::digest([]));
    }
    hex::encode(levels(leaves).last().unwrap()[0])
}

/// The sibling path of leaf `index`: (sibling hash hex, sibling is on the right).
pub fn proof(leaves: &[&[u8]], index: usize) -> Option<Vec<(String, bool)>> {
    if index >= leaves.len() {
        return None;
    }
    let levels = levels(leaves);
    let mut path = Vec::new();
    let mut i = index;
    for level in &levels[..levels.len() - 1] {
        let sibling = i ^ 1;
        if sibling < level.len() {
            path.push((hex::encode(level[sibling]), sibling > i));
        }
        i /= 2;
    }
    Some(path)
}

pub fn verify(root_hex: &str, data: &[u8], path: &[(String, bool)]) -> bool {
    let mut acc = leaf(data);
    for (sibling, right) in path {
        let Ok(raw) = hex::decode(sibling) else { return false };
        let Ok(sib): Result<[u8; 32], _> = raw.as_slice().try_into() else { return false };
        acc = if *right { node(&acc, &sib) } else { node(&sib, &acc) };
    }
    hex::encode(acc) == root_hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(n: usize) -> Vec<Vec<u8>> {
        (0..n).map(|i| format!("leaf-{i}").into_bytes()).collect()
    }

    #[test]
    fn every_leaf_proves_against_the_root_for_any_count() {
        for n in 1..=9 {
            let owned = data(n);
            let leaves: Vec<&[u8]> = owned.iter().map(|v| v.as_slice()).collect();
            let r = root(&leaves);
            for i in 0..n {
                let p = proof(&leaves, i).unwrap();
                assert!(verify(&r, leaves[i], &p), "n={n} i={i}");
            }
        }
    }

    #[test]
    fn a_proof_for_another_leaf_or_root_fails() {
        let owned = data(5);
        let leaves: Vec<&[u8]> = owned.iter().map(|v| v.as_slice()).collect();
        let r = root(&leaves);
        let p = proof(&leaves, 2).unwrap();
        assert!(!verify(&r, leaves[3], &p));
        assert!(!verify(&root(&leaves[..4]), leaves[2], &p));
        assert!(proof(&leaves, 5).is_none());
    }

    #[test]
    fn a_leaf_cannot_pass_for_an_inner_node() {
        let owned = data(2);
        let leaves: Vec<&[u8]> = owned.iter().map(|v| v.as_slice()).collect();
        let inner = node(&leaf(leaves[0]), &leaf(leaves[1]));
        assert_ne!(root(&[&inner]), root(&leaves));
    }
}
