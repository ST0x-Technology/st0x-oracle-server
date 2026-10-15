//! The market session signed into context slots 3-5 (tag, start, end): the
//! session the pricing quote was priced in, on the asset's listing exchange.
//! A quote without a session, or with one that does not hold together, is
//! refused.

use st0x_pricing_types::{Quote, SessionTag};

/// The session slots of one signed context, in whole Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignedSession {
    pub tag: SessionTag,
    pub start: u64,
    pub end: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRefusal {
    /// The quote carries no session.
    Missing,
    /// The quote's session cannot be signed; the text says why.
    Invalid(String),
}

impl SignedSession {
    /// Read the session off `quote`. Every session must contain the quote's
    /// `source_ts_unix_ms`, start inclusive; an open session's end is
    /// exclusive, a closed one's inclusive. Bounds are floored to seconds,
    /// which keeps `publish_time >= start` for any quote read in session.
    pub fn from_quote(quote: &Quote) -> Result<Self, SessionRefusal> {
        let session = quote.session.ok_or(SessionRefusal::Missing)?;
        let (start_ms, end_ms) = (session.start_unix_ms, session.end_unix_ms);
        let read_at = quote.source_ts_unix_ms;
        let ordered = start_ms <= read_at
            && match session.tag {
                SessionTag::Closed => read_at <= end_ms,
                SessionTag::Premarket | SessionTag::Rth | SessionTag::Afterhours => {
                    read_at < end_ms
                }
            };
        if !ordered {
            return Err(SessionRefusal::Invalid(format!(
                "{} session [{start_ms}, {end_ms}) does not hold source_ts {}",
                session.tag.as_str(),
                quote.source_ts_unix_ms
            )));
        }
        let seconds = |ms: i64| {
            u64::try_from(ms.div_euclid(1000)).map_err(|_| {
                SessionRefusal::Invalid(format!(
                    "{} session bound {ms} is before 1970",
                    session.tag.as_str()
                ))
            })
        };
        Ok(Self {
            tag: session.tag,
            start: seconds(start_ms)?,
            end: seconds(end_ms)?,
        })
    }
}

/// Encode a session tag as Rain `IntOrAString` **V3** bytes32: the exact
/// layout the Rainlang parser produces for a `"…"` string literal via
/// `LibIntOrAString::fromStringV3`. Byte 31 = `(len & 0x1f) | 0xe0`, ASCII
/// data at bytes `(31-len)..31`, head zero-padded. Strategies compare
/// `equal-to(signed-context<0 3>() "rth")` directly.
pub fn tag_bytes32_v3(tag: SessionTag) -> [u8; 32] {
    let bytes = tag.as_str().as_bytes();
    assert!(bytes.len() < 32, "session name must fit in 31 bytes");
    let mut out = [0u8; 32];
    let len = bytes.len();
    out[31 - len..31].copy_from_slice(bytes);
    out[31] = 0xe0 | (len as u8);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use st0x_pricing_types::{QuoteSession, WireAddress, WireFloat, WireU256};

    const SOURCE_TS: i64 = 1_791_963_000_000; // 2026-10-14 07:30 UTC
    const PARIS_OPEN: i64 = 1_791_961_200_000; // 07:00 UTC
    const PARIS_CLOSE: i64 = 1_791_991_800_000; // 15:30 UTC

    fn quote(session: Option<QuoteSession>) -> Quote {
        Quote {
            asset: "wtEU".into(),
            chain_id: 8453,
            base: WireAddress::from_bytes([0x11; 20]),
            quote: WireAddress::from_bytes([0x22; 20]),
            rate_base_to_quote: WireFloat::default(),
            rate_quote_to_base: WireFloat::default(),
            expiry_unix_ms: SOURCE_TS + 20_000,
            execution_deadline_unix_ms: Some(SOURCE_TS + 60_000),
            source_ts_unix_ms: SOURCE_TS,
            nav_ratio: WireU256::default(),
            underlying_rate_base_to_quote: WireFloat::default(),
            underlying_rate_quote_to_base: WireFloat::default(),
            session,
        }
    }

    fn session(tag: SessionTag, start_unix_ms: i64, end_unix_ms: i64) -> Option<QuoteSession> {
        Some(QuoteSession {
            tag,
            start_unix_ms,
            end_unix_ms,
        })
    }

    #[test]
    fn open_session_is_signed_in_whole_seconds() {
        assert_eq!(
            SignedSession::from_quote(&quote(session(SessionTag::Rth, PARIS_OPEN, PARIS_CLOSE))),
            Ok(SignedSession {
                tag: SessionTag::Rth,
                start: 1_791_961_200,
                end: 1_791_991_800,
            })
        );
    }

    #[test]
    fn fractional_bounds_are_floored_to_the_second() {
        let mut q = quote(session(
            SessionTag::Rth,
            PARIS_OPEN + 999,
            PARIS_CLOSE + 999,
        ));
        q.source_ts_unix_ms = PARIS_OPEN + 999;
        assert_eq!(
            SignedSession::from_quote(&q),
            Ok(SignedSession {
                tag: SessionTag::Rth,
                start: 1_791_961_200,
                end: 1_791_991_800,
            })
        );
    }

    #[test]
    fn missing_session_is_refused() {
        assert_eq!(
            SignedSession::from_quote(&quote(None)),
            Err(SessionRefusal::Missing)
        );
    }

    #[test]
    fn open_session_must_hold_the_source_timestamp() {
        for (start, end) in [
            (SOURCE_TS + 1, PARIS_CLOSE),
            (PARIS_OPEN, SOURCE_TS),
            (PARIS_OPEN, SOURCE_TS - 1),
            (PARIS_CLOSE, PARIS_OPEN),
        ] {
            for tag in [
                SessionTag::Premarket,
                SessionTag::Rth,
                SessionTag::Afterhours,
            ] {
                assert!(
                    matches!(
                        SignedSession::from_quote(&quote(session(tag, start, end))),
                        Err(SessionRefusal::Invalid(_))
                    ),
                    "{tag:?} [{start}, {end})"
                );
            }
        }
        assert!(SignedSession::from_quote(&quote(session(
            SessionTag::Rth,
            SOURCE_TS,
            SOURCE_TS + 1
        )))
        .is_ok());
    }

    #[test]
    fn closed_session_must_hold_the_source_timestamp() {
        let hour = 3_600_000;
        assert_eq!(
            SignedSession::from_quote(&quote(session(
                SessionTag::Closed,
                SOURCE_TS - hour,
                SOURCE_TS + hour
            ))),
            Ok(SignedSession {
                tag: SessionTag::Closed,
                start: 1_791_959_400,
                end: 1_791_966_600,
            })
        );
        assert!(
            SignedSession::from_quote(&quote(session(SessionTag::Closed, SOURCE_TS, SOURCE_TS)))
                .is_ok(),
            "bounds the producer cannot know equal source_ts"
        );
        for (start, end) in [
            (PARIS_CLOSE, PARIS_OPEN),
            (SOURCE_TS + 1, SOURCE_TS + hour),
            (SOURCE_TS - hour, SOURCE_TS - 1),
        ] {
            assert!(
                matches!(
                    SignedSession::from_quote(&quote(session(SessionTag::Closed, start, end))),
                    Err(SessionRefusal::Invalid(_))
                ),
                "[{start}, {end}]"
            );
        }
    }

    #[test]
    fn negative_bounds_are_refused() {
        for (start, end, source_ts) in [(-1_000, 0, 0), (i64::MIN, i64::MAX, SOURCE_TS)] {
            let mut q = quote(session(SessionTag::Closed, start, end));
            q.source_ts_unix_ms = source_ts;
            assert!(
                matches!(
                    SignedSession::from_quote(&q),
                    Err(SessionRefusal::Invalid(_))
                ),
                "[{start}, {end}]"
            );
        }
    }

    #[test]
    fn tag_bytes32_v3_matches_rain_intorastring_v3_format() {
        for tag in [
            SessionTag::Premarket,
            SessionTag::Rth,
            SessionTag::Afterhours,
            SessionTag::Closed,
        ] {
            let b = tag_bytes32_v3(tag);
            let name = tag.as_str().as_bytes();
            let len = name.len();
            assert_eq!(b[31], 0xe0 | len as u8, "{tag:?}: byte 31 is 0xe0 | length");
            assert_eq!(
                &b[31 - len..31],
                name,
                "{tag:?}: ASCII ends before the length byte"
            );
            assert!(
                b[..31 - len].iter().all(|&x| x == 0),
                "{tag:?}: head is zero"
            );
        }
    }

    #[test]
    fn tag_bytes32_v3_known_rth_value() {
        let mut expected = [0u8; 32];
        expected[28] = b'r';
        expected[29] = b't';
        expected[30] = b'h';
        expected[31] = 0xe3;
        assert_eq!(tag_bytes32_v3(SessionTag::Rth), expected);
    }
}
