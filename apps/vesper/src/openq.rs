//! Latest-request bookkeeping for opening a video without ever blocking the window.
//!
//! Two slow things sit between "the owner chose a file" and "the decoder is told to load it":
//! checking the file on a slow or busy disk, and starting the decoder (loading the DLL and
//! `mpv_initialize` can take seconds from a cold spinning disk). Both run on worker threads. This
//! type keeps at most one worker of each kind and at most one request waiting behind it: a newer
//! request replaces an older waiting one, a cancel drops what has not started, and a failure drops
//! the request. The decoder-side request identity (generation/playlist-entry repair) is untouched:
//! it begins only when `Player::open_validated` is called with the one surviving file.
#[derive(Debug, PartialEq, Eq)]
pub enum Validate<I> {
    /// Check this input now on a worker.
    Start(I),
    /// A check is already running; this input waits behind it and replaces any older waiter.
    Queued,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Checked<I> {
    /// The finished check is the latest request: use its result.
    Deliver,
    /// A newer request arrived while it ran: drop the result and check this one next.
    Restart(I),
    /// The owner cancelled while it ran: drop the result.
    Discard,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Step<T> {
    /// The decoder is ready: start this file now.
    LoadNow(T),
    /// The decoder is starting: the request is queued; start the decoder if it is not yet.
    WaitForDecoder { start_decoder: bool },
}

#[derive(Debug, Default, PartialEq, Eq)]
enum Decoder {
    #[default]
    NotStarted,
    Starting,
    Ready,
}

#[derive(Debug)]
pub struct OpenQueue<I, T> {
    decoder: Decoder,
    pending: Option<T>,
    checking: bool,
    next_input: Option<I>,
    discard: bool,
}

impl<I, T> Default for OpenQueue<I, T> {
    fn default() -> Self {
        Self {
            decoder: Decoder::NotStarted,
            pending: None,
            checking: false,
            next_input: None,
            discard: false,
        }
    }
}

impl<I, T> OpenQueue<I, T> {
    // ---- stage 1: file checks ----------------------------------------------------------
    /// A newer request supersedes everything older that has not reached the decoder, including a
    /// file that was already checked and is waiting for the decoder to start: when the decoder
    /// finishes starting it must not load a file the owner has since replaced.
    pub fn request_check(&mut self, input: I) -> Validate<I> {
        self.discard = false;
        self.pending = None;
        if self.checking {
            self.next_input = Some(input);
            Validate::Queued
        } else {
            self.checking = true;
            Validate::Start(input)
        }
    }
    pub fn check_finished(&mut self) -> Checked<I> {
        if self.discard {
            self.discard = false;
            self.next_input = None;
            self.checking = false;
            return Checked::Discard;
        }
        match self.next_input.take() {
            Some(next) => Checked::Restart(next),
            None => {
                self.checking = false;
                Checked::Deliver
            }
        }
    }
    pub fn checking(&self) -> bool {
        self.checking
    }

    // ---- stage 2: decoder start -----------------------------------------------------------
    /// Mark the decoder as starting before any file has been checked, so its start overlaps the
    /// file check instead of following it. True when the caller must start the worker now; false
    /// when it is already starting or ready. A request that arrives later waits or loads exactly
    /// as it would have after `request` started the decoder itself.
    pub fn begin_decoder(&mut self) -> bool {
        if self.decoder == Decoder::NotStarted {
            self.decoder = Decoder::Starting;
            true
        } else {
            false
        }
    }
    pub fn request(&mut self, item: T) -> Step<T> {
        match self.decoder {
            Decoder::Ready => Step::LoadNow(item),
            Decoder::Starting => {
                self.pending = Some(item);
                Step::WaitForDecoder {
                    start_decoder: false,
                }
            }
            Decoder::NotStarted => {
                self.decoder = Decoder::Starting;
                self.pending = Some(item);
                Step::WaitForDecoder {
                    start_decoder: true,
                }
            }
        }
    }
    /// Drop a request that has not started loading. Returns whether anything was dropped.
    pub fn cancel(&mut self) -> bool {
        let mut dropped = self.pending.take().is_some();
        if self.checking {
            self.discard = true;
            self.next_input = None;
            dropped = true;
        }
        dropped
    }
    pub fn waiting(&self) -> bool {
        self.pending.is_some()
    }
    pub fn starting(&self) -> bool {
        self.decoder == Decoder::Starting
    }
    pub fn ready(&self) -> bool {
        self.decoder == Decoder::Ready
    }
    /// The worker finished. On success the surviving request, if any, is returned to start.
    /// On failure the request is dropped and a later request starts the decoder afresh.
    pub fn decoder_finished(&mut self, ok: bool) -> Option<T> {
        if ok {
            self.decoder = Decoder::Ready;
            self.pending.take()
        } else {
            self.decoder = Decoder::NotStarted;
            self.pending = None;
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    type Queue = OpenQueue<PathBuf, PathBuf>;
    fn p(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    #[test]
    fn first_request_starts_the_decoder_exactly_once() {
        let mut q = Queue::default();
        assert_eq!(
            q.request(p("a.mp4")),
            Step::WaitForDecoder {
                start_decoder: true
            }
        );
        assert_eq!(
            q.request(p("b.mp4")),
            Step::WaitForDecoder {
                start_decoder: false
            }
        );
        assert!(q.starting() && q.waiting());
    }

    #[test]
    fn only_the_latest_request_survives_the_wait() {
        let mut q = Queue::default();
        for name in ["a.mp4", "b.mkv", "c.webm"] {
            q.request(p(name));
        }
        assert_eq!(q.decoder_finished(true), Some(p("c.webm")));
        assert!(!q.waiting() && q.ready());
        assert_eq!(q.decoder_finished(true), None);
    }

    #[test]
    fn a_ready_decoder_loads_immediately_and_replaces_without_queueing() {
        let mut q = Queue::default();
        q.request(p("a.mp4"));
        q.decoder_finished(true);
        assert_eq!(q.request(p("b.mp4")), Step::LoadNow(p("b.mp4")));
        assert_eq!(q.request(p("c.mp4")), Step::LoadNow(p("c.mp4")));
        assert!(!q.waiting());
    }

    #[test]
    fn an_early_decoder_start_happens_once_and_a_later_request_does_not_restart_it() {
        let mut q = Queue::default();
        assert!(q.begin_decoder());
        assert!(!q.begin_decoder(), "already starting");
        assert!(q.starting() && !q.waiting());
        // The checked file arrives while the decoder is still starting: it waits, no second start.
        assert_eq!(
            q.request(p("a.mp4")),
            Step::WaitForDecoder {
                start_decoder: false
            }
        );
        assert_eq!(q.decoder_finished(true), Some(p("a.mp4")));
        assert!(!q.begin_decoder(), "ready decoders are not started again");
    }

    #[test]
    fn a_decoder_that_finishes_before_the_file_is_checked_loads_it_at_once() {
        let mut q = Queue::default();
        assert!(q.begin_decoder());
        assert_eq!(q.request_check(p("a.mp4")), Validate::Start(p("a.mp4")));
        // The decoder is ready first, with nothing to load yet.
        assert_eq!(q.decoder_finished(true), None);
        assert!(q.ready());
        assert_eq!(q.check_finished(), Checked::Deliver);
        assert_eq!(q.request(p("a.mp4")), Step::LoadNow(p("a.mp4")));
    }

    #[test]
    fn an_early_decoder_start_that_fails_can_be_tried_again_by_the_next_request() {
        let mut q = Queue::default();
        assert!(q.begin_decoder());
        assert_eq!(q.decoder_finished(false), None);
        assert!(!q.starting() && !q.ready());
        assert!(q.begin_decoder());
    }

    #[test]
    fn a_rejected_file_after_an_early_start_leaves_a_ready_decoder_and_nothing_queued() {
        let mut q = Queue::default();
        assert!(q.begin_decoder());
        q.request_check(p("bad.mp4"));
        assert_eq!(q.decoder_finished(true), None);
        assert_eq!(q.check_finished(), Checked::Deliver); // the caller reports the error and requests nothing
        assert!(q.ready() && !q.waiting() && !q.checking());
        // The next good file loads immediately.
        assert_eq!(q.request(p("good.mp4")), Step::LoadNow(p("good.mp4")));
    }

    #[test]
    fn cancel_drops_the_request_but_keeps_the_starting_decoder() {
        let mut q = Queue::default();
        q.request(p("a.mp4"));
        assert!(q.cancel());
        assert!(!q.cancel());
        assert!(q.starting());
        assert_eq!(q.decoder_finished(true), None);
        assert!(q.ready());
    }

    #[test]
    fn failed_start_drops_the_request_and_allows_a_fresh_attempt() {
        let mut q = Queue::default();
        q.request(p("a.mp4"));
        assert_eq!(q.decoder_finished(false), None);
        assert!(!q.starting() && !q.ready() && !q.waiting());
        assert_eq!(
            q.request(p("b.mp4")),
            Step::WaitForDecoder {
                start_decoder: true
            }
        );
    }

    #[test]
    fn request_after_cancel_while_starting_waits_without_a_second_start() {
        let mut q = Queue::default();
        q.request(p("a.mp4"));
        q.cancel();
        assert_eq!(
            q.request(p("b.mp4")),
            Step::WaitForDecoder {
                start_decoder: false
            }
        );
        assert_eq!(q.decoder_finished(true), Some(p("b.mp4")));
    }

    #[test]
    fn one_check_runs_at_a_time_and_the_newest_waiter_wins() {
        let mut q = Queue::default();
        assert_eq!(q.request_check(p("a.mp4")), Validate::Start(p("a.mp4")));
        assert_eq!(q.request_check(p("b.mp4")), Validate::Queued);
        assert_eq!(q.request_check(p("c.mp4")), Validate::Queued);
        // a finishes, but c is newer: a's result is dropped and c is checked next.
        assert_eq!(q.check_finished(), Checked::Restart(p("c.mp4")));
        assert!(q.checking());
        assert_eq!(q.check_finished(), Checked::Deliver);
        assert!(!q.checking());
        assert_eq!(q.request_check(p("d.mp4")), Validate::Start(p("d.mp4")));
    }

    #[test]
    fn cancelling_a_running_check_drops_its_result_and_any_waiter() {
        let mut q = Queue::default();
        q.request_check(p("a.mp4"));
        q.request_check(p("b.mp4"));
        assert!(q.cancel());
        assert_eq!(q.check_finished(), Checked::Discard);
        assert!(!q.checking());
        // A fresh request after the cancel starts normally.
        assert_eq!(q.request_check(p("c.mp4")), Validate::Start(p("c.mp4")));
        assert_eq!(q.check_finished(), Checked::Deliver);
    }

    #[test]
    fn a_new_request_after_cancel_before_the_check_ends_is_the_one_delivered() {
        let mut q = Queue::default();
        q.request_check(p("a.mp4"));
        q.cancel();
        // The owner changed their mind again before the old check came back.
        assert_eq!(q.request_check(p("b.mp4")), Validate::Queued);
        assert_eq!(q.check_finished(), Checked::Restart(p("b.mp4")));
        assert_eq!(q.check_finished(), Checked::Deliver);
    }

    #[test]
    fn a_newer_request_keeps_the_decoder_from_loading_the_superseded_file() {
        // A is checked and waits for the decoder; B is requested before the decoder finishes.
        let mut q = Queue::default();
        assert_eq!(q.request_check(p("a.mp4")), Validate::Start(p("a.mp4")));
        assert_eq!(q.check_finished(), Checked::Deliver);
        assert_eq!(
            q.request(p("a.mp4")),
            Step::WaitForDecoder {
                start_decoder: true
            }
        );
        assert_eq!(q.request_check(p("b.mp4")), Validate::Start(p("b.mp4")));
        // The decoder becomes ready while B is still being checked: nothing loads.
        assert_eq!(q.decoder_finished(true), None);
        assert!(q.ready() && !q.waiting() && q.checking());
        // B is valid: it loads immediately, and it is the only file that ever does.
        assert_eq!(q.check_finished(), Checked::Deliver);
        assert_eq!(q.request(p("b.mp4")), Step::LoadNow(p("b.mp4")));
    }

    #[test]
    fn a_rejected_newer_request_leaves_nothing_to_load_and_a_ready_decoder_alone() {
        let mut q = Queue::default();
        q.request_check(p("a.mp4"));
        q.check_finished();
        q.request(p("a.mp4"));
        q.request_check(p("bad.mp4"));
        assert_eq!(q.check_finished(), Checked::Deliver); // the caller sees the error, requests nothing
        assert_eq!(q.decoder_finished(true), None);
        assert!(q.ready() && !q.waiting());
        // Playback that was already running is not touched by a rejected check (nothing queued).
        assert_eq!(q.request(p("c.mp4")), Step::LoadNow(p("c.mp4")));
    }

    #[test]
    fn cancelling_while_the_newer_file_is_checked_loads_nothing_when_the_decoder_arrives() {
        let mut q = Queue::default();
        q.request_check(p("a.mp4"));
        q.check_finished();
        q.request(p("a.mp4"));
        q.request_check(p("b.mp4"));
        assert!(q.cancel());
        assert_eq!(q.decoder_finished(true), None);
        assert_eq!(q.check_finished(), Checked::Discard);
        assert!(!q.waiting() && !q.checking() && q.ready());
    }

    #[test]
    fn rapid_replacement_always_ends_with_exactly_the_last_file_loading() {
        // 30 requests while a check and the decoder are both slow: one file survives.
        let mut q = Queue::default();
        let mut starts = 0;
        for i in 0..30 {
            if let Validate::Start(_) = q.request_check(p(&format!("f{i}.mp4"))) {
                starts += 1;
            }
        }
        assert_eq!(starts, 1);
        let mut last = p("none");
        loop {
            match q.check_finished() {
                Checked::Restart(next) => last = next,
                Checked::Deliver => break,
                Checked::Discard => unreachable!(),
            }
        }
        assert_eq!(last, p("f29.mp4"));
        q.request(last.clone());
        assert_eq!(q.decoder_finished(true), Some(p("f29.mp4")));
    }
}
