//! Shared argument-discipline guard for every `ccync` verb.
//!
//! Each `cmd_*` handler calls [`reject_unknown_flags`] at entry so an
//! out-of-set flag or an extra positional argument prints usage and returns
//! `ExitCode::Usage` (64) with zero side effects, instead of being silently
//! ignored. This prevents `ccync remove <id> --dry-run` from silently
//! performing a real destructive remove.

use ccync_engine::ExitCode;

/// Reject any flag not in `allowed_flags`/`value_flags`, or any positional
/// argument beyond `max_positionals`.
///
/// - `args` is the full verb argv including `args[0]` (the verb name), which is
///   skipped.
/// - Any argument starting with `-` (single dash included) is treated as a
///   flag and must match `allowed_flags` or `value_flags`; single-dash tokens
///   are therefore never mistaken for positional ids.
/// - `value_flags` accept both the separated form (`--flag value`, the value is
///   skipped) and the joined form (`--flag=value`, matched by the `--flag=`
///   prefix).
/// - `--help`/`-h` always prints `usage` and returns `Ok`-shaped `Success`
///   even for verbs that never listed it in `allowed_flags`. Callers
///   that want a richer help body opt in by adding `--help`/`-h` to
///   `allowed_flags` and handling it themselves after this guard returns
///   `Ok`; this guard's own generic `usage` line is then skipped in favor of
///   that caller-owned body.
///
/// On rejection, prints a single stderr line naming the offending token plus
/// `usage`, and returns `Err(ExitCode::Usage)`. On success returns `Ok(())`.
pub(crate) fn reject_unknown_flags(
    verb: &str,
    args: &[String],
    allowed_flags: &[&str],
    value_flags: &[&str],
    max_positionals: usize,
    usage: &str,
) -> Result<(), ExitCode> {
    // Every public verb's `--help`/`-h` must print usage and exit Success,
    // not Usage(64). Callers that want a richer help body put
    // `--help`/`-h` in `allowed_flags` themselves and handle it after this
    // guard returns `Ok`; for every other verb, this guard is the only
    // `--help` handling it gets, so it must supply the fallback here.
    if !allowed_flags.contains(&"--help")
        && !allowed_flags.contains(&"-h")
        && args[1..].iter().any(|a| a == "--help" || a == "-h")
    {
        println!("{usage}");
        return Err(ExitCode::Success);
    }

    let mut positionals = 0usize;
    let mut i = 1; // skip args[0] == verb
    while i < args.len() {
        let arg = &args[i];
        if arg.starts_with('-') {
            // Joined value form: --flag=value → validate the flag part.
            if let Some((name, _)) = arg.split_once('=') {
                if value_flags.contains(&name) {
                    i += 1;
                    continue;
                }
                return reject(verb, arg, usage);
            }
            if value_flags.contains(&arg.as_str()) {
                // Separated value form: skip the following value token.
                i += 2;
                continue;
            }
            if allowed_flags.contains(&arg.as_str()) {
                i += 1;
                continue;
            }
            return reject(verb, arg, usage);
        }
        positionals += 1;
        if positionals > max_positionals {
            eprintln!("ccync {verb}: unexpected argument '{arg}'");
            eprintln!("{usage}");
            return Err(ExitCode::Usage);
        }
        i += 1;
    }
    Ok(())
}

fn reject(verb: &str, flag: &str, usage: &str) -> Result<(), ExitCode> {
    eprintln!("ccync {verb}: unknown flag '{flag}'");
    eprintln!("{usage}");
    Err(ExitCode::Usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn allowed_flag_passes() {
        let args = v(&["sync", "--dry-run"]);
        assert!(
            reject_unknown_flags("sync", &args, &["--dry-run", "--yes"], &[], 0, "usage").is_ok()
        );
    }

    #[test]
    fn unknown_flag_is_rejected_as_usage() {
        let args = v(&["sync", "--frobnicate"]);
        assert_eq!(
            reject_unknown_flags("sync", &args, &["--dry-run"], &[], 0, "usage"),
            Err(ExitCode::Usage)
        );
    }

    #[test]
    fn joined_value_flag_passes() {
        let args = v(&["backup", "--output=/tmp/x"]);
        assert!(reject_unknown_flags("backup", &args, &[], &["--output"], 0, "usage").is_ok());
    }

    #[test]
    fn separated_value_flag_value_is_not_misjudged() {
        // The value token "/tmp/x" must not be treated as an unexpected positional.
        let args = v(&["backup", "--output", "/tmp/x"]);
        assert!(reject_unknown_flags("backup", &args, &[], &["--output"], 0, "usage").is_ok());
    }

    #[test]
    fn single_dash_token_is_treated_as_flag_and_rejected() {
        // An unknown single-dash token must be rejected as a flag, never treated
        // as a positional id.
        let args = v(&["remove", "-x"]);
        assert_eq!(
            reject_unknown_flags("remove", &args, &["--yes"], &[], 1, "usage"),
            Err(ExitCode::Usage)
        );
    }

    #[test]
    fn positional_over_cap_is_rejected() {
        let args = v(&["remove", "a", "b"]);
        assert_eq!(
            reject_unknown_flags("remove", &args, &["--yes"], &[], 1, "usage"),
            Err(ExitCode::Usage)
        );
    }

    #[test]
    fn positional_within_cap_passes() {
        let args = v(&["remove", "a"]);
        assert!(reject_unknown_flags("remove", &args, &["--yes"], &[], 1, "usage").is_ok());
    }

    #[test]
    fn unknown_joined_value_flag_is_rejected() {
        let args = v(&["backup", "--bogus=1"]);
        assert_eq!(
            reject_unknown_flags("backup", &args, &[], &["--output"], 0, "usage"),
            Err(ExitCode::Usage)
        );
    }
}
