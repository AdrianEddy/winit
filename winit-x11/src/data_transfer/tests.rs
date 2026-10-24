//! Unit tests for the selection and drag-and-drop logic that runs without an X server.

use std::ptr;

use winit_core::data_transfer::{DataTransferSend, DataTransferSendBuilder, TypeHint};
use x11_dl::xlib::XSelectionRequestEvent;
use x11rb::protocol::xproto;

use super::*;

const TARGETS_ATOM: xproto::Atom = 101;
const TIMESTAMP_ATOM: xproto::Atom = 102;
const UTF8_STRING_ATOM: xproto::Atom = 103;
const STRING_ATOM: xproto::Atom = 104;
const TEXT_PLAIN_ATOM: xproto::Atom = 105;
const TEXT_PLAIN_UTF8_ATOM: xproto::Atom = 106;
const IMAGE_PNG_ATOM: xproto::Atom = 107;
const MULTIPLE_ATOM: xproto::Atom = 108;
const INCR_ATOM: xproto::Atom = 109;

/// The atoms the tests use, each with its own value; every other atom is 0, which no test uses.
fn atoms() -> Atoms {
    Atoms {
        TARGETS: TARGETS_ATOM,
        TIMESTAMP: TIMESTAMP_ATOM,
        MULTIPLE: MULTIPLE_ATOM,
        UTF8_STRING: UTF8_STRING_ATOM,
        STRING: STRING_ATOM,
        TextPlain: TEXT_PLAIN_ATOM,
        TextPlainCharsetUtf8: TEXT_PLAIN_UTF8_ATOM,
        ImagePng: IMAGE_PNG_ATOM,
        ..Default::default()
    }
}

/// A clipboard owned by this application, offering `text` as plain text.
fn owned_text(text: &'static str) -> ClipboardState {
    let send: Box<dyn DataTransferSend> = DataTransferSendBuilder::new(())
        .with_type(TypeHint::Plaintext, move |_, _| Some(text.to_owned()))
        .build();
    ClipboardState { owned_data: Some(send), ..Default::default() }
}

fn selection_request(target: xproto::Atom, property: xproto::Atom) -> XSelectionRequestEvent {
    XSelectionRequestEvent {
        type_: 30,
        serial: 7,
        send_event: 0,
        display: ptr::null_mut(),
        owner: 11,
        requestor: 12,
        selection: 13,
        target: target as _,
        property: property as _,
        time: 4242,
    }
}

#[test]
fn a_refused_request_is_answered_with_property_none() {
    let request = selection_request(UTF8_STRING_ATOM, 77);
    let reply = selection_notify_reply(&request, None);

    assert_eq!(reply.property, x11rb::NONE);
    assert_eq!(reply.requestor, 12);
    assert_eq!(reply.selection, 13);
    assert_eq!(reply.target, UTF8_STRING_ATOM);
    assert_eq!(reply.time, 4242);
}

#[test]
fn an_answered_request_names_the_property_of_the_reply() {
    let request = selection_request(UTF8_STRING_ATOM, 77);
    let reply = selection_notify_reply(&request, Some(77));

    assert_eq!(reply.property, 77);
    assert_eq!(reply.time, 4242);
}

#[test]
fn an_obsolete_client_is_answered_on_the_target_property() {
    assert_eq!(reply_property(x11rb::NONE, UTF8_STRING_ATOM), UTF8_STRING_ATOM);
    assert_eq!(reply_property(77, UTF8_STRING_ATOM), 77);
}

#[test]
fn a_selection_without_owned_data_refuses_every_conversion() {
    let state = ClipboardState::default();
    let atoms = atoms();

    assert_eq!(state.plan_conversion(&atoms, TARGETS_ATOM, 1024), ReplyPlan::Refuse);
    assert_eq!(state.plan_conversion(&atoms, UTF8_STRING_ATOM, 1024), ReplyPlan::Refuse);
}

#[test]
fn a_conversion_to_a_type_that_is_not_offered_is_refused() {
    let state = owned_text("hello");

    assert_eq!(state.plan_conversion(&atoms(), IMAGE_PNG_ATOM, 1024), ReplyPlan::Refuse);
}

#[test]
fn targets_lists_the_required_targets_and_the_offered_types() {
    let state = owned_text("hello");

    let ReplyPlan::Targets(targets) = state.plan_conversion(&atoms(), TARGETS_ATOM, 1024) else {
        panic!("TARGETS should be answered with a list of targets");
    };
    assert_eq!(&targets[..3], &[TIMESTAMP_ATOM, TARGETS_ATOM, MULTIPLE_ATOM]);
    for text in [STRING_ATOM, UTF8_STRING_ATOM, TEXT_PLAIN_ATOM, TEXT_PLAIN_UTF8_ATOM] {
        assert!(targets.contains(&text), "plain text target {text} is advertised");
    }
    assert!(!targets.contains(&IMAGE_PNG_ATOM));
}

#[test]
fn timestamp_is_answered() {
    let state = owned_text("hello");

    assert_eq!(state.plan_conversion(&atoms(), TIMESTAMP_ATOM, 1024), ReplyPlan::Timestamp);
}

#[test]
fn small_data_is_stored_directly_and_large_data_incrementally() {
    let state = owned_text("hello");
    let utf8 = SelectionType { hint: Some(TypeHint::Plaintext), atom: UTF8_STRING_ATOM };

    assert_eq!(
        state.plan_conversion(&atoms(), UTF8_STRING_ATOM, 6),
        ReplyPlan::Direct(utf8.clone(), b"hello".to_vec())
    );
    assert_eq!(
        state.plan_conversion(&atoms(), UTF8_STRING_ATOM, 5),
        ReplyPlan::Incr(utf8, b"hello".to_vec())
    );
}

#[test]
fn multiple_converts_its_pairs_in_order() {
    let mut converted = Vec::new();
    let failed = answer_multiple(&[UTF8_STRING_ATOM, 1, STRING_ATOM, 2], MULTIPLE_ATOM, |t, p| {
        converted.push((t, p));
        true
    });

    assert_eq!(converted, [(UTF8_STRING_ATOM, 1), (STRING_ATOM, 2)]);
    assert_eq!(failed, None, "nothing is rewritten when every conversion succeeds");
}

#[test]
fn multiple_reports_a_failed_conversion_with_property_none() {
    let failed = answer_multiple(
        &[UTF8_STRING_ATOM, 1, IMAGE_PNG_ATOM, 2, STRING_ATOM, 3],
        MULTIPLE_ATOM,
        |target, _| target != IMAGE_PNG_ATOM,
    );

    assert_eq!(
        failed,
        Some(vec![UTF8_STRING_ATOM, 1, IMAGE_PNG_ATOM, x11rb::NONE, STRING_ATOM, 3])
    );
}

#[test]
fn multiple_fails_pairs_without_property_and_nested_multiple() {
    let mut converted = Vec::new();
    let failed = answer_multiple(
        &[UTF8_STRING_ATOM, x11rb::NONE, MULTIPLE_ATOM, 2, STRING_ATOM, 3],
        MULTIPLE_ATOM,
        |t, p| {
            converted.push((t, p));
            true
        },
    );

    assert_eq!(converted, [(STRING_ATOM, 3)]);
    assert_eq!(
        failed,
        Some(vec![UTF8_STRING_ATOM, x11rb::NONE, MULTIPLE_ATOM, x11rb::NONE, STRING_ATOM, 3])
    );
}

#[test]
fn multiple_ignores_a_trailing_unpaired_atom() {
    let mut converted = Vec::new();
    let failed = answer_multiple(&[UTF8_STRING_ATOM, 1, STRING_ATOM], MULTIPLE_ATOM, |t, p| {
        converted.push((t, p));
        false
    });

    assert_eq!(converted, [(UTF8_STRING_ATOM, 1)]);
    assert_eq!(failed, Some(vec![UTF8_STRING_ATOM, x11rb::NONE]));
}

#[test]
fn an_incremental_transfer_announces_a_lower_bound_of_its_size() {
    assert_eq!(incr_announcement(300_000), [300_000]);
    assert_eq!(incr_announcement(usize::MAX), [u32::MAX]);
}

#[test]
fn incremental_transfers_start_at_the_maximum_request_size() {
    // The smallest maximum request size the core protocol allows.
    assert_eq!(incr_threshold(4096), 3996);
    // A server without BIG-REQUESTS.
    assert_eq!(incr_threshold(262_140), 262_040);
    // A server with BIG-REQUESTS: one request still stores at most 256 KiB.
    assert_eq!(incr_threshold(16 * 1024 * 1024), 256 * 1024);
}

#[test]
fn a_clipboard_fetch_is_delivered_to_the_window_it_was_requested_for() {
    let id = DataTransferId::from_raw(1);
    let mut queue = FetchQueue::default();
    let first = pending(id, 1, Some(request(1, 10)));
    let second = pending(id, 2, None);
    let serials = (first.serial, second.serial);
    queue.push(first);
    queue.push(second);

    let finished = queue.complete_front(Ok(b"a".to_vec())).expect("the first fetch is pending");
    assert_eq!((finished.serial, finished.window()), (serials.0, WindowId::from_raw(1)));
    let finished = queue.complete_front(Ok(b"b".to_vec())).expect("the second fetch is pending");
    assert_eq!((finished.serial, finished.window()), (serials.1, WindowId::from_raw(2)));
    assert!(queue.complete_front(Ok(Vec::new())).is_none());
}

fn utf8() -> SelectionType {
    SelectionType { hint: Some(TypeHint::Plaintext), atom: UTF8_STRING_ATOM }
}

fn request(property: xproto::Atom, sequence: u64) -> Request {
    Request { property, sequence }
}

/// A fetch of UTF-8 text from transfer `id`, for window `window`.
fn pending(id: DataTransferId, window: u64, request: Option<Request>) -> PendingFetch {
    PendingFetch {
        id,
        serial: AsyncRequestSerial::get(),
        ty: utf8(),
        window: WindowId::from_raw(window as _),
        request,
    }
}

fn error_kind(finished: &FinishedDataTransfer) -> Option<io::ErrorKind> {
    finished.value.try_as_bytes().err().map(|err| err.kind())
}

#[test]
fn a_refused_conversion_fails_its_fetch() {
    let outcome =
        reply_outcome(x11rb::NONE, INCR_ATOM, || unreachable!("a refusal stores nothing"));

    assert!(
        matches!(&outcome, ReplyOutcome::Failed(err) if err.kind() == io::ErrorKind::Unsupported),
        "{outcome:?}"
    );
}

#[test]
fn an_unreadable_reply_fails_its_fetch() {
    let outcome =
        reply_outcome(77, INCR_ATOM, || Err(util::GetPropertyError::TypeMismatch(STRING_ATOM)));

    assert!(
        matches!(&outcome, ReplyOutcome::Failed(err) if err.kind() == io::ErrorKind::InvalidData),
        "{outcome:?}"
    );
}

#[test]
fn an_incremental_reply_keeps_its_fetch_pending() {
    let outcome = reply_outcome(77, INCR_ATOM, || Ok((INCR_ATOM, vec![0; 4])));
    assert!(matches!(outcome, ReplyOutcome::Incremental), "{outcome:?}");

    let outcome = reply_outcome(77, INCR_ATOM, || Ok((UTF8_STRING_ATOM, b"hi".to_vec())));
    assert!(matches!(&outcome, ReplyOutcome::Data(data) if data == b"hi"), "{outcome:?}");
}

#[test]
fn a_failed_fetch_reports_its_error_from_every_reader() {
    let mut queue = FetchQueue::default();
    queue.push(pending(DataTransferId::from_raw(1), 1, Some(request(1, 10))));

    let refused = io::Error::new(io::ErrorKind::Unsupported, "refused");
    let finished = queue.complete_front(Err(refused)).expect("the fetch is pending");

    assert!(finished.value.try_read().is_none());
    assert_eq!(error_kind(&finished), Some(io::ErrorKind::Unsupported));
    assert_eq!(finished.value.try_as_string().unwrap_err().kind(), io::ErrorKind::Unsupported);
}

#[test]
fn a_fetch_whose_conversion_cannot_be_requested_completes_and_the_next_one_starts() {
    let id = DataTransferId::from_raw(1);
    let mut queue = FetchQueue::default();
    let fetches = [pending(id, 1, None), pending(id, 1, None), pending(id, 1, None)];
    let serials: Vec<_> = fetches.iter().map(|fetch| fetch.serial).collect();
    fetches.into_iter().for_each(|fetch| queue.push(fetch));

    let mut attempts = 0;
    let failed = queue.start_front(|_| {
        attempts += 1;
        if attempts < 3 { Err("no owner") } else { Ok(request(5, 50)) }
    });

    let failed_serials: Vec<_> = failed.iter().map(|finished| finished.serial).collect();
    assert_eq!(failed_serials, serials[..2]);
    assert!(failed.iter().all(|finished| error_kind(finished) == Some(io::ErrorKind::Other)));
    assert_eq!(queue.front_request().map(|(_, request)| request), Some(request(5, 50)));
}

#[test]
fn an_owner_change_abandons_every_fetch_in_order() {
    let id = DataTransferId::from_raw(1);
    let mut queue = FetchQueue::default();
    let fetches = [pending(id, 1, Some(request(1, 10))), pending(id, 2, None)];
    let serials: Vec<_> = fetches.iter().map(|fetch| fetch.serial).collect();
    fetches.into_iter().for_each(|fetch| queue.push(fetch));

    let abandoned = queue.abandon_while(|_| true, "the owner changed");

    assert_eq!(abandoned.iter().map(|finished| finished.serial).collect::<Vec<_>>(), serials);
    assert!(abandoned.iter().all(|f| error_kind(f) == Some(io::ErrorKind::ConnectionAborted)));
    assert!(queue.is_empty());
}

#[test]
fn new_contents_abandon_only_the_fetches_of_the_previous_contents() {
    let (previous, current) = (DataTransferId::from_raw(1), DataTransferId::from_raw(2));
    let mut queue = FetchQueue::default();
    queue.push(pending(previous, 1, Some(request(1, 10))));
    queue.push(pending(previous, 1, None));
    queue.push(pending(current, 1, None));

    let abandoned = queue.abandon_while(|fetch| fetch.id != current, "the contents changed");

    assert_eq!(abandoned.len(), 2);
    assert!(abandoned.iter().all(|finished| finished.id == previous));
    assert_eq!(queue.fetches.front().map(|fetch| fetch.id), Some(current));
}

/// A selection whose first fetch converts UTF-8 text into property 2, requested with sequence
/// number 20, after the fetch of an earlier owner (converted into property 1) was abandoned.
fn selection_after_an_owner_change() -> ClipboardState {
    let mut state = ClipboardState::default();
    state.fetches.push(pending(DataTransferId::from_raw(2), 1, Some(request(2, 20))));
    state
}

#[test]
fn a_late_success_after_an_owner_change_is_stale() {
    let state = selection_after_an_owner_change();
    let atoms = atoms();

    assert_eq!(state.match_reply(&atoms, 1, UTF8_STRING_ATOM, 30), ClipboardReply::Stale);
    assert_eq!(state.match_reply(&atoms, 2, UTF8_STRING_ATOM, 30), ClipboardReply::Fetch);
}

#[test]
fn a_late_refusal_after_an_owner_change_is_stale() {
    let state = selection_after_an_owner_change();
    let atoms = atoms();
    let none = x11rb::NONE;

    // Generated before the server processed the outstanding request: it answers an older one.
    assert_eq!(state.match_reply(&atoms, none, UTF8_STRING_ATOM, 19), ClipboardReply::Stale);
    // A refusal of another target answers another request.
    assert_eq!(state.match_reply(&atoms, none, STRING_ATOM, 25), ClipboardReply::Stale);
    assert_eq!(state.match_reply(&atoms, none, UTF8_STRING_ATOM, 20), ClipboardReply::Fetch);
    assert_eq!(state.match_reply(&atoms, none, UTF8_STRING_ATOM, 25), ClipboardReply::Fetch);
}

#[test]
fn a_late_incremental_chunk_after_an_owner_change_is_ignored() {
    let window = 9;
    let mut state = ClipboardState::default();
    state.fetches.push(pending(DataTransferId::from_raw(1), 1, Some(request(1, 10))));
    let mut receives = vec![IncrReceive {
        window,
        property: 1,
        data: b"partial".to_vec(),
        target: IncrTarget::Fetch(ClipboardSelectionType::Clipboard),
    }];

    let _ = state.fetches.abandon_while(|_| true, "the owner changed");
    state.fetches.push(pending(DataTransferId::from_raw(2), 1, Some(request(2, 20))));
    let busy: Vec<_> = state.outstanding_requests().map(|request| request.property).collect();
    retain_outstanding_receives(&mut receives, &busy);

    assert!(
        !receives.iter().any(|receive| receive.window == window && receive.property == 1),
        "a chunk stored in the abandoned property finds no reception"
    );
}

#[test]
fn targets_replies_answer_the_targets_request() {
    let state = ClipboardState { targets_request: Some(request(3, 10)), ..Default::default() };
    let atoms = atoms();
    let none = x11rb::NONE;

    assert_eq!(state.match_reply(&atoms, 3, TARGETS_ATOM, 11), ClipboardReply::Targets);
    assert_eq!(state.match_reply(&atoms, none, TARGETS_ATOM, 11), ClipboardReply::Targets);
    assert_eq!(state.match_reply(&atoms, none, TARGETS_ATOM, 9), ClipboardReply::Stale);
    assert_eq!(state.match_reply(&atoms, 4, TARGETS_ATOM, 11), ClipboardReply::Stale);
}

#[test]
fn a_refusal_generated_after_the_request_is_recognised_across_sequence_wrap() {
    let request = request(1, 0xffff_fff0);
    let utf8 = |target| target == UTF8_STRING_ATOM;

    assert!(request.answered_by(utf8, x11rb::NONE, UTF8_STRING_ATOM, 0x1_0000_0005));
    assert!(!request.answered_by(utf8, x11rb::NONE, UTF8_STRING_ATOM, 0xffff_ffef));
}

#[test]
fn conversion_properties_are_used_in_turn_and_never_shared() {
    let mut pool = PropertyPool { atoms: vec![1, 2, 3], next: 0 };

    let mut busy = vec![2];
    assert_eq!(pool.take(&mut busy), Some(1));
    assert_eq!(pool.take(&mut busy), Some(3));
    assert_eq!(pool.take(&mut busy), None, "every property is in use");

    assert_eq!(pool.take(&mut Vec::new()), Some(1));
    assert_eq!(pool.take(&mut Vec::new()), Some(2));
}

#[test]
fn owner_changes_are_classified() {
    let set_owner = 0;
    let window_destroyed = 1;
    let client_closed = 2;
    let ours = 42;

    assert_eq!(OwnerChange::classify(set_owner, 42, ours), OwnerChange::Ours);
    assert_eq!(OwnerChange::classify(set_owner, 7, ours), OwnerChange::Theirs);
    assert_eq!(OwnerChange::classify(set_owner, x11rb::NONE, ours), OwnerChange::Gone);
    assert_eq!(OwnerChange::classify(window_destroyed, 7, ours), OwnerChange::Gone);
    assert_eq!(OwnerChange::classify(client_closed, 0, ours), OwnerChange::Gone);
}

#[test]
fn a_reply_that_reports_another_target_answers_by_its_property() {
    let state = ClipboardState {
        targets_request: Some(request(3, 10)),
        ..selection_after_an_owner_change()
    };
    let atoms = atoms();

    assert_eq!(state.match_reply(&atoms, 2, STRING_ATOM, 30), ClipboardReply::Fetch);
    assert_eq!(state.match_reply(&atoms, 3, IMAGE_PNG_ATOM, 30), ClipboardReply::Targets);
    // A refusal names no property: only its target can tell which request it refuses.
    assert_eq!(state.match_reply(&atoms, x11rb::NONE, STRING_ATOM, 30), ClipboardReply::Stale);
}

fn property_reply(ty: xproto::Atom, value: Vec<u8>, bytes_after: u32) -> PropertyReply {
    PropertyReply { ty, format: 8, bytes_after, value }
}

#[test]
fn a_reply_is_read_with_one_request() {
    let value = vec![7; 4 * 1024 * 1024];
    let mut requests = 0;

    // Answers like the server: at most `length` units of 32 bits from `offset`.
    let read = read_property(|offset, length| {
        requests += 1;
        let start = (offset as usize * 4).min(value.len());
        let end = start.saturating_add(length as usize * 4).min(value.len());
        let bytes_after = (value.len() - end) as u32;
        Ok(property_reply(UTF8_STRING_ATOM, value[start..end].to_vec(), bytes_after))
    });

    assert_eq!(read.map(|(ty, data)| (ty, data.len())).ok(), Some((UTF8_STRING_ATOM, value.len())));
    assert_eq!(requests, 1);
}

#[test]
fn a_missing_reply_property_is_an_error_but_an_empty_one_is_not() {
    let missing =
        read_property(|_, _| Ok(PropertyReply { format: 0, ..property_reply(0, vec![], 0) }));
    assert!(matches!(missing, Err(util::GetPropertyError::Absent)), "{missing:?}");

    let empty = read_property(|_, _| Ok(property_reply(UTF8_STRING_ATOM, Vec::new(), 0)));
    assert_eq!(empty.ok(), Some((UTF8_STRING_ATOM, Vec::new())));
}

#[test]
fn the_rest_of_a_reply_is_read_from_where_the_first_request_stopped() {
    let mut replies = vec![
        property_reply(UTF8_STRING_ATOM, b"abcd".to_vec(), 2),
        property_reply(UTF8_STRING_ATOM, b"ef".to_vec(), 0),
    ]
    .into_iter();
    let mut offsets = Vec::new();

    let read = read_property(|offset, _| {
        offsets.push(offset);
        Ok(replies.next().expect("no more requests than replies"))
    });

    assert_eq!(read.ok(), Some((UTF8_STRING_ATOM, b"abcdef".to_vec())));
    assert_eq!(offsets, [0, 1]);
}

#[test]
fn a_reply_whose_type_changes_while_it_is_read_is_an_error() {
    let mut replies = vec![
        property_reply(UTF8_STRING_ATOM, b"abcd".to_vec(), 2),
        property_reply(STRING_ATOM, b"ef".to_vec(), 0),
    ]
    .into_iter();

    let read = read_property(|_, _| Ok(replies.next().expect("no more requests than replies")));

    assert!(matches!(read, Err(util::GetPropertyError::TypeMismatch(STRING_ATOM))), "{read:?}");
}

#[test]
fn a_fetch_keeps_the_window_and_the_transfer_it_was_requested_for() {
    let mut state =
        ClipboardState { transfer_serial: DataTransferId::from_raw(1), ..owned_text("a") };
    let (window_a, window_b) = (WindowId::from_raw(1), WindowId::from_raw(2));

    let first = state.enqueue_fetch(utf8(), window_a, Some(request(1, 10)));
    // The contents change before the first fetch completes.
    state.transfer_serial = DataTransferId::from_raw(2);
    let second = state.enqueue_fetch(utf8(), window_b, None);

    let finished = state.fetches.complete_front(Ok(b"a".to_vec())).expect("a fetch is pending");
    assert_eq!(
        (finished.serial, finished.id, finished.window()),
        (first, DataTransferId::from_raw(1), window_a)
    );
    let finished = state.fetches.complete_front(Ok(b"b".to_vec())).expect("a fetch is pending");
    assert_eq!(
        (finished.serial, finished.id, finished.window()),
        (second, DataTransferId::from_raw(2), window_b)
    );
}

#[test]
fn owner_changes_are_ignored_while_this_application_owns_the_selection() {
    let mut state = owned_text("ours");

    assert_eq!(state.owner_change_action(OwnerChange::Theirs, 5), OwnerChangeAction::Ignore);
    assert_eq!(state.owner_change_action(OwnerChange::Gone, 6), OwnerChangeAction::Ignore);
    assert_eq!(state.owner_change_action(OwnerChange::Ours, 7), OwnerChangeAction::AbandonPrevious);
}

#[test]
fn a_repeated_owner_change_is_skipped() {
    let mut state = ClipboardState::default();
    let changed_hands = OwnerChangeAction::AbandonAll { request_targets: true };

    assert_eq!(state.owner_change_action(OwnerChange::Theirs, 5), changed_hands);
    assert_eq!(state.owner_change_action(OwnerChange::Theirs, 5), OwnerChangeAction::Ignore);
    assert_eq!(state.owner_change_action(OwnerChange::Theirs, 6), changed_hands);
    assert_eq!(state.owner_change_action(OwnerChange::Gone, 6), OwnerChangeAction::AbandonAll {
        request_targets: false
    });
}

#[test]
fn new_contents_of_ours_abandon_only_the_fetches_of_the_contents_they_replace() {
    let mut state =
        ClipboardState { transfer_serial: DataTransferId::from_raw(1), ..owned_text("a") };
    state.enqueue_fetch(utf8(), WindowId::from_raw(1), Some(request(1, 10)));
    // `set_clipboard` mints the transfer id of the new contents.
    state.transfer_serial = DataTransferId::from_raw(2);
    let kept = state.enqueue_fetch(utf8(), WindowId::from_raw(1), None);

    assert_eq!(state.owner_change_action(OwnerChange::Ours, 9), OwnerChangeAction::AbandonPrevious);
    let abandoned = state.abandon_previous_fetches();

    assert_eq!(abandoned.len(), 1);
    assert_eq!(abandoned[0].id, DataTransferId::from_raw(1));
    assert_eq!(state.fetches.fetches.front().map(|fetch| fetch.serial), Some(kept));
}

#[test]
fn abandoning_a_selection_forgets_its_types_and_mints_a_new_transfer_id() {
    let mut state = ClipboardState {
        types: vec![utf8()].into(),
        targets_request: Some(request(3, 10)),
        ..ClipboardState::default()
    };
    let previous = state.transfer_serial;
    state.enqueue_fetch(utf8(), WindowId::from_raw(1), Some(request(1, 10)));

    let abandoned = state.abandon_all("the owner went away");

    assert_eq!(abandoned.len(), 1);
    assert_ne!(state.transfer_serial, previous);
    assert!(state.types.is_empty());
    assert_eq!(state.targets_request, None);
    assert!(state.fetches.is_empty());
}

#[test]
fn every_fetch_ends_in_exactly_one_result() {
    let id = DataTransferId::from_raw(1);
    let mut queue = FetchQueue::default();
    let fetches = [pending(id, 1, None), pending(id, 1, None), pending(id, 1, None)];
    let serials: Vec<_> = fetches.iter().map(|fetch| fetch.serial).collect();
    fetches.into_iter().for_each(|fetch| queue.push(fetch));

    let mut results = Vec::new();
    results.extend(queue.start_front(|_| Ok::<_, &str>(request(1, 10))));
    results.extend(queue.complete_front(Ok(b"data".to_vec())));
    let mut attempts = 0;
    results.extend(queue.start_front(|_| {
        attempts += 1;
        if attempts == 1 { Err("failed") } else { Ok(request(2, 20)) }
    }));
    results.extend(queue.abandon_while(|_| true, "the owner changed"));
    results.extend(queue.complete_front(Ok(Vec::new())));

    assert_eq!(results.iter().map(|finished| finished.serial).collect::<Vec<_>>(), serials);
}
