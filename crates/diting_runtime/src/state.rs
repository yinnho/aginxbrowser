//! # state — our own op state holder
//!
//! `GothamState` 逐字拷贝自 deno_core 0.411 `gotham_state.rs`（MIT，
//! Copyright 2018-2026 the Deno authors；其本身 fork 自 Gotham 项目，
//! Copyright 2017 Gotham Project Developers, MIT）——同一出处链，原样保
//! 留。认领战役方法论：上游 N 轮修过的坑跟代码走，不重新发明。
//!
//! [`OpState`] 是我们自己的薄壳：deno_core 的 OpState 还挂着
//! resource_table/waker/unrefed 族（我们不用），这里只留类型地图本体。
//! API 面（put/borrow/borrow_mut/try_*）与 deno_core::OpState 对齐，
//! op 体里 `state.borrow::<T>()` 的写法零改动。

use std::any::type_name;
use std::any::Any;
use std::any::TypeId;
use std::collections::BTreeMap;

#[derive(Default)]
pub struct GothamState {
    data: BTreeMap<TypeId, Box<dyn Any>>,
}

// `borrow`/`borrow_mut` shadow the std trait names on purpose — this IS the
// gotham API face (`state.borrow::<T>()`), not a Borrow impl.
#[allow(clippy::should_implement_trait)]
impl GothamState {
    /// Puts a value into the `GothamState` storage. One value of each type is retained.
    /// Successive calls to `put` will overwrite the existing value of the same
    /// type.
    pub fn put<T: 'static>(&mut self, t: T) {
        let type_id = TypeId::of::<T>();
        self.data.insert(type_id, Box::new(t));
    }

    /// Determines if the current value exists in `GothamState` storage.
    pub fn has<T: 'static>(&self) -> bool {
        let type_id = TypeId::of::<T>();
        self.data.contains_key(&type_id)
    }

    /// Tries to borrow a value from the `GothamState` storage.
    pub fn try_borrow<T: 'static>(&self) -> Option<&T> {
        let type_id = TypeId::of::<T>();
        self.data.get(&type_id).and_then(|b| b.downcast_ref())
    }

    /// Borrows a value from the `GothamState` storage.
    pub fn borrow<T: 'static>(&self) -> &T {
        self.try_borrow().unwrap_or_else(|| missing::<T>())
    }

    /// Tries to mutably borrow a value from the `GothamState` storage.
    pub fn try_borrow_mut<T: 'static>(&mut self) -> Option<&mut T> {
        let type_id = TypeId::of::<T>();
        self.data.get_mut(&type_id).and_then(|b| b.downcast_mut())
    }

    /// Mutably borrows a value from the `GothamState` storage.
    pub fn borrow_mut<T: 'static>(&mut self) -> &mut T {
        self.try_borrow_mut().unwrap_or_else(|| missing::<T>())
    }

    /// Tries to move a value out of the `GothamState` storage and return ownership.
    pub fn try_take<T: 'static>(&mut self) -> Option<T> {
        let type_id = TypeId::of::<T>();
        self.data
            .remove(&type_id)
            .and_then(|b| b.downcast().ok())
            .map(|b| *b)
    }
}

fn missing<T: 'static>() -> ! {
    panic!(
        "The requested state value does not exist: {}",
        type_name::<T>()
    );
}

/// The op state bag: one value per type, shared by every op as
/// `Rc<RefCell<OpState>>` (the same shape deno_core uses, so op bodies and
/// `op_state().borrow_mut().put(..)` host calls port unchanged).
#[derive(Default)]
pub struct OpState {
    gotham_state: GothamState,
}

// Same trait-name shadow as GothamState above — the API face op bodies use.
#[allow(clippy::should_implement_trait)]
impl OpState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn put<T: 'static>(&mut self, t: T) {
        self.gotham_state.put(t);
    }

    pub fn has<T: 'static>(&self) -> bool {
        self.gotham_state.has::<T>()
    }

    pub fn try_borrow<T: 'static>(&self) -> Option<&T> {
        self.gotham_state.try_borrow()
    }

    pub fn borrow<T: 'static>(&self) -> &T {
        self.gotham_state.borrow()
    }

    pub fn try_borrow_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.gotham_state.try_borrow_mut()
    }

    pub fn borrow_mut<T: 'static>(&mut self) -> &mut T {
        self.gotham_state.borrow_mut()
    }

    pub fn try_take<T: 'static>(&mut self) -> Option<T> {
        self.gotham_state.try_take()
    }
}
