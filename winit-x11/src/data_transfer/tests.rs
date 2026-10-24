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

fn request(target: xproto::Atom, property: xproto::Atom) -> XSelectionRequestEvent {
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
    let request = request(UTF8_STRING_ATOM, 77);
    let reply = selection_notify_reply(&request, None);

    assert_eq!(reply.property, x11rb::NONE);
    assert_eq!(reply.requestor, 12);
    assert_eq!(reply.selection, 13);
    assert_eq!(reply.target, UTF8_STRING_ATOM);
    assert_eq!(reply.time, 4242);
}

#[test]
fn an_answered_request_names_the_property_of_the_reply() {
    let request = request(UTF8_STRING_ATOM, 77);
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
    let mut state = ClipboardState::default();
    let utf8 = SelectionType { hint: Some(TypeHint::Plaintext), atom: UTF8_STRING_ATOM };
    let (first, second) = (AsyncRequestSerial::get(), AsyncRequestSerial::get());
    let (window_a, window_b) = (WindowId::from_raw(1), WindowId::from_raw(2));

    assert!(state.add_to_fetch(PendingFetch { serial: first, ty: utf8.clone(), window: window_a }));
    assert!(!state.add_to_fetch(PendingFetch { serial: second, ty: utf8, window: window_b }));

    let finished = state.finish_fetch(b"a".to_vec()).expect("the first fetch is pending");
    assert_eq!((finished.serial, finished.window()), (first, window_a));
    let finished = state.finish_fetch(b"b".to_vec()).expect("the second fetch is pending");
    assert_eq!((finished.serial, finished.window()), (second, window_b));
    assert!(state.finish_fetch(Vec::new()).is_none());
}
