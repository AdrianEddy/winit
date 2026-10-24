use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};
use std::{mem, ptr};

use windows_sys::Win32::System::Threading::GetCurrentThreadId;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, HWND_MESSAGE, PM_REMOVE, PeekMessageW,
};
use winit_core::application::ApplicationHandler;
use winit_core::event::{StartCause, WindowEvent};
use winit_core::event_loop::{ActiveEventLoop, ControlFlow};
use winit_core::window::WindowId;

use super::{Event, EventLoopRunner};
use crate::event_loop::wait_for_messages_impl;
use crate::util;

/// Records the events it handles, and sends an event from its first `about_to_wait`, as a data
/// transfer fetch does.
struct App {
    runner: Rc<EventLoopRunner>,
    log: Rc<RefCell<Vec<&'static str>>>,
    sent: bool,
}

impl ApplicationHandler for App {
    fn new_events(&mut self, _: &dyn ActiveEventLoop, _: StartCause) {
        self.log.borrow_mut().push("new_events");
    }

    fn can_create_surfaces(&mut self, _: &dyn ActiveEventLoop) {}

    fn window_event(&mut self, _: &dyn ActiveEventLoop, _: WindowId, _: WindowEvent) {}

    fn proxy_wake_up(&mut self, _: &dyn ActiveEventLoop) {
        self.log.borrow_mut().push("proxy_wake_up");
    }

    fn about_to_wait(&mut self, _: &dyn ActiveEventLoop) {
        self.log.borrow_mut().push("about_to_wait");
        if !self.sent {
            self.sent = true;
            self.runner.send_event(Event::WakeUp);
        }
    }
}

#[test]
fn an_event_sent_from_about_to_wait_ends_the_wait() {
    // A message-only window, which gets no messages unless they are sent to it.
    let class = util::encode_wide("STATIC");
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
    let runner = Rc::new(EventLoopRunner::new(unsafe { GetCurrentThreadId() }, window));
    let log = Rc::new(RefCell::new(Vec::new()));
    let mut app = App { runner: runner.clone(), log: log.clone(), sent: false };
    let app_reset = unsafe { runner.set_app(&mut app) };
    // Nothing queued before the loop starts may end the wait.
    let mut msg = unsafe { mem::zeroed() };
    while unsafe { PeekMessageW(&mut msg, ptr::null_mut(), 0, 0, PM_REMOVE) } != 0 {
        unsafe { DispatchMessageW(&msg) };
    }

    runner.prepare_wait();
    let start = Instant::now();
    wait_for_messages_impl(&mut None, ControlFlow::Wait, Some(Duration::from_secs(2)));
    let waited = start.elapsed();
    runner.wakeup();

    drop(app_reset);
    unsafe { DestroyWindow(window) };
    assert!(waited < Duration::from_secs(1), "the wait lasted {waited:?}");
    assert_eq!(*log.borrow(), ["new_events", "about_to_wait", "new_events", "proxy_wake_up"]);
}
