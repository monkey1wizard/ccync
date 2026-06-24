//! Codex subagent TOML serializer for plugin agent files.
//!
//! Converts a `*.agent.md` frontmatter+body pair into the TOML format Codex reads
//! from `~/.codex/agents/<name>.toml`. `model` / `model_reasoning_effort` come from
//! the agent's own frontmatter (generic — no golem-role routing); `sandbox_mode` is
//! supplied by the caller.

/// Parsed agent metadata extracted from a `*.agent.md` file.
#[derive(Debug, PartialEq, Clone)]
pub struct AgentMeta {
    pub name: String,
    pub description: String,
    /// Optional Codex model from frontmatter (`model:`); the TOML `model` line is
    /// omitted when this is absent/empty.
    pub model: Option<String>,
    /// Optional Codex reasoning effort from frontmatter (`model_reasoning_effort:`).
    pub reasoning_effort: Option<String>,
    /// The charter body (everything after the closing `---` of the frontmatter).
    pub body: String,
}

/// Serialize an `AgentMeta` into a Codex TOML string.
///
/// The output format expected by Codex:
/// ```toml
/// [agent]
/// name = "golem-architect"
/// description = "..."
/// developer_instructions = """
/// ...body...
/// """
/// model = "gpt-5.4"
/// model_reasoning_effort = "medium"
/// sandbox_mode = "read-only"
/// ```
pub fn serialize_codex_toml(
    meta: &AgentMeta,
    model: &str,
    reasoning_effort: Option<&str>,
    sandbox_mode: &str,
) -> String {
    let body = escape_toml_multiline(&meta.body);
    let description = escape_toml_string(&meta.description);
    let name = escape_toml_string(&meta.name);
    let sandbox_escaped = escape_toml_string(sandbox_mode);

    let mut lines = vec![
        "[agent]".to_owned(),
        format!("name = \"{name}\""),
        format!("description = \"{description}\""),
        format!("developer_instructions = \"\"\"\n{body}\n\"\"\""),
    ];

    // Generic: omit the model line entirely when no model is supplied (a plugin
    // agent without a frontmatter `model:` does not pin one).
    if !model.trim().is_empty() {
        lines.push(format!("model = \"{}\"", escape_toml_string(model)));
    }

    if let Some(effort) = reasoning_effort {
        lines.push(format!(
            "model_reasoning_effort = \"{}\"",
            escape_toml_string(effort)
        ));
    }
    lines.push(format!("sandbox_mode = \"{sandbox_escaped}\""));

    lines.join("\n") + "\n"
}

/// Parse the minimal metadata from a `*.agent.md` file.
///
/// Returns `None` when the file has no recognisable YAML frontmatter or is
/// missing the required `name` field.
pub fn parse_agent_meta(content: &str) -> Option<AgentMeta> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() < 3 || lines[0] != "---" {
        return None;
    }
    // Find closing ---
    let closing = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, l)| **l == "---")
        .map(|(i, _)| i)?;

    let mut name = String::new();
    let mut description = String::new();
    let mut model: Option<String> = None;
    let mut reasoning_effort: Option<String> = None;

    for line in &lines[1..closing] {
        if let Some(rest) = line.strip_prefix("name:") {
            name = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("description:") {
            description = rest.trim().to_owned();
        } else if let Some(rest) = line.strip_prefix("model_reasoning_effort:") {
            let v = rest.trim();
            if !v.is_empty() {
                reasoning_effort = Some(v.to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("model:") {
            let v = rest.trim();
            if !v.is_empty() {
                model = Some(v.to_owned());
            }
        }
    }

    if name.is_empty() {
        return None;
    }

    // Everything after the closing --- is the charter body.
    let body = lines[closing + 1..].join("\n");

    Some(AgentMeta {
        name,
        description,
        model,
        reasoning_effort,
        body: body.trim_start_matches('\n').to_owned(),
    })
}

/// Escape a single-line TOML basic string value (content between `"…"`).
fn escape_toml_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Escape content for use inside a TOML multiline basic string (`"""…"""`).
///
/// The only sequence that needs handling is `"""` — we escape the final quote
/// so that three consecutive double-quotes cannot close the string prematurely.
fn escape_toml_multiline(s: &str) -> String {
    s.replace("\"\"\"", "\"\"\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example_meta() -> AgentMeta {
        AgentMeta {
            name: "golem-architect".to_owned(),
            description: "Strict technical architect.".to_owned(),
            model: None,
            reasoning_effort: None,
            body: "You review plans.\n\nBe adversarial.".to_owned(),
        }
    }

    #[test]
    fn serialize_produces_valid_toml_section() {
        let out = serialize_codex_toml(&example_meta(), "gpt-5.4", Some("medium"), "read-only");
        assert!(
            out.starts_with("[agent]\n"),
            "must open with [agent] section"
        );
        assert!(out.contains("name = \"golem-architect\""));
        assert!(out.contains("description = \"Strict technical architect.\""));
        assert!(out.contains("model = \"gpt-5.4\""));
        assert!(out.contains("model_reasoning_effort = \"medium\""));
        assert!(out.contains("sandbox_mode = \"read-only\""));
        assert!(out.contains("developer_instructions = \"\"\"\n"));
    }

    #[test]
    fn serialize_no_reasoning_effort_omits_field() {
        let out = serialize_codex_toml(&example_meta(), "gpt-5.4", None, "read-only");
        assert!(
            !out.contains("model_reasoning_effort"),
            "must omit when None: {out}"
        );
    }

    #[test]
    fn serialize_body_in_multiline_string() {
        let out = serialize_codex_toml(&example_meta(), "m", None, "s");
        assert!(
            out.contains("You review plans."),
            "charter body must appear in output"
        );
    }

    #[test]
    fn serialize_omits_model_line_when_empty() {
        let out = serialize_codex_toml(&example_meta(), "", None, "read-only");
        assert!(
            !out.contains("model = "),
            "empty model must omit the model line: {out}"
        );
        assert!(out.contains("sandbox_mode = \"read-only\""));
    }

    #[test]
    fn parse_reads_model_and_effort_from_frontmatter() {
        let content = "---\nname: a\nmodel: gpt-5.4\nmodel_reasoning_effort: high\n---\nbody\n";
        let meta = parse_agent_meta(content).unwrap();
        assert_eq!(meta.model.as_deref(), Some("gpt-5.4"));
        assert_eq!(meta.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn parse_absent_model_is_none() {
        let content = "---\nname: a\n---\nbody\n";
        let meta = parse_agent_meta(content).unwrap();
        assert!(meta.model.is_none());
        assert!(meta.reasoning_effort.is_none());
    }

    #[test]
    fn serialize_escapes_triple_quote_in_body() {
        let meta = AgentMeta {
            name: "g".to_owned(),
            description: "d".to_owned(),
            model: None,
            reasoning_effort: None,
            body: "before \"\"\" after".to_owned(),
        };
        let out = serialize_codex_toml(&meta, "m", None, "s");
        // The raw unescaped "before """ after" must not appear in the body section.
        assert!(
            !out.contains("before \"\"\" after"),
            "unescaped triple-quote in body must be escaped: {out}"
        );
        // The escaped form ""\" must appear where the body's """ was.
        assert!(
            out.contains("\"\"\\\""),
            "escaped form must be present: {out}"
        );
    }

    #[test]
    fn parse_extracts_name_and_description() {
        let content = "---\nname: golem-architect\ndescription: The architect.\ntools: [read]\n---\nbody content\n";
        let meta = parse_agent_meta(content).expect("should parse");
        assert_eq!(meta.name, "golem-architect");
        assert_eq!(meta.description, "The architect.");
        assert_eq!(meta.body, "body content");
    }

    #[test]
    fn parse_returns_none_without_frontmatter() {
        assert!(parse_agent_meta("no frontmatter here").is_none());
    }

    #[test]
    fn parse_returns_none_without_name() {
        let content = "---\ndescription: d\n---\nbody\n";
        assert!(parse_agent_meta(content).is_none());
    }

    #[test]
    fn parse_body_excludes_frontmatter() {
        let content = "---\nname: g\n---\ncharter starts here\n";
        let meta = parse_agent_meta(content).unwrap();
        assert_eq!(meta.body, "charter starts here");
        assert!(
            !meta.body.contains("---"),
            "frontmatter must not leak into body"
        );
    }
}
