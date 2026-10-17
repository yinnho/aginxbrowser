//! `#[diting_op]` — our own op glue generator, replacing `#[op2]` one op
//! family at a time (campaign step ⑤, batches 6-8).
//!
//! The generated shape is deno_ops' slow path, statement for statement —
//! learned by `cargo expand`ing `#[op2]` output, not read from its source.
//! Two deliberate divergences, both ours to keep:
//!
//! - **no fastcall in v1.** The hot op in the tree (op_dom) is plain
//!   `#[op2]` today, i.e. already slow-path-only; the fast-dropped ops in
//!   the migration set are cold or rare (console, storage, crypto verify).
//!   The fastcall face gets built when a migrating op actually needs one.
//! - **no `super let`.** Every crate here is edition 2021; scope storage is
//!   pinned the way the v8 crate's own `scope!` macro does it (local
//!   storage, `Pin::new_unchecked`, `.init()`), which needs no edition-2024
//!   super-let bindings.
//!
//! The supported matrix grows exactly when a migration batch needs a new
//! shape, never before (no untested scaffolding). Current matrix:
//!
//! - args: `#[string] &str` (8 KB stack buffer via `_ops::to_str`, V8
//!   to_string coercion for non-strings), `#[string] String` (owned,
//!   `_ops::to_string`), `#[buffer] &[u8]` (`_ops::to_v8_slice`, one-byte
//!   throw on mismatch — needs NO CallbackScope, canvas_store proved it),
//!   `bool` (`is_true()`), `u32` (`to_u32_option`), `#[smi] u32` and `i32`
//!   (`to_i32_option` + cast), `f64` (`to_f64_option`), `&OpState` /
//!   `&mut OpState` (OpCtx external decode; borrowed AFTER all JS args
//!   decode, matching op2, so decode-throws never hold the RefCell).
//!   `_` parameter names are accepted (cfg-off stub variants).
//! - returns: `()` (no encode), `#[string] String` and `#[buffer] Vec<u8>`
//!   (`RustToV8Fallible` over the whole value — allocation can fail, so
//!   even infallible-typed returns encode fallibly), `bool` / `f64` / `i32`
//!   (`RustToV8RetVal`), `Result<T, JsErrorBox>` for T = String/Vec<u8>/
//!   bool (`Ok` arm encodes; `Err` arm throws via `error::to_v8_error`),
//!   `#[serde] Result<T, JsErrorBox>` (SerdeMarker wrap).
//! - sync only: `async fn` is a compile error. The async four arrive with
//!   the op driver host swap, where the settle-queue pipeline replaces
//!   deno's deferred machinery.
//!
//! The emitted wrapper keeps the op's fn name, so `build_extension` call
//! sites do not change — a migration diff is the attribute line only.

use proc_macro::TokenStream;
use proc_macro2::{Ident, Literal};
use quote::{quote, ToTokens};
use syn::parse_macro_input;
use syn::{FnArg, ItemFn, ReturnType, Type};

#[proc_macro_attribute]
pub fn diting_op(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "diting_op: bare #[diting_op] only — there is no (fast) etc., \
             the fastcall face is a separate future batch",
        )
        .to_compile_error()
        .into();
    }
    let func = parse_macro_input!(item as ItemFn);
    expand(func).unwrap_or_else(|e| e.to_compile_error()).into()
}

#[derive(Clone, Copy)]
enum ArgKind {
    /// `#[string] &str`
    Str,
    /// `#[string] String`
    StrOwn,
    /// `#[buffer] &[u8]`
    Buffer,
    Bool,
    U32,
    /// u16 — same u32 decode, narrower cast (op2 lumps u8/u16/u32 together)
    U16,
    /// `#[smi] u32` — slow path decodes as i32 then casts
    SmiU32,
    I32,
    F64,
    /// `&OpState`
    StateRef,
    /// `&mut OpState`
    StateMut,
}

#[derive(Clone, Copy)]
enum RetKind {
    Unit,
    /// `#[string] String`
    Str,
    /// `#[buffer] Vec<u8>`
    BufferVec,
    Bool,
    F64,
    I32,
    /// `Result<String, JsErrorBox>` with `#[string]`
    ResultStr,
    /// `Result<Vec<u8>, JsErrorBox>` with `#[buffer]`
    ResultBuffer,
    /// `Result<bool, JsErrorBox>`
    ResultBool,
    /// `Result<T, JsErrorBox>` with `#[serde]`
    ResultSerde,
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
            "sync-only; async ops arrive with the op driver host swap",
        ));
    }
    if !func.sig.generics.params.is_empty() || func.sig.generics.where_clause.is_some() {
        return Err(at(&name, "generic ops are not supported"));
    }
    if func.sig.variadic.is_some() {
        return Err(at(&name, "variadic ops are not supported"));
    }

    // ---- stacked attrs: return markers; the rest pass through -----------
    let mut ret_string = false;
    let mut ret_buffer = false;
    let mut ret_serde = false;
    let mut passthrough: Vec<syn::Attribute> = Vec::new();
    for attr in &func.attrs {
        let path = attr.path();
        if path.is_ident("string") {
            ret_string = true;
        } else if path.is_ident("buffer") {
            ret_buffer = true;
        } else if path.is_ident("serde") {
            ret_serde = true;
        } else if path.is_ident("smi") || path.is_ident("number") {
            return Err(at(
                &name,
                "this return attribute is not in the matrix yet; it lands \
                 with the batch that needs it",
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
    let ret_kind = classify_return(&func.sig.output, ret_string, ret_buffer, ret_serde)
        .ok_or_else(|| {
            at(
                &name,
                "returns in the matrix: (), #[string] String, #[buffer] Vec<u8>, \
                 bool, f64, i32, Result<String/Vec<u8>/bool/T, JsErrorBox> \
                 (#[string]/#[buffer]/#[serde] on the Result's Ok type)",
            )
        })?;

    // ---- args --------------------------------------------------------------
    // Wrapper bindings are POSITIONAL (`arg1`, `arg2`, ... in signature
    // order) and only `call` keeps the user's parameter names — deno_ops'
    // own trick, and the only airtight one: a parameter named `info` (see
    // op_subtle_hkdf's #[buffer] info: &[u8]) would otherwise shadow the
    // wrapper's FunctionCallbackInfo binding mid-decode.
    let mut kinds: Vec<ArgKind> = Vec::new();
    let mut call_inputs: Vec<proc_macro2::TokenStream> = Vec::new();
    for fa in &func.sig.inputs {
        let FnArg::Typed(pt) = fa else {
            return Err(at(&name, "self args are not ops"));
        };
        // Original pattern (minus the #[string]/#[buffer] attrs, which are
        // macro markers, not real parameter attributes) + type verbatim.
        let pt_pat = &pt.pat;
        let pt_ty = &pt.ty;
        call_inputs.push(quote! { #pt_pat: #pt_ty });
        let mut is_string = false;
        let mut is_buffer = false;
        let mut is_smi = false;
        for a in &pt.attrs {
            let p = a.path();
            if p.is_ident("string") {
                is_string = true;
            } else if p.is_ident("buffer") {
                is_buffer = true;
            } else if p.is_ident("smi") {
                is_smi = true;
            } else {
                return Err(at(
                    &name,
                    "arg attributes in the matrix: #[string], #[buffer], #[smi]",
                ));
            }
        }

        let kind = classify_arg(pt.ty.as_ref(), is_string, is_buffer, is_smi).ok_or_else(|| {
            at(
                &name,
                "args in the matrix: #[string] &str|String, #[buffer] &[u8], \
                 bool, u32, u16, #[smi] u32, i32, f64, &OpState, &mut OpState",
            )
        })?;
        kinds.push(kind);
    }

    // ---- glue assembly ---------------------------------------------------
    // A CallbackScope is opened exactly when something needs it: string
    // args (to_str/to_string borrow it) or a return path that allocates or
    // throws (String, Vec<u8>, every Result — the Err arm throws through
    // the scope). Buffers, scalars and state alone never open one
    // (canvas_store / history_jump_to shapes, confirmed against op2).
    let needs_scope = kinds
        .iter()
        .any(|k| matches!(k, ArgKind::Str | ArgKind::StrOwn))
        || matches!(
            ret_kind,
            RetKind::Str
                | RetKind::BufferVec
                | RetKind::ResultStr
                | RetKind::ResultBuffer
                | RetKind::ResultBool
                | RetKind::ResultSerde
        );
    let has_state = kinds
        .iter()
        .any(|k| matches!(k, ArgKind::StateRef | ArgKind::StateMut));

    // JS-visible args decode first (in signature order), state binds after
    // all of them — op2's order, so a decode throw never holds the RefCell.
    // Positional idents: arg{N} is the N-th parameter of the ORIGINAL
    // signature (1-based), state included — the corpus numbers them the
    // same way (state last to decode but keeps its signature position).
    let mut js_idx = 0usize;
    let mut js_decode: Vec<proc_macro2::TokenStream> = Vec::new();
    let mut state_decode: Vec<proc_macro2::TokenStream> = Vec::new();
    for (pos, kind) in kinds.iter().enumerate() {
        let ident = Ident::new(&format!("arg{}", pos + 1), name.span());
        match kind {
            ArgKind::StateRef => state_decode.push(quote! {
                let #ident = &::std::cell::RefCell::borrow(&opstate);
            }),
            ArgKind::StateMut => state_decode.push(quote! {
                let #ident = &mut ::std::cell::RefCell::borrow_mut(&opstate);
            }),
            _ => {
                let idx = Literal::usize_unsuffixed(js_idx);
                js_decode.push(decode_js_arg(&ident, kind, idx));
                js_idx += 1;
            }
        }
    }

    let call_args = (1..=kinds.len()).map(|pos| {
        let ident = Ident::new(&format!("arg{pos}"), name.span());
        quote! { #ident }
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

    let args_binding = if js_idx == 0 && !has_state {
        quote! {}
    } else {
        quote! {
            let args = ::deno_core::v8::FunctionCallbackArguments::
                from_function_callback_info_parts(info, &parts);
        }
    };

    let opctx_prologue = if has_state {
        quote! {
            let opctx: &'s _ = unsafe {
                &*(::deno_core::v8::Local::<::deno_core::v8::External>::cast_unchecked(
                    args.data(),
                )
                .value() as *const ::deno_core::_ops::OpCtx)
            };
            let opstate = &opctx.state;
        }
    } else {
        quote! {}
    };

    let call_expr = quote! { Self::call(#(#call_args,)*) };
    // () returns bind nothing; everything else flows through `result`.
    // Decode statements precede the call in both shapes, state binds last.
    let result_binding = match ret_kind {
        RetKind::Unit => quote! {
            #(#js_decode)*
            #(#state_decode)*
            #call_expr;
        },
        _ => quote! {
            #(#js_decode)*
            #(#state_decode)*
            let result = #call_expr;
        },
    };

    let return_encode = return_encode(ret_kind);

    let arg_count = Literal::usize_unsuffixed(js_idx);
    let ret_ty = func.sig.output.to_token_stream();
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
                    #opctx_prologue
                    #result_binding
                    #return_encode
                }
                extern "C" fn v8_fn_ptr(
                    info: *const ::deno_core::v8::FunctionCallbackInfo,
                ) {
                    Self::slow_function_impl(info);
                }
                #[allow(clippy::too_many_arguments)]
                fn call(#(#call_inputs,)*) #ret_ty {
                    #body
                }
            }
            #name::DECL
        }
    })
}

fn decode_js_arg(ident: &Ident, kind: &ArgKind, idx: Literal) -> proc_macro2::TokenStream {
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
        ArgKind::StrOwn => quote! {
            let #ident = args.get(#idx as i32);
            let #ident = ::deno_core::_ops::to_string(&mut scope, &#ident);
        },
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
        ArgKind::U32 | ArgKind::U16 => {
            option_decode(ident, idx, "to_u32_option", "expected u32", true)
        }
        ArgKind::SmiU32 | ArgKind::I32 => {
            option_decode(ident, idx, "to_i32_option", "expected i32", true)
        }
        ArgKind::F64 => option_decode(ident, idx, "to_f64_option", "expected f64", false),
        ArgKind::StateRef | ArgKind::StateMut => quote! {},
    }
}

/// The `let Some(x) = _ops::<fn>(&arg) else { throw; return 1 }; x as _;`
/// decode shared by u32 / i32(smi) / f64.
fn option_decode(
    ident: &Ident,
    idx: Literal,
    helper: &str,
    msg: &str,
    cast: bool,
) -> proc_macro2::TokenStream {
    let helper = Ident::new(helper, ident.span());
    let cast_line = if cast {
        quote! { let #ident = #ident as _; }
    } else {
        quote! {}
    };
    quote! {
        let #ident = args.get(#idx as i32);
        let ::core::option::Option::Some(#ident) =
            ::deno_core::_ops::#helper(&#ident)
        else {
            ::deno_core::_ops::throw_error_one_byte_info(&info, #msg);
            return 1;
        };
        #cast_line
    }
}

fn return_encode(kind: RetKind) -> proc_macro2::TokenStream {
    match kind {
        RetKind::Unit => quote! { return 0; },
        RetKind::Bool | RetKind::F64 | RetKind::I32 => quote! {
            ::deno_core::_ops::RustToV8RetVal::to_v8_rv(result, &mut rv);
            return 0;
        },
        // Infallible-typed but allocating returns encode the WHOLE value
        // fallibly (to_v8_fallible's Err is the encode failure itself).
        RetKind::Str | RetKind::BufferVec => quote! {
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
        // Result returns: Ok arm encodes (fallibly when allocating, plain
        // otherwise), Err arm converts the error and throws through the
        // scope. op2 also re-decodes OpCtx in the Err arm — a dead binding;
        // dropped here (first divergence beyond the two in the module doc).
        RetKind::ResultStr | RetKind::ResultBuffer => {
            let inner = quote! {
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
                }
            };
            result_match(inner)
        }
        RetKind::ResultBool => {
            let inner = quote! {
                ::deno_core::_ops::RustToV8RetVal::to_v8_rv(result, &mut rv);
            };
            result_match(inner)
        }
        RetKind::ResultSerde => {
            let inner = quote! {
                match ::deno_core::_ops::RustToV8Fallible::to_v8_fallible(
                    ::deno_core::_ops::RustToV8Marker::<
                        ::deno_core::_ops::SerdeMarker,
                        _,
                    >::from(result),
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
                }
            };
            result_match(inner)
        }
    }
}

fn result_match(ok_arm: proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    quote! {
        match result {
            ::core::result::Result::Ok(result) => { #ok_arm }
            ::core::result::Result::Err(err) => {
                let exception =
                    ::deno_core::error::to_v8_error(&mut scope, &err);
                scope.throw_exception(exception);
                return 1;
            }
        };
        return 0;
    }
}

// ---- classification helpers ----------------------------------------------

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

fn is_u8_generic(args: &syn::PathArguments) -> bool {
    if let syn::PathArguments::AngleBracketed(ab) = args {
        return ab.args.len() == 1
            && matches!(&ab.args[0], syn::GenericArgument::Type(t) if is_ident_type(t, "u8"));
    }
    false
}

/// Single-segment path like `Vec<u8>` / `Result<T, E>` when the ident
/// matches and the generic args satisfy `f`.
fn generic_path<'a>(ty: &'a Type, want: &str) -> Option<&'a syn::AngleBracketedGenericArguments> {
    let Type::Path(p) = ty else { return None };
    if p.path.leading_colon.is_none() && p.path.segments.len() == 1 {
        let seg = &p.path.segments[0];
        if seg.ident == want {
            if let syn::PathArguments::AngleBracketed(ab) = &seg.arguments {
                return Some(ab);
            }
        }
    }
    None
}

fn classify_arg(ty: &Type, is_string: bool, is_buffer: bool, is_smi: bool) -> Option<ArgKind> {
    match ty {
        Type::Reference(r) => {
            if is_string && is_ident_type(&r.elem, "str") && r.mutability.is_none() {
                Some(ArgKind::Str)
            } else if is_buffer && is_u8_slice(&r.elem) {
                Some(ArgKind::Buffer)
            } else if !is_string && !is_buffer && is_ident_type(&r.elem, "OpState") {
                if r.mutability.is_some() {
                    Some(ArgKind::StateMut)
                } else {
                    Some(ArgKind::StateRef)
                }
            } else {
                None
            }
        }
        Type::Path(p) => {
            let bare = |w: &str| is_ident(&p.path, w);
            if is_string && bare("String") {
                Some(ArgKind::StrOwn)
            } else if !is_string && !is_buffer && !is_smi {
                if bare("bool") {
                    Some(ArgKind::Bool)
                } else if bare("u32") {
                    Some(ArgKind::U32)
                } else if bare("u16") {
                    Some(ArgKind::U16)
                } else if bare("i32") {
                    Some(ArgKind::I32)
                } else if bare("f64") {
                    Some(ArgKind::F64)
                } else {
                    None
                }
            } else if is_smi && bare("u32") {
                Some(ArgKind::SmiU32)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn classify_return(
    output: &ReturnType,
    ret_string: bool,
    ret_buffer: bool,
    ret_serde: bool,
) -> Option<RetKind> {
    if let ReturnType::Default = output {
        return Some(RetKind::Unit);
    }
    let ty = match output {
        ReturnType::Type(_, t) => t.as_ref(),
        ReturnType::Default => unreachable!(),
    };
    if ret_serde {
        // Result<T, JsErrorBox> with #[serde]
        let ab = generic_path(ty, "Result")?;
        if ab.args.len() != 2 {
            return None;
        }
        let syn::GenericArgument::Type(err) = &ab.args[1] else {
            return None;
        };
        if !js_error_box(err) {
            return None;
        }
        return Some(RetKind::ResultSerde);
    }
    if let Type::Path(p) = ty {
        if is_ident(&p.path, "bool") && !ret_string && !ret_buffer {
            return Some(RetKind::Bool);
        }
        if is_ident(&p.path, "f64") && !ret_string && !ret_buffer {
            return Some(RetKind::F64);
        }
        if is_ident(&p.path, "i32") && !ret_string && !ret_buffer {
            return Some(RetKind::I32);
        }
        if ret_string && is_ident(&p.path, "String") {
            return Some(RetKind::Str);
        }
        if ret_buffer {
            if let Some(ab) = generic_path(ty, "Vec") {
                if is_u8_generic(&syn::PathArguments::AngleBracketed(ab.clone())) {
                    return Some(RetKind::BufferVec);
                }
            }
        }
    }
    // Result<T, JsErrorBox> where T ∈ {String #[string], Vec<u8> #[buffer], bool}
    let ab = generic_path(ty, "Result")?;
    if ab.args.len() != 2 {
        return None;
    }
    let (syn::GenericArgument::Type(ok), syn::GenericArgument::Type(err)) =
        (&ab.args[0], &ab.args[1])
    else {
        return None;
    };
    if !js_error_box(err) {
        return None;
    }
    if ret_string && is_ident_type(ok, "String") {
        return Some(RetKind::ResultStr);
    }
    if ret_buffer {
        if let Some(vab) = generic_path(ok, "Vec") {
            if is_u8_generic(&syn::PathArguments::AngleBracketed(vab.clone())) {
                return Some(RetKind::ResultBuffer);
            }
        }
    }
    if !ret_string && !ret_buffer && is_ident_type(ok, "bool") {
        return Some(RetKind::ResultBool);
    }
    None
}

/// The error type spelled `deno_error::JsErrorBox` or imported bare.
fn js_error_box(ty: &Type) -> bool {
    let Type::Path(p) = ty else { return false };
    p.path
        .segments
        .last()
        .map(|s| s.ident == "JsErrorBox")
        .unwrap_or(false)
}
