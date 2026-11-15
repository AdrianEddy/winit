use sctk::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_v3::ZwpTextInputV3;
use winit_core::event::Ime;
use winit_core::window::{ImeCapabilities, ImeRequest, ImeRequestError};

use super::WindowState;

impl WindowState {
    /// Whether the IME is allowed.
    #[inline]
    pub fn ime_allowed(&self) -> Option<ImeCapabilities> {
        self.ime.state().map(|state| state.capabilities())
    }

    /// Atomically update input method state.
    ///
    /// A request that enables or disables the input method sends one `wl_display.sync` through
    /// `sync`, with the fences of the text inputs it disabled (`ImeWindow::request`).
    ///
    /// Returns `None` if an input method state haven't changed. Alternatively `Some(true)` and
    /// `Some(false)` is returned respectfully.
    pub fn request_ime_update(
        &mut self,
        request: ImeRequest,
        sync: impl FnOnce(Vec<(ZwpTextInputV3, u32)>),
    ) -> Result<Option<bool>, ImeRequestError> {
        let scale_factor = self.scale_factor();
        self.ime.request(request, scale_factor, sync)
    }

    /// Register text input on the top-level. Returns whether the window's `Ime::Enabled` is
    /// pushed now.
    #[inline]
    pub fn text_input_entered(&mut self, text_input: &ZwpTextInputV3) -> bool {
        self.ime.enter(text_input)
    }

    /// The text input left the top-level, by its `leave` or its destruction with its seat. Returns
    /// the events its removal pushes (`ImeWindow::remove`).
    #[inline]
    pub fn text_input_removed(&mut self, text_input: &ZwpTextInputV3) -> Vec<Ime> {
        self.ime.remove(text_input)
    }

    /// A `done` of a text input entered on the top-level. Returns the events it delivers
    /// (`ImeWindow::done`); `sync` sends the sync of a displaced composer's reset.
    pub fn text_input_done(
        &mut self,
        text_input: &ZwpTextInputV3,
        serial: u32,
        sync: impl FnOnce(Vec<(ZwpTextInputV3, u32)>),
    ) -> Vec<Ime> {
        self.ime.done(text_input, serial, sync)
    }

    /// Whether the window's `Ime::Enabled` is pushed now, as the opening a request owed it
    /// (`ImeWindow::take_opening`).
    pub(crate) fn take_ime_opening(&mut self) -> bool {
        self.ime.take_opening()
    }
}
