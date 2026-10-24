//! Drops driven through the drop target of a window, the way OLE drives them, with an
//! application that reads the data of a drop in the handler of `DragDropped`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows_sys::Win32::Foundation::{HWND, POINTL};
use windows_sys::Win32::System::Ole::{CF_UNICODETEXT, DROPEFFECT_COPY};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow;
use winit_core::application::ApplicationHandler;
use winit_core::data_transfer::{TypeHint, TypedData};
use winit_core::event::WindowEvent;
use winit_core::event_loop::{ActiveEventLoop, DndAction};
use winit_core::window::WindowId;

use super::message_window;
use super::test_source::{Medium, TestSource, utf16_text};
use crate::data_transfer::{ComPtr, FileDropHandler, ensure_ole_initialized};
use crate::definitions::{IDataObject, IDropTarget};
use crate::event_loop::{Event, EventLoopRunner};

const WINDOW: WindowId = WindowId::from_raw(1);

/// An application that accepts copies, reads the text of each drop, and runs `while_focused` in
/// the handler of `Focused`.
struct DropApp {
    runner: Rc<EventLoopRunner>,
    /// What the handler of each `DragDropped` read.
    reads: Rc<RefCell<Vec<Result<String, String>>>>,
    while_focused: Option<Box<dyn FnOnce()>>,
}

impl ApplicationHandler for DropApp {
    fn can_create_surfaces(&mut self, _: &dyn ActiveEventLoop) {}

    fn window_event(&mut self, _: &dyn ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let transfers = self.runner.data_transfer();
        match event {
            WindowEvent::DragEntered { id, .. } => {
                transfers.set_valid_dnd_actions(id, &[DndAction::Copy]);
            },
            WindowEvent::DragDropped { id, .. } => {
                let text =
                    transfers.read(id, &TypeHint::Plaintext).map_err(|err| format!("{err:?}"));
                let text =
                    text.and_then(|data| data.try_as_string().map_err(|err| err.to_string()));
                self.reads.borrow_mut().push(text);
            },
            WindowEvent::Focused(true) => {
                if let Some(while_focused) = self.while_focused.take() {
                    while_focused();
                }
            },
            _ => {},
        }
    }
}

/// A runner on a message-only window, and a drop target that sends its events to it.
struct Target {
    window: HWND,
    runner: Rc<EventLoopRunner>,
    drop_target: FileDropHandler,
}

impl Target {
    fn new() -> Self {
        ensure_ole_initialized().unwrap();
        let window = message_window();
        let runner = Rc::new(EventLoopRunner::new(unsafe { GetCurrentThreadId() }, window));
        let events = Rc::clone(&runner);
        let send_event =
            Box::new(move |event| events.send_event(Event::Window { window_id: WINDOW, event }));
        let drop_target = FileDropHandler::new(window, Rc::clone(&runner), send_event);
        Self { window, runner, drop_target }
    }

    fn interface(&mut self) -> *mut IDropTarget {
        unsafe { self.drop_target.interface_unchecked_mut() }
    }

    fn has_drag(&self) -> bool {
        self.runner.data_transfer().drag.borrow().is_some()
    }
}

impl Drop for Target {
    fn drop(&mut self) {
        unsafe { DestroyWindow(self.window) };
    }
}

/// The drop target's answers to a drag of `data_object` that enters and moves, as OLE asks for
/// them, with the source allowing copies.
fn enter_and_move(target: *mut IDropTarget, data_object: &ComPtr<IDataObject>) -> [u32; 2] {
    let point = POINTL { x: 0, y: 0 };
    let mut entered = DROPEFFECT_COPY;
    let mut moved = DROPEFFECT_COPY;
    unsafe { FileDropHandler::DragEnter(target, data_object.as_ptr(), 0, point, &mut entered) };
    unsafe { FileDropHandler::DragOver(target, 0, point, &mut moved) };
    [entered, moved]
}

/// The drop target's answer to the drop of `data_object`.
fn drop_on(target: *mut IDropTarget, data_object: &ComPtr<IDataObject>) -> u32 {
    let mut effect = DROPEFFECT_COPY;
    let point = POINTL { x: 0, y: 0 };
    unsafe { FileDropHandler::Drop(target, data_object.as_ptr(), 0, point, &mut effect) };
    effect
}

fn dropped_text() -> ComPtr<IDataObject> {
    TestSource::new(vec![(CF_UNICODETEXT, Medium::Global(utf16_text("dropped")))])
        .into_data_object()
}

#[test]
fn the_handler_of_a_drop_reads_its_data() {
    let mut target = Target::new();
    let interface = target.interface();
    let reads = Rc::default();
    let mut app = DropApp {
        runner: Rc::clone(&target.runner),
        reads: Rc::clone(&reads),
        while_focused: None,
    };
    let app_reset = unsafe { target.runner.set_app(&mut app) };
    let data_object = dropped_text();

    assert_eq!(enter_and_move(interface, &data_object), [DROPEFFECT_COPY; 2]);
    // The handler of `DragDropped` makes the first read of the data.
    assert_eq!(drop_on(interface, &data_object), DROPEFFECT_COPY);

    drop(app_reset);
    if let Err(panic) = target.runner.take_panic_error() {
        std::panic::resume_unwind(panic);
    }
    assert_eq!(*reads.borrow(), [Ok("dropped".to_owned())]);
    assert!(!target.has_drag(), "the drop was not released after its handler read it");
}

#[test]
fn a_drop_buffered_while_a_handler_runs_is_read_once_the_handler_returns() {
    let mut target = Target::new();
    let reads = Rc::default();
    let data_object = dropped_text();
    let interface = target.interface();
    // The drop comes from a modal loop inside a handler, so its events are buffered.
    let during_handler = Rc::new(Cell::new(None));
    let drop_in_handler = {
        let data_object = data_object.clone();
        let during_handler = Rc::clone(&during_handler);
        let runner = Rc::clone(&target.runner);
        move || {
            let effect = drop_on(interface, &data_object);
            during_handler.set(Some((effect, runner.data_transfer().drag.borrow().is_some())));
        }
    };
    let mut app = DropApp {
        runner: Rc::clone(&target.runner),
        reads: Rc::clone(&reads),
        while_focused: Some(Box::new(drop_in_handler)),
    };
    let app_reset = unsafe { target.runner.set_app(&mut app) };

    // The drag enters while the application is idle, which accepts a copy.
    assert_eq!(enter_and_move(interface, &data_object), [DROPEFFECT_COPY; 2]);
    target
        .runner
        .send_event(Event::Window { window_id: WINDOW, event: WindowEvent::Focused(true) });

    drop(app_reset);
    if let Err(panic) = target.runner.take_panic_error() {
        std::panic::resume_unwind(panic);
    }
    // The drop stayed readable until its buffered handler had read it.
    assert_eq!(during_handler.get(), Some((DROPEFFECT_COPY, true)));
    assert_eq!(*reads.borrow(), [Ok("dropped".to_owned())]);
    assert!(!target.has_drag(), "the drop was not released after its handler read it");
}
