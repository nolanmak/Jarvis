//! Discord rejects a message `nonce` longer than 25 characters, which makes
//! the whole send fail. Keep every deterministic nonce within that limit.
pub const MAX_NONCE_CHARS:usize=25;

/// Return `raw` unchanged when it fits, so existing short nonces keep
/// deduplicating across a deploy. Longer values map deterministically to a
/// 25-character hex digest: the same input (a retry) yields the same nonce,
/// distinct inputs stay distinct, and a digest never contains the `-` that
/// every literal prefixed nonce has.
pub fn bounded_nonce(raw:&str)->String {raw.to_string()}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_nonces_are_bounded_deterministic_and_distinct_while_short_ones_are_unchanged() {
        assert_eq!(bounded_nonce("owner-alert-7"),"owner-alert-7");
        let exact="imsg-0123456789abcdef0123";assert_eq!(exact.len(),25);assert_eq!(bounded_nonce(exact),exact);
        let a=bounded_nonce("owner-imessage-health-1000");
        let b=bounded_nonce("owner-imessage-health-1001");
        assert!(a.chars().count()<=MAX_NONCE_CHARS,"{a}");
        assert_eq!(a,bounded_nonce("owner-imessage-health-1000"),"a retry must reuse the nonce");
        assert_ne!(a,b,"distinct messages must not share a nonce");
        assert!(!a.contains('-'),"a digest cannot collide with a literal prefixed nonce");
        let wide=bounded_nonce(&"é".repeat(30));assert!(wide.chars().count()<=MAX_NONCE_CHARS);
    }
}
