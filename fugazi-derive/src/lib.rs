//! `#[derive(SaveState)]` — the per-indicator run-state save/load body generator
//! for fugazi's run-resuming feature.
//!
//! A spec-built strategy is a tree that interleaves concrete indicator structs
//! with type-erased trait-object boxes, so its runtime state cannot be captured
//! by a plain `#[derive(Serialize)]` (the boxes aren't `Serialize`) nor by
//! `typetag` (the generic instantiations are open-ended). Instead, the structure
//! is always rebuilt from the spec first and only the *values* are replayed in,
//! keyed positionally by tree shape. This derive generates the code that walks
//! one indicator struct's fields for that replay.
//!
//! It emits two **inherent** methods (uniquely named so they never clash with
//! the `Indicator` trait's `save_state`/`load_state`, which forward to them):
//!
//! ```ignore
//! impl<..> Foo<..> where <source-field bounds> {
//!     pub(crate) fn save_state_fields(&self) -> serde_json::Value { .. }
//!     pub(crate) fn load_state_fields(&mut self, v: &serde_json::Value) -> Result<(), String> { .. }
//! }
//! ```
//!
//! Field handling (default = plain serde state):
//! - unannotated → serialized/deserialized in place via `serde_json`.
//! - `#[state(source)]` → a child indicator; recurse via `crate::Indicator::save_state` /
//!   `load_state`. The derive adds a `where <field-ty>: crate::Indicator` bound.
//! - `#[state(skip)]` → omitted entirely (`PhantomData`, `Arc<Mutex>` shared
//!   handles, and config that the spec rebuild already restores identically).
//! - `#[state(window)]` → a fixed-capacity `Ring<T>`. Saved like plain state (a
//!   bare array, oldest first), but restored via
//!   `crate::indicators::stats::LoadWindow`, which sizes the rebuilt ring from
//!   the *destination's* capacity. A bare array does not record a capacity, and
//!   a window saved mid-warm-up is shorter than its period, so a plain
//!   `Deserialize` would restore it at the wrong size.
//!
//! Default-is-state is deliberate: forgetting `#[state(source)]` on a new child
//! field makes the derive try to `serde_json::to_value` a non-`Serialize`
//! trait-object box, which is a **compile error** — not a silent state loss.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, parse_macro_input};

mod grammar;

/// How a field participates in state save/load.
enum FieldRole {
    /// Plain data: serialize/deserialize in place via serde.
    State,
    /// A child indicator: recurse through the `Indicator` trait.
    Source,
    /// Not part of state: `PhantomData`, shared `Arc<Mutex>` handles, config.
    Skip,
    /// A fixed-capacity window (`Ring<T>`): saved as a bare array in logical
    /// order like any other state, but *restored* through
    /// `crate::indicators::stats::LoadWindow` so the capacity comes from the
    /// already-rebuilt destination rather than from the blob, which does not
    /// record it. See that trait's docs.
    Window,
}

/// Parse `#[state(source)]` / `#[state(skip)]` off a field; unannotated = state.
fn field_role(field: &syn::Field) -> Result<FieldRole, syn::Error> {
    let mut role = FieldRole::State;
    let mut seen = false;
    for attr in &field.attrs {
        if !attr.path().is_ident("state") {
            continue;
        }
        seen = true;
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("source") {
                role = FieldRole::Source;
                Ok(())
            } else if meta.path.is_ident("skip") {
                role = FieldRole::Skip;
                Ok(())
            } else if meta.path.is_ident("window") {
                role = FieldRole::Window;
                Ok(())
            } else {
                Err(meta.error("expected `source`, `skip` or `window`"))
            }
        })?;
    }
    // A bare `#[state]` with no inner keyword is a mistake worth catching.
    if seen && matches!(role, FieldRole::State) {
        return Err(syn::Error::new_spanned(
            field,
            "`#[state(...)]` needs `source` or `skip` (unannotated fields are already state)",
        ));
    }
    Ok(role)
}

#[proc_macro_derive(SaveState, attributes(state))]
pub fn derive_save_state(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(input: DeriveInput) -> Result<proc_macro2::TokenStream, syn::Error> {
    let name = &input.ident;

    let fields = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return Err(syn::Error::new_spanned(
                    &input,
                    "SaveState only supports structs with named fields",
                ));
            }
        },
        _ => {
            return Err(syn::Error::new_spanned(
                &input,
                "SaveState only supports structs",
            ));
        }
    };

    let mut save_stmts = Vec::new();
    let mut load_stmts = Vec::new();
    // Extra `where` predicates: every `#[state(source)]` field's type must be an
    // `Indicator` so the recursion resolves.
    let mut source_bounds = Vec::new();

    for field in fields {
        let ident = field.ident.as_ref().expect("named field");
        let key = ident.to_string();
        let ty = &field.ty;
        match field_role(field)? {
            FieldRole::Skip => {}
            FieldRole::State => {
                save_stmts.push(quote! {
                    map.insert(
                        #key.to_owned(),
                        ::serde_json::to_value(&self.#ident).unwrap_or_else(|e| {
                            panic!(concat!("save_state: field `", #key, "` is not serializable: {}"), e)
                        }),
                    );
                });
                load_stmts.push(quote! {
                    self.#ident = ::serde_json::from_value(
                        obj.get(#key).cloned().unwrap_or(::serde_json::Value::Null)
                    ).map_err(|e| ::std::format!("field `{}`: {}", #key, e))?;
                });
            }
            FieldRole::Window => {
                save_stmts.push(quote! {
                    map.insert(
                        #key.to_owned(),
                        ::serde_json::to_value(&self.#ident).unwrap_or_else(|e| {
                            panic!(concat!("save_state: field `", #key, "` is not serializable: {}"), e)
                        }),
                    );
                });
                load_stmts.push(quote! {
                    self.#ident = crate::indicators::stats::LoadWindow::load_window(
                        &self.#ident,
                        obj.get(#key).unwrap_or(&::serde_json::Value::Null),
                    ).map_err(|e| ::std::format!("field `{}`: {}", #key, e))?;
                });
            }
            FieldRole::Source => {
                source_bounds.push(quote! { #ty: crate::Indicator });
                save_stmts.push(quote! {
                    map.insert(#key.to_owned(), crate::Indicator::save_state(&self.#ident));
                });
                load_stmts.push(quote! {
                    crate::Indicator::load_state(
                        &mut self.#ident,
                        obj.get(#key).unwrap_or(&::serde_json::Value::Null),
                    ).map_err(|e| ::std::format!("field `{}` > {}", #key, e))?;
                });
            }
        }
    }

    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    // Fold the source bounds into the existing where-clause (if any). Normalize
    // to a fresh comma-separated predicate list rather than splicing raw tokens:
    // a struct whose `where` ends with a trailing comma (e.g.
    // `where S::Output: Clone,`) would otherwise produce a double comma.
    let mut predicates: Vec<proc_macro2::TokenStream> = Vec::new();
    if let Some(wc) = where_clause {
        for pred in &wc.predicates {
            predicates.push(quote! { #pred });
        }
    }
    predicates.extend(source_bounds);
    let where_tokens = if predicates.is_empty() {
        quote! {}
    } else {
        quote! { where #(#predicates),* }
    };

    Ok(quote! {
        impl #impl_generics #name #ty_generics #where_tokens {
            /// Serialize this indicator's own state plus its children's, keyed by
            /// field name. Generated by `#[derive(SaveState)]`.
            pub(crate) fn save_state_fields(&self) -> ::serde_json::Value {
                let mut map = ::serde_json::Map::new();
                #(#save_stmts)*
                ::serde_json::Value::Object(map)
            }

            /// Restore state produced by [`save_state_fields`](Self::save_state_fields)
            /// on an identically-constructed indicator. Generated by
            /// `#[derive(SaveState)]`.
            pub(crate) fn load_state_fields(
                &mut self,
                v: &::serde_json::Value,
            ) -> ::std::result::Result<(), ::std::string::String> {
                let obj = v.as_object().ok_or_else(|| {
                    ::std::format!("expected a state object, got {}", v)
                })?;
                #(#load_stmts)*
                ::std::result::Result::Ok(())
            }
        }
    })
}

/// `#[derive(SpecGrammar)]` — reflect a spec enum into a JSON-serializable
/// grammar descriptor. See the [`grammar`] module and `src/spec/grammar.rs`
/// in the `fugazi` crate.
///
/// Emits `pub(crate) fn grammar_tags() -> Vec<crate::spec::grammar::GrammarTag>`
/// on the enum, one `GrammarTag` per variant. Names, shapes, fields, defaults,
/// and prose come from the serde attrs and `///` docs already on the variant;
/// the three things serde cannot know come from a per-variant
/// `#[grammar(kind = "…", output = "…", since = "…")]` (and a container-level
/// `#[grammar(group = "…")]`). `kind` is mandatory.
#[proc_macro_derive(SpecGrammar, attributes(grammar))]
pub fn derive_spec_grammar(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match grammar::expand(input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}
