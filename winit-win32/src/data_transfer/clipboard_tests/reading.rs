//! Reading the data of sources that list, store or fail in the ways real sources do.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use windows_sys::Win32::Foundation::{
    CLIPBRD_E_CANT_OPEN, E_FAIL, E_NOTIMPL, OLE_S_USEREG, RPC_E_CALL_REJECTED, RPC_E_DISCONNECTED,
};
use windows_sys::Win32::Globalization::GetACP;
use windows_sys::Win32::System::Ole::{CF_HDROP, CF_LOCALE, CF_TEXT, CF_UNICODETEXT};
use winit_core::data_transfer::{TransferType, TypeHint, TypedData};
use winit_core::error::TransferError;
use winit_core::window::WindowId;

use super::test_source::{Listing, Medium, TestSource, utf16_text};
use super::{DRAG, PNG, SameThread, state_is_free};
use crate::clipboard_format::ClipboardFormat;
use crate::data_transfer::{
    CLIPBOARD_ATTEMPTS, DataTransferState, MAX_LISTINGS, ensure_ole_initialized, enumerate_listings,
};

fn png_id() -> u16 {
    ClipboardFormat::register("PNG").unwrap().id()
}

/// A transfer state with drag [`DRAG`] of `source`.
fn dragging(source: TestSource) -> DataTransferState {
    ensure_ole_initialized().unwrap();
    let state = DataTransferState::default();
    state.register_drag(DRAG, WindowId::from_raw(1), source.into_data_object());
    state
}

fn read_bytes(state: &DataTransferState, type_: &dyn TransferType) -> Vec<u8> {
    state.read(DRAG, type_).unwrap().try_as_bytes().unwrap()
}

#[test]
fn a_source_that_cannot_list_its_formats_is_asked_for_the_formats_winit_converts() {
    for listing in [Listing::Fails(E_NOTIMPL), Listing::NoEnumerator(OLE_S_USEREG)] {
        let mut source = TestSource::new(vec![
            (png_id(), Medium::Global(b"png".to_vec())),
            (CF_UNICODETEXT, Medium::Global(utf16_text("text"))),
            // A format that winit does not convert cannot be asked for.
            (0xc123, Medium::Global(b"other".to_vec())),
        ]);
        source.listing = listing;
        let state = dragging(source);

        let formats = state.available_formats(DRAG).unwrap();
        let ids: Vec<_> = formats.iter().map(|available| available.format.id()).collect();
        assert_eq!(ids, [CF_UNICODETEXT, png_id()], "{listing:?}");
        let text = state.read(DRAG, &TypeHint::Plaintext).unwrap();
        assert_eq!(text.try_as_string().unwrap(), "text");
        assert_eq!(read_bytes(&state, &PNG), b"png");
    }
}

#[test]
fn a_stream_is_read_up_to_its_seek_pointer() {
    // `GetData` leaves the seek pointer at the end of the data.
    let stream = Medium::Stream { bytes: b"data and more".to_vec(), position: 4 };
    let state = dragging(TestSource::new(vec![(png_id(), stream)]));
    assert_eq!(read_bytes(&state, &PNG), b"data");
}

#[test]
fn a_stream_with_its_seek_pointer_at_its_start_is_read_to_its_end() {
    let stream = Medium::Stream { bytes: b"data".to_vec(), position: 0 };
    let state = dragging(TestSource::new(vec![(png_id(), stream)]));
    assert_eq!(read_bytes(&state, &PNG), b"data");
}

#[test]
fn a_stream_medium_without_a_stream_is_not_read() {
    let state = dragging(TestSource::new(vec![(png_id(), Medium::NullStream)]));
    let result = state.read(DRAG, &PNG);
    assert!(matches!(result, Err(TransferError::Failed)), "{result:?}");
}

#[test]
fn empty_global_memory_is_read_as_no_data() {
    let state = dragging(TestSource::new(vec![
        (CF_UNICODETEXT, Medium::Global(Vec::new())),
        (png_id(), Medium::Global(Vec::new())),
    ]));
    let text = state.read(DRAG, &TypeHint::Plaintext).unwrap();
    assert_eq!(text.try_as_string().unwrap(), "");
    assert_eq!(read_bytes(&state, &PNG), b"");
}

#[test]
fn empty_global_memory_gives_way_to_a_stream_of_the_format() {
    let stream = Medium::Stream { bytes: b"data".to_vec(), position: 0 };
    let state =
        dragging(TestSource::new(vec![(png_id(), Medium::Global(Vec::new())), (png_id(), stream)]));
    assert_eq!(read_bytes(&state, &PNG), b"data");
    // Without a stream, the empty global memory is the data.
    let state = dragging(TestSource::new(vec![
        (png_id(), Medium::Global(Vec::new())),
        (png_id(), Medium::NullStream),
    ]));
    assert_eq!(read_bytes(&state, &PNG), b"");
}

#[test]
fn at_most_4096_listings_are_read() {
    ensure_ole_initialized().unwrap();
    let listings = vec![(CF_HDROP, Medium::Global(Vec::new())); MAX_LISTINGS + 100];
    let data_object = TestSource::new(listings).into_data_object();
    assert_eq!(enumerate_listings(&data_object).unwrap().len(), MAX_LISTINGS);
}

fn listed_ids(state: &DataTransferState) -> Result<Vec<u16>, TransferError> {
    let formats = state.available_formats(DRAG)?;
    Ok(formats.iter().map(|available| available.format.id()).collect())
}

#[test]
fn a_listing_that_fails_in_the_call_is_returned_and_not_kept() {
    for hr in [RPC_E_CALL_REJECTED, RPC_E_DISCONNECTED, E_FAIL] {
        let mut source =
            TestSource::new(vec![(CF_UNICODETEXT, Medium::Global(utf16_text("text")))]);
        source.listing = Listing::FailsFirst(hr);
        let calls = Arc::clone(&source.calls);
        let state = dragging(source);

        let result = listed_ids(&state);
        assert!(matches!(result, Err(TransferError::Os(_))), "0x{hr:08x}: {result:?}");
        // The next use lists the formats again.
        assert_eq!(listed_ids(&state).unwrap(), [CF_UNICODETEXT], "0x{hr:08x}");
        assert_eq!(calls.enum_format_etc.load(Ordering::SeqCst), 2, "0x{hr:08x}");
    }
}

#[test]
fn a_query_that_fails_in_the_call_is_returned_and_not_kept() {
    let mut source = TestSource::new(vec![(CF_UNICODETEXT, Medium::Global(utf16_text("text")))]);
    source.listing = Listing::Fails(E_NOTIMPL);
    source.failures = vec![(CF_UNICODETEXT, RPC_E_DISCONNECTED)];
    let state = dragging(source);
    for _ in 0..2 {
        let result = listed_ids(&state);
        assert!(matches!(result, Err(TransferError::Os(_))), "{result:?}");
    }
}

#[test]
fn no_call_reaches_the_data_object_of_a_drag_while_the_state_is_borrowed() {
    ensure_ole_initialized().unwrap();
    // A source that is asked for each format with `QueryGetData`, and one that lists them, with
    // calls that each makes when it is listed, read and released.
    let sources: [(Listing, &[&str]); 2] = [
        (Listing::Fails(E_NOTIMPL), &["EnumFormatEtc", "QueryGetData", "GetData", "Release"]),
        (Listing::Formats, &["EnumFormatEtc", "GetData", "Release"]),
    ];
    for (listing, expected) in sources {
        let state = DataTransferState::default();
        // Each call into the data object, and whether the state was free during it.
        let made = Rc::new(RefCell::new(Vec::new()));
        let mut source =
            TestSource::new(vec![(CF_UNICODETEXT, Medium::Global(utf16_text("text")))]);
        source.listing = listing;
        source.hook = Some(Box::new({
            let state = SameThread(&state);
            let made = Rc::clone(&made);
            move |call| made.borrow_mut().push((call, state_is_free(unsafe { state.get() })))
        }));
        let calls = Arc::clone(&source.calls);
        state.register_drag(DRAG, WindowId::from_raw(1), source.into_data_object());

        assert_eq!(listed_ids(&state).unwrap(), [CF_UNICODETEXT], "{listing:?}");
        let text = state.read(DRAG, &TypeHint::Plaintext).unwrap();
        assert_eq!(text.try_as_string().unwrap(), "text", "{listing:?}");
        state.remove_drag(DRAG);
        assert!(calls.destroyed.load(Ordering::SeqCst), "{listing:?}");

        let made = made.borrow();
        let borrowed: Vec<_> =
            made.iter().filter(|(_, free)| !free).map(|(call, _)| *call).collect();
        assert_eq!(borrowed, [""; 0], "{listing:?}");
        for call in expected {
            let made_it = made.iter().any(|(made, _)| made == call);
            assert!(made_it, "{listing:?}: no {call} in {made:?}");
        }
    }
}

#[test]
fn code_page_text_is_read_in_the_code_page_of_the_source_locale() {
    // Text in the ANSI code page of a locale other than this system's, which would read
    // differently in this system's code page: Russian (1251), or else Greek (1253).
    let (locale, ansi, text) = if unsafe { GetACP() } != 1251 {
        let privet = "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}";
        (0x0419_u32, &b"\xcf\xf0\xe8\xe2\xe5\xf2\0"[..], privet)
    } else {
        (0x0408, &b"\xc3\xe5\xe9\xe1\0"[..], "\u{393}\u{3b5}\u{3b9}\u{3b1}")
    };
    // Also from a source that cannot list its formats, which is asked for its `CF_LOCALE`.
    let listings =
        [Listing::Formats, Listing::Fails(E_NOTIMPL), Listing::NoEnumerator(OLE_S_USEREG)];
    for listing in listings {
        let mut source = TestSource::new(vec![
            (CF_TEXT, Medium::Global(ansi.to_vec())),
            (CF_LOCALE, Medium::Global(locale.to_le_bytes().to_vec())),
        ]);
        source.listing = listing;
        let calls = Arc::clone(&source.calls);
        let state = dragging(source);

        assert_eq!(listed_ids(&state).unwrap(), [CF_TEXT, CF_LOCALE], "{listing:?}");
        let read = state.read(DRAG, &TypeHint::Plaintext).unwrap();
        assert_eq!(read.try_as_string().unwrap(), text, "{listing:?}");
        // The read fetched the locale, then the text.
        assert_eq!(*calls.get_data.lock().unwrap(), [CF_LOCALE, CF_TEXT], "{listing:?}");
    }
}

#[test]
fn a_clipboard_held_open_elsewhere_ends_the_search_for_a_format_to_read() {
    let mut source = TestSource::new(vec![
        (CF_UNICODETEXT, Medium::Global(utf16_text("unicode"))),
        (CF_TEXT, Medium::Global(b"ansi\0".to_vec())),
    ]);
    source.failures = vec![(CF_UNICODETEXT, CLIPBRD_E_CANT_OPEN)];
    let calls = Arc::clone(&source.calls);
    let state = dragging(source);

    let result = state.read(DRAG, &TypeHint::Plaintext);
    assert!(matches!(result, Err(TransferError::Os(_))), "{result:?}");
    // `CF_UNICODETEXT` was asked for until the attempts ran out, and `CF_TEXT` not at all.
    assert_eq!(*calls.get_data.lock().unwrap(), [CF_UNICODETEXT; CLIPBOARD_ATTEMPTS]);
}
