//! Derive macro for `cja_secret::Secrets`. Use it through the `cja-secret`
//! re-export, not directly.

use proc_macro::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, LitStr, parse_macro_input, spanned::Spanned as _};

/// Declares every field of a struct as a setec secret.
///
/// Each field must be a `cja_secret::Secret<T>` (required) or an
/// `Option<cja_secret::Secret<T>>` (`None` when the secret doesn't exist),
/// annotated with `#[secret("<consumer>/<app>/<name>")]`.
#[proc_macro_derive(Secrets, attributes(secret))]
pub fn derive_secrets(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(
            input.span(),
            "Secrets can only be derived for structs",
        ));
    };
    let Fields::Named(fields) = &data.fields else {
        return Err(syn::Error::new(
            input.span(),
            "Secrets requires a struct with named fields",
        ));
    };
    if fields.named.is_empty() {
        return Err(syn::Error::new(
            input.span(),
            "Secrets requires at least one #[secret(\"...\")] field",
        ));
    }

    let mut idents = Vec::new();
    let mut names = Vec::new();
    let mut declares = Vec::new();
    for field in &fields.named {
        let name = secret_name(field)?;
        declares.push(if is_option(&field.ty) {
            quote! { store.declare_optional(#name) }
        } else {
            quote! { store.declare(#name) }
        });
        idents.push(field.ident.clone().expect("named field"));
        names.push(name);
    }

    let ident = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    Ok(quote! {
        impl #impl_generics ::cja_secret::Secrets for #ident #ty_generics #where_clause {
            const NAMES: &'static [&'static str] = &[#(#names),*];

            fn declare(
                store: &::cja_secret::SecretStore,
            ) -> impl ::core::future::Future<
                Output = ::core::result::Result<Self, ::cja_secret::Error>,
            > + ::core::marker::Send {
                async move {
                    let (#(#idents,)*) = ::cja_secret::__private::try_join!(
                        #(#declares),*
                    )?;
                    ::core::result::Result::Ok(Self { #(#idents),* })
                }
            }
        }
    })
}

/// Whether the field's type is spelled `Option<...>` (any path ending in
/// `Option`, so `std::option::Option` works too).
fn is_option(ty: &syn::Type) -> bool {
    let syn::Type::Path(path) = ty else {
        return false;
    };
    path.qself.is_none()
        && path
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "Option")
}

fn secret_name(field: &syn::Field) -> syn::Result<LitStr> {
    let mut found = None;
    for attr in &field.attrs {
        if !attr.path().is_ident("secret") {
            continue;
        }
        if found.is_some() {
            return Err(syn::Error::new(
                attr.span(),
                "duplicate #[secret] attribute",
            ));
        }
        let name: LitStr = attr.parse_args()?;
        validate_name(&name)?;
        found = Some(name);
    }
    found.ok_or_else(|| {
        syn::Error::new(
            field.span(),
            "every field needs #[secret(\"<consumer>/<app>/<name>\")]",
        )
    })
}

fn validate_name(name: &LitStr) -> syn::Result<()> {
    let value = name.value();
    if value.is_empty() || value.split('/').any(str::is_empty) {
        return Err(syn::Error::new(
            name.span(),
            "secret names must be non-empty and have no empty path segments",
        ));
    }
    Ok(())
}
