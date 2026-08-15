//! Provider-neutral transcript tail engine (step C).
//!
//! Both the Claude Code and Codex transcript watchers previously carried a
//! near-identical `tail_loop`: read lines from a growing JSONL file, buffer a
//! trailing partial across the EOF poll boundary, strip CRLF, skip blank
//! lines, and hand each complete line to a provider-specific parser. This
//! module collapses that duplication into one [`TranscriptTailService`] driven
//! by an injected [`TranscriptDecoder`] — the decoder is the only
//! provider-specific seam.
//!
//! The loop owns the frozen line-buffering contract (F-EVENTS replay→live
//! boundary): a partial line survives a non-terminal EOF, [`on_caught_up`]
//! fires on **every** EOF where the partial buffer is empty (NOT just the
//! first — implementations MUST be idempotent, see the trait docstring), and
//! a read error warns + sleeps + continues rather than tearing the watcher
//! down. The empty-partial guard is load-bearing: firing the boundary signal
//! while a line is half-buffered would let decoders that flush replay-only
//! emitter state at that signal classify the straddling line — which started
//! during replay — as a live event when it eventually completes.
//!
//! [`on_caught_up`]: TranscriptDecoder::on_caught_up

use std::io::Read;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Poll cadence between EOF reads of the growing transcript file. Moved here
/// from the per-provider modules so both tails share one value.
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Hard cap on the partial-line buffer to prevent runaway memory growth from a
/// pathological writer that emits data without ever terminating a line.
/// Production Claude / Codex JSONL writers always terminate with `\n`, so this
/// cap is hit only on a writer bug, a truncated mid-write file, or a corrupt
/// transcript. When the cap would be exceeded, the partial is discarded and
/// the engine enters "skip until next newline" mode, then resumes normal
/// processing at the next complete line. 4 MiB is well above any single
/// JSONL line we would ever see in practice (typical lines are <10 KiB; the
/// largest realistic line — a long tool result — is still <1 MiB), so
/// legitimate traffic never trips this. PR #302 cycle 11 F3 introduced the
/// cap; cycle 14 F1 (codex P2) tightened the enforcement to bound allocation
/// DURING the read rather than checking AFTER `read_line` had already
/// allocated the entire giant line.
pub(crate) const MAX_PARTIAL_BYTES: usize = 4 * 1024 * 1024;

/// Per-`Read::read` chunk size. Fixed-size so each iteration's allocation is
/// bounded by this constant, independent of how large the underlying line
/// might be. 8 KiB is small enough that a 4 MiB partial fills in ≤512
/// iterations (well within a single 500ms poll cycle) and large enough that
/// per-syscall overhead is amortized across realistic line sizes
/// (typical Claude / Codex JSONL lines are <10 KiB). PR #302 cycle 14 F1.
pub(crate) const READ_CHUNK_BYTES: usize = 8 * 1024;

/// PR #302 cycle 17 F2 (Claude post-cycle-16 review MED 85%) — stale-
/// partial watchdog. When a JSONL writer dies mid-line (crash, disk
/// full, killed process), `partial` would otherwise stay non-empty
/// forever and `on_caught_up` would never fire, leaving decoders
/// (e.g. `TestRunEmitter`) frozen in replay mode with no user-visible
/// signal. After this threshold of inactivity (no new bytes read) AND
/// `partial.is_empty() == false`, the engine logs a warn, discards
/// the orphaned partial, exits skip-mode if set, and force-fires
/// `on_caught_up` so downstream UIs unblock. 30s is well above any
/// realistic inter-write gap during live tailing (Claude / Codex
/// write multiple events per second when active) and short enough
/// that a stuck UI recovers within human-observable time. Tests
/// override via `with_stale_partial_watchdog`.
pub(crate) const STALE_PARTIAL_WATCHDOG: Duration = Duration::from_secs(30);

/// The provider-specific seam: turns one complete transcript line into events,
/// and reacts to the EOF / caught-up signal. Implementations own their
/// per-session parse state (in-flight tool calls, turn counts, last cwd).
pub(crate) trait TranscriptDecoder: Send {
    /// Decode one complete (newline-stripped, non-blank) transcript line.
    fn decode_line(&mut self, line: &str);
    /// Called on **every** EOF (`Ok(0)` read) where the partial-line
    /// buffer is empty. Implementations MUST be idempotent — this fires
    /// repeatedly during steady-state live tailing (every poll cycle,
    /// ≈ POLL_INTERVAL = 500ms), not just at the initial replay→live
    /// boundary.
    ///
    /// **Idempotency contract.** Treat each call as "the writer has
    /// caught me up to its current end-of-file; flush any pending
    /// emit-batches and return to a steady state." A handler that
    /// emits a non-idempotent side effect (a summary event, a
    /// state-machine transition, etc.) will misbehave every 500ms
    /// during live tailing. If you genuinely need "the replay just
    /// ended" exactly-once, implement that as a per-decoder
    /// `bool replay_done` flag set on the first call and short-circuit
    /// subsequent calls.
    ///
    /// **Partial-buffer guard (PR #302 review F3).** The engine
    /// suppresses this call when a straddling line is half-buffered,
    /// so any line already-buffered before the signal will be decoded
    /// BEFORE the signal fires. If the writer truncated mid-line, the
    /// signal stays parked until the line completes OR until the
    /// stale-partial watchdog fires.
    ///
    /// **Stale-partial watchdog (PR #302 cycle 17 F2).** If the
    /// partial line buffer (or skip-mode) stays stuck without any
    /// new bytes for `STALE_PARTIAL_WATCHDOG` (default 30s), the
    /// engine logs a warn, discards the buffered bytes, and
    /// force-fires this signal so downstream replay-bounded state
    /// can unblock. The orphaned partial is dropped without being
    /// decoded — corrupt or truncated lines are never delivered as
    /// events. Implementations don't need to special-case the
    /// watchdog path; this method is called with the same
    /// idempotency contract as the normal path.
    fn on_caught_up(&mut self);
}

/// Owns the read/buffer/normalize loop; delegates per-line semantics to the
/// injected [`TranscriptDecoder`].
pub(crate) struct TranscriptTailService {
    decoder: Box<dyn TranscriptDecoder>,
    provider_label: &'static str,
    poll_interval: Duration,
    stale_partial_watchdog: Duration,
}

impl TranscriptTailService {
    pub(crate) fn new(decoder: Box<dyn TranscriptDecoder>, provider_label: &'static str) -> Self {
        Self {
            decoder,
            provider_label,
            poll_interval: POLL_INTERVAL,
            stale_partial_watchdog: STALE_PARTIAL_WATCHDOG,
        }
    }

    /// Override the stale-partial watchdog (tests drive short
    /// durations to exercise the recovery path without a 30s sleep).
    #[cfg(test)]
    pub(crate) fn with_stale_partial_watchdog(mut self, d: Duration) -> Self {
        self.stale_partial_watchdog = d;
        self
    }

    /// Override the poll cadence (tests drive `Duration::ZERO` so the EOF
    /// sleeps are no-ops).
    #[cfg(test)]
    pub(crate) fn with_poll_interval(mut self, d: Duration) -> Self {
        self.poll_interval = d;
        self
    }

    /// Tail `reader` until `stop` is set, dispatching each complete line to
    /// the decoder. Reads in fixed-size chunks (`READ_CHUNK_BYTES`) and
    /// scans for `\n` manually so the partial-line buffer's
    /// `MAX_PARTIAL_BYTES` cap actually limits allocation during the read,
    /// not just after (PR #302 cycle 14 F1 — codex P2 caught that the
    /// pre-cycle-14 `BufRead::read_line` form let a giant unterminated line
    /// fully allocate before the cap-check ran). Buffers a trailing partial
    /// across EOF, strips CRLF, skips blank lines, fires `on_caught_up` on
    /// each EOF where `partial.is_empty()`, treats a read error as warn +
    /// sleep + continue, and silently skips non-UTF-8 lines (with a warn).
    ///
    /// **Over-cap behavior:** when accumulating bytes into `partial` would
    /// exceed `MAX_PARTIAL_BYTES`, the partial is discarded and the engine
    /// enters skip-until-newline mode. Subsequent bytes within the
    /// over-long line are dropped until the next `\n`, at which point
    /// normal processing resumes from the line AFTER the bad one.
    pub(crate) fn run<R: Read>(mut self, mut reader: R, stop: Arc<AtomicBool>) {
        let mut chunk = [0u8; READ_CHUNK_BYTES];
        let mut partial: Vec<u8> = Vec::new();
        // True iff we exceeded MAX_PARTIAL_BYTES on the current line; bytes
        // are discarded until the next `\n` resets us to normal processing.
        let mut skip_until_newline = false;
        // PR #302 cycle 17 F2 — last `Instant` at which a non-EOF read
        // returned >0 bytes. Drives the stale-partial watchdog so a
        // truncated JSONL writer doesn't freeze decoders forever.
        // Initialised to `Instant::now()` so the watchdog measures
        // elapsed-from-startup until the first byte arrives.
        let mut last_byte_at = std::time::Instant::now();
        while !stop.load(Ordering::Acquire) {
            match reader.read(&mut chunk) {
                Ok(0) => {
                    // Defer the replay→live boundary signal while a partial
                    // line is half-buffered: firing on_caught_up here would
                    // let decoders that flush replay-only emitter state at
                    // that signal classify the eventually-completed
                    // straddling line as a live event, even though it
                    // started during replay (PR #302 Claude review F3).
                    // Same guard applies during skip mode — if we're in
                    // the middle of dropping a runaway line, the writer
                    // hasn't truly "caught up" yet.
                    if partial.is_empty() && !skip_until_newline {
                        self.decoder.on_caught_up();
                    }
                    std::thread::sleep(self.poll_interval);
                }
                Ok(n) => {
                    last_byte_at = std::time::Instant::now();
                    self.process_chunk(&chunk[..n], &mut partial, &mut skip_until_newline);
                    // Fresh bytes arrived — skip the watchdog check.
                    continue;
                }
                Err(e) => {
                    log::warn!("Error reading {}: {}", self.provider_label, e);
                    std::thread::sleep(self.poll_interval);
                }
            }
            // PR #302 cycle 17 F2 + cycle 20 F1 — stale-partial
            // watchdog: if `partial` (or skip-mode) has been stuck
            // without any new bytes for `STALE_PARTIAL_WATCHDOG`,
            // discard it and force-fire `on_caught_up` so downstream
            // decoders (e.g. `TestRunEmitter`) exit replay mode and
            // the user-visible UI unblocks. Recovers from
            // corrupt-file / writer-crash / disk-full scenarios
            // where the terminating `\n` will never arrive.
            //
            // **Hoisted outside the match (cycle 20 F1, Claude
            // post-cycle-19 review MED 93%):** pre-cycle-20 the
            // check lived only in the `Ok(0)` arm, so a reader
            // stuck in a persistent error state (e.g., EIO on an
            // NFS mount that goes away) would never hit the
            // watchdog — `Ok(0)` never executes, `last_byte_at`
            // stays frozen, and decoders gating replay-bounded
            // state on `on_caught_up` stay stuck indefinitely. The
            // `Ok(n)` arm uses `continue` above to skip this block
            // (fresh bytes resets the stale clock); both `Ok(0)`
            // and `Err` arms fall through to here.
            if (!partial.is_empty() || skip_until_newline)
                && last_byte_at.elapsed() >= self.stale_partial_watchdog
            {
                log::warn!(
                    "{} tail: partial line stalled for >={:?} with no new bytes \
                     (writer crashed mid-line, disk full, or corrupt file); \
                     discarding {} buffered bytes and force-firing on_caught_up",
                    self.provider_label,
                    self.stale_partial_watchdog,
                    partial.len(),
                );
                partial.clear();
                skip_until_newline = false;
                self.decoder.on_caught_up();
                // Reset so the watchdog doesn't refire every poll
                // iteration if the reader / writer stays silent.
                last_byte_at = std::time::Instant::now();
            }
        }
    }

    /// Scan `input` for `\n` boundaries, append/process accordingly. Splits
    /// out of `run` so the per-chunk state-machine is testable in isolation
    /// (and so `run`'s outer loop stays a single match expression).
    fn process_chunk(
        &mut self,
        mut input: &[u8],
        partial: &mut Vec<u8>,
        skip_until_newline: &mut bool,
    ) {
        while !input.is_empty() {
            match input.iter().position(|&b| b == b'\n') {
                Some(pos) => {
                    let (head, rest) = input.split_at(pos + 1);
                    if *skip_until_newline {
                        // The `\n` terminates the over-long line — reset
                        // and resume normal processing from `rest`.
                        *skip_until_newline = false;
                    } else if partial.len().saturating_add(head.len()) > MAX_PARTIAL_BYTES {
                        // PR #302 cycle 14 retry-1: cap check on the
                        // newline-arrives-in-crossing-chunk path. Codex
                        // verify caught that the cap was only enforced
                        // in the no-newline branch — a partial near the
                        // cap followed by a chunk like `b"x\n"` would
                        // hit `Some(pos)` and decode the over-limit
                        // line. Now we discard the head (the over-long
                        // line's terminator), clear partial, and resume
                        // normal processing from `rest`. No skip mode
                        // needed: the newline IS in `head`, so the
                        // line is fully consumed here.
                        log::warn!(
                            "{} tail: partial line buffer would exceed {} bytes (newline \
                             in same chunk); discarding (likely writer bug or corrupt file)",
                            self.provider_label,
                            MAX_PARTIAL_BYTES,
                        );
                        partial.clear();
                    } else {
                        partial.extend_from_slice(head);
                        // Try to decode as UTF-8; non-UTF-8 lines are
                        // skipped with a warn rather than crashing the
                        // tail (defensive — real JSONL is always UTF-8,
                        // but a corrupt file shouldn't kill the watcher).
                        match std::str::from_utf8(partial) {
                            Ok(line_str) => {
                                let trimmed = line_str.trim_end_matches(['\r', '\n']);
                                if !trimmed.trim().is_empty() {
                                    self.decoder.decode_line(trimmed);
                                }
                            }
                            Err(_) => {
                                log::warn!(
                                    "{} tail: non-UTF-8 line, skipping",
                                    self.provider_label,
                                );
                            }
                        }
                        partial.clear();
                    }
                    input = rest;
                }
                None => {
                    // No newline in remaining input — either accumulate
                    // into `partial` (if under the cap) or enter skip mode.
                    if *skip_until_newline {
                        // Still no newline; keep dropping.
                    } else if partial.len().saturating_add(input.len()) > MAX_PARTIAL_BYTES {
                        log::warn!(
                            "{} tail: partial line buffer would exceed {} bytes; \
                             entering skip-until-newline mode (likely writer bug \
                             or corrupt file)",
                            self.provider_label,
                            MAX_PARTIAL_BYTES,
                        );
                        partial.clear();
                        *skip_until_newline = true;
                    } else {
                        partial.extend_from_slice(input);
                    }
                    // Consume all of `input` — the leftover is either
                    // safely in `partial`, dropped, or discarded.
                    input = &[];
                }
            }
        }
    }
}

/// Read a finite JSONL source with the same allocation bound as the live tail.
/// Incomplete trailing lines and over-cap lines are ignored; scanning resumes
/// after the next newline so a corrupt record cannot hide later valid events.
pub(crate) fn for_each_bounded_line<R, F>(
    mut reader: R,
    provider_label: &str,
    mut visit: F,
) -> std::io::Result<()>
where
    R: Read,
    F: FnMut(&str),
{
    let mut chunk = [0u8; READ_CHUNK_BYTES];
    let mut partial = Vec::new();
    let mut skip_until_newline = false;

    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(());
        }

        let mut input = &chunk[..count];
        while !input.is_empty() {
            match input.iter().position(|&byte| byte == b'\n') {
                Some(position) => {
                    let (head, rest) = input.split_at(position + 1);
                    if skip_until_newline {
                        skip_until_newline = false;
                    } else if partial.len().saturating_add(head.len()) > MAX_PARTIAL_BYTES {
                        log::warn!(
                            "{}: recovery line exceeds {} bytes, skipping",
                            provider_label,
                            MAX_PARTIAL_BYTES,
                        );
                        partial.clear();
                    } else {
                        partial.extend_from_slice(head);
                        match std::str::from_utf8(&partial) {
                            Ok(line) => {
                                let trimmed = line.trim_end_matches(['\r', '\n']);
                                if !trimmed.trim().is_empty() {
                                    visit(trimmed);
                                }
                            }
                            Err(_) => {
                                log::warn!("{}: non-UTF-8 recovery line, skipping", provider_label);
                            }
                        }
                        partial.clear();
                    }
                    input = rest;
                }
                None => {
                    if skip_until_newline {
                        // Keep dropping the current over-cap line.
                    } else if partial.len().saturating_add(input.len()) > MAX_PARTIAL_BYTES {
                        log::warn!(
                            "{}: recovery line exceeds {} bytes, skipping",
                            provider_label,
                            MAX_PARTIAL_BYTES,
                        );
                        partial.clear();
                        skip_until_newline = true;
                    } else {
                        partial.extend_from_slice(input);
                    }
                    input = &[];
                }
            }
        }
    }
}

// --- Test support (module-level, `pub(crate)` so the per-provider decoder
// tests in Tasks 2.3/2.4 can drive the same scripted reader). ---

/// One scripted `read` outcome. Drives the chunked-read loop in
/// `TranscriptTailService::run` by emitting one `Read::read` result per
/// step (PR #302 cycle 14 F1 — was `BufRead::read_line`-based before).
#[cfg(test)]
pub(crate) enum Step {
    /// `read` returns these bytes verbatim. To exercise chunking, split
    /// a multi-line input across multiple `Chunk` steps. Newlines must
    /// be present in the chunk content if line termination is desired.
    /// Use with byte literals (`b"..."`); they coerce to `&'static [u8]`
    /// without allocation.
    Chunk(&'static [u8]),
    /// Same as `Chunk` but holds an owned buffer. PR #302 cycle 19 F2
    /// (Claude post-cycle-18 review LOW 90%): over-cap tests need a
    /// runtime-sized `MAX_PARTIAL_BYTES + 1` buffer (4 MiB+) for the
    /// engine's bounded-chunk read loop to grind through. Pre-cycle-19
    /// used `Box::leak(vec![...])` to satisfy the `&'static [u8]`
    /// lifetime — leaking 4–8 MiB of permanent heap per `cargo test`
    /// run, and blocking future Miri CI adoption. `ChunkOwned` keeps
    /// the byte-literal call sites zero-cost while letting over-cap
    /// tests pass a plain `Vec<u8>` that drops normally at end-of-test.
    ChunkOwned(Vec<u8>),
    /// Non-terminal EOF — `Ok(0)`; the loop fires `on_caught_up` (if
    /// partial is empty AND not in skip mode) and polls again.
    Eof,
    /// Like `Eof`, but also flips `stop` so the loop exits after this read.
    EofStop,
    /// One read failure — the loop warns, sleeps, and continues.
    Err,
    /// Like `Err`, but also flips `stop` so the loop exits after
    /// this read. PR #302 cycle 20 retry-1 (codex MED 0.93): needed
    /// so the persistent-Err regression test can terminate WITHOUT
    /// giving the pre-cycle-20 code an `Ok(0)` to fire the watchdog
    /// from — otherwise the test passes against both old and new
    /// implementations.
    ErrStop,
}

#[cfg(test)]
pub(crate) struct ScriptedReader {
    pub steps: std::vec::IntoIter<Step>,
    pub stop: Arc<AtomicBool>,
    /// In-flight chunk bytes — either an owned `Vec<u8>` (from
    /// `Step::ChunkOwned`) or a borrowed `&'static [u8]` (from
    /// `Step::Chunk`), unified via `Cow<'static, [u8]>`. Paired with
    /// a `usize` offset that cursors the engine's bounded `Read::read`
    /// calls through the chunk. PR #302 cycle 14 F1 introduced this
    /// shape for the engine's chunked-read loop; cycle 19 F2 switched
    /// it from `&'static [u8]` (which forced over-cap tests into
    /// `Box::leak`) to `Cow` so owned `Vec<u8>` buffers drop normally
    /// at end-of-test.
    #[cfg(test)]
    current: Option<(std::borrow::Cow<'static, [u8]>, usize)>,
}

#[cfg(test)]
impl ScriptedReader {
    pub(crate) fn new(steps: std::vec::IntoIter<Step>, stop: Arc<AtomicBool>) -> Self {
        Self {
            steps,
            stop,
            current: None,
        }
    }
}

#[cfg(test)]
impl std::io::Read for ScriptedReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // Drain remainder of the previously-started chunk first.
        if let Some((bytes, offset)) = self.current.as_mut() {
            let remaining = &bytes[*offset..];
            let n = remaining.len().min(buf.len());
            buf[..n].copy_from_slice(&remaining[..n]);
            *offset += n;
            if *offset == bytes.len() {
                self.current = None;
            }
            return Ok(n);
        }
        match self.steps.next() {
            Some(Step::Chunk(bytes)) => {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                if n < bytes.len() {
                    self.current = Some((std::borrow::Cow::Borrowed(bytes), n));
                }
                Ok(n)
            }
            Some(Step::ChunkOwned(bytes)) => {
                let n = bytes.len().min(buf.len());
                buf[..n].copy_from_slice(&bytes[..n]);
                if n < bytes.len() {
                    self.current = Some((std::borrow::Cow::Owned(bytes), n));
                } // else: small owned chunk consumed in one read; drop now
                Ok(n)
            }
            Some(Step::Err) => Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "scripted read error",
            )),
            Some(Step::ErrStop) => {
                self.stop.store(true, Ordering::Release);
                Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "scripted read error (stop)",
                ))
            }
            Some(Step::Eof) => Ok(0),
            Some(Step::EofStop) | None => {
                self.stop.store(true, Ordering::Release);
                Ok(0)
            }
        }
    }
}

/// `run` MOVES the decoder, so tests clone the inner `Arc`s up front to inspect
/// what was decoded after the loop returns.
#[cfg(test)]
#[derive(Clone, Default)]
struct RecordingDecoder {
    lines: Arc<std::sync::Mutex<Vec<String>>>,
    caught_up: Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(test)]
impl TranscriptDecoder for RecordingDecoder {
    fn decode_line(&mut self, line: &str) {
        self.lines.lock().unwrap().push(line.to_string());
    }
    fn on_caught_up(&mut self) {
        self.caught_up.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_scan_discards_an_over_cap_line_and_resumes() {
        let mut input = vec![b'x'; MAX_PARTIAL_BYTES + 1];
        input.extend_from_slice(b"\n{\"ok\":true}\n");
        let mut lines = Vec::new();

        for_each_bounded_line(std::io::Cursor::new(input), "test recovery", |line| {
            lines.push(line.to_string());
        })
        .expect("scan lines");

        assert_eq!(lines, ["{\"ok\":true}"]);
    }

    /// Drive the service over a scripted read sequence and return what the
    /// recording decoder saw: the decoded lines + the `on_caught_up` count.
    fn drive(steps: Vec<Step>) -> (Vec<String>, usize) {
        let stop = Arc::new(AtomicBool::new(false));
        let dec = RecordingDecoder::default();
        let lines = dec.lines.clone();
        let caught = dec.caught_up.clone();
        TranscriptTailService::new(Box::new(dec), "t")
            .with_poll_interval(Duration::ZERO)
            .run(ScriptedReader::new(steps.into_iter(), stop.clone()), stop);
        let decoded = lines.lock().unwrap().clone();
        (decoded, caught.load(Ordering::Acquire))
    }

    #[test]
    fn engine_partial_survives_eof_then_completes() {
        // A partial buffered before a non-terminal EOF survives and joins the
        // rest of the line when the file grows.
        let (lines, _) = drive(vec![
            Step::Chunk(b"{\"a\":1"),
            Step::Eof,
            Step::Chunk(b"23}\n"),
            Step::EofStop,
        ]);
        assert_eq!(lines, ["{\"a\":123}"]);
    }

    /// PR #302 cycle 20 F1 (Claude post-cycle-19 review MED 93%) —
    /// stale-partial watchdog also fires under persistent read
    /// errors. Pre-cycle-20 the watchdog lived only in the `Ok(0)`
    /// arm, so a reader stuck on `Err` (e.g., EIO on an NFS mount
    /// that disappears) would never trip the watchdog and decoders
    /// would stay frozen. The fix hoisted the check out of the
    /// match so both `Ok(0)` and `Err` arms reach it.
    #[test]
    fn engine_stale_partial_watchdog_fires_under_persistent_err() {
        let stop = Arc::new(AtomicBool::new(false));
        let dec = RecordingDecoder::default();
        let lines = dec.lines.clone();
        let caught = dec.caught_up.clone();
        TranscriptTailService::new(Box::new(dec), "t")
            .with_poll_interval(Duration::ZERO)
            .with_stale_partial_watchdog(Duration::ZERO)
            .run(
                ScriptedReader::new(
                    vec![
                        // Partial line — never terminated.
                        Step::Chunk(b"{\"a\":1"),
                        // Reader enters persistent error state. NO
                        // intervening `Ok(0)` — codex-verify retry-1
                        // (cycle 20 MED 0.93) caught that the
                        // original test ended with `EofStop`, which
                        // returns `Ok(0)` and gave the pre-cycle-20
                        // (Ok-arm-only) watchdog an opportunity to
                        // fire from that final EOF. `ErrStop` flips
                        // `stop` while returning `Err`, so the loop
                        // exits via the Err arm and ONLY the hoisted
                        // post-match watchdog can fire here.
                        Step::Err,
                        Step::Err,
                        Step::ErrStop,
                    ]
                    .into_iter(),
                    stop.clone(),
                ),
                stop,
            );
        let decoded = lines.lock().unwrap().clone();
        assert!(
            decoded.is_empty(),
            "orphaned partial must not decode as a line under persistent Err",
        );
        assert!(
            caught.load(Ordering::Acquire) >= 1,
            "watchdog must force-fire on_caught_up even when Err is the only post-Chunk read",
        );
    }

    /// PR #302 cycle 17 F2 (Claude post-cycle-16 review MED 85%) —
    /// stale-partial watchdog fires after the configured threshold,
    /// discarding the orphaned partial and force-firing
    /// `on_caught_up` so downstream replay-bounded decoders unblock.
    /// Test uses a 1ms watchdog so the assertion completes promptly;
    /// drives a partial chunk + a sequence of `Step::Eof`s
    /// representing the post-crash silence before the stop signal.
    #[test]
    fn engine_stale_partial_watchdog_force_fires_on_caught_up() {
        let stop = Arc::new(AtomicBool::new(false));
        let dec = RecordingDecoder::default();
        let lines = dec.lines.clone();
        let caught = dec.caught_up.clone();
        TranscriptTailService::new(Box::new(dec), "t")
            .with_poll_interval(Duration::ZERO)
            // ZERO threshold → `elapsed() >= ZERO` is always true,
            // so the very next Eof with non-empty partial fires the
            // watchdog. Exercise-style threshold; production uses
            // 30s. The engine still requires a non-empty `partial`
            // AND an Eof to fire — sub-threshold edge cases (e.g.
            // an Eof immediately after the first chunk before the
            // watchdog elapses in real time) are tested implicitly
            // by the other engine_truncated / engine_caught_up
            // tests, which use the default 30s threshold.
            .with_stale_partial_watchdog(Duration::ZERO)
            .run(
                ScriptedReader::new(
                    vec![
                        // Writer crashed mid-line — no closing `\n`.
                        Step::Chunk(b"{\"a\":1"),
                        // Several EOF polls; the first will be too
                        // soon to trip the 1ms watchdog (or not — the
                        // engine sleeps for poll_interval=ZERO between
                        // polls, but the watchdog measures wall-clock
                        // since last byte). Either way, by the time
                        // we reach EofStop the watchdog should have
                        // fired at least once.
                        Step::Eof,
                        Step::Eof,
                        Step::Eof,
                        Step::Eof,
                        Step::EofStop,
                    ]
                    .into_iter(),
                    stop.clone(),
                ),
                stop,
            );
        let decoded = lines.lock().unwrap().clone();
        // The orphaned partial is discarded, not delivered as an
        // event — corrupt-line safety.
        assert!(
            decoded.is_empty(),
            "watchdog must not decode the orphaned partial as a line; got: {:?}",
            decoded,
        );
        // The watchdog must have force-fired on_caught_up at least
        // once so downstream decoders can flush replay-bounded state.
        assert!(
            caught.load(Ordering::Acquire) >= 1,
            "watchdog must force-fire on_caught_up at least once",
        );
    }

    #[test]
    fn engine_truncated_partial_never_emits() {
        // A partial that never completes before stop is dropped, AND the
        // replay→live boundary signal (on_caught_up) stays parked — every
        // EOF that occurred while the partial was buffered is gated by the
        // empty-partial guard. Decoders keep accumulating replay-only state,
        // which is the correct semantic for "the transcript replay never
        // completed" (PR #302 Claude review F3).
        let (lines, caught) = drive(vec![Step::Chunk(b"{\"a\":1"), Step::EofStop]);
        assert!(lines.is_empty());
        assert_eq!(caught, 0);
    }

    #[test]
    fn engine_caught_up_defers_until_partial_completes() {
        // Regression test for PR #302 Claude review F3 — the engine must NOT
        // fire on_caught_up while a partial line is half-buffered. The
        // ordering pinned here: chunk(open) → Eof (deferred — partial
        // non-empty) → chunk(close) → EofStop (partial cleared after
        // decode_line) → on_caught_up fires exactly once.
        let (lines, caught) = drive(vec![
            Step::Chunk(b"{\"a\":1"),
            Step::Eof,
            Step::Chunk(b"23}\n"),
            Step::EofStop,
        ]);
        assert_eq!(lines, ["{\"a\":123}"]);
        assert_eq!(caught, 1);
    }

    #[test]
    fn engine_caught_up_fires_on_every_empty_eof_documenting_idempotency_requirement() {
        // PR #302 cycle 12 review F1 — pins the engine's actual behavior:
        // `on_caught_up` fires on EVERY EOF where the partial buffer is
        // empty, NOT just the first ("replay→live boundary"). Decoders
        // MUST be idempotent. Without this test, a future maintainer
        // could read the partial-guard logic alone, conclude the signal
        // fires once per replay, and write a non-idempotent
        // `on_caught_up` handler that misfires every poll cycle (~500ms)
        // during steady-state live tailing.
        //
        // Three consecutive empty-partial EOFs → three on_caught_up
        // calls. The recording decoder's `caught_up` counter is itself
        // idempotent (just increments an AtomicUsize), so this test
        // doesn't assert any specific decoder behavior — only the
        // engine's fire-cadence contract.
        let (lines, caught) = drive(vec![
            Step::Chunk(b"{\"a\":1}\n"),
            Step::Eof,
            Step::Eof,
            Step::EofStop,
        ]);
        assert_eq!(lines, ["{\"a\":1}"]);
        assert_eq!(
            caught, 3,
            "on_caught_up must fire on every empty-partial EOF (3 total: 2 Eof + 1 EofStop)",
        );
    }

    #[test]
    fn engine_strips_crlf() {
        let (lines, _) = drive(vec![Step::Chunk(b"{\"a\":1}\r\n"), Step::EofStop]);
        assert_eq!(lines, ["{\"a\":1}"]);
    }

    #[test]
    fn engine_skips_blank_line() {
        let (lines, _) = drive(vec![Step::Chunk(b"   \n"), Step::EofStop]);
        assert!(lines.is_empty());
    }

    #[test]
    fn engine_read_error_warns_and_continues() {
        // A single read error warns + sleeps + continues; the next line still
        // decodes (pins the frozen error→warn→sleep contract).
        let (lines, _) = drive(vec![
            Step::Chunk(b"{\"a\":1}\n"),
            Step::Err,
            Step::Chunk(b"{\"b\":2}\n"),
            Step::EofStop,
        ]);
        assert_eq!(lines, ["{\"a\":1}", "{\"b\":2}"]);
    }

    /// PR #302 cycle 14 retry-1 — over-cap line whose terminating
    /// newline arrives in the SAME crossing chunk must be discarded
    /// just like the skip-mode path. Codex verify caught that the
    /// initial cycle-14 fix only checked the cap in the no-newline
    /// branch — a partial near the cap followed by `b"x\n"` would
    /// reach the newline branch and decode the over-limit line.
    /// This test pins the same-chunk-newline cap-enforcement.
    #[test]
    fn engine_over_cap_line_with_newline_in_crossing_chunk_is_discarded() {
        // Build a chunk that's MAX_PARTIAL_BYTES + 1 bytes ending in
        // `\n`. Without the cap check on the newline path, the engine
        // would decode this as a complete line. With the fix, the
        // entire over-long line is discarded.
        // PR #302 cycle 19 F2: use `Step::ChunkOwned(Vec<u8>)` so the
        // 4 MiB buffer drops normally at end-of-test (pre-cycle-19
        // used `Box::leak` and permanently leaked 4 MiB per run).
        let mut big = vec![b'x'; MAX_PARTIAL_BYTES];
        big.push(b'\n'); // terminator IS in the crossing chunk
        let (lines, _) = drive(vec![
            Step::ChunkOwned(big),
            Step::Chunk(b"{\"a\":1}\n"), // next valid line decodes
            Step::EofStop,
        ]);
        assert_eq!(
            lines,
            ["{\"a\":1}"],
            "over-cap line (newline in same chunk) must be discarded, not decoded",
        );
    }

    /// PR #302 cycle 14 F1 — over-cap line is discarded and the engine
    /// enters skip-until-newline mode; processing resumes on the NEXT
    /// complete line. Uses a leaked chunk of `MAX_PARTIAL_BYTES + 1`
    /// bytes (no `\n` in it) to trip the cap on a single read; a
    /// subsequent valid line is decoded normally.
    ///
    /// Pre-cycle-14 the `read_line`-based engine would have allocated
    /// the entire over-long line into `line_buf` BEFORE checking the
    /// cap — defeating the cap's purpose. The chunked-read engine
    /// processes input as it arrives and discards once the partial
    /// would exceed the cap, so allocation is bounded by
    /// `MAX_PARTIAL_BYTES` regardless of how large the input line is.
    #[test]
    fn engine_over_cap_line_is_discarded_and_processing_resumes() {
        // Build a single chunk that exceeds MAX_PARTIAL_BYTES, with no
        // newline so the engine has to enter skip mode.
        // PR #302 cycle 19 F2: use `Step::ChunkOwned(Vec<u8>)` instead
        // of `Box::leak`-into-`&'static [u8]` so the 4 MiB buffer
        // drops normally at end-of-test.
        let big = vec![b'x'; MAX_PARTIAL_BYTES + 1];
        let (lines, _) = drive(vec![
            Step::ChunkOwned(big),
            Step::Chunk(b"\n"), // terminate the over-long line; exits skip mode
            Step::Chunk(b"{\"b\":2}\n"), // next valid line decodes normally
            Step::EofStop,
        ]);
        assert_eq!(
            lines,
            ["{\"b\":2}"],
            "over-cap line is discarded; subsequent line decodes normally",
        );
    }
}
