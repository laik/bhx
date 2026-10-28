//! Scheduling vocabulary: intervals, credential lookup by name.
//!
//! A schedule stores the task spec and the *names* of credentials it uses.
//! Values are read from the environment when the run starts
//! (`BH_CRED_<NAME>`), so the database never holds a secret and a schedule
//! file can be copied between machines.

use std::collections::BTreeMap;

/// Shortest interval a schedule may have. It is a guard rail, not a technical
/// limit: a mis-typed `1s` should not turn into a busy loop on a shared site.
pub const MIN_INTERVAL_MS: i64 = 1_000;

/// Parses `30s`, `5m`, `2h`, `1d`, or a compound like `1h30m` into
/// milliseconds. Rejects zero, negatives, unknown units and a bare number
/// (ambiguous units are how a "5" becomes five milliseconds).
pub fn parse_interval(s: &str) -> Result<i64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty interval".into());
    }
    let mut rest = s;
    let mut total: i64 = 0;
    while !rest.is_empty() {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            return Err(format!("interval {s:?} needs a number before its unit"));
        }
        let n: i64 = digits
            .parse()
            .map_err(|_| format!("interval {s:?} has an oversized number"))?;
        rest = &rest[digits.len()..];
        // `m` before `ms`, so a value is never read as a shorter unit.
        let (unit_ms, unit_len) = if rest.starts_with("ms") {
            (1, 2)
        } else if rest.starts_with('s') {
            (1_000, 1)
        } else if rest.starts_with('m') {
            (60_000, 1)
        } else if rest.starts_with('h') {
            (3_600_000, 1)
        } else if rest.starts_with('d') {
            (86_400_000, 1)
        } else {
            return Err(format!("interval {s:?} needs a unit: s, m, h or d"));
        };
        total += n * unit_ms;
        rest = &rest[unit_len..];
    }
    if total < MIN_INTERVAL_MS {
        return Err(format!(
            "interval {s:?} is shorter than the {} ms minimum",
            MIN_INTERVAL_MS
        ));
    }
    Ok(total)
}

/// Renders milliseconds as the shortest unambiguous interval string.
pub fn format_interval(ms: i64) -> String {
    for (unit, div) in [("d", 86_400_000), ("h", 3_600_000), ("m", 60_000), ("s", 1_000), ("ms", 1)] {
        if ms % div == 0 && ms >= div {
            return format!("{}{unit}", ms / div);
        }
    }
    format!("{ms}ms")
}

/// The environment variable a credential name resolves from. Names are
/// upper-cased and anything that is not `[A-Za-z0-9]` becomes `_`, so
/// `${cred:db password}` and `${cred:DB_PASSWORD}` both read
/// `BH_CRED_DB_PASSWORD`.
pub fn credential_env_key(name: &str) -> String {
    let mut key = String::from("BH_CRED_");
    for c in name.chars() {
        key.push(if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' });
    }
    key
}

/// Resolves every referenced credential from `lookup`. On a missing value it
/// names the variable to set; it never returns a partially resolved map,
/// because an unresolved `${cred:x}` would be typed into the page verbatim.
pub fn resolve_credentials(
    names: &[String],
    lookup: impl Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    let mut missing = Vec::new();
    for name in names {
        let key = credential_env_key(name);
        match lookup(&key) {
            Some(v) => {
                out.insert(name.clone(), v);
            }
            None => missing.push(key),
        }
    }
    if !missing.is_empty() {
        return Err(format!(
            "missing credential value(s); set {}",
            missing.join(", ")
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_parse_and_render() {
        assert_eq!(parse_interval("30s"), Ok(30_000));
        assert_eq!(parse_interval("5m"), Ok(300_000));
        assert_eq!(parse_interval("2h"), Ok(7_200_000));
        assert_eq!(parse_interval("1d"), Ok(86_400_000));
        assert_eq!(parse_interval("1h30m"), Ok(5_400_000));
        assert_eq!(parse_interval(" 10m "), Ok(600_000));
        assert_eq!(format_interval(600_000), "10m");
        assert_eq!(format_interval(90_000), "90s");
        assert_eq!(format_interval(1_500), "1500ms");
    }

    #[test]
    fn bad_intervals_are_rejected_not_guessed() {
        for bad in ["", "30", "5 minutes", "m5", "-5m", "0s", "500ms", "1w"] {
            assert!(parse_interval(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn credential_names_map_to_one_env_key() {
        assert_eq!(credential_env_key("password"), "BH_CRED_PASSWORD");
        assert_eq!(credential_env_key("db password"), "BH_CRED_DB_PASSWORD");
        assert_eq!(credential_env_key("a-b.c"), "BH_CRED_A_B_C");
    }

    #[test]
    fn missing_credentials_fail_the_whole_map() {
        let names = vec!["user".to_string(), "password".to_string()];
        let ok = resolve_credentials(&names, |k| match k {
            "BH_CRED_USER" => Some("admin".into()),
            "BH_CRED_PASSWORD" => Some("pw".into()),
            _ => None,
        })
        .unwrap();
        assert_eq!(ok["password"], "pw");

        let err = resolve_credentials(&names, |k| {
            (k == "BH_CRED_USER").then(|| "admin".to_string())
        })
        .unwrap_err();
        assert!(err.contains("BH_CRED_PASSWORD"), "the error names what to set: {err}");
        assert!(!err.contains("pw"), "the error never echoes a value");
    }
}
