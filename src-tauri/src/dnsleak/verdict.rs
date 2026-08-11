//! Pure verdict logic: compares the resolvers a leak-test service observed
//! against the ASN of the current external IP (the VPN exit, when the
//! tunnel is actually up). No I/O — everything here is a plain function of
//! its inputs, so it's testable without wiremock or a running app.
//!
//! # The comparison rule, and why it is conservative
//!
//! A false "you're safe" is far worse than a false "possible leak": the user
//! acts on the verdict, and a wrongly-reassuring one is the failure mode that
//! actually hurts someone. So every step below is biased toward *not*
//! reporting `Consistent` unless the evidence is unambiguous:
//!
//! - No resolvers observed at all -> `NoResolvers`. We didn't get to run a
//!   real comparison, full stop.
//! - No usable expected ASN (unknown external IP, or its `asn` string didn't
//!   parse) -> `Inconclusive`. We have resolvers but nothing trustworthy to
//!   compare them against.
//! - A resolver whose own `asn` string doesn't parse is skipped entirely —
//!   it neither confirms nor denies anything, so it must not silently count
//!   as a match.
//! - **Any** resolver with a parseable ASN that differs from the expected
//!   one -> `Leaking`, even if every other resolver matches. One leaking
//!   resolver is a leak; averaging away a bad result would defeat the point
//!   of the check.
//! - Only when every parseable resolver ASN matches, and at least one
//!   resolver was actually comparable, do we report `Consistent`.
//! - If resolvers exist but none of them yielded a comparable ASN ->
//!   `Inconclusive`, not `Consistent` — silence is not evidence of safety.

use std::net::IpAddr;

use serde::Serialize;

use super::Resolver;

/// Outcome of comparing observed resolvers against the current exit's ASN.
/// See the module docs for the comparison rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    /// The service reported no resolvers at all for this session.
    NoResolvers,
    /// Every resolver we could compare shares the exit's ASN.
    Consistent,
    /// At least one resolver sits in a clearly different network than the
    /// exit. `foreign` lists the offending resolver addresses.
    Leaking { foreign: Vec<IpAddr> },
    /// Resolvers exist but there isn't enough trustworthy data to compare
    /// them against the exit (no known exit ASN, or no resolver ASN parsed).
    Inconclusive { reason: String },
}

/// Compares `resolvers` against `expected_asn` (typically the current
/// external IP's `GeoInfo::asn`, e.g. `"AS15169 Google LLC"`) and returns a
/// verdict. See module docs for the full rule.
pub fn evaluate(expected_asn: Option<&str>, resolvers: &[Resolver]) -> Verdict {
    if resolvers.is_empty() {
        return Verdict::NoResolvers;
    }

    let Some(expected) = expected_asn.and_then(as_number) else {
        return Verdict::Inconclusive {
            reason: "no known ASN for the current external IP to compare against".to_string(),
        };
    };

    let mut foreign = Vec::new();
    let mut matched_any = false;

    for resolver in resolvers {
        let Some(resolver_asn) = resolver.asn.as_deref().and_then(as_number) else {
            // Can't judge this one; it neither confirms nor denies a leak.
            continue;
        };

        if resolver_asn == expected {
            matched_any = true;
        } else {
            foreign.push(resolver.ip);
        }
    }

    if !foreign.is_empty() {
        return Verdict::Leaking { foreign };
    }

    if matched_any {
        Verdict::Consistent
    } else {
        Verdict::Inconclusive {
            reason: "no resolver reported a parseable ASN".to_string(),
        }
    }
}

/// Extracts and normalizes the leading `AS<digits>` token from strings like
/// `"AS15169 Google LLC"` or `"as15169"`. Returns `None` for anything else
/// (empty, missing prefix, non-numeric) so an unrecognized format falls
/// through to "can't compare" rather than risking a false match.
fn as_number(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.len() < 2 || !trimmed.is_char_boundary(2) || !trimmed[..2].eq_ignore_ascii_case("AS")
    {
        return None;
    }

    let rest = &trimmed[2..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }

    Some(format!("AS{digits}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolver(ip: &str, asn: Option<&str>) -> Resolver {
        Resolver {
            ip: ip.parse().unwrap(),
            country: None,
            asn: asn.map(String::from),
        }
    }

    // -----------------------------------------------------------------
    // as_number
    // -----------------------------------------------------------------

    #[test]
    fn as_number_extracts_leading_token_and_normalizes_case() {
        assert_eq!(as_number("AS15169 Google LLC"), Some("AS15169".to_string()));
        assert_eq!(as_number("as15169"), Some("AS15169".to_string()));
        assert_eq!(as_number("  AS701  "), Some("AS701".to_string()));
    }

    #[test]
    fn as_number_rejects_unrecognized_formats() {
        assert_eq!(as_number(""), None);
        assert_eq!(as_number("Google LLC"), None);
        assert_eq!(as_number("AS"), None);
        assert_eq!(as_number("A15169"), None);
        assert_eq!(as_number("15169"), None);
    }

    // -----------------------------------------------------------------
    // evaluate — table-driven over the documented rule
    // -----------------------------------------------------------------

    #[test]
    fn empty_resolvers_is_no_resolvers_regardless_of_expected_asn() {
        assert_eq!(evaluate(Some("AS15169"), &[]), Verdict::NoResolvers);
        assert_eq!(evaluate(None, &[]), Verdict::NoResolvers);
    }

    #[test]
    fn no_expected_asn_is_inconclusive_even_with_resolvers_present() {
        let resolvers = vec![resolver("1.1.1.1", Some("AS13335 Cloudflare"))];
        assert_eq!(
            evaluate(None, &resolvers),
            Verdict::Inconclusive {
                reason: "no known ASN for the current external IP to compare against".to_string()
            }
        );
    }

    #[test]
    fn unparseable_expected_asn_is_inconclusive() {
        let resolvers = vec![resolver("1.1.1.1", Some("AS13335 Cloudflare"))];
        assert_eq!(
            evaluate(Some("not-an-asn"), &resolvers),
            Verdict::Inconclusive {
                reason: "no known ASN for the current external IP to compare against".to_string()
            }
        );
    }

    #[test]
    fn same_asn_is_consistent() {
        let resolvers = vec![
            resolver("1.1.1.1", Some("AS13335 Cloudflare")),
            resolver("1.0.0.1", Some("as13335")),
        ];
        assert_eq!(
            evaluate(Some("AS13335 Cloudflare, Inc."), &resolvers),
            Verdict::Consistent
        );
    }

    #[test]
    fn clearly_different_asn_is_leaking() {
        let resolvers = vec![resolver("8.8.8.8", Some("AS15169 Google LLC"))];
        assert_eq!(
            evaluate(Some("AS7922 Comcast Cable"), &resolvers),
            Verdict::Leaking {
                foreign: vec!["8.8.8.8".parse().unwrap()]
            }
        );
    }

    #[test]
    fn one_foreign_resolver_among_matching_ones_is_still_leaking() {
        // A single leaking resolver must not be averaged away by others that
        // happen to match — that would defeat the point of the check.
        let resolvers = vec![
            resolver("1.1.1.1", Some("AS7922 Comcast Cable")),
            resolver("8.8.8.8", Some("AS15169 Google LLC")),
        ];
        assert_eq!(
            evaluate(Some("AS7922 Comcast Cable"), &resolvers),
            Verdict::Leaking {
                foreign: vec!["8.8.8.8".parse().unwrap()]
            }
        );
    }

    #[test]
    fn resolvers_with_unparseable_asn_are_skipped_not_treated_as_matches() {
        let resolvers = vec![resolver("9.9.9.9", Some("Unknown"))];
        assert_eq!(
            evaluate(Some("AS7922 Comcast Cable"), &resolvers),
            Verdict::Inconclusive {
                reason: "no resolver reported a parseable ASN".to_string()
            }
        );
    }

    #[test]
    fn resolvers_with_missing_asn_are_skipped_not_treated_as_matches() {
        let resolvers = vec![resolver("9.9.9.9", None)];
        assert_eq!(
            evaluate(Some("AS7922 Comcast Cable"), &resolvers),
            Verdict::Inconclusive {
                reason: "no resolver reported a parseable ASN".to_string()
            }
        );
    }

    #[test]
    fn mixed_comparable_and_incomparable_resolvers_still_judges_on_the_comparable_ones() {
        let resolvers = vec![
            resolver("9.9.9.9", None),
            resolver("8.8.8.8", Some("AS15169 Google LLC")),
        ];
        assert_eq!(
            evaluate(Some("AS7922 Comcast Cable"), &resolvers),
            Verdict::Leaking {
                foreign: vec!["8.8.8.8".parse().unwrap()]
            }
        );
    }
}
