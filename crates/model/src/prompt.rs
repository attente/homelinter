//! Prompt template rendering for description providers.
//!
//! A description provider entry may carry a custom `prompt` template so the
//! request wording is configurable beyond the built-in word budget. The
//! template supports `{words}`-style placeholders (validated at config time).
//! Fixed inputs the model must always see - the full source path and any
//! extracted document text for descriptions, the reverse-geocoded address
//! for location summaries - are appended verbatim after the rendered
//! template by the caller, so they need no placeholders and a custom
//! template cannot omit them. Literal braces are
//!escaped as `{{` and `}}``;, so a template can quote JSON or format
//! strings without triggering placeholder validation.

/// Return the first placeholder in `template` that is not in `allowed`, or
/// `None` when every placeholder (and escape) is valid. Escaped
/// literal braces (`{{`, `}}`) are skipped; an unescaped placeholder is
/// `{name}` where `name` runs to the next `}` or end of input. An empty
/// placeholder (`{}`) and unmatched `}` are treated as literal text.
pub fn prompt_template_error(template: &str, allowed: &[&str]) -> Option<String> {
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            if chars.peek() == Some(&'{') {
                chars.next(); // escaped literal brace
            } else {
                let mut token = String::new();
                for cc in chars.by_ref() {
                    if cc == '}' {
                        break;
                    }
                    token.push(cc);
                }
                if !token.is_empty() && !allowed.contains(&token.as_str()) {
                    return Some(format!("{{{token}}}"));
                }
            }
        } else if c == '}' {
            if chars.peek() == Some(&'}') {
                chars.next(); // escaped literal brace
            }
        }
    }
    None
}

/// Substitute placeholders in `template` with `values`, mapping `{name}`-style
/// tokens by exact name. `{{` and `}}` decode to literal braces. Unknown
/// tokens and unmatched braces pass through verbatim (config-time validation
/// rejects them, so this is only a defense in depth for callers that skip it).
pub fn render_prompt_template(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '{' {
            if chars.peek() == Some(&'{') {
                chars.next();
                out.push('{');
            } else {
                let mut token = String::new();
                for cc in chars.by_ref() {
                    if cc == '}' {
                        break;
                    }
                    token.push(cc);
                }
                match values.iter().find(|(k, _)| *k == token) {
                    Some((_, v)) => out.push_str(v),
                    None => {
                        out.push('{');
                        out.push_str(&token);
                        out.push('}');
                    }
                }
            }
        } else if c == '}' {
            if chars.peek() == Some(&'}') {
                chars.next();
                out.push('}');
            } else {
                out.push('}');
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_placeholders_and_escapes() {
        let out = render_prompt_template(
            "Move {name} into {path} in at most {words} words",
            &[("name", "a.jpg"), ("path", "/x/y"), ("words", "8")],
        );
        assert_eq!(out, "Move a.jpg into /x/y in at most 8 words");

        // Literal braces escape to single characters. Folders {{%s}} stays
        // a format string fragment untouched.

        let out = render_prompt_template("Folders {{%s}} then {name}", &[("name", "b")]);
        assert_eq!(out, "Folders {%s} then b");
    }

    #[test]
    fn validation_rejects_unknown_and_accepts_known() {
        assert!(prompt_template_error(
            "Describe {name} with {words} at {path}",
            &["name", "path", "words"]
        )
        .is_none());
        assert_eq!(
            prompt_template_error("Describe {nme}", &["name"]),
            Some("{nme}".to_string())
        );
        // Escapes and degenerate braces are literal, not placeholders.

        assert!(prompt_template_error("JSON {{literal}} then {} then }", &["name"]).is_none());
    }
}
