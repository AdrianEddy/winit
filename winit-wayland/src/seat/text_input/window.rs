//! The input method of one window, over the text inputs entered on it.

use winit_core::event::Ime;
use winit_core::window::{ImeRequest, ImeRequestError};

use super::{ClientState, TextInputHandle};

/// The input method of one window: the state the application requested, and the text inputs
/// entered on the window — its members, one for each seat whose text-input focus is on it.
///
/// Only one input method may be active per (seat, surface), but several seats can focus one
/// surface. There is no API to set a per-seat input method state, so the members share one state.
///
/// The window shows one composition, but several members can feed it, so the composition has one
/// owner: the member whose delivered batch left a preedit. Each change of the member that feeds
/// the window ends the owner's composition first, so the window's events never join the text of
/// two seats' input methods into one composition. The window's `Ime::Enabled` and `Ime::Disabled`
/// follow its first and its last member, so one seat's `leave` never ends what another seat's
/// input method is composing.
///
/// Both events are pushed from the compositor's dispatch, never at a request: an enable's
/// `Ime::Enabled` once every batch from before the window's latest disables has arrived
/// (`take_opening`), and a disable's `Ime::Disabled` at its own sync's callback
/// (`take_disabled`), behind every batch the compositor sent before it applied the disable.
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

    /// The member whose composition the window shows.
    composing: Option<T>,

    /// The number of `Ime::Enabled` and `Ime::Disabled` events pushed for the window: a disable's
    /// `Ime::Disabled` is due at its callback only while none was pushed since the request.
    events_pushed: u64,
}

impl<T> Default for ImeWindow<T> {
    fn default() -> Self {
        Self {
            state: None,
            members: Vec::new(),
            opening_owed: false,
            composing: None,
            events_pushed: 0,
        }
    }
}

/// The `Ime::Disabled` a disable request owes its window, pushed at the request's sync callback
/// (`ImeWindow::take_disabled`): the window's count of pushed `Ime::Enabled` and `Ime::Disabled`
/// events at the request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OwedDisabled(u64);

/// What a disable request's sync asks of its window at the callback: whether the `Ime::Disabled`
/// the request owed it is pushed now.
pub trait TakeDisabled {
    fn take_disabled(&mut self, owed: OwedDisabled) -> bool;
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
    /// enables. No event is pushed here. An enable owes the window its opening, which the
    /// compositor's dispatch pushes, behind every event the compositor sent before it applied the
    /// window's latest disables (`take_opening`). A disable owes the window its `Ime::Disabled`,
    /// which the callback of the disable's own sync pushes (`take_disabled`): `sync` carries it.
    ///
    /// Returns `None` if the input method state did not change, or `Some(allowed)` otherwise.
    pub fn request(
        &mut self,
        request: ImeRequest,
        scale_factor: f64,
        sync: impl FnOnce(Vec<(T, u32)>, Option<OwedDisabled>),
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
                sync(Vec::new(), None);
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
                self.composing = None;
                sync(fences, Some(OwedDisabled(self.events_pushed)));
                Ok(Some(false))
            },
        }
    }

    /// `member` entered the window. Returns whether the window's `Ime::Enabled` is pushed now:
    /// when its input method is enabled the state is sent to the text input, and the first member
    /// opens the window. The input method does not have to reply anything, so a synthetic event
    /// carrying an empty state notifies the application about its presence; a window another
    /// member holds open already has its `Ime::Enabled`.
    pub fn enter(&mut self, member: &T) -> bool {
        let first = self.members.is_empty();
        if !self.members.contains(member) {
            self.members.push(member.clone());
        }
        let Some(state) = &self.state else { return false };
        member.with_data(|data| data.set_state(member, state, true));
        if !first {
            return false;
        }
        self.opening_owed = false;
        self.events_pushed += 1;
        true
    }

    /// `member` left the window, by its `leave` or its destruction: the one removal of a member,
    /// a no-op for a text input that is not one. Returns the events it pushes.
    ///
    /// The member's pending batch is discarded. If it owned the composition and members remain,
    /// the composition ends with `Ime::Preedit("", None)`, as text-input-v3 asks of a client at
    /// `leave` ("The client should reset any preedit string previously set"); the remaining
    /// members' readiness is untouched. If it was the last member, the window's `Ime::Disabled`
    /// ends everything, with no preedit before it.
    pub fn remove(&mut self, member: &T) -> Vec<Ime> {
        let Some(position) = self.members.iter().position(|t| t == member) else {
            return Vec::new();
        };
        self.members.remove(position);
        member.with_data(|data| data.discard_pending());

        let mut events = Vec::new();
        if self.composing.as_ref() == Some(member) {
            self.composing = None;
            if !self.members.is_empty() {
                events.push(Ime::Preedit(String::new(), None));
            }
        }
        if self.members.is_empty() {
            events.push(Ime::Disabled);
            self.events_pushed += 1;
        }
        events
    }

    /// A `done` of `member`, whose text-input focus is on the window: the events it delivers.
    ///
    /// A batch from before the member's latest disable is dropped (`admit_done`), and so is any
    /// batch while the window's input method is disabled. A batch from the composition's owner,
    /// or while no member owns it, is delivered. Another member's batch without input of its own
    /// is swallowed: it changes only what that member's input method shows, which the window does
    /// not show. Another member's batch with input takes the composition over: the owner's
    /// composition ends first, in the window's events with `Ime::Preedit("", None)` and natively,
    /// by a reset of the owner's text input alone — `disable` and its fence, the reset's own
    /// `sync` listing it, then `enable` with the window's state — so that its input method no
    /// longer holds the ended composition, and the batches it computed before the reset are
    /// dropped. The reset owes no opening. The batch is delivered after that.
    ///
    /// A delivered batch whose last event is a non-empty preedit makes its member the owner, and
    /// the owner's batch that ends with an empty preedit or a commit ends its ownership.
    pub fn done(&mut self, member: &T, serial: u32, sync: impl FnOnce(Vec<(T, u32)>)) -> Vec<Ime> {
        let state = self.state.as_ref();
        let content = member.with_data(|data| {
            if !data.admit_done(serial) || state.is_none() {
                data.discard_pending();
                return None;
            }
            Some(data.has_content())
        });
        let (Some(content), Some(state)) = (content, state) else { return Vec::new() };

        let mut events = Vec::new();
        if let Some(owner) = self.composing.clone().filter(|owner| owner != member) {
            if !content {
                member.with_data(|data| {
                    data.discard_pending();
                    data.last_preedit_empty = true;
                });
                return events;
            }
            events.push(Ime::Preedit(String::new(), None));
            self.composing = None;
            let fence = owner.with_data(|data| data.disable(&owner));
            sync(vec![(owner.clone(), fence)]);
            owner.with_data(|data| data.set_state(&owner, state, true));
        }

        events.extend(member.with_data(|data| data.take_done_events().collect::<Vec<_>>()));
        match events.last() {
            Some(Ime::Preedit(text, _)) if !text.is_empty() => {
                self.composing = Some(member.clone());
            },
            Some(Ime::Preedit(..) | Ime::Commit(_)) if self.composing.as_ref() == Some(member) => {
                self.composing = None;
            },
            _ => (),
        }
        events
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
            self.events_pushed += 1;
        }
        ready
    }
}

impl<T> TakeDisabled for ImeWindow<T> {
    /// Whether the window's `Ime::Disabled` that a disable request owed (`owed`, from `request`)
    /// is pushed now, at that request's sync callback, ahead of any opening the callback pushes.
    ///
    /// It is due unless the window pushed an `Ime::Enabled` or an `Ime::Disabled` since the
    /// request: the last member's removal already told the application, or a member's `enter`
    /// opened the window again, and an `Ime::Disabled` after that `Ime::Enabled` would contradict
    /// an input method that is enabled. So a window's events alternate, and two disables before the
    /// first callback push one.
    fn take_disabled(&mut self, owed: OwedDisabled) -> bool {
        if owed.0 != self.events_pushed {
            return false;
        }
        self.events_pushed += 1;
        true
    }
}
