//! The workspace as the guards see it: `cargo metadata` plus every target's
//! module tree, loaded once per process.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

use serde_json::Value;

use crate::source::{self, Job, SourceFile};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum TargetKind {
    Lib,
    Bin,
    Test,
    Bench,
    Example,
    Build,
}

impl TargetKind {
    pub fn is_production(self) -> bool {
        matches!(self, TargetKind::Lib | TargetKind::Bin)
    }

    fn from_metadata(kinds: &[Value]) -> TargetKind {
        let kind = kinds.first().and_then(Value::as_str).unwrap_or("");
        match kind {
            "bin" => TargetKind::Bin,
            "test" => TargetKind::Test,
            "bench" => TargetKind::Bench,
            "example" => TargetKind::Example,
            "custom-build" => TargetKind::Build,
            _ => TargetKind::Lib,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Target {
    pub name: String,
    pub kind: TargetKind,
    /// The root file, relative to the workspace root.
    pub src: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DepKind {
    Normal,
    Dev,
    Build,
}

#[derive(Clone, Debug)]
pub struct Dep {
    pub name: String,
    /// The name the code uses: the rename if there is one, else the package
    /// name with `-` turned into `_`.
    pub ident: String,
    pub kind: DepKind,
    pub target: Option<String>,
    pub optional: bool,
    pub features: Vec<String>,
    pub first_party: bool,
}

#[derive(Clone, Debug)]
pub struct Package {
    pub name: String,
    /// The package directory, relative to the workspace root.
    pub dir: String,
    pub lib_name: Option<String>,
    pub targets: Vec<Target>,
    pub deps: Vec<Dep>,
    pub features: BTreeMap<String, Vec<String>>,
}

impl Package {
    pub fn normal_deps(&self) -> impl Iterator<Item = &Dep> {
        self.deps.iter().filter(|d| d.kind != DepKind::Dev)
    }
}

pub struct Workspace {
    root: PathBuf,
    packages: Vec<Package>,
    files: Vec<SourceFile>,
    by_rel: HashMap<String, usize>,
}

pub fn ws() -> &'static Workspace {
    static WS: OnceLock<Workspace> = OnceLock::new();
    WS.get_or_init(|| {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Workspace::load(&root)
            .unwrap_or_else(|e| panic!("the guards cannot read the workspace:\n{e}"))
    })
}

impl Workspace {
    pub fn load(root: &Path) -> Result<Workspace, String> {
        let metadata = metadata(root)?;
        let root = PathBuf::from(
            metadata["workspace_root"]
                .as_str()
                .ok_or("cargo metadata has no workspace_root")?,
        );
        let packages = packages(&metadata, &root)?;
        let files = walk(&root, &packages)?;
        let by_rel = files
            .iter()
            .enumerate()
            .map(|(i, f)| (f.rel.clone(), i))
            .collect();
        Ok(Workspace {
            root,
            packages,
            files,
            by_rel,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn packages(&self) -> &[Package] {
        &self.packages
    }

    pub fn package(&self, name: &str) -> &Package {
        self.packages
            .iter()
            .find(|p| p.name == name)
            .unwrap_or_else(|| {
                let names: Vec<&str> = self.packages.iter().map(|p| p.name.as_str()).collect();
                panic!(
                    "no workspace package is called `{name}`: either the guard names it wrong \
                 or the package was renamed. The packages are {names:?}"
                )
            })
    }

    pub fn files(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter()
    }

    pub fn production(&self) -> impl Iterator<Item = &SourceFile> {
        self.files
            .iter()
            .filter(|f| !f.test_only && f.target.is_production())
    }

    pub fn tests(&self) -> impl Iterator<Item = &SourceFile> {
        self.files.iter().filter(|f| {
            f.test_only
                || matches!(
                    f.target,
                    TargetKind::Test | TargetKind::Bench | TargetKind::Example
                )
        })
    }

    pub fn file(&self, rel: &str) -> &SourceFile {
        match self.by_rel.get(rel) {
            Some(&i) => &self.files[i],
            None => panic!("{rel} is in no target's module tree: renamed, deleted, or never declared with `mod`"),
        }
    }
}

fn metadata(root: &Path) -> Result<Value, String> {
    let output = std::process::Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
            "--manifest-path",
        ])
        .arg(root.join("Cargo.toml"))
        .output()
        .map_err(|e| format!("cargo metadata did not start: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata output: {e}"))
}

fn packages(metadata: &Value, root: &Path) -> Result<Vec<Package>, String> {
    let raw = metadata["packages"]
        .as_array()
        .ok_or("cargo metadata has no packages")?;
    let names: HashSet<&str> = raw.iter().filter_map(|p| p["name"].as_str()).collect();
    let mut out = Vec::new();
    for p in raw {
        let name = p["name"].as_str().unwrap_or_default().to_string();
        let manifest = PathBuf::from(p["manifest_path"].as_str().unwrap_or_default());
        let dir = source::rel_of(root, manifest.parent().unwrap_or(root));
        let mut targets: Vec<Target> = p["targets"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|t| Target {
                name: t["name"].as_str().unwrap_or_default().to_string(),
                kind: TargetKind::from_metadata(
                    t["kind"].as_array().map(Vec::as_slice).unwrap_or(&[]),
                ),
                src: source::rel_of(root, Path::new(t["src_path"].as_str().unwrap_or_default())),
            })
            .collect();
        targets.sort_by_key(|t| t.kind);
        let lib_name = targets
            .iter()
            .find(|t| t.kind == TargetKind::Lib)
            .map(|t| t.name.clone());
        let deps = p["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|d| {
                let dep_name = d["name"].as_str().unwrap_or_default().to_string();
                let ident = d["rename"].as_str().unwrap_or(&dep_name).replace('-', "_");
                Dep {
                    first_party: names.contains(dep_name.as_str()),
                    kind: match d["kind"].as_str() {
                        Some("dev") => DepKind::Dev,
                        Some("build") => DepKind::Build,
                        _ => DepKind::Normal,
                    },
                    target: d["target"].as_str().map(str::to_string),
                    optional: d["optional"].as_bool().unwrap_or(false),
                    features: strings(&d["features"]),
                    ident,
                    name: dep_name,
                }
            })
            .collect();
        let features = p["features"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(k, v)| (k.clone(), strings(v)))
            .collect();
        out.push(Package {
            name,
            dir,
            lib_name,
            targets,
            deps,
            features,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

/// Every target's module tree, one level at a time, each level split over
/// the machine's threads. A file reached from two targets is kept once, under
/// the first: production targets sort first.
fn walk(root: &Path, packages: &[Package]) -> Result<Vec<SourceFile>, String> {
    let mut frontier: Vec<Job> = Vec::new();
    for kind_pass in [true, false] {
        for p in packages {
            for t in p
                .targets
                .iter()
                .filter(|t| t.kind.is_production() == kind_pass)
            {
                frontier.push(Job {
                    abs: root.join(&t.src),
                    rel: t.src.clone(),
                    package: p.name.clone(),
                    target: t.kind,
                    module: Vec::new(),
                    test_only: false,
                    mod_rs: true,
                    declared_by: None,
                });
            }
        }
    }
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut seen: HashSet<String> = HashSet::new();
    let mut files = Vec::new();
    let mut errors = Vec::new();
    while !frontier.is_empty() {
        frontier.retain(|job| seen.insert(job.rel.clone()));
        let chunk = frontier.len().div_ceil(threads).max(1);
        let results: Vec<Result<source::Visited, String>> = std::thread::scope(|s| {
            let handles: Vec<_> = frontier
                .chunks(chunk)
                .map(|jobs| {
                    s.spawn(move || {
                        jobs.iter()
                            .map(|j| source::visit(root, j))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| {
                    h.join()
                        .unwrap_or_else(|_| vec![Err("a parsing thread panicked".to_string())])
                })
                .collect()
        });
        let mut next = Vec::new();
        for result in results {
            match result {
                Ok(visited) => {
                    files.push(visited.file);
                    next.extend(visited.children);
                }
                Err(e) => errors.push(e),
            }
        }
        frontier = next;
    }
    if !errors.is_empty() {
        errors.sort();
        return Err(errors.join("\n"));
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    static KEYS: AtomicUsize = AtomicUsize::new(0);
    let base = KEYS.fetch_add(files.len(), Ordering::Relaxed);
    Ok(files
        .into_iter()
        .enumerate()
        .map(|(i, f)| f.with_cache_key(base + i))
        .collect())
}
