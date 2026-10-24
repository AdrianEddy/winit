//! Clipboard formats: the types of the data on the clipboard and in drag-and-drop operations.

use std::borrow::Cow;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, OnceLock};
use std::{fmt, io};

use windows_sys::Win32::Globalization::{
    CP_ACP, CP_OEMCP, GetLocaleInfoW, LOCALE_IDEFAULTANSICODEPAGE, LOCALE_IDEFAULTCODEPAGE,
    LOCALE_RETURN_NUMBER, MultiByteToWideChar, WideCharToMultiByte,
};
use windows_sys::Win32::Graphics::Gdi::{BI_BITFIELDS, BI_RGB};
use windows_sys::Win32::System::Com::{DVASPECT_CONTENT, TYMED_HGLOBAL, TYMED_ISTREAM};
use windows_sys::Win32::System::DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW};
use windows_sys::Win32::System::Ole::{
    CF_BITMAP, CF_DIB, CF_DIBV5, CF_DIF, CF_DSPBITMAP, CF_DSPENHMETAFILE, CF_DSPMETAFILEPICT,
    CF_DSPTEXT, CF_ENHMETAFILE, CF_HDROP, CF_LOCALE, CF_METAFILEPICT, CF_OEMTEXT, CF_OWNERDISPLAY,
    CF_PALETTE, CF_PENDATA, CF_RIFF, CF_SYLK, CF_TEXT, CF_TIFF, CF_UNICODETEXT, CF_WAVE,
};
use winit_core::data_transfer::{SendData, TransferType, TypeHint};

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
    /// `CF_TEXT`: text in the ANSI code page.
    AnsiText,
    /// `CF_OEMTEXT`: text in the OEM code page.
    OemText,
    /// `"HTML Format"`: the HTML Clipboard Format.
    Html,
    /// `"Rich Text Format"`: RTF.
    Rtf,
    /// `CF_HDROP`: a list of files.
    FileList,
    /// `"UniformResourceLocatorW"`: a URL, as UTF-16.
    Url,
    /// `"PNG"`: a PNG image.
    Png,
    /// `CF_DIBV5`: a device-independent bitmap with a `BITMAPV5HEADER`.
    DibV5,
    /// `CF_DIB`: a device-independent bitmap.
    Dib,
    /// `CF_TIFF`: a TIFF image.
    Tiff,
    /// `CF_WAVE`: WAVE audio.
    Wave,
}

/// The standard formats that winit converts.
const STANDARD_KNOWN_FORMATS: &[(u16, KnownFormat)] = &[
    (CF_UNICODETEXT, KnownFormat::UnicodeText),
    (CF_TEXT, KnownFormat::AnsiText),
    (CF_OEMTEXT, KnownFormat::OemText),
    (CF_HDROP, KnownFormat::FileList),
    (CF_DIBV5, KnownFormat::DibV5),
    (CF_DIB, KnownFormat::Dib),
    (CF_TIFF, KnownFormat::Tiff),
    (CF_WAVE, KnownFormat::Wave),
];

const HTML_FORMAT: &str = "HTML Format";
const RTF_FORMAT: &str = "Rich Text Format";
const URL_FORMAT: &str = "UniformResourceLocatorW";
const PNG_FORMAT: &str = "PNG";

/// The registered formats that winit converts, by name.
const REGISTERED_FORMATS: &[(&str, KnownFormat)] = &[
    (HTML_FORMAT, KnownFormat::Html),
    (RTF_FORMAT, KnownFormat::Rtf),
    (URL_FORMAT, KnownFormat::Url),
    (PNG_FORMAT, KnownFormat::Png),
];

/// The registered formats that winit sends.
#[derive(Clone, Debug)]
pub(crate) struct RegisteredFormats {
    pub(crate) html: Option<ClipboardFormat>,
    pub(crate) rtf: Option<ClipboardFormat>,
    pub(crate) png: Option<ClipboardFormat>,
}

impl RegisteredFormats {
    /// The formats, registered once: the id of a registered format does not change.
    pub(crate) fn get() -> &'static Self {
        static FORMATS: OnceLock<RegisteredFormats> = OnceLock::new();
        let register = |name| ClipboardFormat::register(name).ok();
        FORMATS.get_or_init(|| Self {
            html: register(HTML_FORMAT),
            rtf: register(RTF_FORMAT),
            png: register(PNG_FORMAT),
        })
    }
}

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
            Self::UnicodeText | Self::AnsiText | Self::OemText => TypeHint::Plaintext,
            Self::Html => TypeHint::Html,
            Self::Rtf => TypeHint::Rtf,
            Self::FileList | Self::Url => TypeHint::UriList,
            Self::Png => TypeHint::Image { extension_hint: Some("png") },
            Self::DibV5 | Self::Dib => TypeHint::Image { extension_hint: Some("bmp") },
            Self::Tiff => TypeHint::Image { extension_hint: Some("tiff") },
            Self::Wave => TypeHint::Audio { extension_hint: Some("wav") },
        }
    }

    /// The preference for this format among the formats that match the same hint: the lowest
    /// rank is read first.
    fn rank(self) -> u8 {
        match self {
            Self::UnicodeText
            | Self::Html
            | Self::Rtf
            | Self::FileList
            | Self::Png
            | Self::Wave => 0,
            // Lossy: they cannot hold every character.
            Self::AnsiText => 1,
            Self::OemText => 2,
            // A link, where a file list names files.
            Self::Url => 1,
            // Losslessly converted, but larger than PNG; a V5 header can describe alpha.
            Self::DibV5 => 1,
            Self::Dib => 2,
            Self::Tiff => 3,
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

    /// This format without its hint: a type that the format matches, but that the types matching
    /// the format's hint do not.
    pub(crate) fn without_hint(&self) -> Self {
        Self { known: None, ..self.clone() }
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
/// matches its hint, the most faithful first.
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
    let mut candidates: Vec<_> = available
        .iter()
        .filter(|available| available.format.hint().is_some_and(|other| other.matches(&hint)))
        .collect();
    // Only known formats have hints. The sort is stable: the source's order breaks ties.
    candidates.sort_by_key(|available| available.format.known.map(KnownFormat::rank));
    candidates
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
/// `locale` is the locale identifier in the `CF_LOCALE` of the source, if it offers one; it sets
/// the code page of `CF_TEXT` and `CF_OEMTEXT`. A file list is read through its `HGLOBAL`
/// instead, with `DragQueryFileW`. A format that is not decoded is returned as its bytes; the
/// bytes of a global memory object may end with padding, as `GlobalSize` may exceed the size it
/// was allocated with.
pub(crate) fn decode(
    format: &ClipboardFormat,
    bytes: Cow<'_, [u8]>,
    locale: Option<u32>,
) -> Option<Payload> {
    let payload = match format.known {
        Some(KnownFormat::UnicodeText) => Payload::Text(utf16_until_nul(&bytes)),
        Some(known @ (KnownFormat::AnsiText | KnownFormat::OemText)) => {
            Payload::Text(code_page_text(until_nul(&bytes), text_code_page(known, locale))?)
        },
        Some(KnownFormat::Html) => {
            Payload::Text(String::from_utf8_lossy(html_clipboard_document(&bytes)?).into_owned())
        },
        // RTF is 7-bit text, but may be written in the ANSI code page.
        Some(KnownFormat::Rtf) => Payload::Text(match std::str::from_utf8(until_nul(&bytes)) {
            Ok(rtf) => rtf.to_owned(),
            Err(_) => code_page_text(until_nul(&bytes), CP_ACP)?,
        }),
        Some(KnownFormat::FileList) => return None,
        Some(KnownFormat::Url) => {
            let url = utf16_until_nul(&bytes);
            let url = url.trim();
            if url.is_empty() {
                return None;
            }
            Payload::Uris(vec![url.to_owned()])
        },
        Some(KnownFormat::DibV5 | KnownFormat::Dib) => Payload::Bytes(dib_to_bmp(&bytes)?),
        Some(KnownFormat::Png | KnownFormat::Tiff | KnownFormat::Wave) | None => {
            Payload::Bytes(bytes.into_owned())
        },
    };
    Some(payload)
}

/// The code page of the text of `format`, `CF_TEXT` or `CF_OEMTEXT`, which Microsoft ties to the
/// `CF_LOCALE` of its source: the ANSI or OEM code page of the locale `locale`, or of this system
/// when the source gives no locale or the locale has no such code page.
pub(crate) fn text_code_page(format: KnownFormat, locale: Option<u32>) -> u32 {
    let (info, system) = match format {
        KnownFormat::OemText => (LOCALE_IDEFAULTCODEPAGE, CP_OEMCP),
        _ => (LOCALE_IDEFAULTANSICODEPAGE, CP_ACP),
    };
    let mut number = [0u16; 2];
    locale
        .filter(|&lcid| {
            let len = unsafe {
                GetLocaleInfoW(lcid, info | LOCALE_RETURN_NUMBER, number.as_mut_ptr(), 2)
            };
            len == 2
        })
        .map(|_| u32::from(number[0]) | (u32::from(number[1]) << 16))
        // A locale without an ANSI or OEM code page gives `CP_ACP` or `CP_OEMCP`.
        .filter(|&code_page| code_page != CP_ACP && code_page != CP_OEMCP)
        .unwrap_or(system)
}

/// The bytes before the first NUL.
pub(crate) fn until_nul(bytes: &[u8]) -> &[u8] {
    bytes.iter().position(|&byte| byte == 0).map_or(bytes, |nul| &bytes[..nul])
}

/// The bytes to send `data` in as `format` when winit does not convert it to the format: bytes as
/// they are, and a string in the encoding of the format with the terminating NUL of the text
/// formats, which is the ANSI or OEM code page of `locale` (see [`text_code_page`]) for `CF_TEXT`
/// and `CF_OEMTEXT`, UTF-16 for the URL format, and UTF-8 without a NUL for any other format. A
/// list of URIs has no such form.
pub(crate) fn encode_raw(
    format: &ClipboardFormat,
    data: SendData,
    locale: Option<u32>,
) -> Option<Vec<u8>> {
    match data {
        SendData::Bytes(bytes) => Some(bytes),
        SendData::String(string) => match format.known {
            Some(known @ (KnownFormat::AnsiText | KnownFormat::OemText)) => {
                code_page_bytes(&string, text_code_page(known, locale))
            },
            Some(KnownFormat::Url) => {
                Some(string.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect())
            },
            _ => Some(string.into_bytes()),
        },
        _ => None,
    }
}

/// `text` in the code page `code_page` with a terminating NUL, or `None` if the conversion fails.
/// A character that the code page lacks becomes its default character.
pub(crate) fn code_page_bytes(text: &str, code_page: u32) -> Option<Vec<u8>> {
    let wide: Vec<u16> = text.encode_utf16().collect();
    let mut bytes = Vec::new();
    if !wide.is_empty() {
        let len = i32::try_from(wide.len()).ok()?;
        let (null, null_mut) = (std::ptr::null(), std::ptr::null_mut());
        let size = unsafe {
            WideCharToMultiByte(
                code_page,
                0,
                wide.as_ptr(),
                len,
                null_mut,
                0,
                null,
                null_mut.cast(),
            )
        };
        bytes.resize(usize::try_from(size).ok().filter(|&size| size > 0)?, 0);
        let written = unsafe {
            WideCharToMultiByte(
                code_page,
                0,
                wide.as_ptr(),
                len,
                bytes.as_mut_ptr(),
                size,
                null,
                null_mut.cast(),
            )
        };
        bytes.truncate(usize::try_from(written).ok().filter(|&written| written > 0)?);
    }
    bytes.push(0);
    Some(bytes)
}

/// Decodes text in the code page `code_page` with `MultiByteToWideChar`, or returns `None` if
/// the conversion fails.
pub(crate) fn code_page_text(bytes: &[u8], code_page: u32) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    let len = i32::try_from(bytes.len()).ok()?;
    let wide_len =
        unsafe { MultiByteToWideChar(code_page, 0, bytes.as_ptr(), len, std::ptr::null_mut(), 0) };
    let mut wide = vec![0; usize::try_from(wide_len).ok().filter(|&len| len > 0)?];
    let written = unsafe {
        MultiByteToWideChar(code_page, 0, bytes.as_ptr(), len, wide.as_mut_ptr(), wide_len)
    };
    let written = usize::try_from(written).ok().filter(|&len| len > 0)?;
    Some(String::from_utf16_lossy(&wide[..written.min(wide.len())]))
}

/// The HTML of the [HTML Clipboard Format]: its context, `StartHTML..EndHTML`, which Microsoft
/// documents as a complete HTML document, or its fragment, `StartFragment..EndFragment`, when it
/// has no valid context. Returns `None` when it has neither.
///
/// The offsets are read from the description at the start of the data, a field's first
/// occurrence counting. An offset past the end of the data is taken as the end, and the HTML ends
/// at a NUL.
///
/// [HTML Clipboard Format]: https://learn.microsoft.com/en-us/windows/win32/dataxchg/html-clipboard-format
pub(crate) fn html_clipboard_document(bytes: &[u8]) -> Option<&[u8]> {
    const FIELDS: [&[u8]; 4] = [b"StartHTML", b"EndHTML", b"StartFragment", b"EndFragment"];

    let mut offsets: [Option<Option<i64>>; 4] = [None; 4];
    // The description is a list of `name:value` lines.
    for line in bytes.split(|&byte| byte == b'\r' || byte == b'\n').filter(|line| !line.is_empty())
    {
        let Some(colon) = line.iter().position(|&byte| byte == b':') else {
            break;
        };
        let (name, value) = (&line[..colon], &line[colon + 1..]);
        if name.is_empty() || !name.iter().all(u8::is_ascii_alphanumeric) {
            break;
        }
        if let Some(field) = FIELDS.iter().position(|field| *field == name) {
            offsets[field].get_or_insert_with(|| {
                std::str::from_utf8(value).ok().and_then(|value| value.trim().parse().ok())
            });
        }
    }

    let region = |start: Option<Option<i64>>, end: Option<Option<i64>>| {
        let start = usize::try_from(start.flatten()?).ok()?;
        let end = usize::try_from(end.flatten()?).ok()?.min(bytes.len());
        let html = until_nul(bytes.get(start..end)?);
        (!html.is_empty()).then_some(html)
    };
    region(offsets[0], offsets[1]).or_else(|| region(offsets[2], offsets[3]))
}

/// The compression of a bitmap whose pixels are uncompressed, with four color masks.
const BI_ALPHABITFIELDS: u32 = 6;

/// Converts a packed device-independent bitmap, as `CF_DIB` and `CF_DIBV5` hold it, to a BMP
/// file, or returns `None` when it is not a valid bitmap.
///
/// A BMP file is the bitmap after a `BITMAPFILEHEADER`, whose `bfOffBits` is the offset of the
/// pixels: they follow the header, the color masks of a `BITMAPINFOHEADER` with `BI_BITFIELDS`
/// (three) or `BI_ALPHABITFIELDS` (four) compression, as the later headers contain their masks,
/// and the color table. The bitmap must hold all its pixels: rows of its width padded to four
/// bytes, as many as its height, which is negative for a top-down bitmap, or the size it gives
/// for compressed pixels.
pub(crate) fn dib_to_bmp(dib: &[u8]) -> Option<Vec<u8>> {
    const FILE_HEADER_LEN: usize = 14;
    const INFO_HEADER_LEN: usize = 40;

    let u16_at = |at: usize| Some(u16::from_le_bytes(dib.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(dib.get(at..at + 4)?.try_into().ok()?));
    let i32_at = |at: usize| Some(i32::from_le_bytes(dib.get(at..at + 4)?.try_into().ok()?));

    let header_len = usize::try_from(u32_at(0)?).ok()?;
    // A `BITMAPCOREHEADER` has a color table of `RGBTRIPLE`s, the later headers of `RGBQUAD`s.
    let (width, height, planes, bit_count, compression, image_size, colors_used, color_len) =
        match header_len {
            12 => {
                let (width, height) = (i64::from(u16_at(4)?), i64::from(u16_at(6)?));
                (width, height, u16_at(8)?, u16_at(10)?, BI_RGB, 0, 0, 3)
            },
            40 | 52 | 56 | 108 | 124 => {
                let (width, height) = (i64::from(i32_at(4)?), i64::from(i32_at(8)?));
                (width, height, u16_at(12)?, u16_at(14)?, u32_at(16)?, u32_at(20)?, u32_at(32)?, 4)
            },
            _ => return None,
        };
    if planes != 1 || width <= 0 || height == 0 {
        return None;
    }
    // A bitmap of no more than 8 bits per pixel has a color table of all its colors unless it
    // gives how many it uses; a JPEG or PNG one, of 0 bits per pixel, has none.
    let colors = match (colors_used, bit_count) {
        (0, 1..=8) => 1 << bit_count,
        (colors, _) => usize::try_from(colors).ok()?,
    };
    let masks = match (header_len, compression) {
        (INFO_HEADER_LEN, BI_BITFIELDS) => 3,
        (INFO_HEADER_LEN, BI_ALPHABITFIELDS) => 4,
        _ => 0,
    };
    let pixels_at =
        header_len.checked_add(masks * 4)?.checked_add(colors.checked_mul(color_len)?)?;
    let pixels_len = match compression {
        BI_RGB | BI_BITFIELDS | BI_ALPHABITFIELDS => {
            if !matches!(bit_count, 1 | 4 | 8 | 16 | 24 | 32) {
                return None;
            }
            let row_bits = u64::try_from(width).ok()?.checked_mul(bit_count.into())?;
            let stride = row_bits.checked_add(31)? / 32 * 4;
            stride.checked_mul(height.unsigned_abs())?
        },
        // Compressed pixels have the size the header gives.
        _ if image_size > 0 => u64::from(image_size),
        _ => return None,
    };
    let pixels_end = u64::try_from(pixels_at).ok()?.checked_add(pixels_len)?;
    if pixels_end > u64::try_from(dib.len()).ok()? {
        return None;
    }

    let file_len = u32::try_from(FILE_HEADER_LEN + dib.len()).ok()?;
    let pixels_at = u32::try_from(FILE_HEADER_LEN + pixels_at).ok()?;
    let mut bmp = Vec::with_capacity(FILE_HEADER_LEN + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_len.to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&pixels_at.to_le_bytes());
    bmp.extend_from_slice(dib);
    Some(bmp)
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
