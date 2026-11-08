//! Cron expression parsing and matching engine.
//!
//! Supports standard 5-field cron expressions with:
//! - Numeric values, three-letter names (MON, JAN, etc.)
//! - Comma-separated lists
//! - Ranges (including name-based like MON-FRI)
//! - Step notation (*/5, 3-59/10)

pub(crate) fn cron_matches(expr: &str, now: &time::OffsetDateTime) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }

    let minute = now.minute();
    let hour = now.hour();
    let day = now.day();
    let month: u8 = now.month().into();
    let weekday = now.weekday().number_days_from_sunday(); // cron: 0=Sun

    field_matches(parts[0], minute, 0, 59)
        && field_matches(parts[1], hour, 0, 23)
        && field_matches(parts[2], day, 1, 31)
        && field_matches_month(parts[3], month)
        && field_matches_dow(parts[4], weekday)
}

fn field_matches(field: &str, value: u8, default_min: u8, default_max: u8) -> bool {
    let field = field.trim();
    if field == "*" || field == "?" {
        return true;
    }

    // Step notation: */5, 3-59/10, etc.
    let (range_expr, step) = if let Some(pos) = field.find('/') {
        let step: u8 = field[pos + 1..].trim().parse().unwrap_or(1);
        if step == 0 {
            return false;
        }
        (field[..pos].trim(), step)
    } else {
        (field, 1)
    };

    if step == 1 {
        // Simple list/range check, no step
        value_in_list(range_expr, value, default_min, default_max)
    } else {
        // Step: check base match first, then step constraint
        let base_match = value_in_list(range_expr, value, default_min, default_max);
        if !base_match {
            return false;
        }
        // Find the range start for step arithmetic
        let start = range_start_value(range_expr, default_min, default_min);
        if value < start {
            return false;
        }
        (value - start) % step == 0
    }
}

fn value_in_list(expr: &str, value: u8, default_min: u8, default_max: u8) -> bool {
    // Check for range in each comma-separated item
    expr.split(',').any(|item| {
        let item = item.trim();
        // Check month names
        if let Some(num) = month_name_to_num(item) {
            return value == num;
        }
        // Check day names
        if let Some(num) = day_name_to_num(item) {
            return value == num;
        }
        if item == "*" || item == "?" {
            return true;
        }
        if let Some(pos) = item.find('-') {
            let lo = item[..pos].trim().parse::<u8>().unwrap_or(default_min);
            let hi = item[pos + 1..].trim().parse::<u8>().unwrap_or(default_max);
            value >= lo && value <= hi
        } else {
            match item.parse::<u8>() {
                Ok(v) => value == v,
                Err(_) => false,
            }
        }
    })
}

fn range_start_value(expr: &str, default: u8, _min: u8) -> u8 {
    let first = expr.split(',').next().unwrap_or(expr).trim();
    if let Some(pos) = first.find('-') {
        first[..pos].trim().parse().unwrap_or(default)
    } else {
        first.parse().unwrap_or(default)
    }
}

fn field_matches_month(field: &str, value: u8) -> bool {
    // Expand month names to numbers, then delegate
    let expanded = expand_names(field, MONTH_NAMES);
    field_matches(&expanded, value, 1, 12)
}

fn field_matches_dow(field: &str, value: u8) -> bool {
    // Expand day names to numbers, then delegate
    // Note: cron accepts 0 and 7 for Sunday
    let expanded = expand_names(field, DAY_NAMES);
    let num_val = if value == 0 { 0 } else { value };
    field_matches(&expanded, num_val, 0, 7)
}

fn expand_names(field: &str, names: &[(&str, &str)]) -> String {
    let mut result = field.to_uppercase();
    for (name, num) in names {
        result = result.replace(*name, num);
    }
    result
}

const MONTH_NAMES: &[(&str, &str)] = &[
    ("JAN", "1"),
    ("FEB", "2"),
    ("MAR", "3"),
    ("APR", "4"),
    ("MAY", "5"),
    ("JUN", "6"),
    ("JUL", "7"),
    ("AUG", "8"),
    ("SEP", "9"),
    ("OCT", "10"),
    ("NOV", "11"),
    ("DEC", "12"),
];

const DAY_NAMES: &[(&str, &str)] = &[
    ("SUN", "0"),
    ("MON", "1"),
    ("TUE", "2"),
    ("WED", "3"),
    ("THU", "4"),
    ("FRI", "5"),
    ("SAT", "6"),
];

fn month_name_to_num(s: &str) -> Option<u8> {
    let upper = s.to_uppercase();
    for (name, num) in MONTH_NAMES {
        if upper == *name {
            return num.parse().ok();
        }
    }
    None
}

fn day_name_to_num(s: &str) -> Option<u8> {
    let upper = s.to_uppercase();
    for (name, num) in DAY_NAMES {
        if upper == *name {
            return num.parse().ok();
        }
    }
    None
}

/// Validate a 5-field cron expression has the right structure.
/// Accepts numeric values, three-letter names (MON, JAN, etc.),
/// comma-separated lists, ranges (including name-based like MON-FRI),
/// and step notation (*/5, 3-59/10).
pub fn cron_expression_valid(expr: &str) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return false;
    }
    // Expand names in DOW (field 4) and month (field 3) so name-based
    // ranges like MON-FRI or JAN-MAR pass the numeric range check.
    let processed: Vec<String> = parts
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let f = f.trim();
            match i {
                3 => expand_names(f, MONTH_NAMES),
                4 => expand_names(f, DAY_NAMES),
                _ => f.to_string(),
            }
        })
        .collect();
    processed.iter().all(|f| {
        if f == "*" || f == "?" {
            return true;
        }
        f.split(',').all(|item| {
            let item = item.trim();
            let (base, _has_step) = if let Some(pos) = item.find('/') {
                let step = &item[pos + 1..];
                if step.parse::<u8>().unwrap_or(0) == 0 {
                    return false;
                }
                (&item[..pos], true)
            } else {
                (item, false)
            };
            if base == "*" || base == "?" || base.is_empty() {
                return true;
            }
            // Check range (names already expanded, so numeric parse works)
            if let Some(pos) = base.find('-') {
                base[..pos].trim().parse::<u8>().is_ok()
                    && base[pos + 1..].trim().parse::<u8>().is_ok()
            } else {
                base.parse::<u8>().is_ok()
            }
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::Month;

    #[test]
    fn cron_valid_expression() {
        assert!(cron_expression_valid("* * * * *"), "* * * * *");
        assert!(cron_expression_valid("0 9 * * *"), "0 9 * * *");
        assert!(cron_expression_valid("*/5 * * * *"), "*/5 * * * *");
        assert!(cron_expression_valid("0,30 * * * *"), "0,30 * * * *");
        assert!(cron_expression_valid("0 9-17 * * 1-5"), "0 9-17 * * 1-5");
        assert!(cron_expression_valid("30 4 1,15 * 5"), "30 4 1,15 * 5");
        assert!(cron_expression_valid("0 9 * * MON-FRI"), "0 9 * * MON-FRI");
        assert!(cron_expression_valid("0 9 * JAN *"), "0 9 * JAN *");
        assert!(
            !cron_expression_valid("0 9 * * * *"),
            "6 fields should fail"
        );
        assert!(!cron_expression_valid("0 9 * *"), "4 fields should fail");
        assert!(!cron_expression_valid(""), "empty should fail");
        assert!(!cron_expression_valid("0 */0 * * *"), "step 0 should fail");
    }

    #[test]
    fn cron_matches_basic() {
        // Test that * matches anything
        use time::OffsetDateTime;
        let now = OffsetDateTime::now_utc();
        assert!(cron_matches("* * * * *", &now));
    }

    #[test]
    fn cron_matches_specific() {
        // Build a time at 9:30 on Jan 15 (Monday)
        // 2024-01-15 00:00 UTC = 1705276800
        let t = time::OffsetDateTime::from_unix_timestamp(1705311000) // +9h30m = 09:30 UTC
            .unwrap();
        assert_eq!(t.month(), Month::January);
        assert_eq!(t.day(), 15);
        assert_eq!(t.hour(), 9);
        assert_eq!(t.minute(), 30);
        assert!(cron_matches("30 9 15 1 *", &t));
        assert!(!cron_matches("0 9 15 1 *", &t)); // minute doesn't match
        assert!(!cron_matches("30 8 15 1 *", &t)); // hour doesn't match
    }

    #[test]
    fn cron_matches_step() {
        // At minute 25, */15 should not match (valid: 0,15,30,45)
        let t = time::OffsetDateTime::from_unix_timestamp(1705310700) // 2024-01-15T09:25:00Z
            .unwrap();
        assert!(!cron_matches("*/15 9 15 1 *", &t)); // 25 is not 0,15,30,45
        let t2 = time::OffsetDateTime::from_unix_timestamp(1705311000) // 2024-01-15T09:30:00Z
            .unwrap();
        assert!(cron_matches("*/15 9 15 1 *", &t2)); // 30 is in */15
    }

    #[test]
    fn cron_matches_range() {
        // At 9:30 on a Thursday (2024-01-15 was Monday, so Thursday is 2024-01-18)
        // 2024-01-18 09:30 UTC = 1705276800 + 3*86400 + 9*3600 + 30*60 = 1705570200
        let t = time::OffsetDateTime::from_unix_timestamp(1705570200).unwrap();
        assert_eq!(t.weekday(), time::Weekday::Thursday);
        assert!(cron_matches("30 9 * * MON-FRI", &t)); // Thursday is in Mon-Fri
    }
}
