//! #1833 D12 — sender authentication for Gmail-direct inbound mail.
//!
//! Only the TOP-MOST `Authentication-Results` header whose authserv-id is
//! `mx.google.com` is read. Google prepends its own header on receipt, so
//! any copy below it — including one the sender wrote, even one claiming to
//! be from `mx.google.com` — is ignored, as is every header from another
//! authserv-id.
//!
//! - `dmarc=pass` → trusted.
//! - otherwise `dkim=pass` whose signing domain (`header.d`, or the domain
//!   of `header.i` — Google writes `header.i=@domain`) equals the From
//!   domain or is a parent of it → trusted.
//! - otherwise not trusted.
//!
//! No Google header at all → `None`, which the autonomy gate treats as
//! unauthenticated. Composio-backed mail never reaches this parser, so it is
//! `None` too.

pub use augmentagent_store::autonomy::SenderAuth;

/// The authserv-id whose verdict is trusted.
pub const GOOGLE_AUTHSERV_ID: &str = "mx.google.com";

/// Evaluate a message's headers, in message order (top first), against its
/// `From` header value.
pub fn sender_auth_from_headers<'a, I>(headers: I, from: &str) -> Option<SenderAuth>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    let _ = (headers.into_iter().count(), from);
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FROM: &str = "Jane Doe <jane@work.example.test>";

    fn auth(headers: &[(&str, &str)], from: &str) -> Option<SenderAuth> {
        sender_auth_from_headers(headers.iter().copied(), from)
    }

    const GOOGLE_DMARC_PASS: &str = "mx.google.com;\r\n       dkim=pass header.i=@work.example.test header.s=s1 header.b=AbCd;\r\n       spf=pass (google.com: domain of jane@work.example.test designates 192.0.2.1 as permitted sender) smtp.mailfrom=jane@work.example.test;\r\n       dmarc=pass (p=REJECT sp=REJECT dis=NONE) header.from=work.example.test";

    #[test]
    fn dmarc_pass_trusted() {
        let a = auth(&[("Authentication-Results", GOOGLE_DMARC_PASS)], FROM).unwrap();
        assert!(a.dmarc_pass);
        assert!(a.trusted());
        // Header name matching is case-insensitive.
        assert!(auth(&[("authentication-results", GOOGLE_DMARC_PASS)], FROM).unwrap().trusted());
    }

    #[test]
    fn dkim_aligned_trusted() {
        // No DMARC policy published: dmarc=none, but DKIM signs for the From
        // domain (header.i form, as Google writes it).
        let h = "mx.google.com; dkim=pass header.i=@work.example.test header.s=s1 header.b=x; spf=neutral smtp.mailfrom=bounce@other.example.test; dmarc=none header.from=work.example.test";
        let a = auth(&[("Authentication-Results", h)], FROM).unwrap();
        assert!(!a.dmarc_pass);
        assert!(a.dkim_aligned);
        assert!(a.trusted());
        // header.d form, signing domain a parent of the From domain.
        let h = "mx.google.com; dkim=pass header.d=example.test header.s=s1; dmarc=fail header.from=work.example.test";
        let a = auth(&[("Authentication-Results", h)], FROM).unwrap();
        assert!(a.dkim_aligned && a.trusted());
        // A second, failing signature does not undo an aligned pass.
        let h = "mx.google.com; dkim=fail header.i=@evil.example.test; dkim=pass header.i=@work.example.test; dmarc=none";
        assert!(auth(&[("Authentication-Results", h)], FROM).unwrap().dkim_aligned);
    }

    #[test]
    fn dkim_misaligned_untrusted() {
        // Passing signature from an unrelated domain (e.g. a bulk sender).
        let h = "mx.google.com; dkim=pass header.i=@mailer.example.test header.s=s1; spf=pass smtp.mailfrom=bounce@mailer.example.test; dmarc=fail (p=NONE) header.from=work.example.test";
        let a = auth(&[("Authentication-Results", h)], FROM).unwrap();
        assert_eq!(a, SenderAuth { dmarc_pass: false, dkim_aligned: false });
        assert!(!a.trusted());
        // A child of the From domain is not a parent of it.
        let h = "mx.google.com; dkim=pass header.d=sub.work.example.test; dmarc=none";
        assert!(!auth(&[("Authentication-Results", h)], FROM).unwrap().trusted());
        // Look-alike suffix is not a parent domain.
        let h = "mx.google.com; dkim=pass header.d=k.example.test; dmarc=none";
        assert!(!auth(&[("Authentication-Results", h)], "a@work.example.test").unwrap().trusted());
        let h = "mx.google.com; dkim=pass header.d=ork.example.test; dmarc=none";
        assert!(!auth(&[("Authentication-Results", h)], "a@work.example.test").unwrap().trusted());
        // A bare TLD never counts as a parent.
        let h = "mx.google.com; dkim=pass header.d=test; dmarc=none";
        assert!(!auth(&[("Authentication-Results", h)], FROM).unwrap().trusted());
        // dkim=fail with an aligned domain is still a fail.
        let h = "mx.google.com; dkim=fail header.i=@work.example.test; dmarc=fail";
        assert!(!auth(&[("Authentication-Results", h)], FROM).unwrap().trusted());
    }

    #[test]
    fn forged_sender_added_header_ignored() {
        // Google's header (top) says fail; the sender's own copy below it
        // claims mx.google.com passed. Only the top-most counts.
        let google = "mx.google.com; dkim=none; spf=softfail smtp.mailfrom=jane@work.example.test; dmarc=fail (p=NONE) header.from=work.example.test";
        let forged = "mx.google.com; dkim=pass header.i=@work.example.test; dmarc=pass header.from=work.example.test";
        let a = auth(
            &[
                ("Delivered-To", "owner@example.test"),
                ("Authentication-Results", google),
                ("From", FROM),
                ("Authentication-Results", forged),
            ],
            FROM,
        )
        .unwrap();
        assert!(!a.trusted(), "the sender-added copy must be ignored: {a:?}");
        // A header from any other authserv-id is ignored outright.
        let other = "relay.example.test; dmarc=pass header.from=work.example.test";
        assert_eq!(auth(&[("Authentication-Results", other)], FROM), None);
        // A look-alike authserv-id is not Google.
        let other = "mx.google.com.evil.example.test; dmarc=pass";
        assert_eq!(auth(&[("Authentication-Results", other)], FROM), None);
    }

    #[test]
    fn multiple_headers_topmost_google_wins() {
        let relay = "relay.example.test; dmarc=fail";
        let top_google = "mx.google.com; dmarc=pass header.from=work.example.test";
        let lower_google = "mx.google.com; dkim=fail; dmarc=fail header.from=work.example.test";
        let a = auth(
            &[
                ("Authentication-Results", relay),
                ("Authentication-Results", top_google),
                ("Authentication-Results", lower_google),
            ],
            FROM,
        )
        .unwrap();
        assert!(a.dmarc_pass, "top-most Google header decides: {a:?}");
        // And the other way round.
        let a = auth(
            &[("Authentication-Results", lower_google), ("Authentication-Results", top_google)],
            FROM,
        )
        .unwrap();
        assert!(!a.trusted());
        // An `ARC-Authentication-Results` header is a different header.
        let a = auth(
            &[
                ("ARC-Authentication-Results", "i=1; mx.google.com; dmarc=pass"),
                ("Authentication-Results", lower_google),
            ],
            FROM,
        )
        .unwrap();
        assert!(!a.trusted());
        // authserv-id with a version token still matches.
        let v = "mx.google.com 1; dmarc=pass header.from=work.example.test";
        assert!(auth(&[("Authentication-Results", v)], FROM).unwrap().dmarc_pass);
    }

    #[test]
    fn missing_header_none() {
        assert_eq!(auth(&[("From", FROM), ("Subject", "hi")], FROM), None);
        assert_eq!(auth(&[], FROM), None);
        // Comments mentioning dmarc=pass don't count.
        let h = "mx.google.com; dmarc=fail (looks like dmarc=pass but is not) header.from=work.example.test";
        assert!(!auth(&[("Authentication-Results", h)], FROM).unwrap().trusted());
    }
}
