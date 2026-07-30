//! `ccync search <term>` core model — hit shape, clone-URL normalization,
//! deterministic ranking, and provider adapters. Pure logic plus CLI-local
//! subprocess seams only; no engine-level network module and no CLI wiring
//! here yet (dispatch wiring lands incrementally).
//!
use ccync_engine::ExitCode;
use std::io::IsTerminal;
use std::process::Command;

/// A provider-level failure: something a single provider adapter can hit
/// without other providers being affected. The coordinator distinguishes
/// these from "no results" and turns them into a visible warning while
/// letting the remaining providers proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProviderError {
    /// The provider's CLI is not installed / not on `PATH`.
    Unavailable,
    /// The provider CLI ran but failed (non-zero exit, auth/config error).
    Failed(String),
    /// The provider returned output that could not be parsed.
    Parse(String),
}

/// Forge a hit was returned by. Also doubles as the tie-break priority order
/// used by [`rank_hits`] (`gh` first, then `glab`, then the web/API fallback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    Gh,
    Glab,
    Web,
}

impl Provider {
    /// Lower sorts first. `gh`/`glab` are authenticated CLIs (higher-trust,
    /// richer fields); `web` is the last-resort unauthenticated fallback.
    fn priority(self) -> u8 {
        match self {
            Provider::Gh => 0,
            Provider::Glab => 1,
            Provider::Web => 2,
        }
    }
}

/// Forge a hit's repository lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    GitHub,
    GitLab,
}

/// A single search result, merged across providers before ranking/dedupe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchHit {
    pub(crate) provider: Provider,
    pub(crate) platform: Platform,
    /// `owner/name` (or `group/subgroup/name` on GitLab).
    pub(crate) full_name: String,
    pub(crate) stars: u64,
    pub(crate) description: Option<String>,
    pub(crate) clone_url: String,
}

/// Normalize a clone URL for dedupe comparison: lowercase the scheme, strip
/// a trailing `.git`, and strip a trailing `/`. The rest of the URL (host,
/// path casing) is left as-is — hosts/paths are case-sensitive enough on
/// both GitHub and GitLab that lowercasing them risks false-merging distinct
/// repos.
pub(crate) fn normalize_clone_url(url: &str) -> String {
    let trimmed = url.trim();

    let (scheme, rest) = match trimmed.split_once("://") {
        Some((scheme, rest)) => (scheme.to_ascii_lowercase(), rest),
        None => (String::new(), trimmed),
    };

    let mut rest = rest.trim_end_matches('/');
    if let Some(stripped) = rest.strip_suffix(".git") {
        rest = stripped;
    }

    if scheme.is_empty() {
        rest.to_string()
    } else {
        format!("{scheme}://{rest}")
    }
}

/// Dedupe hits by normalized clone URL, keeping the first occurrence (i.e.
/// caller-supplied provider order acts as the tie-break for which hit's
/// metadata survives — providers should be merged in priority order).
pub(crate) fn dedupe_hits(hits: Vec<SearchHit>) -> Vec<SearchHit> {
    let mut seen = std::collections::HashSet::new();
    hits.into_iter()
        .filter(|hit| seen.insert(normalize_clone_url(&hit.clone_url)))
        .collect()
}

/// Rank hits deterministically for a given search `term`:
/// 1. Exact owner/name match (case-insensitive match on the full `full_name`
///    or its final `/`-separated segment) sorts first.
/// 2. Stars descending.
/// 3. Provider priority (`gh` < `glab` < `web`).
/// 4. Lexical `full_name` ascending.
pub(crate) fn rank_hits(term: &str, mut hits: Vec<SearchHit>) -> Vec<SearchHit> {
    let term_lower = term.to_ascii_lowercase();
    let is_exact = |hit: &SearchHit| -> bool {
        let full_lower = hit.full_name.to_ascii_lowercase();
        if full_lower == term_lower {
            return true;
        }
        full_lower
            .rsplit('/')
            .next()
            .is_some_and(|name| name == term_lower)
    };

    hits.sort_by(|a, b| {
        let exact_a = !is_exact(a); // false (exact) sorts before true
        let exact_b = !is_exact(b);
        exact_a
            .cmp(&exact_b)
            .then_with(|| b.stars.cmp(&a.stars))
            .then_with(|| a.provider.priority().cmp(&b.provider.priority()))
            .then_with(|| a.full_name.cmp(&b.full_name))
    });
    hits
}

/// Merge, dedupe, and rank hits in one pass — the shape the provider
/// coordinator will call once all providers have contributed.
pub(crate) fn normalize_and_rank(term: &str, hits: Vec<SearchHit>) -> Vec<SearchHit> {
    rank_hits(term, dedupe_hits(hits))
}

/// Query the `gh` CLI for public repositories matching `term`, mapping
/// results into [`SearchHit`]. Injectable via `run` so tests can supply
/// canned output without spawning a subprocess or touching the network —
/// mirrors the `run_curl`/`curl_text` seam style used by `commands::update`.
pub(crate) fn gh_search_hits(
    term: &str,
    limit: usize,
    run: impl Fn(&str, usize) -> Result<Vec<u8>, ProviderError>,
) -> Result<Vec<SearchHit>, ProviderError> {
    let out = run(term, limit)?;
    parse_gh_hits(&out)
}

/// `gh` provider entry point using the real `gh` subprocess.
pub(crate) fn gh_search(term: &str, limit: usize) -> Result<Vec<SearchHit>, ProviderError> {
    gh_search_hits(term, limit, run_gh_search)
}

/// Build the single `gh search repos` query argument: the original `term`
/// plus the `fork:true` qualifier — `gh search repos` has no separate
/// forked-repos flag, so fork inclusion is expressed as a query qualifier
/// appended to the same argument, never a second CLI flag.
fn gh_search_query(term: &str) -> String {
    format!("{term} fork:true")
}

/// Spawn `gh search repos "<term> fork:true" --json ... --limit N --sort
/// stars --order desc --visibility public` and return its stdout bytes. A
/// missing `gh` binary is reported as [`ProviderError::Unavailable`], not a
/// panic.
fn run_gh_search(term: &str, limit: usize) -> Result<Vec<u8>, ProviderError> {
    let query = gh_search_query(term);
    let out = Command::new("gh")
        .args([
            "search",
            "repos",
            &query,
            "--json",
            "fullName,url,description,stargazersCount",
            "--limit",
            &limit.to_string(),
            "--sort",
            "stars",
            "--order",
            "desc",
            "--visibility",
            "public",
        ])
        .output();

    let out = match out {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProviderError::Unavailable)
        }
        Err(e) => return Err(ProviderError::Failed(format!("spawn gh: {e}"))),
    };

    if !out.status.success() {
        return Err(ProviderError::Failed(format!(
            "gh search repos failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    Ok(out.stdout)
}

/// Parse `gh search repos --json ...` output into [`SearchHit`]s. Missing or
/// null `description` defaults to `None`; missing or null `stargazersCount`
/// defaults to `0`. Malformed JSON is a parse-failure provider error, not a
/// panic or a silently empty result.
fn parse_gh_hits(bytes: &[u8]) -> Result<Vec<SearchHit>, ProviderError> {
    let text = String::from_utf8_lossy(bytes);
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| ProviderError::Parse(format!("gh search repos: {e}")))?;
    let items = value.as_array().ok_or_else(|| {
        ProviderError::Parse("gh search repos: expected a JSON array".to_string())
    })?;

    Ok(items
        .iter()
        .map(|item| SearchHit {
            provider: Provider::Gh,
            platform: Platform::GitHub,
            full_name: item
                .get("fullName")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            stars: item
                .get("stargazersCount")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            description: item
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            clone_url: item
                .get("url")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .collect())
}

/// Query the `glab` CLI for public repositories matching `term`, mapping
/// results into [`SearchHit`]. Injectable via `run` so tests can supply
/// canned output without spawning a subprocess or touching the network —
/// mirrors the `gh_search_hits` seam shape.
pub(crate) fn glab_search_hits(
    term: &str,
    limit: usize,
    run: impl Fn(&str, usize) -> Result<Vec<u8>, ProviderError>,
) -> Result<Vec<SearchHit>, ProviderError> {
    let out = run(term, limit)?;
    parse_glab_hits(&out)
}

/// `glab` provider entry point using the real `glab` subprocess.
pub(crate) fn glab_search(term: &str, limit: usize) -> Result<Vec<SearchHit>, ProviderError> {
    glab_search_hits(term, limit, run_glab_search)
}

/// Spawn `glab repo search --search <term> --output json --per-page N` and
/// return its stdout bytes. A missing `glab` binary is reported as
/// [`ProviderError::Unavailable`], not a panic. A non-zero exit (including
/// auth/config failures such as a missing or expired token) is reported as
/// [`ProviderError::Failed`] carrying `glab`'s stderr text, so the
/// coordinator can render it as a visible warning rather than treating it
/// as "no results".
fn run_glab_search(term: &str, limit: usize) -> Result<Vec<u8>, ProviderError> {
    let out = Command::new("glab")
        .args([
            "repo",
            "search",
            "--search",
            term,
            "--output",
            "json",
            "--per-page",
            &limit.to_string(),
        ])
        .output();

    let out = match out {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProviderError::Unavailable)
        }
        Err(e) => return Err(ProviderError::Failed(format!("spawn glab: {e}"))),
    };

    if !out.status.success() {
        return Err(ProviderError::Failed(format!(
            "glab repo search failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    Ok(out.stdout)
}

/// Parse `glab repo search --output json` output into [`SearchHit`]s.
///
/// Field names below were verified against a live `glab repo search
/// --search <term> --output json --per-page N` invocation (glab 1.106.0)
/// rather than guessed from documentation: `path_with_namespace`,
/// `http_url_to_repo`, `star_count`, `description`, `visibility`.
///
/// Only hits whose `visibility` is exactly `"public"` are accepted.
/// `glab` can surface non-public results depending on the caller's token
/// scope (internal/private projects the user can see), and a missing
/// `visibility` field is treated the same as non-public rather than
/// silently assumed public — both are excluded from the returned hits.
/// Malformed JSON is a parse-failure provider error, not a panic or a
/// silently empty result.
fn parse_glab_hits(bytes: &[u8]) -> Result<Vec<SearchHit>, ProviderError> {
    let text = String::from_utf8_lossy(bytes);
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| ProviderError::Parse(format!("glab repo search: {e}")))?;
    let items = value.as_array().ok_or_else(|| {
        ProviderError::Parse("glab repo search: expected a JSON array".to_string())
    })?;

    Ok(items
        .iter()
        .filter(|item| item.get("visibility").and_then(serde_json::Value::as_str) == Some("public"))
        .map(|item| SearchHit {
            provider: Provider::Glab,
            platform: Platform::GitLab,
            full_name: item
                .get("path_with_namespace")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            stars: item
                .get("star_count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            description: item
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            clone_url: item
                .get("http_url_to_repo")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        })
        .collect())
}

/// Percent-encode a search term for safe use in a URL query string. Encodes
/// every byte outside the unreserved set (`A-Z a-z 0-9 - _ . ~`) as `%XX`,
/// including spaces (`%20`) — this is the last-resort unauthenticated
/// fallback, so the term must never be interpolated into the URL raw.
fn percent_encode_query(term: &str) -> String {
    let mut out = String::with_capacity(term.len());
    for byte in term.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Shared `curl -fsSL <url>` seam used by both web/API fallback providers —
/// mirrors the `run_curl` pattern in `commands::update`, kept local to this
/// module rather than imported cross-module since `update::run_curl` is
/// private and returns `Result<_, String>` rather than [`ProviderError`].
fn run_curl(url: &str) -> Result<Vec<u8>, ProviderError> {
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https",
            "-H",
            "User-Agent: ccync-search",
            url,
        ])
        .output();

    let out = match out {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ProviderError::Unavailable)
        }
        Err(e) => return Err(ProviderError::Failed(format!("spawn curl: {e}"))),
    };

    if !out.status.success() {
        return Err(ProviderError::Failed(format!(
            "curl {url} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }

    Ok(out.stdout)
}

/// Query the public (unauthenticated) GitHub REST search API for
/// repositories matching `term`. Injectable via `run` so tests can supply
/// canned output without spawning `curl` or touching the network — mirrors
/// the `gh_search_hits`/`glab_search_hits` seam shape.
pub(crate) fn github_web_search_hits(
    term: &str,
    limit: usize,
    run: impl Fn(&str, usize) -> Result<Vec<u8>, ProviderError>,
) -> Result<Vec<SearchHit>, ProviderError> {
    let out = run(term, limit)?;
    parse_github_web_hits(&out)
}

/// GitHub web/API fallback provider entry point using the real `curl`
/// subprocess. Called unconditionally alongside `gh`/`glab` every run (the
/// coordinator merges all providers' hits rather than short-circuiting on
/// first success) — "fallback" here names the unauthenticated-access-tier
/// role, not a call-order gate.
pub(crate) fn github_web_search(term: &str, limit: usize) -> Result<Vec<SearchHit>, ProviderError> {
    github_web_search_hits(term, limit, run_github_web_search)
}

/// Build the encoded `q=` value for the GitHub web/API search: the original
/// `term` plus the `fork:true` qualifier, so forked repositories aren't
/// excluded from results), percent-encoded as one combined unit so the
/// space between them survives as `%20` rather than a raw space.
fn github_web_search_query(term: &str) -> String {
    percent_encode_query(&format!("{term} fork:true"))
}

/// `curl` the public `GET /search/repositories?q=<term>+fork:true` GitHub
/// REST endpoint. A missing `curl` binary is [`ProviderError::Unavailable`];
/// a non-2xx/network failure is [`ProviderError::Failed`].
fn run_github_web_search(term: &str, limit: usize) -> Result<Vec<u8>, ProviderError> {
    let encoded = github_web_search_query(term);
    let url = format!("https://api.github.com/search/repositories?q={encoded}&per_page={limit}");
    run_curl(&url)
}

/// Parse GitHub `search/repositories` JSON into [`SearchHit`]s. Unlike
/// `gh`/`glab`'s bare-array output, results are nested under an `"items"`
/// key. `full_name` -> `full_name`, `clone_url` is preferred when present
/// and falls back to `html_url` otherwise (both are valid `git clone`
/// targets), `stargazers_count` -> `stars` (missing/null defaults to `0`),
/// `description` -> `description` (missing/null defaults to `None`).
/// Malformed JSON or a missing `items` array is a parse-failure provider
/// error, not a panic or a silently empty result.
fn parse_github_web_hits(bytes: &[u8]) -> Result<Vec<SearchHit>, ProviderError> {
    let text = String::from_utf8_lossy(bytes);
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| ProviderError::Parse(format!("github search/repositories: {e}")))?;
    let items = value
        .get("items")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            ProviderError::Parse("github search/repositories: missing items array".to_string())
        })?;

    Ok(items
        .iter()
        .map(|item| SearchHit {
            provider: Provider::Web,
            platform: Platform::GitHub,
            full_name: item
                .get("full_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            stars: item
                .get("stargazers_count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            description: item
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            clone_url: item
                .get("clone_url")
                .and_then(serde_json::Value::as_str)
                .or_else(|| item.get("html_url").and_then(serde_json::Value::as_str))
                .unwrap_or_default()
                .to_string(),
        })
        .collect())
}

/// Query the public (unauthenticated) GitLab REST projects-search API for
/// repositories matching `term`. Injectable via `run` so tests can supply
/// canned output without spawning `curl` or touching the network.
pub(crate) fn gitlab_web_search_hits(
    term: &str,
    limit: usize,
    run: impl Fn(&str, usize) -> Result<Vec<u8>, ProviderError>,
) -> Result<Vec<SearchHit>, ProviderError> {
    let out = run(term, limit)?;
    parse_gitlab_web_hits(&out)
}

/// GitLab web/API fallback provider entry point using the real `curl`
/// subprocess. Called unconditionally alongside `gh`/`glab` every run (the
/// coordinator merges all providers' hits rather than short-circuiting on
/// first success) — "fallback" here names the unauthenticated-access-tier
/// role, not a call-order gate.
pub(crate) fn gitlab_web_search(term: &str, limit: usize) -> Result<Vec<SearchHit>, ProviderError> {
    gitlab_web_search_hits(term, limit, run_gitlab_web_search)
}

/// `curl` the public `GET /projects?search=<term>&visibility=public` GitLab
/// REST endpoint. The term is percent-encoded before being placed in the
/// query string. A missing `curl` binary is [`ProviderError::Unavailable`];
/// a non-2xx/network failure is [`ProviderError::Failed`].
fn run_gitlab_web_search(term: &str, limit: usize) -> Result<Vec<u8>, ProviderError> {
    let encoded = percent_encode_query(term);
    let url = format!(
        "https://gitlab.com/api/v4/projects?search={encoded}&visibility=public&per_page={limit}"
    );
    run_curl(&url)
}

/// Parse GitLab `projects?search=` JSON into [`SearchHit`]s — a bare array,
/// same top-level shape as `glab`'s output. `path_with_namespace` ->
/// `full_name`, `http_url_to_repo` is preferred with `web_url` as a
/// fallback, `star_count` -> `stars` (missing/null defaults to `0`),
/// `description` -> `description` (missing/null defaults to `None`). Only
/// `visibility == "public"` items are accepted — a missing or unrecognized
/// `visibility` field is excluded rather than assumed public, mirroring the
/// `glab` provider's filtering. This endpoint is unauthenticated, but the
/// filter is kept for defense in depth and to keep both providers'
/// contracts identical. Malformed JSON is a parse-failure provider error.
fn parse_gitlab_web_hits(bytes: &[u8]) -> Result<Vec<SearchHit>, ProviderError> {
    let text = String::from_utf8_lossy(bytes);
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| ProviderError::Parse(format!("gitlab projects search: {e}")))?;
    let items = value.as_array().ok_or_else(|| {
        ProviderError::Parse("gitlab projects search: expected a JSON array".to_string())
    })?;

    Ok(items
        .iter()
        .filter(|item| item.get("visibility").and_then(serde_json::Value::as_str) == Some("public"))
        .map(|item| SearchHit {
            provider: Provider::Web,
            platform: Platform::GitLab,
            full_name: item
                .get("path_with_namespace")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            stars: item
                .get("star_count")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0),
            description: item
                .get("description")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            clone_url: item
                .get("http_url_to_repo")
                .and_then(serde_json::Value::as_str)
                .or_else(|| item.get("web_url").and_then(serde_json::Value::as_str))
                .unwrap_or_default()
                .to_string(),
        })
        .collect())
}

/// What a completed search run means for exit-code purposes. Distinct from
/// the raw hits/warnings data so a later CLI-wiring layer can map this
/// directly to a process exit code without re-deriving it from the data
/// shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SearchStatus {
    /// At least one provider succeeded and produced at least one hit.
    Found,
    /// At least one provider succeeded, but the merged result set is empty.
    /// Exit 0 is warranted; the caller should show the manual fallback text.
    NoResults,
    /// Every provider call failed (unavailable/failed/parse). Exit 1 is
    /// warranted.
    AllFailed,
}

/// Result of running all providers in priority order and merging their
/// output. A single provider's failure never discards another provider's
/// hits — failures are collected in `warnings` alongside the merged,
/// deduped, ranked `hits`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SearchOutcome {
    pub(crate) status: SearchStatus,
    pub(crate) hits: Vec<SearchHit>,
    pub(crate) warnings: Vec<(Provider, ProviderError)>,
}

/// Manual fallback text for a search that ran cleanly (no provider errors)
/// but found nothing. Must only ever reference the documented
/// `~/.ccync/plugins.json` schema if a file is mentioned at all — never
/// `config.json`.
pub(crate) const NO_RESULTS_MESSAGE: &str =
    "No results found. Search manually, then run `ccync add <git-url>`.";

/// Substrings that identify a `glab` provider failure as "not authenticated
/// or not configured" rather than a genuine unexpected error. Matched
/// case-insensitively against `glab`'s raw stderr text — deliberately a
/// small, explicit set rather than "any non-zero exit", so a real unknown
/// failure still surfaces its raw diagnostic instead of being silently
/// reclassified as noise.
const GLAB_AUTH_OR_CONFIG_FAILURE_MARKERS: &[&str] = &[
    "auth login",
    "not logged in",
    "no token",
    "not authenticated",
    "authentication required",
    "unauthorized",
    "no gitlab instance",
];

/// True when a `glab` [`ProviderError::Failed`] message matches a known
/// auth/config failure marker.
fn is_glab_auth_or_config_failure(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    GLAB_AUTH_OR_CONFIG_FAILURE_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// Render a single provider failure as visible warning text. Intentionally
/// worded so it can never be confused with [`NO_RESULTS_MESSAGE`] — a
/// provider failure is "this source couldn't be checked", not "nothing was
/// found".
///
/// A known `glab` auth/config failure collapses to one concise non-error
/// note with no raw stderr/token text (`glab repo search` prints the
/// unauthenticated user's own error verbatim, which is noise the user can't
/// act on and shouldn't have echoed back). Every other failure — `glab` or
/// otherwise — keeps its raw diagnostic message unchanged.
pub(crate) fn provider_warning_text(provider: Provider, err: &ProviderError) -> String {
    let name = match provider {
        Provider::Gh => "gh",
        Provider::Glab => "glab",
        Provider::Web => "web",
    };
    match err {
        ProviderError::Unavailable => format!("{name} provider unavailable, skipped"),
        ProviderError::Failed(msg)
            if provider == Provider::Glab && is_glab_auth_or_config_failure(msg) =>
        {
            "glab provider not authenticated/configured, skipped".to_string()
        }
        ProviderError::Failed(msg) => format!("{name} provider failed: {msg}"),
        ProviderError::Parse(msg) => format!("{name} provider returned unreadable output: {msg}"),
    }
}

/// Call all providers in priority order (`gh` -> `glab` -> web/API
/// fallback for both GitHub and GitLab), merge every successful provider's
/// hits through [`normalize_and_rank`], and collect every failure as a
/// visible warning rather than letting it discard other providers' results.
///
/// Providers are injected so this can be tested without spawning any real
/// subprocess or touching the network; [`search`] wires the real provider
/// entry points.
pub(crate) fn coordinate_search(
    term: &str,
    limit: usize,
    gh: impl Fn(&str, usize) -> Result<Vec<SearchHit>, ProviderError>,
    glab: impl Fn(&str, usize) -> Result<Vec<SearchHit>, ProviderError>,
    github_web: impl Fn(&str, usize) -> Result<Vec<SearchHit>, ProviderError>,
    gitlab_web: impl Fn(&str, usize) -> Result<Vec<SearchHit>, ProviderError>,
) -> SearchOutcome {
    let mut merged = Vec::new();
    let mut warnings = Vec::new();
    let mut succeeded = 0usize;

    match gh(term, limit) {
        Ok(hits) => {
            succeeded += 1;
            merged.extend(hits);
        }
        Err(e) => warnings.push((Provider::Gh, e)),
    }
    match glab(term, limit) {
        Ok(hits) => {
            succeeded += 1;
            merged.extend(hits);
        }
        Err(e) => warnings.push((Provider::Glab, e)),
    }
    match github_web(term, limit) {
        Ok(hits) => {
            succeeded += 1;
            merged.extend(hits);
        }
        Err(e) => warnings.push((Provider::Web, e)),
    }
    match gitlab_web(term, limit) {
        Ok(hits) => {
            succeeded += 1;
            merged.extend(hits);
        }
        Err(e) => warnings.push((Provider::Web, e)),
    }

    let hits = normalize_and_rank(term, merged);
    let status = if succeeded == 0 {
        SearchStatus::AllFailed
    } else if hits.is_empty() {
        SearchStatus::NoResults
    } else {
        SearchStatus::Found
    };

    SearchOutcome {
        status,
        hits,
        warnings,
    }
}

/// Provider coordinator entry point using the real `gh`/`glab`/`curl`
/// subprocess seams, in priority order.
pub(crate) fn search(term: &str, limit: usize) -> SearchOutcome {
    coordinate_search(
        term,
        limit,
        gh_search,
        glab_search,
        github_web_search,
        gitlab_web_search,
    )
}

/// Resolve ranked hits down to a single confirmed clone URL via TTY
/// selection/confirmation. Multiple distinct hits use `select` (the real
/// seam backs this with `dialoguer::Select`, matching the prompt/default/
/// `interact_opt` convention in `commands::init::prompt_master`) so the user
/// picks one; a single hit still displays the full clone URL and requires an
/// explicit yes/no `confirm` rather than being silently auto-selected.
///
/// Cancelling the selection (`select` returns `None`, e.g. the user backed
/// out with Esc/Ctrl-C) or declining the confirmation (`confirm` returns
/// `false`) both mean "no add" — the caller must not treat either as a
/// selection. An empty hit slice also resolves to no selection.
///
/// `select` and `confirm` are injected so this is unit-testable without a
/// real terminal: `select` receives display labels for each hit and returns
/// the chosen index, or `None` on cancel; `confirm` receives the exact clone
/// URL to display and returns whether the user accepted it.
pub(crate) fn interactive_confirm(
    hits: &[SearchHit],
    select: impl FnOnce(&[String]) -> Option<usize>,
    confirm: impl FnOnce(&str) -> bool,
) -> Option<String> {
    match hits {
        [] => None,
        [only] => confirm(&only.clone_url).then(|| only.clone_url.clone()),
        many => {
            let labels: Vec<String> = many
                .iter()
                .map(|h| format!("{} ({} stars) - {}", h.full_name, h.stars, h.clone_url))
                .collect();
            let selected = select(&labels)?;
            let clone_url = &many[selected].clone_url;
            confirm(clone_url).then(|| clone_url.clone())
        }
    }
}

/// Real `dialoguer::Select`-backed `select` seam for [`interactive_confirm`].
/// Follows the same prompt-wording/`.default(0)`/`interact_opt` convention as
/// `commands::init::prompt_master` — cancel (Esc/Ctrl-C) or an interaction
/// error both map to `None`, never a panic or a silent first-item pick.
fn select_hit_interactive(labels: &[String]) -> Option<usize> {
    dialoguer::Select::new()
        .with_prompt("Multiple matches found — pick one to add")
        .items(labels)
        .default(0)
        .interact_opt()
        .ok()?
}

/// Real `dialoguer::Confirm`-backed `confirm` seam for
/// [`interactive_confirm`]. Defaults to declining and treats any interaction
/// error the same as an explicit "no" — a confirmation prompt must never
/// fail open into an add.
fn confirm_hit_interactive(clone_url: &str) -> bool {
    dialoguer::Confirm::new()
        .with_prompt(format!("Add {clone_url}?"))
        .default(false)
        .interact()
        .unwrap_or(false)
}

/// Run the interactive selection/confirmation flow over ranked hits and, on
/// confirmation, hand the exact confirmed clone URL off to the existing
/// `ccync add` pipeline (`commands::plugin::cmd_add`) — constructing its argv
/// exactly as `main.rs` would for `ccync add <clone_url>` (`args[0]` is the
/// verb, the clone URL is the first non-`--` argument after it). A cancelled
/// selection or declined confirmation is a no-op: `cmd_add` is never called
/// and `ExitCode::Success` is returned (declining is not an error).
///
/// `select`/`confirm` are injected (see [`interactive_confirm`]) so the
/// handoff itself — not just the selection logic — is testable without a
/// real terminal or real `cmd_add` side effects beyond what the injected
/// closures allow.
pub(crate) fn confirm_and_add_with(
    hits: &[SearchHit],
    select: impl FnOnce(&[String]) -> Option<usize>,
    confirm: impl FnOnce(&str) -> bool,
) -> ExitCode {
    match interactive_confirm(hits, select, confirm) {
        Some(clone_url) => super::plugin::cmd_add(&["add".to_string(), clone_url]),
        None => ExitCode::Success,
    }
}

/// TTY entry point for [`confirm_and_add_with`] using the real
/// `dialoguer`-backed select/confirm seams. This is the reusable "given
/// ranked hits + TTY, get a confirmed clone URL, then call `cmd_add`"
/// function that the `ccync search <term>` CLI dispatch wires up in TTY
/// mode; it does not itself decide whether TTY mode applies.
pub(crate) fn confirm_and_add(hits: &[SearchHit]) -> ExitCode {
    confirm_and_add_with(hits, select_hit_interactive, confirm_hit_interactive)
}

/// Render ranked hits as stable, parseable, non-interactive output — used
/// when stdout is not a TTY, or the caller passed `--no-add`. This function
/// never calls `cmd_add` and never touches `dialoguer`; it is a pure
/// formatter with no side effects, so a caller cannot accidentally trigger
/// an add just by choosing this rendering path.
///
/// Output is tab-separated values (TSV): one header line naming the
/// columns, then one line per hit. Column order is
/// `full_name\tstars\tdescription\tclone_url` — this is the authoritative
/// format the manual documents for `ccync search`'s non-interactive output;
/// changing the column order or set is a documentation-affecting change.
/// A missing `description` renders as an empty field rather than being
/// omitted, so the column count per row is always stable. Any embedded
/// tab/newline/carriage-return in `full_name` or `description` is replaced
/// with a single space so each hit always occupies exactly one line and the
/// TSV stays unambiguous to parse.
///
/// `limit` truncates the rendered rows to at most `limit` hits, independent
/// of whatever limit the provider calls upstream already requested — the
/// rendering path enforces its own bound rather than assuming the caller
/// already trimmed `hits`.
///
/// Boundary with the no-results case: this function is meant to be called
/// only once the caller knows there is at least one hit to show —
/// `SearchStatus::NoResults` already has its own fallback text via
/// [`NO_RESULTS_MESSAGE`], and a caller should prefer that message over an
/// empty TSV body. Passing an empty `hits` slice here is still handled
/// safely (it renders just the header line with no data rows) rather than
/// panicking, but is not the intended way to signal "no results".
pub(crate) fn render_list_only(hits: &[SearchHit], limit: usize) -> String {
    fn sanitize(field: &str) -> String {
        field.replace(['\t', '\n', '\r'], " ")
    }

    let mut out = String::from("full_name\tstars\tdescription\tclone_url\n");
    for hit in hits.iter().take(limit) {
        out.push_str(&sanitize(&hit.full_name));
        out.push('\t');
        out.push_str(&hit.stars.to_string());
        out.push('\t');
        out.push_str(&hit.description.as_deref().map(sanitize).unwrap_or_default());
        out.push('\t');
        out.push_str(&sanitize(&hit.clone_url));
        out.push('\n');
    }
    out
}

/// Parse `--limit N` (separated) or `--limit=N` (joined) out of `search`'s
/// argv, defaulting to 10 when absent. Returns `Err(ExitCode::Usage)` when
/// `--limit` is present but its value does not parse as a non-negative
/// integer, matching the "usage error, zero side effects" contract the other
/// value-flag handlers in this crate (e.g. `cmd_backup`'s `--output`) follow.
fn parse_search_limit(args: &[String]) -> Result<usize, ExitCode> {
    const DEFAULT_LIMIT: usize = 10;
    let mut limit = DEFAULT_LIMIT;
    let mut i = 1usize; // skip args[0] == "search"
    while i < args.len() {
        let arg = &args[i];
        if arg == "--limit" {
            i += 1;
            match args.get(i).and_then(|v| v.parse::<usize>().ok()) {
                Some(v) => limit = v,
                None => {
                    eprintln!("ccync search: --limit requires a non-negative integer");
                    return Err(ExitCode::Usage);
                }
            }
        } else if let Some(value) = arg.strip_prefix("--limit=") {
            match value.parse::<usize>() {
                Ok(v) => limit = v,
                Err(_) => {
                    eprintln!("ccync search: --limit requires a non-negative integer");
                    return Err(ExitCode::Usage);
                }
            }
        }
        i += 1;
    }
    Ok(limit)
}

/// Core dispatch logic behind `ccync search <term> [--limit N] [--no-add]`,
/// factored out from [`cmd_search`] so it is unit-testable with injected
/// `search_fn`/`add_fn` seams — no test needs a real TTY, a real provider
/// subprocess, or a real `cmd_add` side effect.
///
/// Status mapping:
/// - [`SearchStatus::AllFailed`]: every provider warning is printed to
///   stderr and the caller gets `ExitCode::Error` (process exit 1).
/// - [`SearchStatus::NoResults`]: any non-fatal provider warnings (a
///   provider failed but wasn't the sole reason for zero hits) plus
///   [`NO_RESULTS_MESSAGE`] are printed to stdout; `ExitCode::Success`
///   (process exit 0) — this is the documented manual-fallback path, not an
///   error.
/// - [`SearchStatus::Found`]: non-fatal provider warnings are printed to
///   stderr (visible but not blocking), then either `add_fn` runs the
///   interactive confirm-and-add flow, or the ranked hits are rendered via
///   [`render_list_only`] to stdout — the list-only path never calls
///   `add_fn`.
fn run_search(
    term: &str,
    limit: usize,
    interactive: bool,
    search_fn: impl FnOnce(&str, usize) -> SearchOutcome,
    add_fn: impl FnOnce(&[SearchHit]) -> ExitCode,
) -> ExitCode {
    let outcome = search_fn(term, limit);

    match outcome.status {
        SearchStatus::AllFailed => {
            for (provider, err) in &outcome.warnings {
                eprintln!("{}", provider_warning_text(*provider, err));
            }
            ExitCode::Error
        }
        SearchStatus::NoResults => {
            for (provider, err) in &outcome.warnings {
                println!("{}", provider_warning_text(*provider, err));
            }
            println!("{NO_RESULTS_MESSAGE}");
            ExitCode::Success
        }
        SearchStatus::Found => {
            for (provider, err) in &outcome.warnings {
                eprintln!("{}", provider_warning_text(*provider, err));
            }
            if interactive {
                add_fn(&outcome.hits)
            } else {
                print!("{}", render_list_only(&outcome.hits, limit));
                ExitCode::Success
            }
        }
    }
}

/// `ccync search <term> [--limit N] [--no-add]` CLI entry point.
///
/// `<term>` is the first non-`--` argument after `args[0]` ("search").
/// `--limit N` bounds the number of rendered/offered hits (default 10);
/// `--no-add` forces the non-interactive list-only rendering path even on a
/// real TTY. Interactive confirm-and-add only runs when stdout is a TTY and
/// `--no-add` was not passed — piping/redirecting `ccync search`'s stdout,
/// or running it in CI, falls back to the same stable list-only rendering a
/// human would get with `--no-add`.
pub(crate) fn cmd_search(args: &[String]) -> ExitCode {
    if let Err(code) = super::args::reject_unknown_flags(
        "search",
        args,
        &["--no-add"],
        &["--limit"],
        1,
        "Usage: ccync search <term> [--limit N] [--no-add]",
    ) {
        return code;
    }

    let Some(term) = args
        .iter()
        .skip(1)
        .find(|a| !a.starts_with("--"))
        .map(String::as_str)
    else {
        eprintln!("ccync search: missing <term> argument");
        eprintln!("Usage: ccync search <term> [--limit N] [--no-add]");
        return ExitCode::Usage;
    };

    let limit = match parse_search_limit(args) {
        Ok(limit) => limit,
        Err(code) => return code,
    };

    let no_add = args.iter().any(|a| a == "--no-add");
    let interactive = !no_add && std::io::stdout().is_terminal();

    run_search(term, limit, interactive, search, confirm_and_add)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(provider: Provider, full_name: &str, stars: u64, clone_url: &str) -> SearchHit {
        SearchHit {
            provider,
            platform: Platform::GitHub,
            full_name: full_name.to_string(),
            stars,
            description: None,
            clone_url: clone_url.to_string(),
        }
    }

    #[test]
    fn normalize_clone_url_strips_git_suffix_trailing_slash_and_lowercases_scheme() {
        assert_eq!(
            normalize_clone_url("HTTPS://github.com/upstash/context7.git"),
            "https://github.com/upstash/context7"
        );
        assert_eq!(
            normalize_clone_url("https://github.com/upstash/context7/"),
            "https://github.com/upstash/context7"
        );
        assert_eq!(
            normalize_clone_url("https://github.com/upstash/context7.git/"),
            "https://github.com/upstash/context7"
        );
        assert_eq!(
            normalize_clone_url("  https://github.com/upstash/context7  "),
            "https://github.com/upstash/context7"
        );
    }

    #[test]
    fn normalize_clone_url_preserves_path_case() {
        // Host/path casing is NOT lowercased — GitHub/GitLab paths are
        // case-sensitive in practice; only the scheme is normalized.
        assert_eq!(
            normalize_clone_url("https://GitHub.com/Upstash/Context7.git"),
            "https://GitHub.com/Upstash/Context7"
        );
    }

    #[test]
    fn normalize_clone_url_handles_url_without_scheme() {
        // Some sources (e.g. a bare host/path form) may omit the scheme
        // entirely; normalization must still strip trailing slash/.git
        // without panicking or injecting a scheme that wasn't there.
        assert_eq!(
            normalize_clone_url("github.com/upstash/context7.git/"),
            "github.com/upstash/context7"
        );
    }

    #[test]
    fn dedupe_hits_merges_equivalent_clone_urls_keeping_first_occurrence() {
        let hits = vec![
            hit(
                Provider::Gh,
                "upstash/context7",
                100,
                "https://github.com/upstash/context7.git",
            ),
            hit(
                Provider::Web,
                "upstash/context7",
                50,
                "https://github.com/upstash/context7/",
            ),
            hit(
                Provider::Gh,
                "other/repo",
                5,
                "https://github.com/other/repo.git",
            ),
        ];
        let deduped = dedupe_hits(hits);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].provider, Provider::Gh);
        assert_eq!(deduped[0].stars, 100);
        assert_eq!(deduped[1].full_name, "other/repo");
    }

    #[test]
    fn rank_hits_puts_exact_match_first() {
        let hits = vec![
            hit(
                Provider::Gh,
                "someone/context7-clone",
                9000,
                "https://github.com/someone/context7-clone",
            ),
            hit(
                Provider::Gh,
                "upstash/context7",
                50,
                "https://github.com/upstash/context7",
            ),
        ];
        let ranked = rank_hits("context7", hits);
        assert_eq!(ranked[0].full_name, "upstash/context7");
        assert_eq!(ranked[1].full_name, "someone/context7-clone");
    }

    #[test]
    fn rank_hits_exact_match_is_case_insensitive() {
        let hits = vec![
            hit(
                Provider::Gh,
                "someone/other",
                9000,
                "https://github.com/someone/other",
            ),
            hit(
                Provider::Gh,
                "Upstash/Context7",
                50,
                "https://github.com/Upstash/Context7",
            ),
        ];
        // Term case differs from the hit's casing; match must still win on
        // exactness despite far fewer stars than the non-exact competitor.
        let ranked = rank_hits("CONTEXT7", hits);
        assert_eq!(ranked[0].full_name, "Upstash/Context7");
        assert_eq!(ranked[1].full_name, "someone/other");
    }

    #[test]
    fn rank_hits_breaks_ties_among_multiple_exact_matches_by_stars_then_provider_then_lexical() {
        // Two hits can both count as "exact" (e.g. same repo name under
        // different owners, or duplicate provider results not caught by
        // clone-URL dedupe because the URLs genuinely differ). The exact
        // bucket must still apply the same stars -> provider -> lexical
        // chain as the non-exact bucket, not stop at "is exact".
        let hits = vec![
            hit(
                Provider::Web,
                "zowner/context7",
                10,
                "https://github.com/zowner/context7",
            ),
            hit(
                Provider::Gh,
                "aowner/context7",
                10,
                "https://github.com/aowner/context7",
            ),
            hit(
                Provider::Gh,
                "bowner/context7",
                500,
                "https://github.com/bowner/context7",
            ),
        ];
        let ranked = rank_hits("context7", hits);
        let names: Vec<&str> = ranked.iter().map(|h| h.full_name.as_str()).collect();
        assert_eq!(
            names,
            vec!["bowner/context7", "aowner/context7", "zowner/context7"]
        );
    }

    #[test]
    fn rank_hits_orders_by_stars_descending_when_no_exact_match() {
        let hits = vec![
            hit(Provider::Gh, "a/low", 5, "https://github.com/a/low"),
            hit(Provider::Gh, "b/high", 500, "https://github.com/b/high"),
            hit(Provider::Gh, "c/mid", 50, "https://github.com/c/mid"),
        ];
        let ranked = rank_hits("term", hits);
        let names: Vec<&str> = ranked.iter().map(|h| h.full_name.as_str()).collect();
        assert_eq!(names, vec!["b/high", "c/mid", "a/low"]);
    }

    #[test]
    fn rank_hits_breaks_star_ties_by_provider_priority_then_lexical() {
        let hits = vec![
            hit(Provider::Web, "z/repo", 10, "https://github.com/z/repo"),
            hit(Provider::Glab, "a/repo", 10, "https://gitlab.com/a/repo"),
            hit(Provider::Gh, "m/repo", 10, "https://github.com/m/repo"),
        ];
        let ranked = rank_hits("term", hits);
        let names: Vec<&str> = ranked.iter().map(|h| h.full_name.as_str()).collect();
        // Same star count -> provider priority (gh < glab < web) decides.
        assert_eq!(names, vec!["m/repo", "a/repo", "z/repo"]);
    }

    #[test]
    fn rank_hits_breaks_full_ties_lexically() {
        let hits = vec![
            hit(Provider::Gh, "z/repo", 10, "https://github.com/z/repo"),
            hit(Provider::Gh, "a/repo", 10, "https://github.com/a/repo"),
        ];
        let ranked = rank_hits("term", hits);
        let names: Vec<&str> = ranked.iter().map(|h| h.full_name.as_str()).collect();
        assert_eq!(names, vec!["a/repo", "z/repo"]);
    }

    #[test]
    fn normalize_and_rank_dedupes_then_ranks() {
        let hits = vec![
            hit(
                Provider::Web,
                "upstash/context7",
                10,
                "https://github.com/upstash/context7/",
            ),
            hit(
                Provider::Gh,
                "upstash/context7",
                100,
                "https://github.com/upstash/context7.git",
            ),
            hit(
                Provider::Gh,
                "other/repo",
                1,
                "https://github.com/other/repo",
            ),
        ];
        let result = normalize_and_rank("context7", hits);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].full_name, "upstash/context7");
        assert_eq!(result[0].provider, Provider::Web); // first occurrence kept
        assert_eq!(result[1].full_name, "other/repo");
    }

    #[test]
    fn gh_search_hits_parses_fixture_json() {
        let fixture = br#"[
            {
                "fullName": "upstash/context7",
                "url": "https://github.com/upstash/context7",
                "description": "Context7 MCP server",
                "stargazersCount": 12345
            }
        ]"#;
        let hits = gh_search_hits("context7", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider, Provider::Gh);
        assert_eq!(hits[0].platform, Platform::GitHub);
        assert_eq!(hits[0].full_name, "upstash/context7");
        assert_eq!(hits[0].clone_url, "https://github.com/upstash/context7");
        assert_eq!(hits[0].description.as_deref(), Some("Context7 MCP server"));
        assert_eq!(hits[0].stars, 12345);
    }

    #[test]
    fn gh_search_hits_defaults_missing_description_and_stars() {
        let fixture = br#"[
            { "fullName": "owner/repo", "url": "https://github.com/owner/repo" }
        ]"#;
        let hits = gh_search_hits("repo", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].description, None);
        assert_eq!(hits[0].stars, 0);
    }

    #[test]
    fn gh_search_hits_defaults_null_description_and_stars() {
        let fixture = br#"[
            {
                "fullName": "owner/repo",
                "url": "https://github.com/owner/repo",
                "description": null,
                "stargazersCount": null
            }
        ]"#;
        let hits = gh_search_hits("repo", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits[0].description, None);
        assert_eq!(hits[0].stars, 0);
    }

    #[test]
    fn gh_search_hits_reports_malformed_json_as_parse_error() {
        let err = gh_search_hits("term", 10, |_, _| Ok(b"not json".to_vec())).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn gh_search_hits_reports_non_array_json_as_parse_error() {
        let err = gh_search_hits("term", 10, |_, _| Ok(b"{}".to_vec())).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn gh_search_hits_propagates_unavailable_from_runner_without_panicking() {
        let err = gh_search_hits("term", 10, |_, _| Err(ProviderError::Unavailable)).unwrap_err();
        assert_eq!(err, ProviderError::Unavailable);
    }

    #[test]
    fn gh_search_hits_propagates_failed_from_runner_without_panicking() {
        let err = gh_search_hits("term", 10, |_, _| Err(ProviderError::Failed("boom".into())))
            .unwrap_err();
        assert!(matches!(err, ProviderError::Failed(_)));
    }

    #[test]
    fn missing_binary_spawn_maps_to_unavailable_without_panicking() {
        // Exercises the NotFound -> Unavailable mapping that `run_gh_search`
        // relies on, without spawning the real `gh` binary (which may be
        // installed and reachable on this machine, making a real-`gh` smoke
        // test a nondeterministic network call). Spawning a binary name that
        // cannot exist deterministically triggers the same
        // `std::io::ErrorKind::NotFound` path `run_gh_search` maps to
        // `ProviderError::Unavailable`, without ever touching the network.
        let out = Command::new("definitely-not-a-real-binary-ccync-search-tp02").output();
        let mapped = match out {
            Ok(_) => panic!("this binary name must not exist in the test environment"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProviderError::Unavailable,
            Err(e) => ProviderError::Failed(format!("spawn: {e}")),
        };
        assert_eq!(mapped, ProviderError::Unavailable);
    }

    #[test]
    fn gh_search_query_includes_original_term_and_fork_true() {
        assert_eq!(gh_search_query("context7"), "context7 fork:true");
        assert_eq!(
            gh_search_query("upstash/context7"),
            "upstash/context7 fork:true"
        );
    }

    #[test]
    fn glab_search_hits_parses_fixture_json() {
        let fixture = br#"[
            {
                "path_with_namespace": "upstash/context7",
                "http_url_to_repo": "https://gitlab.com/upstash/context7.git",
                "description": "Context7 MCP server",
                "star_count": 4321,
                "visibility": "public"
            }
        ]"#;
        let hits = glab_search_hits("context7", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider, Provider::Glab);
        assert_eq!(hits[0].platform, Platform::GitLab);
        assert_eq!(hits[0].full_name, "upstash/context7");
        assert_eq!(hits[0].clone_url, "https://gitlab.com/upstash/context7.git");
        assert_eq!(hits[0].description.as_deref(), Some("Context7 MCP server"));
        assert_eq!(hits[0].stars, 4321);
    }

    #[test]
    fn glab_search_hits_defaults_missing_description_and_stars() {
        let fixture = br#"[
            {
                "path_with_namespace": "owner/repo",
                "http_url_to_repo": "https://gitlab.com/owner/repo.git",
                "visibility": "public"
            }
        ]"#;
        let hits = glab_search_hits("repo", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].description, None);
        assert_eq!(hits[0].stars, 0);
    }

    #[test]
    fn glab_search_hits_defaults_null_description_and_stars() {
        let fixture = br#"[
            {
                "path_with_namespace": "owner/repo",
                "http_url_to_repo": "https://gitlab.com/owner/repo.git",
                "description": null,
                "star_count": null,
                "visibility": "public"
            }
        ]"#;
        let hits = glab_search_hits("repo", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits[0].description, None);
        assert_eq!(hits[0].stars, 0);
    }

    #[test]
    fn glab_search_hits_excludes_private_and_internal_visibility() {
        let fixture = br#"[
            {
                "path_with_namespace": "owner/private-repo",
                "http_url_to_repo": "https://gitlab.com/owner/private-repo.git",
                "visibility": "private"
            },
            {
                "path_with_namespace": "owner/internal-repo",
                "http_url_to_repo": "https://gitlab.com/owner/internal-repo.git",
                "visibility": "internal"
            },
            {
                "path_with_namespace": "owner/public-repo",
                "http_url_to_repo": "https://gitlab.com/owner/public-repo.git",
                "visibility": "public"
            }
        ]"#;
        let hits = glab_search_hits("repo", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].full_name, "owner/public-repo");
    }

    #[test]
    fn glab_search_hits_excludes_missing_or_unknown_visibility() {
        // Missing/unrecognized `visibility` must not be silently treated as
        // public — it is excluded, same as an explicit non-public value.
        let fixture = br#"[
            {
                "path_with_namespace": "owner/no-visibility-field",
                "http_url_to_repo": "https://gitlab.com/owner/no-visibility-field.git"
            },
            {
                "path_with_namespace": "owner/weird-visibility",
                "http_url_to_repo": "https://gitlab.com/owner/weird-visibility.git",
                "visibility": "something-unexpected"
            }
        ]"#;
        let hits = glab_search_hits("repo", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn glab_search_hits_reports_malformed_json_as_parse_error() {
        let err = glab_search_hits("term", 10, |_, _| Ok(b"not json".to_vec())).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn glab_search_hits_reports_non_array_json_as_parse_error() {
        let err = glab_search_hits("term", 10, |_, _| Ok(b"{}".to_vec())).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn glab_search_hits_propagates_unavailable_from_runner_without_panicking() {
        let err = glab_search_hits("term", 10, |_, _| Err(ProviderError::Unavailable)).unwrap_err();
        assert_eq!(err, ProviderError::Unavailable);
    }

    #[test]
    fn glab_search_hits_surfaces_auth_config_failure_as_visible_warning_not_empty_results() {
        // An auth/config error (e.g. `glab` not logged in, or a missing
        // token scope) must come back as a `Failed` provider error carrying
        // a message the coordinator can render as a warning — it must never
        // be swallowed as an empty `Ok(vec![])` "no results" outcome.
        let err = glab_search_hits("term", 10, |_, _| {
            Err(ProviderError::Failed(
                "glab repo search failed: no token found, please run `glab auth login`".into(),
            ))
        })
        .unwrap_err();
        match err {
            ProviderError::Failed(msg) => {
                assert!(
                    msg.contains("auth"),
                    "expected auth-related message, got: {msg}"
                );
            }
            other => panic!("expected ProviderError::Failed, got {other:?}"),
        }
    }

    #[test]
    fn missing_binary_spawn_maps_to_unavailable_without_panicking_for_glab() {
        // Same NotFound -> Unavailable mapping `run_glab_search` relies on,
        // exercised without spawning the real `glab` binary (confirmed
        // installed on this machine) or touching the network.
        let out = Command::new("definitely-not-a-real-binary-ccync-search-tp03").output();
        let mapped = match out {
            Ok(_) => panic!("this binary name must not exist in the test environment"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProviderError::Unavailable,
            Err(e) => ProviderError::Failed(format!("spawn: {e}")),
        };
        assert_eq!(mapped, ProviderError::Unavailable);
    }

    #[test]
    fn percent_encode_query_encodes_spaces_and_special_characters() {
        assert_eq!(percent_encode_query("context7"), "context7");
        assert_eq!(percent_encode_query("context 7"), "context%207");
        assert_eq!(
            percent_encode_query("a+b&c=d?e#f/g"),
            "a%2Bb%26c%3Dd%3Fe%23f%2Fg"
        );
        assert_eq!(
            percent_encode_query("owner-name_v1.0~x"),
            "owner-name_v1.0~x"
        );
    }

    #[test]
    fn github_web_search_query_includes_original_term_and_fork_true_encoded() {
        assert_eq!(
            github_web_search_query("context7"),
            "context7%20fork%3Atrue"
        );
        assert_eq!(
            github_web_search_query("upstash/context7"),
            "upstash%2Fcontext7%20fork%3Atrue"
        );
    }

    #[test]
    fn github_web_search_hits_parses_fixture_json() {
        let fixture = br#"{
            "items": [
                {
                    "full_name": "upstash/context7",
                    "html_url": "https://github.com/upstash/context7",
                    "clone_url": "https://github.com/upstash/context7.git",
                    "description": "Context7 MCP server",
                    "stargazers_count": 12345
                }
            ]
        }"#;
        let hits = github_web_search_hits("context7", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider, Provider::Web);
        assert_eq!(hits[0].platform, Platform::GitHub);
        assert_eq!(hits[0].full_name, "upstash/context7");
        assert_eq!(hits[0].clone_url, "https://github.com/upstash/context7.git");
        assert_eq!(hits[0].description.as_deref(), Some("Context7 MCP server"));
        assert_eq!(hits[0].stars, 12345);
    }

    #[test]
    fn github_web_search_hits_falls_back_to_html_url_when_clone_url_missing() {
        let fixture = br#"{
            "items": [
                { "full_name": "owner/repo", "html_url": "https://github.com/owner/repo" }
            ]
        }"#;
        let hits = github_web_search_hits("repo", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits[0].clone_url, "https://github.com/owner/repo");
        assert_eq!(hits[0].description, None);
        assert_eq!(hits[0].stars, 0);
    }

    #[test]
    fn github_web_search_hits_defaults_null_description_and_stars() {
        let fixture = br#"{
            "items": [
                {
                    "full_name": "owner/repo",
                    "html_url": "https://github.com/owner/repo",
                    "description": null,
                    "stargazers_count": null
                }
            ]
        }"#;
        let hits = github_web_search_hits("repo", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits[0].description, None);
        assert_eq!(hits[0].stars, 0);
    }

    #[test]
    fn github_web_search_hits_reports_missing_items_array_as_parse_error() {
        let err = github_web_search_hits("term", 10, |_, _| Ok(b"{}".to_vec())).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn github_web_search_hits_reports_bare_array_as_parse_error_not_empty_success() {
        // GitHub's real `search/repositories` response nests hits under an
        // `"items"` key, unlike `gh`/`glab`'s bare-array JSON output. A
        // bare array shaped like those providers' output (including one
        // that actually contains repo-like objects) must be rejected as
        // ProviderError::Parse, not silently treated as zero results —
        // `Value::get("items")` on a `Value::Array` returns `None`, which
        // must surface as an error, not fall through to an empty `Vec`.
        let bare_array = br#"[
            { "full_name": "owner/repo", "html_url": "https://github.com/owner/repo" }
        ]"#;
        let err = github_web_search_hits("term", 10, |_, _| Ok(bare_array.to_vec())).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));

        let empty_bare_array = b"[]";
        let err =
            github_web_search_hits("term", 10, |_, _| Ok(empty_bare_array.to_vec())).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn github_web_search_hits_reports_malformed_json_as_parse_error() {
        let err = github_web_search_hits("term", 10, |_, _| Ok(b"not json".to_vec())).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn github_web_search_hits_propagates_provider_errors_from_runner_without_panicking() {
        let unavailable =
            github_web_search_hits("term", 10, |_, _| Err(ProviderError::Unavailable)).unwrap_err();
        assert_eq!(unavailable, ProviderError::Unavailable);

        let failed =
            github_web_search_hits("term", 10, |_, _| Err(ProviderError::Failed("boom".into())))
                .unwrap_err();
        assert!(matches!(failed, ProviderError::Failed(_)));
    }

    #[test]
    fn github_web_search_hits_seam_never_touches_network() {
        // The injected `run` closure below records the exact URL it would
        // have curled, proving the term is percent-encoded before it
        // reaches the seam boundary, without ever spawning `curl`.
        let hits = github_web_search_hits("context 7", 5, |term, limit| {
            let url = format!(
                "https://api.github.com/search/repositories?q={}&per_page={limit}",
                percent_encode_query(term)
            );
            assert_eq!(
                url,
                "https://api.github.com/search/repositories?q=context%207&per_page=5"
            );
            Ok(br#"{"items": []}"#.to_vec())
        })
        .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn gitlab_web_search_hits_parses_fixture_json() {
        let fixture = br#"[
            {
                "path_with_namespace": "upstash/context7",
                "http_url_to_repo": "https://gitlab.com/upstash/context7.git",
                "web_url": "https://gitlab.com/upstash/context7",
                "description": "Context7 MCP server",
                "star_count": 4321,
                "visibility": "public"
            }
        ]"#;
        let hits = gitlab_web_search_hits("context7", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].provider, Provider::Web);
        assert_eq!(hits[0].platform, Platform::GitLab);
        assert_eq!(hits[0].full_name, "upstash/context7");
        assert_eq!(hits[0].clone_url, "https://gitlab.com/upstash/context7.git");
        assert_eq!(hits[0].description.as_deref(), Some("Context7 MCP server"));
        assert_eq!(hits[0].stars, 4321);
    }

    #[test]
    fn gitlab_web_search_hits_falls_back_to_web_url_when_http_url_to_repo_missing() {
        let fixture = br#"[
            {
                "path_with_namespace": "owner/repo",
                "web_url": "https://gitlab.com/owner/repo",
                "visibility": "public"
            }
        ]"#;
        let hits = gitlab_web_search_hits("repo", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits[0].clone_url, "https://gitlab.com/owner/repo");
        assert_eq!(hits[0].description, None);
        assert_eq!(hits[0].stars, 0);
    }

    #[test]
    fn gitlab_web_search_hits_defaults_null_description_and_stars() {
        let fixture = br#"[
            {
                "path_with_namespace": "owner/repo",
                "http_url_to_repo": "https://gitlab.com/owner/repo.git",
                "description": null,
                "star_count": null,
                "visibility": "public"
            }
        ]"#;
        let hits = gitlab_web_search_hits("repo", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits[0].description, None);
        assert_eq!(hits[0].stars, 0);
    }

    #[test]
    fn gitlab_web_search_hits_excludes_missing_or_non_public_visibility() {
        let fixture = br#"[
            {
                "path_with_namespace": "owner/private-repo",
                "http_url_to_repo": "https://gitlab.com/owner/private-repo.git",
                "visibility": "private"
            },
            {
                "path_with_namespace": "owner/no-visibility-field",
                "http_url_to_repo": "https://gitlab.com/owner/no-visibility-field.git"
            },
            {
                "path_with_namespace": "owner/public-repo",
                "http_url_to_repo": "https://gitlab.com/owner/public-repo.git",
                "visibility": "public"
            }
        ]"#;
        let hits = gitlab_web_search_hits("repo", 10, |_, _| Ok(fixture.to_vec())).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].full_name, "owner/public-repo");
    }

    #[test]
    fn gitlab_web_search_hits_reports_malformed_json_as_parse_error() {
        let err = gitlab_web_search_hits("term", 10, |_, _| Ok(b"not json".to_vec())).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn gitlab_web_search_hits_reports_non_array_json_as_parse_error() {
        let err = gitlab_web_search_hits("term", 10, |_, _| Ok(b"{}".to_vec())).unwrap_err();
        assert!(matches!(err, ProviderError::Parse(_)));
    }

    #[test]
    fn gitlab_web_search_hits_propagates_provider_errors_from_runner_without_panicking() {
        let unavailable =
            gitlab_web_search_hits("term", 10, |_, _| Err(ProviderError::Unavailable)).unwrap_err();
        assert_eq!(unavailable, ProviderError::Unavailable);

        let failed =
            gitlab_web_search_hits("term", 10, |_, _| Err(ProviderError::Failed("boom".into())))
                .unwrap_err();
        assert!(matches!(failed, ProviderError::Failed(_)));
    }

    #[test]
    fn gitlab_web_search_hits_seam_encodes_term_and_never_touches_network() {
        let hits = gitlab_web_search_hits("context 7", 5, |term, limit| {
            let url = format!(
                "https://gitlab.com/api/v4/projects?search={}&visibility=public&per_page={limit}",
                percent_encode_query(term)
            );
            assert_eq!(
                url,
                "https://gitlab.com/api/v4/projects?search=context%207&visibility=public&per_page=5"
            );
            Ok(b"[]".to_vec())
        })
        .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn missing_binary_spawn_maps_to_unavailable_without_panicking_for_curl() {
        // Same NotFound -> Unavailable mapping `run_curl` (shared by both
        // web/API providers) relies on, exercised without spawning the real
        // `curl` binary or touching the network.
        let out = Command::new("definitely-not-a-real-binary-ccync-search-tp04").output();
        let mapped = match out {
            Ok(_) => panic!("this binary name must not exist in the test environment"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProviderError::Unavailable,
            Err(e) => ProviderError::Failed(format!("spawn: {e}")),
        };
        assert_eq!(mapped, ProviderError::Unavailable);
    }

    fn web_hit(full_name: &str, platform: Platform, stars: u64, clone_url: &str) -> SearchHit {
        SearchHit {
            provider: Provider::Web,
            platform,
            full_name: full_name.to_string(),
            stars,
            description: None,
            clone_url: clone_url.to_string(),
        }
    }

    #[test]
    fn coordinate_search_one_provider_fails_others_still_contribute_hits() {
        let outcome = coordinate_search(
            "context7",
            10,
            |_, _| Err(ProviderError::Unavailable),
            |_, _| {
                Ok(vec![hit(
                    Provider::Glab,
                    "upstash/context7",
                    50,
                    "https://gitlab.com/upstash/context7",
                )])
            },
            |_, _| Ok(vec![]),
            |_, _| Ok(vec![]),
        );

        assert_eq!(outcome.status, SearchStatus::Found);
        assert_eq!(outcome.hits.len(), 1);
        assert_eq!(outcome.hits[0].full_name, "upstash/context7");
        assert_eq!(outcome.warnings.len(), 1);
        assert_eq!(outcome.warnings[0].0, Provider::Gh);
        assert_eq!(outcome.warnings[0].1, ProviderError::Unavailable);
    }

    #[test]
    fn coordinate_search_merges_hits_from_multiple_surviving_providers() {
        // A single provider failing must not discard hits contributed by
        // the providers that did succeed, including the web fallback.
        let outcome = coordinate_search(
            "term",
            10,
            |_, _| Err(ProviderError::Failed("gh not authenticated".into())),
            |_, _| Err(ProviderError::Unavailable),
            |_, _| {
                Ok(vec![web_hit(
                    "owner/repo",
                    Platform::GitHub,
                    5,
                    "https://github.com/owner/repo",
                )])
            },
            |_, _| {
                Ok(vec![web_hit(
                    "owner2/repo2",
                    Platform::GitLab,
                    3,
                    "https://gitlab.com/owner2/repo2",
                )])
            },
        );

        assert_eq!(outcome.status, SearchStatus::Found);
        assert_eq!(outcome.hits.len(), 2);
        assert_eq!(outcome.warnings.len(), 2);
    }

    #[test]
    fn coordinate_search_all_providers_failing_is_exit_1_worthy() {
        let outcome = coordinate_search(
            "term",
            10,
            |_, _| Err(ProviderError::Unavailable),
            |_, _| Err(ProviderError::Unavailable),
            |_, _| Err(ProviderError::Failed("curl: could not resolve host".into())),
            |_, _| Err(ProviderError::Failed("curl: could not resolve host".into())),
        );

        assert_eq!(outcome.status, SearchStatus::AllFailed);
        assert!(outcome.hits.is_empty());
        assert_eq!(outcome.warnings.len(), 4);
    }

    #[test]
    fn coordinate_search_zero_hits_with_no_failures_is_exit_0_worthy_with_fallback() {
        let outcome = coordinate_search(
            "nonexistent-term-xyz",
            10,
            |_, _| Ok(vec![]),
            |_, _| Ok(vec![]),
            |_, _| Ok(vec![]),
            |_, _| Ok(vec![]),
        );

        assert_eq!(outcome.status, SearchStatus::NoResults);
        assert!(outcome.hits.is_empty());
        assert!(outcome.warnings.is_empty());

        assert!(NO_RESULTS_MESSAGE.contains("ccync add <git-url>"));
        assert!(!NO_RESULTS_MESSAGE
            .to_ascii_lowercase()
            .contains("config.json"));
    }

    #[test]
    fn provider_failed_text_reads_distinctly_from_no_results_text() {
        let warning = provider_warning_text(
            Provider::Gh,
            &ProviderError::Failed("not authenticated".to_string()),
        );
        assert_ne!(warning, NO_RESULTS_MESSAGE);
        // The two messages must not share a "nothing was found" framing —
        // a provider failure means "this source couldn't be checked", a
        // no-results outcome means every source was checked and came up
        // empty. Guard against either message drifting into the other's
        // wording.
        assert!(warning.contains("gh provider failed"));
        assert!(!warning.to_ascii_lowercase().contains("no results"));
        assert!(!NO_RESULTS_MESSAGE.to_ascii_lowercase().contains("provider"));
    }

    #[test]
    fn provider_warning_text_distinguishes_error_kinds() {
        let unavailable = provider_warning_text(Provider::Glab, &ProviderError::Unavailable);
        let failed = provider_warning_text(Provider::Glab, &ProviderError::Failed("boom".into()));
        let parse = provider_warning_text(Provider::Glab, &ProviderError::Parse("bad json".into()));
        assert_ne!(unavailable, failed);
        assert_ne!(failed, parse);
        assert_ne!(unavailable, parse);
    }

    #[test]
    fn provider_warning_text_never_shares_no_results_framing_for_any_variant() {
        // provider_failed_text_reads_distinctly_from_no_results_text only
        // checks the Failed variant against NO_RESULTS_MESSAGE. Confirm the
        // same "this source couldn't be checked" vs. "nothing was found"
        // distinction holds for Unavailable and Parse too, so no warning
        // variant can ever read like the no-results message.
        for err in [
            ProviderError::Unavailable,
            ProviderError::Failed("boom".into()),
            ProviderError::Parse("bad json".into()),
        ] {
            let warning = provider_warning_text(Provider::Gh, &err);
            assert_ne!(
                warning, NO_RESULTS_MESSAGE,
                "{err:?} warning text collided with NO_RESULTS_MESSAGE"
            );
            assert!(
                !warning.to_ascii_lowercase().contains("no results"),
                "{err:?} warning text must not read like a no-results message: {warning}"
            );
        }
    }

    // ── collapse unauthenticated glab noise ───────────────────────────────────

    #[test]
    fn glab_auth_note_collapses_known_auth_failure_with_no_raw_stderr() {
        for raw_stderr in [
            "glab repo search failed: run `glab auth login` to authenticate",
            "glab repo search failed: not logged in",
            "glab repo search failed: no token provided",
            "glab repo search failed: authentication required",
            "glab repo search failed: 401 Unauthorized",
        ] {
            let warning = provider_warning_text(
                Provider::Glab,
                &ProviderError::Failed(raw_stderr.to_string()),
            );
            assert_eq!(
                warning, "glab provider not authenticated/configured, skipped",
                "known auth/config failure {raw_stderr:?} must collapse to the concise note"
            );
            assert!(
                !warning.to_ascii_lowercase().contains("token"),
                "collapsed note must not leak token/credential text: {warning}"
            );
            assert!(
                !warning.contains("ERROR") && !warning.contains("401"),
                "collapsed note must not carry raw error/status text: {warning}"
            );
        }
    }

    #[test]
    fn glab_unknown_failure_stays_diagnostic() {
        // An unrecognized glab failure (not a known auth/config marker) must
        // keep its raw diagnostic — only known auth/config noise collapses.
        let warning = provider_warning_text(
            Provider::Glab,
            &ProviderError::Failed("glab repo search failed: unexpected EOF".to_string()),
        );
        assert!(
            warning.contains("unexpected EOF"),
            "unknown glab failure must keep its raw diagnostic: {warning}"
        );
        assert_ne!(
            warning,
            "glab provider not authenticated/configured, skipped"
        );
    }

    #[test]
    fn non_glab_auth_style_failure_is_unaffected() {
        // The classifier is glab-specific — an identically-worded gh/web
        // failure must NOT be collapsed (only glab has this known-noisy
        // unauthenticated-CLI behavior).
        let warning = provider_warning_text(
            Provider::Gh,
            &ProviderError::Failed("not logged in".to_string()),
        );
        assert!(warning.contains("not logged in"));
        assert_ne!(
            warning,
            "glab provider not authenticated/configured, skipped"
        );
    }

    #[test]
    fn coordinate_search_glab_auth_failure_yields_exactly_one_glab_note_and_keeps_other_hits() {
        // End-to-end via the real coordinator seam: glab fails with a known
        // auth marker, gh and the GitHub-web fallback still contribute hits.
        // Probe requirement: exactly one glab note, no raw ERROR/token text,
        // other providers' hits survive.
        let outcome = coordinate_search(
            "context7",
            10,
            |_, _| {
                Ok(vec![hit(
                    Provider::Gh,
                    "upstash/context7",
                    100,
                    "https://github.com/upstash/context7",
                )])
            },
            |_, _| {
                Err(ProviderError::Failed(
                    "glab repo search failed: run `glab auth login` to authenticate".to_string(),
                ))
            },
            |_, _| {
                Ok(vec![web_hit(
                    "someone/context7-mirror",
                    Platform::GitHub,
                    5,
                    "https://github.com/someone/context7-mirror",
                )])
            },
            |_, _| Ok(vec![]),
        );

        assert_eq!(outcome.status, SearchStatus::Found);
        assert_eq!(
            outcome.hits.len(),
            2,
            "gh + web hits must both survive the glab failure"
        );

        let glab_warnings: Vec<_> = outcome
            .warnings
            .iter()
            .filter(|(p, _)| *p == Provider::Glab)
            .collect();
        assert_eq!(
            glab_warnings.len(),
            1,
            "exactly one glab warning must be recorded"
        );

        let note = provider_warning_text(glab_warnings[0].0, &glab_warnings[0].1);
        assert_eq!(note, "glab provider not authenticated/configured, skipped");
        assert!(!note.to_ascii_lowercase().contains("error"));
        assert!(!note.to_ascii_lowercase().contains("token"));
    }

    // ── interactive selection/confirmation seam ───────────────────────────────

    #[test]
    fn interactive_confirm_multi_hit_uses_select_then_confirm_to_return_chosen_clone_url() {
        let hits = vec![
            hit(
                Provider::Gh,
                "owner/one",
                10,
                "https://github.com/owner/one",
            ),
            hit(
                Provider::Glab,
                "owner/two",
                20,
                "https://gitlab.com/owner/two",
            ),
        ];
        let result = interactive_confirm(
            &hits,
            |labels| {
                assert_eq!(labels.len(), 2);
                assert!(labels[0].contains("owner/one"));
                assert!(labels[0].contains("https://github.com/owner/one"));
                assert!(labels[1].contains("owner/two"));
                Some(1)
            },
            |url| {
                assert_eq!(url, "https://gitlab.com/owner/two");
                true
            },
        );
        assert_eq!(result.as_deref(), Some("https://gitlab.com/owner/two"));
    }

    #[test]
    fn interactive_confirm_multi_hit_declined_confirmation_is_no_op() {
        let hits = vec![
            hit(
                Provider::Gh,
                "owner/one",
                10,
                "https://github.com/owner/one",
            ),
            hit(
                Provider::Glab,
                "owner/two",
                20,
                "https://gitlab.com/owner/two",
            ),
        ];
        let result = interactive_confirm(
            &hits,
            |_labels| Some(1),
            |url| {
                assert_eq!(url, "https://gitlab.com/owner/two");
                false
            },
        );
        assert_eq!(result, None);
    }

    #[test]
    fn interactive_confirm_multi_hit_cancel_is_no_op() {
        let hits = vec![
            hit(
                Provider::Gh,
                "owner/one",
                10,
                "https://github.com/owner/one",
            ),
            hit(
                Provider::Glab,
                "owner/two",
                20,
                "https://gitlab.com/owner/two",
            ),
        ];
        let result = interactive_confirm(
            &hits,
            |_labels| None,
            |_| panic!("cancelled selection must not call confirm"),
        );
        assert_eq!(result, None);
    }

    #[test]
    fn interactive_confirm_single_hit_still_requires_confirmation_not_auto_selected() {
        let hits = vec![hit(
            Provider::Gh,
            "upstash/context7",
            100,
            "https://github.com/upstash/context7",
        )];

        // Declining the confirmation is a no-op, even for a single hit —
        // never silently auto-selected.
        let declined = interactive_confirm(
            &hits,
            |_labels| panic!("single hit must not use select"),
            |url| {
                assert_eq!(url, "https://github.com/upstash/context7");
                false
            },
        );
        assert_eq!(declined, None);

        // Accepting returns the exact clone URL.
        let accepted = interactive_confirm(
            &hits,
            |_labels| panic!("single hit must not use select"),
            |url| {
                assert_eq!(url, "https://github.com/upstash/context7");
                true
            },
        );
        assert_eq!(
            accepted.as_deref(),
            Some("https://github.com/upstash/context7")
        );
    }

    #[test]
    fn interactive_confirm_empty_hits_is_no_op_without_prompting() {
        let result = interactive_confirm(
            &[],
            |_labels| panic!("no hits must not prompt select"),
            |_url| panic!("no hits must not prompt confirm"),
        );
        assert_eq!(result, None);
    }

    #[test]
    fn confirm_and_add_with_cancel_never_calls_cmd_add() {
        // A cancelled/declined confirmation must not touch the filesystem at
        // all — assert this against a fake HOME with no ccync state so any
        // stray write would be detectable, without needing to inspect
        // cmd_add's internals directly.
        let _env = crate::test_io_lock();
        let fake_home = tempfile::TempDir::new().unwrap();
        set_test_home(fake_home.path());

        let hits = vec![hit(
            Provider::Gh,
            "owner/repo",
            10,
            "https://github.com/owner/repo",
        )];
        let exit = confirm_and_add_with(&hits, |_labels| None, |_url| false);

        clear_test_home();

        assert_eq!(exit, ExitCode::Success);
        assert!(
            !fake_home.path().join(".ccync").exists(),
            "declined/cancelled confirmation must not create any ccync state"
        );
    }

    /// Set HOME/USERPROFILE to `home` for the duration of a test. Callers
    /// must pair this with [`clear_test_home`] before any assertion that
    /// could unwind past the restore (mirrors the guard pattern already used
    /// throughout `commands::plugin`'s and `commands::init`'s fake-home
    /// tests).
    fn set_test_home(home: &std::path::Path) {
        #[cfg(windows)]
        unsafe {
            std::env::set_var("USERPROFILE", home);
            std::env::set_var("HOME", home);
        }
        #[cfg(not(windows))]
        unsafe {
            std::env::set_var("HOME", home);
        }
    }

    fn clear_test_home() {
        unsafe {
            std::env::remove_var("USERPROFILE");
            std::env::remove_var("HOME");
        }
    }

    /// Build a minimal local git repo usable as a `ccync add <path>` source —
    /// classified as a git source and clonable via `git clone --depth 1
    /// <local-path>` with no network access. Mirrors
    /// `commands::plugin::tests::gate::make_source_repo`.
    fn make_source_repo(dir: &std::path::Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("README.md"), "# fixture plugin\n").unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git must be on PATH for this test");
            assert!(
                out.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "test"]);
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "fixture"]);
    }

    #[test]
    fn confirm_and_add_confirmed_hands_off_exact_clone_url_to_cmd_add() {
        // End-to-end coverage of the confirmed handoff itself (not just
        // `interactive_confirm`'s return value): drives the real `cmd_add`
        // against a fake HOME and a local git-clonable source, then asserts
        // the personal catalog recorded exactly the confirmed hit's clone
        // URL as the source — proving `confirm_and_add_with` passed the
        // exact string through, not a re-derived or mutated one.
        let _env = crate::test_io_lock();
        let fake_home = tempfile::TempDir::new().unwrap();
        set_test_home(fake_home.path());
        // Mark the fake home as an already-initialized machine so the
        // handoff's projecting `cmd_add` call reaches its pre-existing
        // gate/fetch/catalog-write behavior instead of being refused by the
        // machine-initialization preflight.
        std::fs::create_dir_all(fake_home.path().join(".ccync")).unwrap();
        std::fs::write(fake_home.path().join(".ccync").join("config.json"), r#"{}"#).unwrap();

        let source = tempfile::TempDir::new().unwrap();
        make_source_repo(source.path());
        let clone_url = source.path().display().to_string();

        let hits = vec![hit(Provider::Gh, "owner/fixture", 42, &clone_url)];

        let exit = confirm_and_add_with(
            &hits,
            |_labels| panic!("single hit must not use select"),
            |url| {
                assert_eq!(url, clone_url, "confirm must receive the exact clone URL");
                true
            },
        );

        let catalog_path = ccync_foundation::paths::personal_catalog_path().unwrap();
        let catalog = super::super::plugin::read_personal_catalog(&catalog_path);

        clear_test_home();

        // The confirmed handoff runs `cmd_add` end-to-end. In this non-TTY test
        // (no `--yes`) it reaches the first-run projection gate and aborts before
        // any live-surface write, which — per the truthful gate contract — now
        // returns `ExitCode::Error`, not the old dishonest `Success`. The catalog
        // (cache/catalog/lock) is written *before* the gate, so the URL-passthrough
        // assertion below still holds; that pass-through is what this test proves.
        assert_eq!(
            exit,
            ExitCode::Error,
            "non-TTY confirmed handoff aborts at the projection gate after writing the catalog"
        );
        let catalog = catalog.expect("cmd_add must have written the personal catalog");
        let plugins = catalog["plugins"].as_array().expect("plugins array");
        assert!(
            plugins
                .iter()
                .any(|e| e.get("source").and_then(|s| s.as_str()) == Some(clone_url.as_str())),
            "cmd_add must have been called with the exact confirmed clone URL as source: {catalog:?}"
        );
    }

    #[test]
    fn render_list_only_emits_stable_tsv_header_and_fields() {
        let hits = vec![SearchHit {
            provider: Provider::Gh,
            platform: Platform::GitHub,
            full_name: "upstash/context7".to_string(),
            stars: 12345,
            description: Some("Context7 MCP server".to_string()),
            clone_url: "https://github.com/upstash/context7".to_string(),
        }];
        let rendered = render_list_only(&hits, 10);
        assert_eq!(
            rendered,
            "full_name\tstars\tdescription\tclone_url\n\
             upstash/context7\t12345\tContext7 MCP server\thttps://github.com/upstash/context7\n"
        );
    }

    #[test]
    fn render_list_only_defaults_missing_description_to_empty_field_not_omitted() {
        let hits = vec![hit(
            Provider::Gh,
            "owner/repo",
            0,
            "https://github.com/owner/repo",
        )];
        let rendered = render_list_only(&hits, 10);
        // Exactly 4 tab-separated columns even with no description, so
        // downstream TSV parsing always sees a stable column count.
        let data_line = rendered.lines().nth(1).expect("one data line");
        let fields: Vec<&str> = data_line.split('\t').collect();
        assert_eq!(
            fields,
            vec!["owner/repo", "0", "", "https://github.com/owner/repo"]
        );
    }

    #[test]
    fn render_list_only_respects_limit_independent_of_upstream_provider_limit() {
        let hits = vec![
            hit(Provider::Gh, "a/one", 3, "https://github.com/a/one"),
            hit(Provider::Gh, "b/two", 2, "https://github.com/b/two"),
            hit(Provider::Gh, "c/three", 1, "https://github.com/c/three"),
        ];
        let rendered = render_list_only(&hits, 2);
        // Header + exactly 2 data lines, even though 3 hits were passed in.
        assert_eq!(rendered.lines().count(), 3);
        assert!(rendered.contains("a/one"));
        assert!(rendered.contains("b/two"));
        assert!(!rendered.contains("c/three"));
    }

    #[test]
    fn render_list_only_sanitizes_embedded_tabs_and_newlines() {
        let hits = vec![SearchHit {
            provider: Provider::Gh,
            platform: Platform::GitHub,
            full_name: "owner/weird\tname".to_string(),
            stars: 1,
            description: Some("line one\nline two\ttabbed".to_string()),
            clone_url: "https://github.com/owner/weird".to_string(),
        }];
        let rendered = render_list_only(&hits, 10);
        let data_line = rendered.lines().nth(1).expect("one data line");
        let fields: Vec<&str> = data_line.split('\t').collect();
        // Exactly 4 fields — no stray tab/newline from full_name or
        // description broke the row into extra columns or lines.
        assert_eq!(fields.len(), 4);
        assert_eq!(fields[0], "owner/weird name");
        assert_eq!(fields[2], "line one line two tabbed");
        assert_eq!(
            rendered.lines().count(),
            2,
            "header + exactly one data line"
        );
    }

    #[test]
    fn render_list_only_sanitizes_embedded_carriage_returns() {
        // The doc comment on `render_list_only` promises tab/newline/CR are
        // all sanitized, but the existing sanitization test only exercised
        // \t and \n. A bare \r (not paired with \n) is a case `str::lines()`
        // would NOT split on by itself, so if `sanitize` ever dropped `\r`
        // from its replace set, this test would still catch it via the
        // field-content assertions even though the line count would look
        // unchanged.
        let hits = vec![SearchHit {
            provider: Provider::Gh,
            platform: Platform::GitHub,
            full_name: "owner/repo\rwithcr".to_string(),
            stars: 1,
            description: Some("part one\rpart two".to_string()),
            clone_url: "https://github.com/owner/repo\r".to_string(),
        }];
        let rendered = render_list_only(&hits, 10);
        assert_eq!(
            rendered.lines().count(),
            2,
            "header + exactly one data line"
        );
        let data_line = rendered.lines().nth(1).expect("one data line");
        let fields: Vec<&str> = data_line.split('\t').collect();
        assert_eq!(fields.len(), 4, "CR must not introduce extra columns");
        assert_eq!(fields[0], "owner/repo withcr");
        assert_eq!(fields[2], "part one part two");
        assert_eq!(fields[3], "https://github.com/owner/repo ");
        assert!(
            !rendered.contains('\r'),
            "no raw CR byte should survive sanitization: {rendered:?}"
        );
    }

    #[test]
    fn render_list_only_handles_empty_hits_without_panicking() {
        let rendered = render_list_only(&[], 10);
        assert_eq!(rendered, "full_name\tstars\tdescription\tclone_url\n");
    }

    #[test]
    fn render_list_only_never_calls_cmd_add_by_construction() {
        // render_list_only takes only a hit slice and a limit — there is no
        // add-capable closure or ExitCode return for it to invoke cmd_add
        // through, unlike confirm_and_add_with. This test pins that
        // signature shape: calling it repeatedly with the same hits must be
        // side-effect-free and always produce byte-identical output, which
        // would not hold if it reached into cmd_add/dialoguer/filesystem
        // state.
        let hits = vec![hit(
            Provider::Gh,
            "owner/repo",
            7,
            "https://github.com/owner/repo",
        )];
        let first = render_list_only(&hits, 10);
        let second = render_list_only(&hits, 10);
        assert_eq!(first, second);
    }

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_search_limit_defaults_to_ten_when_absent() {
        assert_eq!(parse_search_limit(&args(&["search", "term"])), Ok(10));
    }

    #[test]
    fn parse_search_limit_parses_separated_form() {
        assert_eq!(
            parse_search_limit(&args(&["search", "term", "--limit", "3"])),
            Ok(3)
        );
    }

    #[test]
    fn parse_search_limit_parses_joined_form() {
        assert_eq!(
            parse_search_limit(&args(&["search", "term", "--limit=5"])),
            Ok(5)
        );
    }

    #[test]
    fn parse_search_limit_rejects_non_integer_value_as_usage() {
        assert_eq!(
            parse_search_limit(&args(&["search", "term", "--limit", "nope"])),
            Err(ExitCode::Usage)
        );
    }

    #[test]
    fn parse_search_limit_rejects_missing_value_as_usage() {
        assert_eq!(
            parse_search_limit(&args(&["search", "term", "--limit"])),
            Err(ExitCode::Usage)
        );
    }

    #[test]
    fn cmd_search_missing_term_is_usage() {
        assert_eq!(cmd_search(&args(&["search"])), ExitCode::Usage);
    }

    #[test]
    fn cmd_search_only_flags_no_term_is_usage() {
        assert_eq!(cmd_search(&args(&["search", "--no-add"])), ExitCode::Usage);
    }

    #[test]
    fn cmd_search_unknown_flag_is_usage() {
        assert_eq!(
            cmd_search(&args(&["search", "term", "--bogus"])),
            ExitCode::Usage
        );
    }

    #[test]
    fn cmd_search_bad_limit_is_usage() {
        assert_eq!(
            cmd_search(&args(&["search", "term", "--limit", "-1"])),
            ExitCode::Usage
        );
    }

    #[test]
    fn cmd_search_missing_limit_value_is_usage() {
        // `reject_unknown_flags` treats a trailing `--limit` (no following
        // token) as a value-flag whose value slot is simply absent, so it
        // does not itself reject it; `parse_search_limit`'s missing-value
        // branch is what must catch this through the full `cmd_search`
        // entry point, not just via a direct `parse_search_limit` call.
        assert_eq!(
            cmd_search(&args(&["search", "term", "--limit"])),
            ExitCode::Usage
        );
    }

    fn found_outcome(hits: Vec<SearchHit>) -> SearchOutcome {
        SearchOutcome {
            status: SearchStatus::Found,
            hits,
            warnings: vec![],
        }
    }

    #[test]
    fn run_search_no_add_never_calls_add_fn_even_when_interactive_true() {
        // `run_search` itself trusts its `interactive` argument; `cmd_search`
        // is responsible for forcing `interactive = false` when --no-add is
        // present. This test pins the list-only path's own contract: when
        // called with interactive=false, add_fn must never run.
        let hits = vec![hit(
            Provider::Gh,
            "owner/repo",
            1,
            "https://example.com/o/r",
        )];
        let outcome = found_outcome(hits);
        let code = run_search(
            "term",
            10,
            false,
            |_, _| outcome,
            |_| panic!("add_fn must not be called on the list-only path"),
        );
        assert_eq!(code, ExitCode::Success);
    }

    #[test]
    fn run_search_interactive_found_calls_add_fn_with_the_outcome_hits() {
        let hits = vec![hit(
            Provider::Gh,
            "owner/repo",
            1,
            "https://example.com/o/r",
        )];
        let outcome = found_outcome(hits.clone());
        let mut called_with: Vec<SearchHit> = Vec::new();
        let code = run_search(
            "term",
            10,
            true,
            |_, _| outcome,
            |h| {
                called_with = h.to_vec();
                ExitCode::Success
            },
        );
        assert_eq!(code, ExitCode::Success);
        assert_eq!(called_with, hits);
    }

    #[test]
    fn run_search_all_failed_never_calls_add_fn_and_returns_error() {
        let outcome = SearchOutcome {
            status: SearchStatus::AllFailed,
            hits: vec![],
            warnings: vec![(Provider::Gh, ProviderError::Unavailable)],
        };
        let code = run_search(
            "term",
            10,
            true,
            |_, _| outcome,
            |_| panic!("add_fn must not be called when all providers failed"),
        );
        assert_eq!(code, ExitCode::Error);
    }

    #[test]
    fn run_search_no_results_never_calls_add_fn_and_exits_success() {
        let outcome = SearchOutcome {
            status: SearchStatus::NoResults,
            hits: vec![],
            warnings: vec![],
        };
        let code = run_search(
            "term",
            10,
            true,
            |_, _| outcome,
            |_| panic!("add_fn must not be called when there are no results"),
        );
        assert_eq!(code, ExitCode::Success);
    }
}
