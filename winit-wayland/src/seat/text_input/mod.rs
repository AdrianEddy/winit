use std::ops::Deref;

use dpi::{LogicalPosition, LogicalSize};
use sctk::globals::GlobalData;
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::wl_callback::{self, WlCallback};
use sctk::reexports::client::protocol::wl_display::WlDisplay;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Dispatch, Proxy, QueueHandle, delegate_dispatch};
use sctk::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_manager_v3::ZwpTextInputManagerV3;
use sctk::reexports::protocols::wp::text_input::zv3::client::zwp_text_input_v3::{
    ContentHint, ContentPurpose, Event as TextInputEvent, ZwpTextInputV3,
};
use tracing::warn;
use winit_core::event::{Ime, WindowEvent};
use winit_core::window::{
    ImeCapabilities, ImeHint, ImePurpose, ImeRequestData, ImeSurroundingText,
};

use crate::state::WinitState;

mod window;

pub use window::ImeWindow;

#[derive(Debug)]
pub struct TextInputState {
    text_input_manager: ZwpTextInputManagerV3,
}

impl TextInputState {
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        let text_input_manager = globals.bind(queue_handle, 1..=1, GlobalData)?;
        Ok(Self { text_input_manager })
    }
}

impl Deref for TextInputState {
    type Target = ZwpTextInputManagerV3;

    fn deref(&self) -> &Self::Target {
        &self.text_input_manager
    }
}

impl Dispatch<ZwpTextInputManagerV3, GlobalData, WinitState> for TextInputState {
    fn event(
        _state: &mut WinitState,
        _proxy: &ZwpTextInputManagerV3,
        _event: <ZwpTextInputManagerV3 as Proxy>::Event,
        _data: &GlobalData,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
    }
}

impl Dispatch<ZwpTextInputV3, TextInputData, WinitState> for TextInputState {
    fn event(
        state: &mut WinitState,
        text_input: &ZwpTextInputV3,
        event: <ZwpTextInputV3 as Proxy>::Event,
        data: &TextInputData,
        conn: &Connection,
        qhandle: &QueueHandle<WinitState>,
    ) {
        // A window's state is locked before the data of a text input, here as where a request
        // applies the window's state to its text inputs (`ImeWindow::request`).
        let windows = state.windows.get_mut();
        match event {
            TextInputEvent::Enter { surface } => {
                let window_id = crate::make_wid(&surface);
                data.inner.lock().unwrap().surface = Some(surface);

                let mut window = match windows.get(&window_id) {
                    Some(window) => window.lock().unwrap(),
                    None => return,
                };

                if window.text_input_entered(text_input) {
                    // The input method doesn't have to reply anything, so a synthetic event
                    // carrying an empty state notifies the application about its presence.
                    state.events_sink.push_window_event(WindowEvent::Ime(Ime::Enabled), window_id);
                }
            },
            TextInputEvent::Leave { surface } => {
                let fence = {
                    let mut text_input_data = data.inner.lock().unwrap();
                    text_input_data.surface = None;
                    // Always issue a disable.
                    text_input_data.disable(text_input)
                };
                ImeSync::send(&conn.display(), qhandle, vec![(text_input.clone(), fence)]);

                let window_id = crate::make_wid(&surface);

                // XXX this check is essential, because `leave` could have a
                // reference to nil surface...
                match windows.get(&window_id) {
                    Some(window) => window.lock().unwrap().text_input_left(text_input),
                    None => return,
                }

                state.events_sink.push_window_event(WindowEvent::Ime(Ime::Disabled), window_id);
                // The window may be owed its opening, which waited for this text input too.
                state.open_ime_windows();
            },
            TextInputEvent::PreeditString { text, cursor_begin, cursor_end } => {
                data.inner.lock().unwrap().preedit_string(text, cursor_begin, cursor_end)
            },
            TextInputEvent::CommitString { text } => data.inner.lock().unwrap().commit_string(text),
            TextInputEvent::DeleteSurroundingText { before_length, after_length } => {
                data.inner.lock().unwrap().delete_surrounding_text(before_length, after_length)
            },
            TextInputEvent::Done { serial } => {
                let window_id = data.inner.lock().unwrap().surface.as_ref().map(crate::make_wid);
                let window = window_id
                    .and_then(|window_id| windows.get(&window_id))
                    .map(|window| window.lock().unwrap());
                let mut text_input_data = data.inner.lock().unwrap();

                // A batch the compositor computed before it applied the latest disable belongs to
                // the input method state that disable ended.
                if !text_input_data.admit_done(serial) {
                    text_input_data.discard_pending();
                    return;
                }

                // The window this `done` delivers to, if any: none without a surface, and none for
                // a window whose IME is disabled, in case some IME sends events for it.
                let target = window_id.filter(|_| {
                    window.as_ref().is_none_or(|window| window.text_input_state().is_some())
                });

                // text-input-v3 resets the pending state at every `done`, so a `done` that delivers
                // to no window discards it rather than leaving it for the next `done`.
                match target {
                    Some(window_id) => {
                        for event in text_input_data.take_done_events() {
                            state.events_sink.push_window_event(WindowEvent::Ime(event), window_id);
                        }
                    },
                    None => text_input_data.discard_pending(),
                }
            },
            _ => {},
        }
    }
}

/// The `wl_display.sync` of one input method transition: an application's disable or enable of a
/// window's input method, or a text input's `leave`.
///
/// The compositor handles a connection's requests in order and sends a connection's events in
/// order, so the callback follows every event the compositor sent before it applied the requests
/// ahead of the sync: every batch a listed text input's compositor-side state produced before its
/// disable. The callback retires those fences — each only if no later disable replaced it — and
/// opens the windows whose text inputs are all ready (`ImeWindow::take_opening`).
#[derive(Debug)]
pub struct ImeSync {
    /// The text inputs the transition disabled, each with its fence.
    fences: Vec<(ZwpTextInputV3, u32)>,
}

impl ImeSync {
    /// Sends the sync of a transition that disabled the `fences`' text inputs (none for an
    /// enable).
    pub fn send(
        display: &WlDisplay,
        queue_handle: &QueueHandle<WinitState>,
        fences: Vec<(ZwpTextInputV3, u32)>,
    ) {
        display.sync(queue_handle, ImeSync { fences });
    }
}

impl Dispatch<WlCallback, ImeSync, WinitState> for TextInputState {
    fn event(
        state: &mut WinitState,
        _callback: &WlCallback,
        event: <WlCallback as Proxy>::Event,
        data: &ImeSync,
        _conn: &Connection,
        _qhandle: &QueueHandle<WinitState>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            retire(&data.fences);
            state.open_ime_windows();
        }
    }
}

/// Retires the fences a transition's sync lists: each listed text input that is still alive and
/// whose fence is still the listed one. A later disable's fence stays for its own sync.
fn retire<T: TextInputHandle>(fences: &[(T, u32)]) {
    for (text_input, fence) in fences {
        if text_input.is_alive() {
            text_input.with_data(|data| data.retire(*fence));
        }
    }
}

/// A text input as a window's input method holds it: its requests, and its data.
pub(crate) trait TextInputHandle: TextInputRequests + Clone + PartialEq {
    /// Runs `f` on the text input's data. The caller holds no other text input's data, and a
    /// window's state is locked before a text input's data.
    fn with_data<R>(&self, f: impl FnOnce(&mut TextInputDataInner) -> R) -> R;

    /// Whether the text input is still alive; a destroyed one is never retired by a sync.
    fn is_alive(&self) -> bool;
}

impl TextInputHandle for ZwpTextInputV3 {
    fn with_data<R>(&self, f: impl FnOnce(&mut TextInputDataInner) -> R) -> R {
        let data = self.data::<TextInputData>().expect("a text input carries its data");
        f(&mut data.inner.lock().unwrap())
    }

    fn is_alive(&self) -> bool {
        Proxy::is_alive(self)
    }
}

/// The requests that change a text input's state. winit issues them through
/// [`TextInputDataInner`], which counts every `commit`; the unit tests record them.
pub(crate) trait TextInputRequests {
    fn enable(&self);
    fn disable(&self);
    fn set_content_type(&self, hint: ContentHint, purpose: ContentPurpose);
    fn set_cursor_rectangle(&self, x: i32, y: i32, width: i32, height: i32);
    fn set_surrounding_text(&self, text: String, cursor: i32, anchor: i32);
    /// The `commit` request, not counted: only [`TextInputDataInner::commit`] sends it, and counts
    /// it. An uncounted commit would put the compositor's serials ahead of the count, and let a
    /// batch from before a disable through the fence.
    fn commit_uncounted(&self);
}

impl TextInputRequests for ZwpTextInputV3 {
    fn enable(&self) {
        ZwpTextInputV3::enable(self)
    }

    fn disable(&self) {
        ZwpTextInputV3::disable(self)
    }

    fn set_content_type(&self, hint: ContentHint, purpose: ContentPurpose) {
        ZwpTextInputV3::set_content_type(self, hint, purpose)
    }

    fn set_cursor_rectangle(&self, x: i32, y: i32, width: i32, height: i32) {
        ZwpTextInputV3::set_cursor_rectangle(self, x, y, width, height)
    }

    fn set_surrounding_text(&self, text: String, cursor: i32, anchor: i32) {
        ZwpTextInputV3::set_surrounding_text(self, text, cursor, anchor)
    }

    #[expect(
        clippy::disallowed_methods,
        reason = "the one forwarding of the request, counted by `TextInputDataInner::commit`"
    )]
    fn commit_uncounted(&self) {
        ZwpTextInputV3::commit(self)
    }
}

/// The Data associated with the text input.
#[derive(Default)]
pub struct TextInputData {
    inner: std::sync::Mutex<TextInputDataInner>,
}

pub struct TextInputDataInner {
    /// The `WlSurface` we're performing input to.
    surface: Option<WlSurface>,

    /// The commit to submit on `done`.
    pending_commit: Option<String>,

    /// The preedit to submit on `done`.
    pending_preedit: Option<Preedit>,

    /// The text around the cursor to delete on `done`
    pending_delete: Option<DeleteSurroundingText>,

    /// Last preedit empty.
    last_preedit_empty: bool,

    /// The number of `commit` requests issued on the text input, wrapping. text-input-v3: "The
    /// compositor must count the number of commit requests coming from each zwp_text_input_v3
    /// object and use the count as the serial in done events."
    commits: u32,

    /// The serial of the latest commit that disabled the text input, until a `done` at or past it
    /// shows that the compositor has sent every batch it computed before it applied that disable.
    fence: Option<u32>,
}

impl Default for TextInputDataInner {
    fn default() -> Self {
        Self {
            surface: None,
            pending_commit: None,
            pending_preedit: None,
            pending_delete: None,
            last_preedit_empty: true,
            commits: 0,
            fence: None,
        }
    }
}

impl TextInputDataInner {
    /// Applies the window's input method state to `text_input` atomically: the state is sent, after
    /// an `enable` when `send_enable`. A text input is disabled by [`Self::disable`] alone, whose
    /// fence the caller hands to a sync.
    fn set_state(
        &mut self,
        text_input: &impl TextInputRequests,
        state: &ClientState,
        send_enable: bool,
    ) {
        if send_enable {
            text_input.enable();
        }

        if let Some(content_type) = state.content_type() {
            text_input.set_content_type(content_type.hint, content_type.purpose);
        }

        if let Some((position, size)) = state.cursor_area() {
            let (x, y) = (position.x as i32, position.y as i32);
            let (width, height) = (size.width as i32, size.height as i32);
            // The same cursor can be applied on different seats.
            // It's the compositor's responsibility to make sure that any present popups don't
            // overlap.
            text_input.set_cursor_rectangle(x, y, width, height);
        }

        if let Some(surrounding) = state.surrounding_text() {
            text_input.set_surrounding_text(
                surrounding.text().into(),
                surrounding.cursor() as i32,
                surrounding.anchor() as i32,
            );
        }

        self.commit(text_input);
    }

    /// Disables `text_input`: `disable`, then its `commit`.
    ///
    /// The compositor resets the text input's state when it applies the disable, so every batch
    /// it computed before then belongs to the state the disable ended. The commit's serial is
    /// the fence that tells them apart: a `done` whose serial precedes it is one of them. The
    /// batch pending now is one of them too, and the preedit is gone with the state.
    ///
    /// Returns the fence. A sync must list it: only its callback, or a `done` at or past it,
    /// retires it, and a window does not open while one of its text inputs is fenced.
    #[must_use = "the fence is retired only by a sync that lists it"]
    fn disable(&mut self, text_input: &impl TextInputRequests) -> u32 {
        text_input.disable();
        let serial = self.commit(text_input);
        self.fence = Some(serial);
        self.discard_pending();
        self.last_preedit_empty = true;
        serial
    }

    /// Retires `fence` at its sync's callback, unless a later disable replaced it: every batch the
    /// compositor computed before it applied that disable has arrived, and was dropped.
    fn retire(&mut self, fence: u32) {
        if self.fence == Some(fence) {
            self.fence = None;
        }
    }

    /// Whether the text input waits for the retirement of a disable's fence.
    fn fenced(&self) -> bool {
        self.fence.is_some()
    }

    /// Issues `commit` on `text_input` and counts it, returning its serial: the serial of the
    /// first `done` the compositor sends after it applied the commit.
    fn commit(&mut self, text_input: &impl TextInputRequests) -> u32 {
        text_input.commit_uncounted();
        self.commits = self.commits.wrapping_add(1);
        self.commits
    }

    /// Whether a `done` carrying `serial` delivers its batch: not when the compositor computed the
    /// batch before it applied the latest disable, however late the `done` arrives. A `done` at or
    /// past the fence retires it: the compositor sends its events in order, so every older batch
    /// has arrived already.
    ///
    /// text-input-v3 asks a client to apply a batch whose serial does not match its count of
    /// commits "as normal"; a batch from before a disable is the exception, as the state it was
    /// computed for no longer exists. Serials are compared wrapping, which is exact while fewer
    /// than 2^31 commits separate a batch from the fence.
    fn admit_done(&mut self, serial: u32) -> bool {
        let Some(fence) = self.fence else { return true };
        if (serial.wrapping_sub(fence) as i32) < 0 {
            return false;
        }
        self.fence = None;
        true
    }
    /// A `preedit_string` event: the batch's new preedit.
    fn preedit_string(&mut self, text: Option<String>, cursor_begin: i32, cursor_end: i32) {
        self.pending_preedit = Some(Preedit::from_event(text, cursor_begin, cursor_end));
    }

    /// A `commit_string` event: the batch's commit. text-input-v3 double-buffers it apart from
    /// the preedit, and `done` inserts the commit before the new preedit, so a preedit the batch
    /// set before its commit is kept: an input method commits a conversion and starts the next
    /// composition in one batch.
    fn commit_string(&mut self, text: Option<String>) {
        self.pending_commit = text;
    }

    /// A `delete_surrounding_text` event: the batch's deletion.
    fn delete_surrounding_text(&mut self, before_length: u32, after_length: u32) {
        self.pending_delete = Some(DeleteSurroundingText {
            before: before_length as usize,
            after: after_length as usize,
        });
    }

    /// The `Ime` events a `done` delivers, taking the pending state they apply.
    fn take_done_events(&mut self) -> impl Iterator<Item = Ime> + use<> {
        // The events are sent to the user separately, so
        // CAUTION: events must always arrive in the order compatible with the application
        // order specified by the text-input-v3 protocol:
        //
        // As of version 1:
        // 1. Replace existing preedit string with the cursor.
        // 2. Delete requested surrounding text.
        // 3. Insert commit string with the cursor at its end.
        // 4. Calculate surrounding text to send.
        // 5. Insert new preedit text in cursor position.
        // 6. Place cursor inside preedit text.

        let delete = self.pending_delete.take().map(|DeleteSurroundingText { before, after }| {
            Ime::DeleteSurrounding { before_bytes: before, after_bytes: after }
        });

        // Send `Commit`.
        let commit_or_clear = if let Some(text) = self.pending_commit.take() {
            self.last_preedit_empty = true;
            Some(Ime::Commit(text))
        } else if self.pending_preedit.is_none() && !self.last_preedit_empty {
            self.last_preedit_empty = true;
            Some(Ime::Preedit(String::new(), None))
        } else {
            None
        };

        // Send preedit.
        let preedit = self.pending_preedit.take().and_then(|preedit| {
            // An empty preedit is a clear: none is sent when the preedit is already empty, and the
            // next `done` with nothing pending sends none either.
            if preedit.text.is_empty() && self.last_preedit_empty {
                return None;
            }
            let cursor_range = preedit.cursor_begin.map(|b| (b, preedit.cursor_end.unwrap_or(b)));

            self.last_preedit_empty = preedit.text.is_empty();
            Some(Ime::Preedit(preedit.text, cursor_range))
        });

        // A batch that only deletes, while no preedit is shown, ends with an empty preedit, as a
        // batch that clears a shown preedit does: every `done` that delivers anything then ends
        // with a `Commit` or a `Preedit`, so the application can tell where one input method
        // transaction ends and the next begins, however many are delivered together.
        let close = (delete.is_some() && commit_or_clear.is_none() && preedit.is_none())
            .then(|| Ime::Preedit(String::new(), None));

        [delete, commit_or_clear, preedit, close].into_iter().flatten()
    }

    /// Resets the pending state of a `done` that delivers to no window. text-input-v3 says each
    /// item "must be applied and reset to initial on the next zwp_text_input_v3.done event".
    /// Nothing was delivered, so `last_preedit_empty` keeps what the application was last told.
    fn discard_pending(&mut self) {
        self.pending_commit = None;
        self.pending_preedit = None;
        self.pending_delete = None;
    }
}

/// The state of the preedit.
#[derive(Clone)]
struct Preedit {
    text: String,
    cursor_begin: Option<usize>,
    cursor_end: Option<usize>,
}

impl Preedit {
    /// The pending preedit a `preedit_string` event sets. An empty preedit carries no cursor: the
    /// protocol's reset, `preedit_string(NULL, 0, 0)`, which input methods send to clear the
    /// preedit, reaches the application as `Ime::Preedit("", None)`, the clear the other backends
    /// send, not as an empty preedit with a cursor at `(0, 0)`, which the Windows and X11 backends
    /// send for a composition that is still open.
    fn from_event(text: Option<String>, cursor_begin: i32, cursor_end: i32) -> Self {
        let text = text.unwrap_or_default();
        if text.is_empty() {
            return Preedit { text, cursor_begin: None, cursor_end: None };
        }
        let cursor_begin = usize::try_from(cursor_begin)
            .ok()
            .and_then(|idx| text.is_char_boundary(idx).then_some(idx));
        let cursor_end = usize::try_from(cursor_end)
            .ok()
            .and_then(|idx| text.is_char_boundary(idx).then_some(idx));

        Preedit { text, cursor_begin, cursor_end }
    }
}

/// The delete request
#[derive(Clone)]
struct DeleteSurroundingText {
    /// Bytes before cursor
    before: usize,
    /// Bytes after cursor
    after: usize,
}

/// State change requested by the application.
///
/// This is a version that uses text_input abstractions translated from the ones used in
/// winit::core::window::ImeStateChange.
///
/// Fields that are initially set to None are unsupported capabilities
/// and trying to set them raises an error.
#[derive(Debug, PartialEq, Clone)]
pub struct ClientState {
    capabilities: ImeCapabilities,
    content_type: ContentType,
    /// The IME cursor area which should not be covered by the input method popup.
    cursor_area: (LogicalPosition<u32>, LogicalSize<u32>),

    /// The `ImeSurroundingText` struct is based on the Wayland model.
    /// When this changes, another struct might be needed.
    surrounding_text: ImeSurroundingText,
}

impl ClientState {
    pub fn new(
        capabilities: ImeCapabilities,
        request_data: ImeRequestData,
        scale_factor: f64,
    ) -> Self {
        let mut this = Self {
            capabilities,
            content_type: Default::default(),
            cursor_area: Default::default(),
            surrounding_text: ImeSurroundingText::new(String::new(), 0, 0).unwrap(),
        };

        let unsupported_flags = capabilities
            .without_hint_and_purpose()
            .without_cursor_area()
            .without_surrounding_text();

        if unsupported_flags != ImeCapabilities::new() {
            warn!(
                "Backend doesn't support all requested IME capabilities: {:?}.\n Ignoring.",
                unsupported_flags
            );
        }

        this.update(request_data, scale_factor);
        this
    }

    pub fn capabilities(&self) -> ImeCapabilities {
        self.capabilities
    }

    /// Updates the fields of the state which are present in update_fields.
    pub fn update(&mut self, request_data: ImeRequestData, scale_factor: f64) {
        if let Some((hint, purpose)) =
            request_data.hint_and_purpose.filter(|_| self.capabilities.hint_and_purpose())
        {
            self.content_type = (hint, purpose).into();
        }

        if let Some((position, size)) = request_data.cursor_area {
            if self.capabilities.cursor_area() {
                let position: LogicalPosition<u32> = position.to_logical(scale_factor);
                let size: LogicalSize<u32> = size.to_logical(scale_factor);
                self.cursor_area = (position, size);
            } else {
                warn!("discarding IME cursor area update without capability enabled.");
            }
        }

        if let Some(surrounding) = request_data.surrounding_text {
            if self.capabilities.surrounding_text() {
                self.surrounding_text = surrounding;
            } else {
                warn!("discarding IME surrounding text update without capability enabled.");
            }
        }
    }

    pub fn content_type(&self) -> Option<ContentType> {
        self.capabilities.hint_and_purpose().then_some(self.content_type)
    }

    pub fn cursor_area(&self) -> Option<(LogicalPosition<u32>, LogicalSize<u32>)> {
        self.capabilities.cursor_area().then_some(self.cursor_area)
    }

    pub fn surrounding_text(&self) -> Option<&ImeSurroundingText> {
        self.capabilities.surrounding_text().then_some(&self.surrounding_text)
    }
}

/// Arguments to content_type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentType {
    /// Text input hint.
    hint: ContentHint,
    /// Text input purpose.
    purpose: ContentPurpose,
}

/// The two options influence each other, so they must be converted together.
impl From<(ImeHint, ImePurpose)> for ContentType {
    fn from((hint, purpose): (ImeHint, ImePurpose)) -> Self {
        let purpose = match purpose {
            ImePurpose::Password => ContentPurpose::Password,
            ImePurpose::Terminal => ContentPurpose::Terminal,
            ImePurpose::Phone => ContentPurpose::Phone,
            ImePurpose::Number => ContentPurpose::Number,
            ImePurpose::Url => ContentPurpose::Url,
            ImePurpose::Email => ContentPurpose::Email,
            ImePurpose::Pin => ContentPurpose::Pin,
            ImePurpose::Date => ContentPurpose::Date,
            ImePurpose::Time => ContentPurpose::Time,
            ImePurpose::DateTime => ContentPurpose::Datetime,
            _ => ContentPurpose::Normal,
        };

        let base_hint = match purpose {
            // Before the hint API was introduced, password  purpose guaranteed the
            // sensitive hint. Keep this behaviour for the sake of backwards compatibility.
            ContentPurpose::Password | ContentPurpose::Pin => ContentHint::SensitiveData,
            _ => ContentHint::None,
        };

        let mut new_hint = base_hint;
        if hint.contains(ImeHint::COMPLETION) {
            new_hint |= ContentHint::Completion;
        }
        if hint.contains(ImeHint::SPELLCHECK) {
            new_hint |= ContentHint::Spellcheck;
        }
        if hint.contains(ImeHint::AUTO_CAPITALIZATION) {
            new_hint |= ContentHint::AutoCapitalization;
        }
        if hint.contains(ImeHint::LOWERCASE) {
            new_hint |= ContentHint::Lowercase;
        }
        if hint.contains(ImeHint::UPPERCASE) {
            new_hint |= ContentHint::Uppercase;
        }
        if hint.contains(ImeHint::TITLECASE) {
            new_hint |= ContentHint::Titlecase;
        }
        if hint.contains(ImeHint::HIDDEN_TEXT) {
            new_hint |= ContentHint::HiddenText;
        }
        if hint.contains(ImeHint::SENSITIVE_DATA) {
            new_hint |= ContentHint::SensitiveData;
        }
        if hint.contains(ImeHint::LATIN) {
            new_hint |= ContentHint::Latin;
        }
        if hint.contains(ImeHint::MULTILINE) {
            new_hint |= ContentHint::Multiline;
        }

        Self { hint: new_hint, purpose }
    }
}

impl Default for ContentType {
    fn default() -> Self {
        ContentType { purpose: ContentPurpose::Normal, hint: ContentHint::None }
    }
}

delegate_dispatch!(WinitState: [ZwpTextInputManagerV3: GlobalData] => TextInputState);
delegate_dispatch!(WinitState: [ZwpTextInputV3: TextInputData] => TextInputState);
delegate_dispatch!(WinitState: [WlCallback: ImeSync] => TextInputState);

#[cfg(test)]
mod tests;
