use winit_core::window::{ImeEnableRequest, ImeRequest};

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
    data.set_state(&text_input, &client_state(), true);
    data.preedit_string(Some("ab".to_owned()), 2, 2);
    assert!(data.admit_done(1));
    assert_eq!(done(&mut data), [Ime::Preedit("ab".to_owned(), Some((2, 2)))]);

    // The application's reset: the disable is the second commit, the re-enable the third.
    let _ = data.disable(&text_input);
    data.set_state(&text_input, &client_state(), true);
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

    data.set_state(&text_input, &client_state(), true);
    assert_eq!(text_input.take(), ["enable", "cursor_rectangle", "commit"]);
    data.set_state(&text_input, &client_state(), false);
    assert_eq!(text_input.take(), ["cursor_rectangle", "commit"]);
    assert_eq!((data.commits, data.fence), (2, None));

    data.preedit_string(Some("ab".to_owned()), 2, 2);
    assert!(data.admit_done(2));
    assert_eq!(done(&mut data), [Ime::Preedit("ab".to_owned(), Some((2, 2)))]);
    data.commit_string(Some("pending".to_owned()));
    assert_eq!(data.disable(&text_input), 3, "the disable's commit is its fence");
    assert_eq!(text_input.take(), ["disable", "commit"]);
    assert_eq!((data.commits, data.fence), (3, Some(3)));

    // What the disable reset is gone: no pending commit, and no preedit left to clear.
    assert!(data.admit_done(3));
    assert!(done(&mut data).is_empty());

    // `leave` disables through the same method.
    assert_eq!(data.disable(&text_input), 4);
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
        let _ = data.disable(&text_input);
        data.set_state(&text_input, &client_state(), true);
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
    data.set_state(&text_input, &client_state(), true);
    let _ = data.disable(&text_input);
    let fence = data.commits;
    data.set_state(&text_input, &client_state(), true);
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
    let _ = data.disable(&text_input);
    data.set_state(&text_input, &client_state(), true);
    assert_eq!((data.commits, data.fence), (0, Some(u32::MAX)));
    assert!(!data.admit_done(u32::MAX - 1));
    assert!(!data.admit_done(u32::MAX - 2));
    assert!(data.admit_done(0), "past the fence, wrapped");
    assert_eq!(data.fence, None);

    let mut data = TextInputDataInner { commits: u32::MAX - 1, ..TextInputDataInner::default() };
    let _ = data.disable(&text_input);
    assert!(data.admit_done(u32::MAX), "the fence's own serial");
}

/// A text input for the window tests: the requests issued on it, its data, and whether it was
/// destroyed.
#[derive(Clone, Default)]
struct Fake(std::rc::Rc<FakeInner>);

#[derive(Default)]
struct FakeInner {
    requests: Requests,
    data: std::cell::RefCell<TextInputDataInner>,
    destroyed: std::cell::Cell<bool>,
}

impl Fake {
    fn requests(&self) -> Vec<&'static str> {
        self.0.requests.take()
    }

    fn fence(&self) -> Option<u32> {
        self.0.data.borrow().fence
    }

    fn destroy(&self) {
        self.0.destroyed.set(true);
    }
}

impl PartialEq for Fake {
    fn eq(&self, other: &Self) -> bool {
        std::rc::Rc::ptr_eq(&self.0, &other.0)
    }
}

impl std::fmt::Debug for Fake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fake({:p})", std::rc::Rc::as_ptr(&self.0))
    }
}

impl TextInputRequests for Fake {
    fn enable(&self) {
        self.0.requests.enable()
    }

    fn disable(&self) {
        self.0.requests.disable()
    }

    fn set_content_type(&self, hint: ContentHint, purpose: ContentPurpose) {
        self.0.requests.set_content_type(hint, purpose)
    }

    fn set_cursor_rectangle(&self, x: i32, y: i32, width: i32, height: i32) {
        self.0.requests.set_cursor_rectangle(x, y, width, height)
    }

    fn set_surrounding_text(&self, text: String, cursor: i32, anchor: i32) {
        self.0.requests.set_surrounding_text(text, cursor, anchor)
    }

    fn commit_uncounted(&self) {
        self.0.requests.commit_uncounted()
    }
}

impl TextInputHandle for Fake {
    fn with_data<R>(&self, f: impl FnOnce(&mut TextInputDataInner) -> R) -> R {
        f(&mut self.0.data.borrow_mut())
    }

    fn is_alive(&self) -> bool {
        !self.0.destroyed.get()
    }
}

/// The syncs a window's requests send, in order, each as the fences it lists.
type Syncs = Vec<Vec<(Fake, u32)>>;

fn enable() -> ImeRequest {
    let request_data = ImeRequestData::default().with_cursor_area(
        dpi::Position::Logical(dpi::LogicalPosition::new(10.0, 20.0)),
        dpi::Size::Logical(dpi::LogicalSize::new(1.0, 16.0)),
    );
    ImeRequest::Enable(
        ImeEnableRequest::new(ImeCapabilities::new().with_cursor_area(), request_data).unwrap(),
    )
}

/// Sends `request` to `window`, collecting its sync.
fn request(window: &mut ImeWindow<Fake>, syncs: &mut Syncs, request: ImeRequest) -> Option<bool> {
    window.request(request, 1.0, |fences| syncs.push(fences)).unwrap()
}

/// An application's reset of a window's input method: a disable, then an enable.
fn reset(window: &mut ImeWindow<Fake>, syncs: &mut Syncs) {
    assert_eq!(request(window, syncs, ImeRequest::Disable), Some(false));
    assert_eq!(request(window, syncs, enable()), Some(true));
}

/// The compositor's dispatch of the first sync still waiting, then the window's opening if it is
/// due: whether it pushes the window's `Ime::Enabled`.
fn callback(window: &mut ImeWindow<Fake>, syncs: &mut Syncs) -> bool {
    retire(&syncs.remove(0));
    window.take_opening()
}

/// An enable sends no `Ime::Enabled` of its own: the window opens at the compositor's dispatch of
/// the sync after the disable, once the disable's fence is retired, and only then. On the wire the
/// reset is `disable, commit, sync, sync, enable, state, commit`.
#[test]
fn a_reset_opens_the_window_at_the_disables_callback() {
    let a = Fake::default();
    let (mut window, mut syncs) = (ImeWindow::default(), Syncs::new());
    assert!(!window.enter(&a), "the input method is disabled: nothing opens");
    assert_eq!(request(&mut window, &mut syncs, enable()), Some(true));
    assert!(callback(&mut window, &mut syncs), "the first enable opens at its own callback");
    a.requests();

    reset(&mut window, &mut syncs);
    assert_eq!(a.requests(), ["disable", "commit", "enable", "cursor_rectangle", "commit"]);
    assert_eq!(syncs.len(), 2, "one sync per transition");
    assert_eq!(syncs[0], [(a.clone(), 2)], "the disable's sync lists its fence");
    assert!(syncs[1].is_empty());
    assert!(!window.take_opening(), "the fence is not retired yet");

    assert!(callback(&mut window, &mut syncs), "the disable's callback opens the window");
    assert_eq!(a.fence(), None);
    assert!(!callback(&mut window, &mut syncs), "the enable's callback opens nothing more");
    assert!(!window.take_opening());
}

/// Several resets before any callback: each callback retires only its own fence, so the ones of
/// the earlier resets retire nothing, and the window opens once, at the callback after the last
/// disable.
#[test]
fn only_the_latest_disables_callback_opens_the_window() {
    let a = Fake::default();
    let (mut window, mut syncs) = (ImeWindow::default(), Syncs::new());
    window.enter(&a);
    request(&mut window, &mut syncs, enable());
    assert!(callback(&mut window, &mut syncs));

    for _ in 0..3 {
        reset(&mut window, &mut syncs);
    }
    assert_eq!(syncs.len(), 6);
    for _ in 0..4 {
        assert!(!callback(&mut window, &mut syncs), "an earlier reset's callback opens nothing");
        assert!(a.fence().is_some());
    }
    assert!(callback(&mut window, &mut syncs), "the last disable's callback opens the window");
    assert!(!callback(&mut window, &mut syncs));
}

/// A window entered by two seats' text inputs: the reset disables both and its one sync lists both
/// fences; its callback retires both, and the window opens once.
#[test]
fn one_sync_retires_every_member_and_opens_the_window_once() {
    let (a, b) = (Fake::default(), Fake::default());
    let (mut window, mut syncs) = (ImeWindow::default(), Syncs::new());
    request(&mut window, &mut syncs, enable());
    assert!(window.enter(&a));
    assert!(!window.enter(&b), "the window is open already");
    assert!(!callback(&mut window, &mut syncs), "the entered window opened already");

    reset(&mut window, &mut syncs);
    assert_eq!(syncs[0].len(), 2);
    assert!(callback(&mut window, &mut syncs));
    assert_eq!((a.fence(), b.fence()), (None, None));
    assert!(!callback(&mut window, &mut syncs));
}

/// Seat B leaves the window during a reset: it is no longer a member, so the window opens when A,
/// the one member left, is ready, with no new `enter`. B's own `leave` disable replaced the fence
/// the reset listed for it, so the reset's callback leaves B's fence to B's own sync.
#[test]
fn a_member_that_leaves_mid_reset_does_not_hold_the_window() {
    let (a, b) = (Fake::default(), Fake::default());
    let (mut window, mut syncs) = (ImeWindow::default(), Syncs::new());
    request(&mut window, &mut syncs, enable());
    window.enter(&a);
    window.enter(&b);
    syncs.clear();

    reset(&mut window, &mut syncs);
    let leave_fence = b.with_data(|data| data.disable(&b));
    assert!(window.remove(&b).is_empty(), "A stays: no `Ime::Disabled`");
    assert!(callback(&mut window, &mut syncs), "A is ready, and B is no member");
    assert_eq!(b.fence(), Some(leave_fence), "B's `leave` fence waits for its own sync");
}

/// A destroyed member never answers its sync; it does not hold the window shut.
#[test]
fn a_destroyed_member_does_not_hold_the_window() {
    let (a, b) = (Fake::default(), Fake::default());
    let (mut window, mut syncs) = (ImeWindow::default(), Syncs::new());
    request(&mut window, &mut syncs, enable());
    window.enter(&a);
    window.enter(&b);
    syncs.clear();

    reset(&mut window, &mut syncs);
    b.destroy();
    assert!(callback(&mut window, &mut syncs));
    assert!(b.fence().is_some(), "a destroyed text input is not retired");
}

/// A disable with no re-enable owes nothing: its callback retires the fence and opens nothing. A
/// later enable owes the opening, which its own callback pushes.
#[test]
fn a_disable_alone_opens_nothing_and_a_later_enable_opens_at_its_callback() {
    let a = Fake::default();
    let (mut window, mut syncs) = (ImeWindow::default(), Syncs::new());
    request(&mut window, &mut syncs, enable());
    window.enter(&a);
    syncs.clear();

    request(&mut window, &mut syncs, ImeRequest::Disable);
    assert!(!callback(&mut window, &mut syncs));
    assert_eq!(a.fence(), None);

    request(&mut window, &mut syncs, enable());
    assert!(callback(&mut window, &mut syncs));
}

/// An enable while no text input is entered owes the opening, and the first `enter` pays it: the
/// `enter`'s own `Ime::Enabled` is the opening, and no callback pushes another.
#[test]
fn an_enable_with_no_member_opens_at_the_enter() {
    let a = Fake::default();
    let (mut window, mut syncs) = (ImeWindow::default(), Syncs::new());
    request(&mut window, &mut syncs, enable());
    assert!(!callback(&mut window, &mut syncs), "no member: nothing opens");
    assert!(window.enter(&a));
    assert!(!window.take_opening(), "the `enter` paid the opening");
}

/// An update of the state is no transition: it sends the state with its commit, and no sync.
#[test]
fn an_update_sends_no_sync() {
    let a = Fake::default();
    let (mut window, mut syncs) = (ImeWindow::default(), Syncs::new());
    request(&mut window, &mut syncs, enable());
    window.enter(&a);
    syncs.clear();
    a.requests();

    let update = ImeRequestData::default().with_cursor_area(
        dpi::Position::Logical(dpi::LogicalPosition::new(30.0, 20.0)),
        dpi::Size::Logical(dpi::LogicalSize::new(1.0, 16.0)),
    );
    assert_eq!(request(&mut window, &mut syncs, ImeRequest::Update(update)), None);
    assert_eq!(a.requests(), ["cursor_rectangle", "commit"]);
    assert!(syncs.is_empty());
}

/// A compositor that answers every batch with `done(0)` — no count of commits at all — loses only
/// what it sends while a fence stands: every `done` is behind the fence and dropped until the
/// disable's callback retires it, one round trip after the disable, and every `done` after the
/// callback is delivered. The window opens at that callback, as with a conforming compositor.
#[test]
fn a_compositor_answering_done_zero_loses_only_the_batches_sent_while_fenced() {
    let a = Fake::default();
    let (mut window, mut syncs) = (ImeWindow::default(), Syncs::new());
    request(&mut window, &mut syncs, enable());
    window.enter(&a);
    syncs.clear();
    assert!(a.with_data(|data| data.admit_done(0)), "no fence: `done(0)` is delivered");

    reset(&mut window, &mut syncs);
    assert!(!a.with_data(|data| data.admit_done(0)), "while fenced, `done(0)` is behind the fence");
    assert!(!a.with_data(|data| data.admit_done(0)), "however many come");

    assert!(callback(&mut window, &mut syncs), "the disable's callback retires it and opens");
    assert!(a.with_data(|data| data.admit_done(0)), "after the callback `done(0)` is delivered");
}

/// The input method of `member` sends a batch: `batch` sets the pending state, and the `done`
/// carries the serial of the text input's latest commit. Returns the events the window delivers.
fn send(
    window: &mut ImeWindow<Fake>,
    syncs: &mut Syncs,
    member: &Fake,
    batch: impl FnOnce(&mut TextInputDataInner),
) -> Vec<Ime> {
    let serial = member.with_data(|data| {
        batch(data);
        data.commits
    });
    window.done(member, serial, |fences| syncs.push(fences))
}

fn preedit(text: &str) -> impl FnOnce(&mut TextInputDataInner) + '_ {
    move |data| data.preedit_string(Some(text.to_owned()), text.len() as i32, text.len() as i32)
}

fn commit(text: &str) -> impl FnOnce(&mut TextInputDataInner) + '_ {
    move |data| data.commit_string(Some(text.to_owned()))
}

fn shown(text: &str) -> Ime {
    Ime::Preedit(text.to_owned(), Some((text.len(), text.len())))
}

fn cleared() -> Ime {
    Ime::Preedit(String::new(), None)
}

/// A window with its input method enabled and open, entered by `seats` text inputs; no request or
/// sync is left to read.
fn entered(seats: usize) -> (ImeWindow<Fake>, Syncs, Vec<Fake>) {
    let (mut window, mut syncs) = (ImeWindow::default(), Syncs::new());
    request(&mut window, &mut syncs, enable());
    let members: Vec<Fake> = (0..seats).map(|_| Fake::default()).collect();
    for member in &members {
        window.enter(member);
        member.requests();
    }
    assert!(!callback(&mut window, &mut syncs));
    (window, syncs, members)
}

/// The composing seat departs — by `leave`, or by its seat's removal with no `leave` — while an
/// idle seat stays: the departing composition ends with one empty preedit, no `Ime::Disabled` is
/// pushed, and the window stays open: the seat that stays composes at once.
#[test]
fn the_composer_departing_ends_its_composition_and_the_window_stays_open() {
    for by_leave in [true, false] {
        let (mut window, mut syncs, members) = entered(2);
        let (a, b) = (&members[0], &members[1]);
        assert_eq!(send(&mut window, &mut syncs, a, preedit("p")), [shown("p")]);

        if by_leave {
            a.with_data(|data| data.disable(a));
        } else {
            a.destroy();
        }
        assert_eq!(window.remove(a), [cleared()], "the composition ends, the window stays");
        assert!(!window.take_opening());
        assert_eq!(send(&mut window, &mut syncs, b, preedit("x")), [shown("x")]);
    }
}

/// An idle seat departs while another composes: nothing is pushed, and the composition goes on.
#[test]
fn an_idle_seat_departing_pushes_nothing() {
    let (mut window, mut syncs, members) = entered(2);
    let (a, b) = (&members[0], &members[1]);
    assert_eq!(send(&mut window, &mut syncs, a, preedit("p")), [shown("p")]);
    assert!(window.remove(b).is_empty());
    assert_eq!(send(&mut window, &mut syncs, a, preedit("pq")), [shown("pq")]);
}

/// The last member departs while it composes: the window's `Ime::Disabled` alone, with no empty
/// preedit before it.
#[test]
fn the_last_member_departing_pushes_disabled_alone() {
    let (mut window, mut syncs, members) = entered(1);
    let a = &members[0];
    assert_eq!(send(&mut window, &mut syncs, a, preedit("p")), [shown("p")]);
    assert_eq!(window.remove(a), [Ime::Disabled]);
}

/// A removal is one operation: a second removal of the same text input, or of one that never
/// entered, pushes nothing. Each removal discards the member's pending batch.
#[test]
fn a_removal_happens_once_and_discards_the_pending_batch() {
    let (mut window, mut syncs, members) = entered(2);
    let (a, b) = (&members[0], &members[1]);
    a.with_data(commit("stale"));
    assert!(window.remove(a).is_empty());
    assert!(window.remove(a).is_empty(), "removed already");
    assert!(window.remove(&Fake::default()).is_empty(), "never entered");
    assert_eq!(window.remove(b), [Ime::Disabled], "B was the last member");

    window.enter(a);
    assert!(send(&mut window, &mut syncs, a, |_| ()).is_empty(), "nothing stale is delivered");
}

/// One batch of a table of cases: the events the compositor sends before a `done`.
type Batch<'a> = &'a dyn Fn(&mut TextInputDataInner);

/// Another seat's batch with input takes the composition over, by a preedit, a commit or a
/// deletion: the owner's composition ends first with an empty preedit, and the owner's text input
/// alone is reset natively — its disable with a fence, a sync listing it alone, its enable with
/// the window's state — so its input method holds the ended composition no longer. The reset owes
/// the window no opening.
#[test]
fn a_takeover_ends_the_owners_composition_first() {
    let deletion = |data: &mut TextInputDataInner| data.delete_surrounding_text(1, 0);
    let cases: [(Batch<'_>, Vec<Ime>); 3] = [
        (&|data: &mut TextInputDataInner| preedit("x")(data), vec![cleared(), shown("x")]),
        (&|data: &mut TextInputDataInner| commit("x")(data), vec![
            cleared(),
            Ime::Commit("x".to_owned()),
        ]),
        (&deletion, vec![
            cleared(),
            Ime::DeleteSurrounding { before_bytes: 1, after_bytes: 0 },
            cleared(),
        ]),
    ];
    for (batch, expected) in cases {
        let (mut window, mut syncs, members) = entered(2);
        let (a, b) = (&members[0], &members[1]);
        assert_eq!(send(&mut window, &mut syncs, a, preedit("p")), [shown("p")]);

        assert_eq!(send(&mut window, &mut syncs, b, batch), expected);
        assert_eq!(a.requests(), ["disable", "commit", "enable", "cursor_rectangle", "commit"]);
        assert!(b.requests().is_empty(), "the seat that took over is not reset");
        let fence = a.fence().expect("the displaced text input is fenced");
        assert_eq!(syncs, [vec![(a.clone(), fence)]], "one sync, listing the displaced alone");

        assert!(!callback(&mut window, &mut syncs), "the reset owes no opening");
        assert_eq!(a.fence(), None);
    }
}

/// The displaced composer's input method had already computed a commit of the composition when
/// it was reset: that batch's `done` carries a serial before the reset's fence, and is dropped,
/// so the ended composition is never committed after its end.
#[test]
fn the_displaced_composers_late_batch_is_dropped() {
    let (mut window, mut syncs, members) = entered(2);
    let (a, b) = (&members[0], &members[1]);
    let before_reset = a.with_data(|data| data.commits);
    assert_eq!(send(&mut window, &mut syncs, a, preedit("p")), [shown("p")]);
    assert_eq!(send(&mut window, &mut syncs, b, preedit("x")), [cleared(), shown("x")]);

    a.with_data(commit("p"));
    assert!(window.done(a, before_reset, |fences| syncs.push(fences)).is_empty());
    assert_eq!(send(&mut window, &mut syncs, b, preedit("xy")), [shown("xy")], "B still owns it");
}

/// Another seat's batch with no input of its own — an empty `done`, a clear of a preedit the
/// window does not show — is swallowed: nothing is pushed, and the owner's composition goes on.
#[test]
fn a_non_owners_batch_without_input_is_swallowed() {
    let (mut window, mut syncs, members) = entered(2);
    let (a, b) = (&members[0], &members[1]);
    assert_eq!(send(&mut window, &mut syncs, a, preedit("p")), [shown("p")]);

    assert!(send(&mut window, &mut syncs, b, |_| ()).is_empty(), "an empty `done`");
    assert!(
        send(&mut window, &mut syncs, b, |data| data.preedit_string(None, 0, 0)).is_empty(),
        "a clear"
    );
    assert!(syncs.is_empty() && a.requests().is_empty(), "no takeover");
    assert_eq!(send(&mut window, &mut syncs, a, preedit("pq")), [shown("pq")]);
}

/// The displaced seat composes again: a takeover in turn while the seat that displaced it shows a
/// preedit, and a composition from nobody's once that seat committed.
#[test]
fn the_displaced_seat_composing_again() {
    let (mut window, mut syncs, members) = entered(2);
    let (a, b) = (&members[0], &members[1]);
    send(&mut window, &mut syncs, a, preedit("p"));
    send(&mut window, &mut syncs, b, preedit("x"));
    a.requests();
    assert_eq!(send(&mut window, &mut syncs, a, preedit("pp")), [cleared(), shown("pp")]);
    assert_eq!(b.requests(), ["disable", "commit", "enable", "cursor_rectangle", "commit"]);

    let (mut window, mut syncs, members) = entered(2);
    let (a, b) = (&members[0], &members[1]);
    send(&mut window, &mut syncs, a, preedit("p"));
    assert_eq!(send(&mut window, &mut syncs, b, commit("x")), [
        cleared(),
        Ime::Commit("x".to_owned()),
    ]);
    syncs.clear();
    assert_eq!(send(&mut window, &mut syncs, a, preedit("pq")), [shown("pq")], "nobody owned it");
    assert!(syncs.is_empty());
}

/// A request that disables the window's input method ends every member's composition: after the
/// reset, another seat composes with no takeover.
#[test]
fn a_disable_ends_the_ownership() {
    let (mut window, mut syncs, members) = entered(2);
    let (a, b) = (&members[0], &members[1]);
    send(&mut window, &mut syncs, a, preedit("p"));
    reset(&mut window, &mut syncs);
    assert!(callback(&mut window, &mut syncs));
    syncs.clear();
    a.requests();
    assert_eq!(send(&mut window, &mut syncs, b, preedit("x")), [shown("x")]);
    assert!(a.requests().is_empty() && syncs.is_empty(), "no takeover");
}
