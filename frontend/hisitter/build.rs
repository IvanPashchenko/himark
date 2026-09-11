// Copyright © 2026 JetBrains s.r.o.
// SPDX-License-Identifier: Apache-2.0

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("emscripten") {
        println!("cargo:rerun-if-changed=src/side_fetch.c");
        cc::Build::new()
            .file("src/side_fetch.c")
            .compile("hisitter_side_fetch");
    }
}
