//! Progress reporting for network operations.
//!
//! git emits progress to stderr in carriage-return-delimited chunks like
//! `Receiving objects:  50% (5000/10000), 1.20 MiB | 500 KiB/s`. We parse the
//! phase, percentage, and the `(current/total)` counters and forward them to a
//! [`ProgressSink`].

/// A single progress update parsed from git's stderr.
#[derive(Clone, Debug, PartialEq)]
pub struct Progress {
    /// Phase label, e.g. "Receiving objects" or "Resolving deltas".
    pub phase: String,
    /// Completion percentage in `0.0..=100.0`, if git reported one.
    pub percent: Option<f32>,
    /// Items processed so far, if reported.
    pub current: Option<u64>,
    /// Total items, if reported.
    pub total: Option<u64>,
}

/// A receiver for progress updates. Implemented by the application to drive a
/// progress bar; a [`NullSink`] is provided for callers that don't care.
pub trait ProgressSink: Send {
    /// Receive one progress update (a recognized progress meter line).
    fn report(&mut self, progress: Progress);

    /// Receive one raw output line from the command (stdout or stderr), so a UI
    /// can show git's full transcript. Default: ignore.
    fn line(&mut self, _line: &str) {}
}

/// A sink that discards everything.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullSink;

impl ProgressSink for NullSink {
    fn report(&mut self, _progress: Progress) {}
}

/// A sink that forwards every update through a closure.
pub struct FnSink<F: FnMut(Progress) + Send>(pub F);

impl<F: FnMut(Progress) + Send> ProgressSink for FnSink<F> {
    fn report(&mut self, progress: Progress) {
        (self.0)(progress)
    }
}

/// Parse a single git progress chunk. Returns `None` for lines that don't look
/// like progress (so callers can treat them as plain log output / error text).
pub fn parse_progress_line(line: &str) -> Option<Progress> {
    let line = line.trim();
    let (phase, rest) = line.split_once(':')?;
    let phase = phase.trim();
    if phase.is_empty() {
        return None;
    }
    let rest = rest.trim();

    let percent = rest
        .split('%')
        .next()
        .and_then(|p| p.trim().rsplit(|c: char| c.is_whitespace()).next())
        .and_then(|p| p.parse::<f32>().ok())
        .filter(|_| rest.contains('%'));

    // Pull the "(current/total)" counters if present.
    let (mut current, mut total) = (None, None);
    if let Some(open) = rest.find('(') {
        if let Some(close) = rest[open..].find(')') {
            let inner = &rest[open + 1..open + close];
            if let Some((c, t)) = inner.split_once('/') {
                current = c.trim().parse::<u64>().ok();
                total = t
                    .trim()
                    .split(|ch: char| !ch.is_ascii_digit())
                    .next()
                    .and_then(|t| t.parse::<u64>().ok());
            }
        }
    }

    // Require at least one quantitative signal to count as progress.
    if percent.is_none() && current.is_none() {
        return None;
    }

    Some(Progress {
        phase: phase.to_string(),
        percent,
        current,
        total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_receiving_objects() {
        let p = parse_progress_line("Receiving objects:  50% (5000/10000), 1.20 MiB | 500 KiB/s")
            .unwrap();
        assert_eq!(p.phase, "Receiving objects");
        assert_eq!(p.percent, Some(50.0));
        assert_eq!(p.current, Some(5000));
        assert_eq!(p.total, Some(10000));
    }

    #[test]
    fn parses_counting_done() {
        let p = parse_progress_line("Counting objects: 100% (10/10), done.").unwrap();
        assert_eq!(p.phase, "Counting objects");
        assert_eq!(p.percent, Some(100.0));
        assert_eq!(p.current, Some(10));
        assert_eq!(p.total, Some(10));
    }

    #[test]
    fn ignores_non_progress() {
        assert!(parse_progress_line("From github.com:foo/bar").is_none());
        assert!(parse_progress_line(" * [new branch] main -> origin/main").is_none());
    }
}
