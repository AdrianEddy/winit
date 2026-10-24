//! Whether rendering a delay-rendered clipboard format changes the clipboard's sequence number.
//!
//! `OleSetClipboard` publishes every format of a data object for delayed rendering, so another
//! application's clipboard data is rendered by the first read of each format. A clipboard
//! transfer is current while the sequence number is unchanged, so a render must not change it.

use std::cell::Cell;
use std::time::Duration;
use std::{ptr, thread};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber, OpenClipboard,
    SetClipboardData,
};
use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};
use windows_sys::Win32::System::Ole::CF_UNICODETEXT;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, HWND_MESSAGE, RegisterClassW, WM_RENDERFORMAT,
    WNDCLASSW,
};
use winit_core::data_transfer::TypedData;

use crate::clipboard_format::ClipboardFormat;
use crate::data_transfer::{DataTransferState, alloc_hglobal_from, ensure_ole_initialized};
use crate::util;

/// The data the test window renders.
const RENDERED: &[u8] = b"rendered";

thread_local! {
    /// How many times the test window rendered a format.
    static RENDERS: Cell<u32> = const { Cell::new(0) };
}

unsafe extern "system" fn render_window_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_RENDERFORMAT {
        RENDERS.set(RENDERS.get() + 1);
        // The clipboard is already open for the reader, and takes over the memory.
        if let Some(hglobal) = alloc_hglobal_from(RENDERED) {
            unsafe { SetClipboardData(wparam as u32, hglobal) };
        }
        return 0;
    }
    unsafe { DefWindowProcW(window, msg, wparam, lparam) }
}

/// A message-only window that renders formats as [`RENDERED`].
fn render_window() -> HWND {
    let class = util::encode_wide("winit-win32 test render window");
    let class_info = WNDCLASSW {
        lpfnWndProc: Some(render_window_proc),
        hInstance: util::get_instance_handle(),
        lpszClassName: class.as_ptr(),
        ..unsafe { std::mem::zeroed() }
    };
    unsafe { RegisterClassW(&class_info) };
    let window = unsafe {
        CreateWindowExW(
            0,
            class.as_ptr(),
            ptr::null(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            ptr::null_mut(),
            util::get_instance_handle(),
            ptr::null(),
        )
    };
    assert!(!window.is_null());
    window
}

/// Opens the clipboard for `owner`, waiting while another application has it open.
fn open_clipboard(owner: HWND) {
    for _ in 0..100 {
        if unsafe { OpenClipboard(owner) } != 0 {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("the clipboard stayed open in another application");
}

/// The clipboard's Unicode text, without its terminating NUL.
fn clipboard_text() -> Option<Vec<u16>> {
    open_clipboard(ptr::null_mut());
    let hglobal = unsafe { GetClipboardData(CF_UNICODETEXT as u32) };
    let data = if hglobal.is_null() { ptr::null_mut() } else { unsafe { GlobalLock(hglobal) } };
    let text = (!data.is_null()).then(|| {
        let len = unsafe { GlobalSize(hglobal) } / 2;
        let units = unsafe { std::slice::from_raw_parts(data.cast::<u16>(), len) };
        let text = units.iter().copied().take_while(|&unit| unit != 0).collect();
        unsafe { GlobalUnlock(hglobal) };
        text
    });
    unsafe { CloseClipboard() };
    text
}

/// Replaces the clipboard's contents with `text`, owned by `owner`.
fn set_clipboard_text(owner: HWND, text: Option<Vec<u16>>) {
    open_clipboard(owner);
    unsafe { EmptyClipboard() };
    if let Some(mut text) = text {
        text.push(0);
        let bytes: Vec<u8> = text.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        if let Some(hglobal) = alloc_hglobal_from(&bytes) {
            unsafe { SetClipboardData(CF_UNICODETEXT as u32, hglobal) };
        }
    }
    unsafe { CloseClipboard() };
}

#[test]
#[ignore = "writes the system clipboard; restores its text only"]
fn rendering_a_delay_rendered_format_keeps_the_clipboard_transfer() {
    ensure_ole_initialized().unwrap();
    let format = ClipboardFormat::register("winit-win32 test format").unwrap();
    let window = render_window();
    let saved = clipboard_text();

    // Publish the format for delayed rendering, as `OleSetClipboard` publishes every format.
    open_clipboard(window);
    unsafe { EmptyClipboard() };
    unsafe { SetClipboardData(u32::from(format.id()), ptr::null_mut()) };
    unsafe { CloseClipboard() };
    let published = unsafe { GetClipboardSequenceNumber() };

    // The read sends `WM_RENDERFORMAT` to the window, which renders the format.
    let state = DataTransferState::default();
    let id = state.clipboard_transfer_id();
    let read = state.read(id, &format);
    let rendered = unsafe { GetClipboardSequenceNumber() };
    let renders = RENDERS.get();

    set_clipboard_text(window, saved);
    unsafe { DestroyWindow(window) };
    assert_eq!(renders, 1);
    assert_eq!(rendered, published, "rendering the format changed the sequence number");
    assert_eq!(read.unwrap().try_as_bytes().unwrap(), RENDERED);
}
