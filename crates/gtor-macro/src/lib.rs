//! # gtor-macro
//!
//! This crate provides the proc-macro implementation for [`gtor`](../gtor/index.html). It is not
//! intended for direct use; re-exported through `gtor` as `gtor::generator`.
//!
//! The public entry point is the [`generator`] attribute macro, which rewrites an ordinary
//! function into one that returns a `gtor::Generator`. See the [crate docs of `gtor`](../gtor/index.html)
//! for a full tour and examples.
use proc_macro::TokenStream;
use proc_macro2::{Ident, Span};
use quote::quote;
use std::collections::BTreeSet;
use syn::{
    parse_macro_input,
    visit_mut::{self, VisitMut},
    Expr, FnArg, ItemFn, Lifetime, ReceiverKind, ReturnType, Stmt, StmtMacro, Token, Type,
};

/// Marks a function as a [`gtor`](../gtor/index.html) generator.
///
/// This attribute macro rewrites the annotated function so that it returns a `Generator` instead
/// of running its body directly. Inside the body, `yield_value!(expr)` may be used to suspend the
/// generator and hand `expr` back to the caller; it is rewritten into
/// `ctx.yield_value(expr).await`, where `ctx` is a `GeneratorContext` passed into the body.
///
/// # Attributes
///
/// The attribute takes a single required argument:
///
/// * `yield_type`: the type of the values the generator yields.
///
/// # Behavior
///
/// The function's declared return type selects how the resulting generator is driven:
///
/// * No return type (or `-> ()`): the generator implements `Iterator<Item = {yield_type}>`.
/// * A return type `R`: the generator implements
///   `Iterator<Item = gtor::GeneratorItem<{yield_type}, R>>`; the function's final value is
///   delivered as `GeneratorItem::Return`.
/// * `-> !`: the generator never completes and does not implement `Iterator`; drive it with
///   `gtor::Generator::next_value`.
///
/// # Example
///
/// The example below is shown for illustration; a runnable version (with the same semantics)
/// lives in the [crate docs of `gtor`](../gtor/index.html). It is marked `ignore` here because
/// `gtor` depends on `gtor-macro`, not the other way around, so `gtor` cannot be imported from
/// this crate's doctest context.
///
/// ```ignore
/// use gtor::generator;
/// use std::pin::pin;
///
/// #[generator(yield_type = usize)]
/// fn count_down() {
///     for x in (1..10).rev() {
///         if x > 5 {
///             yield_value!(x);
///         }
///     }
/// }
///
/// fn main() {
///     let mut expected = [9, 8, 7, 6].into_iter();
///     for val in pin!(count_down()) {
///         assert_eq!(expected.next().unwrap(), val);
///     }
///     assert!(expected.next().is_none());
/// }
/// ```
#[proc_macro_attribute]
pub fn generator(attr: TokenStream, input: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as GeneratorArgs);
    let mut function = parse_macro_input!(input as ItemFn);

    let result = expand_generator(&mut function, args.yield_type);

    match result {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

struct GeneratorArgs {
    yield_type: Type,
}

impl GeneratorArgs {
    fn parse_yield_type(input: &syn::parse::ParseStream<'_>) -> syn::Result<Type> {
        input.parse()
    }
}

impl syn::parse::Parse for GeneratorArgs {
    fn parse(input: syn::parse::ParseStream<'_>) -> syn::Result<Self> {
        let full_span = input.span();
        let mut yield_type = None;

        while !input.is_empty() {
            let name = input.parse::<Ident>()?;
            input.parse::<Token![=]>()?;

            match name.to_string().as_str() {
                "yield_type" => {
                    if yield_type.is_some() {
                        return Err(syn::Error::new(name.span(), "duplicate `yield_type`"));
                    }
                    yield_type = Some(Self::parse_yield_type(&input)?)
                }
                other => return Err(syn::Error::new(name.span(), format!("invalid attribute `{other}`"))),
            }

            if !input.is_empty() {
                input.parse::<Token![,]>()?;
            }
        }

        let Some(yield_type) = yield_type else {
            return Err(syn::Error::new(full_span, "missing attribute `yield_type`"));
        };

        Ok(Self { yield_type })
    }
}

enum MyReturnType {
    Unit,
    Never,
    Type(Box<Type>),
}

fn expand_generator(function: &mut ItemFn, yield_type: Type) -> syn::Result<proc_macro2::TokenStream> {
    let body = &mut function.block;

    // Rewrite:
    //     yield_value!(expression);
    // into:
    //     ctx.yield_value(expression).await;
    let mut rewriter = YieldRewriter;
    rewriter.visit_block_mut(body);

    let my_return_type = match &function.sig.output {
        ReturnType::Default => MyReturnType::Unit,
        ReturnType::Type(_, ty) => match &**ty {
            Type::Never(_) => MyReturnType::Never,
            Type::Tuple(tuple) => {
                if tuple.elems.is_empty() {
                    MyReturnType::Unit
                } else {
                    MyReturnType::Type(ty.clone())
                }
            }
            _ => MyReturnType::Type(ty.clone()),
        },
    };

    let return_type = match &my_return_type {
        MyReturnType::Unit => quote! { () },
        MyReturnType::Never => quote! { ::gtor::Never },
        MyReturnType::Type(ret) => quote! { ::gtor::Return<#ret> },
    };

    let future_mapper = match &my_return_type {
        MyReturnType::Unit => quote! { ::std::convert::identity },
        MyReturnType::Never => quote! { ::gtor::future::map_to_never },
        MyReturnType::Type(_) => quote! { ::gtor::future::map_to_return },
    };

    // Return type mappings:
    // - `()` or nothing: Generator<impl Future<Output = ()>, Y>
    //   - i.e. a generator doesn't have a final result, but can complete
    // - generic `T`: Generator<impl Future<Output = Return<T>, Y>
    //   - i.e. a generator that has a final result
    // - `!`: Generator<impl Future<Output = Never, Y>
    //   - i.e. an infinite generator

    let attrs = &function.attrs;
    let vis = &function.vis;
    let name = &function.sig.ident;
    let generics = &function.sig.generics;
    let inputs = &function.sig.inputs;

    // Collect all the input lifetimes...
    let mut lifetime_idents: BTreeSet<Ident> = BTreeSet::new();
    let mut add_lifetime = |lifetime: Option<Lifetime>| {
        lifetime_idents.insert(
            lifetime
                .map(|lt| lt.ident)
                .unwrap_or_else(|| Ident::new("_", Span::call_site())),
        );
    };

    for arg in inputs {
        match arg {
            FnArg::Receiver(recv) => {
                if let ReceiverKind::Reference(_, lifetime, _) = &recv.kind {
                    add_lifetime(lifetime.clone());
                }
            }
            FnArg::Typed(pat_type) => {
                if let Type::Reference(reference) = pat_type.ty.as_ref() {
                    add_lifetime(reference.lifetime.clone());
                }
            }
        }
    }

    // ... and generate a `+ use < '_, 'a, >` to append to the future
    let mut use_lifetimes = quote! {};
    for ident in lifetime_idents {
        let lifetime = Lifetime {
            apostrophe: Span::call_site(),
            ident,
        };
        use_lifetimes = quote! { #use_lifetimes #lifetime, };
    }

    let where_clause = &function.sig.generics.where_clause;

    let body = &function.block;

    let expanded = quote! {
        #(#attrs)*
        #vis fn #name #generics(
            #inputs
        ) -> ::gtor::Generator<
            impl ::core::future::Future<Output = #return_type> + use< #use_lifetimes >,
            #yield_type
        >
        #where_clause
        {
            let future_factory = async move |mut ctx: ::gtor::GeneratorContext<#yield_type>| {
                #body
            };
            ::gtor::create_generator_mapped(future_factory, #future_mapper)
        }
    };

    Ok(expanded)
}

struct YieldRewriter;

impl VisitMut for YieldRewriter {
    fn visit_stmt_mut(&mut self, stmt: &mut Stmt) {
        if let Stmt::Macro(StmtMacro {
            attrs: _,
            mac,
            semi_token,
        }) = stmt
        {
            if mac.path.is_ident("yield_value") {
                let tokens = mac.tokens.clone();

                let expr: Expr = syn::parse_quote! {
                    ctx.yield_value(#tokens).await
                };

                *stmt = Stmt::Expr(expr, *semi_token);

                return;
            }
        }

        visit_mut::visit_stmt_mut(self, stmt);
    }
}
