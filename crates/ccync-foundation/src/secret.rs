//! Secret-detection regex — ccync's own copy.
//!
//! Used by `ccync doctor` manifest scanning (`setup::health`). The ccync product
//! keeps its own separate copy; both products diverge freely (decoupling > DRY),
//! neither imports the other.

use regex::Regex;

/// Returns a compiled regex that matches secret-like values in content.
///
/// Pattern: `(API_KEY|TOKEN|SECRET|PASSWORD|PAT)[:=] <non-whitespace-non-angle>+`
/// Covers common credential key suffixes including PAT (personal access token).
pub fn secret_re() -> Regex {
    Regex::new(r"(API_KEY|TOKEN|SECRET|PASSWORD|PAT)[[:space:]]*[:=][[:space:]]*[^<[:space:]]+")
        .unwrap()
}
