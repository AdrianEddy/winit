use super::*;

fn delete(before: usize, after: usize) -> Option<DeleteSurroundingText> {
    Some(DeleteSurroundingText { before, after })
}

fn done(data: &mut TextInputDataInner) -> Vec<Ime> {
    data.take_done_events().collect()
}

/// One `delete_surrounding_text`, then two `done`s: the deletion is delivered once. A later new
/// deletion is delivered once too.
#[test]
fn a_pending_delete_is_delivered_by_one_done_only() {
    let mut data = TextInputDataInner::default();
    data.pending_delete = delete(2, 1);
    assert_eq!(done(&mut data), [
        Ime::DeleteSurrounding { before_bytes: 2, after_bytes: 1 },
        Ime::Preedit(String::new(), None),
    ]);
    assert!(done(&mut data).is_empty(), "a second `done` carries no deletion");
    data.pending_delete = delete(3, 0);
    assert_eq!(done(&mut data), [
        Ime::DeleteSurrounding { before_bytes: 3, after_bytes: 0 },
        Ime::Preedit(String::new(), None),
    ]);
    assert!(done(&mut data).is_empty());
}

/// A `done` applies its pending state in text-input-v3's order — the delete, the commit (which
/// ends the preedit), the new preedit — and each of them once.
#[test]
fn a_done_applies_its_pending_state_in_the_protocols_order() {
    let mut data = TextInputDataInner::default();
    data.pending_delete = delete(1, 0);
    data.pending_commit = Some("ab".to_owned());
    data.pending_preedit =
        Some(Preedit { text: "c".to_owned(), cursor_begin: Some(1), cursor_end: None });
    assert_eq!(done(&mut data), [
        Ime::DeleteSurrounding { before_bytes: 1, after_bytes: 0 },
        Ime::Commit("ab".to_owned()),
        Ime::Preedit("c".to_owned(), Some((1, 1))),
    ]);
    assert_eq!(
        done(&mut data),
        [Ime::Preedit(String::new(), None)],
        "the preedit it left is cleared"
    );
    assert!(done(&mut data).is_empty());
}

/// A batch whose `done` delivers to no window — the window's input method disabled, or no surface —
/// is consumed by that `done`, not left for the next one. After the input method is enabled again,
/// an unrelated batch's `done` delivers that batch alone: no stale deletion, commit or preedit of
/// the first.
#[test]
fn a_done_that_delivers_nothing_discards_its_batch() {
    let mut data = TextInputDataInner::default();
    data.pending_delete = delete(1, 0);
    data.pending_commit = Some("stale".to_owned());
    data.pending_preedit = Some(Preedit::from_event(Some("x".to_owned()), 1, 1));
    data.discard_pending();
    data.pending_commit = Some("new".to_owned());
    assert_eq!(done(&mut data), [Ime::Commit("new".to_owned())]);
    assert!(done(&mut data).is_empty(), "nothing of the discarded batch arrives later");
}

/// A commit that ends a shown preedit is delivered alone: the commit replaces the preedit, so no
/// empty preedit precedes it, and the next empty `done` has nothing left to clear.
#[test]
fn a_commit_ends_a_shown_preedit_without_a_clear() {
    let mut data =
        TextInputDataInner { last_preedit_empty: false, ..TextInputDataInner::default() };
    data.pending_commit = Some("x".to_owned());
    assert_eq!(done(&mut data), [Ime::Commit("x".to_owned())]);
    assert!(done(&mut data).is_empty(), "the commit left no preedit to clear");
}

/// `preedit_string(NULL, 0, 0)` — the protocol's reset, what an input method sends to clear its
/// preedit — and an empty string with `(0, 0)` arrive as winit's "cleared" preedit, with no cursor,
/// and leave nothing owed to the next empty `done`; a non-empty preedit keeps its cursor.
#[test]
fn an_empty_preedit_carries_no_cursor() {
    for text in [None, Some(String::new())] {
        let mut data =
            TextInputDataInner { last_preedit_empty: false, ..TextInputDataInner::default() };
        data.pending_preedit = Some(Preedit::from_event(text, 0, 0));
        assert_eq!(done(&mut data), [Ime::Preedit(String::new(), None)]);
        assert!(
            done(&mut data).is_empty(),
            "the reset was the clear: the next empty `done` owes none"
        );
    }
    let mut data = TextInputDataInner::default();
    data.pending_preedit = Some(Preedit::from_event(Some("ab".to_owned()), 0, 0));
    assert_eq!(done(&mut data), [Ime::Preedit("ab".to_owned(), Some((0, 0)))]);
}

/// A reset while the preedit is already empty sends nothing: not after a commit in the same batch,
/// and not from an input method that resets at every `done`.
#[test]
fn a_reset_of_an_empty_preedit_sends_no_clear() {
    let mut data = TextInputDataInner::default();
    data.pending_commit = Some("x".to_owned());
    data.pending_preedit = Some(Preedit::from_event(None, 0, 0));
    assert_eq!(done(&mut data), [Ime::Commit("x".to_owned())]);

    data.pending_preedit = Some(Preedit::from_event(None, 0, 0));
    assert!(done(&mut data).is_empty(), "the preedit was already empty");
}

/// A batch whose `preedit_string` comes before its `commit_string` — the order wlroots' input
/// method relay sends them in — delivers both: the commit, then the new preedit. The two are
/// double-buffered apart, so the commit does not drop the preedit.
#[test]
fn a_commit_keeps_the_preedit_its_batch_set_before_it() {
    let mut data = TextInputDataInner::default();
    data.preedit_string(Some("g".to_owned()), 1, 1);
    data.commit_string(Some("日本".to_owned()));
    assert_eq!(done(&mut data), [
        Ime::Commit("日本".to_owned()),
        Ime::Preedit("g".to_owned(), Some((1, 1))),
    ]);

    // The other order delivers the same.
    data.commit_string(Some("g".to_owned()));
    data.preedit_string(Some("h".to_owned()), 1, 1);
    assert_eq!(done(&mut data), [
        Ime::Commit("g".to_owned()),
        Ime::Preedit("h".to_owned(), Some((1, 1))),
    ]);
}

/// A batch that only deletes ends with an empty preedit, so every `done` that delivers anything
/// ends with a `Commit` or a `Preedit`: while no preedit is shown, the empty preedit closes the
/// batch and leaves nothing owed to the next `done`; while one is shown, the batch's clear of it
/// is that close, sent once. A deletion beside a commit or a new preedit adds nothing.
#[test]
fn a_delete_only_batch_ends_with_an_empty_preedit() {
    let mut data = TextInputDataInner::default();
    data.delete_surrounding_text(1, 0);
    assert_eq!(done(&mut data), [
        Ime::DeleteSurrounding { before_bytes: 1, after_bytes: 0 },
        Ime::Preedit(String::new(), None),
    ]);
    assert!(done(&mut data).is_empty(), "the close owes nothing to the next `done`");

    // An empty preedit in the batch is no preedit: the batch still only deletes.
    data.delete_surrounding_text(2, 0);
    data.preedit_string(None, 0, 0);
    assert_eq!(done(&mut data), [
        Ime::DeleteSurrounding { before_bytes: 2, after_bytes: 0 },
        Ime::Preedit(String::new(), None),
    ]);

    data.preedit_string(Some("ab".to_owned()), 2, 2);
    assert_eq!(done(&mut data), [Ime::Preedit("ab".to_owned(), Some((2, 2)))]);
    data.delete_surrounding_text(1, 0);
    assert_eq!(
        done(&mut data),
        [
            Ime::DeleteSurrounding { before_bytes: 1, after_bytes: 0 },
            Ime::Preedit(String::new(), None)
        ],
        "the clear of the shown preedit closes the batch, once"
    );

    data.delete_surrounding_text(1, 0);
    data.commit_string(Some("x".to_owned()));
    assert_eq!(done(&mut data), [
        Ime::DeleteSurrounding { before_bytes: 1, after_bytes: 0 },
        Ime::Commit("x".to_owned()),
    ]);
    data.delete_surrounding_text(1, 0);
    data.preedit_string(Some("y".to_owned()), 1, 1);
    assert_eq!(done(&mut data), [
        Ime::DeleteSurrounding { before_bytes: 1, after_bytes: 0 },
        Ime::Preedit("y".to_owned(), Some((1, 1))),
    ]);
}

/// The requests issued on a text input, in order.
#[derive(Default)]
struct Requests(std::cell::RefCell<Vec<&'static str>>);

impl Requests {
    fn take(&self) -> Vec<&'static str> {
        std::mem::take(&mut self.0.borrow_mut())
    }
}

impl TextInputRequests for Requests {
    fn enable(&self) {
        self.0.borrow_mut().push("enable");
    }

    fn disable(&self) {
        self.0.borrow_mut().push("disable");
    }

    fn set_content_type(&self, _: ContentHint, _: ContentPurpose) {
        self.0.borrow_mut().push("content_type");
    }

    fn set_cursor_rectangle(&self, _: i32, _: i32, _: i32, _: i32) {
        self.0.borrow_mut().push("cursor_rectangle");
    }

    fn set_surrounding_text(&self, _: String, _: i32, _: i32) {
        self.0.borrow_mut().push("surrounding_text");
    }

    fn commit_uncounted(&self) {
        self.0.borrow_mut().push("commit");
    }
}

/// A window's input method state with a cursor area.
fn client_state() -> ClientState {
    let request_data = ImeRequestData::default().with_cursor_area(
        dpi::Position::Logical(dpi::LogicalPosition::new(10.0, 20.0)),
        dpi::Size::Logical(dpi::LogicalSize::new(1.0, 16.0)),
    );
    ClientState::new(ImeCapabilities::new().with_cursor_area(), request_data, 1.0)
}

/// An application ends a composition by disabling the input method and enabling it again. A
/// batch the compositor computed before it applied the disable can arrive after the re-enable:
/// its `done` carries a serial before the disable's commit, and is dropped with its batch. The
/// first `done` at the disable's serial is kept, and so is every later one.
#[test]
fn a_done_from_before_the_disable_is_dropped_after_the_re_enable() {
    let text_input = Requests::default();
    let mut data = TextInputDataInner::default();
    data.set_state(&text_input, Some(&client_state()), true);
    data.preedit_string(Some("ab".to_owned()), 2, 2);
    assert!(data.admit_done(1));
    assert_eq!(done(&mut data), [Ime::Preedit("ab".to_owned(), Some((2, 2)))]);

    // The application's reset: the disable is the second commit, the re-enable the third.
    data.set_state(&text_input, None, false);
    data.set_state(&text_input, Some(&client_state()), true);
    let fence = 2;
    assert_eq!(data.fence, Some(fence));

    // The input method's commit of "ab", which the compositor sent before it applied the disable.
    data.commit_string(Some("ab".to_owned()));
    assert!(!data.admit_done(fence - 1), "a batch from before the disable is dropped");
    data.discard_pending();
    data.commit_string(Some("ab".to_owned()));
    assert!(!data.admit_done(fence - 1), "however many arrive");
    data.discard_pending();

    data.preedit_string(Some("x".to_owned()), 1, 1);
    assert!(data.admit_done(fence), "the disable's own serial is past the old state");
    assert_eq!(done(&mut data), [Ime::Preedit("x".to_owned(), Some((1, 1)))]);
    assert_eq!(data.fence, None, "a `done` at the fence retires it");
    assert!(data.admit_done(fence + 1));
}

/// Every `commit` is counted, from every request that sends one: the disable, the enable with the
/// state, and an update of the state alone. A disable also discards the pending batch and the
/// shown preedit, which the compositor resets with the state.
#[test]
fn every_commit_is_counted_and_each_disable_sets_the_fence() {
    let text_input = Requests::default();
    let mut data = TextInputDataInner::default();

    data.set_state(&text_input, Some(&client_state()), true);
    assert_eq!(text_input.take(), ["enable", "cursor_rectangle", "commit"]);
    data.set_state(&text_input, Some(&client_state()), false);
    assert_eq!(text_input.take(), ["cursor_rectangle", "commit"]);
    assert_eq!((data.commits, data.fence), (2, None));

    data.preedit_string(Some("ab".to_owned()), 2, 2);
    assert!(data.admit_done(2));
    assert_eq!(done(&mut data), [Ime::Preedit("ab".to_owned(), Some((2, 2)))]);
    data.commit_string(Some("pending".to_owned()));
    data.set_state(&text_input, None, false);
    assert_eq!(text_input.take(), ["disable", "commit"]);
    assert_eq!((data.commits, data.fence), (3, Some(3)));

    // What the disable reset is gone: no pending commit, and no preedit left to clear.
    assert!(data.admit_done(3));
    assert!(done(&mut data).is_empty());

    // `leave` disables through the same method.
    data.disable(&text_input);
    assert_eq!(text_input.take(), ["disable", "commit"]);
    assert_eq!((data.commits, data.fence), (4, Some(4)));
}

/// Several resets before any `done`: the fence is the latest disable's. A batch the compositor
/// computed between the first re-enable and the second disable is dropped too: it was computed for
/// a state the second disable ended.
#[test]
fn the_fence_is_the_latest_disable() {
    let text_input = Requests::default();
    let mut data = TextInputDataInner::default();
    for _ in 0..2 {
        data.set_state(&text_input, None, false);
        data.set_state(&text_input, Some(&client_state()), true);
    }
    assert_eq!((data.commits, data.fence), (4, Some(3)));
    assert!(!data.admit_done(1));
    assert!(!data.admit_done(2), "computed after the first re-enable, before the second disable");
    assert!(data.admit_done(3));
}

/// A `leave` disables the text input and fences it; an `enter` enables it again with the window's
/// state and leaves the fence standing: a batch the compositor computed before the `leave`'s
/// disable is still dropped after the `enter`, and the first `done` at the fence is kept.
#[test]
fn an_enter_while_fenced_keeps_dropping_the_batches_from_before_the_disable() {
    let text_input = Requests::default();
    let mut data = TextInputDataInner::default();
    data.set_state(&text_input, Some(&client_state()), true);
    data.disable(&text_input);
    let fence = data.commits;
    data.set_state(&text_input, Some(&client_state()), true);
    assert_eq!(text_input.take(), [
        "enable",
        "cursor_rectangle",
        "commit",
        "disable",
        "commit",
        "enable",
        "cursor_rectangle",
        "commit",
    ]);
    assert_eq!(data.fence, Some(fence), "the `enter` leaves the fence");

    data.commit_string(Some("stale".to_owned()));
    assert!(!data.admit_done(fence - 1), "computed before the disable");
    data.discard_pending();
    assert!(data.admit_done(fence));
    assert_eq!(data.fence, None);
}

/// The count wraps, and so does the comparison: a fence at `u32::MAX` drops the serials before it
/// and keeps the ones after it, past zero.
#[test]
fn the_fence_compares_wrapping() {
    let text_input = Requests::default();
    let mut data = TextInputDataInner { commits: u32::MAX - 1, ..TextInputDataInner::default() };
    data.set_state(&text_input, None, false);
    data.set_state(&text_input, Some(&client_state()), true);
    assert_eq!((data.commits, data.fence), (0, Some(u32::MAX)));
    assert!(!data.admit_done(u32::MAX - 1));
    assert!(!data.admit_done(u32::MAX - 2));
    assert!(data.admit_done(0), "past the fence, wrapped");
    assert_eq!(data.fence, None);

    let mut data = TextInputDataInner { commits: u32::MAX - 1, ..TextInputDataInner::default() };
    data.set_state(&text_input, None, false);
    assert!(data.admit_done(u32::MAX), "the fence's own serial");
}
