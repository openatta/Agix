//! The interface-surface guards (D5 A1–A8) that have landed: no production code
//! marks anything `#[deprecated]` (A8), and the settings types a file is read
//! into hold nothing a file cannot set (A7).
//!
//! A deprecated item is an interface kept alive for callers the engine does not
//! have; the owner's rule is to change the callers instead. A `#[serde(skip)]`
//! field on `Settings` or `SettingsFile` is a load result riding on the file
//! values, which is what `LoadedSettings` exists to hold.

use guards::{par_flat_map, ws, Hit, SourceFile, View};
use syn::visit::Visit;

const FILE_SHAPED: &[&str] = &["Settings", "SettingsFile"];
const SETTINGS_FILE: &str = "crates/core/src/settings.rs";

fn deprecated_attrs(f: &SourceFile) -> Vec<Hit> {
    struct Find<'a> {
        f: &'a SourceFile,
        out: Vec<Hit>,
    }
    impl<'ast> Visit<'ast> for Find<'_> {
        fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
            if attr.path().is_ident("deprecated") {
                self.out.push(self.f.hit(syn::spanned::Spanned::span(attr)));
            }
        }
    }
    let tree = f.tree(View::Prod);
    let mut find = Find { f, out: Vec::new() };
    find.visit_file(&tree);
    find.out
}

#[test]
fn no_production_code_is_marked_deprecated() {
    let found = par_flat_map(ws().production(), deprecated_attrs);
    assert!(
        found.is_empty(),
        "[api_surface::A8] `#[deprecated]` in production code — change the callers and \
         remove the old item instead of keeping it alive:\n{}",
        found
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    );
}

fn skips_serde(field: &syn::Field) -> bool {
    field.attrs.iter().any(|attr| {
        if !attr.path().is_ident("serde") {
            return false;
        }
        let mut skip = false;
        let _ = attr.parse_nested_meta(|meta| {
            skip |= meta.path.is_ident("skip") || meta.path.is_ident("skip_deserializing");
            if meta.input.peek(syn::Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            }
            Ok(())
        });
        skip
    })
}

#[test]
fn the_file_shaped_settings_types_hold_only_what_a_file_sets() {
    let f = ws().file(SETTINGS_FILE);
    let tree = f.tree(View::Prod);
    let mut seen = Vec::new();
    let mut found = Vec::new();
    for item in &tree.items {
        let syn::Item::Struct(s) = item else { continue };
        let name = s.ident.to_string();
        if !FILE_SHAPED.contains(&name.as_str()) {
            continue;
        }
        seen.push(name.clone());
        for field in &s.fields {
            if skips_serde(field) {
                found.push(format!(
                    "{}: {name}.{}",
                    f.hit(syn::spanned::Spanned::span(field)),
                    field
                        .ident
                        .as_ref()
                        .map_or(String::new(), ToString::to_string)
                ));
            }
        }
    }
    seen.sort();
    assert_eq!(
        seen, FILE_SHAPED,
        "[api_surface::A7] {SETTINGS_FILE} no longer defines {FILE_SHAPED:?} at the top level; \
         point the guard at where they moved"
    );
    assert!(
        found.is_empty(),
        "[api_surface::A7] a settings file cannot set these, so they belong on \
         `LoadedSettings`, not on the file-shaped types:\n{}",
        found.join("\n")
    );
}
