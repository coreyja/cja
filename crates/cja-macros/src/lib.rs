//! Procedural macros for cja framework.
//!
//! This crate provides the `#[cja::test]` attribute macro for writing database tests.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, ItemFn};

/// Marks an async function as a database test.
///
/// This macro will:
/// 1. Create an isolated test database with a unique name
/// 2. Run all migrations from the specified path
/// 3. Provide a `deadpool_postgres::Pool` to the test function
/// 4. Clean up the test database after the test completes (on success)
///
/// # Arguments
///
/// - `migrations` - Path to the migrations directory (default: `"./migrations"`)
/// - `migrator` - Path to a `Migrator` instance (alternative to `migrations`)
///
/// # Example
///
/// ```rust,ignore
/// use cja::testing::test;
/// use deadpool_postgres::Pool;
///
/// #[cja::test]
/// async fn test_create_user(pool: Pool) {
///     let client = pool.get().await.unwrap();
///     // ... test code using the database
/// }
///
/// #[cja::test(migrations = "../my-app/migrations")]
/// async fn test_with_custom_migrations(pool: Pool) {
///     // ...
/// }
/// ```
#[proc_macro_attribute]
pub fn test(args: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(args as TestArgs);
    let input_fn = parse_macro_input!(input as ItemFn);

    expand_test(args, input_fn)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

struct TestArgs {
    migrations: Option<String>,
    migrator: Option<syn::Path>,
}

impl syn::parse::Parse for TestArgs {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let mut migrations = None;
        let mut migrator = None;

        while !input.is_empty() {
            let ident: syn::Ident = input.parse()?;
            let _: syn::Token![=] = input.parse()?;

            match ident.to_string().as_str() {
                "migrations" => {
                    let lit: syn::LitStr = input.parse()?;
                    migrations = Some(lit.value());
                }
                "migrator" => {
                    migrator = Some(input.parse()?);
                }
                other => {
                    return Err(syn::Error::new(
                        ident.span(),
                        format!("unknown attribute: {other}"),
                    ));
                }
            }

            if input.peek(syn::Token![,]) {
                let _: syn::Token![,] = input.parse()?;
            }
        }

        Ok(Self {
            migrations,
            migrator,
        })
    }
}

fn expand_test(args: TestArgs, mut input_fn: ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    let fn_name = &input_fn.sig.ident;
    let fn_block = &input_fn.block;
    let fn_inputs = &input_fn.sig.inputs;
    let fn_output = &input_fn.sig.output;

    // Check that the function is async
    if input_fn.sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(
            &input_fn.sig,
            "test function must be async",
        ));
    }

    // Remove async from the signature (we'll wrap it ourselves)
    input_fn.sig.asyncness = None;

    // Determine migration source
    let migration_setup = if let Some(migrator_path) = args.migrator {
        quote! {
            let migrator = &#migrator_path;
        }
    } else {
        let migrations_path = args.migrations.unwrap_or_else(|| "./migrations".to_string());
        quote! {
            let migrator = ::cja::db::Migrator::from_path(#migrations_path)
                .expect("failed to load migrations");
        }
    };

    let expanded = quote! {
        #[test]
        fn #fn_name() {
            // Inner async function with original signature
            async fn inner(#fn_inputs) #fn_output #fn_block

            // Build test arguments
            let test_path = concat!(module_path!(), "::", stringify!(#fn_name));

            #migration_setup

            // Run the test using cja's test support
            ::cja::testing::run_test(test_path, &migrator, |pool| async move {
                inner(pool).await
            });
        }
    };

    Ok(expanded)
}
