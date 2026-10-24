use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString, c_void};
use std::num::NonZeroU32;
use std::ops::{BitOr, ControlFlow};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, Prefix};
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{self, AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;
use std::{fmt, io, thread};

use dpi::PhysicalPosition;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, percent_encode};
use windows_sys::Win32::Foundation::{
    CLIPBRD_E_CANT_OPEN, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS,
    DV_E_FORMATETC, E_ABORT, E_FAIL, E_INVALIDARG, E_NOINTERFACE, E_NOTIMPL, E_UNEXPECTED,
    GlobalFree, HGLOBAL, HWND, OLE_E_ADVISENOTSUPPORTED, OLE_E_WRONGCOMPOBJ, POINT, POINTL,
    RPC_E_CHANGED_MODE, S_FALSE, S_OK,
};
use windows_sys::Win32::Globalization::GetSystemDefaultLCID;
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::System::Com::{
    CoTaskMemFree, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STREAM_SEEK_CUR,
    STREAM_SEEK_SET, TYMED_ENHMF, TYMED_FILE, TYMED_GDI, TYMED_HGLOBAL, TYMED_ISTREAM,
    TYMED_MFPICT, TYMED_NULL,
};
use windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber;
use windows_sys::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows_sys::Win32::System::Ole::{
    CF_HDROP, CF_LOCALE, CF_UNICODETEXT, DROPEFFECT_COPY, DROPEFFECT_LINK, DROPEFFECT_MOVE,
    DROPEFFECT_NONE, OleDuplicateData, OleFlushClipboard, OleGetClipboard, OleInitialize,
    ReleaseStgMedium,
};
use windows_sys::Win32::System::SystemServices::{
    MK_LBUTTON, MK_MBUTTON, MK_RBUTTON, MK_XBUTTON1, MK_XBUTTON2,
};
use windows_sys::Win32::UI::Shell::{DROPFILES, DragQueryFileW, HDROP};
use windows_sys::core::{BOOL, GUID, HRESULT, IID_IUnknown};
use winit_core::data_transfer::{
    DataTransfer, DataTransferId, DataTransferSend, SendData, TransferType, TypeHint, TypedData,
};
use winit_core::error::TransferError;
use winit_core::event::WindowEvent;
use winit_core::event_loop::DndAction;
use winit_core::window::WindowId;

use crate::clipboard_format::{
    AvailableFormat, BYTE_MEDIA, ClipboardFormat, FormatListing, KnownFormat, Payload,
    RegisteredFormats, candidates, decode, encode_raw, known_formats, readable_formats,
};
use crate::definitions::{
    IDataObject, IDataObjectVtbl, IDropSource, IDropSourceVtbl, IDropTarget, IDropTargetHelper,
    IDropTargetHelperVtbl, IDropTargetVtbl, IEnumFORMATETC, IEnumFORMATETCVtbl, IID_IDataObject,
    IID_IDropSource, IID_IDropTargetHelper, IID_IEnumFORMATETC, IStream, IStreamVtbl, IUnknown,
    IUnknownVtbl,
};
use crate::event_loop::EventLoopRunner;
use crate::util;

/// An owned reference to a COM object: released on drop, and `AddRef`ed on clone.
pub(crate) struct ComPtr<T>(NonNull<T>);

impl<T> ComPtr<T> {
    /// Takes over the reference that `ptr` holds, or returns `None` if it is null.
    ///
    /// # Safety
    ///
    /// `ptr` must be null, or point to a COM object of which the caller owns a reference.
    unsafe fn from_raw(ptr: *mut T) -> Option<Self> {
        NonNull::new(ptr).map(Self)
    }

    /// Takes a new reference to the object behind `ptr`, or returns `None` if it is null.
    ///
    /// # Safety
    ///
    /// `ptr` must be null, or point to a COM object.
    pub(crate) unsafe fn from_borrowed(ptr: *mut T) -> Option<Self> {
        let this = unsafe { Self::from_raw(ptr) }?;
        unsafe { (this.vtbl::<IUnknownVtbl>().AddRef)(this.as_unknown()) };
        Some(this)
    }

    fn as_ptr(&self) -> *mut T {
        self.0.as_ptr()
    }

    fn as_unknown(&self) -> *mut IUnknown {
        self.0.as_ptr().cast()
    }

    /// The vtable of the object.
    ///
    /// # Safety
    ///
    /// The vtable of the object must start with a `V`.
    unsafe fn vtbl<V>(&self) -> &V {
        unsafe { &**self.0.as_ptr().cast::<*const V>() }
    }
}

impl<T> Clone for ComPtr<T> {
    fn clone(&self) -> Self {
        unsafe { (self.vtbl::<IUnknownVtbl>().AddRef)(self.as_unknown()) };
        Self(self.0)
    }
}

impl<T> Drop for ComPtr<T> {
    fn drop(&mut self) {
        unsafe { (self.vtbl::<IUnknownVtbl>().Release)(self.as_unknown()) };
    }
}

impl<T> fmt::Debug for ComPtr<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ComPtr").field(&self.0).finish()
    }
}

/// Opens the data object of the current clipboard contents.
fn open_clipboard() -> Result<ComPtr<IDataObject>, TransferError> {
    ensure_ole_initialized().map_err(|err| os_error!(err))?;

    let mut ptr: *mut c_void = std::ptr::null_mut();
    let hr =
        retry_while_clipboard_busy(|| unsafe { OleGetClipboard(&mut ptr) }, wait_for_clipboard);
    match unsafe { ComPtr::from_raw(ptr.cast()) } {
        Some(data_object) if hr == S_OK => Ok(data_object),
        _ => Err(os_error!(format!("OleGetClipboard failed: 0x{hr:08x}")).into()),
    }
}

/// The most listings that are read from the format enumerator of a data object.
const MAX_LISTINGS: usize = 4096;

/// Lists the formats of `data_object` with `EnumFormatEtc`, or returns the result of the call
/// that failed. A source that gives no enumerator, as one answering `OLE_S_USEREG` does, fails
/// with that result.
fn enumerate_listings(data_object: &ComPtr<IDataObject>) -> Result<Vec<FormatListing>, HRESULT> {
    let enum_format_etc = unsafe { data_object.vtbl::<IDataObjectVtbl>() }.EnumFormatEtc;
    let mut enumerator: *mut IEnumFORMATETC = std::ptr::null_mut();
    let hr = retry_while_clipboard_busy(
        || unsafe { enum_format_etc(data_object.as_ptr(), DATADIR_GET as u32, &mut enumerator) },
        wait_for_clipboard,
    );
    if hr < 0 {
        return Err(hr);
    }
    let enumerator = unsafe { ComPtr::from_raw(enumerator) }.ok_or(hr)?;
    let next = unsafe { enumerator.vtbl::<IEnumFORMATETCVtbl>() }.Next;

    let mut listings = Vec::new();
    let mut batch = [FORMATETC::default(); 16];
    while listings.len() < MAX_LISTINGS {
        let mut fetched = 0;
        let hr = unsafe {
            next(enumerator.as_ptr(), batch.len() as u32, batch.as_mut_ptr(), &mut fetched)
        };
        if hr < 0 {
            return Err(hr);
        }
        let fetched = &batch[..(fetched as usize).min(batch.len())];
        for format in fetched {
            // The enumerator allocates the target device of a listing for the caller to free.
            if !format.ptd.is_null() {
                unsafe { CoTaskMemFree(format.ptd.cast()) };
            }
        }
        listings.extend(fetched.iter().map(|format| FormatListing {
            id: format.cfFormat,
            aspect: format.dwAspect,
            index: format.lindex,
            media: format.tymed,
        }));
        if hr != S_OK || fetched.is_empty() {
            break;
        }
    }
    listings.truncate(MAX_LISTINGS);
    Ok(listings)
}

/// The formats that winit converts and that `data_object` says it can give, for a source that
/// cannot list its formats, with its `CF_LOCALE`, which sets the code page of `CF_TEXT` and
/// `CF_OEMTEXT`.
fn query_known_formats(data_object: &ComPtr<IDataObject>) -> Result<Vec<AvailableFormat>, HRESULT> {
    let query_get_data = unsafe { data_object.vtbl::<IDataObjectVtbl>() }.QueryGetData;
    let mut formats = Vec::new();
    for format in known_formats().into_iter().chain(ClipboardFormat::from_id(CF_LOCALE)) {
        let query = FORMATETC {
            cfFormat: format.id(),
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: BYTE_MEDIA,
        };
        let hr = retry_while_clipboard_busy(
            || unsafe { query_get_data(data_object.as_ptr(), &query) },
            wait_for_clipboard,
        );
        match hr {
            S_OK => formats.push(AvailableFormat { format, media: BYTE_MEDIA }),
            hr if is_call_failure(hr) => return Err(hr),
            // Any other answer is that the format is not offered.
            _ => {},
        }
    }
    Ok(formats)
}

/// Whether `hr`, the result of a call into a source, is a failure of the call rather than the
/// source's answer: the clipboard held open by another application, or a failure of COM's
/// channel to another process, such as a rejected call or a server that has gone.
fn is_call_failure(hr: HRESULT) -> bool {
    const FACILITY_RPC: u32 = 1;
    const FACILITY_WIN32: u32 = 7;
    // The Win32 errors of RPC, `RPC_S_*` and `RPC_X_*`.
    const RPC_ERRORS: std::ops::Range<u32> = 1700..2000;
    let (facility, code) = ((hr as u32 >> 16) & 0x1fff, hr as u32 & 0xffff);
    hr == CLIPBRD_E_CANT_OPEN
        || (hr < 0
            && (facility == FACILITY_RPC
                || (facility == FACILITY_WIN32 && RPC_ERRORS.contains(&code))))
}

/// The formats on offer in a data transfer, listed once and shared.
type Formats = Arc<[AvailableFormat]>;

/// Lists the formats that `data_object` offers and that can be read as bytes.
///
/// A source that cannot list its formats is asked for each format that winit converts instead.
fn offered_formats(data_object: &ComPtr<IDataObject>) -> Result<Formats, TransferError> {
    match enumerate_listings(data_object) {
        Ok(listings) => Ok(readable_formats(&listings)
            .into_iter()
            .filter_map(|(id, media)| {
                Some(AvailableFormat { format: ClipboardFormat::from_id(id)?, media })
            })
            .collect()),
        // A source that cannot list its formats says so, or gives no enumerator.
        Err(hr) if hr == E_NOTIMPL || hr >= 0 => match query_known_formats(data_object) {
            Ok(formats) => Ok(formats.into()),
            Err(hr) => Err(os_error!(format!("QueryGetData failed: 0x{hr:08x}")).into()),
        },
        // Any other failure, such as of COM's channel to the source, lists nothing, so that the
        // formats are listed again when the transfer is used again.
        Err(hr) => Err(os_error!(format!("EnumFormatEtc failed: 0x{hr:08x}")).into()),
    }
}

/// Why a format could not be read.
#[derive(Debug)]
enum ReadError {
    /// A call into the source failed.
    Call { call: &'static str, hr: HRESULT },
    /// The data is not valid data of the format.
    Invalid,
}

impl From<ReadError> for TransferError {
    fn from(err: ReadError) -> Self {
        match err {
            ReadError::Call { call, hr } => os_error!(format!("{call} failed: 0x{hr:08x}")).into(),
            ReadError::Invalid => TransferError::Failed,
        }
    }
}

/// Gets format `id` of `data_object`, in one of the storage media `media`.
fn get_medium(
    data_object: &ComPtr<IDataObject>,
    id: u16,
    media: u32,
) -> Result<OwnedStgMedium, ReadError> {
    let format = FORMATETC {
        cfFormat: id,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT,
        lindex: -1,
        tymed: media,
    };
    let get_data = unsafe { data_object.vtbl::<IDataObjectVtbl>() }.GetData;
    let mut medium = unsafe { std::mem::zeroed::<STGMEDIUM>() };
    let hr = retry_while_clipboard_busy(
        || unsafe { get_data(data_object.as_ptr(), &format, &mut medium) },
        wait_for_clipboard,
    );
    if hr < 0 {
        return Err(ReadError::Call { call: "GetData", hr });
    }
    if medium.tymed == TYMED_ISTREAM as u32 && unsafe { medium.u.pstm }.is_null() {
        // There is no stream to read, nor to release; `pUnkForRelease` is still released.
        medium.tymed = TYMED_NULL as u32;
    }
    Ok(OwnedStgMedium(medium))
}

/// The locale identifier in the `CF_LOCALE` of `data_object`, whose formats are `formats`, if it
/// offers one.
fn read_locale(data_object: &ComPtr<IDataObject>, formats: &[AvailableFormat]) -> Option<u32> {
    formats.iter().find(|available| available.format.id() == CF_LOCALE)?;
    let medium = get_medium(data_object, CF_LOCALE, TYMED_HGLOBAL as u32).ok()?;
    if medium.0.tymed != TYMED_HGLOBAL as u32 {
        return None;
    }
    let locked = LockedGlobal::lock(unsafe { medium.0.u.hGlobal })?;
    Some(u32::from_le_bytes(locked.bytes().get(..4)?.try_into().ok()?))
}

/// Reads one format of `data_object`, whose formats are `formats`, and decodes it.
fn read_format(
    data_object: &ComPtr<IDataObject>,
    available: &AvailableFormat,
    formats: &[AvailableFormat],
) -> Result<Payload, ReadError> {
    let format = &available.format;
    let locale = match format.known() {
        Some(KnownFormat::AnsiText | KnownFormat::OemText) => read_locale(data_object, formats),
        _ => None,
    };
    let mut medium = get_medium(data_object, format.id(), available.media)?;
    // A source that offers a stream may answer with an empty global memory object instead; its
    // stream is read then, if it gives one.
    if medium.0.tymed == TYMED_HGLOBAL as u32
        && available.media & TYMED_ISTREAM as u32 != 0
        && unsafe { GlobalSize(medium.0.u.hGlobal) } == 0
    {
        if let Some(stream) = get_medium(data_object, format.id(), TYMED_ISTREAM as u32)
            .ok()
            .filter(|stream| stream.0.tymed == TYMED_ISTREAM as u32)
        {
            medium = stream;
        }
    }

    let payload = match medium.0.tymed as i32 {
        TYMED_HGLOBAL if format.known() == Some(KnownFormat::FileList) => unsafe {
            read_file_list(medium.0.u.hGlobal as HDROP)
        },
        TYMED_HGLOBAL => {
            let hglobal = unsafe { medium.0.u.hGlobal };
            match unsafe { GlobalSize(hglobal) } {
                // An empty global memory object has no memory to lock.
                0 if !hglobal.is_null() => decode(format, Cow::Borrowed(&[]), locale),
                _ => LockedGlobal::lock(hglobal)
                    .and_then(|locked| decode(format, Cow::Borrowed(locked.bytes()), locale)),
            }
        },
        TYMED_ISTREAM => {
            let bytes = unsafe { read_stream(medium.0.u.pstm.cast()) }?;
            decode(format, Cow::Owned(bytes), locale)
        },
        _ => None,
    };
    payload.ok_or(ReadError::Invalid)
}

/// A locked global memory object, unlocked on drop.
struct LockedGlobal {
    hglobal: HGLOBAL,
    data: *const u8,
}

impl LockedGlobal {
    fn lock(hglobal: HGLOBAL) -> Option<Self> {
        let data = unsafe { GlobalLock(hglobal) };
        (!data.is_null()).then(|| Self { hglobal, data: data.cast() })
    }

    fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data, GlobalSize(self.hglobal)) }
    }
}

impl Drop for LockedGlobal {
    fn drop(&mut self) {
        unsafe { GlobalUnlock(self.hglobal) };
    }
}

/// Reads the data that `GetData` returned in a stream: from the start of the stream to its seek
/// pointer, or to its end when the seek pointer is at its start.
///
/// # Safety
///
/// `stream` must point to an `IStream`.
unsafe fn read_stream(stream: *mut IStream) -> Result<Vec<u8>, ReadError> {
    const CHUNK: usize = 64 * 1024;

    let vtbl = unsafe { &**stream.cast::<*const IStreamVtbl>() };
    // `GetData` leaves the seek pointer at the end of the data. A stream whose seek pointer is at
    // its start, or that cannot tell where it is, is read to its end instead.
    let mut end = 0;
    let hr = unsafe { (vtbl.Seek)(stream, 0, STREAM_SEEK_CUR, &mut end) };
    let len = (hr >= 0 && end > 0).then(|| usize::try_from(end).unwrap_or(usize::MAX));
    if len.is_some() {
        let hr = unsafe { (vtbl.Seek)(stream, 0, STREAM_SEEK_SET, std::ptr::null_mut()) };
        if hr < 0 {
            return Err(ReadError::Call { call: "IStream::Seek", hr });
        }
    }
    let mut bytes = Vec::new();
    loop {
        let wanted = len.map_or(CHUNK, |len| (len - bytes.len()).min(CHUNK));
        if wanted == 0 {
            return Ok(bytes);
        }
        bytes.reserve(wanted);
        let mut read = 0;
        let spare = bytes.spare_capacity_mut().as_mut_ptr();
        let hr = unsafe { (vtbl.Read)(stream, spare.cast(), wanted as u32, &mut read) };
        if hr < 0 {
            return Err(ReadError::Call { call: "IStream::Read", hr });
        }
        if read == 0 {
            return Ok(bytes);
        }
        let new_len = bytes.len() + (read as usize).min(wanted);
        unsafe { bytes.set_len(new_len) };
    }
}

/// Characters that must be percent-encoded inside a `file:` URI path segment.
///
/// Covers the control and reserved characters that are not allowed unescaped in a path, plus `%`
/// so that a literal percent sign in a file name round-trips. Segment separators are added
/// structurally, so `/` never appears here.
const URI_SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// Convert an absolute Windows path into a percent-encoded `file:` URI.
///
/// Disk paths become `file:///C:/dir/file`, UNC paths become `file://server/share/file`. Returns
/// `None` for anything that cannot be expressed as a `file:` URI: relative paths or components that
/// are not valid Unicode.
fn file_path_to_uri(path: &Path) -> Option<String> {
    let mut components = path.components();
    let mut uri = String::from("file://");

    match components.next()? {
        Component::Prefix(prefix) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                uri.push('/');
                uri.push(letter.to_ascii_uppercase() as char);
                uri.push(':');
            },
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                uri.extend(percent_encode(server.to_str()?.as_bytes(), URI_SEGMENT));
                uri.push('/');
                uri.extend(percent_encode(share.to_str()?.as_bytes(), URI_SEGMENT));
            },
            _ => return None,
        },
        _ => return None,
    }

    // The prefix is followed by the root separator and then the path segments.
    if !matches!(components.next(), Some(Component::RootDir)) {
        return None;
    }
    for component in components {
        let Component::Normal(segment) = component else { return None };
        uri.push('/');
        uri.extend(percent_encode(segment.to_str()?.as_bytes(), URI_SEGMENT));
    }

    Some(uri)
}

/// Convert a `file:` URI back into a Windows path, percent-decoding each segment.
///
/// Inverse of [`file_path_to_uri`]. Returns `None` if the string is not a `file:` URI describing an
/// absolute Windows path.
fn uri_to_file_path(uri: &str) -> Option<PathBuf> {
    fn decode(segment: &str) -> Option<Cow<'_, str>> {
        percent_decode_str(segment).decode_utf8().ok()
    }

    let rest = uri.strip_prefix("file://")?;
    // `split_once` drops the separator, so `path` never keeps a leading slash.
    let (host, path) = rest.split_once('/').unwrap_or((rest, ""));

    let mut out = String::new();
    if host.is_empty() {
        // Local path serialized as `/C:/dir/file`.
        let mut segments = path.split('/');
        let drive = decode(segments.next()?)?;
        let bytes = drive.as_bytes();
        if bytes.len() != 2 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
            return None;
        }
        out.push_str(&drive);
        for segment in segments {
            out.push('\\');
            out.push_str(&decode(segment)?);
        }
    } else {
        // UNC path serialized as `//server/share/file`.
        out.push_str("\\\\");
        out.push_str(&decode(host)?);
        for segment in path.split('/').filter(|s| !s.is_empty()) {
            out.push('\\');
            out.push_str(&decode(segment)?);
        }
    }

    Some(PathBuf::from(out))
}

/// Reads the files of a `CF_HDROP` as `file:` URIs, or returns `None` if it lists none.
///
/// # Safety
///
/// `hdrop` must be the global memory object of a `CF_HDROP`.
unsafe fn read_file_list(hdrop: HDROP) -> Option<Payload> {
    // The second parameter (0xFFFFFFFF) instructs the function to return the item count.
    let item_count = unsafe { DragQueryFileW(hdrop, 0xffff_ffff, std::ptr::null_mut(), 0) };

    let mut paths = Vec::with_capacity(item_count as usize);
    for i in 0..item_count {
        // Query the path length (excluding the NUL terminator), reserve room for it plus the
        // terminator, then copy. `set_len` uses the count actually written, so a short copy can
        // never expose uninitialized memory.
        let character_count = unsafe { DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0) } as usize;

        let mut path_buf = Vec::<u16>::with_capacity(character_count + 1);
        let copied =
            unsafe { DragQueryFileW(hdrop, i, path_buf.as_mut_ptr(), character_count as u32 + 1) }
                as usize;
        unsafe { path_buf.set_len(copied) };

        let path = PathBuf::from(OsString::from_wide(&path_buf));

        paths.push(file_path_to_uri(&path).unwrap_or_else(|| path.to_string_lossy().into_owned()));
    }

    (!paths.is_empty()).then_some(Payload::Uris(paths))
}

/// The formats on offer in a data transfer.
#[derive(Debug)]
pub(crate) struct WinDataTransfer {
    formats: Formats,
}

impl WinDataTransfer {
    pub(crate) fn new(formats: Formats) -> Self {
        Self { formats }
    }
}

impl DataTransfer for WinDataTransfer {
    fn for_each_available_type<'this>(
        &'this self,
        func: &'_ mut dyn FnMut(&'this dyn TransferType) -> ControlFlow<()>,
    ) {
        for available in self.formats.iter() {
            if let ControlFlow::Break(()) = func(&available.format) {
                break;
            }
        }
    }
}

/// The data of a data transfer in one format.
#[derive(Debug)]
pub(crate) struct WinTypedData {
    format: ClipboardFormat,
    payload: Arc<Payload>,
}

impl WinTypedData {
    fn new(format: ClipboardFormat, payload: Payload) -> Self {
        Self { format, payload: Arc::new(payload) }
    }
}

impl TypedData for WinTypedData {
    fn type_(&self) -> &dyn TransferType {
        &self.format
    }

    fn try_read(&self) -> Option<Box<dyn io::BufRead>> {
        match *self.payload {
            Payload::Text(_) | Payload::Bytes(_) => {
                Some(Box::new(io::Cursor::new(PayloadBytes(self.payload.clone()))))
            },
            // A list of URIs can't be neatly expressed as a binary blob.
            Payload::Uris(_) => None,
        }
    }

    fn try_as_uris(&self) -> io::Result<Vec<String>> {
        match &*self.payload {
            Payload::Uris(uris) => Ok(uris.clone()),
            _ => Err(io::ErrorKind::InvalidData.into()),
        }
    }

    fn try_as_string(&self) -> io::Result<String> {
        match &*self.payload {
            Payload::Text(text) => Ok(text.clone()),
            _ => Err(io::ErrorKind::InvalidData.into()),
        }
    }
}

/// The bytes of a text or binary payload, shared with the readers handed out for it.
struct PayloadBytes(Arc<Payload>);

impl AsRef<[u8]> for PayloadBytes {
    fn as_ref(&self) -> &[u8] {
        match &*self.0 {
            Payload::Text(text) => text.as_bytes(),
            Payload::Bytes(bytes) => bytes,
            Payload::Uris(_) => &[],
        }
    }
}

#[repr(C)]
pub struct FileDropHandlerData {
    interface: IDropTarget,
    refcount: AtomicUsize,
    window: HWND,
    runner: Rc<EventLoopRunner>,
    send_event: Box<dyn Fn(WindowEvent)>,
    active_data_transfer_id: Option<DataTransferId>,
    // Shell drop-target helper. Lazy-init on first DragEnter; `None` means "not yet created" or
    // "creation failed and we're running without a drag image". Forwarding to this is what
    // makes the source's `IDragSourceHelper` bitmap actually render under the cursor over our
    // own window and any other helper-aware target.
    drop_target_helper: Option<NonNull<IDropTargetHelper>>,
}

pub struct FileDropHandler {
    data: *mut FileDropHandlerData,
}

#[allow(non_snake_case)]
impl FileDropHandler {
    pub(crate) fn new(
        window: HWND,
        runner: Rc<EventLoopRunner>,
        send_event: Box<dyn Fn(WindowEvent)>,
    ) -> FileDropHandler {
        let data = Box::new(FileDropHandlerData {
            interface: IDropTarget { lpVtbl: &DROP_TARGET_VTBL as *const IDropTargetVtbl },
            refcount: AtomicUsize::new(1),
            window,
            runner,
            send_event,
            active_data_transfer_id: None,
            drop_target_helper: None,
        });
        FileDropHandler { data: Box::into_raw(data) }
    }

    /// Lazy-create the shell drop-target helper. Returns `None` if creation failed; callers
    /// should treat that as "no drag image" and continue silently - failure is purely cosmetic.
    unsafe fn ensure_drop_target_helper(
        data: &mut FileDropHandlerData,
    ) -> Option<NonNull<IDropTargetHelper>> {
        use windows_sys::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
        use windows_sys::Win32::UI::Shell::CLSID_DragDropHelper;

        if let Some(helper) = data.drop_target_helper {
            return Some(helper);
        }
        let mut helper: *mut IDropTargetHelper = std::ptr::null_mut();
        let hr = unsafe {
            CoCreateInstance(
                &CLSID_DragDropHelper,
                std::ptr::null_mut(),
                CLSCTX_ALL,
                &IID_IDropTargetHelper,
                &mut helper as *mut _ as *mut _,
            )
        };
        if hr < 0 {
            return None;
        }
        let helper = NonNull::new(helper)?;
        data.drop_target_helper = Some(helper);
        Some(helper)
    }

    unsafe fn helper_vtbl(helper: NonNull<IDropTargetHelper>) -> &'static IDropTargetHelperVtbl {
        unsafe { &*(*(helper.as_ptr() as *mut *const IDropTargetHelperVtbl)) }
    }

    pub(crate) unsafe fn interface_unchecked_mut(&mut self) -> &mut IDropTarget {
        unsafe { &mut (*self.data).interface }
    }

    // Implement IUnknown
    unsafe extern "system" fn QueryInterface(
        _this: *mut IUnknown,
        _riid: *const GUID,
        _ppvObject: *mut *mut c_void,
    ) -> HRESULT {
        // This function doesn't appear to be required for an `IDropTarget`.
        // An implementation would be nice however.
        // Can't use `unimplemented` here as it's invalid to panic over an FFI boundary.
        tracing::warn!("`QueryInterface` called, but it was unimplemented");
        E_FAIL
    }

    unsafe extern "system" fn AddRef(this: *mut IUnknown) -> u32 {
        let drop_handler_data = unsafe { Self::from_interface(this) };
        let count = drop_handler_data.refcount.fetch_add(1, Ordering::Relaxed) + 1;
        count as u32
    }

    unsafe extern "system" fn Release(this: *mut IUnknown) -> u32 {
        let drop_handler = unsafe { Self::from_interface(this) };
        // Release on decrement publishes any writes made through this reference before the
        // count is observed by other threads. When we hit zero, fence with Acquire so the
        // destructor sees all writes from prior Releases on other threads - the standard
        // Arc pattern. Without the fence, dropping the box could race with reads done by
        // the last releasing thread on another core.
        let count = drop_handler.refcount.fetch_sub(1, Ordering::Release) - 1;
        if count == 0 {
            atomic::fence(Ordering::Acquire);
            // Drop any transfer still in flight (e.g. the window was destroyed mid-drag, so no
            // `DragLeave`/`Drop` ever arrived to clean it up).
            if let Some(id) = drop_handler.active_data_transfer_id.take() {
                drop_handler.runner.data_transfer().remove_drag(id);
            }
            // Release the shell drop-target helper if we created one.
            if let Some(helper) = drop_handler.drop_target_helper {
                let vtbl = unsafe { Self::helper_vtbl(helper) };
                unsafe {
                    (vtbl.parent.Release)(helper.as_ptr() as *mut IUnknown);
                }
            }
            // Destroy the underlying data
            drop(unsafe { Box::from_raw(drop_handler as *mut FileDropHandlerData) });
        }
        count as u32
    }

    unsafe extern "system" fn DragEnter(
        this: *mut IDropTarget,
        pDataObj: *const IDataObject,
        grfKeyState: u32,
        pt: POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        let Some(data_object) = (unsafe { ComPtr::from_borrowed(pDataObj as *mut IDataObject) })
        else {
            unsafe { pdwEffect.write(DROPEFFECT_NONE) };
            return E_INVALIDARG;
        };
        // A drop of a drag that this process started gets the drag's id, so that the application
        // can match the two.
        let data_transfer_id = drop_handler
            .runner
            .source_drag
            .get()
            .map_or_else(next_data_transfer_id, |info| info.id);
        drop_handler.active_data_transfer_id = Some(data_transfer_id);

        let wid = WindowId::from_raw(drop_handler.window.addr());

        drop_handler.runner.data_transfer().register_drag(data_transfer_id, wid, data_object);

        let pt_screen = POINT { x: pt.x, y: pt.y };
        let mut pt_client = pt_screen;
        unsafe {
            ScreenToClient(drop_handler.window, &mut pt_client);
        }
        let position = PhysicalPosition::new(pt_client.x as f64, pt_client.y as f64);
        (drop_handler.send_event)(WindowEvent::DragEntered {
            id: data_transfer_id,
            position: Some(position),
        });

        // Get actions after the event handler has run, so that we update it based on the user's
        // supplied info.
        let source_allowed = unsafe { pdwEffect.read() };
        let new_effect = drop_handler.runner.data_transfer().drop_effect(
            data_transfer_id,
            grfKeyState,
            source_allowed,
        );
        unsafe {
            pdwEffect.write(new_effect);
        }

        // Forward to the shell drop-target helper so any drag image attached by the source's
        // IDragSourceHelper renders the bitmap under the cursor while it's over our window.
        if let Some(helper) = unsafe { Self::ensure_drop_target_helper(drop_handler) } {
            let vtbl = unsafe { Self::helper_vtbl(helper) };
            unsafe {
                (vtbl.DragEnter)(
                    helper.as_ptr(),
                    drop_handler.window,
                    pDataObj as *mut IDataObject,
                    &pt_screen,
                    *pdwEffect,
                );
            }
        }

        S_OK
    }

    unsafe extern "system" fn DragOver(
        this: *mut IDropTarget,
        grfKeyState: u32,
        pt: POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        let Some(data_transfer_id) = drop_handler.active_data_transfer_id else {
            unsafe {
                pdwEffect.write(DROPEFFECT_NONE);
            }

            return E_ABORT;
        };

        let effects = unsafe { pdwEffect.read() };

        let pt_screen = POINT { x: pt.x, y: pt.y };
        let mut pt_client = pt_screen;
        unsafe {
            ScreenToClient(drop_handler.window, &mut pt_client);
        }
        let position = PhysicalPosition::new(pt_client.x as f64, pt_client.y as f64);

        let proposed_action =
            drop_handler.runner.data_transfer().proposed_dnd_action(data_transfer_id, effects);

        (drop_handler.send_event)(WindowEvent::DragPosition {
            id: data_transfer_id,
            position,
            proposed_action,
        });

        // Get actions after the event handler has run, so that we update it based on the user's
        // supplied info.
        let new_effect =
            drop_handler.runner.data_transfer().drop_effect(data_transfer_id, grfKeyState, effects);
        unsafe {
            pdwEffect.write(new_effect);
        }

        if let Some(helper) = drop_handler.drop_target_helper {
            let vtbl = unsafe { Self::helper_vtbl(helper) };
            unsafe { (vtbl.DragOver)(helper.as_ptr(), &pt_screen, new_effect) };
        }

        S_OK
    }

    unsafe extern "system" fn DragLeave(this: *mut IDropTarget) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        let Some(data_transfer_id) = drop_handler.active_data_transfer_id.take() else {
            return E_ABORT;
        };

        (drop_handler.send_event)(WindowEvent::DragLeft { id: data_transfer_id });
        drop_handler.runner.data_transfer().remove_drag(data_transfer_id);

        if let Some(helper) = drop_handler.drop_target_helper {
            let vtbl = unsafe { Self::helper_vtbl(helper) };
            unsafe { (vtbl.DragLeave)(helper.as_ptr()) };
        }

        S_OK
    }

    unsafe extern "system" fn Drop(
        this: *mut IDropTarget,
        pDataObj: *const IDataObject,
        grfKeyState: u32,
        pt: POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        let Some(data_transfer_id) = drop_handler.active_data_transfer_id.take() else {
            unsafe {
                *pdwEffect = DROPEFFECT_NONE;
            }

            return E_ABORT;
        };

        let effects = unsafe { pdwEffect.read() };
        let proposed_action =
            drop_handler.runner.data_transfer().proposed_dnd_action(data_transfer_id, effects);

        let pt_screen = POINT { x: pt.x, y: pt.y };
        let mut pt_client = pt_screen;
        unsafe {
            ScreenToClient(drop_handler.window, &mut pt_client);
        }
        let pt = pt_client;
        let position = PhysicalPosition::new(pt.x as f64, pt.y as f64);

        (drop_handler.send_event)(WindowEvent::DragPosition {
            id: data_transfer_id,
            position,
            proposed_action,
        });

        // Get actions after the event handler has run, so that we update it based on the user's
        // supplied info.
        let transfers = drop_handler.runner.data_transfer();
        let proposed_action = transfers.proposed_dnd_action(data_transfer_id, effects);

        // Negotiate the effect first so we can pick the right outgoing event. If the app
        // rejected the drop (e.g. via `set_valid_actions(none())`), `pick_effect` returns
        // `DROPEFFECT_NONE`; in that case OLE reports back `effect_out == DROPEFFECT_NONE` to
        // the source, so the matching observer-facing event is `DragLeft`, not `DragDropped` -
        // otherwise target and source see contradictory outcomes.
        let effect = transfers.drop_effect(data_transfer_id, grfKeyState, effects);
        let event = if effect == DROPEFFECT_NONE {
            WindowEvent::DragLeft { id: data_transfer_id }
        } else {
            WindowEvent::DragDropped { id: data_transfer_id, proposed_action }
        };
        unsafe {
            *pdwEffect = effect;
        }
        // The handler of `DragDropped` reads the data, which uses the transfer state, so none of
        // it is borrowed here.
        (drop_handler.send_event)(event);

        if let Some(helper) = drop_handler.drop_target_helper {
            let vtbl = unsafe { Self::helper_vtbl(helper) };
            unsafe {
                (vtbl.Drop)(helper.as_ptr(), pDataObj as *mut IDataObject, &pt_screen, effect);
            }
        }

        // The event was dispatched before `send_event` returned, and its handler has read the
        // data, unless a handler was running, which is when the drop came from a modal loop inside
        // it: the event was buffered then, and the drag is released once the buffered events have
        // been dispatched.
        if drop_handler.runner.should_buffer() {
            drop_handler.runner.defer_drag_cleanup(data_transfer_id);
        } else {
            transfers.remove_drag(data_transfer_id);
        }

        S_OK
    }

    unsafe fn from_interface<'a, InterfaceT>(this: *mut InterfaceT) -> &'a mut FileDropHandlerData {
        unsafe { &mut *(this as *mut _) }
    }
}

impl Drop for FileDropHandler {
    fn drop(&mut self) {
        unsafe {
            FileDropHandler::Release(self.data as *mut IUnknown);
        }
    }
}

static DROP_TARGET_VTBL: IDropTargetVtbl = IDropTargetVtbl {
    parent: IUnknownVtbl {
        QueryInterface: FileDropHandler::QueryInterface,
        AddRef: FileDropHandler::AddRef,
        Release: FileDropHandler::Release,
    },
    DragEnter: FileDropHandler::DragEnter,
    DragOver: FileDropHandler::DragOver,
    DragLeave: FileDropHandler::DragLeave,
    Drop: FileDropHandler::Drop,
};

pub(crate) type DropEffect = u32;

/// Map the app's [`DndAction`] to the win32 `DROPEFFECT_*` value.
pub(crate) fn dnd_action_to_dropeffect_mask(action: DndAction) -> DropEffect {
    match action {
        DndAction::Move => DROPEFFECT_MOVE,
        DndAction::Copy => DROPEFFECT_COPY,
        DndAction::Link => DROPEFFECT_LINK,
        _ => DROPEFFECT_NONE,
    }
}

/// Map the app's [`DndAction`]s to the win32 `DROPEFFECT_*` bitmask.
pub(crate) fn dnd_actions_to_dropeffect_mask(actions: &[DndAction]) -> DropEffect {
    actions
        .iter()
        .copied()
        .map(dnd_action_to_dropeffect_mask)
        .fold(DropEffect::default(), BitOr::bitor)
}

pub(crate) fn drop_effect_to_dnd_action(effect: DropEffect) -> Option<DndAction> {
    match effect {
        DROPEFFECT_MOVE => Some(DndAction::Move),
        DROPEFFECT_COPY => Some(DndAction::Copy),
        DROPEFFECT_LINK => Some(DndAction::Link),
        _ => None,
    }
}

// Intersect the app's valid actions with the source's allowed effects, honoring Ctrl/Shift.
fn pick_effect(actions: &[DndAction], key_state: u32, source_allowed: u32) -> u32 {
    const MK_SHIFT: u32 = 0x0004;
    const MK_CONTROL: u32 = 0x0008;

    // TODO: On macOS, option will disable copy (so if copy has higher precedence than move you can
    // select move via option). Is alt expected to act similarly on Windows?
    let mut allowed = dnd_actions_to_dropeffect_mask(actions) & source_allowed;
    if allowed == 0 {
        return DROPEFFECT_NONE;
    }

    // If holding a modifier would result in no valid values, ignore
    // Need to use filter instead of if-let chains for 1.86 compatibility.
    if let Some(new_allowed) =
        NonZeroU32::new(allowed & !DROPEFFECT_MOVE).filter(|_| key_state & MK_CONTROL != 0)
    {
        allowed = new_allowed.get();
    }

    // If holding a modifier would result in no valid values, ignore
    // Need to use filter instead of if-let chains for 1.86 compatibility.
    if let Some(new_allowed) =
        NonZeroU32::new(allowed & !DROPEFFECT_COPY).filter(|_| key_state & MK_SHIFT != 0)
    {
        allowed = new_allowed.get();
    }

    for action in actions {
        let effect = dnd_action_to_dropeffect_mask(*action);
        if (allowed & effect) != 0 {
            return effect;
        }
    }

    DROPEFFECT_NONE
}

// ============================================================================
// Source side: providing data and controlling a `DoDragDrop` session.
// ============================================================================

/// Mint a unique [`DataTransferId`] for either a target-side `DragEnter`, a source-side
/// `start_drag`, or the clipboard, so they never collide.
pub(crate) fn next_data_transfer_id() -> DataTransferId {
    static COUNTER: AtomicI64 = AtomicI64::new(0);
    DataTransferId::from_raw(COUNTER.fetch_add(1, Ordering::Relaxed))
}

#[derive(Debug, Copy, Clone)]
pub(crate) struct OleInitError(HRESULT);

impl fmt::Display for OleInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            RPC_E_CHANGED_MODE => write!(
                f,
                "OleInitialize failed: `RPC_E_CHANGED_MODE`. Make sure other crates are not using \
                 multithreaded COM library on the same thread or disable drag-and-drop and \
                 clipboard support."
            ),
            OLE_E_WRONGCOMPOBJ => write!(f, "OleInitialize failed: `OLE_E_WRONGCOMPOBJ`"),
            hr => write!(f, "OleInitialize failed: 0x{hr:08x}"),
        }
    }
}

impl std::error::Error for OleInitError {}

thread_local! {
    static OLE_INITIALIZED: Cell<bool> = const { Cell::new(false) };
}

pub(crate) fn ensure_ole_initialized() -> Result<(), OleInitError> {
    if OLE_INITIALIZED.get() {
        return Ok(());
    }

    let hr = unsafe { OleInitialize(std::ptr::null_mut()) };
    // `S_FALSE` means OLE was already initialized on this thread.
    if hr != S_OK && hr != S_FALSE {
        return Err(OleInitError(hr));
    }

    OLE_INITIALIZED.set(true);
    Ok(())
}

/// How many times an OLE clipboard call is made while another process holds the clipboard open.
const CLIPBOARD_ATTEMPTS: usize = 5;

/// Makes `attempt`, an OLE clipboard call, until it returns anything but `CLIPBRD_E_CANT_OPEN`,
/// at most [`CLIPBOARD_ATTEMPTS`] times, calling `wait` between two attempts.
///
/// The OLE clipboard functions open the clipboard, and fail with `CLIPBRD_E_CANT_OPEN` while
/// another process has it open, as clipboard managers and remote desktop clients briefly do
/// whenever it changes. Calls are therefore retried for a short while before failing.
pub(crate) fn retry_while_clipboard_busy(
    mut attempt: impl FnMut() -> HRESULT,
    mut wait: impl FnMut(),
) -> HRESULT {
    let mut hr = attempt();
    for _ in 1..CLIPBOARD_ATTEMPTS {
        if hr != CLIPBRD_E_CANT_OPEN {
            break;
        }
        wait();
        hr = attempt();
    }
    hr
}

/// The wait between two attempts of [`retry_while_clipboard_busy`].
pub(crate) fn wait_for_clipboard() {
    thread::sleep(Duration::from_millis(10));
}

/// State for the single drag-and-drop transfer currently in flight (OLE guarantees at most one
/// active drag per process).
#[derive(Debug)]
struct DragState {
    id: DataTransferId,
    window_id: WindowId,
    /// The drop's data object, shared through an `Rc`: a use of the drag takes a share while the
    /// state is borrowed, which, unlike `ComPtr::clone`, makes no call into the data object.
    data_object: Rc<ComPtr<IDataObject>>,
    /// The formats on offer, listed on first use.
    formats: Option<Formats>,
    actions: Vec<DndAction>,
}

#[derive(Debug)]
struct ClipboardState {
    sequence: u32,
    id: DataTransferId,
    /// The formats on offer, listed on first use.
    formats: Option<Formats>,
}

impl ClipboardState {
    fn is_current(&self, id: DataTransferId) -> bool {
        self.id == id && self.sequence == unsafe { GetClipboardSequenceNumber() }
    }
}

/// Where the data of a transfer comes from.
enum Source {
    Drag(Rc<ComPtr<IDataObject>>),
    Clipboard,
}

impl Source {
    fn open(self) -> Result<Rc<ComPtr<IDataObject>>, TransferError> {
        match self {
            Self::Drag(data_object) => Ok(data_object),
            // The clipboard's data object stands for whatever the clipboard holds when it is
            // used, so it is opened for each use instead of being kept.
            Self::Clipboard => open_clipboard().map(Rc::new),
        }
    }
}

/// The state of all in-progress data transfer operations (drag-and-drop and clipboard).
///
/// A call into another process's data object runs a modal loop that dispatches messages, which
/// may use this state again. So the state is never borrowed while OLE runs: a data object and
/// the formats are taken out of it first, and a data object that is dropped from it is released
/// once the borrow has ended.
#[derive(Debug, Default)]
pub(crate) struct DataTransferState {
    drag: RefCell<Option<DragState>>,
    clipboard: RefCell<Option<ClipboardState>>,
    owns_clipboard: Cell<bool>,
}

impl DataTransferState {
    pub(crate) fn register_drag(
        &self,
        id: DataTransferId,
        window_id: WindowId,
        data_object: ComPtr<IDataObject>,
    ) {
        let data_object = Rc::new(data_object);
        let drag = DragState { id, window_id, data_object, formats: None, actions: Vec::new() };
        let replaced = self.drag.replace(Some(drag));
        drop(replaced);
    }

    pub(crate) fn remove_drag(&self, id: DataTransferId) {
        let removed = self.drag.borrow_mut().take_if(|drag| drag.id == id);
        drop(removed);
    }

    /// The effect of dropping drag `id` with the modifier keys `key_state` held, when its source
    /// allows the effects `source_allowed`: the effect of the actions the application accepts.
    pub(crate) fn drop_effect(
        &self,
        id: DataTransferId,
        key_state: u32,
        source_allowed: DropEffect,
    ) -> DropEffect {
        let drag = self.drag.borrow();
        let actions =
            drag.as_ref().filter(|drag| drag.id == id).map_or(&[][..], |drag| &drag.actions);
        pick_effect(actions, key_state, source_allowed)
    }

    pub(crate) fn set_valid_dnd_actions(&self, id: DataTransferId, actions: &[DndAction]) -> bool {
        match &mut *self.drag.borrow_mut() {
            Some(state) if state.id == id => {
                state.actions.clear();
                state.actions.extend_from_slice(actions);
                true
            },
            _ => false,
        }
    }

    pub(crate) fn proposed_dnd_action(
        &self,
        id: DataTransferId,
        effects: DropEffect,
    ) -> Option<DndAction> {
        let drag = self.drag.borrow();
        let actions =
            drag.as_ref().filter(|drag| drag.id == id).map_or(&[][..], |drag| &drag.actions);
        actions.iter().copied().find(|action| {
            let effect = match action {
                DndAction::Move => DROPEFFECT_MOVE,
                DndAction::Copy => DROPEFFECT_COPY,
                DndAction::Link => DROPEFFECT_LINK,
                _ => return false,
            };

            (effects & effect) != 0
        })
    }

    pub(crate) fn clipboard_transfer_id(&self) -> DataTransferId {
        let sequence = unsafe { GetClipboardSequenceNumber() };
        let mut clipboard = self.clipboard.borrow_mut();
        clipboard.take_if(|state| state.sequence != sequence);
        clipboard
            .get_or_insert_with(|| ClipboardState {
                sequence,
                id: next_data_transfer_id(),
                formats: None,
            })
            .id
    }

    /// The window to deliver the data of transfer `id` to: the window of a drag, or `None` for
    /// the clipboard, which belongs to no window.
    pub(crate) fn transfer_window(
        &self,
        id: DataTransferId,
    ) -> Result<Option<WindowId>, TransferError> {
        if let Some(drag) = self.drag.borrow().as_ref().filter(|drag| drag.id == id) {
            return Ok(Some(drag.window_id));
        }
        match &*self.clipboard.borrow() {
            Some(clipboard) if clipboard.is_current(id) => Ok(None),
            _ => Err(TransferError::UnknownTransfer(id)),
        }
    }

    /// The source of transfer `id`, with its formats if they were listed already.
    fn source(&self, id: DataTransferId) -> Result<(Source, Option<Formats>), TransferError> {
        if let Some(drag) = self.drag.borrow().as_ref().filter(|drag| drag.id == id) {
            return Ok((Source::Drag(Rc::clone(&drag.data_object)), drag.formats.clone()));
        }
        match &*self.clipboard.borrow() {
            Some(clipboard) if clipboard.is_current(id) => {
                Ok((Source::Clipboard, clipboard.formats.clone()))
            },
            _ => Err(TransferError::UnknownTransfer(id)),
        }
    }

    /// Lists the formats of `data_object`, the source of transfer `id`, and keeps them for the
    /// transfer.
    fn list_formats(
        &self,
        id: DataTransferId,
        data_object: &ComPtr<IDataObject>,
    ) -> Result<Formats, TransferError> {
        let formats = offered_formats(data_object)?;
        if let Some(drag) = self.drag.borrow_mut().as_mut().filter(|drag| drag.id == id) {
            drag.formats = Some(formats.clone());
            return Ok(formats);
        }
        match &mut *self.clipboard.borrow_mut() {
            Some(clipboard) if clipboard.is_current(id) => {
                clipboard.formats = Some(formats.clone());
                Ok(formats)
            },
            // The transfer ended while its formats were listed.
            _ => Err(TransferError::UnknownTransfer(id)),
        }
    }

    /// The formats on offer in transfer `id`.
    pub(crate) fn available_formats(&self, id: DataTransferId) -> Result<Formats, TransferError> {
        match self.source(id)? {
            (_, Some(formats)) => Ok(formats),
            (source, None) => self.list_formats(id, &*source.open()?),
        }
    }

    /// Reads `type_` from transfer `id`, from the first of its matching formats that can be read.
    ///
    /// The data is read before this returns: reading from another process waits until that
    /// process has rendered the data, and the wait cannot be interrupted.
    pub(crate) fn read(
        &self,
        id: DataTransferId,
        type_: &dyn TransferType,
    ) -> Result<WinTypedData, TransferError> {
        let (source, formats) = self.source(id)?;
        let data_object = source.open()?;
        let formats = match formats {
            Some(formats) => formats,
            None => self.list_formats(id, &data_object)?,
        };
        let mut result = Err(ReadError::Invalid);
        for available in candidates(&formats, type_) {
            result = read_format(&data_object, available, &formats)
                .map(|payload| WinTypedData::new(available.format.clone(), payload));
            // A clipboard that another application holds open cannot give another format either.
            if matches!(result, Ok(_) | Err(ReadError::Call { hr: CLIPBRD_E_CANT_OPEN, .. })) {
                break;
            }
        }
        drop(data_object);
        // The clipboard may have changed, or the drag ended, while the data was read.
        self.transfer_window(id)?;
        Ok(result?)
    }

    pub(crate) fn set_owns_clipboard(&self) {
        self.owns_clipboard.set(true);
    }

    /// Flush data to the clipboard so it outlives our message loop.
    ///
    /// See <https://learn.microsoft.com/en-us/windows/win32/api/ole2/nf-ole2-oleflushclipboard>.
    ///
    /// Flushing is retried a few times with a small delay.
    pub(crate) fn flush_clipboard_blocking(&self) {
        if !self.owns_clipboard.replace(false) {
            return;
        }

        let hr = retry_while_clipboard_busy(|| unsafe { OleFlushClipboard() }, wait_for_clipboard);
        if hr != S_OK {
            tracing::warn!("Failed to flush clipboard: 0x{hr:08x}");
        }
    }

    pub(crate) fn reset(&self) {
        let drag = self.drag.take();
        self.clipboard.take();
        drop(drag);
    }
}

fn guids_eq(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

// Returns the (cf_format, specialised hint) pairs we can lower `hint` to.
//
// Most hints map 1:1. Wildcard hints like `Image { extension_hint: None }` -
// the cross-platform way to say "any image format" - fan out to one entry per
// concrete format we support, each paired with the specialised hint so
// `data_for_type` is invoked with a concrete extension instead of `None`.
fn cf_formats_for_hint(
    hint: TypeHint,
    registered: &RegisteredFormats,
) -> Vec<(ClipboardFormat, TypeHint)> {
    fn one(format: &Option<ClipboardFormat>, hint: TypeHint) -> Vec<(ClipboardFormat, TypeHint)> {
        format.iter().map(|format| (format.clone(), hint)).collect()
    }
    match hint {
        TypeHint::Plaintext => vec![(ClipboardFormat::new(CF_UNICODETEXT, "CF_UNICODETEXT"), hint)],
        TypeHint::UriList => vec![(ClipboardFormat::new(CF_HDROP, "CF_HDROP"), hint)],
        TypeHint::Html => one(&registered.html, hint),
        TypeHint::Rtf => one(&registered.rtf, hint),
        TypeHint::Image { extension_hint: Some("png") } => one(&registered.png, hint),
        TypeHint::Image { extension_hint: None } => {
            // Fan out to every concrete image format we can produce.
            one(&registered.png, TypeHint::Image { extension_hint: Some("png") })
        },
        _ => Vec::new(),
    }
}

/// How a format that the source data object advertises is rendered.
#[derive(Debug, PartialEq)]
enum Advertised {
    /// Converted from the data of the cross-platform type `hint`, which the application supplies
    /// for that type or for the format itself.
    Converted { format: ClipboardFormat, hint: TypeHint },
    /// Sent as the data that the application supplies for the format itself.
    Raw(ClipboardFormat),
    /// `CF_LOCALE`, with this locale identifier, for raw `CF_TEXT` and `CF_OEMTEXT` when the
    /// application offers no `CF_LOCALE`: the locale of this system, in whose code pages their
    /// strings are encoded.
    Locale(u32),
}

impl Advertised {
    /// The type to ask the application's data for, which only the types that supply the format
    /// match: [`DataTransferSend::data_for_type`] serves the first offered type that matches it.
    ///
    /// The data of a converted format is asked for as the format, which a type hint matches
    /// through the format's hint and a [`ClipboardFormat`] by its id, so that another format of
    /// the same hint does not supply it. The data of a raw format is asked for as the format
    /// without its hint, so that a type hint does not supply it either. A locale that winit
    /// supplies is not asked for.
    fn request(&self) -> Option<ClipboardFormat> {
        match self {
            Self::Converted { format, .. } => Some(format.clone()),
            Self::Raw(format) => Some(format.without_hint()),
            Self::Locale(_) => None,
        }
    }
}

/// The formats to advertise for the types that `send_data` offers, each once.
///
/// A type hint is advertised as the formats it converts to. A [`ClipboardFormat`] is advertised
/// as converted from its hint when that converts to the format itself, and as raw otherwise.
fn advertised_formats(
    send_data: &dyn DataTransfer,
    registered: &RegisteredFormats,
) -> Vec<(u16, Advertised)> {
    let mut formats: Vec<(u16, Advertised)> = Vec::new();
    let mut advertise = |id: u16, advertised: Advertised| {
        if !formats.iter().any(|(advertised_id, _)| *advertised_id == id) {
            formats.push((id, advertised));
        }
    };
    send_data.for_each_available_type(&mut |ty| {
        if let Some(format) = ty.cast_ref::<ClipboardFormat>() {
            let converted = format.hint().and_then(|hint| {
                cf_formats_for_hint(hint, registered)
                    .into_iter()
                    .find(|(converted, _)| converted == format)
            });
            match converted {
                Some((format, hint)) => {
                    advertise(format.id(), Advertised::Converted { format, hint })
                },
                None => advertise(format.id(), Advertised::Raw(format.clone())),
            }
        } else if let Some(hint) = ty.hint() {
            for (format, hint) in cf_formats_for_hint(hint, registered) {
                advertise(format.id(), Advertised::Converted { format, hint });
            }
        }
        ControlFlow::Continue(())
    });
    // A reader decodes code page text in the code page of its `CF_LOCALE`, which, unless the data
    // object gives one, the system takes from the current input language.
    let code_page_text = formats.iter().any(|(_, advertised)| {
        matches!(advertised, Advertised::Raw(format)
            if matches!(format.known(), Some(KnownFormat::AnsiText | KnownFormat::OemText)))
    });
    if code_page_text && !formats.iter().any(|(id, _)| *id == CF_LOCALE) {
        formats.push((CF_LOCALE, Advertised::Locale(unsafe { GetSystemDefaultLCID() })));
    }
    formats
}

/// Duplicate a `STGMEDIUM` for handing out via `GetData` without losing the original.
///
/// Delegates to `OleDuplicateData` which knows how to clone HGLOBAL, HBITMAP, HENHMETAFILE,
/// HMETAFILEPICT and file-name mediums. Returns `None` for tymeds the shell helper doesn't use
/// (interface-based mediums like IStream / IStorage) so we never hand out an aliased pointer
/// the caller would later `Release` once we also drop ours.
unsafe fn duplicate_stgmedium(src: &STGMEDIUM, cf_format: u16) -> Option<STGMEDIUM> {
    use windows_sys::Win32::Foundation::HANDLE;

    // Pull the handle out of the union by tymed. The shell drag helper only uses HGLOBAL today,
    // but forwarding every handle-typed tymed `OleDuplicateData` understands is essentially
    // free, so do it. Interface-based tymeds (IStream / IStorage) deliberately fall through -
    // duplicating them properly requires AddRef, not OleDuplicateData.
    let tymed = src.tymed as i32;
    let handle: HANDLE = unsafe {
        if tymed == TYMED_HGLOBAL {
            src.u.hGlobal as HANDLE
        } else if tymed == TYMED_FILE {
            src.u.lpszFileName as HANDLE
        } else if tymed == TYMED_GDI {
            src.u.hBitmap as HANDLE
        } else if tymed == TYMED_MFPICT {
            src.u.hMetaFilePict as HANDLE
        } else if tymed == TYMED_ENHMF {
            src.u.hEnhMetaFile as HANDLE
        } else {
            return None;
        }
    };
    let dup = unsafe { OleDuplicateData(handle, cf_format, 0) };
    if dup.is_null() {
        return None;
    }
    let mut out: STGMEDIUM = unsafe { std::mem::zeroed() };
    out.tymed = src.tymed;
    if tymed == TYMED_HGLOBAL {
        out.u.hGlobal = dup as _;
    } else if tymed == TYMED_FILE {
        out.u.lpszFileName = dup as _;
    } else if tymed == TYMED_GDI {
        out.u.hBitmap = dup as _;
    } else if tymed == TYMED_MFPICT {
        out.u.hMetaFilePict = dup as _;
    } else if tymed == TYMED_ENHMF {
        out.u.hEnhMetaFile = dup as _;
    } else {
        // Should be unreachable - the same `tymed` matched the first chain above. Return
        // `None` rather than panic: this runs across the COM/FFI boundary, where unwinding
        // would be UB.
        return None;
    }
    Some(out)
}

fn alloc_hglobal_from(src: &[u8]) -> Option<HGLOBAL> {
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, src.len()) };
    if hglobal.is_null() {
        return None;
    }
    // An empty global memory object has no memory to lock.
    if src.is_empty() {
        return Some(hglobal);
    }
    let dst = unsafe { GlobalLock(hglobal) };
    if dst.is_null() {
        // `GlobalAlloc` succeeded but locking failed - free before bailing so we don't
        // leak the moveable handle.
        unsafe { GlobalFree(hglobal) };
        return None;
    }
    unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), dst as *mut u8, src.len()) };
    unsafe { GlobalUnlock(hglobal) };
    Some(hglobal)
}

/// Build the [HTML Clipboard Format] wire bytes from an app-supplied HTML string.
///
/// The format is a UTF-8 buffer with a small text header naming byte offsets into itself. Each
/// offset placeholder is exactly 10 zero-padded decimal digits, which fixes the header at a
/// known constant length and removes the chicken-and-egg between header length and offsets.
///
/// If the input already brackets a fragment, with `<!--StartFragment-->` followed by
/// `<!--EndFragment-->`, we trust the caller's wrapping; otherwise we wrap it in a minimal
/// `<html><body>` document with fragment markers.
///
/// [HTML Clipboard Format]: https://learn.microsoft.com/en-us/windows/win32/dataxchg/html-clipboard-format
fn build_html_clipboard_format(html: &str) -> Vec<u8> {
    const START_MARKER: &str = "<!--StartFragment-->";
    const END_MARKER: &str = "<!--EndFragment-->";
    const DOCUMENT_START: &str = "<html><body>\r\n";

    // The byte range of the fragment inside the input, when the input marks one.
    let marked_fragment = html.find(START_MARKER).and_then(|start| {
        let fragment_start = start + START_MARKER.len();
        let fragment_len = html[fragment_start..].find(END_MARKER)?;
        Some((fragment_start, fragment_start + fragment_len))
    });
    let (body, (fragment_start, fragment_end)): (Cow<'_, str>, _) = match marked_fragment {
        Some(fragment) => (Cow::Borrowed(html), fragment),
        None => {
            let fragment_start = DOCUMENT_START.len() + START_MARKER.len();
            let body =
                format!("{DOCUMENT_START}{START_MARKER}{html}{END_MARKER}\r\n</body></html>");
            (Cow::Owned(body), (fragment_start, fragment_start + html.len()))
        },
    };

    const HEADER_LEN: usize = concat!(
        "Version:0.9\r\n",
        "StartHTML:0000000000\r\n",
        "EndHTML:0000000000\r\n",
        "StartFragment:0000000000\r\n",
        "EndFragment:0000000000\r\n",
    )
    .len();

    let start_html = HEADER_LEN;
    let end_html = HEADER_LEN + body.len();
    let start_fragment = HEADER_LEN + fragment_start;
    let end_fragment = HEADER_LEN + fragment_end;

    let header = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:\
         {start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n",
    );
    debug_assert_eq!(header.len(), HEADER_LEN);

    let mut buf = Vec::with_capacity(header.len() + body.len());
    buf.extend_from_slice(header.as_bytes());
    buf.extend_from_slice(body.as_bytes());
    buf
}

/// Convert app-supplied [`SendData`] into an `HGLOBAL`-backed [`STGMEDIUM`].
///
/// Callers must have already validated that the `SendData` variant matches the on-the-wire shape
/// the requested clipboard format expects (see `variant_matches_hint` at the `GetData` call site).
unsafe fn send_data_to_stgmedium(data: SendData, hint: TypeHint) -> Option<STGMEDIUM> {
    let hglobal = match data {
        SendData::String(s) if matches!(hint, TypeHint::Html) => {
            // HTML Clipboard Format: UTF-8 with a Version/StartHTML/EndHTML/StartFragment/
            // EndFragment header. Targets parse the header before reading the wrapped HTML.
            let bytes = build_html_clipboard_format(&s);
            alloc_hglobal_from(&bytes)?
        },
        SendData::String(s) if matches!(hint, TypeHint::Rtf) => {
            // RTF is 8-bit text, terminated by a NUL like the other text formats.
            let mut bytes = s.into_bytes();
            bytes.push(0);
            alloc_hglobal_from(&bytes)?
        },
        SendData::String(s) => {
            // UTF-16 + NUL - used for `CF_UNICODETEXT` and other text-ish registered formats.
            let utf16 = util::encode_wide(&s);
            let utf16_bytes =
                unsafe { std::slice::from_raw_parts(utf16.as_ptr() as *const u8, utf16.len() * 2) };
            alloc_hglobal_from(utf16_bytes)?
        },
        SendData::Uris(paths) => {
            // CF_HDROP: `DROPFILES` header + double-NUL-terminated UTF-16 path list.
            let mut wide: Vec<u16> = Vec::new();
            for path in paths {
                let path_from_uri_owned;
                let encoded = if let Some(path_from_uri) = uri_to_file_path(&path) {
                    path_from_uri_owned = path_from_uri;
                    OsStr::new(&path_from_uri_owned).encode_wide()
                } else {
                    OsStr::new(&path).encode_wide()
                };

                wide.extend(encoded);
                wide.push(0);
            }
            wide.push(0);
            let header = DROPFILES {
                pFiles: std::mem::size_of::<DROPFILES>() as u32,
                pt: POINT { x: 0, y: 0 },
                fNC: 0,
                fWide: 1,
            };
            let total = std::mem::size_of::<DROPFILES>() + wide.len() * 2;
            let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, total) };
            if hglobal.is_null() {
                return None;
            }
            let dst = unsafe { GlobalLock(hglobal) };
            if dst.is_null() {
                unsafe { GlobalFree(hglobal) };
                return None;
            }
            unsafe {
                std::ptr::write_unaligned(dst as *mut DROPFILES, header);
                let paths_dst = (dst as *mut u8).add(std::mem::size_of::<DROPFILES>());
                std::ptr::copy_nonoverlapping(
                    wide.as_ptr() as *const u8,
                    paths_dst,
                    wide.len() * 2,
                );
                GlobalUnlock(hglobal);
            }
            hglobal
        },
        SendData::Bytes(b) => alloc_hglobal_from(&b)?,
        _ => return None,
    };
    Some(hglobal_medium(hglobal))
}

/// A storage medium that hands over `hglobal`.
fn hglobal_medium(hglobal: HGLOBAL) -> STGMEDIUM {
    let mut medium = unsafe { std::mem::zeroed::<STGMEDIUM>() };
    medium.tymed = TYMED_HGLOBAL as u32;
    medium.u.hGlobal = hglobal;
    medium
}

/// True if the `SendData` variant carries the on-the-wire shape OLE expects for `hint`'s mapped
/// clipboard format. Mismatches (e.g. `SendData::String` for a `UriList` hint) would otherwise
/// produce malformed `HGLOBAL` payloads (e.g. UTF-16 text mislabeled as `CF_HDROP`), which the
/// target would parse as wild offsets - memory corruption in the receiving process.
fn variant_matches_hint(hint: TypeHint, data: &SendData) -> bool {
    matches!(
        (hint, data),
        (TypeHint::UriList, SendData::Uris(_))
            | (TypeHint::Plaintext | TypeHint::Html | TypeHint::Rtf, SendData::String(_))
            | (TypeHint::Image { .. } | TypeHint::Audio { .. }, SendData::Bytes(_))
    )
}

// ---- IEnumFORMATETC: a cursor over a precomputed `Vec<FORMATETC>`. -----------

#[repr(C)]
#[allow(non_snake_case)]
struct IEnumFORMATETCInterface {
    lpVtbl: *const IEnumFORMATETCVtbl,
}

/// Generates the shared `IUnknown` boilerplate for our hand-rolled COM source-side objects.
///
/// Each such object is a `Box`-allocated `#[repr(C)]` struct whose first field is its COM
/// interface and which carries an `AtomicUsize` `refcount`. The thunks are identical across
/// objects apart from the concrete type and the one extra IID accepted by `QueryInterface`, so
/// the subtle refcount memory ordering lives in exactly one place.
macro_rules! com_iunknown_impl {
    ($ty:ty, $extra_iid:expr) => {
        #[allow(non_snake_case)]
        impl $ty {
            unsafe fn from_interface<'a, I>(this: *mut I) -> &'a mut Self {
                unsafe { &mut *(this as *mut _) }
            }

            unsafe extern "system" fn QueryInterface(
                this: *mut IUnknown,
                riid: *const GUID,
                ppv: *mut *mut c_void,
            ) -> HRESULT {
                let riid = unsafe { &*riid };
                if guids_eq(riid, &IID_IUnknown) || guids_eq(riid, $extra_iid) {
                    unsafe { *ppv = this as *mut c_void };
                    unsafe { Self::AddRef(this) };
                    S_OK
                } else {
                    unsafe { *ppv = std::ptr::null_mut() };
                    E_NOINTERFACE
                }
            }

            unsafe extern "system" fn AddRef(this: *mut IUnknown) -> u32 {
                let me = unsafe { Self::from_interface(this) };
                // Mere refcount bump - Relaxed is enough; the caller already holds a synchronized
                // reference to the object.
                me.refcount.fetch_add(1, Ordering::Relaxed) as u32 + 1
            }

            unsafe extern "system" fn Release(this: *mut IUnknown) -> u32 {
                let me = unsafe { Self::from_interface(this) };
                // Release on decrement publishes any writes made through this reference before the
                // count is observed by other threads. When we hit zero, fence with Acquire so the
                // destructor sees all writes from prior Releases on other threads - the standard
                // Arc pattern. Without the fence, dropping the box could race with reads done by
                // the last releasing thread on another core.
                let count = me.refcount.fetch_sub(1, Ordering::Release) - 1;
                if count == 0 {
                    atomic::fence(Ordering::Acquire);
                    drop(unsafe { Box::from_raw(me as *mut Self) });
                }
                count as u32
            }
        }
    };
}

#[repr(C)]
struct SourceFormatEnumerator {
    interface: IEnumFORMATETCInterface,
    refcount: AtomicUsize,
    formats: Vec<FORMATETC>,
    cursor: Cell<usize>,
}

com_iunknown_impl!(SourceFormatEnumerator, &IID_IEnumFORMATETC);

#[allow(non_snake_case)]
impl SourceFormatEnumerator {
    fn new_boxed(formats: Vec<FORMATETC>) -> *mut Self {
        Box::into_raw(Box::new(Self {
            interface: IEnumFORMATETCInterface {
                lpVtbl: &SOURCE_ENUM_FORMATETC_VTBL as *const IEnumFORMATETCVtbl,
            },
            refcount: AtomicUsize::new(1),
            formats,
            cursor: Cell::new(0),
        }))
    }

    unsafe extern "system" fn Next(
        this: *mut IEnumFORMATETC,
        celt: u32,
        rgelt: *mut FORMATETC,
        pcelt_fetched: *mut u32,
    ) -> HRESULT {
        let me = unsafe { Self::from_interface(this) };
        let cursor = me.cursor.get();
        let to_copy = (celt as usize).min(me.formats.len().saturating_sub(cursor));
        for i in 0..to_copy {
            unsafe { *rgelt.add(i) = me.formats[cursor + i] };
        }
        me.cursor.set(cursor + to_copy);
        if !pcelt_fetched.is_null() {
            unsafe { *pcelt_fetched = to_copy as u32 };
        }
        if to_copy < celt as usize { S_FALSE } else { S_OK }
    }

    unsafe extern "system" fn Skip(this: *mut IEnumFORMATETC, celt: u32) -> HRESULT {
        let me = unsafe { Self::from_interface(this) };
        let new_cursor = me.cursor.get().saturating_add(celt as usize);
        if new_cursor > me.formats.len() {
            me.cursor.set(me.formats.len());
            S_FALSE
        } else {
            me.cursor.set(new_cursor);
            S_OK
        }
    }

    unsafe extern "system" fn Reset(this: *mut IEnumFORMATETC) -> HRESULT {
        let me = unsafe { Self::from_interface(this) };
        me.cursor.set(0);
        S_OK
    }

    unsafe extern "system" fn Clone(
        this: *mut IEnumFORMATETC,
        ppenum: *mut *mut IEnumFORMATETC,
    ) -> HRESULT {
        let me = unsafe { Self::from_interface(this) };
        let cloned = SourceFormatEnumerator::new_boxed(me.formats.clone());
        unsafe { (*cloned).cursor.set(me.cursor.get()) };
        unsafe { *ppenum = cloned as *mut IEnumFORMATETC };
        S_OK
    }
}

static SOURCE_ENUM_FORMATETC_VTBL: IEnumFORMATETCVtbl = IEnumFORMATETCVtbl {
    parent: IUnknownVtbl {
        QueryInterface: SourceFormatEnumerator::QueryInterface,
        AddRef: SourceFormatEnumerator::AddRef,
        Release: SourceFormatEnumerator::Release,
    },
    Next: SourceFormatEnumerator::Next,
    Skip: SourceFormatEnumerator::Skip,
    Reset: SourceFormatEnumerator::Reset,
    Clone: SourceFormatEnumerator::Clone,
};

// ---- IDataObject (source) ---------------------------------------------------

#[repr(C)]
#[allow(non_snake_case)]
struct IDataObjectInterface {
    lpVtbl: *const IDataObjectVtbl,
}

/// Owning wrapper around a `STGMEDIUM` that releases the underlying handle on drop.
struct OwnedStgMedium(STGMEDIUM);

impl Drop for OwnedStgMedium {
    fn drop(&mut self) {
        unsafe { ReleaseStgMedium(&mut self.0) };
    }
}

#[repr(C)]
struct SourceDataObjectData {
    interface: IDataObjectInterface,
    refcount: AtomicUsize,
    send_data: RefCell<Box<dyn DataTransferSend>>,
    // The formats we advertise to the target.
    formats: Vec<(u16, Advertised)>,
    // Formats injected via `SetData` - primarily by `IDragSourceHelper::InitializeFromBitmap`,
    // which stores the drag image bits (CFSTR_DRAGIMAGEBITS) and related shell formats here so
    // the target-side `IDropTargetHelper` can read them back via `GetData`.
    extras: RefCell<Vec<(FORMATETC, OwnedStgMedium)>>,
}

com_iunknown_impl!(SourceDataObjectData, &IID_IDataObject);

#[allow(non_snake_case)]
impl SourceDataObjectData {
    fn new_boxed(send_data: Box<dyn DataTransferSend>) -> *mut Self {
        let formats = advertised_formats(&*send_data, RegisteredFormats::get());

        Box::into_raw(Box::new(Self {
            interface: IDataObjectInterface {
                lpVtbl: &SOURCE_DATA_OBJECT_VTBL as *const IDataObjectVtbl,
            },
            refcount: AtomicUsize::new(1),
            send_data: RefCell::new(send_data),
            formats,
            extras: RefCell::new(Vec::new()),
        }))
    }

    /// The locale identifier of the `CF_LOCALE` that this object gives: the application's, or
    /// the one that winit supplies, if either is advertised.
    fn served_locale(&self, send_data: &dyn DataTransferSend) -> Option<u32> {
        let (_, locale) = self.formats.iter().find(|(id, _)| *id == CF_LOCALE)?;
        match locale {
            Advertised::Locale(locale) => Some(*locale),
            _ => match send_data.data_for_type(&locale.request()?)? {
                SendData::Bytes(bytes) => {
                    Some(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?))
                },
                _ => None,
            },
        }
    }

    unsafe extern "system" fn GetData(
        this: *mut IDataObject,
        pformatetc_in: *const FORMATETC,
        pmedium: *mut STGMEDIUM,
    ) -> HRESULT {
        let me = unsafe { Self::from_interface(this) };
        let format = unsafe { &*pformatetc_in };
        if (format.tymed & TYMED_HGLOBAL as u32) == 0 {
            return DV_E_FORMATETC;
        }
        // Shell-helper-injected formats live in `extras`; serve those first by duplicating the
        // stored HGLOBAL so the caller can `ReleaseStgMedium` independently of our storage.
        if let Ok(extras) = me.extras.try_borrow() {
            if let Some((stored_fmt, stored)) =
                extras.iter().find(|(f, _)| f.cfFormat == format.cfFormat)
            {
                if (stored_fmt.tymed & format.tymed) != 0 {
                    if let Some(dup) = unsafe { duplicate_stgmedium(&stored.0, format.cfFormat) } {
                        unsafe { *pmedium = dup };
                        return S_OK;
                    }
                    return E_FAIL;
                }
            }
        }
        let Some((_, advertised)) = me.formats.iter().find(|(cf, _)| *cf == format.cfFormat) else {
            return DV_E_FORMATETC;
        };
        // `try_borrow_mut` rather than `borrow_mut`: `data_for_type` is the app's callback and
        // may itself reach back into this `IDataObject`. A panic across the `extern "system"`
        // boundary would be UB; return `E_UNEXPECTED` instead.
        let Ok(send_data) = me.send_data.try_borrow_mut() else {
            return E_UNEXPECTED;
        };
        let data = match advertised.request() {
            Some(request) => match send_data.data_for_type(&request) {
                Some(data) => Some(data),
                None => return DV_E_FORMATETC,
            },
            None => None,
        };
        let medium = match (advertised, data) {
            (Advertised::Converted { hint, .. }, Some(data)) => {
                if !variant_matches_hint(*hint, &data) {
                    return DV_E_FORMATETC;
                }
                unsafe { send_data_to_stgmedium(data, *hint) }
            },
            (Advertised::Raw(format), Some(data)) => {
                // A string of code page text is encoded in the code page of the locale that this
                // object gives as its `CF_LOCALE`.
                let locale = match (&data, format.known()) {
                    (SendData::String(_), Some(KnownFormat::AnsiText | KnownFormat::OemText)) => {
                        me.served_locale(&**send_data)
                    },
                    _ => None,
                };
                let Some(bytes) = encode_raw(format, data, locale) else {
                    return DV_E_FORMATETC;
                };
                alloc_hglobal_from(&bytes).map(hglobal_medium)
            },
            (Advertised::Locale(locale), _) => {
                alloc_hglobal_from(&locale.to_le_bytes()).map(hglobal_medium)
            },
            (Advertised::Converted { .. } | Advertised::Raw(_), None) => return DV_E_FORMATETC,
        };
        drop(send_data);
        let Some(medium) = medium else {
            return E_FAIL;
        };
        unsafe { *pmedium = medium };
        S_OK
    }

    unsafe extern "system" fn GetDataHere(
        _this: *mut IDataObject,
        _pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> HRESULT {
        E_NOTIMPL
    }

    unsafe extern "system" fn QueryGetData(
        this: *mut IDataObject,
        pformatetc: *const FORMATETC,
    ) -> HRESULT {
        let me = unsafe { Self::from_interface(this) };
        let format = unsafe { &*pformatetc };
        if (format.tymed & TYMED_HGLOBAL as u32) == 0 {
            return DV_E_FORMATETC;
        }
        if me.formats.iter().any(|&(cf, _)| cf == format.cfFormat) {
            return S_OK;
        }
        if let Ok(extras) = me.extras.try_borrow() {
            if extras.iter().any(|(f, _)| f.cfFormat == format.cfFormat) {
                return S_OK;
            }
        }
        S_FALSE
    }

    unsafe extern "system" fn GetCanonicalFormatEtc(
        _this: *mut IDataObject,
        _pformatetc_in: *const FORMATETC,
        _pformatetc_out: *mut FORMATETC,
    ) -> HRESULT {
        E_NOTIMPL
    }

    unsafe extern "system" fn SetData(
        this: *mut IDataObject,
        pformatetc: *const FORMATETC,
        pmedium: *const STGMEDIUM,
        f_release: BOOL,
    ) -> HRESULT {
        // Primary caller is `IDragSourceHelper::InitializeFromBitmap`, which attaches the drag
        // image bits and related shell formats to our data object. We don't interpret them -
        // just hold them so `GetData` can hand them back to `IDropTargetHelper`.
        let me = unsafe { Self::from_interface(this) };
        if pformatetc.is_null() || pmedium.is_null() {
            return E_FAIL;
        }
        let format = unsafe { *pformatetc };
        let medium = if f_release != 0 {
            // We take ownership of the passed-in medium as-is.
            unsafe { *pmedium }
        } else {
            // Caller retains ownership; we must duplicate.
            let Some(dup) = (unsafe { duplicate_stgmedium(&*pmedium, format.cfFormat) }) else {
                return E_FAIL;
            };
            dup
        };
        let Ok(mut extras) = me.extras.try_borrow_mut() else {
            if f_release != 0 {
                // We promised to take ownership but can't store - release immediately so we
                // don't leak.
                let mut m = medium;
                unsafe { ReleaseStgMedium(&mut m) };
            }
            return E_UNEXPECTED;
        };
        // Replace any earlier entry with the same format - last-write-wins matches what real
        // shell apps do and avoids growing the vec unboundedly on repeated SetData calls.
        if let Some(slot) = extras.iter_mut().find(|(f, _)| f.cfFormat == format.cfFormat) {
            slot.0 = format;
            slot.1 = OwnedStgMedium(medium);
        } else {
            extras.push((format, OwnedStgMedium(medium)));
        }
        S_OK
    }

    unsafe extern "system" fn EnumFormatEtc(
        this: *mut IDataObject,
        dw_direction: u32,
        ppenum: *mut *mut IEnumFORMATETC,
    ) -> HRESULT {
        const DATADIR_GET: u32 = 1;
        if dw_direction != DATADIR_GET {
            return E_NOTIMPL;
        }
        let me = unsafe { Self::from_interface(this) };
        let mut formats: Vec<FORMATETC> = me
            .formats
            .iter()
            .map(|&(cf, _)| FORMATETC {
                cfFormat: cf,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT,
                lindex: -1,
                tymed: TYMED_HGLOBAL as u32,
            })
            .collect();
        if let Ok(extras) = me.extras.try_borrow() {
            for (fmt, _) in extras.iter() {
                if !formats.iter().any(|f| f.cfFormat == fmt.cfFormat) {
                    formats.push(*fmt);
                }
            }
        }
        let enumerator = SourceFormatEnumerator::new_boxed(formats);
        unsafe { *ppenum = enumerator as *mut IEnumFORMATETC };
        S_OK
    }

    unsafe extern "system" fn DAdvise(
        _this: *mut IDataObject,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _adv_sink: *const crate::definitions::IAdviseSink,
        _pdw_connection: *mut u32,
    ) -> HRESULT {
        OLE_E_ADVISENOTSUPPORTED
    }

    unsafe extern "system" fn DUnadvise(_this: *mut IDataObject, _connection: u32) -> HRESULT {
        OLE_E_ADVISENOTSUPPORTED
    }

    unsafe extern "system" fn EnumDAdvise(
        _this: *mut IDataObject,
        _ppenum_advise: *const *const crate::definitions::IEnumSTATDATA,
    ) -> HRESULT {
        OLE_E_ADVISENOTSUPPORTED
    }
}

static SOURCE_DATA_OBJECT_VTBL: IDataObjectVtbl = IDataObjectVtbl {
    parent: IUnknownVtbl {
        QueryInterface: SourceDataObjectData::QueryInterface,
        AddRef: SourceDataObjectData::AddRef,
        Release: SourceDataObjectData::Release,
    },
    GetData: SourceDataObjectData::GetData,
    GetDataHere: SourceDataObjectData::GetDataHere,
    QueryGetData: SourceDataObjectData::QueryGetData,
    GetCanonicalFormatEtc: SourceDataObjectData::GetCanonicalFormatEtc,
    SetData: SourceDataObjectData::SetData,
    EnumFormatEtc: SourceDataObjectData::EnumFormatEtc,
    DAdvise: SourceDataObjectData::DAdvise,
    DUnadvise: SourceDataObjectData::DUnadvise,
    EnumDAdvise: SourceDataObjectData::EnumDAdvise,
};

pub(crate) struct SourceDataObject {
    data: *mut SourceDataObjectData,
}

impl SourceDataObject {
    pub(crate) fn new(send_data: Box<dyn DataTransferSend>) -> Self {
        Self { data: SourceDataObjectData::new_boxed(send_data) }
    }

    pub(crate) fn interface_ptr(&self) -> *mut c_void {
        self.data as *mut c_void
    }
}

impl Drop for SourceDataObject {
    fn drop(&mut self) {
        unsafe { SourceDataObjectData::Release(self.data as *mut IUnknown) };
    }
}

// ---- IDropSource ------------------------------------------------------------

#[repr(C)]
#[allow(non_snake_case)]
struct IDropSourceInterface {
    lpVtbl: *const IDropSourceVtbl,
}

#[repr(C)]
struct DropSourceData {
    interface: IDropSourceInterface,
    refcount: AtomicUsize,
}

com_iunknown_impl!(DropSourceData, &IID_IDropSource);

#[allow(non_snake_case)]
impl DropSourceData {
    fn new_boxed() -> *mut Self {
        Box::into_raw(Box::new(Self {
            interface: IDropSourceInterface { lpVtbl: &DROP_SOURCE_VTBL as *const IDropSourceVtbl },
            refcount: AtomicUsize::new(1),
        }))
    }

    unsafe extern "system" fn QueryContinueDrag(
        _this: *mut IDropSource,
        escape_pressed: BOOL,
        grf_key_state: u32,
    ) -> HRESULT {
        // Drop when no mouse button is pressed - matches Microsoft's documented
        // `IDropSource` example and covers right-button / middle-button drags as
        // well as the common left-button case. Hardcoding `MK_LBUTTON` would make
        // a right-drag (e.g. context-menu drag) never terminate by mouse release.
        const ANY_MOUSE_BUTTON: u32 =
            MK_LBUTTON | MK_RBUTTON | MK_MBUTTON | MK_XBUTTON1 | MK_XBUTTON2;
        if escape_pressed != 0 {
            return DRAGDROP_S_CANCEL;
        }
        if (grf_key_state & ANY_MOUSE_BUTTON) == 0 {
            return DRAGDROP_S_DROP;
        }
        S_OK
    }

    unsafe extern "system" fn GiveFeedback(_this: *mut IDropSource, _dw_effect: u32) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

static DROP_SOURCE_VTBL: IDropSourceVtbl = IDropSourceVtbl {
    parent: IUnknownVtbl {
        QueryInterface: DropSourceData::QueryInterface,
        AddRef: DropSourceData::AddRef,
        Release: DropSourceData::Release,
    },
    QueryContinueDrag: DropSourceData::QueryContinueDrag,
    GiveFeedback: DropSourceData::GiveFeedback,
};

pub(crate) struct DropSource {
    data: *mut DropSourceData,
}

impl DropSource {
    pub(crate) fn new() -> Self {
        Self { data: DropSourceData::new_boxed() }
    }

    pub(crate) fn interface_ptr(&self) -> *mut c_void {
        self.data as *mut c_void
    }
}

impl Drop for DropSource {
    fn drop(&mut self) {
        unsafe { DropSourceData::Release(self.data as *mut IUnknown) };
    }
}

/// Attach a drag image to `data_object` so the shell renders the app's icon under the cursor
/// during the drag instead of the default no-image cursor.
///
/// `rgba` is the icon's pixel buffer in straight RGBA8 with `width * height * 4` bytes;
/// `offset` matches `DragIcon::offset` - `(0, 0)` sits the cursor at the icon's top-left,
/// `(-w/2, -h/2)` centres the icon on the cursor. Windows expresses this as the offset from
/// the icon's upper-left to the cursor hot spot (sign inverted), so we negate when filling
/// in `SHDRAGIMAGE::ptOffset`.
///
/// Returns `Ok(())` on success. On failure the caller's data object is left untouched and the
/// drag still runs - just without a custom image. We never propagate the error: a missing drag
/// preview is purely cosmetic and shouldn't fail the whole `start_drag`.
pub(crate) unsafe fn apply_drag_image(
    data_object: *mut IDataObject,
    width: u32,
    height: u32,
    rgba: &[u8],
    offset_x: i32,
    offset_y: i32,
) -> Result<(), HRESULT> {
    use windows_sys::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, HDC,
    };
    use windows_sys::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};
    use windows_sys::Win32::UI::Shell::{CLSID_DragDropHelper, SHDRAGIMAGE};

    use crate::definitions::{IDragSourceHelper, IDragSourceHelperVtbl, IID_IDragSourceHelper};

    if width == 0 || height == 0 || rgba.len() != (width as usize) * (height as usize) * 4 {
        return Err(E_FAIL);
    }

    // Build a top-down 32bpp BGRA DIB. Top-down means negative biHeight; the shell helper
    // expects BGRA byte order (B, G, R, A) with premultiplied alpha for smooth compositing.
    let mut header: BITMAPINFO = unsafe { std::mem::zeroed() };
    header.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width as i32,
        biHeight: -(height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: width * height * 4,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };

    let mut bits_ptr: *mut c_void = std::ptr::null_mut();
    let hbitmap = unsafe {
        CreateDIBSection(
            std::ptr::null_mut::<HDC>() as HDC,
            &header,
            DIB_RGB_COLORS,
            &mut bits_ptr,
            std::ptr::null_mut(),
            0,
        )
    };
    if hbitmap.is_null() || bits_ptr.is_null() {
        return Err(E_FAIL);
    }

    // Copy RGBA -> premultiplied BGRA so the shell can render the image with smooth alpha
    // edges. Without premultiplication, the cursor preview shows a halo on every translucent
    // pixel.
    let dst = unsafe { std::slice::from_raw_parts_mut(bits_ptr as *mut u8, rgba.len()) };
    for (src_px, dst_px) in rgba.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
        let (r, g, b, a) = (src_px[0] as u32, src_px[1] as u32, src_px[2] as u32, src_px[3]);
        dst_px[0] = ((b * a as u32) / 255) as u8;
        dst_px[1] = ((g * a as u32) / 255) as u8;
        dst_px[2] = ((r * a as u32) / 255) as u8;
        dst_px[3] = a;
    }

    let mut helper: *mut IDragSourceHelper = std::ptr::null_mut();
    let hr = unsafe {
        CoCreateInstance(
            &CLSID_DragDropHelper,
            std::ptr::null_mut(),
            CLSCTX_ALL,
            &IID_IDragSourceHelper,
            &mut helper as *mut _ as *mut _,
        )
    };
    if hr < 0 || helper.is_null() {
        unsafe { DeleteObject(hbitmap as _) };
        return Err(hr);
    }

    // The helper API takes `ptOffset` as the cursor's position inside the image (positive
    // values into the image), but cross-platform `DragIcon::offset` is the icon-relative
    // offset where the cursor sits (negative values mean the icon extends up/left of the
    // cursor). Negate to translate between the two conventions.
    let sdi = SHDRAGIMAGE {
        sizeDragImage: windows_sys::Win32::Foundation::SIZE { cx: width as i32, cy: height as i32 },
        ptOffset: POINT { x: -offset_x, y: -offset_y },
        hbmpDragImage: hbitmap,
        crColorKey: 0xffff_ffff, // CLR_NONE - use the alpha channel
    };

    let vtbl = unsafe {
        &*((*(helper as *mut *mut IDragSourceHelperVtbl)) as *const IDragSourceHelperVtbl)
    };
    let init_hr = unsafe { (vtbl.InitializeFromBitmap)(helper, &sdi, data_object) };
    let release = vtbl.parent.Release;
    unsafe { release(helper as *mut IUnknown) };

    if init_hr < 0 {
        // On failure the helper did not take ownership of the bitmap - we still own it.
        unsafe { DeleteObject(hbitmap as _) };
        return Err(init_hr);
    }
    // On success the helper stores the bitmap on the data object and will delete it later;
    // do NOT call DeleteObject here.
    Ok(())
}

#[cfg(test)]
mod clipboard_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_offset(buf: &[u8], name: &str) -> usize {
        let prefix = format!("{name}:");
        let head = std::str::from_utf8(buf).unwrap();
        let pos = head.find(&prefix).unwrap() + prefix.len();
        head[pos..pos + 10].parse().unwrap()
    }

    #[test]
    fn html_clipboard_format_brackets_user_html() {
        let html = "<span><strong>Winit</strong> example</span>";
        let buf = build_html_clipboard_format(html);

        assert!(buf.starts_with(b"Version:0.9\r\n"));

        let start_html = parse_offset(&buf, "StartHTML");
        let end_html = parse_offset(&buf, "EndHTML");
        let doc = std::str::from_utf8(&buf[start_html..end_html]).unwrap();
        assert!(doc.starts_with("<html><body>"));
        assert!(doc.ends_with("</body></html>"));

        let start_fragment = parse_offset(&buf, "StartFragment");
        let end_fragment = parse_offset(&buf, "EndFragment");
        let fragment = std::str::from_utf8(&buf[start_fragment..end_fragment]).unwrap();
        assert_eq!(fragment, html);
    }

    #[test]
    fn disk_path_round_trips_through_uri() {
        let uri = file_path_to_uri(Path::new(r"C:\Users\me\my file.txt")).unwrap();
        assert_eq!(uri, "file:///C:/Users/me/my%20file.txt");
        assert_eq!(uri_to_file_path(&uri).unwrap(), PathBuf::from(r"C:\Users\me\my file.txt"));
    }

    #[test]
    fn unc_path_round_trips_through_uri() {
        let uri = file_path_to_uri(Path::new(r"\\server\share\dir\file.txt")).unwrap();
        assert_eq!(uri, "file://server/share/dir/file.txt");
        assert_eq!(uri_to_file_path(&uri).unwrap(), PathBuf::from(r"\\server\share\dir\file.txt"));
    }

    #[test]
    fn relative_path_has_no_uri() {
        assert_eq!(file_path_to_uri(Path::new(r"me\file.txt")), None);
    }

    #[test]
    fn percent_and_unicode_round_trip() {
        let uri = file_path_to_uri(Path::new(r"C:\100% café")).unwrap();
        assert_eq!(uri, "file:///C:/100%25%20caf%C3%A9");
        assert_eq!(uri_to_file_path(&uri).unwrap(), PathBuf::from(r"C:\100% café"));
    }

    #[test]
    fn html_clipboard_format_preserves_pre_wrapped() {
        let pre_wrapped =
            "<html><body><!--StartFragment--><p>hi</p><!--EndFragment--></body></html>";
        let buf = build_html_clipboard_format(pre_wrapped);

        let start_fragment = parse_offset(&buf, "StartFragment");
        let end_fragment = parse_offset(&buf, "EndFragment");
        let fragment = std::str::from_utf8(&buf[start_fragment..end_fragment]).unwrap();
        assert_eq!(fragment, "<p>hi</p>");
    }
}
