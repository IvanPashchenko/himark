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

// ---- primitives (docs/UI.md stage 2) --------------------------------
//
// Jetpack-Compose-shaped, deliberately: `Column`/`Row` with
// `Arrangement`-style gaps, cross-axis `CrossAlign`, per-child
// `weight`; `Pad`/`Align`/`SizedBox` as modifier structs behind
// `LayoutExt`. Every layout is a REIFIED struct — no closures to
// squint at — so a view's `display` reads as the structure it names.

use crate::container::container;
use crate::thunk_ext::ThunkExt;
use skia_safe::Size;

/// Treat imba's conventional f32::MAX-ish bounds as "unbounded".
fn bounded(extent: f32) -> Option<f32> {
    (extent < f32::MAX / 2.0).then_some(extent)
}

/// Cross-axis placement of a stack's children (Compose's
/// `horizontalAlignment` / `verticalAlignment`).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum CrossAlign {
    #[default]
    Start,
    Center,
    End,
}

impl CrossAlign {
    fn offset(self, room: f32, child: f32) -> f32 {
        match self {
            CrossAlign::Start => 0.0,
            CrossAlign::Center => ((room - child) * 0.5).max(0.0),
            CrossAlign::End => (room - child).max(0.0),
        }
    }
}

struct StackChild<'a, Command> {
    layout: LayoutBox<'a, Command>,
    weight: Option<f32>,
}

/// The one flex algorithm, parameterized by axis (Column = vertical).
/// Unweighted children measure first, in order, against the space
/// still free on the main axis; weighted children then split the
/// leftover proportionally with TIGHT main-axis constraints —
/// weights need a bounded main axis to mean anything (an unbounded
/// stack gives them zero, like Compose forbids). The cross extent is
/// the widest child, clamped into the incoming constraints.
struct Stack<'a, Command> {
    arena: &'a Arena,
    children: Vec<StackChild<'a, Command>>,
    gap: f32,
    cross: CrossAlign,
    horizontal: bool,
}

impl<'a, Command: 'a> Stack<'a, Command> {
    fn lay(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command> {
        let (main_max, cross_max) = match self.horizontal {
            true => (constraints.max.width, constraints.max.height),
            false => (constraints.max.height, constraints.max.width),
        };
        let (main_min, cross_min) = match self.horizontal {
            true => (constraints.min.width, constraints.min.height),
            false => (constraints.min.height, constraints.min.width),
        };
        let gaps = self.gap * self.children.len().saturating_sub(1) as f32;
        let total_weight: f32 = self.children.iter().filter_map(|child| child.weight).sum();

        let horizontal = self.horizontal;
        let child_constraints = |main: Option<f32>, tight: bool| -> Constraints {
            let main = main.unwrap_or(f32::MAX);
            let (width, height) = match horizontal {
                true => (main, cross_max),
                false => (cross_max, main),
            };
            let min = match tight {
                true => match horizontal {
                    true => Size::new(main, 0.0),
                    false => Size::new(0.0, main),
                },
                false => Size::default(),
            };
            Constraints {
                min,
                max: Size::new(width, height),
            }
        };

        let mut thunks: Vec<Option<ThunkBox<'a, Command>>> =
            self.children.iter().map(|_| None).collect();
        // Measure passes need ownership of the children's layouts;
        // both passes run over a drained vec keyed by index.
        let mut children = self.children;
        let mut order: Vec<(usize, Option<f32>)> = children
            .iter()
            .enumerate()
            .map(|(index, child)| (index, child.weight))
            .collect();
        // unweighted first (in order), then weighted (in order)
        order.sort_by_key(|(index, weight)| (weight.is_some(), *index));
        let mut used = 0.0f32;
        let mut main_sizes = vec![0.0f32; children.len()];
        let mut cross_sizes = vec![0.0f32; children.len()];
        let leftover_at = |used: f32| bounded(main_max).map(|max| (max - gaps - used).max(0.0));
        let mut leftover_for_weights = 0.0f32;
        let mut weighted_started = false;
        for (index, weight) in order {
            let slot = std::mem::replace(&mut children[index].layout, LayoutBox::tombstone(arena));
            let thunk = match weight {
                None => slot.layout(arena, child_constraints(leftover_at(used), false)),
                Some(weight) => {
                    if !weighted_started {
                        leftover_for_weights = leftover_at(used).unwrap_or(0.0);
                        weighted_started = true;
                    }
                    let share = match total_weight > 0.0 {
                        true => leftover_for_weights * weight / total_weight,
                        false => 0.0,
                    };
                    slot.layout(arena, child_constraints(Some(share), true))
                }
            };
            let size = thunk.size();
            let (main, cross) = match self.horizontal {
                true => (size.width, size.height),
                false => (size.height, size.width),
            };
            if weight.is_none() {
                used += main;
            }
            main_sizes[index] = main;
            cross_sizes[index] = cross;
            thunks[index] = Some(thunk);
        }

        let content_main: f32 = main_sizes.iter().sum::<f32>() + gaps;
        let main_extent = match total_weight > 0.0 {
            true => bounded(main_max).unwrap_or(content_main).max(main_min),
            false => content_main.max(main_min).min(main_max),
        };
        let cross_extent = cross_sizes
            .iter()
            .fold(0.0f32, |widest, cross| widest.max(*cross))
            .max(cross_min)
            .min(cross_max);

        let size = match self.horizontal {
            true => Size::new(main_extent, cross_extent),
            false => Size::new(cross_extent, main_extent),
        };
        let mut frame = container(self.arena, size);
        let mut at = 0.0f32;
        for (index, thunk) in thunks.into_iter().enumerate() {
            let Some(thunk) = thunk else { continue };
            let along = self.cross.offset(cross_extent, cross_sizes[index]);
            let (x, y) = match self.horizontal {
                true => (at, along),
                false => (along, at),
            };
            at += main_sizes[index] + self.gap;
            frame.place_boxed(x, y, thunk);
        }
        ThunkBox::new(arena, frame)
    }
}

impl<'a, Command: 'a> LayoutBox<'a, Command> {
    /// A zero-size placeholder for slots being drained during a
    /// measure pass — never laid out.
    fn tombstone(arena: &'a Arena) -> Self {
        LayoutBox::new(arena, Fixed(crate::leaf::leaf::<Command>(0.0, 0.0)))
    }
}

/// Compose's `Column`: children stack top-to-bottom; `gap` is
/// `Arrangement.spacedBy`; `weight` children split the leftover
/// height; `align_items` is `horizontalAlignment`.
pub struct Column<'a, Command> {
    stack: Stack<'a, Command>,
}

impl<'a, Command: 'a> Column<'a, Command> {
    pub fn new(arena: &'a Arena) -> Self {
        Self {
            stack: Stack {
                arena,
                children: Vec::new(),
                gap: 0.0,
                cross: CrossAlign::Start,
                horizontal: false,
            },
        }
    }

    pub fn gap(mut self, gap: f32) -> Self {
        self.stack.gap = gap;
        self
    }

    pub fn align_items(mut self, cross: CrossAlign) -> Self {
        self.stack.cross = cross;
        self
    }

    pub fn child(mut self, child: impl Layout<'a, Command> + 'a) -> Self {
        self.stack.children.push(StackChild {
            layout: LayoutBox::new(self.stack.arena, child),
            weight: None,
        });
        self
    }

    pub fn weighted(mut self, weight: f32, child: impl Layout<'a, Command> + 'a) -> Self {
        self.stack.children.push(StackChild {
            layout: LayoutBox::new(self.stack.arena, child),
            weight: Some(weight.max(0.0)),
        });
        self
    }
}

impl<'a, Command: 'a> Layout<'a, Command> for Column<'a, Command> {
    fn layout(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command> {
        self.stack.lay(arena, constraints)
    }
}

/// Compose's `Row`: left-to-right; `align_items` is
/// `verticalAlignment`.
pub struct Row<'a, Command> {
    stack: Stack<'a, Command>,
}

impl<'a, Command: 'a> Row<'a, Command> {
    pub fn new(arena: &'a Arena) -> Self {
        Self {
            stack: Stack {
                arena,
                children: Vec::new(),
                gap: 0.0,
                cross: CrossAlign::Start,
                horizontal: true,
            },
        }
    }

    pub fn gap(mut self, gap: f32) -> Self {
        self.stack.gap = gap;
        self
    }

    pub fn align_items(mut self, cross: CrossAlign) -> Self {
        self.stack.cross = cross;
        self
    }

    pub fn child(mut self, child: impl Layout<'a, Command> + 'a) -> Self {
        self.stack.children.push(StackChild {
            layout: LayoutBox::new(self.stack.arena, child),
            weight: None,
        });
        self
    }

    pub fn weighted(mut self, weight: f32, child: impl Layout<'a, Command> + 'a) -> Self {
        self.stack.children.push(StackChild {
            layout: LayoutBox::new(self.stack.arena, child),
            weight: Some(weight.max(0.0)),
        });
        self
    }
}

impl<'a, Command: 'a> Layout<'a, Command> for Row<'a, Command> {
    fn layout(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command> {
        self.stack.lay(arena, constraints)
    }
}

/// Compose's `Spacer`/`fillMaxSize`: an empty box taking the whole
/// incoming bound — the blank panel, the flexible gap.
pub struct Fill<Command>(std::marker::PhantomData<fn() -> Command>);

impl<Command> Fill<Command> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(std::marker::PhantomData)
    }
}

impl<'a, Command: 'a> Layout<'a, Command> for Fill<Command> {
    fn layout(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command> {
        ThunkBox::new(
            arena,
            crate::leaf::leaf::<Command>(constraints.max.width, constraints.max.height),
        )
    }
}

/// Lifts a THUNK into a layout that ignores the incoming constraints
/// — the adapter for content whose size is its own fact (a `leaf`, a
/// measured widget).
pub struct Fixed<T>(pub T);

pub fn fixed<T>(thunk: T) -> Fixed<T> {
    Fixed(thunk)
}

impl<'a, Command: 'a, T: Thunk<'a, Command> + 'a> Layout<'a, Command> for Fixed<T> {
    fn layout(self, arena: &'a Arena, _constraints: Constraints) -> ThunkBox<'a, Command> {
        ThunkBox::new(arena, self.0)
    }
}

#[derive(Clone, Copy, Default)]
pub struct Insets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

impl Insets {
    pub fn all(value: f32) -> Self {
        Self {
            left: value,
            top: value,
            right: value,
            bottom: value,
        }
    }

    pub fn xy(x: f32, y: f32) -> Self {
        Self {
            left: x,
            top: y,
            right: x,
            bottom: y,
        }
    }
}

/// Compose's `Modifier.padding`: deflates the constraints, offsets
/// the child.
pub struct Pad<L> {
    inner: L,
    insets: Insets,
}

impl<'a, Command: 'a, L: Layout<'a, Command> + 'a> Layout<'a, Command> for Pad<L> {
    fn layout(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command> {
        let x = self.insets.left + self.insets.right;
        let y = self.insets.top + self.insets.bottom;
        let deflated = Constraints {
            min: Size::new(
                (constraints.min.width - x).max(0.0),
                (constraints.min.height - y).max(0.0),
            ),
            max: Size::new(
                (constraints.max.width - x).max(0.0),
                (constraints.max.height - y).max(0.0),
            ),
        };
        let child = self.inner.layout(arena, deflated);
        let size = child.size();
        let mut frame = container(arena, Size::new(size.width + x, size.height + y));
        frame.place_boxed(self.insets.left, self.insets.top, child);
        ThunkBox::new(arena, frame)
    }
}

/// Compose's 9-point `Alignment` for a child inside the incoming max
/// bounds (a one-child `Box(contentAlignment = …)`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Alignment {
    TopStart,
    TopCenter,
    TopEnd,
    CenterStart,
    Center,
    CenterEnd,
    BottomStart,
    BottomCenter,
    BottomEnd,
}

impl Alignment {
    fn place(self, room: Size, child: Size) -> (f32, f32) {
        let x = match self {
            Alignment::TopStart | Alignment::CenterStart | Alignment::BottomStart => 0.0,
            Alignment::TopCenter | Alignment::Center | Alignment::BottomCenter => {
                ((room.width - child.width) * 0.5).max(0.0)
            }
            Alignment::TopEnd | Alignment::CenterEnd | Alignment::BottomEnd => {
                (room.width - child.width).max(0.0)
            }
        };
        let y = match self {
            Alignment::TopStart | Alignment::TopCenter | Alignment::TopEnd => 0.0,
            Alignment::CenterStart | Alignment::Center | Alignment::CenterEnd => {
                ((room.height - child.height) * 0.5).max(0.0)
            }
            Alignment::BottomStart | Alignment::BottomCenter | Alignment::BottomEnd => {
                (room.height - child.height).max(0.0)
            }
        };
        (x, y)
    }
}

pub struct Align<L> {
    inner: L,
    alignment: Alignment,
}

impl<'a, Command: 'a, L: Layout<'a, Command> + 'a> Layout<'a, Command> for Align<L> {
    fn layout(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command> {
        let child = self.inner.layout(arena, constraints.loosen());
        let size = child.size();
        let room = Size::new(
            match bounded(constraints.max.width) {
                Some(width) => width,
                None => size.width.max(constraints.min.width),
            },
            match bounded(constraints.max.height) {
                Some(height) => height,
                None => size.height.max(constraints.min.height),
            },
        );
        let (x, y) = self.alignment.place(room, size);
        let mut frame = container(arena, room);
        frame.place_boxed(x, y, child);
        ThunkBox::new(arena, frame)
    }
}

/// Compose's `Modifier.size`/`width`/`height`: tightens the given
/// axes; `f32::NAN` leaves an axis as it came.
pub struct SizedBox<L> {
    inner: L,
    width: f32,
    height: f32,
}

impl<'a, Command: 'a, L: Layout<'a, Command> + 'a> Layout<'a, Command> for SizedBox<L> {
    fn layout(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command> {
        let tightened = Constraints {
            min: Size::new(
                if self.width.is_nan() {
                    constraints.min.width
                } else {
                    self.width
                },
                if self.height.is_nan() {
                    constraints.min.height
                } else {
                    self.height
                },
            ),
            max: Size::new(
                if self.width.is_nan() {
                    constraints.max.width
                } else {
                    self.width
                },
                if self.height.is_nan() {
                    constraints.max.height
                } else {
                    self.height
                },
            ),
        };
        self.inner.layout(arena, tightened)
    }
}

/// The command boundary, one stage above `ThunkExt::map`.
pub struct MapLayout<L, F, Child> {
    inner: L,
    wrap: F,
    _child: std::marker::PhantomData<fn() -> Child>,
}

impl<'a, Child: 'a, Parent: 'a, L, F> Layout<'a, Parent> for MapLayout<L, F, Child>
where
    L: Layout<'a, Child> + 'a,
    F: Fn(Child) -> Parent + Clone + 'a,
{
    fn layout(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Parent> {
        ThunkBox::new(arena, self.inner.layout(arena, constraints).map(self.wrap))
    }
}

/// The modifier surface (Compose's `Modifier`, curried onto the
/// layout value itself).
pub trait LayoutExt<'a, Command: 'a>: Layout<'a, Command> + Sized + 'a {
    fn pad(self, all: f32) -> Pad<Self> {
        self.pad_insets(Insets::all(all))
    }

    fn pad_xy(self, x: f32, y: f32) -> Pad<Self> {
        self.pad_insets(Insets::xy(x, y))
    }

    fn pad_insets(self, insets: Insets) -> Pad<Self> {
        Pad {
            inner: self,
            insets,
        }
    }

    fn align(self, alignment: Alignment) -> Align<Self> {
        Align {
            inner: self,
            alignment,
        }
    }

    fn width(self, width: f32) -> SizedBox<Self> {
        SizedBox {
            inner: self,
            width,
            height: f32::NAN,
        }
    }

    fn height(self, height: f32) -> SizedBox<Self> {
        SizedBox {
            inner: self,
            width: f32::NAN,
            height,
        }
    }

    fn sized(self, width: f32, height: f32) -> SizedBox<Self> {
        SizedBox {
            inner: self,
            width,
            height,
        }
    }

    fn map_layout<Parent, F>(self, wrap: F) -> MapLayout<Self, F, Command>
    where
        F: Fn(Command) -> Parent + Clone + 'a,
    {
        MapLayout {
            inner: self,
            wrap,
            _child: std::marker::PhantomData,
        }
    }
}

impl<'a, Command: 'a, L: Layout<'a, Command> + Sized + 'a> LayoutExt<'a, Command> for L {}

/// Compose's `Text`, single-line: measures itself from the font's
/// metrics and paints its own glyphs — labels stop being hand-rolled
/// `draw_str` closures inside leaves. Wider text than the incoming
/// bound clips (the container's clip); position it with `.align()` /
/// `Pad` like any layout. (Wrapping and ellipsis are later verses —
/// paragraphs belong to the editor.)
pub struct Text {
    text: String,
    font: skia_safe::Font,
    color: skia_safe::Color,
    tracking: f32,
}

pub fn text(content: impl Into<String>, font: skia_safe::Font, color: skia_safe::Color) -> Text {
    Text {
        text: content.into(),
        font,
        color,
        tracking: 0.0,
    }
}

impl Text {
    /// Extra per-glyph advance — the caps-label look several chrome
    /// labels hand-roll today.
    pub fn tracking(mut self, tracking: f32) -> Self {
        self.tracking = tracking;
        self
    }
}

impl<'a, Command: 'a> Layout<'a, Command> for Text {
    fn layout(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, Command> {
        let (_, metrics) = self.font.metrics();
        let ascent = -metrics.ascent;
        let height = (ascent + metrics.descent).ceil().max(1.0);
        let advance = match self.tracking == 0.0 {
            true => self.font.measure_str(&self.text, None).0,
            false => self
                .text
                .chars()
                .map(|ch| self.font.measure_str(ch.to_string(), None).0 + self.tracking)
                .sum(),
        };
        let width = advance
            .min(constraints.max.width)
            .max(constraints.min.width);
        let Text {
            text,
            font,
            color,
            tracking,
        } = self;
        let label = crate::leaf::leaf::<Command>(width, height).paint_instead(
            move |_arena, canvas, rect| {
                let mut paint = skia_safe::Paint::default();
                paint.set_anti_alias(true);
                paint.set_color(color);
                let baseline = rect.top + ascent;
                if tracking == 0.0 {
                    canvas.draw_str(&text, (rect.left, baseline), &font, &paint);
                } else {
                    let mut x = rect.left;
                    for ch in text.chars() {
                        let glyph = ch.to_string();
                        canvas.draw_str(&glyph, (x, baseline), &font, &paint);
                        x += font.measure_str(&glyph, None).0 + tracking;
                    }
                }
            },
        );
        ThunkBox::new(arena, label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::leaf::leaf;

    fn sized(width: f32, height: f32) -> impl for<'a> Layout<'a, ()> {
        SizedProbe { width, height }
    }

    struct SizedProbe {
        width: f32,
        height: f32,
    }

    impl<'a> Layout<'a, ()> for SizedProbe {
        fn layout(self, arena: &'a Arena, _constraints: Constraints) -> ThunkBox<'a, ()> {
            ThunkBox::new(arena, leaf::<()>(self.width, self.height))
        }
    }

    fn arena() -> Arena {
        Arena::default()
    }

    #[test]
    fn a_column_stacks_gaps_and_reports_the_widest_child() {
        let arena = arena();
        let thunk = Column::new(&arena)
            .gap(4.0)
            .child(sized(30.0, 10.0))
            .child(sized(50.0, 20.0))
            .child(sized(20.0, 5.0))
            .layout(
                &arena,
                Constraints {
                    min: Size::default(),
                    max: Size::new(400.0, 400.0),
                },
            );
        let size = thunk.size();
        assert_eq!(
            (size.width, size.height),
            (50.0, 10.0 + 4.0 + 20.0 + 4.0 + 5.0)
        );
    }

    #[test]
    fn weighted_children_split_the_leftover_and_fill_the_axis() {
        use std::cell::RefCell;
        use std::rc::Rc;

        struct Probe {
            heights: Rc<RefCell<Vec<f32>>>,
        }
        impl<'a> Layout<'a, ()> for Probe {
            fn layout(self, arena: &'a Arena, constraints: Constraints) -> ThunkBox<'a, ()> {
                self.heights.borrow_mut().push(constraints.min.height);
                ThunkBox::new(arena, leaf::<()>(10.0, constraints.min.height))
            }
        }

        let arena = arena();
        let heights = Rc::new(RefCell::new(Vec::new()));
        let thunk = Column::new(&arena)
            .child(sized(10.0, 40.0))
            .weighted(
                1.0,
                Probe {
                    heights: Rc::clone(&heights),
                },
            )
            .weighted(
                3.0,
                Probe {
                    heights: Rc::clone(&heights),
                },
            )
            .layout(
                &arena,
                Constraints {
                    min: Size::default(),
                    max: Size::new(100.0, 240.0),
                },
            );
        // 200 leftover after the 40px child: weights 1:3 → 50 and 150.
        assert_eq!(*heights.borrow(), vec![50.0, 150.0]);
        assert_eq!(
            thunk.size().height,
            240.0,
            "a weighted column fills its axis"
        );
    }

    #[test]
    fn a_row_mirrors_the_axes() {
        let arena = arena();
        let thunk = Row::new(&arena)
            .gap(2.0)
            .child(sized(10.0, 30.0))
            .child(sized(20.0, 10.0))
            .layout(
                &arena,
                Constraints {
                    min: Size::default(),
                    max: Size::new(400.0, 400.0),
                },
            );
        let size = thunk.size();
        assert_eq!((size.width, size.height), (32.0, 30.0));
    }

    #[test]
    fn pad_inflates_the_child_and_align_centers_it() {
        let arena = arena();
        let padded = sized(10.0, 10.0).pad_xy(6.0, 2.0).layout(
            &arena,
            Constraints {
                min: Size::default(),
                max: Size::new(100.0, 100.0),
            },
        );
        assert_eq!((padded.size().width, padded.size().height), (22.0, 14.0));

        let aligned = sized(10.0, 10.0).align(Alignment::Center).layout(
            &arena,
            Constraints {
                min: Size::default(),
                max: Size::new(100.0, 50.0),
            },
        );
        assert_eq!((aligned.size().width, aligned.size().height), (100.0, 50.0));
    }

    #[test]
    fn sized_box_tightens_one_axis() {
        let arena = arena();
        let thunk = Column::new(&arena)
            .weighted(1.0, sized(10.0, 0.0))
            .height(80.0)
            .layout(
                &arena,
                Constraints {
                    min: Size::default(),
                    max: Size::new(100.0, f32::MAX),
                },
            );
        assert_eq!(thunk.size().height, 80.0);
    }
}
