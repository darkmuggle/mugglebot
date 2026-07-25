//! What kind of system is this, actually?
//!
//! A proposal is only useful if it's expressed in the idiom of the stack it's for.
//! Asked how to add "YAML validation" to a Kubernetes control plane, a model given
//! nothing but six TypeScript files will answer with `js-yaml` and a JSON schema —
//! technically about YAML, and not how anyone validates manifests in that ecosystem.
//! The real answers are admission webhooks, `ValidatingAdmissionPolicy`, CRD OpenAPI
//! schemas, or a manifest linter in CI. The model didn't get that wrong through lack
//! of capability; it was never told what it was looking at.
//!
//! So before asking for approaches, MuggleBot detects the ecosystem from the
//! checkout — from **markers that are actually present**, never inferred — and states
//! it in the prompt along with that ecosystem's real extension points and the
//! dependencies the repo already has.
//!
//! The dependency list matters for a second reason: it's what makes "don't invent a
//! library" checkable. A model that proposed `flux-schema` (which does not exist)
//! would have had to either find it in the manifest or declare it a new dependency.

use std::collections::BTreeSet;
use std::path::Path;

/// A detected ecosystem, with the evidence that detected it.
#[derive(Debug, Clone, Default)]
pub struct Ecosystem {
    /// Platform/framework labels, e.g. `kubernetes`, `kubernetes-operator`, `helm`.
    pub platforms: Vec<&'static str>,
    /// Languages, from manifests present.
    pub languages: Vec<&'static str>,
    /// Dependencies already declared, so a proposal can prefer what's here and a
    /// new dependency has to be named as one.
    pub dependencies: Vec<String>,
    /// Files that produced the detection — the citation.
    pub evidence: Vec<String>,
}

impl Ecosystem {
    pub fn is_empty(&self) -> bool {
        self.platforms.is_empty() && self.languages.is_empty()
    }

    /// The prompt block: what this is, how it's extended, and what's already here.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return "(could not determine the stack from the checkout; do not assume one)".into();
        }
        let mut out = String::new();
        if !self.platforms.is_empty() {
            out.push_str(&format!("Platform: {}\n", self.platforms.join(", ")));
        }
        if !self.languages.is_empty() {
            out.push_str(&format!("Languages: {}\n", self.languages.join(", ")));
        }
        if !self.evidence.is_empty() {
            out.push_str(&format!(
                "Detected from: {}\n",
                self.evidence
                    .iter()
                    .take(8)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !self.dependencies.is_empty() {
            out.push_str(&format!(
                "Existing dependencies (prefer these; anything else is a NEW dependency): {}\n",
                self.dependencies
                    .iter()
                    .take(60)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        let mechanisms: BTreeSet<&str> = self
            .platforms
            .iter()
            .flat_map(|p| mechanisms_for(p))
            .copied()
            .collect();
        if !mechanisms.is_empty() {
            out.push_str(
                "\nThis ecosystem's native extension points — a proposal should use one of \
                 these rather than a language-generic equivalent:\n",
            );
            for m in mechanisms {
                out.push_str(&format!("  - {m}\n"));
            }
        }
        out
    }
}

/// Where each ecosystem actually does cross-cutting work.
///
/// Kept short and factual. This is the knowledge a model reliably has but doesn't
/// apply unless told which ecosystem it's in — naming the mechanisms is what turns
/// "validate the YAML" into "validate it where this platform validates things".
fn mechanisms_for(platform: &str) -> &'static [&'static str] {
    match platform {
        "kubernetes" => &[
            "CRD OpenAPI v3 schema (`spec.versions[].schema`) — rejects bad fields at the API server, no code",
            "ValidatingAdmissionPolicy with CEL — in-tree validation, no webhook to run",
            "ValidatingAdmissionWebhook / MutatingAdmissionWebhook — for logic a schema can't express",
            "Manifest linting in CI (kubeconform, kubeval, `kubectl --dry-run=server`)",
            "`kubectl explain` / API discovery as the source of truth for field names",
        ],
        "kubernetes-operator" => &[
            "Kubebuilder/controller-runtime validation markers (`+kubebuilder:validation:...`) generating the CRD schema",
            "A webhook in the same manager (`SetupWebhookWithManager`) for cross-field rules",
            "Status conditions to report rejection back to the user",
            "envtest for validation behaviour",
        ],
        "helm" => &[
            "`values.schema.json` — Helm validates values against it on install/upgrade",
            "`helm lint` and `helm template --validate` in CI",
            "`required`/`fail` template functions for values that must be set",
        ],
        "kustomize" => &["`kustomize build | kubeconform` in CI", "Overlay-level patches validated by schema"],
        "terraform" => &[
            "Variable `validation` blocks",
            "`terraform validate` and `tflint` in CI",
            "Policy-as-code (OPA/Sentinel/Conftest) on the plan",
        ],
        // cdk8s synthesizes Kubernetes manifests from a real programming language,
        // so validation moves *left* — into the type system at synth time — while the
        // cluster-side mechanisms still apply to what gets applied.
        "cdk8s" => &[
            "Typed constructs — an invalid field is a compile error before anything is synthesized",
            "`cdk8s import` to generate types from a CRD, so custom resources are typed too",
            "`cdk8s synth | kubeconform` in CI, validating the generated manifests",
            "Snapshot tests over synthesized output to catch unintended manifest changes",
        ],
        "cdk" | "pulumi" => &[
            "Types in the synthesizing language — invalid config fails at compile time",
            "`cdk synth` / `pulumi preview` in CI",
            "Aspects / policy packs for cross-stack rules",
        ],
        "github-actions" => &[
            "A required status check running the validator",
            "Reusable workflow so every repo validates the same way",
        ],
        _ => &[],
    }
}

/// Detect the ecosystem from a checked-out tree.
///
/// Every detection is evidence-based: a marker file or a declared dependency, never
/// a guess from a repository name. A wrong ecosystem is worse than none — it would
/// confidently steer proposals into the wrong idiom.
pub fn detect(root: &Path) -> Ecosystem {
    let mut eco = Ecosystem::default();
    let note = |eco: &mut Ecosystem, platform: &'static str, file: &str| {
        if !eco.platforms.contains(&platform) {
            eco.platforms.push(platform);
        }
        if !eco.evidence.iter().any(|e| e == file) {
            eco.evidence.push(file.to_string());
        }
    };

    // ---- manifests ----
    if let Some(pkg) = read(root, "package.json") {
        eco.languages.push("typescript/javascript");
        eco.evidence.push("package.json".into());
        eco.dependencies.extend(npm_dependencies(&pkg));
    }
    if let Some(cargo) = read(root, "Cargo.toml") {
        eco.languages.push("rust");
        eco.evidence.push("Cargo.toml".into());
        eco.dependencies.extend(toml_dependencies(&cargo));
    }
    if let Some(gomod) = read(root, "go.mod") {
        eco.languages.push("go");
        eco.evidence.push("go.mod".into());
        eco.dependencies.extend(go_dependencies(&gomod));
    }
    for (file, lang) in [
        ("pyproject.toml", "python"),
        ("setup.py", "python"),
        ("pom.xml", "java"),
        ("build.gradle", "java/kotlin"),
        ("build.gradle.kts", "kotlin"),
    ] {
        if root.join(file).exists() {
            if !eco.languages.contains(&lang) {
                eco.languages.push(lang);
            }
            eco.evidence.push(file.to_string());
        }
    }

    // ---- platform markers ----
    if root.join("Chart.yaml").exists() || dir_contains(root, "charts") {
        note(&mut eco, "helm", "Chart.yaml");
    }
    if find_file(root, "kustomization.yaml").is_some() {
        note(&mut eco, "kustomize", "kustomization.yaml");
    }
    if find_file(root, "main.tf").is_some() || find_file(root, "versions.tf").is_some() {
        note(&mut eco, "terraform", "*.tf");
    }
    if root.join("cdk.json").exists() {
        note(&mut eco, "cdk", "cdk.json");
    }
    if root.join("Pulumi.yaml").exists() {
        note(&mut eco, "pulumi", "Pulumi.yaml");
    }
    if dir_contains(root, ".github/workflows") {
        note(&mut eco, "github-actions", ".github/workflows");
    }

    // Kubernetes: a CRD or a manifest with apiVersion+kind is definitive. Checked by
    // *content* because a `k8s/` directory name proves nothing.
    if let Some(path) = find_kubernetes_manifest(root) {
        note(&mut eco, "kubernetes", &path);
    }
    // An operator is Kubernetes plus a controller runtime.
    let deps = eco.dependencies.join(" ");
    if deps.contains("controller-runtime")
        || deps.contains("kubebuilder")
        || deps.contains("operator-sdk")
        || root.join("PROJECT").exists()
    {
        note(&mut eco, "kubernetes-operator", "controller-runtime");
        note(&mut eco, "kubernetes", "controller-runtime");
    }
    // A Kubernetes client library also implies the platform, even with no manifests
    // in this repo (a control plane that talks to clusters).
    if deps.contains("@kubernetes/client-node")
        || deps.contains("k8s.io/client-go")
        || deps.contains("kube-rs")
        || deps.contains("kubernetes-client")
        || deps.contains("kubectl")
    {
        note(&mut eco, "kubernetes", "kubernetes client library");
    }
    // cdk8s: Kubernetes manifests generated from code. Worth its own platform because
    // its idiom is different from hand-written YAML — validation lives in the type
    // system at synth time, not only in the cluster.
    if deps.contains("cdk8s") {
        note(&mut eco, "cdk8s", "cdk8s");
        note(&mut eco, "kubernetes", "cdk8s");
    }

    eco.dependencies.sort();
    eco.dependencies.dedup();
    eco
}

// ---- helpers -----------------------------------------------------------------

fn read(root: &Path, name: &str) -> Option<String> {
    std::fs::read_to_string(root.join(name)).ok()
}

fn dir_contains(root: &Path, rel: &str) -> bool {
    root.join(rel).is_dir()
}

/// Find `name` within a couple of levels — deep enough for `deploy/`, `charts/`, or
/// `config/`, shallow enough not to walk a monorepo.
fn find_file(root: &Path, name: &str) -> Option<String> {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > 3 {
            continue;
        }
        let entries = std::fs::read_dir(&dir).ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !matches!(
                    file_name.as_str(),
                    ".git" | "node_modules" | "target" | "vendor" | "dist"
                ) {
                    stack.push((path, depth + 1));
                }
            } else if file_name == name {
                return Some(
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string(),
                );
            }
        }
    }
    None
}

/// A YAML file that is actually a Kubernetes object — `apiVersion` plus `kind`.
/// Checked by content: a directory called `k8s/` proves nothing, and plenty of
/// non-Kubernetes YAML exists.
fn find_kubernetes_manifest(root: &Path) -> Option<String> {
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut checked = 0usize;
    while let Some((dir, depth)) = stack.pop() {
        if depth > 3 || checked > 400 {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                if !matches!(
                    name.as_str(),
                    ".git" | "node_modules" | "target" | "vendor" | "dist"
                ) {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !(name.ends_with(".yaml") || name.ends_with(".yml")) {
                continue;
            }
            checked += 1;
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            let head: String = body.chars().take(2_000).collect();
            if head.contains("apiVersion:") && head.contains("kind:") {
                return Some(
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string(),
                );
            }
        }
    }
    None
}

fn npm_dependencies(pkg: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(pkg) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for section in ["dependencies", "devDependencies", "peerDependencies"] {
        if let Some(map) = v.get(section).and_then(|d| d.as_object()) {
            out.extend(map.keys().cloned());
        }
    }
    out
}

fn toml_dependencies(cargo: &str) -> Vec<String> {
    let Ok(v) = toml::from_str::<toml::Value>(cargo) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(map) = v.get(section).and_then(|d| d.as_table()) {
            out.extend(map.keys().cloned());
        }
    }
    out
}

fn go_dependencies(gomod: &str) -> Vec<String> {
    gomod
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("k8s.io/") || l.starts_with("sigs.k8s.io/") || l.contains('/'))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|m| m.contains('/') && !m.starts_with("//"))
        .map(str::to_string)
        .take(80)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("mb-eco-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// The case that motivated this: a Kubernetes control plane written in
    /// TypeScript. Without the platform, "add YAML validation" gets answered with
    /// `js-yaml`; with it, the admission-webhook mechanisms are on the table.
    #[test]
    fn a_typescript_kubernetes_control_plane_is_detected_as_kubernetes() {
        let root = tmp("k8s-ts");
        std::fs::write(
            root.join("package.json"),
            r#"{"dependencies":{"@kubernetes/client-node":"^0.20.0","zod":"^3"}}"#,
        )
        .unwrap();
        let eco = detect(&root);
        assert!(eco.platforms.contains(&"kubernetes"), "{:?}", eco.platforms);
        assert!(eco.languages.contains(&"typescript/javascript"));
        assert!(eco.dependencies.iter().any(|d| d == "zod"));

        // And the rendered block names the real mechanisms.
        let rendered = eco.render();
        assert!(rendered.contains("ValidatingAdmissionPolicy"));
        assert!(rendered.contains("CRD OpenAPI"));
        // Existing deps are offered so a proposal prefers them.
        assert!(rendered.contains("zod"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// A directory named `k8s/` proves nothing; a manifest with apiVersion+kind does.
    #[test]
    fn kubernetes_is_detected_by_manifest_content_not_by_directory_name() {
        let root = tmp("k8s-name");
        std::fs::create_dir_all(root.join("k8s")).unwrap();
        std::fs::write(root.join("k8s/notes.yaml"), "just: some config\n").unwrap();
        assert!(!detect(&root).platforms.contains(&"kubernetes"));

        std::fs::write(
            root.join("k8s/deploy.yaml"),
            "apiVersion: apps/v1\nkind: Deployment\n",
        )
        .unwrap();
        assert!(detect(&root).platforms.contains(&"kubernetes"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn an_operator_is_kubernetes_plus_a_controller_runtime() {
        let root = tmp("operator");
        std::fs::write(
            root.join("go.mod"),
            "module x\n\nrequire (\n\tsigs.k8s.io/controller-runtime v0.17.0\n)\n",
        )
        .unwrap();
        let eco = detect(&root);
        assert!(eco.platforms.contains(&"kubernetes-operator"));
        assert!(eco.platforms.contains(&"kubernetes"));
        assert!(eco.render().contains("kubebuilder:validation"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn helm_and_terraform_are_detected_from_their_markers() {
        let root = tmp("helm-tf");
        std::fs::write(root.join("Chart.yaml"), "name: x\nversion: 1\n").unwrap();
        std::fs::create_dir_all(root.join("infra")).unwrap();
        std::fs::write(root.join("infra/main.tf"), "provider \"aws\" {}\n").unwrap();
        let eco = detect(&root);
        assert!(eco.platforms.contains(&"helm"));
        assert!(eco.platforms.contains(&"terraform"));
        let rendered = eco.render();
        assert!(rendered.contains("values.schema.json"));
        assert!(
            rendered.contains("validation` blocks") || rendered.contains("Variable `validation")
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// Their actual repository: a CDK/cdk8s TypeScript control plane that generates
    /// Kubernetes manifests. Detecting `cdk8s` is what puts "typed constructs" and
    /// "kubeconform on synth output" in front of the model instead of `js-yaml`.
    #[test]
    fn a_cdk8s_control_plane_gets_the_kubernetes_and_cdk8s_idioms() {
        let root = tmp("cdk8s");
        std::fs::write(
            root.join("package.json"),
            r#"{"dependencies":{"cdk8s":"^2","aws-cdk-lib":"^2","@aws-cdk/lambda-layer-kubectl-v34":"^2"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("cdk.json"), "{}").unwrap();
        let eco = detect(&root);
        assert!(eco.platforms.contains(&"cdk8s"), "{:?}", eco.platforms);
        assert!(eco.platforms.contains(&"kubernetes"));
        assert!(eco.platforms.contains(&"cdk"));

        let rendered = eco.render();
        // The cdk8s-native answer…
        assert!(rendered.contains("cdk8s import"));
        assert!(rendered.contains("kubeconform"));
        // …and the cluster-side one.
        assert!(rendered.contains("ValidatingAdmissionPolicy"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// An undetectable stack must say so rather than let the model assume one.
    #[test]
    fn an_unknown_stack_is_stated_as_unknown() {
        let root = tmp("empty");
        let eco = detect(&root);
        assert!(eco.is_empty());
        assert!(eco.render().contains("do not assume one"));
        std::fs::remove_dir_all(&root).ok();
    }
}
