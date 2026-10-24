use std::sync::Mutex;

use windows_sys::Win32::UI::WindowsAndMessaging::{CreateWindowExW, HWND_MESSAGE};
use winit_core::data_transfer::DataTransferSendBuilder;

use super::*;

mod delay_render;
mod drop_target;
mod reading;
mod remote_source;
mod test_source;

const DRAG: DataTransferId = DataTransferId::from_raw(1);

/// A message-only window, which gets no messages unless they are sent to it.
fn message_window() -> HWND {
    let class = util::encode_wide("STATIC");
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            std::ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            util::get_instance_handle(),
            std::ptr::null(),
        )
    };
    assert!(!window.is_null());
    window
}
const FILE_URI: &str = "file:///C:/dir/a%20b.txt";
const PNG: TypeHint = TypeHint::Image { extension_hint: Some("png") };
const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n";

/// A pointer that a data callback, which must be `Send`, may carry. The callbacks run on the
/// test's thread, which owns the pointee: the tests call the data objects directly.
#[derive(Debug)]
struct SameThread<T>(*const T);

unsafe impl<T> Send for SameThread<T> {}

impl<T> Clone for SameThread<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for SameThread<T> {}

impl<T> SameThread<T> {
    /// # Safety
    ///
    /// The pointee must be alive.
    unsafe fn get(&self) -> &T {
        unsafe { &*self.0 }
    }
}

/// The type of each read of the data, and whether the state was free while it was read.
type Reads = Arc<Mutex<Vec<(TypeHint, bool)>>>;

fn state_is_free(state: &DataTransferState) -> bool {
    state.drag.try_borrow_mut().is_ok() && state.clipboard.try_borrow_mut().is_ok()
}

/// Text, a file list and a PNG image, offered the way an application offers them. Each read is
/// logged in `reads`, then `while_read` runs, as messages can be dispatched while another
/// application renders the data.
fn offer(
    state: &DataTransferState,
    reads: &Reads,
    while_read: fn(&DataTransferState),
) -> SourceDataObject {
    let state = SameThread(state);
    let render = move |data: SendData| {
        let reads = Arc::clone(reads);
        move |_: &(), type_: &dyn TransferType| {
            let state = unsafe { state.get() };
            reads.lock().unwrap().push((type_.hint().unwrap(), state_is_free(state)));
            while_read(state);
            Some(data.clone())
        }
    };
    let send = DataTransferSendBuilder::new(())
        .with_type(TypeHint::Plaintext, render(SendData::String("text".to_owned())))
        .with_type(TypeHint::UriList, render(SendData::Uris(vec![FILE_URI.to_owned()])))
        .with_type(PNG, render(SendData::Bytes(PNG_BYTES.to_vec())))
        .build();
    SourceDataObject::new(send)
}

/// Starts drag `DRAG` of the data of `source`, which only the drag keeps alive.
fn drag(state: &DataTransferState, source: SourceDataObject) {
    let data_object = unsafe { ComPtr::from_borrowed(source.interface_ptr().cast()) }.unwrap();
    state.register_drag(DRAG, WindowId::from_raw(1), data_object);
}

/// Logs, when the data object that owns it is destroyed, whether the state was free.
#[derive(Debug)]
struct ReleaseProbe {
    state: SameThread<DataTransferState>,
    released: Arc<Mutex<Vec<bool>>>,
}

impl Drop for ReleaseProbe {
    fn drop(&mut self) {
        let state = unsafe { self.state.get() };
        self.released.lock().unwrap().push(state_is_free(state));
    }
}

fn probed(state: &DataTransferState, released: &Arc<Mutex<Vec<bool>>>) -> SourceDataObject {
    let probe = ReleaseProbe { state: SameThread(state), released: Arc::clone(released) };
    let send = DataTransferSendBuilder::new(probe)
        .with_type(
            TypeHint::Plaintext,
            |_: &ReleaseProbe, _: &dyn TransferType| Some(String::new()),
        )
        .build();
    SourceDataObject::new(send)
}

#[test]
fn a_drag_is_read_one_format_at_a_time() {
    ensure_ole_initialized().unwrap();
    let state = DataTransferState::default();
    let reads = Reads::default();
    drag(&state, offer(&state, &reads, |_| ()));

    let formats = state.available_formats(DRAG).unwrap();
    let hints: Vec<_> = formats.iter().map(|available| available.format.hint()).collect();
    assert_eq!(hints, [Some(TypeHint::Plaintext), Some(TypeHint::UriList), Some(PNG)]);
    assert!(reads.lock().unwrap().is_empty(), "listing the formats read the data");

    let text = state.read(DRAG, &TypeHint::Plaintext).unwrap();
    assert_eq!(text.try_as_string().unwrap(), "text");
    assert_eq!(*reads.lock().unwrap(), [(TypeHint::Plaintext, true)]);

    let files = state.read(DRAG, &TypeHint::UriList).unwrap();
    assert_eq!(files.try_as_uris().unwrap(), [FILE_URI]);
    let image = state.read(DRAG, &TypeHint::Image { extension_hint: None }).unwrap();
    assert_eq!(image.type_().hint(), Some(PNG));
    assert_eq!(image.try_as_bytes().unwrap(), PNG_BYTES);
    assert_eq!(*reads.lock().unwrap(), [
        (TypeHint::Plaintext, true),
        (TypeHint::UriList, true),
        (PNG, true)
    ]);
}

#[test]
fn a_drag_that_ends_while_its_data_is_read_is_not_delivered() {
    ensure_ole_initialized().unwrap();
    let state = DataTransferState::default();
    let reads = Reads::default();
    // The drag leaves while the source renders the data.
    drag(
        &state,
        offer(&state, &reads, |state| {
            if state_is_free(state) {
                state.remove_drag(DRAG);
            }
        }),
    );

    let result = state.read(DRAG, &TypeHint::Plaintext);
    assert!(matches!(result, Err(TransferError::UnknownTransfer(id)) if id == DRAG), "{result:?}");
    assert_eq!(*reads.lock().unwrap(), [(TypeHint::Plaintext, true)]);
    // The data object outlived the drag while it was read, and was released after.
    assert_eq!(Arc::strong_count(&reads), 1);
}

#[test]
fn a_drag_releases_its_data_object_with_no_state_borrowed() {
    ensure_ole_initialized().unwrap();
    let state = DataTransferState::default();
    let released = Arc::default();
    drag(&state, probed(&state, &released));
    // A drag that enters replaces the drag before it.
    drag(&state, probed(&state, &released));
    assert_eq!(*released.lock().unwrap(), [true]);
    state.remove_drag(DRAG);
    assert_eq!(*released.lock().unwrap(), [true, true]);
    drag(&state, probed(&state, &released));
    state.reset();
    assert_eq!(*released.lock().unwrap(), [true, true, true]);
}

/// The `[StartHTML, EndHTML, StartFragment, EndFragment]` offsets of an HTML Clipboard Format
/// buffer built by `build_html_clipboard_format`.
fn html_offsets(buf: &[u8]) -> [usize; 4] {
    let text = std::str::from_utf8(buf).unwrap();
    ["StartHTML:", "EndHTML:", "StartFragment:", "EndFragment:"].map(|name| {
        let at = text.find(name).unwrap() + name.len();
        text[at..at + 10].parse().unwrap()
    })
}

fn html_fragment(buf: &[u8]) -> &str {
    let [_, _, start, end] = html_offsets(buf);
    std::str::from_utf8(&buf[start..end]).unwrap()
}

/// Runs `retry_while_clipboard_busy` with attempts that return `results` in turn, repeating the
/// last one, and returns its result with the number of attempts and of waits.
fn retry(results: &[HRESULT]) -> (HRESULT, usize, usize) {
    let mut attempts = 0;
    let mut waits = 0;
    let hr = retry_while_clipboard_busy(
        || {
            attempts += 1;
            results[(attempts - 1).min(results.len() - 1)]
        },
        || waits += 1,
    );
    (hr, attempts, waits)
}

#[test]
fn a_busy_clipboard_is_retried_until_it_opens() {
    let busy = CLIPBRD_E_CANT_OPEN;
    assert_eq!(retry(&[busy, busy, S_OK]), (S_OK, 3, 2));
}

#[test]
fn a_busy_clipboard_is_given_up_after_five_attempts() {
    assert_eq!(retry(&[CLIPBRD_E_CANT_OPEN]), (CLIPBRD_E_CANT_OPEN, 5, 4));
}

#[test]
fn other_results_are_returned_at_once() {
    assert_eq!(retry(&[S_OK]), (S_OK, 1, 0));
    assert_eq!(retry(&[E_FAIL, S_OK]), (E_FAIL, 1, 0));
}

#[test]
fn html_with_a_start_marker_but_no_end_marker_is_wrapped() {
    let html = "<p>a</p><!--StartFragment--><p>b</p>";
    let buf = build_html_clipboard_format(html);
    assert_eq!(html_fragment(&buf), html);
}

#[test]
fn html_with_the_end_marker_before_the_start_marker_is_wrapped() {
    let html = "<!--EndFragment--><p>a</p><!--StartFragment-->";
    let buf = build_html_clipboard_format(html);
    let [start_html, end_html, start_fragment, end_fragment] = html_offsets(&buf);
    assert!(start_html <= start_fragment);
    assert!(start_fragment <= end_fragment);
    assert!(end_fragment <= end_html);
    assert_eq!(end_html, buf.len());
    assert_eq!(html_fragment(&buf), html);
}

#[test]
fn html_with_its_own_fragment_markers_is_kept_as_is() {
    let html = "<html><body>x<!--StartFragment--><b>y</b><!--EndFragment-->z</body></html>";
    let buf = build_html_clipboard_format(html);
    let [start_html, end_html, ..] = html_offsets(&buf);
    assert_eq!(&buf[start_html..end_html], html.as_bytes());
    assert_eq!(html_fragment(&buf), "<b>y</b>");
}

#[test]
fn html_without_fragment_markers_is_wrapped() {
    let html = "<b>y</b><!--EndFragment-->";
    let buf = build_html_clipboard_format(html);
    let [start_html, end_html, ..] = html_offsets(&buf);
    assert_eq!(end_html, buf.len());
    let document = std::str::from_utf8(&buf[start_html..end_html]).unwrap();
    assert!(document.starts_with("<html><body>"));
    assert!(document.ends_with("</body></html>"));
    assert_eq!(html_fragment(&buf), html);
}
