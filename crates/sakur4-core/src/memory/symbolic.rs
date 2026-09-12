//! The Symbolic Ledger: deterministic, parser-derived facts (FR-2).
//!
//! # The hard rule
//!
//! No LLM may write to this table, under any configuration. The type system does
//! most of the work: [`SymbolicFact`] has exactly one constructor —
//! [`SymbolicFact::from_deterministic_source`] — and [`FactSource`] is a closed
//! enum of deterministic extractors. There is no `From<&str>` and no general
//! `new`, so producing a fact requires naming the parser that produced it.
//!
//! What a *review* still has to confirm is the module graph: `symbolic.rs` must
//! never gain an import of [`crate::embed`], [`crate::llama`] or
//! [`crate::consolidate`]. `tests/symbolic_purity.rs` asserts that mechanically
//! by scanning this file's own source, which turns FR-2's "a code review or
//! static check in CI fails the build" acceptance criterion into an actual test.
//!
//! # Structured tool output counts as symbolic too
//!
//! The PRD's risk register notes that a code-only symbolic track under-serves
//! research and general tool use. The answer is [`ToolOutputParser`]: JSON, CSV,
//! HTTP headers, git diffs and exit codes all have structure, and structure can
//! be extracted deterministically. Free-form prose has no symbolic anchor — and
//! the PRD is explicit that Sakur4 must *say so* rather than pretend otherwise,
//! which is why [`ToolOutputParser::parse_any`] reports that it found no structure
//! instead of guessing.

use serde_json::Value;

use crate::error::{Error, Result};
use crate::ids::{normalize_rel_path, short_hash_str};
use crate::tokens::TokenCounter;

/// What kind of thing a fact describes.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Interface,
    Type,
    Constant,
    Module,
    Import,
    Export,
    // --- structured tool output ---
    ToolOutputField,
    ToolExitCode,
    HttpHeader,
    JsonField,
    CsvColumn,
    DiffHunk,
    RegexMatch,
}

impl FactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            FactKind::Function => "function",
            FactKind::Method => "method",
            FactKind::Class => "class",
            FactKind::Struct => "struct",
            FactKind::Enum => "enum",
            FactKind::Trait => "trait",
            FactKind::Interface => "interface",
            FactKind::Type => "type",
            FactKind::Constant => "constant",
            FactKind::Module => "module",
            FactKind::Import => "import",
            FactKind::Export => "export",
            FactKind::ToolOutputField => "tool_output_field",
            FactKind::ToolExitCode => "tool_exit_code",
            FactKind::HttpHeader => "http_header",
            FactKind::JsonField => "json_field",
            FactKind::CsvColumn => "csv_column",
            FactKind::DiffHunk => "diff_hunk",
            FactKind::RegexMatch => "regex_match",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "function" => FactKind::Function,
            "method" => FactKind::Method,
            "class" => FactKind::Class,
            "struct" => FactKind::Struct,
            "enum" => FactKind::Enum,
            "trait" => FactKind::Trait,
            "interface" => FactKind::Interface,
            "type" => FactKind::Type,
            "constant" => FactKind::Constant,
            "module" => FactKind::Module,
            "import" => FactKind::Import,
            "export" => FactKind::Export,
            "tool_output_field" => FactKind::ToolOutputField,
            "tool_exit_code" => FactKind::ToolExitCode,
            "http_header" => FactKind::HttpHeader,
            "json_field" => FactKind::JsonField,
            "csv_column" => FactKind::CsvColumn,
            "diff_hunk" => FactKind::DiffHunk,
            "regex_match" => FactKind::RegexMatch,
            other => return Err(Error::Invalid(format!("unknown fact kind: {other}"))),
        })
    }

    /// True for facts that describe code structure.
    pub fn is_code(self) -> bool {
        !matches!(
            self,
            FactKind::ToolOutputField
                | FactKind::ToolExitCode
                | FactKind::HttpHeader
                | FactKind::JsonField
                | FactKind::CsvColumn
                | FactKind::DiffHunk
                | FactKind::RegexMatch
        )
    }
}

/// The closed set of deterministic extractors permitted to write the Ledger.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FactSource {
    /// tree-sitter parse of a source file.
    TreeSitter,
    /// A structured-output parser in [`ToolOutputParser`].
    ToolOutputParser,
    /// A top-level command name parsed from a POSIX command string.
    CommandName,
}

impl FactSource {
    pub fn as_str(self) -> &'static str {
        match self {
            FactSource::TreeSitter => "tree_sitter",
            FactSource::ToolOutputParser => "tool_output_parser",
            FactSource::CommandName => "command_name",
        }
    }
}

/// A row of the Symbolic Ledger.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SymbolicFact {
    pub fact_id: String,
    pub kind: FactKind,
    pub qualified_name: String,
    pub file_path: Option<String>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    pub signature: Option<String>,
    pub ast_hash: String,
    pub source: FactSource,
    pub project_id: Option<String>,
    pub parent_name: Option<String>,
    pub body: Option<String>,
    pub updated_at: String,
}

/// A fact as produced by an extractor, before it is assigned an id.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SymbolicWrite {
    pub kind: FactKind,
    pub qualified_name: String,
    pub file_path: Option<String>,
    pub line_start: Option<i64>,
    pub line_end: Option<i64>,
    pub signature: Option<String>,
    /// The exact source text the fact was derived from. Hashed to produce
    /// `ast_hash`, which is what makes staleness detection O(1) on re-parse.
    pub body: Option<String>,
    pub parent_name: Option<String>,
}

impl SymbolicWrite {
    pub fn new(kind: FactKind, qualified_name: impl Into<String>) -> Self {
        Self {
            kind,
            qualified_name: qualified_name.into(),
            file_path: None,
            line_start: None,
            line_end: None,
            signature: None,
            body: None,
            parent_name: None,
        }
    }

    pub fn at(mut self, file: impl AsRef<std::path::Path>) -> Self {
        self.file_path = Some(normalize_rel_path(file.as_ref()));
        self
    }

    pub fn at_path(mut self, rel_path: impl Into<String>) -> Self {
        self.file_path = Some(rel_path.into());
        self
    }

    pub fn lines(mut self, start: i64, end: i64) -> Self {
        self.line_start = Some(start);
        self.line_end = Some(end);
        self
    }

    pub fn signature(mut self, sig: impl Into<String>) -> Self {
        self.signature = Some(sig.into());
        self
    }

    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    pub fn in_parent(mut self, parent: impl Into<String>) -> Self {
        self.parent_name = Some(parent.into());
        self
    }

    /// Hash the fact's identity plus its body.
    ///
    /// Hashing the qualified name alongside the body means renaming a symbol
    /// invalidates summaries anchored to it even when the body is byte-identical
    /// — which is the correct behaviour for a "signature changed" staleness
    /// check (FR-11, FR-12).
    pub fn ast_hash(&self) -> String {
        let mut material = String::with_capacity(64);
        material.push_str(self.kind.as_str());
        material.push('\u{1}');
        material.push_str(&self.qualified_name);
        material.push('\u{1}');
        if let Some(sig) = &self.signature {
            material.push_str(sig);
        }
        material.push('\u{1}');
        if let Some(body) = &self.body {
            material.push_str(body);
        }
        short_hash_str(&material)
    }

    /// Materialise into a ledger row from a named deterministic source.
    pub fn into_fact(self, source: FactSource, project_id: Option<String>) -> SymbolicFact {
        let ast_hash = self.ast_hash();
        SymbolicFact {
            fact_id: crate::ids::new_id("sym"),
            kind: self.kind,
            qualified_name: self.qualified_name,
            file_path: self.file_path,
            line_start: self.line_start,
            line_end: self.line_end,
            signature: self.signature,
            ast_hash,
            source,
            project_id,
            parent_name: self.parent_name,
            body: self.body,
            updated_at: crate::ids::now_rfc3339(),
        }
    }
}

impl SymbolicFact {
    /// The only constructor of a ledger row.
    ///
    /// Requiring a [`FactSource`] is the type-level half of FR-2. The runtime
    /// half is that no code path can obtain a `FactSource` from a model: it is a
    /// closed enum with no `Deserialize` from arbitrary text in the write path.
    pub fn from_deterministic_source(
        write: SymbolicWrite,
        source: FactSource,
        project_id: Option<String>,
    ) -> Self {
        write.into_fact(source, project_id)
    }

    /// True when this fact's content differs from `other`'s despite describing
    /// the same symbol — the definition of staleness for a dependent summary.
    pub fn differs_from(&self, other: &SymbolicFact) -> bool {
        self.ast_hash != other.ast_hash
    }

    /// Render for injection into a prompt. Short by construction: signatures, not
    /// bodies, are what the Ledger gives the model.
    pub fn render(&self) -> String {
        let loc = match (&self.file_path, self.line_start) {
            (Some(f), Some(l)) => format!(" ({f}:{l})"),
            (Some(f), None) => format!(" ({f})"),
            _ => String::new(),
        };
        match &self.signature {
            Some(sig) => format!("{} {}{loc}", self.kind.as_str(), sig),
            None => format!("{} {}{loc}", self.kind.as_str(), self.qualified_name),
        }
    }
}

/// Facts extracted from one structured tool output.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ToolOutputFacts {
    pub facts: Vec<SymbolicWrite>,
    /// A one-line summary of what was parsed, surfaced in the receipt so the
    /// operator can see whether a tool result became symbolic facts or not.
    pub summary: String,
    /// Which parser handled it, or `None` when the output had no structure.
    pub parser: Option<&'static str>,
}

impl ToolOutputFacts {
    pub fn is_empty(&self) -> bool {
        self.facts.is_empty()
    }

    pub fn len(&self) -> usize {
        self.facts.len()
    }

    /// Attach token counts so the Ledger knows what each fact would cost.
    pub fn total_body_tokens(&self, counter: &TokenCounter) -> usize {
        self.facts.iter().filter_map(|f| f.body.as_deref()).map(|b| counter.count(b).get()).sum()
    }
}

/// The structured-output parsers.
///
/// Each returns `None` when the input does not match its shape, and
/// [`ToolOutputParser::parse_any`] tries them in order of specificity. Returning
/// `None` from all of them is a legitimate, reported outcome: it means this tool
/// result contributed raw episodic text and no symbolic facts, and the PRD
/// requires that distinction to be visible rather than papered over.
pub struct ToolOutputParser;

impl ToolOutputParser {
    /// Parse with whichever parser recognises the input.
    pub fn parse_any(tool_name: Option<&str>, content: &str) -> ToolOutputFacts {
        eprintln!(
            "DBG parse_any len={} first20={:?}",
            content.len(),
            content.chars().take(20).collect::<String>()
        );
        // Tool-name hints first: a `git diff` result is unambiguously a diff even
        // if it happens to look like a header block.
        if let Some(name) = tool_name {
            let n = name.to_ascii_lowercase();
            if (n.contains("diff") || n.contains("patch"))
                && let Some(f) = Self::parse_diff(content)
            {
                return f;
            }
        }
        if let Some(f) = Self::parse_json(content) {
            return f;
        }
        if let Some(f) = Self::parse_http_headers(content) {
            return f;
        }
        if let Some(f) = Self::parse_csv(content) {
            return f;
        }
        if let Some(f) = Self::parse_diff(content) {
            return f;
        }
        if let Some(f) = Self::parse_exit_code(content) {
            return f;
        }
        ToolOutputFacts {
            facts: Vec::new(),
            summary: "no structure detected; retained as raw episodic text only".into(),
            parser: None,
        }
    }

    /// `{"a": {"b": 1}}` → one `json_field` fact per leaf path.
    pub fn parse_json(content: &str) -> Option<ToolOutputFacts> {
        let trimmed = content.trim();
        if !(trimmed.starts_with('{') || trimmed.starts_with('[')) {
            return None;
        }
        let value: Value = serde_json::from_str(trimmed).ok()?;
        let mut facts = Vec::new();
        flatten_json(&value, String::new(), &mut facts, 0);
        if facts.is_empty() {
            return None;
        }
        Some(ToolOutputFacts {
            summary: format!("{} JSON field paths", facts.len()),
            facts,
            parser: Some("json"),
        })
    }

    /// `Key: value` header blocks, e.g. `curl -i` output.
    pub fn parse_http_headers(content: &str) -> Option<ToolOutputFacts> {
        let mut facts = Vec::new();
        let mut status: Option<String> = None;
        for line in content.lines().take(64) {
            let line = line.trim_end();
            if line.is_empty() {
                if !facts.is_empty() {
                    break;
                }
                continue;
            }
            if status.is_none() && line.starts_with("HTTP/") {
                status = Some(line.to_string());
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                // A non-header line before any header means this is not a header
                // block at all.
                if facts.is_empty() {
                    return None;
                }
                break;
            };
            if name.is_empty()
                || name.len() > 64
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                if facts.is_empty() {
                    return None;
                }
                break;
            }
            let mut f = SymbolicWrite::new(FactKind::HttpHeader, name.to_ascii_lowercase())
                .signature(value.trim().to_string());
            if let Some(s) = &status {
                f = f.body(format!("{s}\n{name}: {}", value.trim()));
            } else {
                f = f.body(format!("{name}: {}", value.trim()));
            }
            facts.push(f);
        }
        if facts.is_empty() {
            return None;
        }
        let mut summary = format!("{} HTTP headers", facts.len());
        if let Some(s) = status {
            summary.push_str(&format!(" from {s}"));
        }
        Some(ToolOutputFacts { summary, facts, parser: Some("http_headers") })
    }

    /// Comma- or tab-separated tabular data with a header row.
    pub fn parse_csv(content: &str) -> Option<ToolOutputFacts> {
        let mut lines = content.lines().filter(|l| !l.trim().is_empty());
        let header = lines.next()?;
        let delimiter =
            if header.matches('\t').count() > header.matches(',').count() { '\t' } else { ',' };
        if header.matches(delimiter).count() < 1 {
            return None;
        }
        let columns: Vec<&str> = header.split(delimiter).map(|c| c.trim()).collect();
        if columns.iter().any(|c| c.is_empty() || c.contains(' ')) {
            return None;
        }
        let rows: Vec<&str> = lines.take(8).collect();
        if rows.is_empty() {
            return None;
        }
        let facts = columns
            .iter()
            .map(|c| {
                SymbolicWrite::new(FactKind::CsvColumn, *c)
                    .body(format!("{header}\n{}", rows.join("\n")))
            })
            .collect::<Vec<_>>();
        Some(ToolOutputFacts {
            summary: format!("{} CSV columns, {} sample rows", facts.len(), rows.len()),
            facts,
            parser: Some("csv"),
        })
    }

    /// Unified-diff hunks: file path plus the set of changed line ranges.
    pub fn parse_diff(content: &str) -> Option<ToolOutputFacts> {
        if !content.contains("@@") && !content.contains("+++ ") {
            return None;
        }
        let mut facts = Vec::new();
        let mut current_file: Option<String> = None;
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("+++ ") {
                let path = rest.trim_start_matches("b/").trim();
                if path != "/dev/null" {
                    current_file = Some(path.to_string());
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("@@") {
                let header = rest.trim_end_matches("@@").trim();
                let file = current_file.clone().unwrap_or_else(|| "<unknown>".into());
                facts.push(
                    SymbolicWrite::new(FactKind::DiffHunk, format!("{file}#{header}"))
                        .at_path(file.clone())
                        .signature(header.to_string())
                        .body(line.to_string()),
                );
            }
        }
        if facts.is_empty() {
            return None;
        }
        Some(ToolOutputFacts {
            summary: format!("{} diff hunks", facts.len()),
            facts,
            parser: Some("diff"),
        })
    }

    /// A bare exit code, e.g. `exit code: 1`.
    pub fn parse_exit_code(content: &str) -> Option<ToolOutputFacts> {
        let lower = content.trim().to_ascii_lowercase();
        if lower.len() > 64 {
            return None;
        }
        let digits: String = lower
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if digits.is_empty() || !(lower.contains("exit") || lower.contains("status")) {
            return None;
        }
        let code = digits.parse::<i64>().ok()?;
        Some(ToolOutputFacts {
            summary: format!("process exited with status {code}"),
            facts: vec![
                SymbolicWrite::new(FactKind::ToolExitCode, "exit_code")
                    .signature(code.to_string())
                    .body(content.trim().to_string()),
            ],
            parser: Some("exit_code"),
        })
    }
}

fn flatten_json(value: &Value, prefix: String, out: &mut Vec<SymbolicWrite>, depth: usize) {
    if depth > 6 || out.len() > 200 {
        return;
    }
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                match v {
                    Value::Object(_) | Value::Array(_) => flatten_json(v, path, out, depth + 1),
                    _ => out.push(json_leaf(&path, v)),
                }
            }
        }
        Value::Array(items) => {
            // Sample the first element to describe the array's shape without
            // exploding the fact count on large result sets.
            if let Some(first) = items.first() {
                match first {
                    Value::Object(_) | Value::Array(_) => {
                        flatten_json(first, format!("{prefix}[]"), out, depth + 1)
                    }
                    _ => out.push(json_leaf(&format!("{prefix}[]"), first)),
                }
            }
        }
        other => out.push(json_leaf(&prefix, other)),
    }
}

fn json_leaf(path: &str, value: &Value) -> SymbolicWrite {
    let type_name = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    SymbolicWrite::new(FactKind::JsonField, path)
        .signature(type_name.to_string())
        .body(format!("{path}: {type_name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ast_hash_changes_when_the_signature_changes() {
        let a = SymbolicWrite::new(FactKind::Function, "m::f")
            .at_path("src/lib.rs")
            .signature("fn f(x: i32)")
            .body("fn f(x: i32) -> i32 { x }");
        let b = SymbolicWrite::new(FactKind::Function, "m::f")
            .at_path("src/lib.rs")
            .signature("fn f(x: i64)")
            .body("fn f(x: i64) -> i64 { x }");
        assert_ne!(a.ast_hash(), b.ast_hash());
    }

    #[test]
    fn ast_hash_is_stable_for_identical_input() {
        let make =
            || SymbolicWrite::new(FactKind::Function, "m::f").signature("fn f()").body("fn f() {}");
        assert_eq!(make().ast_hash(), make().ast_hash());
    }

    #[test]
    fn rename_invalidates_even_with_identical_body() {
        let body = "fn f() {}";
        let a = SymbolicWrite::new(FactKind::Function, "m::f").body(body);
        let b = SymbolicWrite::new(FactKind::Function, "m::g").body(body);
        assert_ne!(a.ast_hash(), b.ast_hash());
    }

    #[test]
    fn json_parser_flattens_leaf_paths() {
        let out =
            ToolOutputParser::parse_json(r#"{"user":{"id":1,"name":"a"},"ok":true}"#).unwrap();
        let names: Vec<&str> = out.facts.iter().map(|f| f.qualified_name.as_str()).collect();
        assert!(names.contains(&"user.id"));
        assert!(names.contains(&"user.name"));
        assert!(names.contains(&"ok"));
        assert_eq!(out.parser, Some("json"));
    }

    #[test]
    fn json_parser_refuses_prose() {
        assert!(ToolOutputParser::parse_json("just some words").is_none());
        assert!(ToolOutputParser::parse_json("{not json").is_none());
    }

    #[test]
    fn header_parser_reads_status_and_fields() {
        let raw = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nX-Trace: abc\r\n";
        let out = ToolOutputParser::parse_http_headers(raw).unwrap();
        assert_eq!(out.facts.len(), 2);
        assert_eq!(out.facts[0].qualified_name, "content-type");
        assert_eq!(out.facts[0].signature.as_deref(), Some("application/json"));
        assert!(out.summary.contains("200 OK"));
    }

    #[test]
    fn csv_parser_needs_a_header_row() {
        let out = ToolOutputParser::parse_csv("id,name\n1,a\n2,b\n").unwrap();
        assert_eq!(out.facts.len(), 2);
        assert_eq!(out.facts[1].qualified_name, "name");
        assert!(ToolOutputParser::parse_csv("no delimiters here").is_none());
    }

    #[test]
    fn diff_parser_extracts_hunks_with_files() {
        let raw = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,4 @@\n context\n+added\n@@ -10,2 +11,2 @@\n-x\n+y\n";
        let out = ToolOutputParser::parse_diff(raw).unwrap();
        assert_eq!(out.facts.len(), 2);
        assert_eq!(out.facts[0].file_path.as_deref(), Some("src/lib.rs"));
        assert!(out.facts[0].qualified_name.contains("src/lib.rs"));
    }

    #[test]
    fn exit_code_parser_is_conservative() {
        assert!(ToolOutputParser::parse_exit_code("exit code: 1").is_some());
        let out = ToolOutputParser::parse_exit_code("Exit status 127").unwrap();
        assert_eq!(out.facts[0].signature.as_deref(), Some("127"));
        assert!(ToolOutputParser::parse_exit_code("42").is_none());
    }

    #[test]
    fn parse_any_reports_no_structure_honestly() {
        let out = ToolOutputParser::parse_any(Some("read_file"), "def f():\n    return 1\n");
        assert!(out.is_empty());
        assert!(out.parser.is_none());
        assert!(out.summary.contains("no structure detected"));
    }

    #[test]
    fn parse_any_prefers_diff_for_diff_tools() {
        let raw = "+++ b/a.rs\n@@ -1 +1 @@\n-a\n+b\n";
        let out = ToolOutputParser::parse_any(Some("git_diff"), raw);
        assert_eq!(out.parser, Some("diff"));
    }

    #[test]
    fn only_deterministic_sources_can_construct_a_fact() {
        // The point of this test is compile-time as much as runtime: there is no
        // constructor that accepts an arbitrary string as a provenance.
        let write = SymbolicWrite::new(FactKind::Function, "a::b").body("fn b() {}");
        let fact = SymbolicFact::from_deterministic_source(write, FactSource::TreeSitter, None);
        assert_eq!(fact.source, FactSource::TreeSitter);
        assert_eq!(fact.ast_hash.len(), 16);
        assert!(fact.render().contains("a::b"));
    }

    #[test]
    fn fact_kinds_round_trip_through_the_database_encoding() {
        for kind in [
            FactKind::Function,
            FactKind::JsonField,
            FactKind::DiffHunk,
            FactKind::RegexMatch,
            FactKind::ToolExitCode,
        ] {
            assert_eq!(FactKind::parse(kind.as_str()).unwrap(), kind);
        }
        assert!(FactKind::parse("nope").is_err());
    }
}
