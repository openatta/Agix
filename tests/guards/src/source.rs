//! One source file of a target's module tree, and the walk that finds them.
//!
//! The walk starts at a target's root file and follows `mod x;` the way rustc
//! does: `#[path]`, `x.rs`, `x/mod.rs`, inline `mod a { mod b; }` adding a
//! directory, and a non-`mod.rs` file owning the directory named after it. A
//! declaration whose file is missing, or a file syn cannot parse, is an error
//! naming the file.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use proc_macro2::Span;

use crate::workspace::TargetKind;
use crate::Hit;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum View {
    /// The file with its test code pruned.
    Prod,
    Full,
}

#[derive(Debug)]
pub struct SourceFile {
    pub rel: String,
    pub package: String,
    pub target: TargetKind,
    pub module: Vec<String>,
    pub test_only: bool,
    pub text: String,
    pub test_lines: Vec<RangeInclusive<usize>>,
    cache_key: Option<usize>,
}

thread_local! {
    static TREES: RefCell<HashMap<(usize, View), Rc<syn::File>>> = RefCell::new(HashMap::new());
}

impl SourceFile {
    pub fn production_lines(&self) -> usize {
        if self.test_only || !self.target.is_production() {
            return 0;
        }
        let total = self.text.lines().count();
        let taken: usize = self
            .test_lines
            .iter()
            .map(|r| r.end().min(&total).saturating_sub(*r.start()) + 1)
            .sum();
        total.saturating_sub(taken)
    }

    /// Line `n`, counted from 1; empty past the end.
    pub fn line(&self, n: usize) -> &str {
        n.checked_sub(1)
            .and_then(|i| self.text.lines().nth(i))
            .unwrap_or("")
    }

    pub fn hit(&self, span: Span) -> Hit {
        let line = span.start().line;
        Hit {
            file: self.rel.clone(),
            line,
            snippet: self.line(line).trim().to_string(),
        }
    }

    pub fn is_test_line(&self, line: usize) -> bool {
        self.test_only || self.test_lines.iter().any(|r| r.contains(&line))
    }

    /// The parsed tree.
    ///
    /// proc-macro2 keeps span locations in a thread-local source map and its
    /// types are neither `Send` nor `Sync`, so a tree is only meaningful on the
    /// thread that parsed it. Each thread parses a file once and keeps it.
    pub fn tree(&self, view: View) -> Rc<syn::File> {
        let Some(key) = self.cache_key else {
            return Rc::new(self.parse(view));
        };
        if let Some(tree) = TREES.with(|t| t.borrow().get(&(key, view)).cloned()) {
            return tree;
        }
        let tree = Rc::new(self.parse(view));
        TREES.with(|t| t.borrow_mut().insert((key, view), tree.clone()));
        tree
    }

    fn parse(&self, view: View) -> syn::File {
        let mut file = syn::parse_file(&self.text)
            .unwrap_or_else(|e| panic!("{}: syn could not parse it: {e}", self.rel));
        if view == View::Prod {
            crate::cfg::prune(&mut file);
        }
        file
    }

    pub(crate) fn in_workspace(&self) -> bool {
        self.cache_key.is_some()
    }

    pub(crate) fn with_cache_key(mut self, key: usize) -> Self {
        self.cache_key = Some(key);
        self
    }
}

/// A file outside any target, read as a production file of a library: what
/// the self-tests use for their fixtures.
pub fn standalone(rel: &str, text: String) -> Result<SourceFile, String> {
    let facts = examine(rel, &text)?;
    Ok(SourceFile {
        rel: rel.to_string(),
        package: String::new(),
        target: TargetKind::Lib,
        module: Vec::new(),
        test_only: false,
        text,
        test_lines: facts.test_lines,
        cache_key: None,
    })
}

#[derive(Clone, Debug)]
pub(crate) struct Job {
    pub abs: PathBuf,
    pub rel: String,
    pub package: String,
    pub target: TargetKind,
    pub module: Vec<String>,
    pub test_only: bool,
    pub mod_rs: bool,
    pub declared_by: Option<String>,
}

pub(crate) struct Visited {
    pub file: SourceFile,
    pub children: Vec<Job>,
}

pub(crate) fn rel_of(root: &Path, abs: &Path) -> String {
    abs.strip_prefix(root)
        .unwrap_or(abs)
        .to_string_lossy()
        .replace('\\', "/")
}

pub(crate) fn visit(root: &Path, job: &Job) -> Result<Visited, String> {
    let text = std::fs::read_to_string(&job.abs).map_err(|e| match &job.declared_by {
        Some(by) => format!(
            "{by}: the file it declares, {}, cannot be read: {e}",
            job.rel
        ),
        None => format!("{}: cannot be read: {e}", job.rel),
    })?;
    let facts = examine(&job.rel, &text)?;
    let mut children = Vec::new();
    for decl in facts.mods {
        let (abs, mod_rs) =
            resolve(job, &decl).map_err(|e| format!("{}:{}: {e}", job.rel, decl.line))?;
        let mut module = job.module.clone();
        module.extend(decl.inline.iter().cloned());
        module.push(decl.name.clone());
        children.push(Job {
            rel: rel_of(root, &abs),
            abs,
            package: job.package.clone(),
            target: job.target,
            module,
            test_only: job.test_only || decl.test,
            mod_rs,
            declared_by: Some(format!("{}:{}", job.rel, decl.line)),
        });
    }
    Ok(Visited {
        file: SourceFile {
            rel: job.rel.clone(),
            package: job.package.clone(),
            target: job.target,
            module: job.module.clone(),
            test_only: job.test_only,
            text,
            test_lines: facts.test_lines,
            cache_key: None,
        },
        children,
    })
}

/// Walks one crate from its root file without `cargo metadata`; for fixtures.
pub fn load_crate(root: &Path, root_file: &Path, package: &str) -> Result<Vec<SourceFile>, String> {
    let mut queue = vec![Job {
        rel: rel_of(root, root_file),
        abs: root_file.to_path_buf(),
        package: package.to_string(),
        target: TargetKind::Lib,
        module: Vec::new(),
        test_only: false,
        mod_rs: true,
        declared_by: None,
    }];
    let mut out = Vec::new();
    while let Some(job) = queue.pop() {
        let visited = visit(root, &job)?;
        out.push(visited.file);
        queue.extend(visited.children);
    }
    out.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok(out)
}

struct ModDecl {
    name: String,
    inline: Vec<String>,
    path_attr: Option<String>,
    test: bool,
    line: usize,
}

struct Facts {
    mods: Vec<ModDecl>,
    test_lines: Vec<RangeInclusive<usize>>,
}

fn examine(rel: &str, text: &str) -> Result<Facts, String> {
    let mut file = syn::parse_file(text).map_err(|e| {
        let at = e.span().start();
        format!(
            "{rel}:{}:{}: syn could not parse it: {e}",
            at.line,
            at.column + 1
        )
    })?;
    let mut mods = Vec::new();
    collect_mods(&file.items, &mut Vec::new(), false, &mut mods);
    let test_lines = crate::cfg::prune(&mut file);
    Ok(Facts { mods, test_lines })
}

fn collect_mods(items: &[syn::Item], inline: &mut Vec<String>, test: bool, out: &mut Vec<ModDecl>) {
    for item in items {
        let syn::Item::Mod(m) = item else { continue };
        let is_test = test || crate::cfg::implies_test(&m.attrs);
        match &m.content {
            None => out.push(ModDecl {
                name: m.ident.to_string(),
                inline: inline.clone(),
                path_attr: path_attr(&m.attrs),
                test: is_test,
                line: m.ident.span().start().line,
            }),
            Some((_, children)) => {
                inline.push(m.ident.to_string());
                collect_mods(children, inline, is_test, out);
                inline.pop();
            }
        }
    }
}

fn path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find_map(|attr| {
        if !attr.path().is_ident("path") {
            return None;
        }
        let syn::Meta::NameValue(nv) = &attr.meta else {
            return None;
        };
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(s),
            ..
        }) = &nv.value
        else {
            return None;
        };
        Some(s.value())
    })
}

fn resolve(job: &Job, decl: &ModDecl) -> Result<(PathBuf, bool), String> {
    let dir = job.abs.parent().unwrap_or(Path::new("")).to_path_buf();
    let owned = if job.mod_rs {
        dir.clone()
    } else {
        let stem = job.abs.file_stem().unwrap_or_default();
        dir.join(stem)
    };
    if let Some(path) = &decl.path_attr {
        let base = if decl.inline.is_empty() {
            dir
        } else {
            decl.inline.iter().fold(owned, |d, m| d.join(m))
        };
        let abs = base.join(path);
        if !abs.is_file() {
            return Err(format!(
                "`#[path = \"{path}\"] mod {};` names {}, which does not exist",
                decl.name,
                abs.display()
            ));
        }
        return Ok((abs, true));
    }
    let base = decl.inline.iter().fold(owned, |d, m| d.join(m));
    let flat = base.join(format!("{}.rs", decl.name));
    let nested = base.join(&decl.name).join("mod.rs");
    if flat.is_file() {
        Ok((flat, false))
    } else if nested.is_file() {
        Ok((nested, true))
    } else {
        Err(format!(
            "`mod {};` has no file: neither {} nor {} exists",
            decl.name,
            flat.display(),
            nested.display()
        ))
    }
}
