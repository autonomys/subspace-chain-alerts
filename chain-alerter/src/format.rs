//! General formatting code.

use crate::subspace::AI3;

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
/// If `None`, return "unknown".
pub fn fmt_amount(val: impl Into<Option<u128>>) -> String {
    if let Some(val) = val.into() {
        format!("{} AI3", val / AI3)
    } else {
        "unknown".to_string()
    }
}
