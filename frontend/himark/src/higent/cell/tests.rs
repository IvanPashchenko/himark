// Copyright © 2026 JetBrains s.r.o.
// SPDX-License-Identifier: Apache-2.0

use super::*;

fn resolved(before: &str, after: &str) -> Cell {
    let mut store = Store::new();
    let ui = UiCtx::new();
    let (mut cell, _) = Cell::pending_diff(
        &store,
        DiffHeader {
            title: "sample.md".to_owned(),
            added: Some(1),
            removed: Some(1),
        },
        640.0,
    );
    let mut batch = imba::effect::Batch::new();
    cell.perform(
        &mut store,
        &ui,
        CellCommand::ResolveDiff(Ok(crate::higent::FileEditContents {
            before: Some(before.to_owned()),
            after: Some(after.to_owned()),
        })),
        &mut batch.effects(),
    );
    cell
}

#[test]
fn a_resolved_edit_lands_as_an_inline_diff_with_folds() {
    let mut before = String::new();
    let mut after = String::new();
    for line in 0..40 {
        let text = format!("line {line:02} of the quiet unchanged context\n");
        before.push_str(&text);
        after.push_str(&text);
    }
    before.push_str("old tail\n");
    after.push_str("new tail\n");
    let cell = resolved(&before, &after);

    let CellBody::Diff { view, .. } = &cell.body else {
        panic!("the resolve lands the diff face");
    };
    assert_eq!(view.layout, crate::DiffLayout::Inline, "inline from birth");
    let inline = view.inline_editor.expect("the inline face is minted");

    // The forty untouched lines hide behind a fold strip...
    let (_, right_marks) = view.split.state.mark_markups();
    let extras = view
        .split
        .right
        .document
        .feature_markup(right_marks)
        .expect("the pane extras ride the document");
    assert!(
        !extras.all_inlays_in(0..u32::MAX).is_empty(),
        "a fold strip stands in the unchanged context"
    );
    // ...and the folded inline face is far shorter than the text.
    let folded = view.split.right.document.content_height(inline);
    let rows = view
        .split
        .right
        .document
        .content_height(view.split.right.editor)
        / 41.0;
    assert!(
        folded < rows * 20.0,
        "the context is collapsed: {folded} vs {rows} per row"
    );

    // THE diff markup washes the rewritten tail.
    let hunks = view
        .split
        .right
        .document
        .feature_markup(view.split.state.hunk_markup_oracle())
        .expect("THE diff markup rides the document");
    assert!(
        !crate::set_diff(None, hunks).is_empty(),
        "the hunk is washed"
    );

    let (_, oracle) = cell.oracle();
    assert!(oracle.contains("new tail"), "the after side shows");
}
