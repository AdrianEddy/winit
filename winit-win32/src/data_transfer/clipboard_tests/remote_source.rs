//! Data objects served from another single-threaded apartment, as another application's are.
//!
//! A call into such an object runs COM's modal loop on the calling thread, which processes the
//! messages that other threads send to the caller while it waits. (It was not seen to dispatch a
//! message posted to the caller.) A message sent during the call uses the transfer state
//! again: it ends the drag being read, starts a new one, and lists and reads that one.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Once, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use std::{mem, panic, ptr};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, S_OK, WPARAM};
use windows_sys::Win32::System::Com::Marshal::CoMarshalInterThreadInterfaceInStream;
use windows_sys::Win32::System::Com::StructuredStorage::CoGetInterfaceAndReleaseStream;
use windows_sys::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HWND_MESSAGE,
    PM_REMOVE, PeekMessageW, PostThreadMessageW, RegisterClassW, SendNotifyMessageW, WM_APP,
    WM_QUIT, WNDCLASSW,
};
use winit_core::data_transfer::{TypeHint, TypedData};
use winit_core::error::TransferError;
use winit_core::window::WindowId;

use super::test_source::{Calls, Medium, TestSource, utf16_text};
use super::{DRAG, SameThread};
use crate::data_transfer::{ComPtr, DataTransferState, ensure_ole_initialized};
use crate::definitions::{IDataObject, IID_IDataObject};
use crate::util;

/// The drag that starts while the first one is read.
const NEW_DRAG: winit_core::data_transfer::DataTransferId =
    winit_core::data_transfer::DataTransferId::from_raw(2);

/// The message that makes the re-entry window run its re-entry.
const REENTER: u32 = WM_APP + 1;

thread_local! {
    /// What the re-entry window runs when it gets `REENTER`.
    static REENTRY: Cell<Option<Box<dyn FnOnce()>>> = const { Cell::new(None) };
    /// Set when a re-entry panicked.
    static REENTRY_PANICKED: Cell<bool> = const { Cell::new(false) };
}

unsafe extern "system" fn reentry_window_proc(
    window: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == REENTER {
        if let Some(reentry) = REENTRY.take() {
            // A panic must not unwind into the modal loop that dispatched the message.
            if panic::catch_unwind(panic::AssertUnwindSafe(reentry)).is_err() {
                REENTRY_PANICKED.set(true);
            }
        }
        return 0;
    }
    unsafe { DefWindowProcW(window, msg, wparam, lparam) }
}

/// A message-only window that runs [`REENTRY`] when it gets [`REENTER`].
fn reentry_window() -> HWND {
    let class = util::encode_wide("winit-win32 test re-entry window");
    static REGISTER: Once = Once::new();
    REGISTER.call_once(|| {
        let class_info = WNDCLASSW {
            lpfnWndProc: Some(reentry_window_proc),
            hInstance: util::get_instance_handle(),
            lpszClassName: class.as_ptr(),
            ..unsafe { mem::zeroed() }
        };
        assert_ne!(unsafe { RegisterClassW(&class_info) }, 0);
    });
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

/// A data object served from a thread of its own, which runs a single-threaded apartment.
struct Remote {
    thread_id: u32,
    thread: JoinHandle<()>,
}

impl Remote {
    /// Serves the data object of the source that `make` creates, and returns the proxy to it,
    /// which holds the only reference.
    fn serve(make: impl FnOnce() -> TestSource + Send + 'static) -> (Self, ComPtr<IDataObject>) {
        let (sender, receiver) = mpsc::channel();
        let thread = thread::spawn(move || {
            ensure_ole_initialized().unwrap();
            let data_object = make().into_data_object();
            let mut stream = ptr::null_mut();
            let hr = unsafe {
                CoMarshalInterThreadInterfaceInStream(
                    &IID_IDataObject,
                    data_object.as_unknown().cast(),
                    &mut stream,
                )
            };
            assert_eq!(hr, S_OK);
            // The marshalled reference keeps the object alive.
            drop(data_object);
            sender.send((unsafe { GetCurrentThreadId() }, stream as usize)).unwrap();
            let mut msg = unsafe { mem::zeroed() };
            while unsafe { GetMessageW(&mut msg, ptr::null_mut(), 0, 0) } > 0 {
                unsafe { DispatchMessageW(&msg) };
            }
        });
        let (thread_id, stream) = receiver.recv().unwrap();
        let mut proxy = ptr::null_mut();
        let hr = unsafe {
            CoGetInterfaceAndReleaseStream(stream as *mut c_void, &IID_IDataObject, &mut proxy)
        };
        assert_eq!(hr, S_OK);
        let proxy = unsafe { ComPtr::from_raw(proxy.cast()) }.unwrap();
        (Self { thread_id, thread }, proxy)
    }

    /// Stops serving, once the data object has been released.
    fn finish(self) {
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, 0, 0) };
        self.thread.join().unwrap();
    }
}

/// Dispatches the messages of this thread until `condition` holds, or five seconds have passed.
fn pump_until(condition: impl Fn() -> bool) -> bool {
    let start = Instant::now();
    while !condition() {
        if start.elapsed() > Duration::from_secs(5) {
            return false;
        }
        let mut msg = unsafe { mem::zeroed() };
        while unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
            unsafe { DispatchMessageW(&msg) };
        }
        thread::sleep(Duration::from_millis(1));
    }
    true
}

/// Whether the re-entry ran while the remote call waited for it.
#[derive(Debug, Default)]
struct Reentered {
    done: Mutex<bool>,
    signal: Condvar,
    timed_out: AtomicBool,
}

/// What the re-entry saw.
#[derive(Debug, Default)]
struct Seen {
    remote_alive: bool,
    new_formats: Vec<u16>,
    new_text: String,
}

fn ids(formats: &[crate::clipboard_format::AvailableFormat]) -> Vec<u16> {
    formats.iter().map(|available| available.format.id()).collect()
}

/// Runs `outer` on a drag served from another apartment, whose call `trigger` posts a message
/// that the caller's modal loop dispatches, and checks what the re-entry did.
fn reenter_during(trigger: &'static str, outer: fn(&DataTransferState) -> Option<TransferError>) {
    ensure_ole_initialized().unwrap();
    let window = reentry_window() as usize;
    let calls = Arc::new(Calls::default());
    let reentered = Arc::new(Reentered::default());
    let (remote, data_object) = Remote::serve({
        let calls = Arc::clone(&calls);
        let reentered = Arc::clone(&reentered);
        move || {
            let mut source = TestSource::new(vec![
                (CF_UNICODETEXT, Medium::Global(utf16_text("remote"))),
                (CF_HDROP, Medium::Global(Vec::new())),
            ]);
            source.calls = calls;
            let fired = AtomicBool::new(false);
            source.hook = Some(Box::new(move |call| {
                if call != trigger || fired.swap(true, Ordering::SeqCst) {
                    return;
                }
                // Sent without waiting: the call waits below.
                unsafe { SendNotifyMessageW(window as HWND, REENTER, 0, 0) };
                let done = reentered.done.lock().unwrap();
                let wait = Duration::from_secs(5);
                let (done, waited) =
                    reentered.signal.wait_timeout_while(done, wait, |done| !*done).unwrap();
                drop(done);
                reentered.timed_out.store(waited.timed_out(), Ordering::SeqCst);
            }));
            source
        }
    });

    let state = DataTransferState::default();
    state.register_drag(DRAG, WindowId::from_raw(1), data_object);
    let seen = Rc::new(RefCell::new(None));
    REENTRY.set(Some(Box::new({
        let state = SameThread(&state);
        let calls = Arc::clone(&calls);
        let reentered = Arc::clone(&reentered);
        let seen = Rc::clone(&seen);
        move || {
            let state = unsafe { state.get() };
            let remote_alive = !calls.destroyed.load(Ordering::SeqCst);
            // The drag being read ends, and a new one starts, of local data.
            state.remove_drag(DRAG);
            let local =
                TestSource::new(vec![(CF_UNICODETEXT, Medium::Global(utf16_text("local")))]);
            state.register_drag(NEW_DRAG, WindowId::from_raw(1), local.into_data_object());
            let new_formats = ids(&state.available_formats(NEW_DRAG).unwrap());
            let new_text = state.read(NEW_DRAG, &TypeHint::Plaintext).unwrap();
            let new_text = new_text.try_as_string().unwrap();
            *seen.borrow_mut() = Some(Seen { remote_alive, new_formats, new_text });
            *reentered.done.lock().unwrap() = true;
            reentered.signal.notify_all();
        }
    })));

    let outer_error = outer(&state);

    REENTRY.take();
    let panicked = REENTRY_PANICKED.replace(false);
    let released = pump_until(|| calls.destroyed.load(Ordering::SeqCst));
    let new_formats = state.available_formats(NEW_DRAG).map(|formats| ids(&formats));
    state.reset();
    remote.finish();
    unsafe { DestroyWindow(window as HWND) };

    assert!(!panicked, "the re-entry panicked");
    assert!(*reentered.done.lock().unwrap(), "the re-entry did not run during {trigger}");
    assert!(!reentered.timed_out.load(Ordering::SeqCst), "the re-entry ran after {trigger}");
    let seen = seen.borrow_mut().take().unwrap();
    // The data object of the ended drag stayed alive while it was called.
    assert!(seen.remote_alive);
    assert_eq!(seen.new_formats, [CF_UNICODETEXT]);
    assert_eq!(seen.new_text, "local");
    // The call into the ended drag reports it as ended, and left the new drag as it was.
    assert!(
        matches!(outer_error, Some(TransferError::UnknownTransfer(id)) if id == DRAG),
        "{outer_error:?}"
    );
    assert_eq!(new_formats.unwrap(), [CF_UNICODETEXT]);
    // The ended drag's data object was released once the call had returned.
    assert!(released, "the data object of the ended drag was not released");
}

#[test]
fn a_drag_read_from_another_apartment_survives_its_end_during_the_read() {
    reenter_during("GetData", |state| state.read(DRAG, &TypeHint::Plaintext).err());
}

#[test]
fn a_drag_listed_from_another_apartment_survives_its_end_during_the_listing() {
    reenter_during("EnumFormatEtc", |state| state.available_formats(DRAG).err());
}
