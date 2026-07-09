//! `gg-diff` — line and intra-line diffing on top of `imara-diff`, producing
//! the renderer-friendly hunk model from `gg-core`.
//!
//! The pipeline follows imara-diff's recommended pattern: compute a line-level
//! diff with the Histogram algorithm (postprocessed for human-readable slider
//! placement), group the changed blocks into git-style hunks with surrounding
//! context, then compute intra-line token spans only on paired add/delete lines
//! (never on pure insertions/deletions or pathologically large changes).

#![forbid(unsafe_code)]
#![warn(missing_docs)]

use std::path::PathBuf;

use gg_core::diff::{DiffLine, FileChange, FileDiff, Hunk, LineKind, TokenSpan};
use imara_diff::{Algorithm, Diff, InternedInput};

/// Tunables for diff rendering.
#[derive(Clone, Copy, Debug)]
pub struct DiffOptions {
    /// Context lines kept around each change (git's default is 3).
    pub context: usize,
    /// Skip intra-line highlighting for lines longer than this many bytes.
    pub max_intra_line_len: usize,
    /// Diff algorithm.
    pub algorithm: Algorithm,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            context: 3,
            max_intra_line_len: 2048,
            algorithm: Algorithm::Histogram,
        }
    }
}

/// True if the bytes look binary (contain a NUL within the first 8 KiB).
pub fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|&b| b == 0)
}

/// Diff two text blobs into hunks. Inputs are treated as UTF-8 text; callers
/// should gate on [`looks_binary`] first.
pub fn diff_text(old: &str, new: &str, opts: &DiffOptions) -> Vec<Hunk> {
    let before_lines: Vec<&str> = split_lines(old);
    let after_lines: Vec<&str> = split_lines(new);

    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(opts.algorithm, &input);
    diff.postprocess_lines(&input);

    // Changed blocks in monotonic order; ranges index into the line vectors.
    let blocks: Vec<imara_diff::Hunk> = diff.hunks().collect();
    if blocks.is_empty() {
        return Vec::new();
    }

    // Group blocks whose surrounding context would touch/overlap into one hunk.
    let ctx = opts.context as u32;
    let mut groups: Vec<Vec<imara_diff::Hunk>> = Vec::new();
    for h in blocks {
        match groups.last_mut() {
            Some(g) if h.before.start.saturating_sub(g.last().unwrap().before.end) <= 2 * ctx => {
                g.push(h);
            }
            _ => groups.push(vec![h]),
        }
    }

    groups
        .into_iter()
        .map(|group| build_hunk(&group, &before_lines, &after_lines, opts))
        .collect()
}

/// Diff two text blobs and wrap them as a single-file diff.
pub fn diff_file(
    old: &[u8],
    new: &[u8],
    path: impl Into<PathBuf>,
    old_path: Option<PathBuf>,
    change: FileChange,
    opts: &DiffOptions,
) -> FileDiff {
    let path = path.into();
    if looks_binary(old) || looks_binary(new) {
        return FileDiff {
            path,
            old_path,
            change,
            is_binary: true,
            hunks: Vec::new(),
        };
    }
    let old_s = String::from_utf8_lossy(old);
    let new_s = String::from_utf8_lossy(new);
    FileDiff {
        path,
        old_path,
        change,
        is_binary: false,
        hunks: diff_text(&old_s, &new_s, opts),
    }
}

/// Split into lines the same way imara-diff tokenizes (each line keeps its
/// terminator internally; we strip it for display).
fn split_lines(s: &str) -> Vec<&str> {
    s.split_inclusive('\n').collect()
}

fn trim_eol(s: &str) -> String {
    let s = s.strip_suffix('\n').unwrap_or(s);
    let s = s.strip_suffix('\r').unwrap_or(s);
    s.to_string()
}

fn build_hunk(
    group: &[imara_diff::Hunk],
    before: &[&str],
    after: &[&str],
    opts: &DiffOptions,
) -> Hunk {
    let ctx = opts.context as u32;
    let first = &group[0];
    let last = group.last().unwrap();

    let old_start = first.before.start.saturating_sub(ctx);
    let new_start = first.after.start.saturating_sub(ctx);
    let old_end = (last.before.end + ctx).min(before.len() as u32);
    let new_end = (last.after.end + ctx).min(after.len() as u32);

    let mut lines: Vec<DiffLine> = Vec::new();
    let mut bi = old_start;
    let mut ai = new_start;

    for h in group {
        // Leading context shared by both sides.
        while bi < h.before.start {
            lines.push(DiffLine {
                kind: LineKind::Context,
                text: trim_eol(before[bi as usize]),
                old_lineno: Some(bi + 1),
                new_lineno: Some(ai + 1),
                intra: Vec::new(),
            });
            bi += 1;
            ai += 1;
        }

        let del_start = lines.len();
        while bi < h.before.end {
            lines.push(DiffLine {
                kind: LineKind::Deletion,
                text: trim_eol(before[bi as usize]),
                old_lineno: Some(bi + 1),
                new_lineno: None,
                intra: Vec::new(),
            });
            bi += 1;
        }
        let add_start = lines.len();
        while ai < h.after.end {
            lines.push(DiffLine {
                kind: LineKind::Addition,
                text: trim_eol(after[ai as usize]),
                old_lineno: None,
                new_lineno: Some(ai + 1),
                intra: Vec::new(),
            });
            ai += 1;
        }

        annotate_intra(&mut lines, del_start, add_start, add_start, opts);
    }

    // Trailing context (kept aligned across both sides).
    let trailing = (old_end - bi).min(new_end - ai);
    for _ in 0..trailing {
        lines.push(DiffLine {
            kind: LineKind::Context,
            text: trim_eol(before[bi as usize]),
            old_lineno: Some(bi + 1),
            new_lineno: Some(ai + 1),
            intra: Vec::new(),
        });
        bi += 1;
        ai += 1;
    }

    Hunk {
        old_start: old_start + 1,
        old_lines: bi - old_start,
        new_start: new_start + 1,
        new_lines: ai - new_start,
        header: String::new(),
        lines,
    }
}

/// Pair the i-th deletion with the i-th addition in a change block and set
/// their intra-line changed spans. Skips when the block is a pure
/// insertion/deletion or the lines are too long.
fn annotate_intra(
    lines: &mut [DiffLine],
    del_start: usize,
    del_end: usize,
    add_start: usize,
    opts: &DiffOptions,
) {
    let dels = del_end - del_start;
    let adds = lines.len() - add_start;
    if dels == 0 || adds == 0 {
        return;
    }
    let pairs = dels.min(adds);
    for i in 0..pairs {
        let (del_idx, add_idx) = (del_start + i, add_start + i);
        let old_text = lines[del_idx].text.clone();
        let new_text = lines[add_idx].text.clone();
        if old_text.len() > opts.max_intra_line_len || new_text.len() > opts.max_intra_line_len {
            continue;
        }
        let (old_span, new_span) = intra_line_span(&old_text, &new_text);
        if let Some(s) = old_span {
            lines[del_idx].intra.push(s);
        }
        if let Some(s) = new_span {
            lines[add_idx].intra.push(s);
        }
    }
}

/// Compute the changed middle of two similar lines by stripping the shared
/// prefix and suffix (on char boundaries). Returns the differing byte span on
/// each side, or `None` for a side whose span is empty.
fn intra_line_span(old: &str, new: &str) -> (Option<TokenSpan>, Option<TokenSpan>) {
    if old == new {
        return (None, None);
    }
    let prefix = common_prefix_len(old, new);
    let suffix = common_suffix_len(&old[prefix..], &new[prefix..]);

    let old_span = (prefix < old.len() - suffix).then_some(TokenSpan {
        start: prefix,
        end: old.len() - suffix,
    });
    let new_span = (prefix < new.len() - suffix).then_some(TokenSpan {
        start: prefix,
        end: new.len() - suffix,
    });
    (old_span, new_span)
}

fn common_prefix_len(a: &str, b: &str) -> usize {
    let mut len = 0;
    for (ca, cb) in a.char_indices().zip(b.char_indices()) {
        if ca.1 != cb.1 {
            break;
        }
        len = ca.0 + ca.1.len_utf8();
    }
    len
}

fn common_suffix_len(a: &str, b: &str) -> usize {
    let mut len = 0;
    let mut ai = a.char_indices().rev();
    let mut bi = b.char_indices().rev();
    loop {
        match (ai.next(), bi.next()) {
            (Some((_, ca)), Some((_, cb))) if ca == cb => len += ca.len_utf8(),
            _ => break,
        }
    }
    len
}

/// Parse the output of `git diff` (unified format) into the hunk model.
///
/// Used for the working-tree diff preview, where shelling out to `git diff`
/// (the spec's CLI reserve read path) reproduces git's exact rename/binary
/// handling. Intra-line spans are computed on paired add/delete lines, matching
/// [`diff_text`].
pub fn parse_unified(text: &str) -> Vec<FileDiff> {
    let opts = DiffOptions::default();
    let mut files: Vec<FileDiff> = Vec::new();
    let mut cur: Option<FileDiff> = None;
    let mut hunk: Option<Hunk> = None;
    let mut minus_path: Option<PathBuf> = None;
    let mut old_no = 0u32;
    let mut new_no = 0u32;

    // Finalize and stash the in-progress hunk into the current file.
    fn flush_hunk(cur: &mut Option<FileDiff>, hunk: &mut Option<Hunk>, opts: &DiffOptions) {
        if let Some(mut h) = hunk.take() {
            annotate_parsed(&mut h, opts);
            if let Some(f) = cur.as_mut() {
                f.hunks.push(h);
            }
        }
    }

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush_hunk(&mut cur, &mut hunk, &opts);
            if let Some(f) = cur.take() {
                files.push(f);
            }
            cur = Some(FileDiff {
                path: fallback_path(rest),
                old_path: None,
                change: FileChange::Modified,
                is_binary: false,
                hunks: Vec::new(),
            });
            minus_path = None;
            continue;
        }
        let Some(file) = cur.as_mut() else { continue };

        if line.starts_with("new file mode") {
            file.change = FileChange::Added;
        } else if line.starts_with("deleted file mode") {
            file.change = FileChange::Deleted;
        } else if let Some(r) = line.strip_prefix("rename from ") {
            file.old_path = Some(PathBuf::from(r));
            file.change = FileChange::Renamed;
        } else if let Some(r) = line.strip_prefix("rename to ") {
            file.path = PathBuf::from(r);
            file.change = FileChange::Renamed;
        } else if line.starts_with("copy from ") {
            file.change = FileChange::Copied;
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            file.is_binary = true;
        } else if let Some(r) = line.strip_prefix("--- ") {
            minus_path = (r != "/dev/null").then(|| strip_ab(r));
        } else if let Some(r) = line.strip_prefix("+++ ") {
            if r == "/dev/null" {
                if let Some(m) = minus_path.take() {
                    file.path = m;
                }
            } else {
                file.path = strip_ab(r);
            }
        } else if line.starts_with("@@") {
            flush_hunk(&mut cur, &mut hunk, &opts);
            let (os, oc, ns, nc, header) = parse_hunk_header(line);
            old_no = os;
            new_no = ns;
            hunk = Some(Hunk {
                old_start: os,
                old_lines: oc,
                new_start: ns,
                new_lines: nc,
                header,
                lines: Vec::new(),
            });
        } else if let Some(h) = hunk.as_mut() {
            match line.as_bytes().first() {
                Some(b' ') => {
                    h.lines.push(DiffLine {
                        kind: LineKind::Context,
                        text: line[1..].to_string(),
                        old_lineno: Some(old_no),
                        new_lineno: Some(new_no),
                        intra: Vec::new(),
                    });
                    old_no += 1;
                    new_no += 1;
                }
                Some(b'+') => {
                    h.lines.push(DiffLine {
                        kind: LineKind::Addition,
                        text: line[1..].to_string(),
                        old_lineno: None,
                        new_lineno: Some(new_no),
                        intra: Vec::new(),
                    });
                    new_no += 1;
                }
                Some(b'-') => {
                    h.lines.push(DiffLine {
                        kind: LineKind::Deletion,
                        text: line[1..].to_string(),
                        old_lineno: Some(old_no),
                        new_lineno: None,
                        intra: Vec::new(),
                    });
                    old_no += 1;
                }
                // "\ No newline at end of file" and anything else: ignore.
                _ => {}
            }
        }
    }

    flush_hunk(&mut cur, &mut hunk, &opts);
    if let Some(f) = cur.take() {
        files.push(f);
    }
    files
}

/// A single-file unified diff split into its header and per-hunk bodies, so a
/// caller can rebuild a one-hunk patch (`header + hunks[i]`) for
/// `git apply --cached`. The pieces are verbatim slices of git's output,
/// preserving its exact bytes (including any `\ No newline at end of file`
/// markers), which reconstructing from the parsed [`Hunk`] model would drop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePatch {
    /// Everything before the first hunk: the `diff --git`, `index`,
    /// `--- a/…`, and `+++ b/…` lines (each newline-terminated, so the header
    /// always ends on a line boundary).
    pub header: String,
    /// Each hunk verbatim, from its `@@` line through the line before the next
    /// hunk (or the end of input).
    pub hunks: Vec<String>,
}

/// Split a single-file unified diff (e.g. the output of `git diff -- <path>`)
/// into its header and hunk texts. Returns `None` when the text contains no
/// hunk (`@@`): e.g. a binary or pure-mode-change diff, which cannot be staged
/// hunk-by-hunk.
///
/// Content lines are always prefixed by `' '`, `'+'`, or `'-'`, so only a real
/// hunk header begins a line with `@@`; this lets us split on column-0 `@@`
/// without misclassifying an added/removed line that happens to contain `@@`.
pub fn split_file_patch(raw: &str) -> Option<FilePatch> {
    let mut header = String::new();
    let mut hunks: Vec<String> = Vec::new();
    for piece in raw.split_inclusive('\n') {
        if piece.starts_with("@@") {
            hunks.push(piece.to_string());
        } else if let Some(last) = hunks.last_mut() {
            last.push_str(piece);
        } else {
            header.push_str(piece);
        }
    }
    (!hunks.is_empty()).then_some(FilePatch { header, hunks })
}

/// Build a standalone, `git apply`-able patch carrying only hunk `index` of a
/// single-file unified diff. Feed it to `git apply --cached` (forward to stage
/// the hunk, `--reverse` to unstage it) to move exactly one hunk between the
/// working tree and the index. Returns `None` if the text has no hunks or
/// `index` is out of range.
pub fn single_hunk_patch(raw: &str, index: usize) -> Option<String> {
    let patch = split_file_patch(raw)?;
    let hunk = patch.hunks.get(index)?;
    Some(format!("{}{}", patch.header, hunk))
}

/// Strip a leading `a/` or `b/` (and an optional trailing tab+timestamp).
fn strip_ab(s: &str) -> PathBuf {
    let s = s.split('\t').next().unwrap_or(s);
    let s = s
        .strip_prefix("a/")
        .or_else(|| s.strip_prefix("b/"))
        .unwrap_or(s);
    PathBuf::from(s)
}

/// Best-effort path from a `diff --git a/x b/y` line (used until `+++`/`---`
/// give the authoritative path).
fn fallback_path(rest: &str) -> PathBuf {
    rest.rsplit(' ').next().map(strip_ab).unwrap_or_default()
}

/// Parse `@@ -os[,oc] +ns[,nc] @@ header`.
fn parse_hunk_header(line: &str) -> (u32, u32, u32, u32, String) {
    let after = &line[2..];
    let (ranges, header) = match after.find("@@") {
        Some(i) => (after[..i].trim(), after[i + 2..].trim().to_string()),
        None => (after.trim(), String::new()),
    };
    let mut parts = ranges.split_whitespace();
    let old = parts.next().unwrap_or("-0,0");
    let new = parts.next().unwrap_or("+0,0");
    let (os, oc) = parse_range(old.trim_start_matches('-'));
    let (ns, nc) = parse_range(new.trim_start_matches('+'));
    (os, oc, ns, nc, header)
}

fn parse_range(s: &str) -> (u32, u32) {
    let mut it = s.split(',');
    let start = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let count = it.next().and_then(|v| v.parse().ok()).unwrap_or(1);
    (start, count)
}

/// Annotate intra-line spans on paired deletion/addition runs in a parsed hunk.
fn annotate_parsed(h: &mut Hunk, opts: &DiffOptions) {
    let mut i = 0;
    while i < h.lines.len() {
        if h.lines[i].kind != LineKind::Deletion {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < h.lines.len() && h.lines[i].kind == LineKind::Deletion {
            i += 1;
        }
        let del_end = i;
        if i >= h.lines.len() || h.lines[i].kind != LineKind::Addition {
            continue;
        }
        let add_start = i;
        while i < h.lines.len() && h.lines[i].kind == LineKind::Addition {
            i += 1;
        }
        let add_end = i;

        let pairs = (del_end - del_start).min(add_end - add_start);
        for k in 0..pairs {
            let old = h.lines[del_start + k].text.clone();
            let new = h.lines[add_start + k].text.clone();
            if old.len() > opts.max_intra_line_len || new.len() > opts.max_intra_line_len {
                continue;
            }
            let (o, n) = intra_line_span(&old, &new);
            if let Some(s) = o {
                h.lines[del_start + k].intra.push(s);
            }
            if let Some(s) = n {
                h.lines[add_start + k].intra.push(s);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_modification() {
        let old = "line one\nline two\nline three\n";
        let new = "line one\nline 2\nline three\n";
        let hunks = diff_text(old, new, &DiffOptions::default());
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        let dels: Vec<_> = h
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Deletion)
            .collect();
        let adds: Vec<_> = h
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Addition)
            .collect();
        assert_eq!(dels.len(), 1);
        assert_eq!(adds.len(), 1);
        assert_eq!(dels[0].text, "line two");
        assert_eq!(adds[0].text, "line 2");
        // Intra-line span isolates the changed middle ("two" vs "2").
        assert!(!dels[0].intra.is_empty());
        let span = dels[0].intra[0];
        assert_eq!(&dels[0].text[span.start..span.end], "two");
    }

    #[test]
    fn pure_insertion_has_no_intra() {
        let old = "a\nb\n";
        let new = "a\nx\ny\nb\n";
        let hunks = diff_text(old, new, &DiffOptions::default());
        assert_eq!(hunks.len(), 1);
        for l in &hunks[0].lines {
            assert!(l.intra.is_empty());
        }
        let adds = hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind == LineKind::Addition)
            .count();
        assert_eq!(adds, 2);
    }

    #[test]
    fn identical_text_no_hunks() {
        let s = "same\ncontent\n";
        assert!(diff_text(s, s, &DiffOptions::default()).is_empty());
    }

    #[test]
    fn binary_detection() {
        assert!(looks_binary(b"abc\0def"));
        assert!(!looks_binary(b"plain text"));
        let fd = diff_file(
            b"abc\0",
            b"abc\0def",
            "bin",
            None,
            FileChange::Modified,
            &DiffOptions::default(),
        );
        assert!(fd.is_binary);
        assert!(fd.hunks.is_empty());
    }

    #[test]
    fn line_numbers_are_consistent() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n";
        let new = "1\n2\n3\nFOUR\n5\n6\n7\n8\n";
        let hunks = diff_text(old, new, &DiffOptions::default());
        let h = &hunks[0];
        // Context around line 4 with 3 lines of context on each side.
        let first = &h.lines[0];
        assert_eq!(first.kind, LineKind::Context);
        assert_eq!(first.old_lineno, Some(1));
        let del = h
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Deletion)
            .unwrap();
        assert_eq!(del.old_lineno, Some(4));
        assert_eq!(del.text, "4");
    }

    #[test]
    fn parses_unified_diff() {
        let text = concat!(
            "diff --git a/src/foo.rs b/src/foo.rs\n",
            "index abc..def 100644\n",
            "--- a/src/foo.rs\n",
            "+++ b/src/foo.rs\n",
            "@@ -1,4 +1,4 @@\n",
            " fn main() {\n",
            "-    let x = 1;\n",
            "+    let x = 2;\n",
            "     println!(\"{x}\");\n",
            " }\n",
        );
        let files = parse_unified(text);
        assert_eq!(files.len(), 1);
        let f = &files[0];
        assert_eq!(f.path, PathBuf::from("src/foo.rs"));
        assert_eq!(f.change, FileChange::Modified);
        assert_eq!(f.hunks.len(), 1);
        let h = &f.hunks[0];
        assert_eq!((h.old_start, h.new_start), (1, 1));
        let del = h
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Deletion)
            .unwrap();
        let add = h
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Addition)
            .unwrap();
        assert_eq!(del.text, "    let x = 1;");
        assert_eq!(add.text, "    let x = 2;");
        assert_eq!(del.old_lineno, Some(2));
        assert_eq!(add.new_lineno, Some(2));
        // Intra-line span isolates the "1" vs "2".
        assert!(!del.intra.is_empty());
        let s = del.intra[0];
        assert_eq!(&del.text[s.start..s.end], "1");
    }

    #[test]
    fn parses_new_and_deleted_files() {
        let text = concat!(
            "diff --git a/new.txt b/new.txt\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/new.txt\n",
            "@@ -0,0 +1,2 @@\n",
            "+hello\n",
            "+world\n",
            "diff --git a/gone.txt b/gone.txt\n",
            "deleted file mode 100644\n",
            "--- a/gone.txt\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-bye\n",
        );
        let files = parse_unified(text);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].change, FileChange::Added);
        assert_eq!(files[0].path, PathBuf::from("new.txt"));
        assert_eq!(files[0].additions(), 2);
        assert_eq!(files[1].change, FileChange::Deleted);
        assert_eq!(files[1].path, PathBuf::from("gone.txt"));
        assert_eq!(files[1].deletions(), 1);
    }

    #[test]
    fn splits_multi_hunk_patch() {
        let raw = concat!(
            "diff --git a/f.txt b/f.txt\n",
            "index 111..222 100644\n",
            "--- a/f.txt\n",
            "+++ b/f.txt\n",
            "@@ -1,3 +1,3 @@\n",
            " a\n",
            "-b\n",
            "+B\n",
            " c\n",
            "@@ -10,3 +10,3 @@ fn ctx()\n",
            " x\n",
            "-y\n",
            "+Y\n",
            " z\n",
        );
        let fp = split_file_patch(raw).unwrap();
        assert!(fp.header.starts_with("diff --git a/f.txt b/f.txt\n"));
        assert!(fp.header.ends_with("+++ b/f.txt\n"));
        assert_eq!(fp.hunks.len(), 2);
        assert!(fp.hunks[0].starts_with("@@ -1,3 +1,3 @@\n"));
        assert!(fp.hunks[1].starts_with("@@ -10,3 +10,3 @@ fn ctx()\n"));

        let p0 = single_hunk_patch(raw, 0).unwrap();
        // The one-hunk patch is exactly the header followed by that hunk.
        assert_eq!(p0, format!("{}{}", fp.header, fp.hunks[0]));
        assert!(p0.contains("+B\n"));
        assert!(!p0.contains("@@ -10,3")); // the second hunk is excluded
        assert!(single_hunk_patch(raw, 2).is_none());
    }

    #[test]
    fn split_preserves_no_newline_marker() {
        let raw = concat!(
            "diff --git a/f b/f\n",
            "--- a/f\n",
            "+++ b/f\n",
            "@@ -1 +1 @@\n",
            "-old\n",
            "+new\n",
            "\\ No newline at end of file\n",
        );
        let fp = split_file_patch(raw).unwrap();
        assert_eq!(fp.hunks.len(), 1);
        assert!(fp.hunks[0].contains("\\ No newline at end of file"));
        // A content line that contains "@@" must not be mistaken for a header.
        let raw2 = concat!(
            "diff --git a/f b/f\n",
            "--- a/f\n",
            "+++ b/f\n",
            "@@ -1 +1 @@\n",
            "-@@ not a header\n",
            "+@@ still not a header\n",
        );
        assert_eq!(split_file_patch(raw2).unwrap().hunks.len(), 1);
    }

    #[test]
    fn split_returns_none_without_hunks() {
        let raw = "diff --git a/f b/f\nold mode 100644\nnew mode 100755\n";
        assert!(split_file_patch(raw).is_none());
        assert!(single_hunk_patch(raw, 0).is_none());
    }
}
