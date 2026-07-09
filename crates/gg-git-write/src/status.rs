//! Parsing of `git status --porcelain=v2 -z` into [`StatusSnapshot`].
//!
//! Porcelain v2 is the stable, machine-readable format; `-z` makes records
//! NUL-terminated so paths with spaces/newlines are unambiguous. Renames carry
//! their original path in the following NUL-delimited field.

use std::path::PathBuf;

use gg_core::{ChangeKind, StatusEntry, StatusSnapshot};

/// Map a porcelain v2 status code character to a [`ChangeKind`].
fn change(c: u8) -> ChangeKind {
    match c {
        b'.' => ChangeKind::Unmodified,
        b'M' => ChangeKind::Modified,
        b'A' => ChangeKind::Added,
        b'D' => ChangeKind::Deleted,
        b'R' => ChangeKind::Renamed,
        b'C' => ChangeKind::Copied,
        b'T' => ChangeKind::TypeChanged,
        b'U' => ChangeKind::Conflicted,
        _ => ChangeKind::Modified,
    }
}

/// Parse the raw `--porcelain=v2 --branch -z` output.
pub fn parse(out: &str) -> StatusSnapshot {
    let mut snap = StatusSnapshot::default();
    let mut fields = out.split('\0');

    while let Some(field) = fields.next() {
        if field.is_empty() {
            continue;
        }
        match field.as_bytes()[0] {
            b'#' => parse_header(field, &mut snap),
            b'1' => {
                if let Some(e) = parse_ordinary(field) {
                    snap.entries.push(e);
                }
            }
            b'2' => {
                if let Some(mut e) = parse_ordinary(field) {
                    // The original path is the next NUL-delimited field.
                    if let Some(orig) = fields.next() {
                        e.orig_path = Some(PathBuf::from(orig));
                    }
                    snap.entries.push(e);
                }
            }
            b'u' => {
                if let Some(path) = field.rsplit(' ').next() {
                    snap.entries.push(StatusEntry {
                        path: PathBuf::from(path),
                        orig_path: None,
                        staged: ChangeKind::Conflicted,
                        unstaged: ChangeKind::Conflicted,
                    });
                }
            }
            b'?' => snap.entries.push(StatusEntry {
                path: PathBuf::from(&field[2..]),
                orig_path: None,
                staged: ChangeKind::Unmodified,
                unstaged: ChangeKind::Untracked,
            }),
            b'!' => { /* ignored entry; skip */ }
            _ => {}
        }
    }

    snap
}

fn parse_header(field: &str, snap: &mut StatusSnapshot) {
    let mut it = field.split(' ');
    let _hash = it.next(); // "#"
    match it.next() {
        Some("branch.head") => {
            if let Some(name) = it.next() {
                if name != "(detached)" {
                    snap.branch = Some(name.to_string());
                }
            }
        }
        Some("branch.upstream") => {
            if let Some(name) = it.next() {
                snap.upstream = Some(name.to_string());
            }
        }
        Some("branch.ab") => {
            // "+<ahead> -<behind>"
            if let Some(a) = it.next() {
                snap.ahead = a.trim_start_matches('+').parse().unwrap_or(0);
            }
            if let Some(b) = it.next() {
                snap.behind = b.trim_start_matches('-').parse().unwrap_or(0);
            }
        }
        _ => {}
    }
}

/// Parse a `1`/`2` entry. Both share the `<type> <XY> <sub> <mH> <mI> <mW>
/// <hH> <hI> [<Xscore>] <path>` prefix; type 2 has the extra score field.
fn parse_ordinary(field: &str) -> Option<StatusEntry> {
    let is_rename = field.as_bytes()[0] == b'2';
    // Number of space-separated fields before the path.
    let prefix = if is_rename { 9 } else { 8 };
    let parts: Vec<&str> = field.splitn(prefix + 1, ' ').collect();
    if parts.len() < prefix + 1 {
        return None;
    }
    let xy = parts[1].as_bytes();
    if xy.len() < 2 {
        return None;
    }
    let path = parts[prefix];
    Some(StatusEntry {
        path: PathBuf::from(path),
        orig_path: None,
        staged: change(xy[0]),
        unstaged: change(xy[1]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_branch_and_entries() {
        let out = concat!(
            "# branch.head main\0",
            "# branch.upstream origin/main\0",
            "# branch.ab +2 -1\0",
            "1 .M N... 100644 100644 100644 aaaa bbbb file1.txt\0",
            "1 M. N... 100644 100644 100644 aaaa bbbb staged.txt\0",
            "1 MM N... 100644 100644 100644 aaaa bbbb both.txt\0",
            "? untracked.txt\0",
        );
        let snap = parse(out);
        assert_eq!(snap.branch.as_deref(), Some("main"));
        assert_eq!(snap.upstream.as_deref(), Some("origin/main"));
        assert_eq!(snap.ahead, 2);
        assert_eq!(snap.behind, 1);
        assert_eq!(snap.entries.len(), 4);

        let file1 = &snap.entries[0];
        assert_eq!(file1.path, PathBuf::from("file1.txt"));
        assert_eq!(file1.staged, ChangeKind::Unmodified);
        assert_eq!(file1.unstaged, ChangeKind::Modified);
        assert!(file1.has_unstaged() && !file1.is_staged());

        let staged = &snap.entries[1];
        assert!(staged.is_staged() && !staged.has_unstaged());

        let both = &snap.entries[2];
        assert!(both.is_staged() && both.has_unstaged());

        let unt = &snap.entries[3];
        assert_eq!(unt.unstaged, ChangeKind::Untracked);
    }

    #[test]
    fn parses_rename_with_original_path() {
        let out = concat!(
            "# branch.head main\0",
            "2 R. N... 100644 100644 100644 aaaa bbbb R100 new name.txt\0old name.txt\0",
        );
        let snap = parse(out);
        assert_eq!(snap.entries.len(), 1);
        let e = &snap.entries[0];
        assert_eq!(e.path, PathBuf::from("new name.txt"));
        assert_eq!(e.orig_path, Some(PathBuf::from("old name.txt")));
        assert_eq!(e.staged, ChangeKind::Renamed);
    }

    #[test]
    fn handles_paths_with_spaces() {
        let out = "1 .M N... 100644 100644 100644 aaaa bbbb dir/a file.txt\0";
        let snap = parse(out);
        assert_eq!(snap.entries[0].path, PathBuf::from("dir/a file.txt"));
    }
}
