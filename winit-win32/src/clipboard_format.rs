//! Clipboard formats: the types of the data on the clipboard and in drag-and-drop operations.

use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::{fmt, io};

use windows_sys::Win32::System::Com::{DVASPECT_CONTENT, TYMED_HGLOBAL, TYMED_ISTREAM};
use windows_sys::Win32::System::DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW};
use windows_sys::Win32::System::Ole::{
    CF_BITMAP, CF_DIB, CF_DIBV5, CF_DIF, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT,
    CF_DSPTEXT, CF_ENHMETAFILE, CF_HDROP, CF_LOCALE, CF_METAFILEPICT, CF_OEMTEXT, CF_OWNERDISPLAY,
    CF_PALETTE, CF_PENDATA, CF_RIFF, CF_SYLK, CF_TEXT, CF_TIFF, CF_UNICODETEXT, CF_WAVE,
};
use winit_core::data_transfer::{TransferType, TypeHint};

use crate::util;

#[cfg(test)]
mod tests;

/// The first id of the formats registered with `RegisterClipboardFormatW`.
const FIRST_REGISTERED_ID: u16 = 0xc000;

/// The standard clipboard formats, named after their constants.
const STANDARD_FORMATS: &[(u16, &str)] = &[
    (CF_TEXT, "CF_TEXT"),
    (CF_BITMAP, "CF_BITMAP"),
    (CF_METAFILEPICT, "CF_METAFILEPICT"),
    (CF_SYLK, "CF_SYLK"),
    (CF_DIF, "CF_DIF"),
    (CF_TIFF, "CF_TIFF"),
    (CF_OEMTEXT, "CF_OEMTEXT"),
    (CF_DIB, "CF_DIB"),
    (CF_PALETTE, "CF_PALETTE"),
    (CF_PENDATA, "CF_PENDATA"),
    (CF_RIFF, "CF_RIFF"),
    (CF_WAVE, "CF_WAVE"),
    (CF_UNICODETEXT, "CF_UNICODETEXT"),
    (CF_ENHMETAFILE, "CF_ENHMETAFILE"),
    (CF_HDROP, "CF_HDROP"),
    (CF_LOCALE, "CF_LOCALE"),
    (CF_DIBV5, "CF_DIBV5"),
    (CF_OWNERDISPLAY, "CF_OWNERDISPLAY"),
    (CF_DSPTEXT, "CF_DSPTEXT"),
    (CF_DSPBITMAP, "CF_DSPBITMAP"),
    (CF_DSPMETAFILEPICT, "CF_DSPMETAFILEPICT"),
    (CF_DSPENHMETAFILE, "CF_DSPENHMETAFILE"),
];

/// The formats that winit converts to a cross-platform type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KnownFormat {
    /// `CF_UNICODETEXT`: UTF-16 text.
    UnicodeText,
    /// `CF_HDROP`: a list of files.
    FileList,
    /// `"PNG"`: a PNG image.
    Png,
}

/// The standard formats that winit converts.
const STANDARD_KNOWN_FORMATS: &[(u16, KnownFormat)] =
    &[(CF_UNICODETEXT, KnownFormat::UnicodeText), (CF_HDROP, KnownFormat::FileList)];

/// The registered formats that winit converts, by name.
const REGISTERED_FORMATS: &[(&str, KnownFormat)] = &[("PNG", KnownFormat::Png)];

impl KnownFormat {
    fn identify(id: u16, name: &str) -> Option<Self> {
        if id >= FIRST_REGISTERED_ID {
            // Registered names are case-insensitive.
            REGISTERED_FORMATS
                .iter()
                .find(|(registered, _)| registered.eq_ignore_ascii_case(name))
                .map(|&(_, known)| known)
        } else {
            STANDARD_KNOWN_FORMATS
                .iter()
                .find(|&&(standard, _)| standard == id)
                .map(|&(_, known)| known)
        }
    }

    fn hint(self) -> TypeHint {
        match self {
            Self::UnicodeText => TypeHint::Plaintext,
            Self::FileList => TypeHint::UriList,
            Self::Png => TypeHint::Image { extension_hint: Some("png") },
        }
    }
}

/// A clipboard format: the type of data on the clipboard or in a drag-and-drop operation.
///
/// A format is either one of the [standard formats], such as `CF_UNICODETEXT`, or a format
/// registered by name with [`RegisterClipboardFormatW`]. Formats compare equal when their ids
/// are equal. Registered names are case-insensitive, so two equal formats can spell their names
/// differently.
///
/// The [hint](TransferType::hint) of a format is the cross-platform type that winit converts its
/// data to; formats without a hint are read as their raw bytes.
///
/// # Reading
///
/// The data of a transfer is read when it is fetched:
/// [`fetch_data_transfer`](winit_core::event_loop::ActiveEventLoop::fetch_data_transfer) reads
/// it before it returns, and the
/// [`DataTransferReceived`](winit_core::event::WindowEvent::DataTransferReceived) event follows.
/// Reading from another application waits until that application has rendered the data, and the
/// wait cannot be interrupted.
///
/// [standard formats]: https://learn.microsoft.com/en-us/windows/win32/dataxchg/standard-clipboard-formats
/// [`RegisterClipboardFormatW`]: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-registerclipboardformatw
#[derive(Clone, Debug)]
pub struct ClipboardFormat {
    id: u16,
    name: Arc<str>,
    known: Option<KnownFormat>,
}

impl ClipboardFormat {
    /// The format registered with the name `name`, registering it first if no application has.
    pub fn register(name: &str) -> io::Result<Self> {
        let wide = util::encode_wide(name);
        match unsafe { RegisterClipboardFormatW(wide.as_ptr()) } {
            0 => Err(io::Error::last_os_error()),
            id => Ok(Self::new(id as u16, name)),
        }
    }

    /// The format with the id `id`: a standard format or a registered one.
    ///
    /// Returns `None` when `id` is neither, such as the id of a private format.
    pub fn from_id(id: u16) -> Option<Self> {
        if let Some(&(_, name)) = STANDARD_FORMATS.iter().find(|&&(standard, _)| standard == id) {
            return Some(Self::new(id, name));
        }
        if id < FIRST_REGISTERED_ID {
            return None;
        }
        let mut name = [0u16; 256];
        let len =
            unsafe { GetClipboardFormatNameW(id.into(), name.as_mut_ptr(), name.len() as i32) };
        let len = usize::try_from(len).ok().filter(|&len| len > 0)?.min(name.len());
        Some(Self::new(id, &String::from_utf16_lossy(&name[..len])))
    }

    pub(crate) fn new(id: u16, name: &str) -> Self {
        Self { id, name: name.into(), known: KnownFormat::identify(id, name) }
    }

    /// The id of the format, as used by the Win32 clipboard and OLE functions.
    pub fn id(&self) -> u16 {
        self.id
    }

    /// The name of the format: the name it was registered with, or the name of the constant of a
    /// standard format, such as `CF_UNICODETEXT`.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn known(&self) -> Option<KnownFormat> {
        self.known
    }
}

impl PartialEq for ClipboardFormat {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for ClipboardFormat {}

impl Hash for ClipboardFormat {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl fmt::Display for ClipboardFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.name)
    }
}

impl TransferType for ClipboardFormat {
    fn hint(&self) -> Option<TypeHint> {
        self.known.map(KnownFormat::hint)
    }

    fn matches(&self, other: &dyn TransferType) -> bool {
        match other.cast_ref::<Self>() {
            Some(other) => self == other,
            None => self.hint().zip(other.hint()).is_some_and(|(hint, other)| hint.matches(&other)),
        }
    }
}

/// The formats that winit converts, to ask a source for when it cannot list its formats.
pub(crate) fn known_formats() -> Vec<ClipboardFormat> {
    let standard =
        STANDARD_KNOWN_FORMATS.iter().filter_map(|&(id, _)| ClipboardFormat::from_id(id));
    let registered =
        REGISTERED_FORMATS.iter().filter_map(|&(name, _)| ClipboardFormat::register(name).ok());
    standard.chain(registered).collect()
}

/// The storage media that winit reads as bytes.
pub(crate) const BYTE_MEDIA: u32 = (TYMED_HGLOBAL | TYMED_ISTREAM) as u32;

/// A listing of `IEnumFORMATETC`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FormatListing {
    pub(crate) id: u16,
    pub(crate) aspect: u32,
    pub(crate) index: i32,
    pub(crate) media: u32,
}

/// The formats listed in `listings` that can be read as bytes, each with the media to read it in.
///
/// Only listings of the whole content count: aspect `DVASPECT_CONTENT` and index -1. A format
/// listed more than once can be read in the media of all its listings. The formats keep the order
/// of their first listing, which is the order of preference of the source.
pub(crate) fn readable_formats(listings: &[FormatListing]) -> Vec<(u16, u32)> {
    let mut formats: Vec<(u16, u32)> = Vec::new();
    for listing in listings {
        if listing.aspect != DVASPECT_CONTENT || listing.index != -1 {
            continue;
        }
        match formats.iter_mut().find(|(id, _)| *id == listing.id) {
            Some((_, media)) => *media |= listing.media,
            None => formats.push((listing.id, listing.media)),
        }
    }
    formats.retain_mut(|(_, media)| {
        *media &= BYTE_MEDIA;
        *media != 0
    });
    formats
}

/// A format on offer in a data transfer.
#[derive(Clone, Debug)]
pub(crate) struct AvailableFormat {
    pub(crate) format: ClipboardFormat,
    /// The media to request the format in: `TYMED_HGLOBAL`, `TYMED_ISTREAM` or both.
    pub(crate) media: u32,
}

/// The formats of `available` to read for `requested`, in the order to try them.
///
/// A [`ClipboardFormat`] is read as itself. Any other type is read from the formats whose hint
/// matches its hint.
pub(crate) fn candidates<'a>(
    available: &'a [AvailableFormat],
    requested: &dyn TransferType,
) -> Vec<&'a AvailableFormat> {
    if let Some(format) = requested.cast_ref::<ClipboardFormat>() {
        return available.iter().filter(|available| available.format == *format).collect();
    }
    let Some(hint) = requested.hint() else {
        return Vec::new();
    };
    available
        .iter()
        .filter(|available| available.format.hint().is_some_and(|other| other.matches(&hint)))
        .collect()
}

/// Data read from a format.
#[derive(Debug)]
pub(crate) enum Payload {
    Text(String),
    Uris(Vec<String>),
    Bytes(Vec<u8>),
}

/// Decodes the bytes of a format, or returns `None` when they are not valid data of the format.
///
/// A file list is read through its `HGLOBAL` instead, with `DragQueryFileW`.
pub(crate) fn decode(format: &ClipboardFormat, bytes: Cow<'_, [u8]>) -> Option<Payload> {
    match format.known {
        Some(KnownFormat::UnicodeText) => Some(Payload::Text(utf16_until_nul(&bytes))),
        Some(KnownFormat::FileList) => None,
        Some(KnownFormat::Png) | None => Some(Payload::Bytes(bytes.into_owned())),
    }
}

/// Decodes UTF-16 text that ends at its first NUL, or at the end of `bytes`. An odd last byte is
/// not part of the text.
pub(crate) fn utf16_until_nul(bytes: &[u8]) -> String {
    let units = bytes
        .chunks_exact(2)
        .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
        .take_while(|&unit| unit != 0);
    char::decode_utf16(units).map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
}
