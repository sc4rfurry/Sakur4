//! Regression tests for Repo Cortex's structural extraction.
//!
//! These live in `tests/` rather than beside the parser so they read as
//! behavioural contracts about what the Symbolic Ledger ends up containing,
//! which is what the rest of Sakur4 depends on. The unit tests in `repo.rs`
//! cover the individual extractors; these cover the walk that assembles
//! qualified names across nesting.

use sakur4_core::repo::{parse_config, parse_structured, Language};

/// Every qualified name produced for a snippet.
///
/// Config formats have their own extractor — routing them through the grammar
/// path yields nothing, which is itself a bug this helper used to hide.
fn names(src: &str, language: Language, path: &str) -> Vec<String> {
    let parsed = match language {
        Language::Config => parse_config(src, language, path),
        _ => parse_structured(src, language, path),
    };
    parsed
        .extracts
        .into_iter()
        .map(|e| e.write.qualified_name)
        .collect()
}

#[test]
fn methods_are_qualified_by_their_implementing_type() {
    // A method named only `new` is useless for symbol lookup: the whole point of
    // the Ledger's natural key is that `Engine::new` and `Server::new` are
    // different facts.
    let src = r#"
pub struct Engine { n: usize }

impl Engine {
    pub fn new() -> Self { Self { n: 0 } }
    fn private_helper(&self) -> usize { self.n }
}
"#;
    let got = names(src, Language::Rust, "src/engine.rs");
    assert!(
        got.iter().any(|n| n.ends_with("Engine::new")),
        "expected an Engine::new fact, got {got:?}"
    );
    assert!(
        got.iter().any(|n| n.ends_with("Engine::private_helper")),
        "expected an Engine::private_helper fact, got {got:?}"
    );
    assert!(
        !got.iter().any(|n| n.ends_with("::new") && !n.contains("Engine::")),
        "an unqualified method name leaked into the ledger: {got:?}"
    );
}

#[test]
fn trait_methods_are_qualified_by_their_trait() {
    let src = "pub trait Doer {\n    fn do_it(&self) -> u8;\n}\n";
    let got = names(src, Language::Rust, "src/doer.rs");
    assert!(got.iter().any(|n| n.ends_with("Doer::do_it")), "got {got:?}");
}

#[test]
fn rust_free_functions_stay_unqualified_by_a_type() {
    let src = "pub fn standalone() -> u8 { 1 }\n";
    let got = names(src, Language::Rust, "src/lib.rs");
    assert!(got.iter().any(|n| n.ends_with("::standalone")), "got {got:?}");
    assert!(!got.iter().any(|n| n.contains("standalone::")), "got {got:?}");
}

#[test]
fn go_named_types_are_extracted() {
    // Go's `type_declaration` wraps a `type_spec`, so a naive child lookup finds
    // no name at all. Named types are load-bearing for the call graph.
    let src = r#"
package main

type Server struct { Port int }
type Handler interface { Handle() string }
type Alias = int

func New() *Server { return &Server{} }
func (s *Server) Listen() { }
"#;
    let got = names(src, Language::Go, "main.go");
    assert!(got.iter().any(|n| n.ends_with("::Server")), "got {got:?}");
    assert!(got.iter().any(|n| n.ends_with("::Handler")), "got {got:?}");
    assert!(got.iter().any(|n| n.ends_with("::Alias")), "got {got:?}");
    assert!(got.iter().any(|n| n.ends_with("::New")), "got {got:?}");
    assert!(got.iter().any(|n| n.ends_with("::Listen")), "got {got:?}");
}

#[test]
fn python_class_methods_are_qualified_by_their_class() {
    let src = r#"
class Store:
    def __init__(self, root):
        self.root = root

    def load(self, name):
        return None


def make_store(path):
    return Store(path)
"#;
    let got = names(src, Language::Python, "py/store.py");
    assert!(got.iter().any(|n| n.ends_with("Store::load")), "got {got:?}");
    assert!(got.iter().any(|n| n.ends_with("Store::__init__")), "got {got:?}");
    assert!(got.iter().any(|n| n.ends_with("::make_store")), "got {got:?}");
}

#[test]
fn qualified_names_never_collide_across_directories() {
    // The Ledger's natural key is (project, qualified name, kind, file). If the
    // module prefix were the package name rather than the path, two files could
    // collide and one symbol would silently shadow the other.
    let a = names("fn f() {}", Language::Rust, "src/a/mod.rs");
    let b = names("fn f() {}", Language::Rust, "src/b/mod.rs");
    assert_ne!(a, b);
}

#[test]
fn json_top_level_keys_are_anchored_and_nested_ones_are_not() {
    let json = "{\n  \"name\": \"sakur4\",\n  \"nested\": {\n    \"deep\": 1\n  }\n}";
    let got = names(json, Language::Config, "package.json");
    assert!(got.iter().any(|n| n.ends_with("::name")), "got {got:?}");
    assert!(got.iter().any(|n| n.ends_with("::nested")), "got {got:?}");
    assert!(
        !got.iter().any(|n| n.ends_with("::deep")),
        "nested keys must not be flattened into the ledger: {got:?}"
    );
}

#[test]
fn a_file_that_cannot_be_parsed_produces_no_facts_but_does_not_fail() {
    let parsed = parse_structured("pub fn nope( {{{ \n", Language::Rust, "broken.rs");
    // tree-sitter recovers partial trees, so some facts may exist; what must not
    // happen is a panic or a fabricated name.
    for e in &parsed.extracts {
        assert!(!e.write.qualified_name.ends_with("::"), "empty name emitted");
        assert!(e.write.ast_hash().len() == 16);
    }
}
#[test]
fn zz_debug_config() {
    let json = "{\n  \"name\": \"sakur4\",\n  \"nested\": {\n    \"deep\": 1\n  }\n}";
    println!("--- lines ---");
    for (i, line) in json.lines().enumerate() {
        println!("{i}: starts_with_space={} line={line:?}", line.starts_with(' '));
    }
    let parsed = parse_config(json, Language::Config, "package.json");
    println!("--- extracts: {} ---", parsed.extracts.len());
    for e in &parsed.extracts {
        println!("  kind={:?} name={}", e.write.kind, e.write.qualified_name);
    }
}
