// Copyright © 2026 JetBrains s.r.o.
// SPDX-License-Identifier: Apache-2.0

fn main() -> Result<(), Box<dyn std::error::Error>> {
    himark_winit::run(himark_winit::Options::from_env())
}
