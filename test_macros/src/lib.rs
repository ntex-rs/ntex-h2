use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemFn, parse_macro_input};

fn expand_async_fn(
    item: TokenStream,
    macro_name: &str,
    extra_attrs: proc_macro2::TokenStream,
    with_testing: bool,
) -> TokenStream {
    let input_fn = parse_macro_input!(item as ItemFn);

    if input_fn.sig.asyncness.is_none() {
        return syn::Error::new_spanned(
            input_fn.sig.fn_token,
            format!("#[{macro_name}] can only be applied to `async fn` functions"),
        )
        .to_compile_error()
        .into();
    }

    if !input_fn.sig.inputs.is_empty() {
        return syn::Error::new_spanned(
            &input_fn.sig.inputs,
            format!("#[{macro_name}] does not support arguments on the function"),
        )
        .to_compile_error()
        .into();
    }

    let vis = &input_fn.vis;
    let attrs = &input_fn.attrs;
    let ident = &input_fn.sig.ident;
    let output = &input_fn.sig.output;
    let block = &input_fn.block;

    let testing_call = if with_testing {
        quote! { .testing() }
    } else {
        quote! {}
    };

    let expanded = quote! {
        #(#attrs)*
        #extra_attrs
        #vis fn #ident() #output {
            ntex_rt::System::build()
                .name(stringify!(#ident))
                #testing_call
                .build(ntex_net::DefaultRuntime)
                .block_on(async move #block)
        }
    };

    TokenStream::from(expanded)
}

#[proc_macro_attribute]
pub fn asyn(_attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_async_fn(item, "asyn", quote! { #[test] }, true)
}

#[proc_macro_attribute]
pub fn example(_attr: TokenStream, item: TokenStream) -> TokenStream {
    expand_async_fn(item, "example", quote! {}, false)
}
