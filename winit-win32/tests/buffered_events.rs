//! An event sent from `about_to_wait` is delivered without waiting for another message.
#![cfg(target_os = "windows")]

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::data_transfer::{DataTransferSendBuilder, TypeHint};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::platform::windows::EventLoopBuilderExtWindows;
use winit::window::{Window, WindowAttributes, WindowId};

/// Copies text, fetches it from `about_to_wait`, and records how long the fetch took to arrive.
struct App {
    window: Option<Box<dyn Window>>,
    fetched_at: Option<Instant>,
    delivered: Rc<RefCell<Option<(Duration, String)>>>,
}

impl ApplicationHandler for App {
    fn can_create_surfaces(&mut self, event_loop: &dyn ActiveEventLoop) {
        // A clipboard fetch is delivered to a window.
        let attributes = WindowAttributes::default().with_visible(false);
        self.window = Some(event_loop.create_window(attributes).unwrap());
        let text = DataTransferSendBuilder::new("probe".to_owned())
            .with_type(TypeHint::Plaintext, |text: &String, _| Some(text.clone()))
            .build();
        event_loop.set_clipboard(text).unwrap();
        event_loop.set_control_flow(ControlFlow::Wait);
        // Ends the loop if the fetch is never delivered.
        let proxy = event_loop.create_proxy();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            proxy.wake_up();
        });
    }

    fn about_to_wait(&mut self, event_loop: &dyn ActiveEventLoop) {
        if self.fetched_at.is_none() {
            let id = event_loop.clipboard().unwrap().unwrap();
            event_loop.fetch_data_transfer(id, &TypeHint::Plaintext).unwrap();
            self.fetched_at = Some(Instant::now());
        }
    }

    fn window_event(&mut self, event_loop: &dyn ActiveEventLoop, _: WindowId, event: WindowEvent) {
        if let WindowEvent::DataTransferReceived { value, .. } = event {
            let elapsed = self.fetched_at.unwrap().elapsed();
            *self.delivered.borrow_mut() = Some((elapsed, value.try_as_string().unwrap()));
            event_loop.exit();
        }
    }

    fn proxy_wake_up(&mut self, event_loop: &dyn ActiveEventLoop) {
        event_loop.exit();
    }
}

#[test]
#[ignore = "writes the system clipboard"]
fn a_fetch_from_about_to_wait_is_delivered_without_another_message() {
    let event_loop = EventLoop::builder().with_any_thread(true).build().unwrap();
    let delivered = Rc::default();
    let app = App { window: None, fetched_at: None, delivered: Rc::clone(&delivered) };
    event_loop.run_app(app).unwrap();
    let (elapsed, text) = delivered.take().expect("the fetch was not delivered");
    assert_eq!(text, "probe");
    assert!(elapsed < Duration::from_secs(1), "the fetch arrived after {elapsed:?}");
}
