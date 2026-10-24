//! Types related to data transfer (drag-and-drop and clipboard) on X11.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::os::raw::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};
use std::{fmt, io};

use winit_core::data_transfer::{
    DataTransfer, DataTransferId, DataTransferSend, SendData, TransferType, TypeHint, TypedData,
};
use winit_core::error::TransferError;
use winit_core::event::WindowEvent;
use winit_core::event_loop::AsyncRequestSerial;
use winit_core::window::WindowId;
use x11_dl::xfixes::XFixesSelectionNotifyEvent;
use x11_dl::xlib::{self, XEvent, XPropertyEvent, XSelectionEvent, XSelectionRequestEvent};
use x11rb::CURRENT_TIME;
use x11rb::connection::{Connection as _, RequestConnection as _};
use x11rb::errors::ReplyError;
use x11rb::protocol::xfixes::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{self, ConnectionExt};

use crate::atoms::*;
use crate::event_loop::{CookieResultExt, X11Error};
use crate::xdisplay::XConnection;
use crate::{XWindow, util};

/// The most data stored in a property with one request, whatever the server accepts.
const MAX_PROPERTY_CHUNK_BYTES: usize = 256 * 1024;

/// The largest selection reply stored in one request, which is also the chunk size of an
/// incremental (INCR) transfer: the server's maximum request size, less room for the request's
/// header, capped at [`MAX_PROPERTY_CHUNK_BYTES`].
///
/// ICCCM: INCR is for data too large to store in the property with one request. The core
/// protocol guarantees a maximum request size of at least 4096 bytes.
pub(crate) fn incr_threshold(maximum_request_bytes: usize) -> usize {
    maximum_request_bytes.saturating_sub(100).min(MAX_PROPERTY_CHUNK_BYTES)
}

/// The value of the `INCR` property that starts an incremental transfer of `len` bytes.
///
/// ICCCM: it is a single 32-bit integer, a lower bound on the number of bytes of the selection.
pub(crate) fn incr_announcement(len: usize) -> [u32; 1] {
    [u32::try_from(len).unwrap_or(u32::MAX)]
}

/// TODO: this is copied from wayland. Is this the right approach for X11 as well?
fn encode_uri_list<I>(uri_list: I) -> Vec<u8>
where
    I: IntoIterator,
    I::Item: AsRef<OsStr>,
{
    let mut out = Vec::new();

    for uri in uri_list {
        out.extend_from_slice(OsStr::new(&uri).as_encoded_bytes());
        out.extend_from_slice(b"\r\n");
    }

    out
}

#[derive(Debug, Clone, Copy)]
pub enum DndState {
    Accepted,
    Rejected,
}

/// The data of a selection, received for one fetch.
///
/// When the transfer failed, every read returns its error (and [`TypedData::try_read`] returns
/// `None`): [`io::ErrorKind::Unsupported`] when the owner refused the conversion,
/// [`io::ErrorKind::InvalidData`] when the reply could not be read,
/// [`io::ErrorKind::ConnectionAborted`] when the owner changed or went away, or the drag ended,
/// before the transfer completed, and [`io::ErrorKind::Other`] when the conversion could not be
/// requested.
#[derive(Debug)]
pub struct SelectionReader {
    type_: SelectionType,
    result: Result<Vec<u8>, Arc<io::Error>>,
}

impl SelectionReader {
    fn data(&self) -> io::Result<&[u8]> {
        self.result.as_deref().map_err(|err| io::Error::new(err.kind(), Arc::clone(err)))
    }
}

impl TypedData for SelectionReader {
    fn try_read(&self) -> Option<Box<dyn io::BufRead>> {
        let data = self.result.as_ref().ok()?;
        Some(Box::new(io::Cursor::new(data.clone())))
    }

    fn try_as_bytes(&self) -> io::Result<Vec<u8>> {
        self.data().map(ToOwned::to_owned)
    }

    fn type_(&self) -> &dyn TransferType {
        &self.type_
    }

    fn try_as_string(&self) -> io::Result<String> {
        fn invalid_data<E>(err: E) -> io::Error
        where
            E: Into<Box<dyn std::error::Error + Send + Sync>>,
        {
            io::Error::new(io::ErrorKind::InvalidData, err)
        }

        fn decode_utf16_bytes(bytes: &[u8]) -> io::Result<String> {
            let utf16 = bytes
                .chunks_exact(2)
                .map(|chunk| {
                    let bytes: &[u8; 2] = chunk.try_into().unwrap();
                    u16::from_ne_bytes(*bytes)
                })
                .collect::<Vec<_>>();
            String::from_utf16(&utf16).map_err(invalid_data)
        }

        let data = self.data()?;
        match self.type_.hint() {
            Some(TypeHint::Plaintext) | Some(TypeHint::Html) => std::str::from_utf8(data)
                .map(|str| str.to_owned())
                .map_err(invalid_data)
                .or_else(|_| decode_utf16_bytes(data)),
            Some(TypeHint::UriList) => String::from_utf8(data.to_vec()).map_err(invalid_data),
            _ => Err(io::ErrorKind::InvalidData.into()),
        }
    }

    fn try_as_uris(&self) -> io::Result<Vec<String>> {
        if self.type_().hint() != Some(TypeHint::UriList) {
            return Err(io::ErrorKind::InvalidData.into());
        }

        Ok(self
            .try_as_string()?
            .split(['\n', '\r'])
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }
}

#[derive(Debug)]
pub struct DragState {
    // Populated by XdndEnter event handler
    pub version: c_long,
    pub transfer_id: DataTransferId,
    pub types: Arc<[SelectionType]>,
    // Populated by Xdnd* event handlers
    pub source_window: xproto::Window,
    // Populated by Xdnd* event handlers
    pub target_window: xproto::Window,
    /// The fetches of the dragged data, populated by `fetch_data_transfer`
    pub(crate) fetches: FetchQueue,
    pub finished: Option<(XWindow, XWindow)>,
    /// Whether the drag operation is accepted (or `None` if the user never indicated that it's
    /// accepted or rejected)
    // Populated by `Window::accept_drag`/`Window::reject_drag`.
    pub accepted: bool,
}

/// Create a new [`DataTransferId`] from a global counter
fn generate_transfer_id() -> DataTransferId {
    static DATA_TRANSFER_ID: AtomicI64 = AtomicI64::new(0);
    DataTransferId::from_raw(DATA_TRANSFER_ID.fetch_add(1, Ordering::Relaxed))
}

impl Default for DragState {
    fn default() -> Self {
        Self {
            version: Default::default(),
            transfer_id: generate_transfer_id(),
            types: Default::default(),
            source_window: Default::default(),
            target_window: Default::default(),
            fetches: Default::default(),
            finished: None,
            accepted: Default::default(),
        }
    }
}

impl DragState {
    /// Complete the first fetch of the dragged data.
    pub(crate) fn complete_fetch(
        &mut self,
        result: io::Result<Vec<u8>>,
    ) -> Option<FinishedDataTransfer> {
        self.fetches.complete_front(result)
    }

    /// The windows to send `XdndFinished` between, once the data was dropped and no fetch is
    /// left: XDND asks the target to send it when it is done with the data.
    pub(crate) fn drop_to_finish(&self) -> Option<(XWindow, XWindow)> {
        self.finished.filter(|_| self.fetches.is_empty())
    }
}

/// There are three standard selections defined in the ICCCM: PRIMARY, SECONDARY, and CLIPBOARD.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum ClipboardSelectionType {
    /// Primary selection, often used for the currently selected text
    Primary,
    /// Secondary selection, defined by the ICCCM but unused in practice, see
    /// <https://www.freedesktop.org/wiki/Specifications/ClipboardsWiki/>.
    Secondary,
    /// Clipboard, used for Ctrl+C Ctrl+V pasting
    Clipboard,
}

impl ClipboardSelectionType {
    pub const COUNT: usize = 3;

    /// An atom representing that selection
    #[must_use]
    pub fn selection_atom(self, atoms: &crate::atoms::Atoms) -> xproto::Atom {
        match self {
            Self::Primary => xproto::AtomEnum::PRIMARY.into(),
            Self::Secondary => xproto::AtomEnum::SECONDARY.into(),
            Self::Clipboard => atoms[CLIPBOARD],
        }
    }

    /// Try to convert an atom representing a selection back into a clipboard type
    #[must_use]
    pub fn from_atom(atoms: &crate::atoms::Atoms, atom: xproto::Atom) -> Option<Self> {
        match atom {
            _ if Self::Primary.selection_atom(atoms) == atom => Some(Self::Primary),
            _ if Self::Secondary.selection_atom(atoms) == atom => Some(Self::Secondary),
            _ if Self::Clipboard.selection_atom(atoms) == atom => Some(Self::Clipboard),
            _ => None,
        }
    }

    /// Iterate through all clipboards
    pub fn iter() -> impl Iterator<Item = Self> {
        [Self::Primary, Self::Secondary, Self::Clipboard].into_iter()
    }
}

/// State for the INCR clipboard protocol. The protocol works as follows:
/// - Paster calls convert_selection
/// - Copier receives a SelectionNotify event and returns empty data and a type of INCR
/// - Paster deletes the property where the empty data was sent
/// - Copier listens for the delete property message and then puts the first chunk of actual data
/// - Paster listens for the new property message and grabs the data, then deletes the property
///   again
/// - Copier puts then next chunk of data
/// - etc.
/// - When the copier puts an empty chunk of data, the sequence is over
///
/// This is the state of the copier; the paster's is [`IncrReceive`].
#[derive(Debug)]
struct IncrState {
    /// The bytes still to send
    data: Vec<u8>,
    /// The place where the INCR protocol takes place (is modified with each chunk) e.g.
    /// CLIPBOARD or META_SELECTION (not necessarily the same as the selection)
    property: xproto::Atom,
    /// The data type transmitted e.g. UTF8_STRING
    ty: SelectionType,
    clipboard: ClipboardSelectionType,
}

#[derive(Debug)]
pub struct ClipboardState {
    /// Data for each [`ClipboardSelection`] if held by this application (data is copied from the
    /// application and sent to others)
    owned_data: Option<Box<dyn DataTransferSend>>,
    /// ID for the current clipboard (each time it is changed this is updated; multiple pastes do
    /// not change this)
    transfer_serial: DataTransferId,
    /// The clipboard notify event serial, used for deduplicating events
    x11_serial: u64,
    /// Cached types available for pasting
    types: Arc<[SelectionType]>,
    /// The outstanding conversion to `TARGETS`, which lists the types available for pasting.
    targets_request: Option<Request>,
    /// The fetches of the selection's data, the first one being in progress
    fetches: FetchQueue,
    /// The sequence number of our latest `SetSelectionOwner` request, which published
    /// `owned_data`.
    owner_sequence: Option<u64>,
}

impl Default for ClipboardState {
    fn default() -> Self {
        Self {
            owned_data: Default::default(),
            transfer_serial: generate_transfer_id(),
            x11_serial: u64::MAX,
            types: Default::default(),
            targets_request: None,
            fetches: Default::default(),
            owner_sequence: None,
        }
    }
}

/// A conversion requested from a selection owner and not answered yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Request {
    /// The property on the requestor window that the owner was asked to store its reply in.
    /// Outstanding conversions never share a property, so the property of a reply names the
    /// request it answers.
    property: xproto::Atom,
    /// The sequence number of the `ConvertSelection` request.
    sequence: u64,
}

impl Request {
    /// Whether a `SelectionNotify` with `property` and `target`, whose sequence number is
    /// `event_serial`, answers this request, a conversion to a target that `expected_target`
    /// accepts.
    ///
    /// A reply that stores the data names the request by its property alone: no other
    /// outstanding conversion uses it, and some owners report another target than the one asked
    /// for (the type `ATOM` for `TARGETS`, for instance).
    ///
    /// ICCCM: a refusal carries the property `None`, so it names no request. It answers this one
    /// if it refuses its target and was generated after the server processed this request, which
    /// its sequence number tells: an event carries the sequence number of the last request the
    /// server had processed for this client. The low 32 bits are compared with wrapping, which is
    /// exact for requests less than 2^31 apart.
    fn answered_by(
        &self,
        expected_target: impl Fn(xproto::Atom) -> bool,
        property: xproto::Atom,
        target: xproto::Atom,
        event_serial: u64,
    ) -> bool {
        if property == x11rb::NONE {
            expected_target(target)
                && (event_serial as u32).wrapping_sub(self.sequence as u32) as i32 >= 0
        } else {
            property == self.property
        }
    }
}

/// A fetch requested with `fetch_data_transfer`, until its `DataTransferReceived`.
#[derive(Debug)]
pub(crate) struct PendingFetch {
    /// The data transfer the fetch was requested from.
    id: DataTransferId,
    serial: AsyncRequestSerial,
    ty: SelectionType,
    /// The window to deliver the data to. The clipboard is not tied to a window, so it is chosen
    /// when the fetch is requested.
    window: WindowId,
    /// The conversion of the fetch, once requested; only the first fetch of a queue has one.
    request: Option<Request>,
    /// Whether the data is text in ISO Latin-1, the encoding of the `STRING` target (ICCCM),
    /// which is converted to the UTF-8 of [`TypeHint::Plaintext`].
    latin1: bool,
}

impl PendingFetch {
    fn finish(self, result: io::Result<Vec<u8>>) -> FinishedDataTransfer {
        let result = if self.latin1 { result.map(|data| latin1_to_utf8(&data)) } else { result };
        let value = Arc::new(SelectionReader { type_: self.ty, result: result.map_err(Arc::new) });
        FinishedDataTransfer { id: self.id, serial: self.serial, value, window: self.window }
    }
}

/// ISO Latin-1 text, the encoding of the `STRING` target (ICCCM), as UTF-8.
pub(crate) fn latin1_to_utf8(latin1: &[u8]) -> Vec<u8> {
    latin1.iter().map(|&byte| char::from(byte)).collect::<String>().into_bytes()
}

/// Text as ISO Latin-1, the encoding of the `STRING` target (ICCCM), with `?` for the characters
/// that Latin-1 cannot represent.
pub(crate) fn utf8_to_latin1(text: &str) -> Vec<u8> {
    text.chars().map(|char| u8::try_from(char).unwrap_or(b'?')).collect()
}

/// The type to read for `hint` among `types`, the types on offer: the first one with that hint,
/// except that plain text is read from the UTF-8 targets rather than from `text/plain`, and from
/// `text/plain` rather than from `STRING` (ICCCM: ISO Latin-1), whatever the order they are
/// offered in.
pub(crate) fn preferred_type<'a>(
    types: &'a [SelectionType],
    hint: TypeHint,
    atoms: &Atoms,
) -> Option<&'a SelectionType> {
    most_preferred(types.iter().filter(|ty| ty.hint() == Some(hint)), hint, atoms)
}

/// The type to read for `hint` among `candidates`, types offered for it, as [`preferred_type`]
/// chooses.
fn most_preferred<'a>(
    candidates: impl Iterator<Item = &'a SelectionType>,
    hint: TypeHint,
    atoms: &Atoms,
) -> Option<&'a SelectionType> {
    let text = [atoms[UTF8_STRING], atoms[TextPlainCharsetUtf8], atoms[TextPlain], atoms[STRING]];
    let rank = |ty: &SelectionType| match hint {
        TypeHint::Plaintext => {
            text.iter().position(|&atom| atom == ty.atom()).unwrap_or(text.len())
        },
        _ => 0,
    };
    candidates.min_by_key(|ty| rank(ty))
}

/// The fetches of one selection, completed in the order they were requested: only the first one
/// has a conversion outstanding.
///
/// Every fetch ends in exactly one [`FinishedDataTransfer`]: with its data, or with an error when
/// its conversion is refused, fails, cannot be requested or is abandoned.
#[derive(Debug, Default)]
pub(crate) struct FetchQueue {
    fetches: VecDeque<PendingFetch>,
}

impl FetchQueue {
    fn push(&mut self, fetch: PendingFetch) {
        self.fetches.push_back(fetch);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.fetches.is_empty()
    }

    /// The type and the outstanding conversion of the first fetch.
    fn front_request(&self) -> Option<(&SelectionType, Request)> {
        let front = self.fetches.front()?;
        Some((&front.ty, front.request?))
    }

    /// Request the conversion of the first fetch, unless it has one. A fetch whose conversion
    /// cannot be requested completes with an error, and the next one is tried.
    fn start_front<E: fmt::Display>(
        &mut self,
        mut convert: impl FnMut(&SelectionType) -> Result<Request, E>,
    ) -> Vec<FinishedDataTransfer> {
        let mut failed = Vec::new();
        while let Some(front) = self.fetches.front_mut() {
            if front.request.is_some() {
                break;
            }
            match convert(&front.ty) {
                Ok(request) => front.request = Some(request),
                Err(err) => failed.extend(self.complete_front(Err(io::Error::other(format!(
                    "could not request the conversion of the selection: {err}"
                ))))),
            }
        }
        failed
    }

    /// Complete the first fetch.
    fn complete_front(&mut self, result: io::Result<Vec<u8>>) -> Option<FinishedDataTransfer> {
        Some(self.fetches.pop_front()?.finish(result))
    }

    /// Abandon the fetches at the front of the queue that `abandon` selects, completing each
    /// with an [`io::ErrorKind::ConnectionAborted`] error.
    fn abandon_while(
        &mut self,
        mut abandon: impl FnMut(&PendingFetch) -> bool,
        why: &str,
    ) -> Vec<FinishedDataTransfer> {
        let mut abandoned = Vec::new();
        while self.fetches.front().is_some_and(&mut abandon) {
            let aborted = io::Error::new(io::ErrorKind::ConnectionAborted, why.to_owned());
            abandoned.extend(self.complete_front(Err(aborted)));
        }
        abandoned
    }
}

/// The properties that conversions store their replies in, used in turn so that a late reply to
/// an abandoned conversion is not taken for the reply to a newer one.
#[derive(Debug)]
pub(crate) struct PropertyPool {
    atoms: Vec<xproto::Atom>,
    next: usize,
}

impl PropertyPool {
    /// How many properties there are: comfortably more than the conversions that can be
    /// outstanding at once (one fetch and one `TARGETS` per selection, and one fetch of a drag).
    const SIZE: usize = 16;

    /// The next property in turn that is not `busy` (in use by an outstanding conversion), which
    /// is then added to `busy`.
    fn take(&mut self, busy: &mut Vec<xproto::Atom>) -> Option<xproto::Atom> {
        for _ in 0..self.atoms.len() {
            let atom = self.atoms[self.next];
            self.next = (self.next + 1) % self.atoms.len();
            if !busy.contains(&atom) {
                busy.push(atom);
                return Some(atom);
            }
        }
        None
    }
}

/// Request the conversion of `selection` to `target` into a property from `properties` that is
/// not `busy`.
fn request_conversion(
    xconn: &XConnection,
    properties: &mut PropertyPool,
    busy: &mut Vec<xproto::Atom>,
    requestor: xproto::Window,
    selection: xproto::Atom,
    target: xproto::Atom,
) -> Result<Request, X11Error> {
    let property =
        properties.take(busy).ok_or(X11Error::UnexpectedNull("free conversion property"))?;
    let sequence = xconn.convert_selection(requestor, selection, target, property)?;
    Ok(Request { property, sequence })
}

/// What a `SelectionNotify` for one of the clipboards answers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ClipboardReply {
    /// The outstanding conversion to `TARGETS`.
    Targets,
    /// The conversion of the first fetch.
    Fetch,
    /// No outstanding conversion: a late reply to a conversion that was abandoned.
    Stale,
}

/// What the reply to a conversion delivered.
#[derive(Debug)]
pub(crate) enum ReplyOutcome {
    /// The data, read from the reply's property.
    Data(Vec<u8>),
    /// The owner sends the data incrementally (INCR), in property changes to come.
    Incremental,
    /// The conversion failed.
    Failed(io::Error),
}

/// Interpret the reply to a conversion, stored in `property`: `read` reads (and deletes) the
/// property, returning its type and contents.
pub(crate) fn reply_outcome(
    property: xproto::Atom,
    incr: xproto::Atom,
    read: impl FnOnce() -> Result<(xproto::Atom, Vec<u8>), util::GetPropertyError>,
) -> ReplyOutcome {
    if property == x11rb::NONE {
        return ReplyOutcome::Failed(io::Error::new(
            io::ErrorKind::Unsupported,
            "the selection owner refused the conversion",
        ));
    }

    match read() {
        Ok((ty, _)) if ty == incr => ReplyOutcome::Incremental,
        Ok((_, data)) => ReplyOutcome::Data(data),
        Err(err) => ReplyOutcome::Failed(io::Error::new(io::ErrorKind::InvalidData, err)),
    }
}

/// One reply to a `GetProperty` request.
#[derive(Debug)]
pub(crate) struct PropertyReply {
    pub(crate) ty: xproto::Atom,
    pub(crate) format: u8,
    pub(crate) bytes_after: u32,
    pub(crate) value: Vec<u8>,
}

/// Read the whole value of a property, returning its type and value: `get` makes a `GetProperty`
/// request for a length of the value from an offset, both in units of 32 bits.
///
/// The value is read with one request, unless more is left after it (`bytes_after`). A property
/// that does not exist is an error; a property whose value is empty is not.
pub(crate) fn read_property(
    mut get: impl FnMut(u32, u32) -> Result<PropertyReply, util::GetPropertyError>,
) -> Result<(xproto::Atom, Vec<u8>), util::GetPropertyError> {
    // The length of the whole value, in units of 32 bits, whatever it is.
    const WHOLE_VALUE: u32 = u32::MAX / 4;

    let PropertyReply { ty, format, mut bytes_after, value: mut data } = get(0, WHOLE_VALUE)?;
    if ty == x11rb::NONE {
        return Err(util::GetPropertyError::Absent);
    }

    while bytes_after != 0 {
        // A reply is only cut short at a multiple of 4 bytes.
        let reply = get((data.len() / 4) as u32, WHOLE_VALUE)?;
        if reply.ty != ty {
            return Err(util::GetPropertyError::TypeMismatch(reply.ty));
        }
        if reply.format != format {
            return Err(util::GetPropertyError::FormatMismatch(reply.format.into()));
        }
        data.extend_from_slice(&reply.value);
        bytes_after = reply.bytes_after;
    }

    Ok((ty, data))
}

/// How an XFixes selection event changed the owner of a selection.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OwnerChange {
    /// The selection window took it: this application set new contents.
    Ours,
    /// Another client took it.
    Theirs,
    /// Nobody owns it any more: its owner set no owner, or its window was destroyed, or its
    /// client closed its connection.
    Gone,
}

impl OwnerChange {
    pub(crate) fn classify(
        subtype: c_int,
        owner: xproto::Window,
        selection_window: xproto::Window,
    ) -> Self {
        let set_owner = c_int::from(u8::from(xfixes::SelectionEvent::SET_SELECTION_OWNER));
        if subtype != set_owner || owner == x11rb::NONE {
            Self::Gone
        } else if owner == selection_window {
            Self::Ours
        } else {
            Self::Theirs
        }
    }
}

/// What a change of a selection's owner calls for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OwnerChangeAction {
    /// Nothing: the event predates the selection's state, or repeats an earlier one.
    Ignore,
    /// Abandon the fetches of the contents that this application replaced.
    AbandonPrevious,
    /// The selection changed hands, or its owner went away: abandon every fetch and forget the
    /// types. With `request_targets`, the types of the new owner are requested.
    AbandonAll { request_targets: bool },
}

/// What `XdndFinished` reports: the target and source windows, and whether the drop was accepted.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DropFinished {
    pub(crate) windows: (XWindow, XWindow),
    pub(crate) accepted: bool,
}

impl DropFinished {
    /// The refusal of `drop`, an `XdndDrop` that arrived with no drag in progress here: it is
    /// reported to the source window that the message names (`data.l[0]`).
    pub(crate) fn refusal_of(drop: &x11_dl::xlib::XClientMessageEvent) -> Self {
        let windows = (drop.window as XWindow, drop.data.get_long(0) as XWindow);
        Self { windows, accepted: false }
    }

    /// The data of the `XdndFinished` message (XDND): the target window, whether it accepted the
    /// drop, and the action it performed, `XdndActionCopy` for an accepted drop and `None` (0)
    /// for a refused one.
    pub(crate) fn message_data(&self, atoms: &Atoms) -> [u32; 5] {
        let (target_window, _) = self.windows;
        let (accepted, action) =
            if self.accepted { (1, atoms[XdndActionCopy]) } else { (0, x11rb::NONE) };
        [target_window, accepted, action, 0, 0]
    }
}

impl DndState {
    /// The data of the `XdndStatus` message that `this_window` sends (XDND): the target window,
    /// whether it accepts the drop, no rectangle, and the action it would perform,
    /// `XdndActionCopy` when it accepts and `None` (0) when it rejects.
    pub(crate) fn message_data(self, this_window: xproto::Window, atoms: &Atoms) -> [u32; 5] {
        let (accepted, action) = match self {
            DndState::Accepted => (1, atoms[XdndActionCopy]),
            DndState::Rejected => (0, x11rb::NONE),
        };
        [this_window, accepted, 0, 0, action]
    }
}

/// Deliver the fetches that a reply completed, then settle the drop: once the data was dropped
/// and no fetch is left, end the drag (which `drag` finds in `state`) and return what
/// `XdndFinished` is to report.
///
/// The drop is settled after the delivery, during which `state` is not borrowed: the
/// application handles the last `DataTransferReceived` of a drop while the drag is still open, so
/// it can still read the data transfer, fetch more, or refuse the drop, and `XdndFinished`
/// reports what it decided.
pub(crate) fn deliver_then_settle<S>(
    state: &RefCell<S>,
    drag: impl FnOnce(&mut S) -> &mut Option<DragState>,
    finished: Vec<FinishedDataTransfer>,
    mut deliver: impl FnMut(FinishedDataTransfer),
) -> Option<DropFinished> {
    for finished in finished {
        deliver(finished);
    }
    let mut state = state.borrow_mut();
    let drag = drag(&mut state);
    let windows = drag.as_ref()?.drop_to_finish()?;
    let accepted = drag.take()?.accepted;
    Some(DropFinished { windows, accepted })
}

/// A reply received incrementally (INCR).
#[derive(Debug)]
struct IncrReceive {
    /// The requestor window, and the property the chunks are stored in.
    window: xproto::Window,
    property: xproto::Atom,
    /// The chunks received so far.
    data: Vec<u8>,
    /// What the reply answers.
    target: IncrTarget,
}

/// Keep the incremental receptions whose property an outstanding conversion still uses (`busy`).
fn retain_outstanding_receives(receives: &mut Vec<IncrReceive>, busy: &[xproto::Atom]) {
    receives.retain(|receive| busy.contains(&receive.property));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum IncrTarget {
    /// The conversion to `TARGETS` of a clipboard.
    Targets(ClipboardSelectionType),
    /// The conversion of the first fetch of a clipboard.
    Fetch(ClipboardSelectionType),
    /// The conversion of the first fetch of the dragged data.
    Drag,
}

impl ClipboardState {
    pub fn get_types(&self, atoms: &Atoms) -> Selection {
        if let Some(owned_data) = &self.owned_data {
            let mut accepted: Vec<SelectionType> = Vec::new();

            owned_data.for_each_available_type(&mut |ty| {
                accepted.extend(SelectionType::from_dyn(atoms, ty));
                core::ops::ControlFlow::Continue(())
            });

            return Selection::new(accepted.into());
        }
        Selection::new(Arc::clone(&self.types))
    }
    /// The conversions of this selection that are outstanding.
    fn outstanding_requests(&self) -> impl Iterator<Item = Request> + '_ {
        let fetch = self.fetches.front_request().map(|(_, request)| request);
        self.targets_request.into_iter().chain(fetch)
    }

    /// Which outstanding conversion a `SelectionNotify` with `property` and `target`, whose
    /// sequence number is `event_serial`, answers.
    pub(crate) fn match_reply(
        &self,
        atoms: &Atoms,
        property: xproto::Atom,
        target: xproto::Atom,
        event_serial: u64,
    ) -> ClipboardReply {
        let targets = |target| target == atoms[TARGETS] || target == xproto::AtomEnum::ATOM.into();
        if self
            .targets_request
            .is_some_and(|request| request.answered_by(targets, property, target, event_serial))
        {
            return ClipboardReply::Targets;
        }

        if self.fetches.front_request().is_some_and(|(ty, request)| {
            request.answered_by(|target| target == ty.atom(), property, target, event_serial)
        }) {
            return ClipboardReply::Fetch;
        }

        ClipboardReply::Stale
    }

    /// Queue a fetch of `ty` from the current contents, to be delivered to `window`, with the
    /// conversion `request` if it was already requested. `latin1` tells whether the data is text
    /// in ISO Latin-1.
    pub(crate) fn enqueue_fetch(
        &mut self,
        ty: SelectionType,
        window: WindowId,
        request: Option<Request>,
        latin1: bool,
    ) -> AsyncRequestSerial {
        let serial = AsyncRequestSerial::get();
        let id = self.transfer_serial;
        self.fetches.push(PendingFetch { id, serial, ty, window, request, latin1 });
        serial
    }

    /// Decide what an XFixes selection event, whose selection timestamp is `serial`, calls for.
    pub(crate) fn owner_change_action(
        &mut self,
        change: OwnerChange,
        serial: u64,
    ) -> OwnerChangeAction {
        match change {
            // `set_clipboard` minted the transfer id of the new contents: the fetches of the
            // previous contents are abandoned, which spares them from waiting on an owner that
            // may never answer them.
            OwnerChange::Ours => OwnerChangeAction::AbandonPrevious,
            // While we own the selection, only a `SelectionClear` takes it from us: this event
            // was generated before we took it.
            OwnerChange::Theirs | OwnerChange::Gone if self.owned_data.is_some() => {
                OwnerChangeAction::Ignore
            },
            // Sometimes you get duplicate update events. Skip these.
            OwnerChange::Theirs if serial == self.x11_serial => OwnerChangeAction::Ignore,
            OwnerChange::Theirs => {
                self.x11_serial = serial;
                OwnerChangeAction::AbandonAll { request_targets: true }
            },
            OwnerChange::Gone => OwnerChangeAction::AbandonAll { request_targets: false },
        }
    }

    /// Abandon the fetches of contents that this application replaced: those requested under an
    /// earlier transfer id.
    pub(crate) fn abandon_previous_fetches(&mut self) -> Vec<FinishedDataTransfer> {
        let current = self.transfer_serial;
        self.fetches.abandon_while(|fetch| fetch.id != current, "the selection changed hands")
    }

    /// The selection changed hands: abandon its fetches, and forget its types, which the
    /// transfer id that named them no longer identifies.
    pub(crate) fn abandon_all(&mut self, why: &str) -> Vec<FinishedDataTransfer> {
        self.transfer_serial = generate_transfer_id();
        self.types = Default::default();
        self.targets_request = None;
        self.fetches.abandon_while(|_| true, why)
    }

    /// Take `send_data` as our contents, published by the `SetSelectionOwner` request
    /// `owner_sequence`.
    pub(crate) fn publish(&mut self, send_data: Box<dyn DataTransferSend>, owner_sequence: u64) {
        // The contents change: the transfer id of the previous ones no longer names them.
        self.transfer_serial = generate_transfer_id();
        self.owned_data = Some(send_data);
        self.owner_sequence = Some(owner_sequence);
    }

    /// Handle a `SelectionClear` that the server generated once it had processed our request
    /// `event_serial`: returns whether it took our contents, which it then drops.
    ///
    /// One generated before our latest `SetSelectionOwner` took contents that were replaced
    /// since, e.g. handed over to a clipboard manager while the event loop exited, and is
    /// ignored. Sequence numbers are compared in the 32 bits that X events carry, wrapping
    /// around.
    pub(crate) fn clear(&mut self, event_serial: u64) -> bool {
        let stale = self.owner_sequence.is_some_and(|sequence| {
            ((event_serial as u32).wrapping_sub(sequence as u32) as i32) < 0
        });
        if !stale {
            self.owned_data = None;
        }
        !stale
    }

    /// The transfer id of the contents to hand over to a clipboard manager: those that this
    /// application owns, unless they were already handed over (`handed_off`).
    pub(crate) fn contents_to_hand_off(
        &self,
        handed_off: Option<DataTransferId>,
    ) -> Option<DataTransferId> {
        self.owned_data.as_ref()?;
        Some(self.transfer_serial).filter(|&contents| handed_off != Some(contents))
    }

    /// The type to read for `hint` among the types available on the clipboard, as
    /// [`preferred_type`] chooses, whether the contents are ours or another client's.
    pub fn find_type_by_hint(&self, hint: TypeHint, atoms: &Atoms) -> Option<SelectionType> {
        if let Some(owned_data) = &self.owned_data {
            // The targets that our types matching `hint` are offered under.
            let mut offered = Vec::new();
            owned_data.for_each_available_type(&mut |ty| {
                if ty.matches(&hint) {
                    offered.extend(SelectionType::from_dyn(atoms, ty));
                }
                core::ops::ControlFlow::Continue(())
            });
            most_preferred(offered.iter(), hint, atoms).cloned()
        } else {
            preferred_type(&self.types, hint, atoms).cloned()
        }
    }

    /// Decide how to answer another client's request to convert this selection, `selection`, to
    /// `target`.
    ///
    /// A conversion that cannot be made is refused, which the requestor learns from a
    /// `SelectionNotify` whose property is `None` (ICCCM, "Responsibilities of the Selection
    /// Owner").
    pub(crate) fn plan_conversion(
        &self,
        atoms: &Atoms,
        selection: ClipboardSelectionType,
        target: xproto::Atom,
        incr_threshold: usize,
    ) -> ReplyPlan {
        let Some(owned_data) = &self.owned_data else {
            return ReplyPlan::Refuse;
        };
        // The freedesktop clipboard manager specification covers CLIPBOARD only.
        let hands_off = selection == ClipboardSelectionType::Clipboard;

        if target == atoms[TARGETS] {
            // We must advertise at least TIMESTAMP, TARGETS and MULTIPLE, which ICCCM requires
            // every owner to support. SAVE_TARGETS tells clipboard managers that we hand the
            // clipboard over to them.
            let mut targets = vec![atoms[TIMESTAMP], atoms[TARGETS], atoms[MULTIPLE]];
            targets.extend(hands_off.then_some(atoms[SAVE_TARGETS]));

            // Add user defined types if possible
            owned_data.for_each_available_type(&mut |ty| {
                targets.extend(SelectionType::from_dyn(atoms, ty).map(|ty| ty.atom()));
                core::ops::ControlFlow::Continue(())
            });

            ReplyPlan::Targets(targets)
        } else if target == atoms[TIMESTAMP] {
            ReplyPlan::Timestamp
        } else if target == atoms[SAVE_TARGETS] && hands_off {
            // A marker target: converting to it has no effect.
            ReplyPlan::SideEffect
        } else {
            let ty = SelectionType::new(atoms, target);
            let data = match owned_data.data_for_type(&ty) {
                Some(SendData::Uris(strings)) => encode_uri_list(strings),
                // ICCCM: `STRING` is ISO Latin-1.
                Some(SendData::String(str)) if target == atoms[STRING] => utf8_to_latin1(&str),
                // Plain text is UTF-8; bytes that are not are read lossily.
                Some(SendData::Bytes(text)) if target == atoms[STRING] => {
                    utf8_to_latin1(&String::from_utf8_lossy(&text))
                },
                Some(SendData::String(str)) => str.into_bytes(),
                Some(SendData::Bytes(binary)) => binary,
                Some(send_data) => {
                    warn!("Unknown send data {send_data:?}");
                    return ReplyPlan::Refuse;
                },
                None => return ReplyPlan::Refuse,
            };

            if data.len() < incr_threshold {
                // We can skip using the INCR for short messages.
                ReplyPlan::Direct(ty, data)
            } else {
                ReplyPlan::Incr(ty, data)
            }
        }
    }
}

/// How the owner of a selection answers the conversion of one target.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReplyPlan {
    /// Store the list of supported targets, of type `ATOM`.
    Targets(Vec<xproto::Atom>),
    /// Store the timestamp used to acquire the selection, of type `INTEGER`.
    Timestamp,
    /// Store the data in the property with one request.
    Direct(SelectionType, Vec<u8>),
    /// Send the data incrementally, with the `INCR` mechanism.
    Incr(SelectionType, Vec<u8>),
    /// Answer a target whose conversion is a side effect, with an empty property of type `NULL`
    /// (ICCCM).
    SideEffect,
    /// Refuse the conversion.
    Refuse,
}

/// The property to store the reply to a `SelectionRequest` in.
///
/// ICCCM: a request whose property is `None` comes from an obsolete client, and owners are
/// encouraged to use the target atom as the property name instead.
pub(crate) fn reply_property(property: xproto::Atom, target: xproto::Atom) -> xproto::Atom {
    if property == x11rb::NONE { target } else { property }
}

/// Answer the conversions listed by a `MULTIPLE` request: `pairs` holds `(target, property)` atom
/// pairs, and `answer` converts one of them, returning whether it succeeded.
///
/// ICCCM: the pairs are converted in order, as if each were a separate request; a pair whose
/// property is `None` is not valid; and the property of every pair that could not be converted is
/// replaced by `None`. Returns that rewritten list when some conversion failed, so the caller can
/// store it back, or `None` when all of them succeeded. A trailing unpaired atom is ignored, and
/// `MULTIPLE` itself is not converted inside a `MULTIPLE` request.
pub(crate) fn answer_multiple(
    pairs: &[xproto::Atom],
    multiple: xproto::Atom,
    mut answer: impl FnMut(xproto::Atom, xproto::Atom) -> bool,
) -> Option<Vec<xproto::Atom>> {
    let pairs = &pairs[..pairs.len() / 2 * 2];
    let mut rewritten: Option<Vec<xproto::Atom>> = None;

    for (index, pair) in pairs.chunks_exact(2).enumerate() {
        let (target, property) = (pair[0], pair[1]);
        let converted = property != x11rb::NONE && target != multiple && answer(target, property);
        if !converted {
            rewritten.get_or_insert_with(|| pairs.to_vec())[index * 2 + 1] = x11rb::NONE;
        }
    }

    rewritten
}

/// The `SelectionNotify` that answers a `SelectionRequest`.
///
/// ICCCM: its requestor, selection, target and time are those of the request, and its property is
/// the one the reply was stored in, or `None` when the conversion is refused.
pub(crate) fn selection_notify_reply(
    request: &XSelectionRequestEvent,
    property: Option<xproto::Atom>,
) -> xproto::SelectionNotifyEvent {
    xproto::SelectionNotifyEvent {
        response_type: xproto::SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time as _,
        requestor: request.requestor as _,
        selection: request.selection as _,
        target: request.target as _,
        property: property.unwrap_or(x11rb::NONE),
    }
}

/// How long the clipboard manager is given to save the clipboard when the event loop ends.
pub(crate) const CLIPBOARD_MANAGER_TIMEOUT: Duration = Duration::from_secs(5);

/// A hand-over of the clipboard to the clipboard manager, waiting for the manager's reply
/// (freedesktop clipboard manager specification).
#[derive(Debug)]
pub(crate) struct ClipboardHandOff {
    /// The window that asked the manager to save the clipboard.
    requestor: xproto::Window,
    /// `CLIPBOARD_MANAGER`, the manager's selection.
    manager: xproto::Atom,
    /// `SAVE_TARGETS`, the target the manager's selection was converted to.
    save_targets: xproto::Atom,
    /// When to stop waiting for the reply.
    deadline: Instant,
}

/// How a hand-over of the clipboard ended, short of its deadline.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HandOffOutcome {
    /// The manager saved the clipboard.
    Saved,
    /// The manager could not save it.
    Refused,
    /// The manager went away.
    ManagerGone,
}

impl ClipboardHandOff {
    /// The outcome that a `SelectionNotify` brings, if it is the manager's reply.
    pub(crate) fn reply(
        &self,
        requestor: xproto::Window,
        selection: xproto::Atom,
        target: xproto::Atom,
        property: xproto::Atom,
    ) -> Option<HandOffOutcome> {
        let answers =
            requestor == self.requestor && selection == self.manager && target == self.save_targets;
        answers.then_some(if property == x11rb::NONE {
            HandOffOutcome::Refused
        } else {
            HandOffOutcome::Saved
        })
    }

    /// The time left to wait for the manager's reply, if any.
    pub(crate) fn remaining(&self, now: Instant) -> Option<Duration> {
        self.deadline.checked_duration_since(now).filter(|remaining| !remaining.is_zero())
    }

    /// Whether the hand-over takes `event` from the event queue: the requests for our selections
    /// and the deletions of the properties that our INCR transfers wait for (`incr_properties`),
    /// which it serves, and the manager's reply and departure, which end it
    /// (`xfixes_selection_notify` is the type of XFixes selection events). Other events stay
    /// queued for the event loop, which may run again.
    pub(crate) fn takes(
        &self,
        event: &XEvent,
        xfixes_selection_notify: c_int,
        incr_properties: &[xproto::Atom],
    ) -> bool {
        match event.get_type() {
            xlib::SelectionRequest => true,
            xlib::PropertyNotify => {
                let event: &XPropertyEvent = event.as_ref();
                event.state == xlib::PropertyDelete
                    && incr_properties.contains(&(event.atom as xproto::Atom))
            },
            xlib::SelectionNotify => {
                let event: &XSelectionEvent = event.as_ref();
                let (requestor, selection) = (event.requestor as _, event.selection as _);
                self.reply(requestor, selection, event.target as _, event.property as _).is_some()
            },
            ty if ty == xfixes_selection_notify => {
                // SAFETY: an event of this type is an XFixes selection event, which `XEvent`'s
                // padding holds.
                let event =
                    unsafe { &*(event as *const XEvent).cast::<XFixesSelectionNotifyEvent>() };
                event.selection as xproto::Atom == self.manager
            },
            _ => false,
        }
    }
}

/// Whether our contents are handed over to the clipboard manager now, which marks them handed off:
/// only if CLIPBOARD is still ours, its owner as the server reports it being `clipboard_owner`,
/// and a clipboard manager owns `CLIPBOARD_MANAGER` (`manager`). Contents that no manager was there
/// for stay unmarked, to be handed over when the event loop next exits or is dropped.
pub(crate) fn hand_off_proceeds(
    clipboard_owner: Option<xproto::Window>,
    selection_window: xproto::Window,
    manager: Option<xproto::Window>,
) -> bool {
    clipboard_owner == Some(selection_window) && manager.is_some()
}

/// The targets to ask the clipboard manager to save: the advertised ones, without those that are
/// not data (`not_data`).
pub(crate) fn save_targets_list(
    advertised: &[xproto::Atom],
    not_data: &[xproto::Atom],
) -> Vec<xproto::Atom> {
    advertised.iter().copied().filter(|target| !not_data.contains(target)).collect()
}

#[derive(Debug)]
pub struct DataTransferState {
    xconn: Arc<XConnection>,
    /// The window that owns this application's selections and receives the conversions it
    /// requests. See [`DataTransferState::new`].
    selection_window: xproto::Window,
    // If `None`, no drag operation is in progress.
    state: Option<DragState>,

    clipboards: [ClipboardState; ClipboardSelectionType::COUNT],
    /// Contains the remnants of the message that are yet to be sent to the current clipboard
    /// recipient using the INCR ICCCM protocol
    clip_incr_send: Vec<IncrState>,
    /// Contains the partially received message from the clipboard using the INCR ICCCM protocol
    clip_incr_receive: Vec<IncrReceive>,
    /// See [`incr_threshold`].
    incr_threshold: usize,
    /// The properties that conversions store their replies in.
    properties: PropertyPool,
    /// The transfer id of the clipboard contents last handed over to the clipboard manager.
    clipboard_handed_off: Option<DataTransferId>,
}

#[derive(Debug)]
pub struct Selection {
    types: Arc<[SelectionType]>,
}

impl Selection {
    pub(crate) fn new(types: Arc<[SelectionType]>) -> Selection {
        Selection { types }
    }
}

pub struct FinishedDataTransfer {
    id: DataTransferId,
    serial: AsyncRequestSerial,
    value: Arc<dyn TypedData>,
    window: WindowId,
}

impl FinishedDataTransfer {
    #[must_use]
    pub fn window(&self) -> WindowId {
        self.window
    }
    #[must_use]
    pub fn into_event(self) -> WindowEvent {
        WindowEvent::DataTransferReceived { id: self.id, serial: self.serial, value: self.value }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SelectionType {
    hint: Option<TypeHint>,
    atom: xproto::Atom,
}

impl SelectionType {
    fn atom_hints(atoms: &Atoms) -> impl Iterator<Item = (xproto::Atom, TypeHint)> {
        [
            // Files
            (atoms[TextUriList], TypeHint::UriList),
            // Plaintext, in the order it is read in (`preferred_type`): our `TARGETS` list them in
            // this order, so that a requestor taking the first text target gets UTF-8.
            (atoms[UTF8_STRING], TypeHint::Plaintext),
            (atoms[TextPlainCharsetUtf8], TypeHint::Plaintext),
            (atoms[TextPlain], TypeHint::Plaintext),
            (atoms[STRING], TypeHint::Plaintext),
            // HTML
            (atoms[TextHtml], TypeHint::Html),
            (atoms[TextHtmlCharsetUtf8], TypeHint::Html),
            // RTF
            (atoms[ApplicationRtf], TypeHint::Rtf),
            // Audio
            (atoms[AudioAac], TypeHint::Audio { extension_hint: Some("aac") }),
            (atoms[AudioAiff], TypeHint::Audio { extension_hint: Some("aif") }),
            (atoms[AudioFlac], TypeHint::Audio { extension_hint: Some("flac") }),
            (atoms[AudioVndWav], TypeHint::Audio { extension_hint: Some("wav") }),
            (atoms[AudioVndWave], TypeHint::Audio { extension_hint: Some("wav") }),
            (atoms[AudioWav], TypeHint::Audio { extension_hint: Some("wav") }),
            (atoms[AudioWave], TypeHint::Audio { extension_hint: Some("wav") }),
            (atoms[AudioXWav], TypeHint::Audio { extension_hint: Some("wav") }),
            (atoms[AudioOgg], TypeHint::Audio { extension_hint: Some("ogg") }),
            (atoms[AudioMpeg], TypeHint::Audio { extension_hint: Some("mp3") }),
            // Image
            (atoms[ImageBmp], TypeHint::Image { extension_hint: Some("bmp") }),
            (atoms[ImageGif], TypeHint::Image { extension_hint: Some("gif") }),
            (atoms[ImageJpeg], TypeHint::Image { extension_hint: Some("jpg") }),
            (atoms[ImagePjpeg], TypeHint::Image { extension_hint: Some("jpg") }),
            (atoms[ImagePng], TypeHint::Image { extension_hint: Some("png") }),
            (atoms[ImageRaw], TypeHint::Image { extension_hint: Some("raw") }),
            (atoms[ImageSvg], TypeHint::Image { extension_hint: Some("svg") }),
            (atoms[ImageTiff], TypeHint::Image { extension_hint: Some("tiff") }),
            (atoms[ImageWebp], TypeHint::Image { extension_hint: Some("webp") }),
            (atoms[ImageXIcon], TypeHint::Image { extension_hint: Some("ico") }),
        ]
        .into_iter()
    }

    pub(crate) fn new(atoms: &Atoms, atom: xproto::Atom) -> Self {
        let hint =
            Self::atom_hints(atoms).find_map(|(haystack, hint)| (haystack == atom).then_some(hint));

        Self { hint, atom }
    }

    fn from_dyn(atoms: &Atoms, type_: &dyn TransferType) -> impl Iterator<Item = Self> {
        let downcast = type_.cast_ref::<Self>().cloned();
        let downcast_failed = downcast.is_none();
        // This filter is a bit hacky, but it's the only way to ensure that we always
        // return the same type.
        let from_hint = downcast_failed
            .then_some(
                Self::atom_hints(atoms)
                    .filter(|(_, haystack)| TransferType::matches(haystack, type_))
                    .map(|(atom, hint)| Self { atom, hint: Some(hint) }),
            )
            .into_iter()
            .flatten();

        downcast.into_iter().chain(from_hint)
    }

    pub fn atom(&self) -> xproto::Atom {
        self.atom
    }
}

impl TransferType for SelectionType {
    fn hint(&self) -> Option<TypeHint> {
        self.hint
    }

    fn matches(&self, other: &dyn TransferType) -> bool {
        if let Some(other_mime) = other.cast_ref::<Self>() {
            *self == *other_mime
        } else {
            self.hint().is_some_and(|hint| other.hint() == Some(hint))
        }
    }
}

impl DataTransfer for Selection {
    fn for_each_available_type<'this>(
        &'this self,
        func: &'_ mut dyn FnMut(&'this dyn TransferType) -> std::ops::ControlFlow<()>,
    ) {
        let _ = self.types.iter().map(|mime| mime as &dyn TransferType).try_for_each(func);
    }
}

impl DataTransferState {
    pub fn new(xconn: Arc<XConnection>) -> Result<Self, X11Error> {
        let conn = xconn.xcb_connection();
        let root = xconn.default_root().root;

        // Selections are owned, and the conversions this application requests are received,
        // through a window of its own that lives as long as the event loop. Neither may depend
        // on a window of the application: when the owner's window is destroyed, the selection
        // reverts to no owner without a `SelectionClear`, and a conversion whose requestor
        // window is gone is never answered. The window is never mapped; it selects property
        // changes to receive incremental (INCR) transfers.
        let selection_window = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            selection_window,
            root,
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new().event_mask(xproto::EventMask::PROPERTY_CHANGE),
        )?
        .check()?;

        // From here on the state owns the window: dropping it, also when a later step fails,
        // destroys the window.
        let mut data_transfer = DataTransferState {
            xconn: Arc::clone(&xconn),
            selection_window,
            state: Default::default(),
            clipboards: Default::default(),
            clip_incr_send: Default::default(),
            clip_incr_receive: Default::default(),
            incr_threshold: incr_threshold(conn.maximum_request_bytes()),
            properties: PropertyPool { atoms: Vec::new(), next: 0 },
            clipboard_handed_off: None,
        };

        // Listen for all selection owner changes for the clipboard. This is a bit hacky but we must
        // know all of the types when the user wants to paste.
        match conn.xfixes_query_version(5, 0).map_err(ReplyError::from).and_then(|c| c.reply()) {
            Ok(_) => {
                for item in ClipboardSelectionType::iter() {
                    conn.xfixes_select_selection_input(
                        root,
                        item.selection_atom(xconn.atoms()),
                        // An owner whose window is destroyed or whose client exits will never
                        // answer the conversions it was asked for.
                        xfixes::SelectionEventMask::SET_SELECTION_OWNER
                            | xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                            | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
                    )
                    .expect_then_ignore_error("Failed to select selection owner changes");
                }
            },
            Err(err) => error!("Failed to query the XFixes version: {err}"),
        }

        data_transfer.properties.atoms = (0..PropertyPool::SIZE)
            .map(|index| conn.intern_atom(false, format!("WINIT_SELECTION_{index}").as_bytes()))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|cookie| cookie.reply().map(|reply| reply.atom))
            .collect::<Result<Vec<_>, _>>()?;

        // Request the types on offer in the selections that already have an owner.
        for clipboard in ClipboardSelectionType::iter() {
            if let Err(err) = data_transfer.request_targets(clipboard) {
                debug!("Not requesting the initial TARGETS of {clipboard:?}: {err}");
            }
        }

        Ok(data_transfer)
    }

    /// The window that owns this application's selections and receives its conversions.
    pub fn selection_window(&self) -> xproto::Window {
        self.selection_window
    }

    /// The drag in progress, if any.
    pub(crate) fn drag_mut(&mut self) -> &mut Option<DragState> {
        &mut self.state
    }

    pub fn state(&self) -> Option<&DragState> {
        self.state.as_ref()
    }

    pub fn state_mut(&mut self) -> Option<&mut DragState> {
        self.state.as_mut()
    }

    pub fn find_type_by_hint(&self, hint: TypeHint) -> Option<&SelectionType> {
        preferred_type(&self.state.as_ref()?.types, hint, self.xconn.atoms())
    }

    pub fn init_state(
        &mut self,
        version: c_long,
        source_window: xproto::Window,
        target_window: xproto::Window,
        types: Arc<[SelectionType]>,
    ) -> &DragState {
        self.state.insert(DragState {
            version,
            types,
            source_window,
            target_window,
            ..Default::default()
        })
    }

    pub fn reset_state(&mut self) -> Option<DragState> {
        self.state.take()
    }

    /// Tell the source that the drop is finished (`XdndFinished`), reporting `finished`.
    pub unsafe fn send_finished(&self, finished: &DropFinished) -> Result<(), X11Error> {
        let atoms = self.xconn.atoms();
        let (_, source_window) = finished.windows;
        let data = finished.message_data(atoms);
        self.xconn
            .send_client_msg(source_window, source_window, atoms[XdndFinished] as _, None, data)?
            .ignore_error();

        Ok(())
    }

    pub unsafe fn get_type_list(
        &self,
        source_window: xproto::Window,
    ) -> Result<Vec<xproto::Atom>, util::GetPropertyError> {
        let atoms = self.xconn.atoms();
        self.xconn.get_property(
            source_window,
            atoms[XdndTypeList],
            xproto::Atom::from(xproto::AtomEnum::ATOM),
        )
    }

    pub unsafe fn send_status(
        &self,
        this_window: xproto::Window,
        target_window: xproto::Window,
        status: DndState,
    ) -> Result<(), X11Error> {
        let atoms = self.xconn.atoms();
        let data = status.message_data(this_window, atoms);
        self.xconn
            .send_client_msg(target_window, target_window, atoms[XdndStatus] as _, None, data)?
            .ignore_error();

        Ok(())
    }

    /// Gets the [`ClipboardSelectionType`] associated with the specified [`DataTransferId`].
    pub fn resolve_clipboard_type(
        &self,
        data_transfer: DataTransferId,
    ) -> Option<ClipboardSelectionType> {
        ClipboardSelectionType::iter()
            .find(|&clip| self.clipboards[clip as usize].transfer_serial == data_transfer)
    }

    pub fn get_clipboard(&self, clipboard: ClipboardSelectionType) -> &ClipboardState {
        &self.clipboards[clipboard as usize]
    }

    pub fn get_clipboard_mut(&mut self, clipboard: ClipboardSelectionType) -> &mut ClipboardState {
        &mut self.clipboards[clipboard as usize]
    }

    /// Answer a `SelectionRequest` from another client, returning the property the reply was
    /// stored in, or `None` if the conversion is refused.
    ///
    /// The caller sends the `SelectionNotify` in both cases (see [`selection_notify_reply`]).
    pub fn answer_selection_request(
        &mut self,
        xev: &XSelectionRequestEvent,
    ) -> Option<xproto::Atom> {
        // For strange reasons, all the values in the input request are u64 whereas the sending uses
        // a different library with u32s
        let target = xev.target as xproto::Atom;
        let property = reply_property(xev.property as xproto::Atom, target);
        let requestor = xev.requestor as xproto::Window;

        let Some(clipboard) =
            ClipboardSelectionType::from_atom(self.xconn.atoms(), xev.selection as _)
        else {
            warn!(
                "Received request for unknown selection {}",
                self.xconn.atom_str(xev.selection as _)
            );
            return None;
        };

        if target == self.xconn.atoms()[MULTIPLE] {
            // ICCCM: MULTIPLE is valid only when a property is specified.
            if xev.property as xproto::Atom == x11rb::NONE {
                return None;
            }
            return self
                .answer_multiple_request(requestor, clipboard, property)
                .then_some(property);
        }

        self.answer_target(requestor, clipboard, target, property).then_some(property)
    }

    /// Answer a `MULTIPLE` request, whose property on `requestor` lists the `(target, property)`
    /// pairs to convert. Returns whether the list could be read.
    fn answer_multiple_request(
        &mut self,
        requestor: xproto::Window,
        clipboard: ClipboardSelectionType,
        property: xproto::Atom,
    ) -> bool {
        let atoms = self.xconn.atoms();
        let (atom_pair, multiple) = (atoms[ATOM_PAIR], atoms[MULTIPLE]);
        let pairs = match self.xconn.get_property::<u32>(requestor, property, atom_pair) {
            Ok(pairs) => pairs,
            Err(err) => {
                debug!("Refusing MULTIPLE, could not read its list of conversions: {err}");
                return false;
            },
        };

        let failed = answer_multiple(&pairs, multiple, |target, property| {
            self.answer_target(requestor, clipboard, target, property)
        });

        if let Some(rewritten) = failed {
            match self.xconn.change_property(
                requestor,
                property,
                atom_pair,
                xproto::PropMode::REPLACE,
                &rewritten,
            ) {
                Ok(cookie) => cookie.ignore_error(),
                Err(err) => warn!("Unable to report the failed conversions of MULTIPLE: {err}"),
            }
        }

        true
    }

    /// Convert the selection to `target`, storing the result in `property` on `requestor`.
    /// Returns whether the conversion was made.
    fn answer_target(
        &mut self,
        requestor: xproto::Window,
        clipboard: ClipboardSelectionType,
        target: xproto::Atom,
        property: xproto::Atom,
    ) -> bool {
        let atoms = self.xconn.atoms();
        let replace = xproto::PropMode::REPLACE;
        let plan = self.get_clipboard(clipboard).plan_conversion(
            atoms,
            clipboard,
            target,
            self.incr_threshold,
        );

        let stored = match plan {
            ReplyPlan::Refuse => {
                debug!(
                    "Refusing the conversion of {clipboard:?} to {}",
                    self.xconn.atom_str(target)
                );
                return false;
            },
            ReplyPlan::Targets(targets) => {
                let stored = self.xconn.change_property(
                    requestor,
                    property,
                    xproto::AtomEnum::ATOM.into(),
                    replace,
                    &targets,
                );
                // The names are resolved inside `trace!`, so that the round trip per atom is only
                // made when trace logging is enabled.
                trace!(
                    "Written TARGETS {:?} into property {} ",
                    targets.iter().map(|&atom| self.xconn.atom_str(atom)).collect::<Vec<_>>(),
                    self.xconn.atom_str(property)
                );
                stored
            },
            ReplyPlan::Timestamp => self.xconn.change_property(
                requestor,
                property,
                xproto::AtomEnum::INTEGER.into(),
                replace,
                &[CURRENT_TIME],
            ),
            ReplyPlan::SideEffect => {
                self.xconn.change_property::<u8>(requestor, property, atoms[NULL], replace, &[])
            },
            ReplyPlan::Direct(ty, data) => {
                self.xconn.change_property(requestor, property, ty.atom(), replace, &data)
            },
            ReplyPlan::Incr(ty, data) => {
                return match self.start_incr_sender(requestor, property, clipboard, data, ty) {
                    Ok(()) => true,
                    Err(err) => {
                        warn!("Unable to start INCR sender {err}");
                        false
                    },
                };
            },
        };

        match stored {
            Ok(cookie) => {
                cookie.ignore_error();
                true
            },
            Err(err) => {
                warn!("Unable to store the converted selection: {err}");
                false
            },
        }
    }

    /// Sets up the sending for the INCR
    fn start_incr_sender(
        &mut self,
        requestor: xproto::Window,
        property: xproto::Atom,
        clipboard: ClipboardSelectionType,
        data: Vec<u8>,
        ty: SelectionType,
    ) -> Result<(), crate::event_loop::X11Error> {
        let xconn = &self.xconn;
        let atoms = xconn.atoms();

        let current_attributes =
            xconn.xcb_connection().get_window_attributes(requestor)?.reply()?;
        // Add the property change listener to the existing mask so we do not delete bits
        let event_mask =
            Some(current_attributes.your_event_mask | xproto::EventMask::PROPERTY_CHANGE);

        // For INCR we must know when the recipient deletes the property to add some more
        // Therefore we listen for property change events on the other window
        xconn.xcb_connection().change_window_attributes(
            requestor,
            &xproto::ChangeWindowAttributesAux { event_mask, ..Default::default() },
        )?;
        let announcement = incr_announcement(data.len());
        self.clip_incr_send.push(IncrState { data, property, ty, clipboard });
        xconn
            .change_property(
                requestor,
                property,
                atoms[INCR],
                xproto::PropMode::REPLACE,
                &announcement,
            )
            .expect_then_ignore_error("unable to change property to start INCR sender");
        trace!("Starting INCR sender to {requestor} property {}", xconn.atom_str(property));

        Ok(())
    }

    /// The properties that outstanding conversions store their replies in.
    fn busy_properties(&self) -> Vec<xproto::Atom> {
        let drag = self.state.iter().filter_map(|state| state.fetches.front_request());
        self.clipboards
            .iter()
            .flat_map(ClipboardState::outstanding_requests)
            .chain(drag.map(|(_, request)| request))
            .map(|request| request.property)
            .collect()
    }

    /// Forget the incremental receptions whose conversion was abandoned, so that their late
    /// chunks are ignored.
    fn drop_orphan_receives(&mut self) {
        let busy = self.busy_properties();
        retain_outstanding_receives(&mut self.clip_incr_receive, &busy);
    }

    /// Ignore a late reply to an abandoned conversion, deleting the property it was stored in
    /// unless an outstanding conversion uses it.
    fn discard_stale_reply(&self, requestor: xproto::Window, property: xproto::Atom) {
        trace!("Ignoring a selection reply that answers no outstanding conversion");
        if property != x11rb::NONE && !self.busy_properties().contains(&property) {
            if let Ok(cookie) = self.xconn.xcb_connection().delete_property(requestor, property) {
                cookie.ignore_error();
            }
        }
    }

    /// Read (and delete) the property a reply was stored in, returning its type and contents.
    ///
    /// ICCCM: the requestor deletes the property once it has read the reply.
    fn read_reply(
        &self,
        window: xproto::Window,
        property: xproto::Atom,
    ) -> Result<(xproto::Atom, Vec<u8>), util::GetPropertyError> {
        let conn = self.xconn.xcb_connection();
        read_property(|offset, length| {
            let reply = conn
                .get_property(true, window, property, xproto::AtomEnum::ANY, offset, length)?
                .reply()?;
            Ok(PropertyReply {
                ty: reply.type_,
                format: reply.format,
                bytes_after: reply.bytes_after,
                value: reply.value,
            })
        })
    }

    /// Handle a `SelectionNotify` for one of the clipboards: the reply to one of our conversions.
    /// Returns the fetches it completes.
    #[must_use]
    pub fn clipboard_reply(&mut self, xev: &XSelectionEvent) -> Vec<FinishedDataTransfer> {
        let requestor = xev.requestor as xproto::Window;
        let property = xev.property as xproto::Atom;
        let target = xev.target as xproto::Atom;

        // Our conversions are received on the selection window: anything else is not ours
        if requestor != self.selection_window {
            return Vec::new();
        }
        let atoms = self.xconn.atoms();
        let Some(clipboard) = ClipboardSelectionType::from_atom(atoms, xev.selection as _) else {
            return Vec::new();
        };

        let incr = atoms[INCR];
        let reply =
            self.get_clipboard(clipboard).match_reply(atoms, property, target, xev.serial as u64);
        match reply {
            ClipboardReply::Stale => {
                self.discard_stale_reply(requestor, property);
                Vec::new()
            },
            ClipboardReply::Targets => {
                match reply_outcome(property, incr, || self.read_reply(requestor, property)) {
                    ReplyOutcome::Incremental => self.clip_incr_receive.push(IncrReceive {
                        window: requestor,
                        property,
                        data: Vec::new(),
                        target: IncrTarget::Targets(clipboard),
                    }),
                    ReplyOutcome::Data(data) => self.populate_targets(clipboard, data),
                    ReplyOutcome::Failed(err) => {
                        self.get_clipboard_mut(clipboard).targets_request = None;
                        debug!("No TARGETS for {clipboard:?}: {err}");
                    },
                }
                Vec::new()
            },
            ClipboardReply::Fetch => {
                match reply_outcome(property, incr, || self.read_reply(requestor, property)) {
                    ReplyOutcome::Incremental => {
                        self.clip_incr_receive.push(IncrReceive {
                            window: requestor,
                            property,
                            data: Vec::new(),
                            target: IncrTarget::Fetch(clipboard),
                        });
                        Vec::new()
                    },
                    ReplyOutcome::Data(data) => self.complete_clipboard_fetch(clipboard, Ok(data)),
                    ReplyOutcome::Failed(err) => self.complete_clipboard_fetch(clipboard, Err(err)),
                }
            },
        }
    }

    /// Check if we are the ones receiving and there is some new value for the clipboard property.
    /// Returns the fetches it completes.
    #[must_use]
    pub fn check_incr_receive_data(&mut self, xev: &XPropertyEvent) -> Vec<FinishedDataTransfer> {
        let atom = xev.atom as xproto::Atom;
        let xwindow = xev.window as xproto::Window;

        // We only care about new values
        if xev.state as u32 != xproto::Property::NEW_VALUE.into() {
            return Vec::new();
        }

        let Some(index) = self
            .clip_incr_receive
            .iter()
            .position(|receive| receive.window == xwindow && receive.property == atom)
        else {
            return Vec::new();
        };

        let result = match self.read_reply(xwindow, atom) {
            // A zero-length chunk ends the transfer.
            Ok((_, chunk)) if chunk.is_empty() => Ok(()),
            Ok((_, chunk)) => {
                self.clip_incr_receive[index].data.extend_from_slice(&chunk);
                return Vec::new();
            },
            // A chunk that is gone, or that cannot be read, ends the transfer as a failure.
            Err(err) => Err(io::Error::new(io::ErrorKind::InvalidData, err)),
        };
        let IncrReceive { data, target, .. } = self.clip_incr_receive.remove(index);
        let result = result.map(|()| data);

        match target {
            IncrTarget::Targets(clipboard) => {
                match result {
                    Ok(data) => self.populate_targets(clipboard, data),
                    Err(err) => {
                        self.get_clipboard_mut(clipboard).targets_request = None;
                        debug!("No TARGETS for {clipboard:?}: {err}");
                    },
                }
                Vec::new()
            },
            IncrTarget::Fetch(clipboard) => self.complete_clipboard_fetch(clipboard, result),
            IncrTarget::Drag => self.complete_drag_fetch(result),
        }
    }

    /// Complete the first fetch of a clipboard, and request the conversion of the next one.
    #[must_use]
    fn complete_clipboard_fetch(
        &mut self,
        clipboard: ClipboardSelectionType,
        result: io::Result<Vec<u8>>,
    ) -> Vec<FinishedDataTransfer> {
        let finished = self.get_clipboard_mut(clipboard).fetches.complete_front(result);
        let mut finished: Vec<_> = finished.into_iter().collect();
        finished.extend(self.start_clipboard_fetch(clipboard));
        finished
    }

    /// Request the conversion of the first fetch of a clipboard, unless it has one. Returns the
    /// fetches whose conversion could not be requested.
    #[must_use]
    fn start_clipboard_fetch(
        &mut self,
        clipboard: ClipboardSelectionType,
    ) -> Vec<FinishedDataTransfer> {
        let mut busy = self.busy_properties();
        let Self { xconn, clipboards, properties, selection_window, .. } = self;
        let selection = clipboard.selection_atom(xconn.atoms());
        clipboards[clipboard as usize].fetches.start_front(|ty| {
            request_conversion(
                xconn,
                properties,
                &mut busy,
                *selection_window,
                selection,
                ty.atom(),
            )
        })
    }

    /// The selection changed hands: abandon its fetches, and forget its types, which the
    /// transfer id that named them no longer identifies.
    #[must_use]
    fn abandon_clipboard(
        &mut self,
        clipboard: ClipboardSelectionType,
        why: &str,
    ) -> Vec<FinishedDataTransfer> {
        let abandoned = self.get_clipboard_mut(clipboard).abandon_all(why);
        self.drop_orphan_receives();
        abandoned
    }

    /// The properties that our INCR transfers in progress wait for the deletion of.
    pub(crate) fn incr_send_properties(&self) -> impl Iterator<Item = xproto::Atom> + '_ {
        self.clip_incr_send.iter().map(|send| send.property)
    }

    /// Check if we are the ones sending and the receiver deleted their property for the clipboard
    /// then update
    pub fn check_incr_send_data(&mut self, xev: &XPropertyEvent) {
        let atom = xev.atom as xproto::Atom;
        let xwindow = xev.window as xproto::Window;

        // We only care about deleted values
        if xev.state as u32 != xproto::Property::DELETE.into() {
            return;
        }

        let chunk_size = self.incr_threshold;
        self.clip_incr_send.retain_mut(|IncrState { data, property, ty, .. }| {
            if *property != atom {
                return true;
            }

            let bytes_to_take = chunk_size.min(data.len());
            let sending = &data[..bytes_to_take];
            if let Err(err) = self.xconn.change_property(
                xwindow,
                atom,
                ty.atom(),
                xproto::PropMode::REPLACE,
                sending,
            ) {
                error!("Unable to change property {} for INCR: {}", self.xconn.atom_str(atom), err);
                return false; // Delete broken properties to avoid memory leak
            }
            trace!("Written partial data {:?} to {} ", sending, self.xconn.atom_str(atom),);
            data.drain(..bytes_to_take);
            if bytes_to_take != 0 {
                return true;
            }
            trace!(
                "We are done sending {} ty {}",
                self.xconn.atom_str(*property),
                self.xconn.atom_str(ty.atom())
            );
            false
        });
    }

    /// From the TARGETS request, populate the relevant clipboard
    fn populate_targets(&mut self, clipboard: ClipboardSelectionType, data: Vec<u8>) {
        let targets_bytes = data.chunks_exact(size_of::<xproto::Atom>());
        if !targets_bytes.remainder().is_empty() {
            warn!("Trailing bytes in list of targets from X11");
        }
        let targets: Vec<_> = targets_bytes
            .map(|bytes| xproto::Atom::from_ne_bytes(bytes.try_into().unwrap()))
            .map(|atom| SelectionType::new(self.xconn.atoms(), atom))
            .filter(|ty| ty.hint.is_some())
            .collect();

        trace!(
            "Received available targets {:?} for clipboard {clipboard:?}",
            targets.iter().map(|t| self.xconn.atom_str(t.atom())).collect::<Vec<_>>()
        );

        let state = self.get_clipboard_mut(clipboard);
        state.targets_request = None;
        state.types = targets.into();
    }

    /// Handle an XFixes selection event: the owner of a selection changed. Returns the fetches
    /// that the change abandons.
    #[must_use]
    pub fn selection_owner_changed(
        &mut self,
        clipboard: ClipboardSelectionType,
        change: OwnerChange,
        serial: u64,
    ) -> Vec<FinishedDataTransfer> {
        match self.get_clipboard_mut(clipboard).owner_change_action(change, serial) {
            OwnerChangeAction::Ignore => Vec::new(),
            OwnerChangeAction::AbandonPrevious => {
                let mut abandoned = self.get_clipboard_mut(clipboard).abandon_previous_fetches();
                self.drop_orphan_receives();
                abandoned.extend(self.start_clipboard_fetch(clipboard));
                abandoned
            },
            OwnerChangeAction::AbandonAll { request_targets: false } => {
                self.abandon_clipboard(clipboard, "the selection owner went away")
            },
            OwnerChangeAction::AbandonAll { request_targets: true } => {
                let abandoned = self.abandon_clipboard(clipboard, "the selection changed hands");
                if let Err(e) = self.request_targets(clipboard) {
                    error!("Unable to request TARGETS for {clipboard:?}: {e}")
                }
                abandoned
            },
        }
    }

    /// Handle a `SelectionClear`, generated once the server had processed our request
    /// `event_serial`: another client took a selection we owned. Returns the fetches that the
    /// change abandons.
    #[must_use]
    pub fn selection_cleared(
        &mut self,
        clipboard: ClipboardSelectionType,
        event_serial: u64,
    ) -> Vec<FinishedDataTransfer> {
        if !self.get_clipboard_mut(clipboard).clear(event_serial) {
            debug!("Ignoring a SelectionClear of {clipboard:?} that predates our contents");
            return Vec::new();
        }
        trace!("Cleared owned data for {clipboard:?}");
        self.clip_incr_send.retain(|send| send.clipboard != clipboard);
        self.abandon_clipboard(clipboard, "the selection changed hands")
    }

    /// Request the list of types on offer in a selection.
    fn request_targets(&mut self, clipboard: ClipboardSelectionType) -> Result<(), X11Error> {
        let mut busy = self.busy_properties();
        let atoms = self.xconn.atoms();
        let request = request_conversion(
            &self.xconn,
            &mut self.properties,
            &mut busy,
            self.selection_window,
            clipboard.selection_atom(atoms),
            atoms[TARGETS],
        )?;
        self.get_clipboard_mut(clipboard).targets_request = Some(request);
        Ok(())
    }

    /// Ask the clipboard manager, if there is one, to save the clipboard that this application
    /// owns, once per contents (freedesktop clipboard manager specification). Returns the
    /// hand-over, whose reply is to be waited for while serving the manager's requests.
    pub(crate) fn start_clipboard_hand_off(&mut self) -> Option<ClipboardHandOff> {
        let clipboard = ClipboardSelectionType::Clipboard;
        let contents =
            self.get_clipboard(clipboard).contents_to_hand_off(self.clipboard_handed_off)?;

        let atoms = self.xconn.atoms();
        // Another client may have taken the clipboard without our having seen it yet.
        let clipboard_owner = self.xconn.selection_owner(atoms[CLIPBOARD]).ok().flatten();
        let manager = self.xconn.selection_owner(atoms[CLIPBOARD_MANAGER]).ok().flatten();
        if !hand_off_proceeds(clipboard_owner, self.selection_window, manager) {
            return None;
        }
        // Once per contents that a manager is there for, whatever comes of it.
        self.clipboard_handed_off = Some(contents);

        // The targets to save are listed in a property of the requestor.
        let ReplyPlan::Targets(advertised) = self.get_clipboard(clipboard).plan_conversion(
            atoms,
            clipboard,
            atoms[TARGETS],
            self.incr_threshold,
        ) else {
            return None;
        };
        let not_data = [atoms[TARGETS], atoms[TIMESTAMP], atoms[MULTIPLE], atoms[SAVE_TARGETS]];
        let targets = save_targets_list(&advertised, &not_data);
        match self.xconn.change_property(
            self.selection_window,
            atoms[WINIT_SAVE_TARGETS],
            xproto::AtomEnum::ATOM.into(),
            xproto::PropMode::REPLACE,
            &targets,
        ) {
            Ok(cookie) => cookie.ignore_error(),
            Err(err) => {
                warn!("Unable to list the clipboard targets to save: {err}");
                return None;
            },
        }

        // A manager whose window is destroyed or whose client exits will never answer.
        let conn = self.xconn.xcb_connection();
        if let Ok(cookie) = conn.xfixes_select_selection_input(
            self.xconn.default_root().root,
            atoms[CLIPBOARD_MANAGER],
            xfixes::SelectionEventMask::SELECTION_WINDOW_DESTROY
                | xfixes::SelectionEventMask::SELECTION_CLIENT_CLOSE,
        ) {
            cookie.ignore_error();
        }

        if let Err(err) = self.xconn.convert_selection(
            self.selection_window,
            atoms[CLIPBOARD_MANAGER],
            atoms[SAVE_TARGETS],
            atoms[WINIT_SAVE_TARGETS],
        ) {
            warn!("Unable to ask the clipboard manager to save the clipboard: {err}");
            return None;
        }

        Some(ClipboardHandOff {
            requestor: self.selection_window,
            manager: atoms[CLIPBOARD_MANAGER],
            save_targets: atoms[SAVE_TARGETS],
            deadline: Instant::now() + CLIPBOARD_MANAGER_TIMEOUT,
        })
    }

    pub fn request_clipboard_read(&mut self, clipboard: ClipboardSelectionType) -> DataTransferId {
        let clipboard_state = self.get_clipboard(clipboard);
        // TODO: skip the X11 protocol when we own the clipboard.
        if clipboard_state.owned_data.is_some() {
            trace!("Clipboard is owned by current application");
        }

        clipboard_state.transfer_serial
    }

    /// Update the internal cache of the clipboard contents
    pub fn set_clipboard(
        &mut self,
        send_data: Box<dyn DataTransferSend>,
        clipboard: ClipboardSelectionType,
    ) {
        let selection = clipboard.selection_atom(self.xconn.atoms());
        let sequence = self.xconn.set_selection_owner(self.selection_window, selection);
        self.get_clipboard_mut(clipboard).publish(send_data, sequence);
    }

    pub(crate) fn fetch_dnd_data_transfer(
        &mut self,
        type_: &dyn TransferType,
    ) -> Result<AsyncRequestSerial, TransferError> {
        let ty = type_
            .cast_ref::<SelectionType>()
            .or_else(|| self.find_type_by_hint(type_.hint()?))
            .cloned()
            .ok_or(TransferError::Failed)?;

        let Some(state) = self.state.as_ref() else {
            return Err(TransferError::Failed);
        };

        // The first fetch's conversion is requested now, so that a failure to request it is
        // returned rather than delivered. A later fetch waits for the ones before it. The drop
        // target converts the selection into a property on its own window.
        let request = if state.fetches.is_empty() {
            let mut busy = self.busy_properties();
            let request = request_conversion(
                &self.xconn,
                &mut self.properties,
                &mut busy,
                state.target_window,
                self.xconn.atoms()[XdndSelection],
                ty.atom(),
            )
            .map_err(|err| os_error!(format!("could not convert the selection: {err}")))?;
            Some(request)
        } else {
            None
        };

        let Some(state) = self.state.as_mut() else {
            return Err(TransferError::Failed);
        };
        let serial = AsyncRequestSerial::get();
        let window = crate::event_loop::mkwid(state.target_window);
        let latin1 = ty.atom() == self.xconn.atoms()[STRING];
        let id = state.transfer_id;
        state.fetches.push(PendingFetch { id, serial, ty, window, request, latin1 });

        Ok(serial)
    }

    /// Handle a `SelectionNotify` for `XdndSelection`: the reply to a conversion of the dragged
    /// data. Returns the fetches it completes.
    #[must_use]
    pub fn drag_reply(&mut self, xev: &XSelectionEvent) -> Vec<FinishedDataTransfer> {
        let requestor = xev.requestor as xproto::Window;
        let property = xev.property as xproto::Atom;
        let target = xev.target as xproto::Atom;

        let answered = self.state.as_ref().is_some_and(|state| {
            state.target_window == requestor
                && state.fetches.front_request().is_some_and(|(ty, request)| {
                    let expected = |target| target == ty.atom();
                    request.answered_by(expected, property, target, xev.serial as u64)
                })
        });
        if !answered {
            self.discard_stale_reply(requestor, property);
            return Vec::new();
        }

        let incr = self.xconn.atoms()[INCR];
        match reply_outcome(property, incr, || self.read_reply(requestor, property)) {
            ReplyOutcome::Incremental => {
                self.clip_incr_receive.push(IncrReceive {
                    window: requestor,
                    property,
                    data: Vec::new(),
                    target: IncrTarget::Drag,
                });
                Vec::new()
            },
            ReplyOutcome::Data(data) => self.complete_drag_fetch(Ok(data)),
            ReplyOutcome::Failed(err) => self.complete_drag_fetch(Err(err)),
        }
    }

    /// Complete the first fetch of the dragged data and request the conversion of the next one.
    ///
    /// The drop is settled by [`deliver_then_settle`] once the fetches are delivered.
    #[must_use]
    fn complete_drag_fetch(&mut self, result: io::Result<Vec<u8>>) -> Vec<FinishedDataTransfer> {
        let finished = self.state.as_mut().and_then(|state| state.complete_fetch(result));
        let mut finished: Vec<_> = finished.into_iter().collect();
        finished.extend(self.start_drag_fetch());
        finished
    }

    /// Request the conversion of the first fetch of the dragged data, unless it has one. Returns
    /// the fetches whose conversion could not be requested.
    #[must_use]
    fn start_drag_fetch(&mut self) -> Vec<FinishedDataTransfer> {
        let mut busy = self.busy_properties();
        let Self { xconn, state, properties, .. } = self;
        let Some(state) = state.as_mut() else {
            return Vec::new();
        };
        let (requestor, selection) = (state.target_window, xconn.atoms()[XdndSelection]);
        state.fetches.start_front(|ty| {
            request_conversion(xconn, properties, &mut busy, requestor, selection, ty.atom())
        })
    }

    /// End the drag, completing its pending fetches with `ConnectionAborted`. Returns the drag,
    /// and the fetches it abandons.
    #[must_use]
    pub fn end_drag(&mut self) -> Option<(DragState, Vec<FinishedDataTransfer>)> {
        let mut state = self.state.take()?;
        let abandoned = state.fetches.abandon_while(|_| true, "the drag ended");
        self.drop_orphan_receives();
        Some((state, abandoned))
    }

    /// Fetch the clipboard's data of type `type_`, delivering it to `window`.
    pub(crate) fn fetch_clipboard_data_transfer(
        &mut self,
        clipboard: ClipboardSelectionType,
        type_: &dyn TransferType,
        window: WindowId,
    ) -> Result<AsyncRequestSerial, TransferError> {
        let ty = type_
            .cast_ref::<SelectionType>()
            .cloned()
            .or_else(|| {
                self.get_clipboard(clipboard).find_type_by_hint(type_.hint()?, self.xconn.atoms())
            })
            .ok_or(TransferError::Failed)?;

        // The first fetch's conversion is requested now, so that a failure to request it is
        // returned rather than delivered. A later fetch waits for the ones before it.
        let request = if self.get_clipboard(clipboard).fetches.is_empty() {
            let mut busy = self.busy_properties();
            let selection = clipboard.selection_atom(self.xconn.atoms());
            let request = request_conversion(
                &self.xconn,
                &mut self.properties,
                &mut busy,
                self.selection_window,
                selection,
                ty.atom(),
            )
            .map_err(|err| os_error!(format!("could not convert the selection: {err}")))?;
            Some(request)
        } else {
            None
        };

        let latin1 = ty.atom() == self.xconn.atoms()[STRING];
        Ok(self.get_clipboard_mut(clipboard).enqueue_fetch(ty, window, request, latin1))
    }
}

impl Drop for DataTransferState {
    fn drop(&mut self) {
        if let Ok(cookie) = self.xconn.xcb_connection().destroy_window(self.selection_window) {
            cookie.ignore_error();
        }
    }
}

#[cfg(test)]
mod tests;
