// Copyright © 2026 JetBrains s.r.o.
// SPDX-License-Identifier: Apache-2.0

//! The host-side disk reload for mirrored documents. The host is the
//! SOURCE OF TRUTH for a mirrored document: when the file changes
//! underneath (the agent edited it), the host diffs the mirror
//! against the disk — three-way against the last disk text it knew
//! when clients hold unflushed edits — and issues the result as the
//! HOST's own edit, broadcast to every subscriber. Clients never
//! reload mirrored files themselves.

/// One replacement: `start..end` bytes of the OLD text become `text`.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Hunk {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) text: String,
}

fn lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        match rest.find('\n') {
            Some(at) => {
                out.push(&rest[..=at]);
                rest = &rest[at + 1..];
            }
            None => {
                out.push(rest);
                rest = "";
            }
        }
    }
    out
}

/// The middle's cell budget: beyond it the diff degrades to one
/// opaque replacement (4MB of DP at the cap — host worker money).
const CELL_CAP: usize = 1_000_000;

/// Line-level diff as byte hunks over the OLD text. Common prefix and
/// suffix trimmed first; the middle resolved by Myers on lines, or as
/// one replacement when the edit distance blows the cap.
pub(crate) fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    let old_lines = lines(old);
    let new_lines = lines(new);

    let mut prefix = 0usize;
    while prefix < old_lines.len()
        && prefix < new_lines.len()
        && old_lines[prefix] == new_lines[prefix]
    {
        prefix += 1;
    }
    let mut suffix = 0usize;
    while suffix < old_lines.len() - prefix
        && suffix < new_lines.len() - prefix
        && old_lines[old_lines.len() - 1 - suffix] == new_lines[new_lines.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let old_mid = &old_lines[prefix..old_lines.len() - suffix];
    let new_mid = &new_lines[prefix..new_lines.len() - suffix];
    let offset: usize = old_lines[..prefix].iter().map(|line| line.len()).sum();

    let mut out = Vec::new();
    let mut old_pos = offset;
    let push = |start: usize, end: usize, text: String, out: &mut Vec<Hunk>| {
        if start == end && text.is_empty() {
            return;
        }
        // Merge adjacent hunks so a delete+insert at one seam is one
        // replacement.
        if let Some(last) = out.last_mut() {
            let Hunk {
                end: last_end,
                text: last_text,
                ..
            } = last;
            if *last_end == start {
                *last_end = end;
                last_text.push_str(&text);
                return;
            }
        }
        out.push(Hunk { start, end, text });
    };

    match lcs_steps(old_mid, new_mid) {
        Some(steps) => {
            let mut old_index = 0usize;
            let mut new_index = 0usize;
            for step in steps {
                match step {
                    Step::Keep => {
                        old_pos += old_mid[old_index].len();
                        old_index += 1;
                        new_index += 1;
                    }
                    Step::Delete => {
                        let len = old_mid[old_index].len();
                        push(old_pos, old_pos + len, String::new(), &mut out);
                        old_pos += len;
                        old_index += 1;
                    }
                    Step::Insert => {
                        push(old_pos, old_pos, new_mid[new_index].to_owned(), &mut out);
                        new_index += 1;
                    }
                }
            }
        }
        None => {
            // The edit distance blew the cap: the middle is one
            // opaque replacement.
            let old_len: usize = old_mid.iter().map(|line| line.len()).sum();
            push(offset, offset + old_len, new_mid.concat(), &mut out);
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Step {
    Keep,
    Delete,
    Insert,
}

/// LCS steps over the trimmed middle — plain dynamic programming,
/// exact and boring; `None` when the middle is too large to afford.
fn lcs_steps(old: &[&str], new: &[&str]) -> Option<Vec<Step>> {
    let n = old.len();
    let m = new.len();
    if n.saturating_mul(m) > CELL_CAP {
        return None;
    }
    let at = |i: usize, j: usize| i * (m + 1) + j;
    let mut dp = vec![0u32; (n + 1) * (m + 1)];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[at(i, j)] = match old[i] == new[j] {
                true => dp[at(i + 1, j + 1)] + 1,
                false => dp[at(i + 1, j)].max(dp[at(i, j + 1)]),
            };
        }
    }
    let mut steps = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old[i] == new[j] {
            steps.push(Step::Keep);
            i += 1;
            j += 1;
        } else if dp[at(i + 1, j)] >= dp[at(i, j + 1)] {
            steps.push(Step::Delete);
            i += 1;
        } else {
            steps.push(Step::Insert);
            j += 1;
        }
    }
    while i < n {
        steps.push(Step::Delete);
        i += 1;
    }
    while j < m {
        steps.push(Step::Insert);
        j += 1;
    }
    Some(steps)
}

/// Three-way merge of the last disk text's two descendants: the
/// mirror (ours — clients' unflushed edits) and the fresh disk
/// (theirs — the agent's write). Shared hunks land once; genuine
/// conflicts keep both sides' bytes, ours first.
pub(crate) fn merged(base: &str, ours: &str, theirs: &str) -> String {
    let mine = hunks(base, ours);
    let disk = hunks(base, theirs);
    let mut out = String::new();
    let mut pos = 0usize;
    let (mut i, mut j) = (0usize, 0usize);
    let copy_to = |out: &mut String, pos: &mut usize, to: usize| {
        if to > *pos {
            out.push_str(&base[*pos..to]);
            *pos = to;
        }
    };
    while i < mine.len() || j < disk.len() {
        let ours_next = mine.get(i);
        let theirs_next = disk.get(j);
        match (ours_next, theirs_next) {
            (Some(o), Some(t)) => {
                if o.end <= t.start && o.start != t.start {
                    copy_to(&mut out, &mut pos, o.start);
                    pos = o.end;
                    out.push_str(&o.text);
                    i += 1;
                } else if t.end <= o.start && o.start != t.start {
                    copy_to(&mut out, &mut pos, t.start);
                    pos = t.end;
                    out.push_str(&t.text);
                    j += 1;
                } else if o.start == t.start && o.end == t.end {
                    // The same base span replaced on both sides: the
                    // shared change, possibly further along on one.
                    copy_to(&mut out, &mut pos, o.start);
                    pos = o.end;
                    if t.text.starts_with(&o.text) {
                        out.push_str(&t.text);
                    } else if o.text.starts_with(&t.text) {
                        out.push_str(&o.text);
                    } else {
                        out.push_str(&o.text);
                        out.push_str(&t.text);
                    }
                    i += 1;
                    j += 1;
                } else {
                    // Messy overlap: the union span becomes ours'
                    // text then theirs' — nobody's bytes dropped.
                    let union_start = o.start.min(t.start);
                    let union_end = o.end.max(t.end);
                    copy_to(&mut out, &mut pos, union_start);
                    pos = union_end;
                    out.push_str(&o.text);
                    out.push_str(&t.text);
                    i += 1;
                    j += 1;
                }
            }
            (Some(o), None) => {
                copy_to(&mut out, &mut pos, o.start);
                pos = o.end;
                out.push_str(&o.text);
                i += 1;
            }
            (None, Some(t)) => {
                copy_to(&mut out, &mut pos, t.start);
                pos = t.end;
                out.push_str(&t.text);
                j += 1;
            }
            (None, None) => unreachable!(),
        }
    }
    out.push_str(&base[pos..]);
    out
}

/// The wire spans that turn `old` into `new`.
pub(crate) fn spans(old: &str, new: &str) -> Vec<himark_ahp_ext_types::text::Span> {
    hunks(old, new)
        .into_iter()
        .map(|hunk| himark_ahp_ext_types::text::Span {
            start: hunk.start,
            end: hunk.end,
            text: hunk.text,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn apply(old: &str, hunks: &[Hunk]) -> String {
        let mut out = String::new();
        let mut pos = 0usize;
        for hunk in hunks {
            out.push_str(&old[pos..hunk.start]);
            out.push_str(&hunk.text);
            pos = hunk.end;
        }
        out.push_str(&old[pos..]);
        out
    }

    #[test]
    fn the_line_diff_reconstructs_the_target() {
        let cases = [
            ("a\nb\nc\n", "a\nB\nc\n"),
            ("a\nb\nc\n", "a\nb\nc\nd\n"),
            ("a\nb\nc\n", "b\nc\n"),
            ("", "fresh\n"),
            ("gone\n", ""),
            ("x\ny\nz\n", "z\ny\nx\n"),
            ("a\nb\na\nb\n", "a\nb\nX\na\nb\n"),
            ("no trailing newline", "no trailing newline at all"),
        ];
        for (old, new) in cases {
            assert_eq!(apply(old, &hunks(old, new)), new, "{old:?} -> {new:?}");
        }
    }

    #[test]
    fn disjoint_edits_merge_from_both_sides() {
        let base = "top\nmid\nbottom\n";
        let ours = "OURS\nmid\nbottom\n";
        let theirs = "top\nmid\nTHEIRS\n";
        assert_eq!(merged(base, ours, theirs), "OURS\nmid\nTHEIRS\n");
    }

    #[test]
    fn the_shared_change_lands_once() {
        let base = "a\nb\n";
        let both = "a\nSHARED\nb\n";
        assert_eq!(merged(base, both, both), both);
    }

    #[test]
    fn a_conflict_drops_nobodys_bytes() {
        let base = "a\nMID\nb\n";
        let ours = "a\nOURS\nb\n";
        let theirs = "a\nTHEIRS\nb\n";
        let merge = merged(base, ours, theirs);
        assert!(
            merge.contains("OURS") && merge.contains("THEIRS"),
            "{merge:?}"
        );
    }

    #[test]
    fn a_prefix_of_the_shared_change_is_not_doubled() {
        let base = "a\nb\n";
        let ours = "a\nONE\nTWO\nb\n";
        let theirs = "a\nONE\nb\n";
        assert_eq!(merged(base, ours, theirs), "a\nONE\nTWO\nb\n");
    }
}
