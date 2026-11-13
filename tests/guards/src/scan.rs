//! Syntax-tree scanners over one [`SourceFile`].
//!
//! syn does not expand macros. A macro whose body parses as comma-separated
//! expressions (`vec!`, `format!`, `json!`, `assert!`) is scanned as those
//! expressions; any other body falls back to tokens, where `Ident (:: Ident)+`
//! is a path, a path or `.ident` followed by `( .. )` is a call, and a string
//! literal is a string. The fallback over-reports, which for an allow-list is
//! the safe direction.
//!
//! A path keeps its segments as written, with a leading `::` kept as a first
//! segment `"::"`. One level of `use` aliasing is undone: after
//! `use std::process::Command;`, `Command::new` is reported as
//! `std::process::Command::new`.

use std::collections::HashMap;

use proc_macro2::{Delimiter, Span, TokenStream, TokenTree};
use quote::ToTokens;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{Expr, Token};

use crate::source::{SourceFile, View};
use crate::workspace::{TargetKind, Workspace};
use crate::Hit;

#[derive(Clone, Debug)]
pub enum Callee {
    Path(Vec<String>),
    Method(String),
}

#[derive(Clone, Debug)]
pub struct Call {
    pub callee: Callee,
    pub args: Vec<Expr>,
    pub hit: Hit,
}

#[derive(Clone, Debug)]
pub struct Static {
    pub name: String,
    pub ty_tokens: String,
    pub is_mut: bool,
    pub thread_local: bool,
    /// The functions, types and inline modules around it, `::`-joined; empty
    /// at a file's top level.
    pub enclosing: String,
    pub hit: Hit,
}

#[derive(Clone, Debug)]
pub struct FnSpan {
    pub qual: String,
    /// From the line of `fn` to the line of the closing brace.
    pub lines: usize,
    pub hit: Hit,
}

/// A `.name` field access.
#[derive(Clone, Debug)]
pub struct FieldAccess {
    pub name: String,
    /// The left side of an assignment.
    pub write: bool,
    pub hit: Hit,
}

#[derive(Clone, Debug)]
pub struct FieldInfo {
    pub name: Option<String>,
    pub ty: String,
    pub vis: String,
}

#[derive(Clone, Debug)]
pub struct StructInfo {
    pub name: String,
    pub vis: String,
    pub attrs: Vec<String>,
    pub fields: Vec<FieldInfo>,
    pub hit: Hit,
}

#[derive(Clone, Debug)]
pub struct EnumInfo {
    pub name: String,
    pub vis: String,
    pub attrs: Vec<String>,
    pub variants: Vec<String>,
    pub hit: Hit,
}

#[derive(Clone, Debug)]
pub struct TraitInfo {
    pub name: String,
    pub vis: String,
    pub attrs: Vec<String>,
    pub supertraits: Vec<String>,
    pub items: Vec<String>,
    pub hit: Hit,
}

#[derive(Clone, Debug)]
pub struct ImplInfo {
    pub trait_path: Option<Vec<String>>,
    pub self_ty: String,
    pub attrs: Vec<String>,
    pub items: Vec<String>,
    pub hit: Hit,
}

pub fn paths(f: &SourceFile, v: View) -> Vec<(Vec<String>, Hit)> {
    collect(f, v, Want::PATHS)
        .paths
        .into_iter()
        .map(|p| (p.segments, p.hit))
        .collect()
}

/// [`paths`] that can name a crate: a lone identifier counts only as a `use`,
/// an `extern crate` or a macro, since otherwise it is as often a local binding.
pub fn crate_paths(f: &SourceFile, v: View) -> Vec<(Vec<String>, Hit)> {
    collect(f, v, Want::PATHS)
        .paths
        .into_iter()
        .filter(|p| !p.bare)
        .map(|p| (p.segments, p.hit))
        .collect()
}

pub fn expr_paths(f: &SourceFile, v: View) -> Vec<(Vec<String>, Hit)> {
    collect(f, v, Want::EXPR_PATHS).expr_paths
}

/// Every named field access in `f`. In a macro body syn could not parse, a
/// `.ident` not followed by `( .. )` counts as a read.
pub fn field_accesses(f: &SourceFile, v: View) -> Vec<FieldAccess> {
    collect(f, v, Want::FIELDS).fields
}

pub fn calls(f: &SourceFile, v: View) -> Vec<Call> {
    collect(f, v, Want::CALLS).calls
}

/// Invocations of macros whose last path segment is `name`.
pub fn macro_calls(f: &SourceFile, v: View, name: &str) -> Vec<Hit> {
    collect(f, v, Want::MACROS)
        .macros
        .into_iter()
        .filter(|(n, _)| n == name)
        .map(|(_, hit)| hit)
        .collect()
}

pub fn string_literals(f: &SourceFile, v: View) -> Vec<(String, Hit)> {
    collect(f, v, Want::STRINGS).strings
}

pub fn statics(f: &SourceFile, v: View) -> Vec<Static> {
    collect(f, v, Want::ITEMS).statics
}

pub fn fns(f: &SourceFile, v: View) -> Vec<FnSpan> {
    collect(f, v, Want::ITEMS).fns
}

pub fn structs(f: &SourceFile, v: View) -> Vec<StructInfo> {
    collect(f, v, Want::ITEMS).structs
}

pub fn enums(f: &SourceFile, v: View) -> Vec<EnumInfo> {
    collect(f, v, Want::ITEMS).enums
}

pub fn traits(f: &SourceFile, v: View) -> Vec<TraitInfo> {
    collect(f, v, Want::ITEMS).traits
}

pub fn impls(f: &SourceFile, v: View) -> Vec<ImplInfo> {
    collect(f, v, Want::ITEMS).impls
}

/// Every `feature = "<feature>"` in a `#[cfg]`, a `#[cfg_attr]` or a `cfg!()`.
pub fn cfg_feature_uses(f: &SourceFile, v: View, feature: &str) -> Vec<Hit> {
    collect(f, v, Want::FEATURES)
        .features
        .into_iter()
        .filter(|(name, _)| name == feature)
        .map(|(_, hit)| hit)
        .collect()
}

/// What a trait method's body is, when it is nothing but one string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Returned {
    Literal(String),
    /// A lone identifier, to be looked up among [`str_consts`].
    Const(String),
}

/// What each `fn <fn_name>` returns in an `impl <trait_name> for` block whose
/// body is a string literal or a lone identifier.
pub fn trait_fn_returns(f: &SourceFile, v: View, trait_name: &str, fn_name: &str) -> Vec<Returned> {
    struct Impls<'a> {
        trait_name: &'a str,
        fn_name: &'a str,
        out: Vec<Returned>,
    }
    impl<'ast> Visit<'ast> for Impls<'_> {
        fn visit_item_impl(&mut self, imp: &'ast syn::ItemImpl) {
            let is_trait = imp
                .trait_
                .as_ref()
                .and_then(|(_, path, _)| path.segments.last())
                .is_some_and(|s| s.ident == self.trait_name);
            if is_trait {
                for item in &imp.items {
                    let syn::ImplItem::Fn(func) = item else {
                        continue;
                    };
                    if func.sig.ident != self.fn_name {
                        continue;
                    }
                    if let [syn::Stmt::Expr(body, None)] = func.block.stmts.as_slice() {
                        match body {
                            Expr::Lit(syn::ExprLit {
                                lit: syn::Lit::Str(s),
                                ..
                            }) => self.out.push(Returned::Literal(s.value())),
                            Expr::Path(p) => {
                                if let Some(id) = p.path.get_ident() {
                                    self.out.push(Returned::Const(id.to_string()));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            visit::visit_item_impl(self, imp);
        }
    }
    let mut impls = Impls {
        trait_name,
        fn_name,
        out: Vec::new(),
    };
    impls.visit_file(&f.tree(v));
    impls.out
}

/// Every `const NAME: &str = "..."` in the file, at any depth.
pub fn str_consts(f: &SourceFile, v: View) -> Vec<(String, String)> {
    #[derive(Default)]
    struct Consts(Vec<(String, String)>);
    impl<'ast> Visit<'ast> for Consts {
        fn visit_item_const(&mut self, c: &'ast syn::ItemConst) {
            if let Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(s),
                ..
            }) = &*c.expr
            {
                self.0.push((c.ident.to_string(), s.value()));
            }
        }
    }
    let mut consts = Consts::default();
    consts.visit_file(&f.tree(v));
    consts.0
}

/// The variants of the one enum called `name` in `krate`'s library.
pub fn enum_variants(ws: &Workspace, krate: &str, name: &str) -> Vec<String> {
    ws.package(krate);
    let found: Vec<EnumInfo> = ws
        .production()
        .filter(|f| f.package == krate && f.target == TargetKind::Lib)
        .flat_map(|f| enums(f, View::Prod))
        .filter(|e| e.name == name)
        .collect();
    match found.as_slice() {
        [one] => one.variants.clone(),
        [] => {
            panic!("{krate} defines no enum `{name}`: renamed, moved, or the guard names it wrong")
        }
        many => panic!(
            "{krate} defines `{name}` {} times: {:?}; the guard cannot tell which",
            many.len(),
            many.iter().map(|e| e.hit.to_string()).collect::<Vec<_>>()
        ),
    }
}

pub(crate) struct ScopedPath {
    pub segments: Vec<String>,
    /// The inline `mod` blocks around it within the file.
    pub mods: Vec<String>,
    pub hit: Hit,
    bare: bool,
}

pub(crate) fn scoped_paths(f: &SourceFile, v: View) -> Vec<ScopedPath> {
    collect(f, v, Want::PATHS).paths
}

#[derive(Clone, Copy, Default)]
struct Want {
    paths: bool,
    expr_paths: bool,
    calls: bool,
    macros: bool,
    strings: bool,
    items: bool,
    features: bool,
    fields: bool,
}

impl Want {
    const PATHS: Want = Want {
        paths: true,
        ..Want::NONE
    };
    const EXPR_PATHS: Want = Want {
        expr_paths: true,
        ..Want::NONE
    };
    const CALLS: Want = Want {
        calls: true,
        ..Want::NONE
    };
    const MACROS: Want = Want {
        macros: true,
        ..Want::NONE
    };
    const STRINGS: Want = Want {
        strings: true,
        ..Want::NONE
    };
    const ITEMS: Want = Want {
        items: true,
        ..Want::NONE
    };
    const FEATURES: Want = Want {
        features: true,
        ..Want::NONE
    };
    const FIELDS: Want = Want {
        fields: true,
        ..Want::NONE
    };
    const NONE: Want = Want {
        paths: false,
        expr_paths: false,
        calls: false,
        macros: false,
        strings: false,
        items: false,
        features: false,
        fields: false,
    };
}

#[derive(Default)]
struct Found {
    paths: Vec<ScopedPath>,
    expr_paths: Vec<(Vec<String>, Hit)>,
    calls: Vec<Call>,
    macros: Vec<(String, Hit)>,
    strings: Vec<(String, Hit)>,
    statics: Vec<Static>,
    fns: Vec<FnSpan>,
    structs: Vec<StructInfo>,
    enums: Vec<EnumInfo>,
    traits: Vec<TraitInfo>,
    impls: Vec<ImplInfo>,
    features: Vec<(String, Hit)>,
    fields: Vec<FieldAccess>,
}

fn collect(f: &SourceFile, v: View, want: Want) -> Found {
    let tree = f.tree(v);
    let mut aliases = HashMap::new();
    let mut use_collector = UseAliases(&mut aliases);
    use_collector.visit_file(&tree);
    let mut c = Collector {
        f,
        want,
        aliases,
        scope: Vec::new(),
        mods: Vec::new(),
        found: Found::default(),
    };
    c.visit_file(&tree);
    c.found
}

pub(crate) fn segments(path: &syn::Path) -> Vec<String> {
    let mut out = Vec::new();
    if path.leading_colon.is_some() {
        out.push("::".to_string());
    }
    out.extend(path.segments.iter().map(|s| s.ident.to_string()));
    out
}

/// Each leaf of a `use` tree as a full path; a glob ends in `"*"`, a rename
/// keeps the original name.
pub(crate) fn use_leaves(item: &syn::ItemUse) -> Vec<(Vec<String>, Option<String>, Span)> {
    let mut out = Vec::new();
    let mut prefix = Vec::new();
    if item.leading_colon.is_some() {
        prefix.push("::".to_string());
    }
    flatten(&item.tree, &mut prefix, &mut out);
    out
}

fn flatten(
    tree: &syn::UseTree,
    prefix: &mut Vec<String>,
    out: &mut Vec<(Vec<String>, Option<String>, Span)>,
) {
    match tree {
        syn::UseTree::Path(p) => {
            prefix.push(p.ident.to_string());
            flatten(&p.tree, prefix, out);
            prefix.pop();
        }
        syn::UseTree::Name(n) => {
            let mut full = prefix.clone();
            full.push(n.ident.to_string());
            out.push((full, None, n.ident.span()));
        }
        syn::UseTree::Rename(r) => {
            let mut full = prefix.clone();
            full.push(r.ident.to_string());
            out.push((full, Some(r.rename.to_string()), r.ident.span()));
        }
        syn::UseTree::Glob(g) => {
            let mut full = prefix.clone();
            full.push("*".to_string());
            out.push((full, None, g.star_token.span()));
        }
        syn::UseTree::Group(g) => {
            for t in &g.items {
                flatten(t, prefix, out);
            }
        }
    }
}

struct UseAliases<'a>(&'a mut HashMap<String, Vec<String>>);

impl<'ast> Visit<'ast> for UseAliases<'_> {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        for (full, rename, _) in use_leaves(item) {
            let local = match rename {
                Some(r) => r,
                None => full.last().cloned().unwrap_or_default(),
            };
            if local == "*" || local == "self" || local == "_" {
                continue;
            }
            self.0.entry(local).or_insert(full);
        }
    }
}

struct Collector<'f> {
    f: &'f SourceFile,
    want: Want,
    aliases: HashMap<String, Vec<String>>,
    scope: Vec<String>,
    mods: Vec<String>,
    found: Found,
}

impl Collector<'_> {
    fn expand(&self, segs: Vec<String>) -> Vec<String> {
        let Some(first) = segs.first() else {
            return segs;
        };
        match self.aliases.get(first) {
            Some(full) if !matches!(first.as_str(), "crate" | "self" | "super" | "Self" | "::") => {
                let mut out = full.clone();
                out.extend(segs.into_iter().skip(1));
                out
            }
            _ => segs,
        }
    }

    fn push_path(&mut self, segs: Vec<String>, span: Span, from_use: bool) {
        self.record_path(segs, span, from_use, from_use);
    }

    fn record_path(&mut self, segs: Vec<String>, span: Span, from_use: bool, names_item: bool) {
        let segments = if from_use { segs } else { self.expand(segs) };
        self.found.paths.push(ScopedPath {
            bare: !names_item && segments.len() == 1,
            segments,
            mods: self.mods.clone(),
            hit: self.f.hit(span),
        });
    }

    fn attrs(&self, attrs: &[syn::Attribute]) -> Vec<String> {
        attrs
            .iter()
            .filter(|a| !a.path().is_ident("doc"))
            .map(|a| a.to_token_stream().to_string())
            .collect()
    }

    fn enclosing(&self) -> String {
        self.scope.join("::")
    }

    fn qual(&self, name: &str) -> String {
        let mut q = self.scope.clone();
        q.push(name.to_string());
        q.join("::")
    }

    fn fn_span(&mut self, name: &str, fn_token: Span, block: &syn::Block) {
        if !self.want.items {
            return;
        }
        let start = fn_token.start().line;
        let end = block.brace_token.span.close().end().line;
        self.found.fns.push(FnSpan {
            qual: self.qual(name),
            lines: end.saturating_sub(start) + 1,
            hit: self.f.hit(fn_token),
        });
    }

    fn feature_tokens(&mut self, tokens: TokenStream) {
        let trees: Vec<TokenTree> = tokens.into_iter().collect();
        for (i, tt) in trees.iter().enumerate() {
            match tt {
                TokenTree::Group(g) => self.feature_tokens(g.stream()),
                TokenTree::Ident(id) if id == "feature" => {
                    let eq =
                        matches!(trees.get(i + 1), Some(TokenTree::Punct(p)) if p.as_char() == '=');
                    if let (true, Some(TokenTree::Literal(lit))) = (eq, trees.get(i + 2)) {
                        if let Some(value) = literal_str(lit) {
                            let hit = self.f.hit(id.span());
                            self.found.features.push((value, hit));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Paths, calls and strings in a token stream syn could not parse.
    fn fallback(&mut self, tokens: TokenStream) {
        let trees: Vec<TokenTree> = tokens.into_iter().collect();
        let mut i = 0;
        while i < trees.len() {
            match &trees[i] {
                TokenTree::Group(g) => {
                    self.fallback(g.stream());
                    i += 1;
                }
                TokenTree::Literal(lit) => {
                    if self.want.strings {
                        if let Some(value) = literal_str(lit) {
                            let hit = self.f.hit(lit.span());
                            self.found.strings.push((value, hit));
                        }
                    }
                    i += 1;
                }
                TokenTree::Punct(p) if p.as_char() == '.' => {
                    if let Some(TokenTree::Ident(id)) = trees.get(i + 1) {
                        let called = matches!(
                            trees.get(i + 2),
                            Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis
                        );
                        if self.want.fields && !called {
                            self.found.fields.push(FieldAccess {
                                name: id.to_string(),
                                write: false,
                                hit: self.f.hit(id.span()),
                            });
                        }
                    }
                    if let (Some(TokenTree::Ident(id)), Some(TokenTree::Group(g))) =
                        (trees.get(i + 1), trees.get(i + 2))
                    {
                        if g.delimiter() == Delimiter::Parenthesis && self.want.calls {
                            let hit = self.f.hit(id.span());
                            self.found.calls.push(Call {
                                callee: Callee::Method(id.to_string()),
                                args: parse_args(g.stream()),
                                hit,
                            });
                        }
                    }
                    i += 1;
                }
                _ => {
                    let (segs, span, next) = token_path(&trees, i);
                    if segs.len() >= 2 {
                        let expanded = self.expand(segs.clone());
                        if self.want.paths {
                            self.push_path(segs, span, false);
                        }
                        if self.want.expr_paths {
                            let hit = self.f.hit(span);
                            self.found.expr_paths.push((expanded.clone(), hit));
                        }
                        if self.want.calls {
                            if let Some(TokenTree::Group(g)) = trees.get(next) {
                                if g.delimiter() == Delimiter::Parenthesis {
                                    let hit = self.f.hit(span);
                                    self.found.calls.push(Call {
                                        callee: Callee::Path(expanded),
                                        args: parse_args(g.stream()),
                                        hit,
                                    });
                                }
                            }
                        }
                        i = next;
                    } else {
                        i += 1;
                    }
                }
            }
        }
    }

    fn field(&mut self, e: &syn::ExprField, write: bool) {
        if let (true, syn::Member::Named(name)) = (self.want.fields, &e.member) {
            self.found.fields.push(FieldAccess {
                name: name.to_string(),
                write,
                hit: self.f.hit(name.span()),
            });
        }
    }

    fn thread_local(&mut self, mac: &syn::Macro) -> bool {
        let Ok(decls) = mac.parse_body_with(parse_thread_locals) else {
            return false;
        };
        for decl in decls {
            if self.want.items {
                self.found.statics.push(Static {
                    name: decl.name.to_string(),
                    ty_tokens: decl.ty.to_token_stream().to_string(),
                    is_mut: false,
                    thread_local: true,
                    enclosing: self.enclosing(),
                    hit: self.f.hit(decl.name.span()),
                });
            }
            self.visit_type(&decl.ty);
            match decl.init {
                Some(e) => self.visit_expr(&e),
                None => self.fallback(decl.rest),
            }
        }
        true
    }
}

fn literal_str(lit: &proc_macro2::Literal) -> Option<String> {
    let tokens: TokenStream = TokenTree::Literal(lit.clone()).into();
    syn::parse2::<syn::LitStr>(tokens).ok().map(|s| s.value())
}

fn parse_args(tokens: TokenStream) -> Vec<Expr> {
    syn::parse::Parser::parse2(Punctuated::<Expr, Token![,]>::parse_terminated, tokens)
        .map(|p| p.into_iter().collect())
        .unwrap_or_default()
}

/// The longest `[::] Ident (:: Ident)*` starting at `i`, its first span and
/// the index after it.
fn token_path(trees: &[TokenTree], i: usize) -> (Vec<String>, Span, usize) {
    let mut segs = Vec::new();
    let mut j = i;
    let mut span = trees[i].span();
    if is_colon2(trees, j) {
        segs.push("::".to_string());
        j += 2;
    }
    let Some(TokenTree::Ident(first)) = trees.get(j) else {
        return (Vec::new(), span, i + 1);
    };
    if segs.is_empty() {
        span = first.span();
    }
    segs.push(first.to_string());
    j += 1;
    while is_colon2(trees, j) {
        match trees.get(j + 2) {
            Some(TokenTree::Ident(id)) => {
                segs.push(id.to_string());
                j += 3;
            }
            _ => break,
        }
    }
    (segs, span, j)
}

fn is_colon2(trees: &[TokenTree], j: usize) -> bool {
    matches!(
        (trees.get(j), trees.get(j + 1)),
        (Some(TokenTree::Punct(a)), Some(TokenTree::Punct(b)))
            if a.as_char() == ':' && b.as_char() == ':' && a.spacing() == proc_macro2::Spacing::Joint
    )
}

struct ThreadLocal {
    name: syn::Ident,
    ty: syn::Type,
    init: Option<Expr>,
    rest: TokenStream,
}

fn parse_thread_locals(input: syn::parse::ParseStream) -> syn::Result<Vec<ThreadLocal>> {
    let mut out = Vec::new();
    while !input.is_empty() {
        input.call(syn::Attribute::parse_outer)?;
        input.parse::<syn::Visibility>()?;
        input.parse::<Token![static]>()?;
        let name: syn::Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let ty: syn::Type = input.parse()?;
        input.parse::<Token![=]>()?;
        let mut rest = TokenStream::new();
        while !input.is_empty() && !input.peek(Token![;]) {
            let tt: TokenTree = input.parse()?;
            rest.extend([tt]);
        }
        if !input.is_empty() {
            input.parse::<Token![;]>()?;
        }
        let init = syn::parse2::<Expr>(rest.clone()).ok();
        out.push(ThreadLocal {
            name,
            ty,
            init,
            rest,
        });
    }
    Ok(out)
}

fn vis(v: &syn::Visibility) -> String {
    v.to_token_stream().to_string()
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        other => other.to_token_stream().to_string(),
    }
}

impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        if self.want.paths {
            self.push_path(segments(path), path.span(), false);
        }
        visit::visit_path(self, path);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        if self.want.paths {
            for (full, _, span) in use_leaves(item) {
                self.push_path(full, span, true);
            }
        }
        for attr in &item.attrs {
            self.visit_attribute(attr);
        }
    }

    fn visit_expr_path(&mut self, e: &'ast syn::ExprPath) {
        if self.want.expr_paths {
            let segs = self.expand(segments(&e.path));
            let hit = self.f.hit(e.path.span());
            self.found.expr_paths.push((segs, hit));
        }
        visit::visit_expr_path(self, e);
    }

    fn visit_expr_field(&mut self, e: &'ast syn::ExprField) {
        self.field(e, false);
        visit::visit_expr_field(self, e);
    }

    fn visit_expr_assign(&mut self, e: &'ast syn::ExprAssign) {
        match &*e.left {
            Expr::Field(field) => {
                self.field(field, true);
                visit::visit_expr(self, &field.base);
            }
            left => visit::visit_expr(self, left),
        }
        visit::visit_expr(self, &e.right);
    }

    fn visit_expr_struct(&mut self, e: &'ast syn::ExprStruct) {
        if self.want.expr_paths {
            let segs = self.expand(segments(&e.path));
            let hit = self.f.hit(e.path.span());
            self.found.expr_paths.push((segs, hit));
        }
        visit::visit_expr_struct(self, e);
    }

    fn visit_expr_call(&mut self, e: &'ast syn::ExprCall) {
        if self.want.calls {
            if let Expr::Path(p) = &*e.func {
                let callee = Callee::Path(self.expand(segments(&p.path)));
                let hit = self.f.hit(p.path.span());
                self.found.calls.push(Call {
                    callee,
                    args: e.args.iter().cloned().collect(),
                    hit,
                });
            }
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_expr_method_call(&mut self, e: &'ast syn::ExprMethodCall) {
        if self.want.calls {
            let hit = self.f.hit(e.method.span());
            self.found.calls.push(Call {
                callee: Callee::Method(e.method.to_string()),
                args: e.args.iter().cloned().collect(),
                hit,
            });
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_lit_str(&mut self, s: &'ast syn::LitStr) {
        if self.want.strings {
            let hit = self.f.hit(s.span());
            self.found.strings.push((s.value(), hit));
        }
    }

    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if attr.path().is_ident("doc") {
            return;
        }
        if self.want.features && (attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr")) {
            if let syn::Meta::List(list) = &attr.meta {
                self.feature_tokens(list.tokens.clone());
            }
        }
        if let syn::Meta::List(list) = &attr.meta {
            if self.want.paths {
                self.push_path(segments(&list.path), list.path.span(), false);
            }
            self.fallback(list.tokens.clone());
            return;
        }
        visit::visit_attribute(self, attr);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let name = mac
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default();
        if self.want.macros {
            let hit = self.f.hit(mac.path.span());
            self.found.macros.push((name.clone(), hit));
        }
        if self.want.paths {
            self.record_path(segments(&mac.path), mac.path.span(), false, true);
        }
        visit::visit_path(self, &mac.path);
        if self.want.features && name == "cfg" {
            self.feature_tokens(mac.tokens.clone());
        }
        if name == "thread_local" && self.thread_local(mac) {
            return;
        }
        match mac.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated) {
            Ok(exprs) => {
                for e in &exprs {
                    self.visit_expr(e);
                }
            }
            Err(_) => self.fallback(mac.tokens.clone()),
        }
    }

    fn visit_item_extern_crate(&mut self, item: &'ast syn::ItemExternCrate) {
        if self.want.paths {
            self.push_path(vec![item.ident.to_string()], item.ident.span(), true);
        }
        visit::visit_item_extern_crate(self, item);
    }

    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        let inline = m.content.is_some();
        if inline {
            self.scope.push(m.ident.to_string());
            self.mods.push(m.ident.to_string());
        }
        visit::visit_item_mod(self, m);
        if inline {
            self.scope.pop();
            self.mods.pop();
        }
    }

    fn visit_item_static(&mut self, s: &'ast syn::ItemStatic) {
        if self.want.items {
            self.found.statics.push(Static {
                name: s.ident.to_string(),
                ty_tokens: s.ty.to_token_stream().to_string(),
                is_mut: matches!(s.mutability, syn::StaticMutability::Mut(_)),
                thread_local: false,
                enclosing: self.enclosing(),
                hit: self.f.hit(s.ident.span()),
            });
        }
        visit::visit_item_static(self, s);
    }

    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let name = item.sig.ident.to_string();
        self.fn_span(&name, item.sig.fn_token.span, &item.block);
        self.scope.push(name);
        visit::visit_item_fn(self, item);
        self.scope.pop();
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        let name = item.sig.ident.to_string();
        self.fn_span(&name, item.sig.fn_token.span, &item.block);
        self.scope.push(name);
        visit::visit_impl_item_fn(self, item);
        self.scope.pop();
    }

    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        let name = item.sig.ident.to_string();
        if let Some(block) = &item.default {
            self.fn_span(&name, item.sig.fn_token.span, block);
        }
        self.scope.push(name);
        visit::visit_trait_item_fn(self, item);
        self.scope.pop();
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let self_ty = type_name(&item.self_ty);
        if self.want.items {
            let items = item
                .items
                .iter()
                .filter_map(|i| match i {
                    syn::ImplItem::Fn(f) => Some(f.sig.ident.to_string()),
                    syn::ImplItem::Const(c) => Some(c.ident.to_string()),
                    syn::ImplItem::Type(t) => Some(t.ident.to_string()),
                    _ => None,
                })
                .collect();
            self.found.impls.push(ImplInfo {
                trait_path: item.trait_.as_ref().map(|(_, p, _)| segments(p)),
                self_ty: self_ty.clone(),
                attrs: self.attrs(&item.attrs),
                items,
                hit: self.f.hit(item.impl_token.span),
            });
        }
        self.scope.push(self_ty);
        visit::visit_item_impl(self, item);
        self.scope.pop();
    }

    fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
        if self.want.items {
            let items = item
                .items
                .iter()
                .filter_map(|i| match i {
                    syn::TraitItem::Fn(f) => Some(f.sig.ident.to_string()),
                    syn::TraitItem::Const(c) => Some(c.ident.to_string()),
                    syn::TraitItem::Type(t) => Some(t.ident.to_string()),
                    _ => None,
                })
                .collect();
            self.found.traits.push(TraitInfo {
                name: item.ident.to_string(),
                vis: vis(&item.vis),
                attrs: self.attrs(&item.attrs),
                supertraits: item
                    .supertraits
                    .iter()
                    .map(|b| b.to_token_stream().to_string())
                    .collect(),
                items,
                hit: self.f.hit(item.ident.span()),
            });
        }
        self.scope.push(item.ident.to_string());
        visit::visit_item_trait(self, item);
        self.scope.pop();
    }

    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        if self.want.items {
            let fields = item
                .fields
                .iter()
                .map(|f| FieldInfo {
                    name: f.ident.as_ref().map(ToString::to_string),
                    ty: f.ty.to_token_stream().to_string(),
                    vis: vis(&f.vis),
                })
                .collect();
            self.found.structs.push(StructInfo {
                name: item.ident.to_string(),
                vis: vis(&item.vis),
                attrs: self.attrs(&item.attrs),
                fields,
                hit: self.f.hit(item.ident.span()),
            });
        }
        visit::visit_item_struct(self, item);
    }

    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        if self.want.items {
            self.found.enums.push(EnumInfo {
                name: item.ident.to_string(),
                vis: vis(&item.vis),
                attrs: self.attrs(&item.attrs),
                variants: item.variants.iter().map(|v| v.ident.to_string()).collect(),
                hit: self.f.hit(item.ident.span()),
            });
        }
        visit::visit_item_enum(self, item);
    }
}
