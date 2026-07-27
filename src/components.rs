//! Components: the granularity an engineer actually acts on.
//!
//! "Which repo is this about?" is a useful answer once. "Which component" is the answer
//! you can start from — and in a monorepo, repo-level routing barely narrows anything.
//!
//! A component is a **module root**: a directory that declares itself one by carrying a
//! manifest (`Cargo.toml`, `package.json`, `go.mod`, `pyproject.toml`), or that sits
//! directly under a conventional source root (`crates/`, `packages/`, `services/`,
//! `apps/`, `cmd/`, `src/`). Derived from what's on disk, never guessed: a component
//! list that includes modules the repo doesn't have sends the operator hunting, which is
//! the same failure the repo index avoids by reading code rather than the README.
//!
//! Each component gets the same two-line card the repo index uses — `PURPOSE:` what this
//! runs, `SYMPTOMS:` the terms that should route an incident here — because that shape is
//! already what routing reads, and one vocabulary beats two.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tracing::debug;

/// Directory names that hold sibling modules rather than being modules themselves.
const CONTAINER_DIRS: &[&str] = &[
    "crates",
    "packages",
    "services",
    "apps",
    "cmd",
    "libs",
    "modules",
    "components",
];

/// Files that mark a directory as a module root in its own right.
const MANIFESTS: &[&str] = &[
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
    "setup.py",
    "build.gradle",
    "pom.xml",
    "CMakeLists.txt",
];

/// Directories that are never components.
const SKIP: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    ".venv",
    "__pycache__",
    ".idea",
    ".vscode",
];

/// A component found on disk, with the structural digest its summary is derived from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// Repo-relative path, e.g. `crates/partition-processor`.
    pub path: String,
    /// Layout, file-type counts, and module names — names being the
    /// highest-signal-per-token description a codebase offers, and a digest staying
    /// bounded whether the component is one file or ten thousand.
    pub digest: String,
}

/// Find a repo's components by walking the checkout.
///
/// Returns at most `max` of them, largest first: a cap keeps a pathological monorepo
/// from minting a thousand summaries, and taking the biggest first means the cap drops
/// the components least likely to be the answer.
pub fn discover(root: &Path, max: usize) -> Vec<Component> {
    let mut found: BTreeMap<String, ComponentFiles> = BTreeMap::new();

    // A repo with no internal structure is one component: itself. Recorded explicitly so
    // every repo has at least one row, and "which component" always has an answer.
    collect_into(root, root, &mut found, 0);
    if found.is_empty() {
        let mut files = ComponentFiles::default();
        walk_files(root, &mut files, 0);
        found.insert(".".to_string(), files);
    }

    let mut components: Vec<(usize, Component)> = found
        .into_iter()
        .map(|(path, files)| {
            (
                files.total,
                Component {
                    digest: files.render(&path),
                    path,
                },
            )
        })
        .collect();
    components.sort_by_key(|(total, _)| std::cmp::Reverse(*total));
    components.truncate(max);
    components.sort_by(|a, b| a.1.path.cmp(&b.1.path));
    debug!(
        "components: {} found under {}",
        components.len(),
        root.display()
    );
    components.into_iter().map(|(_, c)| c).collect()
}

/// Which component a changed path belongs to, longest prefix first.
///
/// Used to attribute a commit to components from its file list alone — no checkout
/// needed, so a commit can be attributed long after the tree has moved on.
pub fn attribute_path<'a>(path: &str, components: &'a [String]) -> Option<&'a String> {
    components
        .iter()
        .filter(|c| *c != "." && path.starts_with(c.as_str()))
        .max_by_key(|c| c.len())
        .or_else(|| components.iter().find(|c| *c == "."))
}

#[derive(Debug, Clone, Default)]
struct ComponentFiles {
    total: usize,
    /// Extension → count, so the digest says what kind of thing this is.
    by_ext: BTreeMap<String, usize>,
    /// Immediate child names — the module names.
    names: BTreeSet<String>,
    /// The manifest that marked it, if any.
    manifest: Option<String>,
}

impl ComponentFiles {
    fn render(&self, path: &str) -> String {
        let mut exts: Vec<(&String, &usize)> = self.by_ext.iter().collect();
        exts.sort_by(|a, b| b.1.cmp(a.1));
        let types = exts
            .iter()
            .take(6)
            .map(|(e, n)| format!("{e}:{n}"))
            .collect::<Vec<_>>()
            .join(" ");
        let names = self
            .names
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        let mut out = format!("path: {path}\nfiles: {} ({types})\n", self.total);
        if let Some(m) = &self.manifest {
            out.push_str(&format!("manifest: {m}\n"));
        }
        out.push_str(&format!("modules: {names}\n"));
        out
    }
}

/// Walk looking for module roots. Bounded depth: a component nested six levels down is
/// not a component anyone routes to.
fn collect_into(root: &Path, dir: &Path, out: &mut BTreeMap<String, ComponentFiles>, depth: usize) {
    if depth > 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let entries: Vec<_> = entries.flatten().collect();
    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP.contains(&name.as_str()) || name.starts_with('.') {
            continue;
        }
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();

        let manifest = MANIFESTS
            .iter()
            .find(|m| path.join(m).is_file())
            .map(|m| (*m).to_string());
        let is_container = CONTAINER_DIRS.contains(&name.as_str()) && depth == 0;

        if manifest.is_some() || (!is_container && depth > 0) {
            let mut files = ComponentFiles {
                manifest,
                ..Default::default()
            };
            walk_files(&path, &mut files, 0);
            if files.total > 0 {
                out.insert(rel, files);
            }
            // A module root's children are its internals, not sibling components.
            continue;
        }
        collect_into(root, &path, out, depth + 1);
    }
}

fn walk_files(dir: &Path, files: &mut ComponentFiles, depth: usize) {
    if depth > 6 || files.total > 5_000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if SKIP.contains(&name.as_str()) {
            continue;
        }
        let path = entry.path();
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if depth == 0 {
                files.names.insert(name);
            }
            walk_files(&path, files, depth + 1);
        } else {
            files.total += 1;
            if depth == 0 {
                files.names.insert(name.clone());
            }
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                *files.by_ext.entry(ext.to_string()).or_insert(0) += 1;
            }
        }
    }
}

/// Split a component card into its two lines. Same shape the repo index uses, so one
/// vocabulary covers both.
pub fn split_card(card: &str) -> (Option<String>, Option<String>) {
    let mut purpose = None;
    let mut symptoms = None;
    for line in card.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("PURPOSE:") {
            purpose = Some(rest.trim().to_string()).filter(|s| !s.is_empty());
        } else if let Some(rest) = line.strip_prefix("SYMPTOMS:") {
            symptoms = Some(rest.trim().to_string()).filter(|s| !s.is_empty());
        }
    }
    (purpose, symptoms)
}

/// The prompt for one component card.
pub fn card_prompt(full_name: &str, component: &Component) -> String {
    format!(
        "Repository: {full_name}\nComponent: {}\n\n=== STRUCTURE ===\n{}\n\n\
         === YOUR TASK ===\nWrite exactly two lines describing THIS COMPONENT, not the \
         repository:\n\
         PURPOSE: <one sentence: what this component runs or provides>\n\
         SYMPTOMS: <comma-separated terms that should route an incident to this \
         component — the words that would appear in an alert or an error about it>\n\n\
         Use only the structure above. If it is too thin to tell, say so in PURPOSE \
         rather than guessing.",
        component.path, component.digest
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("mb-comp-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn touch(root: &Path, rel: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, "x").unwrap();
    }

    #[test]
    fn a_manifest_marks_a_module_root_and_its_children_are_internals() {
        let root = scratch("manifest");
        touch(&root, "crates/engine/Cargo.toml");
        touch(&root, "crates/engine/src/lib.rs");
        touch(&root, "crates/engine/src/deep/nested.rs");
        touch(&root, "crates/api/Cargo.toml");
        touch(&root, "crates/api/src/main.rs");

        let paths: Vec<String> = discover(&root, 20).into_iter().map(|c| c.path).collect();
        assert_eq!(paths, vec!["crates/api", "crates/engine"]);
        // `crates/` is a container, and `src/` inside a module is not a sibling component.
        assert!(!paths.iter().any(|p| p == "crates" || p.contains("/src")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_flat_repo_is_one_component_so_the_question_always_has_an_answer() {
        let root = scratch("flat");
        touch(&root, "main.py");
        touch(&root, "util.py");
        let components = discover(&root, 20);
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].path, ".");
        assert!(components[0].digest.contains("py:2"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn build_output_and_dependencies_are_not_components() {
        let root = scratch("skip");
        touch(&root, "crates/real/Cargo.toml");
        touch(&root, "crates/real/src/lib.rs");
        touch(&root, "node_modules/left-pad/package.json");
        touch(&root, "target/debug/build.rs");
        let paths: Vec<String> = discover(&root, 20).into_iter().map(|c| c.path).collect();
        assert_eq!(paths, vec!["crates/real"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_changed_path_attributes_to_its_deepest_component() {
        let components = vec![
            "crates/engine".to_string(),
            "crates/engine/sub".to_string(),
            "crates/api".to_string(),
        ];
        assert_eq!(
            attribute_path("crates/engine/sub/lib.rs", &components).map(String::as_str),
            Some("crates/engine/sub"),
            "the longest matching prefix wins, or a nested module is invisible"
        );
        assert_eq!(
            attribute_path("crates/api/main.rs", &components).map(String::as_str),
            Some("crates/api")
        );
        // Nothing matches and there is no flat-repo component: unattributed rather than
        // guessed onto whichever component sorted first.
        assert_eq!(attribute_path("README.md", &components), None);
    }

    #[test]
    fn a_flat_repo_absorbs_any_changed_path() {
        let components = vec![".".to_string()];
        assert_eq!(
            attribute_path("anything/at/all.rs", &components).map(String::as_str),
            Some(".")
        );
    }

    #[test]
    fn the_card_splits_into_purpose_and_symptoms() {
        let (p, s) =
            split_card("PURPOSE: Runs the partition processor.\nSYMPTOMS: partition, stuck, lag\n");
        assert_eq!(p.as_deref(), Some("Runs the partition processor."));
        assert_eq!(s.as_deref(), Some("partition, stuck, lag"));
        // A model that answers with prose gets nothing extracted rather than the prose
        // stored as if it were a routing key.
        let (p2, s2) = split_card("This component seems to handle partitions.");
        assert!(p2.is_none() && s2.is_none());
    }
}
