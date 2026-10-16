//! # diting_runtime — the JS runtime layer we own
//!
//! deno_core 退役战役的核心 crate（docs/diting_runtime认领计划_20261017.md）。
//! 引擎本体（`v8` crate）不重写、版本钉死；这一层——事件循环、op 结算管线、
//! promise 表——从租用改为自有，逐模块认领，行为代码自己写（只学不 fork）。
//!
//! ## 为什么是这个 crate
//!
//! #226/#39/#240 未投递族的凶器在 deno_core `runtime/jsruntime.rs`
//! `dispatch_event_loop_tick`：completed-op 三元组从 FIFO pop 出来之后、
//! 投给 `__eventLoopTick` 之前/之中被打断（terminate 命中、tick 抛异常），
//! 三元组就地蒸发，零重投递。V8 对这层零立法（那是 deno_core 的私产传输
//! 层），cancel 之后重投完全合法——但 deno_core 不做，我们只能自己做。
//!
//! JS 侧已先遣落了半边（`crates/diting/js/core_base.js` 的 replay 停泊栈，
//! 同一条法）：tick 已开跑才被打断的窗口，尾巴不再蒸发。剩 Rust 侧窗口
//! （三元组 pop 了、tick 压根没被调）只有本 crate 拿下 dispatch 才闭环。
//!
//! ## 认领路线（逐批，每批独立测试绿）
//!
//! 1. **settle 队列**：重投递法的 Rust 正身。纯 Rust，无 V8。
//! 2. **v8 竖切片**（`vm`）：裸 isolate + 自有 loop pass（settle 投递 +
//!    microtask 排水）+ 队列接线；terminate → 停泊 → cancel → 重投 →
//!    promise settle 的最小闭环钉测。op driver、immediates/close 相、
//!    环形表是后续批——现在建就是没有测试钉着的脚手架。
//! 3. **op driver**（`driver`，已落）：异步 op future 每轮驱动一次，
//!    完成按完成序灌进 settle 队列；裸 V8 回调（`__diting_spawn`）经
//!    isolate slot 找到 driver——Rust 侧对 terminate 免疫，恰好就是
//!    "op 在 terminate 中完成 → 停泊 → revive 落地"的 #226 钉测面。
//!    环形表自建（ring + overflow map + async op stub + resolver）、
//!    immediates/close 相（diting 现役零消费，先不建）后续批。
//! 4. 自有快照格式（[v8 data][sidecar][ulen] 三段式，sidecar 自有）。
//! 5. op 注册面（40 ops 从 `#[op2]` 迁出）+ diting 换装 + 删 deno_core。
//!
//! ## 法条（与 js/core_base.js v1 同一条法，两层各自实现）
//!
//! - 批次先停泊、完整投递才出队；投递失败整个批次停泊（前缀可能已被
//!   tick 结算过——JS 侧 Missing-absorb 层把重复投递折叠成 no-op）。
//! - 后续投递轮先排空停泊批次，oldest first；结算序即完成序。
//! - 停泊 3 次无进展 = 毒丸，丢弃并计数（`Stats`），错误照常上抛——
//!   内部 bug 必须露脸，绝不空转。
//! - terminate 打断一切 catch——停泊态就是中断现场，cancel 后下一轮
//!   poll 自动重投，无需特殊处理。

pub mod driver;
pub mod settle;
pub mod vm;

pub use driver::{AfterTicks, OpDriver};
pub use settle::{SettleQueue, Stats};
pub use vm::{LoopError, Triplet, Vm};
