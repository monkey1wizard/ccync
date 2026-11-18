//! Bounded JSONC support: comments and trailing commas only.
use serde_json::Value;

pub fn parse(text: &str) -> Result<Value, serde_json::Error> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let (mut i, mut quoted, mut escaped) = (0, false, false);
    while i < chars.len() {
        let c = chars[i];
        if quoted {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                quoted = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            quoted = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            i += 2;
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            out.push('\n');
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                if chars[i] == '\n' {
                    out.push('\n');
                }
                i += 1;
            }
            if i + 1 >= chars.len() {
                return serde_json::from_str("/* unterminated");
            }
            i += 2;
            continue;
        }
        if c == ',' {
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j].is_whitespace() {
                    j += 1;
                    continue;
                }
                if chars[j] == '/' && chars.get(j + 1) == Some(&'/') {
                    j += 2;
                    while j < chars.len() && chars[j] != '\n' {
                        j += 1;
                    }
                    continue;
                }
                if chars[j] == '/' && chars.get(j + 1) == Some(&'*') {
                    j += 2;
                    while j + 1 < chars.len() && !(chars[j] == '*' && chars[j + 1] == '/') {
                        j += 1;
                    }
                    if j + 1 < chars.len() {
                        j += 2;
                        continue;
                    }
                }
                break;
            }
            if matches!(chars.get(j), Some(']' | '}')) {
                i += 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    serde_json::from_str(&out)
}

#[cfg(test)]
mod tests {
    use super::parse;
    #[test]
    fn permits_only_comments_and_trailing_commas() {
        assert!(parse("{\"x\": [1,],}").is_ok());
        assert!(parse("{\"x\": 1, /* comment */}").is_ok());
        assert!(parse("{\"x\": 1 /* comment */ 2}").is_err());
        assert!(parse("{\"x\": 1, // comment\n \"y\": 2}").is_ok());
    }
}
