//! Minimal strict templating for prompt and text assets.
//!
//! Supported syntax:
//! - `{{ name }}` placeholder interpolation
//! - `{{{{` for a literal `{{`
//! - `}}}}` for a literal `}}`

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateParseError {
    EmptyPlaceholder { start: usize },
    NestedPlaceholder { start: usize },
    UnmatchedClosingDelimiter { start: usize },
    UnterminatedPlaceholder { start: usize },
}

impl fmt::Display for TemplateParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPlaceholder { start } => {
                write!(f, "template placeholder at byte {start} is empty")
            }
            Self::NestedPlaceholder { start } => {
                write!(
                    f,
                    "template placeholder starting at byte {start} contains a nested `{{`"
                )
            }
            Self::UnmatchedClosingDelimiter { start } => {
                write!(f, "template contains an unmatched `}}` at byte {start}")
            }
            Self::UnterminatedPlaceholder { start } => {
                write!(
                    f,
                    "template placeholder starting at byte {start} is missing `}}`"
                )
            }
        }
    }
}

impl Error for TemplateParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateRenderError {
    DuplicateValue { name: String },
    ExtraValue { name: String },
    MissingValue { name: String },
}

impl fmt::Display for TemplateRenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateValue { name } => {
                write!(f, "template value `{name}` was provided more than once")
            }
            Self::ExtraValue { name } => {
                write!(f, "template value `{name}` is not used by this template")
            }
            Self::MissingValue { name } => {
                write!(f, "template placeholder `{name}` is missing a value")
            }
        }
    }
}

impl Error for TemplateRenderError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Placeholder(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    placeholders: BTreeSet<String>,
    segments: Vec<Segment>,
}

impl Template {
    pub fn parse(source: &str) -> Result<Self, TemplateParseError> {
        let mut placeholders = BTreeSet::new();
        let mut segments = Vec::new();
        let mut literal_start = 0usize;
        let mut cursor = 0usize;

        while cursor < source.len() {
            let rest = &source[cursor..];
            if rest.starts_with("{{{{") {
                push_literal(&mut segments, &source[literal_start..cursor]);
                push_literal(&mut segments, "{{");
                cursor += "{{{{".len();
                literal_start = cursor;
                continue;
            }
            if rest.starts_with("}}}}") {
                push_literal(&mut segments, &source[literal_start..cursor]);
                push_literal(&mut segments, "}}");
                cursor += "}}}}".len();
                literal_start = cursor;
                continue;
            }
            if rest.starts_with("{{") {
                push_literal(&mut segments, &source[literal_start..cursor]);
                let (placeholder, next_cursor) = parse_placeholder(source, cursor)?;
                placeholders.insert(placeholder.clone());
                segments.push(Segment::Placeholder(placeholder));
                cursor = next_cursor;
                literal_start = cursor;
                continue;
            }
            if rest.starts_with("}}") {
                return Err(TemplateParseError::UnmatchedClosingDelimiter { start: cursor });
            }
            cursor += 1;
        }

        push_literal(&mut segments, &source[literal_start..]);

        Ok(Self {
            placeholders,
            segments,
        })
    }

    pub fn render<'a>(
        &self,
        values: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<String, TemplateRenderError> {
        let values = values_map(values)?;

        for name in values.keys() {
            if !self.placeholders.contains(name) {
                return Err(TemplateRenderError::ExtraValue { name: name.clone() });
            }
        }

        let mut output = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => output.push_str(text),
                Segment::Placeholder(name) => {
                    let value = values
                        .get(name)
                        .ok_or_else(|| TemplateRenderError::MissingValue { name: name.clone() })?;
                    output.push_str(value);
                }
            }
        }

        Ok(output)
    }
}

fn push_literal(segments: &mut Vec<Segment>, literal: &str) {
    if literal.is_empty() {
        return;
    }

    if let Some(Segment::Literal(existing)) = segments.last_mut() {
        existing.push_str(literal);
    } else {
        segments.push(Segment::Literal(literal.to_string()));
    }
}

fn parse_placeholder(source: &str, start: usize) -> Result<(String, usize), TemplateParseError> {
    let placeholder_start = start + "{{".len();
    let Some(relative_end) = source[placeholder_start..].find("}}") else {
        return Err(TemplateParseError::UnterminatedPlaceholder { start });
    };
    let end = placeholder_start + relative_end;
    let raw = &source[placeholder_start..end];
    if raw.contains("{{") {
        return Err(TemplateParseError::NestedPlaceholder { start });
    }
    let placeholder = raw.trim();
    if placeholder.is_empty() {
        return Err(TemplateParseError::EmptyPlaceholder { start });
    }
    Ok((placeholder.to_string(), end + "}}".len()))
}

fn values_map<'a>(
    values: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<BTreeMap<String, String>, TemplateRenderError> {
    let mut map = BTreeMap::new();
    for (name, value) in values {
        let name = name.to_string();
        if map.insert(name.clone(), value.to_string()).is_some() {
            return Err(TemplateRenderError::DuplicateValue { name });
        }
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::Template;
    use super::TemplateParseError;
    use super::TemplateRenderError;

    #[test]
    fn parsed_templates_can_be_reused() {
        let template = Template::parse("{{greeting}}, {{ name }}!").unwrap();

        assert_eq!(
            template.render([("greeting", "Hello"), ("name", "Codex")]),
            Ok("Hello, Codex!".to_string())
        );
        assert_eq!(
            template.render([("greeting", "Hi"), ("name", "builder")]),
            Ok("Hi, builder!".to_string())
        );
    }

    #[test]
    fn parse_errors_when_placeholder_is_empty() {
        let err = Template::parse("Hello, {{   }}.").unwrap_err();

        assert_eq!(err, TemplateParseError::EmptyPlaceholder { start: 7 });
    }

    #[test]
    fn render_errors_when_extra_value_is_provided() {
        let template = Template::parse("Hello, {{ name }}.").unwrap();

        assert_eq!(
            template.render([("name", "Codex"), ("unused", "extra")]),
            Err(TemplateRenderError::ExtraValue {
                name: "unused".to_string(),
            })
        );
    }
}
