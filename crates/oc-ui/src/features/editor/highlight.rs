//! Incremental syntax highlighting for the editor, off the UI thread.
//!
//! Each line has its role spans; parser states are kept at checkpoints
//! (every [`CHECKPOINT`] lines). After an edit only the lines from the edit
//! on are re-highlighted, and only until the parser state at a checkpoint
//! past the edit matches the one stored before it ("converged"): typing in
//! a function re-highlights a line or two, opening a string quote
//! re-highlights until it closes.
//!
//! Work runs on a worker in slices of at most [`SLICE`] on a snapshot of the
//! rope (cloning a rope is O(1)), from the first invalid line onwards — the
//! edited, visible part first. Results for a document that changed meanwhile
//! are kept for the lines before the first edit since the job started.
//! Lines not highlighted yet keep their old colors (shifted with the edits)
//! or are drawn plain; typing never waits for the highlighter.

use std::sync::Arc;
use std::time::{Duration, Instant};

use oc_preview::highlight::{LineHighlighter, LineState, RoleSpan};
use ropey::Rope;

use super::buffer::Change;
use crate::tasks::{CancelToken, Tasks};

/// Parser states are stored every this many lines.
pub const CHECKPOINT: usize = 64;
/// A worker job stops after this long and hands its lines over.
const SLICE: Duration = Duration::from_millis(30);
/// Like 1.x: larger documents are shown as plain text (re-highlighting would lag).
pub const MAX_HIGHLIGHT_CHARS: usize = 2_000_000;

pub struct JobResult {
    version: u64,
    start: usize,
    spans: Vec<Vec<RoleSpan>>,
    checkpoints: Vec<(usize, LineState)>,
    converged: bool,
    failed: bool,
}

#[derive(Default)]
pub struct Highlight {
    hl: Option<Arc<LineHighlighter>>,
    /// Role spans per line (byte offsets in the line); always one per line.
    spans: Vec<Vec<RoleSpan>>,
    /// (line, state at its start), sorted; the first is line 0.
    checkpoints: Vec<(usize, LineState)>,
    /// Lines `[0, valid)` are highlighted for the current text.
    valid: usize,
    /// Lines up to here were right before the edits; they are again once
    /// the state after the last edited line (`dirty_to`) converges.
    tail_ok: usize,
    dirty_to: usize,
    version: u64,
    job: Option<CancelToken>,
    /// The first line edited since the running job started.
    edited_since_job: usize,
}

impl Highlight {
    /// Highlighting for a document of `lines` lines (None: plain text).
    pub fn new(hl: Option<LineHighlighter>, lines: usize) -> Self {
        let hl = hl.map(Arc::new);
        let checkpoints = hl.as_ref().map(|h| vec![(0, h.start())]).unwrap_or_default();
        Highlight { hl, spans: vec![Vec::new(); lines], checkpoints, edited_since_job: usize::MAX, ..Default::default() }
    }

    pub fn enabled(&self) -> bool {
        self.hl.is_some()
    }

    /// The spans of `line` (empty if plain or not highlighted yet).
    pub fn spans(&self, line: usize) -> &[RoleSpan] {
        self.spans.get(line).map_or(&[], Vec::as_slice)
    }

    /// Everything is highlighted (tests, screenshots).
    pub fn done(&self) -> bool {
        self.hl.is_none() || (self.valid >= self.spans.len() && self.job.is_none())
    }

    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// Bring the line bookkeeping up to date with buffer changes.
    pub fn apply_changes(&mut self, changes: &[Change], lines: usize) {
        if changes.contains(&Change::Reset) {
            self.reset(lines); // everything is redone anyway
        } else if let Some((line, old_end, new_end)) = compose(changes) {
            // One pass over the span table and checkpoints, however many
            // edits happened since the last event.
            self.lines_changed(line, old_end, new_end);
        }
        // Defensive: the span table always has one entry per line.
        self.spans.resize_with(lines, Vec::new);
    }

    fn reset(&mut self, lines: usize) {
        self.version += 1;
        self.spans = vec![Vec::new(); lines];
        self.checkpoints.truncate(1);
        self.valid = 0;
        self.tail_ok = 0;
        self.dirty_to = 0;
        self.edited_since_job = 0;
    }

    fn lines_changed(&mut self, line: usize, old_end: usize, new_end: usize) {
        self.version += 1;
        self.edited_since_job = self.edited_since_job.min(line);
        let delta = new_end as isize - old_end as isize;
        let shift = |x: usize| -> usize { if x > old_end { (x as isize + delta) as usize } else { x } };
        // Keep the edited lines' old colors (shifted) until they are redone.
        if delta > 0 {
            let at = (old_end + 1).min(self.spans.len());
            self.spans.splice(at..at, std::iter::repeat_with(Vec::new).take(delta as usize));
        } else if delta < 0 {
            let from = (new_end + 1).min(self.spans.len());
            let to = (old_end + 1).min(self.spans.len());
            self.spans.drain(from..to);
        }
        // What was valid after the edited lines may be valid again.
        let valid_tail = if self.valid > old_end + 1 { shift(self.valid) } else { 0 };
        let old_tail = if self.tail_ok > old_end + 1 { shift(self.tail_ok) } else { 0 };
        // Lines edited earlier and not redone yet stay dirty.
        let old_dirty = if self.tail_ok > 0 { if self.dirty_to > old_end { shift(self.dirty_to) } else { self.dirty_to } } else { 0 };
        self.dirty_to = new_end.max(old_dirty);
        if self.valid > line {
            self.tail_ok = valid_tail.max(old_tail);
            self.valid = line;
        } else {
            self.tail_ok = old_tail;
        }
        self.checkpoints.retain(|(l, _)| *l <= line || *l > old_end);
        for (l, _) in &mut self.checkpoints {
            *l = shift(*l);
        }
    }

    /// Start a worker job if there is something to highlight and none runs.
    /// `on_done` gets the result on the UI thread (the window applies it).
    pub fn kick(&mut self, rope: &Rope, tasks: &Tasks, on_done: impl FnOnce(&mut crate::App, JobResult) + Send + 'static) {
        let Some(hl) = self.hl.clone() else { return };
        if self.job.is_some() || self.valid >= self.spans.len() {
            return;
        }
        let Some(start_idx) = self.checkpoints.iter().rposition(|(l, _)| *l <= self.valid) else {
            return;
        };
        let (start, state) = self.checkpoints[start_idx].clone();
        let converge: Vec<(usize, LineState)> = if self.tail_ok > self.dirty_to + 1 {
            self.checkpoints.iter().filter(|(l, _)| *l > self.dirty_to && *l <= self.tail_ok).take(4).cloned().collect()
        } else {
            Vec::new()
        };
        let rope = rope.clone();
        let version = self.version;
        self.edited_since_job = usize::MAX;
        let token = tasks.run_in_background(move |token| run(&hl, &rope, start, state, &converge, version, token), on_done);
        self.job = Some(token);
    }

    /// A job's lines arrived.
    pub fn apply(&mut self, res: JobResult) {
        self.job = None;
        if res.failed {
            self.hl = None;
            self.spans.iter_mut().for_each(Vec::clear);
            return;
        }
        let current = res.version == self.version;
        let limit = if current { usize::MAX } else { self.edited_since_job };
        let n = res.spans.len().min(limit.saturating_sub(res.start)).min(self.spans.len().saturating_sub(res.start));
        let end = res.start + n;
        for (i, s) in res.spans.into_iter().take(n).enumerate() {
            self.spans[res.start + i] = s;
        }
        // At a convergence point the stored state is the right one already.
        let keep_end = current && res.converged;
        self.checkpoints.retain(|(l, _)| *l <= res.start || *l > end || (keep_end && *l == end));
        self.checkpoints.extend(res.checkpoints.into_iter().filter(|(l, _)| *l > res.start && *l <= end));
        self.checkpoints.sort_by_key(|(l, _)| *l);
        self.checkpoints.dedup_by_key(|(l, _)| *l);
        if res.start <= self.valid {
            self.valid = self.valid.max(end);
        }
        if current && res.converged && end > self.dirty_to {
            self.valid = self.valid.max(self.tail_ok);
        }
        if self.valid >= self.tail_ok {
            self.tail_ok = 0;
        }
    }

    pub fn cancel(&mut self) {
        if let Some(t) = self.job.take() {
            t.cancel();
        }
    }
}

/// Several line changes in a row as one: `[line, old_end]` of the text
/// before all of them became `[line, new_end]` (it may cover unchanged lines
/// in between; they are just highlighted again).
fn compose(changes: &[Change]) -> Option<(usize, usize, usize)> {
    let mut acc: Option<(usize, usize, usize)> = None;
    for c in changes {
        let Change::Lines { line, old_end, new_end } = *c else { continue };
        acc = Some(match acc {
            None => (line, old_end, new_end),
            Some((s, oe, ne)) => {
                let delta = new_end as isize - old_end as isize;
                let oe2 = if old_end > ne { oe + (old_end - ne) } else { oe };
                let ne2 = if ne > old_end { ((ne as isize + delta) as usize).max(new_end) } else { new_end };
                (s.min(line), oe2, ne2)
            }
        });
    }
    acc
}

/// Lines longer than this (bytes) are shown plain and leave the parser
/// state as it was: grammars can take seconds on a megabyte of minified
/// code, and every keystroke on that line would redo it.
const LONG_LINE: usize = 20_000;

/// The worker: highlight from `start` until the end, convergence or the time slice.
fn run(
    hl: &LineHighlighter,
    rope: &Rope,
    start: usize,
    mut state: LineState,
    converge: &[(usize, LineState)],
    version: u64,
    token: &CancelToken,
) -> JobResult {
    let t0 = Instant::now();
    let total = rope.len_lines();
    let mut spans = Vec::new();
    let mut checkpoints = Vec::new();
    let mut converged = false;
    let mut failed = false;
    let mut buf = String::new();
    let mut line = start;
    while line < total {
        if line > start {
            if let Some((_, old)) = converge.iter().find(|(l, _)| *l == line)
                && *old == state
            {
                converged = true;
                break;
            }
            if (line - start).is_multiple_of(CHECKPOINT) {
                checkpoints.push((line, state.clone()));
            }
        }
        let text = rope.line(line);
        if text.len_bytes() > LONG_LINE {
            spans.push(Vec::new());
            line += 1;
            continue;
        }
        buf.clear();
        for chunk in text.chunks() {
            buf.push_str(chunk);
        }
        match hl.line(&mut state, &buf) {
            Some(s) => spans.push(s),
            None => {
                failed = true;
                break;
            }
        }
        line += 1;
        if (line - start).is_multiple_of(32) && (t0.elapsed() > SLICE || token.is_cancelled()) {
            break;
        }
    }
    if !converged && !failed {
        checkpoints.push((line, state));
    }
    JobResult { version, start, spans, checkpoints, converged, failed }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::editor::buffer::Buffer;
    use oc_preview::data::SyntaxRole;
    use oc_preview::highlight::syntax_for_file;

    /// Run jobs synchronously until done; returns how many lines were highlighted.
    fn finish(h: &mut Highlight, rope: &Rope) -> usize {
        let (mut jobs, mut lines) = (0, 0);
        while h.valid < h.spans.len() {
            let hl = h.hl.clone().unwrap();
            let idx = h.checkpoints.iter().rposition(|(l, _)| *l <= h.valid).unwrap();
            let (start, state) = h.checkpoints[idx].clone();
            let converge: Vec<_> = if h.tail_ok > h.dirty_to + 1 {
                h.checkpoints.iter().filter(|(l, _)| *l > h.dirty_to && *l <= h.tail_ok).take(4).cloned().collect()
            } else {
                Vec::new()
            };
            h.edited_since_job = usize::MAX;
            h.job = Some(CancelToken::new());
            let res = run(&hl, rope, start, state, &converge, h.version, &CancelToken::new());
            lines += res.spans.len();
            h.apply(res);
            jobs += 1;
            assert!(jobs < 1000);
        }
        lines
    }

    fn roles(h: &Highlight, line: usize) -> Vec<SyntaxRole> {
        h.spans(line).iter().map(|s| s.role).collect()
    }

    #[test]
    fn edits_rehighlight_until_the_state_converges() {
        let text: String = (0..1000).map(|i| format!("x{i} = {i}  # note\n")).collect();
        let mut b = Buffer::new(&text);
        let mut h = Highlight::new(Some(LineHighlighter::new(syntax_for_file("a.py").unwrap())), b.len_lines());
        finish(&mut h, b.rope());
        assert!(h.done());
        assert!(roles(&h, 500).contains(&SyntaxRole::Comment));
        // A plain edit: done after a line or so (converges at the next checkpoint).
        b.move_to(b.line_start(500), false);
        b.insert("y", true);
        h.apply_changes(&b.take_changes(), b.len_lines());
        assert_eq!(h.valid, 500);
        let redone = finish(&mut h, b.rope());
        assert!(redone < 3 * CHECKPOINT, "re-highlighted {redone} lines");
        assert!(h.done());
        // Opening a string: everything after it changes.
        b.move_to(b.line_start(10), false);
        b.insert("s = \"\"\"\n", false);
        h.apply_changes(&b.take_changes(), b.len_lines());
        finish(&mut h, b.rope());
        assert_eq!(roles(&h, 800), [SyntaxRole::String]);
        assert!(h.spans(10).iter().any(|s| s.role == SyntaxRole::String));
        // Closing it again.
        b.undo();
        h.apply_changes(&b.take_changes(), b.len_lines());
        assert_eq!(h.spans.len(), b.len_lines());
        finish(&mut h, b.rope());
        assert!(roles(&h, 800).contains(&SyntaxRole::Comment));
    }

    #[test]
    fn results_for_an_older_text_keep_the_lines_before_the_edit() {
        let text: String = (0..300).map(|i| format!("v{i} = 1\n")).collect();
        let mut b = Buffer::new(&text);
        let mut h = Highlight::new(Some(LineHighlighter::new(syntax_for_file("a.py").unwrap())), b.len_lines());
        let hl = h.hl.clone().unwrap();
        let (start, state) = h.checkpoints[0].clone();
        h.edited_since_job = usize::MAX;
        h.job = Some(CancelToken::new());
        let res = run(&hl, &b.rope().clone(), start, state, &[], h.version, &CancelToken::new());
        b.move_to(b.line_start(100), false);
        b.insert("\n\n", false);
        h.apply_changes(&b.take_changes(), b.len_lines());
        h.apply(res);
        assert_eq!(h.valid, 100, "lines before the edit are kept");
        assert!(h.checkpoints.iter().all(|(l, _)| *l <= 100));
        finish(&mut h, b.rope());
        assert_eq!(h.spans.len(), b.len_lines());
        assert!(!h.spans(301).is_empty());
    }

    /// Random edits (with strings opening and closing, line joins and
    /// splits, several edits per event, results for an older text): the
    /// incremental result always equals highlighting the text from scratch.
    #[test]
    fn incremental_matches_a_full_rehighlight() {
        let syntax = syntax_for_file("a.py").unwrap();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        let base: String = (0..400).map(|i| format!("def f{i}(x):\n    return x + {i}  # c\n")).collect();
        let mut b = Buffer::new(&base);
        let mut h = Highlight::new(Some(LineHighlighter::new(syntax)), b.len_lines());
        finish(&mut h, b.rope());
        let snippets = ["\"\"\"", "\n", "x", "# ", "'", "\n\n", "def g():\n    pass\n", "\"", ""];
        for round in 0..60 {
            // Sometimes a job runs on the text as it is, and arrives after more edits.
            let stale = (round % 3 == 0).then(|| {
                let hl = h.hl.clone().unwrap();
                let idx = h.checkpoints.iter().rposition(|(l, _)| *l <= h.valid).unwrap();
                let (start, state) = h.checkpoints[idx].clone();
                h.edited_since_job = usize::MAX;
                h.job = Some(CancelToken::new());
                run(&hl, &b.rope().clone(), start, state, &[], h.version, &CancelToken::new())
            });
            for _ in 0..1 + rnd(3) {
                let n = b.len_chars();
                let at = rnd(n + 1);
                if rnd(3) == 0 && n > 0 {
                    let end = (at + rnd(40)).min(n);
                    b.select(at..end);
                    b.insert("", false);
                } else {
                    b.move_to(at, false);
                    b.insert(snippets[rnd(snippets.len())], false);
                }
            }
            h.apply_changes(&b.take_changes(), b.len_lines());
            if let Some(res) = stale {
                h.apply(res);
            }
            finish(&mut h, b.rope());
            let mut fresh = Highlight::new(Some(LineHighlighter::new(syntax)), b.len_lines());
            finish(&mut fresh, b.rope());
            for line in 0..b.len_lines() {
                assert_eq!(h.spans(line), fresh.spans(line), "round {round}, line {line}: {:?}", b.line(line).to_string());
            }
        }
    }
}
