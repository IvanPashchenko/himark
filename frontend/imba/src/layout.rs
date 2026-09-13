// Copyright © 2026 JetBrains s.r.o.
// SPDX-License-Identifier: Apache-2.0

//! The LAYOUT stage (docs/UI.md, revision 3): the structure of a
//! subtree with the state already read — every store-derived fact
//! captured, no geometry yet. `View::display` answers one; sizing it
//! (`layout(constraints)`) answers the thunk. Placement arithmetic
//! lives HERE, as reusable values (`Column`, `Row`, … — stage 2),
//! instead of being hand-rolled inside every composite view.

use crate::arena::{self, Arena};
use crate::constraints::Constraints;
use crate::{Thunk, ThunkBox};

/// A structured, unsized subtree: state read, geometry pending.
/// Consumed by sizing (single-shot, like every frame artifact). The
/// erased return keeps one method surface for static and boxed
/// children alike — the measured default (docs/UI.md: the perf gate
/// decides whether the RPITIT form replaces it).
pub trait Layout<'a, Command> {
    fn layout(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command>
    where
        Self: Sized;
}

/// Type erasure — the same arena-box story as `ThunkBox`/`WidgetBox`:
/// a bump pointer, destructors riding the frame arena.
pub struct LayoutBox<'a, Command>(arena::ArenaBox<'a, dyn DynLayout<'a, Command> + 'a>);

impl<'a, Command: 'a> LayoutBox<'a, Command> {
    pub fn new<L: Layout<'a, Command> + 'a>(arena: &'a Arena, layout: L) -> Self {
        let slot = arena.boxed(Slot(Some(layout)));
        let raw: *mut Slot<L> = arena::ArenaBox::into_raw(slot);

        Self(unsafe { arena::ArenaBox::from_raw(raw as *mut (dyn DynLayout<'a, Command> + 'a)) })
    }
}

impl<'a, Command: 'a> Layout<'a, Command> for LayoutBox<'a, Command> {
    fn layout(mut self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command> {
        self.0.layout_dyn(arena, constraints)
    }
}

trait DynLayout<'a, Command> {
    fn layout_dyn(&mut self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command>;
}

struct Slot<L>(Option<L>);

impl<'a, Command: 'a, L: Layout<'a, Command>> DynLayout<'a, Command> for Slot<L> {
    fn layout_dyn(&mut self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command> {
        self.0
            .take()
            .expect("laid out twice")
            .layout(arena, constraints)
    }
}

/// The migration shim (docs/UI.md): lifts a sizing closure — an old
/// `View::layout` body, verbatim — into a `Layout`. Stage-2 views
/// return stock layouts instead; a `laid` at a call site marks
/// placement arithmetic not yet extracted.
pub fn laid<F>(sizing: F) -> Laid<F> {
    Laid(sizing)
}

pub struct Laid<F>(F);

impl<'a, Command: 'a, T, F> Layout<'a, Command> for Laid<F>
where
    T: Thunk<'a, Command> + 'a,
    F: FnOnce(&'a Arena, Constraints) -> T,
{
    fn layout(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command> {
        ThunkBox::new(arena, (self.0)(arena, constraints))
    }
}
