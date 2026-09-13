// Copyright © 2026 JetBrains s.r.o.
// SPDX-License-Identifier: Apache-2.0

use std::any::{Any, TypeId};
use std::cell::RefCell;

pub struct UiCtx {
    slots: RefCell<Vec<(TypeId, Box<dyn Any>)>>,
}

impl UiCtx {
    /// A COLD context: every env slot downstream (typefaces, font
    /// collections, shapers) starts empty and pays its full
    /// resolution cost on first use. Mint one per UI thread or effect
    /// handler and KEEP it — a ctx minted per row or per frame is the
    /// classic cold-cache bug.
    pub fn cold() -> Self {
        Self {
            slots: RefCell::new(Vec::new()),
        }
    }

    pub fn set<T: 'static>(&self, value: T) {
        if self.get::<T>().is_none() {
            self.slots
                .borrow_mut()
                .push((TypeId::of::<T>(), Box::new(value)));
        }
    }

    pub fn get<T: 'static>(&self) -> Option<&T> {
        let slots = self.slots.borrow();
        for (id, slot) in slots.iter() {
            if *id == TypeId::of::<T>() {
                let reference: &T = slot.downcast_ref::<T>().expect("typeid matched");
                let pointer: *const T = reference;
                return Some(unsafe { &*pointer });
            }
        }
        None
    }

    pub fn env<T: 'static>(&self, init: impl FnOnce() -> T) -> &T {
        if let Some(value) = self.get::<T>() {
            return value;
        }
        self.set(init());
        self.get::<T>().expect("just set")
    }
}
