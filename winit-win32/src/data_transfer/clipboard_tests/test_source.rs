//! A data object for the tests, which offers fixed data in fixed storage media.

use std::ptr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use windows_sys::Win32::System::Com::{STREAM_SEEK_SET, TYMED_HGLOBAL, TYMED_ISTREAM};
use windows_sys::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc};
use windows_sys::Win32::UI::Shell::SHCreateMemStream;

use crate::data_transfer::*;

/// How a test source answers `EnumFormatEtc`.
#[derive(Clone, Copy, Debug)]
pub(super) enum Listing {
    /// It lists its formats.
    Formats,
    /// It fails with this result.
    Fails(HRESULT),
    /// It fails with this result the first time, and lists its formats after.
    FailsFirst(HRESULT),
    /// It returns this success code without an enumerator, as a source answering `OLE_S_USEREG`
    /// does.
    NoEnumerator(HRESULT),
}

/// The storage medium a test source gives a format in.
#[derive(Clone, Debug)]
pub(super) enum Medium {
    /// Global memory holding these bytes.
    Global(Vec<u8>),
    /// A stream holding `bytes`, with its seek pointer at `position` when it is given.
    Stream { bytes: Vec<u8>, position: u64 },
    /// `TYMED_ISTREAM` without a stream.
    NullStream,
}

impl Medium {
    fn tymed(&self) -> u32 {
        match self {
            Self::Global(_) => TYMED_HGLOBAL as u32,
            Self::Stream { .. } | Self::NullStream => TYMED_ISTREAM as u32,
        }
    }
}

/// What a test source was asked for, shared with the test.
#[derive(Debug, Default)]
pub(super) struct Calls {
    /// The formats `GetData` was asked for, in order.
    pub(super) get_data: Mutex<Vec<u16>>,
    /// How many times `EnumFormatEtc` was called.
    pub(super) enum_format_etc: AtomicUsize,
    /// Set when the data object is destroyed.
    pub(super) destroyed: AtomicBool,
}

/// A data object that offers fixed data.
pub(super) struct TestSource {
    /// The formats, each with the medium it is given in, in the order they are listed. A format
    /// in more than one medium is listed once for each.
    pub(super) formats: Vec<(u16, Medium)>,
    pub(super) listing: Listing,
    /// Results that `GetData` and `QueryGetData` return for formats instead of answering.
    pub(super) failures: Vec<(u16, HRESULT)>,
    /// Runs at the start of each `AddRef`, `Release`, `QueryGetData`, `EnumFormatEtc` and
    /// `GetData`, with the name of the call.
    pub(super) hook: Option<Box<dyn Fn(&'static str)>>,
    pub(super) calls: Arc<Calls>,
}

impl TestSource {
    pub(super) fn new(formats: Vec<(u16, Medium)>) -> Self {
        Self {
            formats,
            listing: Listing::Formats,
            failures: Vec::new(),
            hook: None,
            calls: Arc::default(),
        }
    }

    /// The data object that serves this data, of which the result holds the only reference.
    pub(super) fn into_data_object(self) -> ComPtr<IDataObject> {
        let object = Box::new(TestSourceObject {
            interface: IDataObjectInterface { lpVtbl: &TEST_SOURCE_VTBL },
            refcount: AtomicUsize::new(1),
            source: self,
        });
        unsafe { ComPtr::from_raw(Box::into_raw(object).cast()) }.unwrap()
    }
}

/// UTF-16 text with its terminating NUL, as `CF_UNICODETEXT` holds it.
pub(super) fn utf16_text(text: &str) -> Vec<u8> {
    text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

#[repr(C)]
struct TestSourceObject {
    interface: IDataObjectInterface,
    refcount: AtomicUsize,
    source: TestSource,
}

com_iunknown_impl!(TestSourceObject, &IID_IDataObject);

impl Drop for TestSourceObject {
    fn drop(&mut self) {
        self.source.calls.destroyed.store(true, Ordering::SeqCst);
    }
}

#[allow(non_snake_case)]
impl TestSourceObject {
    fn medium(&self, format: &FORMATETC) -> Option<&Medium> {
        self.source
            .formats
            .iter()
            .find(|(id, medium)| *id == format.cfFormat && format.tymed & medium.tymed() != 0)
            .map(|(_, medium)| medium)
    }

    fn run_hook(&self, call: &'static str) {
        if let Some(hook) = &self.source.hook {
            hook(call);
        }
    }

    fn failure(&self, format: &FORMATETC) -> Option<HRESULT> {
        self.source.failures.iter().find(|(id, _)| *id == format.cfFormat).map(|&(_, hr)| hr)
    }

    unsafe extern "system" fn HookedAddRef(this: *mut IUnknown) -> u32 {
        unsafe { Self::from_interface(this) }.run_hook("AddRef");
        unsafe { Self::AddRef(this) }
    }

    unsafe extern "system" fn HookedRelease(this: *mut IUnknown) -> u32 {
        unsafe { Self::from_interface(this) }.run_hook("Release");
        unsafe { Self::Release(this) }
    }

    unsafe extern "system" fn GetData(
        this: *mut IDataObject,
        pformatetc_in: *const FORMATETC,
        pmedium: *mut STGMEDIUM,
    ) -> HRESULT {
        let me = unsafe { Self::from_interface(this) };
        let format = unsafe { &*pformatetc_in };
        me.run_hook("GetData");
        me.source.calls.get_data.lock().unwrap().push(format.cfFormat);
        if let Some(hr) = me.failure(format) {
            return hr;
        }
        let Some(data) = me.medium(format) else {
            return DV_E_FORMATETC;
        };
        let mut medium = unsafe { std::mem::zeroed::<STGMEDIUM>() };
        medium.tymed = data.tymed();
        match data {
            // An object of no bytes cannot be locked, so it is not written to.
            Medium::Global(bytes) if bytes.is_empty() => {
                medium.u.hGlobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, 0) };
            },
            Medium::Global(bytes) => medium.u.hGlobal = alloc_hglobal_from(bytes).unwrap(),
            Medium::Stream { bytes, position } => {
                let stream = unsafe { SHCreateMemStream(bytes.as_ptr(), bytes.len() as u32) };
                assert!(!stream.is_null());
                let vtbl = unsafe { &**stream.cast::<*const IStreamVtbl>() };
                let position = *position as i64;
                unsafe { (vtbl.Seek)(stream.cast(), position, STREAM_SEEK_SET, ptr::null_mut()) };
                medium.u.pstm = stream;
            },
            Medium::NullStream => {},
        }
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
        me.run_hook("QueryGetData");
        match me.failure(format) {
            Some(hr) => hr,
            None if me.medium(format).is_some() => S_OK,
            None => DV_E_FORMATETC,
        }
    }

    unsafe extern "system" fn GetCanonicalFormatEtc(
        _this: *mut IDataObject,
        _pformatetc_in: *const FORMATETC,
        _pformatetc_out: *mut FORMATETC,
    ) -> HRESULT {
        E_NOTIMPL
    }

    unsafe extern "system" fn SetData(
        _this: *mut IDataObject,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _f_release: BOOL,
    ) -> HRESULT {
        E_NOTIMPL
    }

    unsafe extern "system" fn EnumFormatEtc(
        this: *mut IDataObject,
        _dw_direction: u32,
        ppenum: *mut *mut IEnumFORMATETC,
    ) -> HRESULT {
        let me = unsafe { Self::from_interface(this) };
        me.run_hook("EnumFormatEtc");
        let first = me.source.calls.enum_format_etc.fetch_add(1, Ordering::SeqCst) == 0;
        unsafe { *ppenum = ptr::null_mut() };
        match me.source.listing {
            Listing::FailsFirst(hr) if first => hr,
            Listing::Formats | Listing::FailsFirst(_) => {
                let formats = me
                    .source
                    .formats
                    .iter()
                    .map(|(id, medium)| FORMATETC {
                        cfFormat: *id,
                        ptd: ptr::null_mut(),
                        dwAspect: DVASPECT_CONTENT,
                        lindex: -1,
                        tymed: medium.tymed(),
                    })
                    .collect();
                unsafe { *ppenum = SourceFormatEnumerator::new_boxed(formats).cast() };
                S_OK
            },
            Listing::Fails(hr) | Listing::NoEnumerator(hr) => hr,
        }
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

static TEST_SOURCE_VTBL: IDataObjectVtbl = IDataObjectVtbl {
    parent: IUnknownVtbl {
        QueryInterface: TestSourceObject::QueryInterface,
        AddRef: TestSourceObject::HookedAddRef,
        Release: TestSourceObject::HookedRelease,
    },
    GetData: TestSourceObject::GetData,
    GetDataHere: TestSourceObject::GetDataHere,
    QueryGetData: TestSourceObject::QueryGetData,
    GetCanonicalFormatEtc: TestSourceObject::GetCanonicalFormatEtc,
    SetData: TestSourceObject::SetData,
    EnumFormatEtc: TestSourceObject::EnumFormatEtc,
    DAdvise: TestSourceObject::DAdvise,
    DUnadvise: TestSourceObject::DUnadvise,
    EnumDAdvise: TestSourceObject::EnumDAdvise,
};
