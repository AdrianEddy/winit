//! The input method of one window, over the text inputs entered on it.

use winit_core::window::{ImeRequest, ImeRequestError};

use super::{ClientState, TextInputHandle};

/// The input method of one window: the state the application requested, and the text inputs
/// entered on the window — its members, one for each seat whose text-input focus is on it.
///
/// Only one input method may be active per (seat, surface), but several seats can focus one
/// surface. There is no API to set a per-seat input method state, so the members share one state.
#[derive(Debug)]
pub struct ImeWindow<T> {
    /// The input method properties provided by the application to the IME, `None` while it is
    /// disabled.
    ///
    /// This state is cached here so that the window can automatically send the state to the IME as
    /// soon as it becomes available without application involvement.
    state: Option<ClientState>,

    /// The text inputs entered on the window.
    members: Vec<T>,

    /// A request enabled the input method, and the window's `Ime::Enabled` is not pushed yet.
    opening_owed: bool,
}

impl<T> Default for ImeWindow<T> {
    fn default() -> Self {
        Self { state: None, members: Vec::new(), opening_owed: false }
    }
}

impl<T: TextInputHandle> ImeWindow<T> {
    /// The state the application requested, `None` while the input method is disabled.
    pub fn state(&self) -> Option<&ClientState> {
        self.state.as_ref()
    }

    /// Applies an application's request to the window's text inputs atomically.
    ///
    /// A request that enables or disables the input method is a transition, and `sync` sends its
    /// `wl_display.sync`: after the disables, listing them with their fences, or before the
    /// enables. No `Ime::Enabled` is pushed here: an enable owes the window its opening, which the
    /// compositor's dispatch pushes, behind every event the compositor sent before it applied the
    /// window's latest disables (`take_opening`).
    ///
    /// Returns `None` if the input method state did not change, or `Some(allowed)` otherwise.
    pub fn request(
        &mut self,
        request: ImeRequest,
        scale_factor: f64,
        sync: impl FnOnce(Vec<(T, u32)>),
    ) -> Result<Option<bool>, ImeRequestError> {
        let state_change = match request {
            ImeRequest::Enable(enable) => {
                let (capabilities, request_data) = enable.into_raw();

                if self.state.is_some() {
                    return Err(ImeRequestError::AlreadyEnabled);
                }

                self.state = Some(ClientState::new(capabilities, request_data, scale_factor));
                true
            },
            ImeRequest::Update(request_data) => {
                if let Some(state) = self.state.as_mut() {
                    state.update(request_data, scale_factor);
                } else {
                    return Err(ImeRequestError::NotEnabled);
                }
                false
            },
            ImeRequest::Disable => {
                self.state = None;
                true
            },
            _ => return Err(ImeRequestError::NotSupported),
        };

        match (&self.state, state_change) {
            (Some(state), false) => {
                for member in &self.members {
                    member.with_data(|data| data.set_state(member, state, false));
                }
                Ok(None)
            },
            (Some(state), true) => {
                sync(Vec::new());
                self.opening_owed = true;
                for member in &self.members {
                    member.with_data(|data| data.set_state(member, state, true));
                }
                Ok(Some(true))
            },
            (None, _) => {
                let fences = self
                    .members
                    .iter()
                    .map(|member| (member.clone(), member.with_data(|data| data.disable(member))))
                    .collect();
                self.opening_owed = false;
                sync(fences);
                Ok(Some(false))
            },
        }
    }

    /// `member` entered the window. Returns whether the window's `Ime::Enabled` is pushed now:
    /// when its input method is enabled, the state is sent to the text input, and the input method
    /// does not have to reply anything, so a synthetic event carrying an empty state notifies the
    /// application about its presence. That event is the window's opening.
    pub fn enter(&mut self, member: &T) -> bool {
        if !self.members.contains(member) {
            self.members.push(member.clone());
        }
        let Some(state) = &self.state else { return false };
        member.with_data(|data| data.set_state(member, state, true));
        self.opening_owed = false;
        true
    }

    /// `member` left the window, or was destroyed. Returns whether it was a member.
    pub fn remove(&mut self, member: &T) -> bool {
        let Some(position) = self.members.iter().position(|t| t == member) else {
            return false;
        };
        self.members.remove(position);
        true
    }

    /// Whether the window's `Ime::Enabled` is pushed now, as the opening a request owed it.
    ///
    /// It is pushed once its text inputs are all ready: a request enabled the input method and
    /// owes the opening, the input method is still enabled, the window has a member, and no living
    /// member waits for a disable's fence. Every batch the compositor computed before it applied
    /// any member's latest disable has then arrived, and was dropped, so the opening follows all of
    /// them, whichever phase of the event loop the requests were made in.
    pub fn take_opening(&mut self) -> bool {
        if !self.opening_owed || self.state.is_none() {
            return false;
        }
        let mut ready = false;
        for member in self.members.iter().filter(|member| member.is_alive()) {
            if member.with_data(|data| data.fenced()) {
                return false;
            }
            ready = true;
        }
        if ready {
            self.opening_owed = false;
        }
        ready
    }
}
