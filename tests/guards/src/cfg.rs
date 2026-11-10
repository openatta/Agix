//! Which code is test code, decided on the syntax tree.
//!
//! An item, `mod` declaration, `impl` member, statement, field, variant or
//! match arm is test code when one of its `#[cfg(..)]` predicates can only hold
//! under `test` (`test`, `all(.., test, ..)`, `any` whose every arm implies
//! `test`), or when it is a `#[test]`-style function. `not(test)` and
//! `any(test, x)` are production. [`prune`] removes test code from a tree and
//! reports the lines it took.

use std::ops::RangeInclusive;

use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit_mut::{self, VisitMut};
use syn::{Attribute, Expr, Meta, Token};

pub fn implies_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let path = attr.path();
        if path.is_ident("test") {
            return true;
        }
        if path.segments.len() > 1 && path.segments.last().is_some_and(|s| s.ident == "test") {
            return true;
        }
        if !path.is_ident("cfg") {
            return false;
        }
        attr.parse_args::<Meta>()
            .map(|meta| predicate_implies_test(&meta))
            .unwrap_or(false)
    })
}

pub fn predicate_implies_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::NameValue(_) => false,
        Meta::List(list) => {
            let Ok(inner) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                return false;
            };
            if list.path.is_ident("all") {
                inner.iter().any(predicate_implies_test)
            } else if list.path.is_ident("any") {
                !inner.is_empty() && inner.iter().all(predicate_implies_test)
            } else {
                false
            }
        }
    }
}

/// Removes test code from `file`, returning the 1-based line ranges it covered,
/// sorted and merged.
pub fn prune(file: &mut syn::File) -> Vec<RangeInclusive<usize>> {
    let mut pruner = Pruner { taken: Vec::new() };
    pruner.visit_file_mut(file);
    merge(pruner.taken)
}

fn merge(mut ranges: Vec<RangeInclusive<usize>>) -> Vec<RangeInclusive<usize>> {
    ranges.sort_by_key(|r| *r.start());
    let mut out: Vec<RangeInclusive<usize>> = Vec::new();
    for range in ranges {
        match out.last_mut() {
            Some(last) if *range.start() <= *last.end() + 1 => {
                if range.end() > last.end() {
                    *last = *last.start()..=*range.end();
                }
            }
            _ => out.push(range),
        }
    }
    out
}

struct Pruner {
    taken: Vec<RangeInclusive<usize>>,
}

impl Pruner {
    fn keep<T: Spanned>(&mut self, attrs: &[Attribute], node: &T) -> bool {
        if !implies_test(attrs) {
            return true;
        }
        let span = node.span();
        self.taken
            .push(span.start().line..=span.end().line.max(span.start().line));
        false
    }

    fn retain<T: Spanned + Clone, P: Clone + Default>(
        &mut self,
        list: &mut Punctuated<T, P>,
        attrs: impl Fn(&T) -> &[Attribute],
    ) {
        if !list.iter().any(|t| implies_test(attrs(t))) {
            return;
        }
        let kept: Vec<T> = list
            .iter()
            .filter(|t| self.keep(attrs(t), *t))
            .cloned()
            .collect();
        let mut rebuilt = Punctuated::new();
        for t in kept {
            rebuilt.push(t);
        }
        *list = rebuilt;
    }
}

pub(crate) fn item_attrs(item: &syn::Item) -> &[Attribute] {
    use syn::Item::*;
    match item {
        Const(i) => &i.attrs,
        Enum(i) => &i.attrs,
        ExternCrate(i) => &i.attrs,
        Fn(i) => &i.attrs,
        ForeignMod(i) => &i.attrs,
        Impl(i) => &i.attrs,
        Macro(i) => &i.attrs,
        Mod(i) => &i.attrs,
        Static(i) => &i.attrs,
        Struct(i) => &i.attrs,
        Trait(i) => &i.attrs,
        TraitAlias(i) => &i.attrs,
        Type(i) => &i.attrs,
        Union(i) => &i.attrs,
        Use(i) => &i.attrs,
        _ => &[],
    }
}

fn impl_item_attrs(item: &syn::ImplItem) -> &[Attribute] {
    match item {
        syn::ImplItem::Const(i) => &i.attrs,
        syn::ImplItem::Fn(i) => &i.attrs,
        syn::ImplItem::Type(i) => &i.attrs,
        syn::ImplItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn trait_item_attrs(item: &syn::TraitItem) -> &[Attribute] {
    match item {
        syn::TraitItem::Const(i) => &i.attrs,
        syn::TraitItem::Fn(i) => &i.attrs,
        syn::TraitItem::Type(i) => &i.attrs,
        syn::TraitItem::Macro(i) => &i.attrs,
        _ => &[],
    }
}

fn expr_attrs(expr: &Expr) -> &[Attribute] {
    match expr {
        Expr::Assign(e) => &e.attrs,
        Expr::Async(e) => &e.attrs,
        Expr::Await(e) => &e.attrs,
        Expr::Block(e) => &e.attrs,
        Expr::Call(e) => &e.attrs,
        Expr::ForLoop(e) => &e.attrs,
        Expr::If(e) => &e.attrs,
        Expr::Loop(e) => &e.attrs,
        Expr::Macro(e) => &e.attrs,
        Expr::Match(e) => &e.attrs,
        Expr::MethodCall(e) => &e.attrs,
        Expr::Path(e) => &e.attrs,
        Expr::Unsafe(e) => &e.attrs,
        Expr::While(e) => &e.attrs,
        _ => &[],
    }
}

fn stmt_attrs(stmt: &syn::Stmt) -> &[Attribute] {
    match stmt {
        syn::Stmt::Local(l) => &l.attrs,
        syn::Stmt::Item(i) => item_attrs(i),
        syn::Stmt::Expr(e, _) => expr_attrs(e),
        syn::Stmt::Macro(m) => &m.attrs,
    }
}

impl VisitMut for Pruner {
    fn visit_file_mut(&mut self, file: &mut syn::File) {
        let items = std::mem::take(&mut file.items);
        file.items = items
            .into_iter()
            .filter(|i| self.keep(item_attrs(i), i))
            .collect();
        visit_mut::visit_file_mut(self, file);
    }

    fn visit_item_mod_mut(&mut self, m: &mut syn::ItemMod) {
        if let Some((_, items)) = &mut m.content {
            let taken = std::mem::take(items);
            *items = taken
                .into_iter()
                .filter(|i| self.keep(item_attrs(i), i))
                .collect();
        }
        visit_mut::visit_item_mod_mut(self, m);
    }

    fn visit_item_impl_mut(&mut self, i: &mut syn::ItemImpl) {
        let items = std::mem::take(&mut i.items);
        i.items = items
            .into_iter()
            .filter(|it| self.keep(impl_item_attrs(it), it))
            .collect();
        visit_mut::visit_item_impl_mut(self, i);
    }

    fn visit_item_trait_mut(&mut self, t: &mut syn::ItemTrait) {
        let items = std::mem::take(&mut t.items);
        t.items = items
            .into_iter()
            .filter(|it| self.keep(trait_item_attrs(it), it))
            .collect();
        visit_mut::visit_item_trait_mut(self, t);
    }

    fn visit_item_foreign_mod_mut(&mut self, m: &mut syn::ItemForeignMod) {
        let items = std::mem::take(&mut m.items);
        m.items = items
            .into_iter()
            .filter(|it| {
                let attrs: &[Attribute] = match it {
                    syn::ForeignItem::Fn(f) => &f.attrs,
                    syn::ForeignItem::Static(s) => &s.attrs,
                    syn::ForeignItem::Type(t) => &t.attrs,
                    syn::ForeignItem::Macro(m) => &m.attrs,
                    _ => &[],
                };
                self.keep(attrs, it)
            })
            .collect();
        visit_mut::visit_item_foreign_mod_mut(self, m);
    }

    fn visit_block_mut(&mut self, b: &mut syn::Block) {
        let stmts = std::mem::take(&mut b.stmts);
        b.stmts = stmts
            .into_iter()
            .filter(|s| self.keep(stmt_attrs(s), s))
            .collect();
        visit_mut::visit_block_mut(self, b);
    }

    fn visit_expr_match_mut(&mut self, m: &mut syn::ExprMatch) {
        let arms = std::mem::take(&mut m.arms);
        m.arms = arms
            .into_iter()
            .filter(|a| self.keep(&a.attrs, a))
            .collect();
        visit_mut::visit_expr_match_mut(self, m);
    }

    fn visit_fields_named_mut(&mut self, f: &mut syn::FieldsNamed) {
        self.retain(&mut f.named, |field| &field.attrs);
        visit_mut::visit_fields_named_mut(self, f);
    }

    fn visit_item_enum_mut(&mut self, e: &mut syn::ItemEnum) {
        self.retain(&mut e.variants, |v| &v.attrs);
        visit_mut::visit_item_enum_mut(self, e);
    }
}
