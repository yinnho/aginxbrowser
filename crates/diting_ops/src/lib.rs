//! `#[diting_op]` — our own op glue generator, replacing `#[op2]` one op
//! family at a time (campaign step ④, batch 6).
//!
//! The generated shape is deno_ops' slow path, statement for statement —
//! learned by `cargo expand`ing `#[op2]` output, not read from its source.
//! Two deliberate divergences, both ours to keep:
//!
//! - **no fastcall in v1.** The batch-1 ops are cold paths (URL plumbing,
//!   encoding labels, CSS.supports); every genuinely hot op in the tree is
//!   plain `#[op2]` today, i.e. already slow-path-only. The fastcall face
//!   gets built when a migrating op actually has one.
//! - **no `super let`.** Every crate here is edition 2021; scope storage is
//!   pinned the way the v8 crate's own `scope!` macro does it (local
//!   storage, `Pin::new_unchecked`, `.init()`), which needs no edition-2024
//!   super-let bindings.
//!
//! The supported matrix grows exactly when a migration batch needs a new
//! shape, never before (no untested scaffolding):
//!
//! - args: `#[string] &str` (8 KB stack buffer via `_ops::to_str`, V8
//!   to_string coercion for non-strings), `#[buffer] &[u8]`
//!   (`_ops::to_v8_slice`, one-byte throw on mismatch), plain `bool`
//!   (`is_true()`).
//! - returns: `#[string] String` (`RustToV8Fallible` — string allocation
//!   can fail, so even infallible-typed returns encode fallibly),
//!   `bool` / `f64` (`RustToV8RetVal`; no CallbackScope needed).
//! - sync only: `async fn` is a compile error. The async four arrive with
//!   the op driver host swap, where the settle-queue pipeline replaces
//!   deno's deferred machinery.
//!
//! The emitted wrapper keeps the op's fn name, so `build_extension` call
//! sites do not change — a migration diff is the attribute line only.

use proc_macro::TokenStream;
use proc_macro2::{Ident, Literal};
use quote::quote;
use syn::parse_macro_input;
use syn::{FnArg, ItemFn, Pat, ReturnType, Type};

#[proc_macro_attribute]
pub fn diting_op(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let func = parse_macro_input!(item as ItemFn);
    expand(func).unwrap_or_else(|e| e.to_compile_error()).into()
}

enum ArgKind {
    Str,
    Buffer,
    Bool,
}

enum RetKind {
    Str,
    Bool,
    F64,
}

fn at(item: &Ident, msg: &str) -> syn::Error {
    syn::Error::new(item.span(), format!("diting_op: {msg}"))
}

fn expand(func: ItemFn) -> syn::Result<proc_macro2::TokenStream> {
    let name = func.sig.ident.clone();
    let name_str = name.to_string();

    // ---- signature gates -------------------------------------------------
    if func.sig.asyncness.is_some() {
        return Err(at(
            &name,
            "v1 is sync-only; async ops arrive with the op driver host swap",
        ));
    }
    if !func.sig.generics.params.is_empty() || func.sig.generics.where_clause.is_some() {
        return Err(at(&name, "generic ops are not supported"));
    }
    if func.sig.variadic.is_some() {
        return Err(at(&name, "variadic ops are not supported"));
    }

    // ---- stacked attrs: #[string] return marker; the rest pass through ----
    let mut ret_string = false;
    let mut passthrough: Vec<syn::Attribute> = Vec::new();
    for attr in &func.attrs {
        let path = attr.path();
        if path.is_ident("string") {
            ret_string = true;
        } else if path.is_ident("buffer")
            || path.is_ident("smi")
            || path.is_ident("number")
            || path.is_ident("serde")
        {
            return Err(at(
                &name,
                "this return attribute is not in the v1 matrix; it lands with \
                 the batch that needs it",
            ));
        } else if path.is_ident("op2") {
            return Err(at(
                &name,
                "remove the #[op2] attribute — diting_op replaces it",
            ));
        } else if path.is_ident("diting_op") {
            // already consumed; harmless if it shows up in the item attrs
        } else {
            passthrough.push(attr.clone());
        }
    }

    // ---- return type -----------------------------------------------------
    let ret_kind = match &func.sig.output {
        ReturnType::Default => {
            return Err(at(&name, "v1 returns are #[string] String, bool, or f64"))
        }
        ReturnType::Type(_, ty) => match (ty.as_ref(), ret_string) {
            (Type::Path(p), true) if is_ident(&p.path, "String") => RetKind::Str,
            (Type::Path(p), false) if is_ident(&p.path, "bool") => RetKind::Bool,
            (Type::Path(p), false) if is_ident(&p.path, "f64") => RetKind::F64,
            _ => {
                return Err(at(
                    &name,
                    "v1 returns are #[string] String, bool, or f64 — anything \
                     else lands with the batch that needs it",
                ))
            }
        },
    };

    // ---- args --------------------------------------------------------------
    let mut args: Vec<(Ident, ArgKind)> = Vec::new();
    for fa in &func.sig.inputs {
        let FnArg::Typed(pt) = fa else {
            return Err(at(&name, "self args are not ops"));
        };
        let Pat::Ident(pi) = pt.pat.as_ref() else {
            return Err(at(&name, "plain identifier parameters only"));
        };
        let ident = pi.ident.clone();

        let mut is_string = false;
        let mut is_buffer = false;
        for a in &pt.attrs {
            let p = a.path();
            if p.is_ident("string") {
                is_string = true;
            } else if p.is_ident("buffer") {
                is_buffer = true;
            } else {
                return Err(at(
                    &ident,
                    "v1 arg attributes are #[string] and #[buffer] only",
                ));
            }
        }

        let kind = match (pt.ty.as_ref(), is_string, is_buffer) {
            (Type::Reference(r), true, false) if is_ident_type(&r.elem, "str") => ArgKind::Str,
            (Type::Reference(r), false, true) if is_u8_slice(&r.elem) => ArgKind::Buffer,
            (Type::Path(p), false, false) if is_ident(&p.path, "bool") => ArgKind::Bool,
            _ => {
                return Err(at(
                    &ident,
                    "v1 args are #[string] &str, #[buffer] &[u8], or plain bool",
                ))
            }
        };
        args.push((ident, kind));
    }

    // ---- glue assembly ---------------------------------------------------
    // A scope is opened exactly when something needs it: string args (to_str
    // borrows it), buffer args, or a fallible String return encode. Zero-arg
    // f64 ops (op_clock_ms shape) never open one — matches deno's output.
    let needs_scope =
        args.iter().any(|(_, k)| !matches!(k, ArgKind::Bool)) || matches!(ret_kind, RetKind::Str);

    let decode: Vec<_> = args
        .iter()
        .enumerate()
        .map(|(i, (ident, kind))| {
            let idx = Literal::usize_unsuffixed(i);
            match kind {
                ArgKind::Str => {
                    let temp = Ident::new(&format!("{ident}_temp"), ident.span());
                    quote! {
                        let #ident = args.get(#idx as i32);
                        let mut #temp: [::core::mem::MaybeUninit<u8>;
                            ::deno_core::_ops::STRING_STACK_BUFFER_SIZE] =
                            [::core::mem::MaybeUninit::uninit();
                                ::deno_core::_ops::STRING_STACK_BUFFER_SIZE];
                        let #ident = if !#ident.is_string() {
                            let mut tc_store =
                                ::deno_core::v8::TryCatch::new(&mut scope);
                            let tc = unsafe {
                                ::core::pin::Pin::new_unchecked(&mut tc_store)
                            };
                            let mut tc = tc.init();
                            match #ident.to_string(&mut tc) {
                                ::core::option::Option::Some(v) => v.into(),
                                ::core::option::Option::None => {
                                    tc.rethrow();
                                    return 1;
                                }
                            }
                        } else {
                            #ident
                        };
                        let #ident = &::deno_core::_ops::to_str(
                            &mut scope,
                            &#ident,
                            &mut #temp,
                        );
                    }
                }
                ArgKind::Buffer => {
                    let temp = Ident::new(&format!("{ident}_temp"), ident.span());
                    quote! {
                        let #ident = args.get(#idx as i32);
                        let #temp =
                            match unsafe { ::deno_core::_ops::to_v8_slice::<u8>(#ident) } {
                                ::core::result::Result::Ok(v) => v,
                                ::core::result::Result::Err(e) => {
                                    ::deno_core::_ops::throw_error_one_byte_info(&info, e);
                                    return 1;
                                }
                            };
                        let #ident = #temp.as_ref();
                    }
                }
                ArgKind::Bool => quote! {
                    let #ident = args.get(#idx as i32);
                    let #ident = #ident.is_true();
                },
            }
        })
        .collect();

    let call_args = args.iter().map(|(ident, _)| quote! { #ident });
    let call_inputs = args.iter().map(|(ident, kind)| {
        let ty = match kind {
            ArgKind::Str => quote! { &str },
            ArgKind::Buffer => quote! { &[u8] },
            ArgKind::Bool => quote! { bool },
        };
        quote! { #ident: #ty }
    });

    let scope_prologue = if needs_scope {
        quote! {
            let mut scope_store =
                unsafe { ::deno_core::v8::CallbackScope::new(info) };
            let scope = unsafe { ::core::pin::Pin::new_unchecked(&mut scope_store) };
            let mut scope = scope.init();
        }
    } else {
        quote! {}
    };

    let args_binding = if args.is_empty() {
        quote! {}
    } else {
        quote! {
            let args = ::deno_core::v8::FunctionCallbackArguments::
                from_function_callback_info_parts(info, &parts);
        }
    };

    let return_encode = match ret_kind {
        RetKind::Str => quote! {
            match ::deno_core::_ops::RustToV8Fallible::to_v8_fallible(
                result,
                &mut scope,
            ) {
                ::core::result::Result::Ok(v) => rv.set(v),
                ::core::result::Result::Err(rv_err) => {
                    ::deno_core::_ops::throw_error_js_error_class(
                        &mut scope,
                        &rv_err,
                    );
                    return 1;
                }
            };
            return 0;
        },
        RetKind::Bool | RetKind::F64 => quote! {
            ::deno_core::_ops::RustToV8RetVal::to_v8_rv(result, &mut rv);
            return 0;
        },
    };

    let arg_count = Literal::usize_unsuffixed(args.len());
    let ret_ty = match ret_kind {
        RetKind::Str => quote! { ::std::string::String },
        RetKind::Bool => quote! { bool },
        RetKind::F64 => quote! { f64 },
    };
    let vis = &func.vis;
    let body = &func.block;

    Ok(quote! {
        #(#passthrough)*
        #[allow(non_camel_case_types)]
        #vis fn #name() -> ::deno_core::_ops::OpDecl {
            struct #name {
                _unconstructable: ::core::marker::PhantomData<()>,
            }
            impl #name {
                const DECL: ::deno_core::_ops::OpDecl =
                    ::deno_core::_ops::OpDecl::new_internal_op2(
                        {
                            const LITERAL: &'static [u8] = #name_str.as_bytes();
                            const STR: ::deno_core::v8::OneByteConst =
                                ::deno_core::FastStaticString::create_external_onebyte_const(
                                    LITERAL,
                                );
                            let s: &'static ::deno_core::v8::OneByteConst = &STR;
                            (#name_str, ::deno_core::FastStaticString::new(s))
                        },
                        false,
                        false,
                        false,
                        #arg_count as u8,
                        false,
                        false,
                        Self::v8_fn_ptr as _,
                        Self::slow_function_impl,
                        ::deno_core::AccessorType::None,
                        ::core::option::Option::None,
                        ::core::option::Option::None,
                        ::deno_core::OpMetadata {
                            ..::deno_core::OpMetadata::default()
                        },
                    );
                fn slow_function_impl<'s>(
                    info: *const ::deno_core::v8::FunctionCallbackInfo,
                ) -> usize {
                    let info: &'s _ = unsafe { &*info };
                    let _reentrancy_check_guard =
                        ::deno_core::_ops::reentrancy_check(&Self::DECL);
                    let parts: ::deno_core::v8::FunctionCallbackInfoParts<'s> =
                        info.get_parts();
                    #scope_prologue
                    let mut rv = parts.return_value;
                    #args_binding
                    let result = {
                        #(#decode)*
                        Self::call(#(#call_args,)*)
                    };
                    #return_encode
                }
                extern "C" fn v8_fn_ptr(
                    info: *const ::deno_core::v8::FunctionCallbackInfo,
                ) {
                    Self::slow_function_impl(info);
                }
                #[allow(clippy::too_many_arguments)]
                fn call(#(#call_inputs,)*) -> #ret_ty {
                    #body
                }
            }
            #name::DECL
        }
    })
}

// ---- small type helpers --------------------------------------------------

/// True when a path is exactly the bare identifier `want` — no colons, no
/// generic arguments.
fn is_ident(path: &syn::Path, want: &str) -> bool {
    path.leading_colon.is_none()
        && path.segments.len() == 1
        && path.segments[0].arguments.is_none()
        && path.segments[0].ident == want
}

fn is_ident_type(ty: &Type, want: &str) -> bool {
    matches!(ty, Type::Path(p) if is_ident(&p.path, want))
}

fn is_u8_slice(ty: &Type) -> bool {
    matches!(ty, Type::Slice(s) if is_ident_type(&s.elem, "u8"))
}
