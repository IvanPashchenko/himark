// Copyright © 2026 JetBrains s.r.o.
// SPDX-License-Identifier: Apache-2.0

pub struct ClipboardContent {
    pub text: String,
}

pub trait ClipboardClient {
    fn copy(&mut self) -> Option<ClipboardContent>;

    fn cut(&mut self) -> Option<ClipboardContent>;

    fn paste(&mut self, content: &ClipboardContent) -> bool;
}
