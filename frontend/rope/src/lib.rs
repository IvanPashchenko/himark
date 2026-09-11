// Copyright © 2026 JetBrains s.r.o.
// SPDX-License-Identifier: Apache-2.0

mod cursor;
mod metrics;
mod node;
mod rope;
mod siblings;
mod zipper;

pub use crate::cursor::{Cursor, CursorIter, SeekMode};
pub use crate::metrics::{Measure, MetricId, Metrics};
pub use crate::rope::{from_leaves_with_branching, Rope};

#[cfg(test)]
mod tests;
