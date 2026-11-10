//! Reading the server's "come back in this long" out of response headers.
//!
//! Both wire protocols share it: `retry-after` (delta-seconds or HTTP-date,
//! RFC 9110 §10.2.3) and the millisecond variant `retry-after-ms` that
//! Anthropic and OpenAI-style gateways send. The result is a hint for the
//! backoff policy, never an instruction.

use std::collections::BTreeMap;
use std::time::{Duration, SystemTime};

pub fn parse_retry_after(headers: &BTreeMap<String, String>, now: SystemTime) -> Option<Duration> {
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    };
    if let Some(ms) = get("retry-after-ms").and_then(parse_millis) {
        return Some(ms);
    }
    let v = get("retry-after")?;
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(v).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

fn parse_millis(v: &str) -> Option<Duration> {
    let ms: f64 = v.parse().ok()?;
    (ms.is_finite() && ms >= 0.0).then(|| Duration::from_secs_f64(ms / 1000.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn now() -> SystemTime {
        // Wed, 21 Oct 2015 07:28:00 GMT
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_445_412_480)
    }

    fn parse(v: &str) -> Option<Duration> {
        parse_retry_after(&headers(&[("retry-after", v)]), now())
    }

    #[test]
    fn integer_seconds() {
        assert_eq!(parse("7"), Some(Duration::from_secs(7)));
        assert_eq!(parse(" 0 "), Some(Duration::ZERO));
    }

    #[test]
    fn all_three_http_date_formats() {
        let want = Some(Duration::from_secs(30));
        assert_eq!(parse("Wed, 21 Oct 2015 07:28:30 GMT"), want);
        assert_eq!(parse("Wednesday, 21-Oct-15 07:28:30 GMT"), want);
        assert_eq!(parse("Wed Oct 21 07:28:30 2015"), want);
    }

    #[test]
    fn a_date_in_the_past_is_zero() {
        assert_eq!(parse("Wed, 21 Oct 2015 07:27:00 GMT"), Some(Duration::ZERO));
    }

    #[test]
    fn milliseconds_win_over_seconds() {
        let h = headers(&[("Retry-After-Ms", "1500"), ("retry-after", "7")]);
        assert_eq!(
            parse_retry_after(&h, now()),
            Some(Duration::from_millis(1500))
        );
    }

    #[test]
    fn garbage_is_none() {
        assert_eq!(parse("soon"), None);
        assert_eq!(parse("-3"), None);
        assert_eq!(parse(""), None);
        let h = headers(&[("retry-after-ms", "NaN")]);
        assert_eq!(parse_retry_after(&h, now()), None);
        assert_eq!(parse_retry_after(&headers(&[]), now()), None);
    }
}
