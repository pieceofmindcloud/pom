//! Link digests (spec 3). Each hop keeps two running digests per session,
//! over every frame item it received and sent, in order. The wire does not
//! change bytes between hops, so hop `n`'s `out` digest equals hop `n+1`'s
//! `in` digest when both handled the same frames.

use sha2::{Digest, Sha256};
use std::ops::Range;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LinkChain([u8; 32]);

impl LinkChain {
    /// Absorbs one frame item: the positions it covers and its wire bytes.
    pub fn absorb(&mut self, positions: Range<u32>, bytes: &[u8]) {
        let item = Sha256::digest(bytes);
        let mut h = Sha256::new();
        h.update(self.0);
        h.update(positions.start.to_le_bytes());
        h.update(positions.end.to_le_bytes());
        h.update(item);
        self.0 = h.finalize().into();
    }

    /// Absorbs token ids, the head's input (and the gateway's view of it).
    pub fn absorb_tokens(&mut self, first_pos: u32, tokens: &[i32]) {
        let bytes: Vec<u8> = tokens.iter().flat_map(|t| t.to_le_bytes()).collect();
        self.absorb(first_pos..first_pos + tokens.len() as u32, &bytes);
    }

    pub fn of_tokens(first_pos: u32, tokens: &[i32]) -> Self {
        let mut chain = Self::default();
        chain.absorb_tokens(first_pos, tokens);
        chain
    }

    pub fn digest(&self) -> String {
        hex::encode(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sender_out_chain_equals_receiver_in_chain() {
        let (mut out, mut inp) = (LinkChain::default(), LinkChain::default());
        for (pos, bytes) in [(0u32..32, b"a".as_slice()), (32..33, b"b")] {
            out.absorb(pos.clone(), bytes);
            inp.absorb(pos, bytes);
        }
        assert_eq!(out.digest(), inp.digest());
    }

    #[test]
    fn a_changed_byte_position_or_order_breaks_the_chain() {
        let mut a = LinkChain::default();
        a.absorb(0..1, b"x");
        let mut b = LinkChain::default();
        b.absorb(0..1, b"y");
        let mut c = LinkChain::default();
        c.absorb(1..2, b"x");
        assert_ne!(a.digest(), b.digest());
        assert_ne!(a.digest(), c.digest());
        let mut d = LinkChain::default();
        d.absorb(0..1, b"x");
        d.absorb(1..2, b"y");
        let mut e = LinkChain::default();
        e.absorb(1..2, b"y");
        e.absorb(0..1, b"x");
        assert_ne!(d.digest(), e.digest());
    }

    #[test]
    fn head_in_chain_hashes_token_ids_like_the_gateway() {
        let mut hop = LinkChain::default();
        hop.absorb_tokens(0, &[1, 2, 3]);
        assert_eq!(hop.digest(), LinkChain::of_tokens(0, &[1, 2, 3]).digest());
        assert_ne!(hop.digest(), LinkChain::of_tokens(0, &[1, 2, 4]).digest());
    }
}
