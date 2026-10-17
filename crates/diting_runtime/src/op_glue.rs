//! # op glue — the standalone decode/encode helpers `#[diting_op]` emits
//!
//! **逐字拷贝自 deno_core 0.411.0**（`runtime/ops.rs` + `runtime/
//! ops_rust_to_v8.rs`，MIT，Copyright 2018-2026 the Deno authors）。
//! 战役方法论（2026-10-17 用户指示）：GitHub 上早有的轮子学习而不是硬
//! 重写——上游 N 多轮修的坑（数值回卷语义、latin1→utf8 快路、分配失败
//! 路径）跟着代码一起搬过来。改动只有三处：路径改名、只留矩阵用到的
//! 子集（其余成员随批增长，同宏的矩阵纪律）、每处删减都有注。
//!
//! 还留在 deno_core 的三张脸（OpDecl/OpCtx/reentrancy_check 与
//! `error::to_v8_error`/`throw_error_js_error_class`——后者依赖
//! JsRealm exception_state + JS 侧装的 custom error builder，属于宿主
//! 面）随宿主换批（步骤⑤→⑥）再认领。
//!
//! Copy source: `~/.cargo/registry/src/*/deno_core-0.411.0/runtime/ops.rs`
//! and `.../runtime/ops_rust_to_v8.rs`.

use std::borrow::Cow;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::mem::MaybeUninit;

use serde_v8::V8Sliceable;

/// The default string buffer size on the stack that prevents mallocs in some
/// string functions. Keep in mind that Windows only offers 1MB stacks by default,
/// so this is a limited resource!
pub const STRING_STACK_BUFFER_SIZE: usize = 1024 * 8;

macro_rules! try_integer_some {
    ($n:ident $type:ident $is:ident) => {
        if $n.$is() {
            // SAFETY: v8 handles can be transmuted
            let n: &v8::$type = unsafe { std::mem::transmute($n) };
            // `n.value()` is already an integer here, so the `as` cast wraps on
            // overflow rather than saturating. That is deliberate.
            return Some(n.value() as _);
        }
    };
}

macro_rules! try_number_int_some {
    ($n:ident $type:ident $is:ident $trunc:ident) => {
        if $n.$is() {
            // SAFETY: v8 handles can be transmuted
            let n: &v8::$type = unsafe { std::mem::transmute($n) };
            // The inner `as $trunc` saturates the float; the outer `as _` narrows
            // the resulting integer with wrapping. Both are deliberate.
            return Some(n.value().trunc() as $trunc as _);
        }
    };
}

macro_rules! try_number_some {
    ($n:ident $type:ident $is:ident) => {
        if $n.$is() {
            // SAFETY: v8 handles can be transmuted
            let n: &v8::$type = unsafe { std::mem::transmute($n) };
            return Some(n.value() as _);
        }
    };
}

macro_rules! try_bignum {
    ($n:ident $method:ident) => {
        if $n.is_big_int() {
            // SAFETY: v8 handles can be transmuted
            let $n: &v8::BigInt = unsafe { std::mem::transmute($n) };
            return Some($n.$method().0 as _);
        }
    };
}

// Subset note: upstream also has to_u64/i64/f32_option — dropped, the
// matrix has no u64/i64/f32 params yet.

pub fn to_u32_option(number: &v8::Value) -> Option<u32> {
    try_integer_some!(number Integer is_uint32);
    try_integer_some!(number Int32 is_int32);
    try_number_int_some!(number Number is_number u64);
    try_bignum!(number u64_value);
    None
}

pub fn to_i32_option(number: &v8::Value) -> Option<i32> {
    try_integer_some!(number Uint32 is_uint32);
    try_integer_some!(number Int32 is_int32);
    try_number_int_some!(number Number is_number i64);
    try_bignum!(number i64_value);
    None
}

pub fn to_f64_option(number: &v8::Value) -> Option<f64> {
    try_number_some!(number Number is_number);
    try_bignum!(number i64_value);
    None
}

/// Converts a [`v8::Value`] to an owned string.
///
/// Uses a thread-local reusable buffer with [`v8::String::write_utf8_into`] to
/// avoid repeated heap allocation. After warmup the thread-local buffer's
/// capacity is large enough for most strings, so `write_utf8_into` performs a
/// single-pass write (via `ValueView` internally) with no malloc. The final
/// `clone()` is a simple memcpy.
#[inline(always)]
pub fn to_string(scope: &mut v8::Isolate, string: &v8::Value) -> String {
    if !string.is_string() {
        return String::new();
    }

    let string: &v8::String = unsafe { std::mem::transmute(string) };

    thread_local! {
      static BUF: RefCell<String> = const { RefCell::new(String::new()) };
    }

    BUF.with(|buf| {
        let mut buf = buf.borrow_mut();
        string.write_utf8_into(scope, &mut buf);
        buf.clone()
    })
}

/// Converts a [`v8::String`] to either an owned string, or a borrowed string, depending on whether it fits into the
/// provided buffer.
#[inline(always)]
pub fn to_str<'a, const N: usize>(
    scope: &mut v8::Isolate,
    string: &v8::Value,
    buffer: &'a mut [MaybeUninit<u8>; N],
) -> Cow<'a, str> {
    if !string.is_string() {
        return Cow::Borrowed("");
    }

    // SAFETY: We checked is_string above
    let string: &v8::String = unsafe { std::mem::transmute(string) };

    string.to_rust_cow_lossy(scope, buffer)
}

/// Retrieve a [`serde_v8::V8Slice`] from a typed array in an [`v8::ArrayBufferView`].
pub fn to_v8_slice<'a, T>(
    input: v8::Local<'a, v8::Value>,
) -> Result<serde_v8::V8Slice<T>, &'static str>
where
    T: V8Sliceable,
    v8::Local<'a, T::V8>: TryFrom<v8::Local<'a, v8::Value>>,
    v8::Local<'a, v8::ArrayBufferView>: From<v8::Local<'a, T::V8>>,
{
    let (store, offset, length) = match v8::Local::<T::V8>::try_from(input) {
        Ok(buf) => {
            let buf: v8::Local<v8::ArrayBufferView> = buf.into();
            let Some(buffer) = buf.get_backing_store() else {
                return Err("buffer missing");
            };
            (buffer, buf.byte_offset(), buf.byte_length())
        }
        _ => {
            return Err("expected typed ArrayBufferView");
        }
    };
    let slice = unsafe { serde_v8::V8Slice::from_parts(store, offset..(offset + length)) };
    Ok(slice)
}

pub fn throw_error_one_byte_info(info: &v8::FunctionCallbackInfo, message: &str) {
    v8::callback_scope!(unsafe scope, info);
    throw_error_one_byte(scope, message);
}

pub fn throw_error_one_byte<'s, 'i>(scope: &mut v8::PinScope<'s, 'i>, message: &str) {
    let msg = v8::String::new_from_one_byte(scope, message.as_bytes(), v8::NewStringType::Normal)
        .unwrap();
    let exc = v8::Exception::type_error(scope, msg);
    scope.throw_exception(exc);
}

/// The encode-failure throw: when `to_v8_fallible` fails (string/array over
/// v8's length limit, serde serialization failure) the value never
/// materialized, so a plain Error is thrown — NOT the class-built JS error
/// machinery deno's `throw_error_js_error_class` routes through (that needs
/// the host's exception state + JS builder callback, a host face claimed
/// with the runtime-host swap). Deliberate divergence on a barely-observable
/// path; noted in the campaign plan.
pub fn throw_serde_v8_error<'s, 'i>(scope: &mut v8::PinScope<'s, 'i>, err: &serde_v8::Error) {
    let msg = err.to_string();
    let m = v8::String::new(scope, &msg).unwrap_or_else(|| {
        v8::String::new_from_one_byte(
            scope,
            b"failed to encode op return value",
            v8::NewStringType::Normal,
        )
        .unwrap()
    });
    let exc = v8::Exception::error(scope, m);
    scope.throw_exception(exc);
}

//
// Rust→V8 conversion traits (ops_rust_to_v8.rs subset)
//

/// Convert a value to a `v8::Local`, potentially allocating.
pub trait RustToV8<'a> {
    fn to_v8<'i>(self, scope: &mut v8::PinScope<'a, 'i>) -> v8::Local<'a, v8::Value>;
}

/// Places a value in a `v8::ReturnValue`, non-allocating.
pub trait RustToV8RetVal<'a>: RustToV8<'a> {
    fn to_v8_rv(self, rv: &mut v8::ReturnValue<'a>);
}

/// Convert a value to a `v8::Local`, potentially allocating or failing.
pub trait RustToV8Fallible<'a> {
    fn to_v8_fallible<'i>(
        self,
        scope: &mut v8::PinScope<'a, 'i>,
    ) -> serde_v8::Result<v8::Local<'a, v8::Value>>;
}

/// Transparent marker struct that can be used to alter the conversion in various ways. The
/// main uses of this are to mark something as being `serde`-serializable.
#[repr(transparent)]
pub struct RustToV8Marker<M, T>(T, PhantomData<M>);
impl<M: Marker, T> From<T> for RustToV8Marker<M, T> {
    #[inline(always)]
    fn from(value: T) -> Self {
        RustToV8Marker(value, PhantomData)
    }
}

/// This struct should be serialized using `serde`.
pub struct SerdeMarker;
impl Marker for SerdeMarker {}

/// This buffer should be serialized as an ArrayBuffer.
pub struct ArrayBufferMarker;
impl Marker for ArrayBufferMarker {}

trait Marker {}

/// Helper macro for [`RustToV8`] to reduce boilerplate.
///
/// Implements a Rust-to-v8 conversion that cannot fail. Multiple types may be specified
/// to apply the same implementation.
macro_rules! to_v8 {
  (( $( $ty:ty ),+ ) : |$value:ident, $scope:ident| $block:expr_2021) => {
    $(
      impl <'a> RustToV8<'a> for $ty {
        #[inline(always)]
        fn to_v8<'i>(self, scope: &mut v8::PinScope<'a, 'i>) -> v8::Local<'a, v8::Value> {
          let $value = self;
          let $scope = scope;
          v8::Local::<v8::Value>::from($block)
        }
      }
    )+
  };
  ($ty:ty : |$value:ident, $scope:ident| $block:expr_2021) => {
    to_v8!(( $ty ) : |$value, $scope| $block);
  };
}

/// Helper macro for [`RustToV8RetVal`] to reduce boilerplate.
///
/// Implements a Rust-to-v8 conversion that cannot allocate or fail. Multiple types may be specified
/// to apply the same implementation. Places the return value in a `v8::ReturnValue`.
macro_rules! to_v8_retval {
  (( $( $ty:ty ),+ ) : |$value:ident, $rv:ident| $block:expr_2021) => {
    $(
      impl <'a> RustToV8RetVal<'a> for $ty {
        #[inline(always)]
        fn to_v8_rv(self, rv: &mut v8::ReturnValue<'a>) {
          let $value = self;
          let $rv = rv;
          $block
        }
      }
    )+
  };
  ($ty:ty : |$rv:ident, $scope:ident| $block:expr_2021) => {
    to_v8_retval!(( $ty ) : |$rv, $scope| $block);
  };
}

/// Helper macro for [`RustToV8Fallible`] to reduce boilerplate.
///
/// Implements a Rust-to-v8 conversion that can fail. Multiple types may be specified
/// to apply the same implementation.
macro_rules! to_v8_fallible {
  (( $( $ty:ty ),+ ) : |$value:ident, $scope:ident| $block:expr_2021) => {
    $(
      #[allow(clippy::needless_borrow, reason = "borrow may be needed depending on macro instantiation type")]
      impl <'a> RustToV8Fallible<'a> for $ty {
        #[inline(always)]
        fn to_v8_fallible<'i>(self, scope: &mut v8::PinScope<'a, 'i>) -> serde_v8::Result<v8::Local<'a, v8::Value>> {
          let $value = self;
          let $scope = scope;
          let res = $block;
          match res {
            Ok(v) => Ok(v.into()),
            Err(err) => Err(err),
          }
        }
      }
    )+
  };
  ($ty:ty : |$value:ident, $scope:ident| $block:expr_2021) => {
    to_v8_fallible!(( $ty ) : |$value, $scope| $block);
  };
}

//
// Integer/primitive conversions (cheap) — subset: the matrix's retval faces
//

to_v8!(bool: |value, scope| v8::Boolean::new(scope, value as _));
to_v8_retval!(bool: |value, rv| rv.set_bool(value));
to_v8!((f32, f64): |value, scope| v8::Number::new(scope, value as _));
to_v8_retval!((f32, f64): |value, rv| rv.set_double(value as _));
to_v8!((i8, i16, i32): |value, scope| v8::Integer::new(scope, value as _));
to_v8_retval!((i8, i16, i32): |value, rv| rv.set_int32(value as _));

//
// Strings
//

to_v8_fallible!((String, Cow<'a, str>, &'a str): |value, scope| v8::String::new(scope, &value).ok_or_else(|| serde_v8::Error::Message("failed to allocate string; buffer exceeds maximum length".into())));

//
// Buffers — subset: u8 only (the matrix's Vec<u8> #[buffer] face)
//

to_v8!(RustToV8Marker<ArrayBufferMarker, serde_v8::V8Slice<u8>>: |value, scope| {
  value.0.into_v8_unsliced_arraybuffer_local(scope)
});

to_v8_fallible!(serde_v8::V8Slice<u8>: |value, scope| {
  value.into_v8_local(scope).ok_or_else(|| serde_v8::Error::Message("failed to allocate array".into()))
});

macro_rules! typedarray {
  ($rty:ty, $v8ty:ident) => {
    to_v8!(RustToV8Marker<ArrayBufferMarker, Box<[$rty]>>: |buf, scope| {
      let buf = buf.0;
      if buf.is_empty() {
        v8::ArrayBuffer::new(scope, 0)
      } else {
        let backing_store =
          v8::ArrayBuffer::new_backing_store_from_bytes(buf);
        let backing_store_shared = backing_store.make_shared();
        v8::ArrayBuffer::with_backing_store(scope, &backing_store_shared)
      }
    });
    to_v8_fallible!(Box<[$rty]>: |buf, scope| {
      let len = buf.len();
      let ab = unsafe {
        v8::Local::cast_unchecked(RustToV8Marker::<ArrayBufferMarker, _>::from(buf).to_v8(scope))
      };
      v8::$v8ty::new(scope, ab, 0, len).ok_or_else(|| serde_v8::Error::Message("failed to allocate array".into()))
    });
    to_v8!(RustToV8Marker<ArrayBufferMarker, Vec<$rty>>: |value, scope| {
      RustToV8Marker::<ArrayBufferMarker, _>::from(value.0.into_boxed_slice()).to_v8(scope)
    });
    to_v8_fallible!(Vec<$rty>: |value, scope| value.into_boxed_slice().to_v8_fallible(scope));
  }
}

typedarray!(u8, Uint8Array);

//
// Serde
//

impl<'a, T: serde::Serialize> RustToV8Fallible<'a> for RustToV8Marker<SerdeMarker, T> {
    #[inline(always)]
    fn to_v8_fallible<'i>(
        self,
        scope: &mut v8::PinScope<'a, 'i>,
    ) -> serde_v8::Result<v8::Local<'a, v8::Value>> {
        serde_v8::to_v8(scope, self.0)
    }
}
