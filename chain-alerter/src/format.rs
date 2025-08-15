//! General formatting code.

use crate::subspace::AI3;
use chrono::{DateTime, TimeDelta, Utc};
use scale_value::Composite;
use std::time::Duration;

/// The maximum length for debug-formatted extrinsic fields.
/// This includes whitespace indentation.
pub const MAX_EXTRINSIC_DEBUG_LENGTH: usize = 200;

/// Truncate a string to a maximum number of characters, respecting UTF-8 character boundaries.
pub fn truncate(s: &mut String, max_chars: usize) {
    // Keep half at the start and end, and remove two more characters to account for "...".
    let kept_len = max_chars / 2 - 2;

    // If the start index is greater than or equal to the end index, then the string is too short
    // and doesn't need to be truncated.
    if let (Some((start_idx, _)), Some((end_idx, _))) = (
        s.char_indices().nth(kept_len),
        s.char_indices().nth_back(kept_len),
    ) && start_idx < end_idx
    {
        // Truncate the string in the middle, inserting "...".
        s.replace_range(start_idx..=end_idx, "...");
    }
}

/// Format an amount in AI3, accepting `u128` or `Option<u128>`.
///
/// Returns a placeholder value if the input is missing.
pub fn fmt_amount(val: impl Into<Option<u128>>) -> String {
    if let Some(val) = val.into() {
        format!("{} AI3", val / AI3)
    } else {
        "unknown".to_string()
    }
}

/// Format a timestamp (a moment in time) as a human-readable string.
///
/// Returns a placeholder value if the input is missing.
pub fn fmt_timestamp(date_time: impl Into<Option<DateTime<Utc>>>) -> String {
    if let Some(date_time) = date_time.into() {
        date_time.format("%Y-%m-%d %H:%M:%S UTC").to_string()
    } else {
        "unknown".to_string()
    }
}

/// Format a duration (an unsigned amount of time) as a human-readable string.
///
/// Returns a placeholder value if the input is missing.
pub fn fmt_duration(duration: impl Into<Option<Duration>>) -> String {
    let Some(duration) = duration.into() else {
        return "missing".to_string();
    };

    // Truncate the duration, sub-second amounts don't matter to us.
    let duration = duration - Duration::from_nanos(duration.subsec_nanos().into());

    humantime::format_duration(duration).to_string()
}

/// Format a time delta (a signed amount of time) as a human-readable string.
///
/// Returns a placeholder value if the input is missing or out of range.
#[expect(
    dead_code,
    reason = "TODO: remove this if we don't use it in any alert"
)]
pub fn fmt_time_delta(time_delta: impl Into<Option<TimeDelta>>) -> String {
    let Some(time_delta) = time_delta.into() else {
        return "missing".to_string();
    };

    let prefix = if time_delta < TimeDelta::zero() {
        "- "
    } else {
        ""
    };

    let Ok(duration) = time_delta.abs().to_std() else {
        return format!("{prefix}out of range");
    };

    format!("{prefix}{}", fmt_duration(duration))
}

/// Format runtime fields as a string, truncating it if it is too long.
pub fn fmt_fields(fields: &Composite<u32>) -> String {
    // The decoded value debug format is extremely verbose, display seems a bit better.
    let mut fields_str = format!("{fields}");
    truncate(&mut fields_str, MAX_EXTRINSIC_DEBUG_LENGTH);

    fields_str
}
