//! Tiny derive macro used by `app` so the fixture exercises proc-macro
//! compilation (a distinct, non-trivial compile-time contributor from plain
//! dependency crates).

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, DeriveInput};

/// Derives a `greet()` method that returns a friendly string mentioning the
/// type name. Purely illustrative — the fixture only needs *a* macro that
/// compiles and gets used.
#[proc_macro_derive(Greet)]
pub fn derive_greet(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = input.ident;
    let expanded = quote! {
        impl #name {
            pub fn greet(&self) -> String {
                format!("hello from {}", stringify!(#name))
            }
        }
    };
    TokenStream::from(expanded)
}
