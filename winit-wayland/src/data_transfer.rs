//! Types related to data transfer (drag-and-drop and clipboard).

use std::collections::hash_map::Entry;
use std::ffi::OsStr;
use std::io::{self, BufRead, Cursor, ErrorKind, Write};
use std::ops::{BitOr, Deref};
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::{fmt, mem};

use calloop::PostAction;
use dpi::{LogicalPosition, PhysicalPosition};
use foldhash::{HashMap, HashSet};
use rustix::pipe::{self, PipeFlags};
use sctk::compositor::Surface;
use sctk::data_device_manager::data_device::{DataDeviceData, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{
    DataOfferHandler, DragOffer, SelectionOffer as ClipboardOffer, receive_to_fd,
};
use sctk::data_device_manager::data_source::{
    CopyPasteSource, DataSourceHandler, DragSource as SctkDragSource,
};
use sctk::data_device_manager::{ReadPipe, WritePipe};
use sctk::primary_selection::device::{PrimarySelectionDeviceData, PrimarySelectionDeviceHandler};
use sctk::primary_selection::offer::PrimarySelectionOffer;
use sctk::primary_selection::selection::{
    PrimarySelectionSource as SctkPrimarySelectionSource, PrimarySelectionSourceHandler,
};
use sctk::reexports::client::backend::ObjectId;
use sctk::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1;
use sctk::reexports::protocols::wp::primary_selection::zv1::client::zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1;
use sctk::shell::WaylandSurface;
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_device_manager::DndAction as WlDndAction;
use wayland_client::protocol::wl_data_offer::{self, WlDataOffer};
use wayland_client::protocol::wl_data_source::WlDataSource;
use wayland_client::protocol::wl_shm::Format;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Proxy, QueueHandle};
use winit_core::data_transfer::{
    DataTransfer, DataTransferId, DataTransferSend, SendData, TransferType, TypeHint, TypedData,
};
use winit_core::error::{NotSupportedError, TransferError};
use winit_core::event::WindowEvent;
use winit_core::event_loop::{AsyncRequestSerial, DndAction, DragIcon};
use winit_core::icon::RgbaIcon;
use winit_core::window::WindowId;

use crate::state::WinitState;
use crate::{image_to_buffer, make_data_transfer_id};

#[derive(Default, Debug, PartialEq, Eq, Clone, Hash)]
enum Charset {
    #[default]
    Utf8,
}

/// MIME type as string, with an optional hint detected from the MIME type.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct MimeType {
    mime: Arc<str>,
    hint: Option<TypeHint>,
}

// MIME types
// Files
const TEXT_URI_LIST: &str = "text/uri-list";
// Plaintext and HTML: the canonical spelling of UTF-8 text, which other clients and XWayland's
// bridges look for, the bare type, and the spelling with a space that winit used before.
const TEXT_PLAIN: &str = "text/plain";
const TEXT_PLAIN_CHARSET_UTF8: &str = "text/plain;charset=utf-8";
const TEXT_PLAIN_CHARSET_UTF8_SPACED: &str = "text/plain; charset=utf-8";
const TEXT_HTML: &str = "text/html";
const TEXT_HTML_CHARSET_UTF8: &str = "text/html;charset=utf-8";
const TEXT_HTML_CHARSET_UTF8_SPACED: &str = "text/html; charset=utf-8";
// Charsets, whose names are compared case-insensitively (RFC 2046, section 4.1.2).
const UTF_8: &str = "utf-8";
/// The default charset of `text/plain` (RFC 2046, section 4.1.2), a subset of UTF-8.
const US_ASCII: &str = "us-ascii";
// RTF
const APPLICATION_RTF: &str = "application/rtf";
// Audio
const AUDIO_AAC: &str = "audio/aac";
const AUDIO_AIFF: &str = "audio/aiff";
const AUDIO_FLAC: &str = "audio/flac";
const AUDIO_WAV: &str = "audio/wav";
const AUDIO_WAVE: &str = "audio/wave";
const AUDIO_X_WAV: &str = "audio/x-wav";
const AUDIO_VND_WAV: &str = "audio/vnd.wav";
const AUDIO_VND_WAVE: &str = "audio/vnd.wave";
const AUDIO_MPEG: &str = "audio/mpeg";
const AUDIO_OGG: &str = "audio/ogg";
// Image
const IMAGE_BMP: &str = "image/bmp";
const IMAGE_GIF: &str = "image/gif";
const IMAGE_JPEG: &str = "image/jpeg";
const IMAGE_PJPEG: &str = "image/pjpeg";
const IMAGE_PNG: &str = "image/png";
const IMAGE_SVG: &str = "image/svg+xml";
const IMAGE_TIFF: &str = "image/tiff";
const IMAGE_WEBP: &str = "image/webp";
const IMAGE_X_ICON: &str = "image/x-icon";
const IMAGE_RAW: &str = "image/x-panasonic-raw";

#[derive(Debug)]
struct UnexpectedCharsetError<'a>(&'a str);

impl std::error::Error for UnexpectedCharsetError<'_> {}

impl fmt::Display for UnexpectedCharsetError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Unsupported charset in MIME type {}", self.0)
    }
}

impl MimeType {
    const MIME_HINT_MAP: &[(&str, TypeHint)] = &[
        // Files
        (TEXT_URI_LIST, TypeHint::UriList),
        // Plaintext, offered in this order: receivers that take the first type they recognise get
        // the one that says UTF-8.
        (TEXT_PLAIN_CHARSET_UTF8, TypeHint::Plaintext),
        (TEXT_PLAIN, TypeHint::Plaintext),
        (TEXT_PLAIN_CHARSET_UTF8_SPACED, TypeHint::Plaintext),
        // HTML, likewise
        (TEXT_HTML_CHARSET_UTF8, TypeHint::Html),
        (TEXT_HTML, TypeHint::Html),
        (TEXT_HTML_CHARSET_UTF8_SPACED, TypeHint::Html),
        // RTF
        (APPLICATION_RTF, TypeHint::Rtf),
        // Audio
        (AUDIO_AAC, TypeHint::Audio { extension_hint: Some("aac") }),
        (AUDIO_AIFF, TypeHint::Audio { extension_hint: Some("aif") }),
        (AUDIO_FLAC, TypeHint::Audio { extension_hint: Some("flac") }),
        (AUDIO_VND_WAV, TypeHint::Audio { extension_hint: Some("wav") }),
        (AUDIO_VND_WAVE, TypeHint::Audio { extension_hint: Some("wav") }),
        (AUDIO_WAV, TypeHint::Audio { extension_hint: Some("wav") }),
        (AUDIO_WAVE, TypeHint::Audio { extension_hint: Some("wav") }),
        (AUDIO_X_WAV, TypeHint::Audio { extension_hint: Some("wav") }),
        (AUDIO_OGG, TypeHint::Audio { extension_hint: Some("ogg") }),
        (AUDIO_MPEG, TypeHint::Audio { extension_hint: Some("mp3") }),
        // Image
        (IMAGE_BMP, TypeHint::Image { extension_hint: Some("bmp") }),
        (IMAGE_GIF, TypeHint::Image { extension_hint: Some("gif") }),
        (IMAGE_JPEG, TypeHint::Image { extension_hint: Some("jpg") }),
        (IMAGE_PJPEG, TypeHint::Image { extension_hint: Some("jpg") }),
        (IMAGE_PNG, TypeHint::Image { extension_hint: Some("png") }),
        (IMAGE_RAW, TypeHint::Image { extension_hint: Some("raw") }),
        (IMAGE_SVG, TypeHint::Image { extension_hint: Some("svg") }),
        (IMAGE_TIFF, TypeHint::Image { extension_hint: Some("tiff") }),
        (IMAGE_WEBP, TypeHint::Image { extension_hint: Some("webp") }),
        (IMAGE_X_ICON, TypeHint::Image { extension_hint: Some("ico") }),
    ];

    /// Creates a MIME type from its name, such as `application/x-example`.
    ///
    /// The hint is detected from the name, the same way as for a type offered by another client.
    pub fn new(mime: impl Into<String>) -> Self {
        Self::parse(mime.into())
    }

    /// The name of this MIME type.
    pub fn as_str(&self) -> &str {
        &self.mime
    }

    // Returns an iterator so that things like the multiple charsets for plaintext/HTML
    // and the multiple ways of expressing .wav work correctly.
    fn from_dyn(type_: &dyn TransferType) -> impl Iterator<Item = Self> {
        let downcast = type_.cast_ref::<Self>().cloned();
        let downcast_failed = downcast.is_none();
        // This filter is a bit hacky, but it's the only way to ensure that we always
        // return the same type.
        let from_hint = downcast_failed
            .then_some(
                Self::MIME_HINT_MAP
                    .iter()
                    .filter(move |(_, haystack)| TransferType::matches(haystack, type_))
                    .map(move |(mime, _)| Self {
                        mime: mime.to_string().into(),
                        hint: type_.hint(),
                    }),
            )
            .into_iter()
            .flatten();

        downcast.into_iter().chain(from_hint)
    }

    /// The charset to decode and encode text of this type in. Text is UTF-8, which also covers a
    /// type without a charset and US-ASCII; any other charset is refused.
    fn parse_charset(&self) -> Result<Charset, UnexpectedCharsetError<'_>> {
        match TextCharset::of(&self.mime) {
            TextCharset::Unstated | TextCharset::UsAscii | TextCharset::Utf8 => Ok(Charset::Utf8),
            TextCharset::Other => Err(UnexpectedCharsetError(&self.mime)),
        }
    }

    /// The hint of text, plain or HTML, in whatever spelling the type is written: `text/plain` or
    /// `text/html` in any case, without a charset or with the charset UTF-8 or US-ASCII.
    fn text_hint(mime: &str) -> Option<TypeHint> {
        let essence = essence(mime);
        let hint = if essence.eq_ignore_ascii_case(TEXT_PLAIN) {
            TypeHint::Plaintext
        } else if essence.eq_ignore_ascii_case(TEXT_HTML) {
            TypeHint::Html
        } else {
            return None;
        };
        (TextCharset::of(mime) != TextCharset::Other).then_some(hint)
    }

    fn parse(mime: String) -> Self {
        let has_prefix = |prefix: &str| {
            mime.get(..prefix.len()).is_some_and(|start| start.eq_ignore_ascii_case(prefix))
        };
        let hint = Self::text_hint(&mime)
            .or_else(|| {
                Self::MIME_HINT_MAP.iter().find_map(|(haystack, hint)| {
                    haystack.eq_ignore_ascii_case(&mime).then_some(*hint)
                })
            })
            .or_else(|| {
                if has_prefix("image/") {
                    Some(TypeHint::Image { extension_hint: None })
                } else if has_prefix("audio/") {
                    Some(TypeHint::Audio { extension_hint: None })
                } else {
                    None
                }
            });

        Self { mime: mime.into(), hint }
    }

    /// How this type ranks among the types offered for its hint, lowest first: text that says it
    /// is UTF-8 comes before text without a charset or in US-ASCII, which a reader cannot tell
    /// from text in the sender's locale.
    fn read_rank(&self) -> u8 {
        match self.hint {
            Some(TypeHint::Plaintext | TypeHint::Html) => {
                u8::from(TextCharset::of(&self.mime) != TextCharset::Utf8)
            },
            _ => 0,
        }
    }
}

/// The essence of a MIME type, `type/subtype`: what precedes its parameters.
fn essence(mime: &str) -> &str {
    mime.split_once(';').map_or(mime, |(essence, _)| essence).trim_matches(is_lws)
}

/// Whether `essence` is the essence of text, `text/plain` or `text/html`, in any case.
fn is_text_essence(essence: &str) -> bool {
    essence.eq_ignore_ascii_case(TEXT_PLAIN) || essence.eq_ignore_ascii_case(TEXT_HTML)
}

/// Linear white space, which may surround the parts of a MIME type's parameters (RFC 2045, section
/// 5.1, after RFC 822): a space or a tab.
fn is_lws(char: char) -> bool {
    char == ' ' || char == '\t'
}

/// What the parameters of a MIME type say about the charset of its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextCharset {
    /// No `charset` parameter.
    Unstated,
    UsAscii,
    Utf8,
    /// A charset that is not decoded: another one, an empty one, conflicting ones, one in RFC
    /// 2231's form (`charset*`, `charset*0`), or one that does not follow the grammar; for text,
    /// also any other parameter that does not follow the grammar.
    Other,
}

/// A parameter that does not follow the grammar of RFC 2045.
struct Malformed;

impl TextCharset {
    /// The charset that the parameters of `mime` state.
    ///
    /// The parameters are read as RFC 2045 (section 5.1) writes them: each one follows a `;` and
    /// is `attribute=value`, where the attribute is a token, compared case-insensitively, and the
    /// value is a token or a quoted string; linear white space (a space or a tab) may surround the
    /// `;`, the `=` and the parameter. A quoted string is read up to its closing quote, a `\`
    /// quoting the next character, and its content is not otherwise checked. Two more shapes are
    /// allowed, in which nothing can hide: an empty parameter (`;;`, or a `;` at the end), and a
    /// lone token other than `charset`, without `=` (`;flowed`), which is skipped.
    ///
    /// Any other parameter does not follow the grammar: a comment, text after a value, an attribute
    /// or an unquoted value that is not a token (an empty value is not one), an unterminated quoted
    /// string, and a `charset` without a value. In text, `text/plain` and `text/html`, such a
    /// parameter makes the charset `Other`, so that no charset can hide in parameters that are read
    /// some other way. In other types, which are read leniently, it is skipped as a piece up to the
    /// next raw `;` and states no charset, unless that piece may state one
    /// ([`may_state_charset`]): unless it contains `charset`, in any case, followed by `=`, with
    /// linear white space between them or not, or its attribute is a charset in RFC 2231's form.
    /// That makes the charset `Other`. A charset that the other parameters state is still read,
    /// and a well-formed parameter is read exactly (a `charset=` inside a quoted value is no
    /// charset). The skip does not respect quotes: a malformed quoted string can end the piece at
    /// a `;` inside it, which shifts where the next piece starts, and a charset in such input is
    /// not guaranteed to be refused.
    fn of(mime: &str) -> Self {
        let strict = is_text_essence(essence(mime));
        let mut rest = mime.split_once(';').map_or("", |(_, parameters)| parameters);
        let mut charset = None;
        loop {
            rest = rest.trim_start_matches(is_lws);
            if rest.is_empty() {
                return charset.unwrap_or(Self::Unstated);
            }
            if let Some(after) = rest.strip_prefix(';') {
                rest = after;
                continue;
            }

            let this = match parameter(rest) {
                Ok((attribute, value, after)) => {
                    rest = after;
                    match value {
                        _ if is_rfc2231_charset(attribute) => Self::Other,
                        Some(value) if attribute.eq_ignore_ascii_case("charset") => {
                            Self::named(value)
                        },
                        _ => continue,
                    }
                },
                Err(Malformed) if strict => return Self::Other,
                Err(Malformed) => {
                    let (malformed, after) = rest.split_at(rest.find(';').unwrap_or(rest.len()));
                    if may_state_charset(malformed) {
                        return Self::Other;
                    }
                    rest = after;
                    continue;
                },
            };
            if charset.is_some_and(|earlier| earlier != this) {
                return Self::Other;
            }
            charset = Some(this);
        }
    }

    fn named(value: ParameterValue<'_>) -> Self {
        if value.eq_ignore_ascii_case(UTF_8) {
            Self::Utf8
        } else if value.eq_ignore_ascii_case(US_ASCII) {
            Self::UsAscii
        } else {
            Self::Other
        }
    }
}

/// Reads the parameter at the start of `text`, which neither is empty nor starts with `;` or linear
/// white space ([`TextCharset::of`]): its attribute, its value (`None` for a lone token), and what
/// follows it, which is empty or starts with `;`.
fn parameter(text: &str) -> Result<(&str, Option<ParameterValue<'_>>, &str), Malformed> {
    let end = text.find(|char| char == ';' || char == '=').unwrap_or(text.len());
    let attribute = text[..end].trim_end_matches(is_lws);
    if !is_token(attribute) {
        return Err(Malformed);
    }
    let Some(after) = text[end..].strip_prefix('=') else {
        return if attribute.eq_ignore_ascii_case("charset") {
            Err(Malformed)
        } else {
            Ok((attribute, None, &text[end..]))
        };
    };
    let (value, after) = ParameterValue::read(after.trim_start_matches(is_lws)).ok_or(Malformed)?;
    let after = after.trim_start_matches(is_lws);
    if !after.is_empty() && !after.starts_with(';') {
        return Err(Malformed);
    }
    Ok((attribute, Some(value), after))
}

/// Whether a piece of a parameter that does not follow the grammar, `malformed`, up to the next raw
/// `;`, may state a charset, which cannot be read ([`TextCharset::of`]). A lone `charset` states
/// none.
fn may_state_charset(malformed: &str) -> bool {
    const CHARSET: &[u8] = b"charset";
    let attribute = malformed.split_once('=').map_or(malformed, |(attribute, _)| attribute);
    let bytes = malformed.as_bytes();
    is_rfc2231_charset(attribute.trim_end_matches(is_lws))
        || bytes.windows(CHARSET.len()).enumerate().any(|(start, word)| {
            word.eq_ignore_ascii_case(CHARSET)
                && bytes[start + CHARSET.len()..]
                    .iter()
                    .find(|&&byte| !is_lws(char::from(byte)))
                    .is_some_and(|&byte| byte == b'=')
        })
}

/// Whether `text` is a token of RFC 2045 (section 5.1): one or more US-ASCII characters other than
/// space, the control characters and the `tspecials`.
fn is_token(text: &str) -> bool {
    const TSPECIALS: &[u8] = b"()<>@,;:\\\"/[]?=";
    !text.is_empty()
        && text.bytes().all(|byte| byte.is_ascii_graphic() && !TSPECIALS.contains(&byte))
}

/// Whether the attribute `attribute` is a charset in RFC 2231's form (`charset*`, or a section of
/// one, `charset*0`), which is not decoded.
fn is_rfc2231_charset(attribute: &str) -> bool {
    // The ASCII `*` makes its index a character boundary, at which the attribute can be sliced.
    attribute.len() > "charset".len()
        && attribute.as_bytes()["charset".len()] == b'*'
        && attribute[.."charset".len()].eq_ignore_ascii_case("charset")
}

/// The value of a MIME type's parameter: a token, or the inside of a quoted string, in which a
/// `\` quotes the next character. It is read where it stands, without allocating.
#[derive(Debug, Clone, Copy)]
struct ParameterValue<'a> {
    text: &'a str,
    quoted: bool,
}

impl<'a> ParameterValue<'a> {
    /// Reads the value at the start of `text`, returning it and what follows it, or `None` for an
    /// unquoted value that is not a token, or an unterminated quoted string.
    fn read(text: &'a str) -> Option<(Self, &'a str)> {
        let Some(quoted) = text.strip_prefix('"') else {
            let end = text.find(';').unwrap_or(text.len());
            let token = text[..end].trim_end_matches(is_lws);
            return is_token(token).then_some((Self { text: token, quoted: false }, &text[end..]));
        };
        let mut escaped = false;
        let close = quoted.char_indices().find_map(|(index, char)| {
            match (escaped, char) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => return Some(index),
                (false, _) => {},
            }
            None
        })?;
        Some((Self { text: &quoted[..close], quoted: true }, &quoted[close + 1..]))
    }

    /// The characters of the value, with each quoted pair standing for the character it quotes.
    fn chars(self) -> impl Iterator<Item = char> + 'a {
        let mut chars = self.text.chars();
        std::iter::from_fn(move || match chars.next()? {
            '\\' if self.quoted => chars.next(),
            char => Some(char),
        })
    }

    fn eq_ignore_ascii_case(self, other: &str) -> bool {
        let mut other = other.chars();
        self.chars().all(|char| other.next().is_some_and(|o| o.eq_ignore_ascii_case(&char)))
            && other.next().is_none()
    }
}

/// Tests of the MIME types: their parsing, the types offered, read and sent.
#[cfg(test)]
mod mime_tests;

impl fmt::Display for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.mime.fmt(f)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct UnknownTypeHint(pub TypeHint);

impl fmt::Display for UnknownTypeHint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Unknown type hint: {:?}", self.0)
    }
}

impl TryFrom<TypeHint> for MimeType {
    type Error = UnknownTypeHint;

    fn try_from(hint: TypeHint) -> Result<Self, Self::Error> {
        // Text keeps its bare type here, whichever spelling is offered first.
        let mime = match hint {
            TypeHint::Plaintext => Some(TEXT_PLAIN),
            TypeHint::Html => Some(TEXT_HTML),
            _ => Self::MIME_HINT_MAP
                .iter()
                .find_map(|(mime, haystack)| (*haystack == hint).then_some(*mime)),
        }
        .ok_or(UnknownTypeHint(hint))?;

        Ok(Self { mime: mime.to_owned().into(), hint: Some(hint) })
    }
}

impl TransferType for MimeType {
    fn hint(&self) -> Option<TypeHint> {
        self.hint
    }

    fn matches(&self, other: &dyn TransferType) -> bool {
        if let Some(other_mime) = other.cast_ref::<Self>() {
            *self == *other_mime
        } else {
            // If either hint is `None`, return false
            self.hint().is_some_and(|hint| other.hint() == Some(hint))
        }
    }
}

type BytesResult = Result<Vec<u8>, Arc<io::Error>>;

#[derive(Debug)]
pub struct MimeData {
    mime_type: MimeType,
    result: BytesResult,
}

impl MimeData {
    fn new(mime_type: MimeType, result: BytesResult) -> Self {
        Self { mime_type, result }
    }

    fn data(&self) -> io::Result<&[u8]> {
        fn arc_to_io_error(arc: Arc<io::Error>) -> io::Error {
            io::Error::new(arc.kind(), arc)
        }

        self.result.as_deref().map_err(|e| arc_to_io_error(e.clone()))
    }
}

impl TypedData for MimeData {
    fn type_(&self) -> &dyn TransferType {
        &self.mime_type
    }

    fn try_read(&self) -> Option<Box<dyn io::BufRead>> {
        let data = self.data().ok()?.to_owned();

        Some(Box::new(io::Cursor::new(data)))
    }

    fn try_as_bytes(&self) -> io::Result<Vec<u8>> {
        self.data().map(ToOwned::to_owned)
    }

    fn try_as_uris(&self) -> io::Result<Vec<String>> {
        let data = self.data()?;

        Cursor::new(&data)
            .lines()
            .filter(|result| match result {
                Ok(s) => !s.starts_with('#'),
                // We want to maintain errors, so the final `collect` returns an error too
                Err(_) => true,
            })
            .collect()
    }

    fn try_as_string(&self) -> io::Result<String> {
        let charset = self.mime_type.parse_charset();

        let data = self.data()?;

        match charset {
            Ok(Charset::Utf8) => String::from_utf8(data.to_vec())
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err)),
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    }
}

/// A snapshot of a data transfer offered by the compositor, implementing [`DataTransfer`].
#[derive(Debug, Clone)]
pub struct DataOffer {
    mime_types: Arc<[MimeType]>,
    transfer_id: DataTransferId,
    /// The actions that the source of a drag allows, as it last reported them; `None` for an
    /// offer that is not a drag, or that does not report them.
    source_actions: Option<WlDndAction>,
}

impl DataOffer {
    fn new(mime_types: &[String], transfer_id: DataTransferId) -> Self {
        Self {
            mime_types: mime_types.iter().map(|mime| MimeType::parse(mime.clone())).collect(),
            transfer_id,
            source_actions: None,
        }
    }

    /// The offer of a drag, made through a `wl_data_offer` of version `offer_version`, whose
    /// source allows `source_actions`. Offers before version 3 do not report the source actions,
    /// which then remain unknown.
    fn dragged(mut self, offer_version: u32, source_actions: WlDndAction) -> Self {
        if offer_version >= wl_data_offer::EVT_SOURCE_ACTIONS_SINCE {
            self.source_actions = Some(source_actions);
        }
        self
    }

    /// The source of the drag offered now allows `source_actions`, as the
    /// `wl_data_offer.source_actions` event (of offers of version 3 and later) reports.
    fn set_source_actions(&mut self, source_actions: WlDndAction) {
        self.source_actions = Some(source_actions);
    }

    /// The actions to negotiate with the source of the drag offered: none if they are unknown,
    /// which is also what an offer before version 3 negotiates with.
    fn actions_allowed_by_source(&self) -> WlDndAction {
        self.source_actions.unwrap_or(WlDndAction::empty())
    }

    fn transfer_id(&self) -> DataTransferId {
        self.transfer_id
    }

    fn first_mime_type(&self) -> Option<&MimeType> {
        self.mime_types.first()
    }

    fn find_type_dyn<'a>(&'a self, type_: &'a dyn TransferType) -> Option<&'a MimeType> {
        match type_.cast_ref::<MimeType>() {
            Some(mime_type) => Some(mime_type),
            None => {
                let hint = type_.hint()?;
                // The first offered type of the best rank: text that says it is UTF-8 first.
                self.mime_types
                    .iter()
                    .filter(|mime_type| {
                        mime_type.hint().is_some_and(|haystack| haystack.matches(&hint))
                    })
                    .min_by_key(|mime_type| mime_type.read_rank())
            },
        }
    }
}

impl DataTransfer for DataOffer {
    fn for_each_available_type<'this>(
        &'this self,
        func: &'_ mut dyn FnMut(&'this dyn TransferType) -> std::ops::ControlFlow<()>,
    ) {
        let _ = self.mime_types.iter().map(|mime| mime as &dyn TransferType).try_for_each(func);
    }

    fn source_actions(&self) -> Option<&[DndAction]> {
        self.source_actions.map(dnd_actions_wl_to_winit)
    }
}

#[derive(Debug)]
struct DataFetch {
    id: DataTransferId,
    serial: AsyncRequestSerial,
    window_id: WindowId,
    mime_type: Option<MimeType>,
    buffer: Vec<u8>,
}

impl DataFetch {
    fn new(
        id: DataTransferId,
        serial: AsyncRequestSerial,
        mime_type: MimeType,
        window_id: WindowId,
    ) -> Self {
        Self { id, serial, window_id, mime_type: Some(mime_type), buffer: Vec::new() }
    }

    fn read(&mut self, file: &mut impl io::Read, state: &mut WinitState) -> PostAction {
        let result = match file.read_to_end(&mut self.buffer) {
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                return PostAction::Continue;
            },
            Ok(0) => Ok(mem::take(&mut self.buffer)),
            Ok(_) => {
                return PostAction::Continue;
            },
            Err(e) => Err(Arc::new(e)),
        };

        state.events_sink.push_window_event(
            WindowEvent::DataTransferReceived {
                id: self.id,
                serial: self.serial,
                // `unwrap` is safe here, as completion happens exactly once.
                value: Arc::new(MimeData::new(self.mime_type.take().unwrap(), result)),
            },
            self.window_id,
        );

        state.dispatched_events = true;

        state.data_transfer_state.fetch_completed(self.id, self.serial);

        PostAction::Remove
    }
}

enum Session<'a> {
    Drag(&'a mut DragSession),
    Clipboard(&'a mut ClipboardSession),
}

impl Session<'_> {
    fn data(&self) -> &DataOffer {
        match self {
            Session::Drag(session) => &session.data,
            Session::Clipboard(session) => &session.data,
        }
    }

    fn start_fetch(self, mime_type: String, writefd: OwnedFd, serial: AsyncRequestSerial) {
        match self {
            Session::Drag(session) => session.start_fetch(mime_type, writefd, serial),
            Session::Clipboard(session) => session.start_fetch(mime_type, writefd, serial),
        }
    }

    fn fetch_completed(self, serial: AsyncRequestSerial) {
        let fetches = match self {
            Session::Drag(session) => &mut session.fetches,
            Session::Clipboard(session) => &mut session.fetches,
        };

        let removed = fetches.remove(&serial);
        debug_assert!(removed, "fetch completed without a matching start_fetch");
    }
}

#[derive(Debug, Default)]
pub struct DataTransferState {
    drags: HashMap<ObjectId, DragSession>,
    clipboards: HashMap<ObjectId, ClipboardSession>,
    primary_selections: HashMap<ObjectId, ClipboardSession>,
    send_drags: SendDrags<DragSource>,
    send_clipboard: Option<ClipboardSource>,
    send_primary_selection: Option<PrimarySelectionSource>,
    clipboard_serial: u32,
}

/// The drags the application started that have not ended yet, oldest first.
///
/// A drag stays here after its drop until the target finishes it, so a new drag can start while an
/// earlier one is still finishing.
#[derive(Debug)]
struct SendDrags<D> {
    drags: Vec<D>,
}

impl<D> Default for SendDrags<D> {
    fn default() -> Self {
        Self { drags: Vec::new() }
    }
}

impl<D> SendDrags<D> {
    fn find_mut(&mut self, mut is: impl FnMut(&D) -> bool) -> Option<&mut D> {
        self.drags.iter_mut().find(|drag| is(drag))
    }

    fn take(&mut self, mut is: impl FnMut(&D) -> bool) -> Option<D> {
        let index = self.drags.iter().position(|drag| is(drag))?;
        Some(self.drags.remove(index))
    }

    /// Adds a newly started drag. A seat holds one drag at a time, so an earlier drag that was
    /// never dropped gets no further events from the compositor: those drags are removed and
    /// returned, for the caller to end.
    fn start(&mut self, drag: D, dropped: impl Fn(&D) -> bool) -> Vec<D> {
        let (finishing, abandoned) =
            mem::take(&mut self.drags).into_iter().partition(|drag| dropped(drag));
        self.drags = finishing;
        self.drags.push(drag);
        abandoned
    }
}

impl DataTransferState {
    fn session_mut(&mut self, id: DataTransferId) -> Option<Session<'_>> {
        if let Some(session) = self.drags.values_mut().find(|drag| drag.data.transfer_id() == id) {
            return Some(Session::Drag(session));
        }

        self.clipboards
            .values_mut()
            .chain(self.primary_selections.values_mut())
            .find(|clipboard| clipboard.data.transfer_id() == id)
            .map(Session::Clipboard)
    }

    fn fetch_completed(&mut self, id: DataTransferId, serial: AsyncRequestSerial) {
        if let Some(session) = self.session_mut(id) {
            session.fetch_completed(serial);
        }
    }

    fn send_data_mut(&mut self, source: &WlDataSource) -> Option<&mut dyn DataTransferSend> {
        if let Some(drag) = self.send_drags.find_mut(|drag| drag.data_source.inner() == source) {
            return Some(drag.data.as_mut());
        }

        self.send_clipboard
            .as_mut()
            .filter(|clipboard| clipboard.data_source.inner() == source)
            .map(|clipboard| &mut *clipboard.data)
    }
}

impl WinitState {
    pub(crate) fn fetch_data_transfer(
        &mut self,
        id: DataTransferId,
        type_: &dyn TransferType,
    ) -> Result<AsyncRequestSerial, TransferError> {
        let Some(session) = self.data_transfer_state.session_mut(id) else {
            return Err(TransferError::UnknownTransfer(id));
        };

        let Some(mime_type) = session.data().find_type_dyn(type_).cloned() else {
            return Err(TransferError::Failed);
        };

        let window_id = match &session {
            Session::Drag(session) => session.window_id,
            Session::Clipboard(session) => {
                let seat = &session.seat;
                let windows = self.windows.borrow();

                windows
                    .iter()
                    .find_map(|(window_id, window)| {
                        let window = window.lock().unwrap();
                        window.focused_seats().any(|id| id == seat).then_some(*window_id)
                    })
                    .or_else(|| self.seats.get(seat)?.pointer_data()?.data().focused_window())
                    .or_else(|| windows.keys().next().copied())
                    .ok_or(TransferError::Failed)?
            },
        };

        let mime_type_str = mime_type.to_string();

        let (readfd, writefd) =
            pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).map_err(|e| os_error!(e))?;

        let async_request_serial = AsyncRequestSerial::get();

        let mut fetch = DataFetch::new(id, async_request_serial, mime_type, window_id);

        self.loop_handle
            .insert_source(ReadPipe::from(readfd), move |_, file, state| {
                // SAFETY: We do not overwrite the referent of `file`
                let file = unsafe { file.get_mut() };

                fetch.read(file, state)
            })
            .map_err(|err| os_error!(io::Error::other(err.to_string())))?;

        session.start_fetch(mime_type_str, writefd, async_request_serial);

        Ok(async_request_serial)
    }

    pub(crate) fn data_transfer(
        &mut self,
        id: DataTransferId,
    ) -> Result<Box<dyn DataTransfer>, TransferError> {
        match self.data_transfer_state.session_mut(id) {
            Some(session) => Ok(Box::new(session.data().clone())),
            None => Err(TransferError::UnknownTransfer(id)),
        }
    }

    pub(crate) fn set_valid_dnd_actions(
        &mut self,
        id: DataTransferId,
        actions: &[DndAction],
    ) -> Result<(), TransferError> {
        let Some(Session::Drag(session)) = self.data_transfer_state.session_mut(id) else {
            return Err(TransferError::UnknownTransfer(id));
        };

        session.set_actions(actions);

        Ok(())
    }
}

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

/// The bytes that answer a request for `send_data` as `mime`, or `None` when it cannot be sent so.
fn send_data_bytes(mime: &MimeType, send_data: SendData) -> Option<Vec<u8>> {
    match send_data {
        SendData::Uris(strings) => Some(encode_uri_list(strings)),
        SendData::String(str) => match TextCharset::of(&mime.mime) {
            TextCharset::Unstated | TextCharset::Utf8 => Some(str.into_bytes()),
            // Asked for by name, US-ASCII must be what is sent.
            TextCharset::UsAscii if str.is_ascii() => Some(str.into_bytes()),
            TextCharset::UsAscii => {
                tracing::error!("Text that is not ASCII cannot be sent as {mime}");
                None
            },
            TextCharset::Other => {
                tracing::error!("{}", UnexpectedCharsetError(&mime.mime));
                None
            },
        },
        SendData::Bytes(binary) => Some(binary),
        _ => None,
    }
}

impl WinitState {
    fn write_send_data(&self, mime: MimeType, send_data: SendData, fd: WritePipe) {
        let Some(bytes) = send_data_bytes(&mime, send_data) else {
            return;
        };
        let mut encoder = Cursor::new(bytes);

        let _ = self.loop_handle.insert_source(fd, move |_, file, _| {
            // SAFETY: We only mutate `file` in-place and do not replace and drop it.
            let file = unsafe { file.get_mut() };
            loop {
                let Ok(encoded_bytes) = encoder.fill_buf() else {
                    return PostAction::Remove;
                };

                match file.write(encoded_bytes) {
                    Ok(0) => {
                        break PostAction::Remove;
                    },
                    Ok(consumed) => {
                        encoder.consume(consumed);
                    },
                    Err(e) if e.kind() == ErrorKind::WouldBlock => {
                        break PostAction::Continue;
                    },
                    Err(_) => {
                        break PostAction::Remove;
                    },
                }
            }
        });
    }
}

impl DataSourceHandler for WinitState {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
        // This method isn't a necessary part of the protocol, it's a holdover from the first
        // version of DnD in Wayland and now just serves as a hint.
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        fd: WritePipe,
    ) {
        let Some(data) = self.data_transfer_state.send_data_mut(source) else {
            // TODO: Is there a way to explicitly express that the data was not sent?
            return;
        };

        let mime = MimeType::parse(mime);

        let Some(send_data) = data.data_for_type(&mime) else {
            return;
        };

        self.write_send_data(mime, send_data, fd);
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        let send_clipboard = &mut self.data_transfer_state.send_clipboard;
        if send_clipboard.take_if(|clipboard| clipboard.data_source.inner() == source).is_some() {
            return;
        }

        let Some(current_drag) = self.data_transfer_state.take_send_drag(source) else {
            return;
        };

        let window_id = current_drag.window_id;
        let id = current_drag.data_transfer_id;

        self.events_sink.push_window_event(WindowEvent::OutgoingDragCanceled { id }, window_id);
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        // The drop is not complete yet: the target can still refuse it, and the compositor then
        // cancels the source. The drag ends at `dnd_finished` or at `cancelled`.
        if let Some(drag) =
            self.data_transfer_state.send_drags.find_mut(|drag| drag.data_source.inner() == source)
        {
            drag.dropped = true;
        }
    }

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        let Some(current_drag) = self.data_transfer_state.take_send_drag(source) else {
            return;
        };

        self.events_sink.push_window_event(
            WindowEvent::OutgoingDragDropped {
                id: current_drag.data_transfer_id,
                action: dnd_action_wl_to_winit(current_drag.selected_action),
            },
            current_drag.window_id,
        );
    }

    fn action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        action: WlDndAction,
    ) {
        self.data_transfer_state.set_target_drag_action(source, action);
    }
}

#[derive(Debug)]
enum SelectionOffer {
    Clipboard(ClipboardOffer),
    Primary(PrimarySelectionOffer),
}

impl SelectionOffer {
    fn receive_to_fd(&self, mime_type: String, writefd: OwnedFd) {
        match self {
            Self::Clipboard(offer) => receive_to_fd(offer.inner(), mime_type, writefd),
            Self::Primary(offer) => offer.receive_to_fd(mime_type, writefd),
        }
    }
}

#[derive(Debug)]
struct ClipboardSession {
    offer: SelectionOffer,
    data: DataOffer,
    seat: ObjectId,
    fetches: HashSet<AsyncRequestSerial>,
}

impl ClipboardSession {
    fn start_fetch(&mut self, mime_type: String, writefd: OwnedFd, serial: AsyncRequestSerial) {
        self.offer.receive_to_fd(mime_type, writefd);
        self.fetches.insert(serial);
    }
}

#[derive(Debug)]
struct ClipboardSource {
    data_source: CopyPasteSource,
    data: Box<dyn DataTransferSend>,
}

#[derive(Debug)]
struct PrimarySelectionSource {
    data_source: SctkPrimarySelectionSource,
    data: Box<dyn DataTransferSend>,
}

impl DataTransferState {
    fn clipboard_id(&self) -> Option<DataTransferId> {
        self.clipboards.values().next().map(|clipboard| clipboard.data.transfer_id())
    }

    fn primary_selection_id(&self) -> Option<DataTransferId> {
        self.primary_selections.values().next().map(|primary| primary.data.transfer_id())
    }
}

impl WinitState {
    pub(crate) fn clipboard(&self) -> Result<Option<DataTransferId>, TransferError> {
        self.data_device_manager_state.as_ref().ok_or(NotSupportedError::new(
            "Tried to read the clipboard, but data device not enabled",
        ))?;

        Ok(self.data_transfer_state.clipboard_id())
    }

    pub(crate) fn set_clipboard(
        &mut self,
        queue_handle: &QueueHandle<Self>,
        send_data: Box<dyn DataTransferSend>,
    ) -> Result<(), TransferError> {
        let data_device_manager = self.data_device_manager_state.as_ref().ok_or(
            NotSupportedError::new("Tried to set the clipboard, but data device not enabled"),
        )?;

        let mime_types = send_data.available_types().into_iter().flat_map(MimeType::from_dyn);

        let data_source = data_device_manager.create_copy_paste_source(queue_handle, mime_types);

        let (data_device, serial) = self
            .seats
            .values()
            .find_map(|seat| Some((seat.data_device()?, seat.latest_serial()?)))
            .ok_or(NotSupportedError::new(
                "Tried to set the clipboard, but no seat has received input",
            ))?;

        data_source.set_selection(data_device, serial);

        self.data_transfer_state.send_clipboard =
            Some(ClipboardSource { data_source, data: send_data });

        Ok(())
    }

    pub(crate) fn primary_selection(&self) -> Result<Option<DataTransferId>, TransferError> {
        self.primary_selection_manager_state.as_ref().ok_or(NotSupportedError::new(
            "Tried to read the primary selection, but it is not supported",
        ))?;

        Ok(self.data_transfer_state.primary_selection_id())
    }

    pub(crate) fn set_primary_selection(
        &mut self,
        queue_handle: &QueueHandle<Self>,
        send_data: Box<dyn DataTransferSend>,
    ) -> Result<(), TransferError> {
        let manager = self.primary_selection_manager_state.as_ref().ok_or(
            NotSupportedError::new("Tried to set the primary selection, but it is not supported"),
        )?;

        let mime_types = send_data.available_types().into_iter().flat_map(MimeType::from_dyn);

        let data_source = manager.create_selection_source(queue_handle, mime_types);

        let (device, serial) = self
            .seats
            .values()
            .find_map(|seat| Some((seat.primary_selection_device()?, seat.latest_serial()?)))
            .ok_or(NotSupportedError::new(
                "Tried to set the primary selection, but no seat has received input",
            ))?;

        data_source.set_selection(device, serial);

        self.data_transfer_state.send_primary_selection =
            Some(PrimarySelectionSource { data_source, data: send_data });

        Ok(())
    }
}

fn dnd_action_winit_to_wl(winit: DndAction) -> WlDndAction {
    match winit {
        DndAction::Move => WlDndAction::Move,
        DndAction::Copy => WlDndAction::Copy,
        DndAction::Ask => WlDndAction::Ask,
        _ => WlDndAction::empty(),
    }
}

fn dnd_action_wl_to_winit(wl: WlDndAction) -> Option<DndAction> {
    match wl {
        WlDndAction::Move => Some(DndAction::Move),
        WlDndAction::Copy => Some(DndAction::Copy),
        WlDndAction::Ask => Some(DndAction::Ask),
        _ => None,
    }
}

/// The actions of a set of `wl_data_device_manager.dnd_action` flags.
fn dnd_actions_wl_to_winit(wl: WlDndAction) -> &'static [DndAction] {
    use DndAction::{Ask, Copy, Move};

    let flag = |action| wl.contains(action);
    match (flag(WlDndAction::Move), flag(WlDndAction::Copy), flag(WlDndAction::Ask)) {
        (false, false, false) => &[],
        (true, false, false) => &[Move],
        (false, true, false) => &[Copy],
        (false, false, true) => &[Ask],
        (true, true, false) => &[Move, Copy],
        (true, false, true) => &[Move, Ask],
        (false, true, true) => &[Copy, Ask],
        (true, true, true) => &[Move, Copy, Ask],
    }
}

#[derive(Debug)]
struct OfferHandle {
    data: WlDataOffer,
    serial: u32,
}

impl Deref for OfferHandle {
    type Target = WlDataOffer;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

#[derive(Debug)]
struct DragSession {
    offer: OfferHandle,
    /// The data offered, and the actions its source allows.
    data: DataOffer,
    window_id: WindowId,
    selected_action: WlDndAction,
    accepted: bool,
    fetches: HashSet<AsyncRequestSerial>,
    phase: Phase,
}

#[derive(Debug, PartialEq, Eq)]
enum Phase {
    Hovering,
    Dropped,
    Concluding,
}

impl DragSession {
    fn new(offer: OfferHandle, data: DataOffer, window_id: WindowId) -> Self {
        Self {
            offer,
            data,
            window_id,
            selected_action: WlDndAction::empty(),
            accepted: false,
            fetches: HashSet::default(),
            phase: Phase::Hovering,
        }
    }

    fn proposed_action(&self) -> Option<DndAction> {
        // `selected_action` should only contain a single flag, but we check with `contains`
        // just in case we or the compositor misunderstood the spec.
        if self.selected_action.contains(WlDndAction::Move) {
            Some(DndAction::Move)
        } else if self.selected_action.contains(WlDndAction::Copy) {
            Some(DndAction::Copy)
        } else if self.selected_action.contains(WlDndAction::Ask) {
            Some(DndAction::Ask)
        } else {
            None
        }
    }

    fn set_actions(&mut self, action_set: &[DndAction]) {
        let source_actions = self.data.actions_allowed_by_source();
        let preferred_action = action_set.iter().find_map(|winit| {
            let wl = dnd_action_winit_to_wl(*winit);
            source_actions.intersects(wl).then_some(wl)
        });

        let all_actions = action_set
            .iter()
            .copied()
            .map(dnd_action_winit_to_wl)
            .fold(WlDndAction::empty(), BitOr::bitor);

        self.offer.set_actions(all_actions, preferred_action.unwrap_or(WlDndAction::empty()));

        // Some compositors won't even send the "dropped" event if no type
        // has been accepted, so we need to accept _something_ here. The
        // application can accept further types by fetching the data, but
        // this will at least mean that waiting until the drop to start
        // fetching data won't prevent the drop from working at all.
        let accepted_type =
            preferred_action.and(self.data.first_mime_type()).map(|mime| mime.to_string());
        self.accepted = accepted_type.is_some();
        self.offer.accept(self.offer.serial, accepted_type);
    }

    fn start_fetch(&mut self, mime_type: String, writefd: OwnedFd, serial: AsyncRequestSerial) {
        self.accepted = true;
        self.offer.accept(self.offer.serial, Some(mime_type.clone()));
        receive_to_fd(&self.offer, mime_type, writefd);
        self.fetches.insert(serial);
    }

    fn ready_to_conclude(&self) -> bool {
        self.phase == Phase::Concluding && self.fetches.is_empty()
    }

    fn conclude(self) {
        if self.accepted && !self.selected_action.is_empty() && self.offer.version() >= 3 {
            self.offer.finish();
        }
        self.offer.destroy();
    }

    fn abort(self) {
        self.offer.destroy();
    }
}

#[derive(Debug)]
pub struct DragSource {
    data_transfer_id: DataTransferId,
    data_source: SctkDragSource,
    data: Box<dyn DataTransferSend>,
    selected_action: WlDndAction,
    /// Whether the drop was performed; the drag then ends when the target finishes it.
    dropped: bool,
    window_id: WindowId,
    _icon: Option<Surface>,
}

impl DragSource {
    fn new(
        data_transfer_id: DataTransferId,
        data_source: SctkDragSource,
        data: Box<dyn DataTransferSend>,
        icon: Option<Surface>,
        window_id: WindowId,
    ) -> Self {
        Self {
            data_transfer_id,
            data_source,
            data,
            selected_action: WlDndAction::None,
            dropped: false,
            window_id,
            _icon: icon,
        }
    }
}

impl WinitState {
    pub(crate) fn start_drag(
        &mut self,
        queue_handle: &QueueHandle<Self>,
        source: WindowId,
        send_data: Box<dyn DataTransferSend>,
        action_mask: &[DndAction],
        icon: Option<DragIcon>,
    ) -> Result<DataTransferId, TransferError> {
        let dnd_actions = action_mask
            .iter()
            .copied()
            .map(dnd_action_winit_to_wl)
            .fold(WlDndAction::empty(), BitOr::bitor);

        let mime_types = send_data.available_types().into_iter().flat_map(MimeType::from_dyn);

        let data_device_manager = self
            .data_device_manager_state
            .as_ref()
            .ok_or(NotSupportedError::new("Tried to initiate drag, but data device not enabled"))?;
        let data_source =
            data_device_manager.create_drag_and_drop_source(queue_handle, mime_types, dnd_actions);

        let icon_surface = icon.and_then(|icon| {
            let rgba = icon.icon.cast_ref::<RgbaIcon>()?;

            let width = rgba.width().try_into().ok()?;
            let height = rgba.height().try_into().ok()?;

            let mut pool = self.image_pool.lock().unwrap();
            let buffer =
                image_to_buffer(width, height, rgba.buffer(), Format::Argb8888, &mut pool).ok()?;

            let surface = self.compositor_state.create_surface(queue_handle);
            if surface.version() >= 5 {
                buffer.attach_to(&surface).ok()?;
                surface.offset(icon.offset_x, icon.offset_y);
            } else {
                surface.attach(Some(buffer.wl_buffer()), icon.offset_x, icon.offset_y);
            }

            Some(Surface::from(surface))
        });

        let windows = self.windows.borrow();
        let window = windows
            .get(&source)
            .ok_or(os_error!("Tried to initiate drag, but source window ID was invalid"))?
            .lock()
            .unwrap();

        const NO_POINTER_CAP: &str =
            "Tried to initiate drag, but source window does not have the pointer capability";

        let seat = window
            .focused_seats()
            // HACK: How do we get the correct seat for pointers here?
            .find_map(|seat| self.seats.get(seat).filter(|seat| seat.data_device().is_some()))
            .ok_or(NotSupportedError::new(NO_POINTER_CAP))?;
        let data_device = seat.data_device().ok_or(NotSupportedError::new(NO_POINTER_CAP))?;

        let serial = seat
            .pointer_data()
            .ok_or(NotSupportedError::new(NO_POINTER_CAP))?
            .latest_button_serial()
            .unwrap_or_default();

        data_source.start_drag(
            data_device,
            window.window.wl_surface(),
            icon_surface.as_ref().map(Surface::wl_surface),
            serial,
        );
        let transfer_id = make_data_transfer_id(data_device.inner().id(), serial);

        // For some reason, if we commit before starting the drag then the offset isn't applied.
        // This doesn't seem to be documented anywhere, and it's possible that it's a bug in KDE.
        if let Some(surface) = &icon_surface {
            surface.wl_surface().commit();
        }

        let drag = DragSource::new(transfer_id, data_source, send_data, icon_surface, source);
        // An earlier drag that was never dropped cannot end any more: the compositor ignored its
        // `start_drag` without cancelling it. Dropping it destroys its data source.
        for abandoned in self.data_transfer_state.send_drags.start(drag, |drag| drag.dropped) {
            self.events_sink.push_window_event(
                WindowEvent::OutgoingDragCanceled { id: abandoned.data_transfer_id },
                abandoned.window_id,
            );
        }

        Ok(transfer_id)
    }
}

impl DataTransferState {
    pub(crate) fn settle_drops(&mut self) {
        for session in self.drags.values_mut() {
            if session.phase == Phase::Dropped {
                session.phase = Phase::Concluding;
            }
        }

        let concluded: Vec<ObjectId> = self
            .drags
            .iter()
            .filter(|(_, session)| session.ready_to_conclude())
            .map(|(device, _)| device.clone())
            .collect();

        for device in concluded {
            self.drags.remove(&device).unwrap().conclude();
        }
    }

    fn set_target_drag_action(&mut self, source: &WlDataSource, action: WlDndAction) {
        if let Some(drag) = self.send_drags.find_mut(|drag| drag.data_source.inner() == source) {
            drag.selected_action = action;
        }
    }

    /// Removes the drag started with `source`, which has ended.
    fn take_send_drag(&mut self, source: &WlDataSource) -> Option<DragSource> {
        self.send_drags.take(|drag| drag.data_source.inner() == source)
    }
}

impl DataOfferHandler for WinitState {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        offer: &mut DragOffer,
        actions: WlDndAction,
    ) {
        if let Some(session) = self
            .data_transfer_state
            .drags
            .values_mut()
            .find(|session| session.offer.data == *offer.inner())
        {
            session.data.set_source_actions(actions);
        }
    }

    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        offer: &mut DragOffer,
        actions: WlDndAction,
    ) {
        if let Some(session) = self
            .data_transfer_state
            .drags
            .values_mut()
            .find(|session| session.offer.data == *offer.inner())
        {
            session.selected_action = actions;
        }
    }
}

impl DataDeviceHandler for WinitState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
        x: f64,
        y: f64,
        wl_surface: &WlSurface,
    ) {
        let Some(data) = data_device.data::<DataDeviceData>() else {
            return;
        };

        let Some(drag) = data.drag_offer() else {
            return;
        };

        let device = data_device.id();
        let window_id = crate::make_wid(wl_surface);

        let view = drag
            .with_mime_types(|types| {
                DataOffer::new(types, make_data_transfer_id(device.clone(), drag.serial))
            })
            .dragged(drag.inner().version(), drag.source_actions);
        let offer = OfferHandle { data: drag.inner().clone(), serial: drag.serial };

        let id = view.transfer_id();

        let mut session = DragSession::new(offer, view, window_id);
        session.set_actions(&[]);

        if let Some(old) = self.data_transfer_state.drags.insert(device, session) {
            old.abort();
        }

        let scale_factor = self
            .windows
            .borrow()
            .get(&window_id)
            .map(|window| window.lock().unwrap().scale_factor())
            .unwrap_or(1.);
        let position: PhysicalPosition<f64> = LogicalPosition::new(x, y).to_physical(scale_factor);

        self.events_sink.push_window_event(
            WindowEvent::DragEntered { id, position: Some(position) },
            window_id,
        );
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, data_device: &WlDataDevice) {
        if let Entry::Occupied(entry) = self.data_transfer_state.drags.entry(data_device.id()) {
            if entry.get().phase == Phase::Hovering {
                let session = entry.remove();

                self.events_sink.push_window_event(
                    WindowEvent::DragLeft { id: session.data.transfer_id() },
                    session.window_id,
                );

                session.abort();
            }
        }
    }

    fn motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        let Some(session) = self.data_transfer_state.drags.get(&data_device.id()) else {
            return;
        };

        let id = session.data.transfer_id();
        let window_id = session.window_id;
        let proposed_action = session.proposed_action();

        let scale_factor = self
            .windows
            .borrow()
            .get(&window_id)
            .map(|window| window.lock().unwrap().scale_factor())
            .unwrap_or(1.);
        let position: PhysicalPosition<f64> = LogicalPosition::new(x, y).to_physical(scale_factor);

        self.events_sink.push_window_event(
            WindowEvent::DragPosition { id, position, proposed_action },
            window_id,
        );
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, data_device: &WlDataDevice) {
        let device = data_device.id();

        let serial = &mut self.data_transfer_state.clipboard_serial;
        *serial = serial.wrapping_add(1);
        let serial = *serial;

        let offer = data_device.data::<DataDeviceData>().and_then(|data| data.selection_offer());
        let Some(offer) = offer else {
            self.data_transfer_state.clipboards.remove(&device);
            return;
        };

        let Some(seat) = self.seats.iter().find_map(|(seat, state)| {
            (state.data_device()?.inner() == data_device).then(|| seat.clone())
        }) else {
            return;
        };

        let session = ClipboardSession {
            data: offer.with_mime_types(|types| {
                DataOffer::new(types, make_data_transfer_id(offer.inner().id(), serial))
            }),
            offer: SelectionOffer::Clipboard(offer),
            seat,
            fetches: HashSet::default(),
        };

        self.data_transfer_state.clipboards.insert(device, session);
    }

    fn drop_performed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
    ) {
        let Some(session) = self.data_transfer_state.drags.get_mut(&data_device.id()) else {
            return;
        };

        session.phase = Phase::Dropped;

        self.events_sink.push_window_event(
            WindowEvent::DragDropped {
                id: session.data.transfer_id(),
                proposed_action: session.proposed_action(),
            },
            session.window_id,
        );
    }
}

impl PrimarySelectionDeviceHandler for WinitState {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &ZwpPrimarySelectionDeviceV1,
    ) {
        let device_id = device.id();

        let serial = &mut self.data_transfer_state.clipboard_serial;
        *serial = serial.wrapping_add(1);
        let serial = *serial;

        let Some(data) = device.data::<PrimarySelectionDeviceData>() else {
            return;
        };

        let Some(offer) = data.selection_offer() else {
            self.data_transfer_state.primary_selections.remove(&device_id);
            return;
        };

        let session = ClipboardSession {
            data: offer.with_mime_types(|types| {
                DataOffer::new(types, make_data_transfer_id(device_id.clone(), serial))
            }),
            offer: SelectionOffer::Primary(offer),
            seat: data.seat().id(),
            fetches: HashSet::default(),
        };

        self.data_transfer_state.primary_selections.insert(device_id, session);
    }
}

impl PrimarySelectionSourceHandler for WinitState {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
        mime: String,
        fd: WritePipe,
    ) {
        let Some(primary) = self
            .data_transfer_state
            .send_primary_selection
            .as_ref()
            .filter(|primary| primary.data_source.inner() == source)
        else {
            return;
        };

        let mime = MimeType::parse(mime);

        let Some(send_data) = primary.data.data_for_type(&mime) else {
            return;
        };

        self.write_send_data(mime, send_data, fd);
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &ZwpPrimarySelectionSourceV1,
    ) {
        self.data_transfer_state
            .send_primary_selection
            .take_if(|primary| primary.data_source.inner() == source);
    }
}

#[cfg(test)]
mod tests;
