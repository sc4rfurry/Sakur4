//! Synthetic repositories with known structure.
//!
//! Repo Cortex's value is that its output can be checked against ground truth.
//! These fixtures provide that ground truth: a fixed call graph with a known
//! transitive closure, so `impact_of_change` has a right answer to be compared
//! against rather than a shape that merely looks plausible.

use std::path::{Path, PathBuf};

/// What kind of fixture to build.
#[derive(Debug, Clone)]
pub struct FixtureSpec {
    /// Extra unrelated files, to give the indexer something to skip.
    pub filler_files: usize,
    /// Include Rust sources.
    pub rust: bool,
    /// Include Python sources.
    pub python: bool,
    /// Include TypeScript sources.
    pub typescript: bool,
    /// Include Go sources.
    pub go: bool,
    /// Include structured config files.
    pub config: bool,
    /// Include a file with a syntax error.
    pub broken: bool,
}

impl Default for FixtureSpec {
    fn default() -> Self {
        Self {
            filler_files: 0,
            rust: true,
            python: true,
            typescript: true,
            go: true,
            config: true,
            broken: false,
        }
    }
}

impl FixtureSpec {
    /// The smallest fixture that still has a non-trivial call graph.
    pub fn minimal() -> Self {
        Self {
            rust: true,
            python: false,
            typescript: false,
            go: false,
            config: false,
            ..Default::default()
        }
    }

    /// A larger repository, for incremental-index timing checks.
    pub fn large(files: usize) -> Self {
        Self { filler_files: files, ..Default::default() }
    }
}

/// A materialised fixture repository.
pub struct FixtureRepo {
    /// Held so the temporary directory outlives the fixture: dropping it deletes
    /// the tree. Nothing reads the field, which is the point.
    #[allow(dead_code)]
    dir: tempfile::TempDir,
    root: PathBuf,
}

impl std::fmt::Debug for FixtureRepo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FixtureRepo").field("root", &self.root).finish()
    }
}

impl FixtureRepo {
    /// Create a fixture in a temporary directory.
    pub fn create(spec: FixtureSpec) -> std::io::Result<Self> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        write_files(&root, &spec)?;
        Ok(Self { dir, root })
    }

    /// Create a fixture whose lifetime is tied to the caller's directory.
    pub fn create_at(root: impl AsRef<Path>, spec: FixtureSpec) -> std::io::Result<PathBuf> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        write_files(&root, &spec)?;
        Ok(root)
    }

    /// The repository root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Modify a file, for incremental-index and staleness tests.
    pub fn write(&self, rel: &str, contents: &str) -> std::io::Result<()> {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)
    }

    /// Delete a file, for removal tests.
    pub fn remove(&self, rel: &str) -> std::io::Result<()> {
        std::fs::remove_file(self.root.join(rel))
    }

    /// The known answer for `impact_of_change("auth::validate")` in the Rust
    /// fixture: every transitive caller, which the tests assert exactly.
    pub fn known_impact_of_auth_validate() -> &'static [&'static str] {
        &[
            "src/auth.rs::validate",
            "src/auth.rs::login",
            "src/handlers.rs::login_handler",
            "src/main.rs::bootstrap",
        ]
    }
}

fn write_files(root: &Path, spec: &FixtureSpec) -> std::io::Result<()> {
    let w = |rel: &str, contents: &str| -> std::io::Result<()> {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)
    };

    w(".gitignore", "target/\nnode_modules/\n*.log\n")?;
    w("README.md", "# Fixture repository\n\nA synthetic repo for Sakur4 tests.\n")?;

    if spec.rust {
        // A deliberate four-level call chain, so the transitive closure has a
        // known shape: bootstrap -> login_handler -> login -> validate.
        w(
            "src/main.rs",
            r#"mod auth;
mod handlers;

use crate::handlers::login_handler;

pub fn bootstrap() -> bool {
    login_handler("alice", "secret")
}

fn unused_helper() -> u8 {
    0
}
"#,
        )?;
        w(
            "src/auth.rs",
            r#"use std::collections::HashMap;

pub struct Session {
    pub user: String,
}

pub fn validate(user: &str, password: &str) -> bool {
    let table = load_table();
    table.contains_key(user) && password.len() > 3
}

fn load_table() -> HashMap<String, String> {
    HashMap::new()
}

pub fn login(user: &str, password: &str) -> Option<Session> {
    if validate(user, password) {
        Some(Session { user: user.to_string() })
    } else {
        None
    }
}
"#,
        )?;
        w(
            "src/handlers.rs",
            r#"use crate::auth::login;

pub fn login_handler(user: &str, password: &str) -> bool {
    login(user, password).is_some()
}

pub fn health() -> &'static str {
    "ok"
}
"#,
        )?;
    }

    if spec.python {
        w(
            "py/store.py",
            r#"import json
from pathlib import Path


class Store:
    def __init__(self, root: Path):
        self.root = root

    def load(self, name: str) -> dict:
        return json.loads((self.root / name).read_text())

    def save(self, name: str, payload: dict) -> None:
        (self.root / name).write_text(json.dumps(payload))


def make_store(path: str) -> Store:
    return Store(Path(path))
"#,
        )?;
    }

    if spec.typescript {
        w(
            "ts/server.ts",
            r#"import { createServer } from "http";

export interface Config {
  port: number;
}

export type Handler = (path: string) => string;

export class Server {
  constructor(private config: Config) {}

  start(): void {
    createServer((req, res) => res.end(this.route(req.url ?? "/")));
  }

  private route(path: string): string {
    return lookup(path);
  }
}

export function lookup(path: string): string {
  return path === "/" ? "home" : "other";
}

export const health = (): string => "ok";
"#,
        )?;
    }

    if spec.go {
        w(
            "go/main.go",
            r#"package main

import "fmt"

type Server struct {
	Port int
}

type Handler interface {
	Handle(path string) string
}

func New(port int) *Server {
	return &Server{Port: port}
}

func (s *Server) Start() {
	fmt.Println(route("/"))
}

func route(path string) string {
	return path
}
"#,
        )?;
    }

    if spec.config {
        w(
            "config/app.json",
            "{\n  \"name\": \"fixture\",\n  \"port\": 8080,\n  \"nested\": {\n    \"deep\": true\n  }\n}\n",
        )?;
        w("config/app.toml", "[server]\nport = 8080\nhost = \"localhost\"\n")?;
        w(
            "schema.sql",
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT);\nCREATE VIEW active AS SELECT * FROM users;\n",
        )?;
        w("scripts/deploy.sh", "#!/bin/bash\nfunction deploy() {\n  echo deploying\n}\n")?;
    }

    if spec.broken {
        w("src/broken.rs", "pub fn ok() -> u8 { 1 }\npub fn nope( {{{ \n")?;
    }

    for i in 0..spec.filler_files {
        let dir = format!("src/filler/mod_{}", i % 50);
        w(
            &format!("{dir}/file_{i}.rs"),
            &format!(
                "// filler file {i}\npub fn filler_{i}() -> usize {{\n    {i}\n}}\n\n\
                 pub fn calls_previous() -> usize {{ filler_{i}() }}\n"
            ),
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_fixture_has_the_expected_call_chain() {
        let repo = FixtureRepo::create(FixtureSpec::minimal()).unwrap();
        let main = std::fs::read_to_string(repo.root().join("src/main.rs")).unwrap();
        assert!(main.contains("login_handler"));
        let auth = std::fs::read_to_string(repo.root().join("src/auth.rs")).unwrap();
        assert!(auth.contains("fn validate"));
        assert!(auth.contains("fn login"));
    }

    #[test]
    fn large_fixture_produces_the_requested_file_count() {
        let repo = FixtureRepo::create(FixtureSpec::large(120)).unwrap();
        let count = walkdir::WalkDir::new(repo.root())
            .into_iter()
            .flatten()
            .filter(|e| e.file_type().is_file())
            .count();
        assert!(count >= 120, "expected at least 120 files, got {count}");
    }

    #[test]
    fn write_and_remove_affect_the_tree() {
        let repo = FixtureRepo::create(FixtureSpec::minimal()).unwrap();
        repo.write("src/extra.rs", "pub fn extra() {}").unwrap();
        assert!(repo.root().join("src/extra.rs").exists());
        repo.remove("src/extra.rs").unwrap();
        assert!(!repo.root().join("src/extra.rs").exists());
    }
}
