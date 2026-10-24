use std::collections::hash_map::DefaultHasher;
use std::ops::Range;

use windows_sys::Win32::Graphics::Gdi::{BI_PNG, BI_RLE8};
use windows_sys::Win32::System::Com::{DVASPECT_ICON, TYMED_GDI};

use super::*;

const HGLOBAL: u32 = TYMED_HGLOBAL as u32;
const ISTREAM: u32 = TYMED_ISTREAM as u32;
const GDI: u32 = TYMED_GDI as u32;

/// An id in the range of registered formats, which no test registers.
const REGISTERED_ID: u16 = 0xc123;

const PNG: TypeHint = TypeHint::Image { extension_hint: Some("png") };
const BMP: TypeHint = TypeHint::Image { extension_hint: Some("bmp") };

fn listing(id: u16, media: u32) -> FormatListing {
    FormatListing { id, aspect: DVASPECT_CONTENT, index: -1, media }
}

fn available(format: ClipboardFormat) -> AvailableFormat {
    AvailableFormat { format, media: HGLOBAL }
}

fn ids(formats: &[&AvailableFormat]) -> Vec<u16> {
    formats.iter().map(|available| available.format.id()).collect()
}

fn hash(format: &ClipboardFormat) -> u64 {
    let mut hasher = DefaultHasher::new();
    format.hash(&mut hasher);
    hasher.finish()
}

#[test]
fn standard_formats_are_named_after_their_constants() {
    assert_eq!(ClipboardFormat::from_id(CF_UNICODETEXT).unwrap().name(), "CF_UNICODETEXT");
    assert_eq!(ClipboardFormat::from_id(CF_DIBV5).unwrap().to_string(), "CF_DIBV5");
}

#[test]
fn private_formats_are_not_formats() {
    assert_eq!(ClipboardFormat::from_id(0x0200), None);
    assert_eq!(ClipboardFormat::from_id(0), None);
}

#[test]
fn a_registered_format_is_found_by_its_id() {
    let png = ClipboardFormat::register("PNG").unwrap();
    assert!(png.id() >= FIRST_REGISTERED_ID);
    let found = ClipboardFormat::from_id(png.id()).unwrap();
    assert_eq!(found, png);
    assert!(found.name().eq_ignore_ascii_case("PNG"));
    assert_eq!(found.hint(), Some(PNG));
}

#[test]
fn known_formats_have_hints() {
    let hint = |id, name| ClipboardFormat::new(id, name).hint();
    assert_eq!(hint(CF_UNICODETEXT, "CF_UNICODETEXT"), Some(TypeHint::Plaintext));
    assert_eq!(hint(CF_HDROP, "CF_HDROP"), Some(TypeHint::UriList));
    assert_eq!(hint(REGISTERED_ID, "PNG"), Some(PNG));
    // Registered names are case-insensitive.
    assert_eq!(hint(REGISTERED_ID, "png"), Some(PNG));
    assert_eq!(hint(REGISTERED_ID, "application/x-example"), None);
    assert_eq!(hint(CF_LOCALE, "CF_LOCALE"), None);
    // Only a registered format is known by its name.
    assert_eq!(hint(CF_SYLK, "PNG"), None);
}

#[test]
fn formats_are_equal_when_their_ids_are() {
    let format = ClipboardFormat::new(REGISTERED_ID, "PNG");
    let same = ClipboardFormat::new(REGISTERED_ID, "png");
    let other = ClipboardFormat::new(REGISTERED_ID + 1, "PNG");
    assert_eq!(format, same);
    assert_eq!(hash(&format), hash(&same));
    assert_ne!(format, other);
}

#[test]
fn a_format_matches_formats_by_id_and_other_types_by_hint() {
    let text = ClipboardFormat::new(CF_UNICODETEXT, "CF_UNICODETEXT");
    let png = ClipboardFormat::new(REGISTERED_ID, "PNG");
    let custom = ClipboardFormat::new(REGISTERED_ID + 1, "application/x-example");
    assert!(text.matches(&TypeHint::Plaintext));
    assert!(!text.matches(&TypeHint::Html));
    assert!(png.matches(&TypeHint::Image { extension_hint: None }));
    assert!(!png.matches(&TypeHint::Image { extension_hint: Some("bmp") }));
    assert!(custom.matches(&ClipboardFormat::new(REGISTERED_ID + 1, "APPLICATION/X-EXAMPLE")));
    assert!(!custom.matches(&png));
    assert!(!custom.matches(&TypeHint::Plaintext));
}

#[test]
fn a_format_is_read_as_itself() {
    let formats = [
        available(ClipboardFormat::new(CF_UNICODETEXT, "CF_UNICODETEXT")),
        available(ClipboardFormat::new(REGISTERED_ID, "application/x-example")),
    ];
    let requested = ClipboardFormat::new(REGISTERED_ID, "application/x-example");
    assert_eq!(ids(&candidates(&formats, &requested)), [REGISTERED_ID]);
    let missing = ClipboardFormat::new(REGISTERED_ID + 1, "application/x-missing");
    assert!(candidates(&formats, &missing).is_empty());
}

#[test]
fn a_hint_is_read_from_the_formats_with_that_hint() {
    let formats = [
        available(ClipboardFormat::new(CF_LOCALE, "CF_LOCALE")),
        available(ClipboardFormat::new(REGISTERED_ID, "PNG")),
        available(ClipboardFormat::new(CF_UNICODETEXT, "CF_UNICODETEXT")),
        available(ClipboardFormat::new(CF_HDROP, "CF_HDROP")),
    ];
    assert_eq!(ids(&candidates(&formats, &TypeHint::Plaintext)), [CF_UNICODETEXT]);
    assert_eq!(ids(&candidates(&formats, &TypeHint::UriList)), [CF_HDROP]);
    let any_image = TypeHint::Image { extension_hint: None };
    assert_eq!(ids(&candidates(&formats, &any_image)), [REGISTERED_ID]);
    assert!(candidates(&formats, &TypeHint::Html).is_empty());
}

#[test]
fn formats_that_cannot_be_read_as_bytes_are_left_out() {
    let listings = [listing(CF_BITMAP, GDI), listing(CF_DIB, HGLOBAL), listing(CF_TIFF, ISTREAM)];
    assert_eq!(readable_formats(&listings), [(CF_DIB, HGLOBAL), (CF_TIFF, ISTREAM)]);
}

#[test]
fn a_format_listed_twice_is_read_in_the_media_of_both_listings() {
    let listings = [
        listing(CF_UNICODETEXT, ISTREAM),
        listing(CF_TEXT, HGLOBAL),
        listing(CF_UNICODETEXT, HGLOBAL | GDI),
        listing(CF_DIB, GDI),
        listing(CF_DIB, ISTREAM),
    ];
    assert_eq!(readable_formats(&listings), [
        (CF_UNICODETEXT, ISTREAM | HGLOBAL),
        (CF_TEXT, HGLOBAL),
        (CF_DIB, ISTREAM),
    ]);
}

#[test]
fn listings_of_part_of_the_content_are_ignored() {
    let icon = FormatListing { aspect: DVASPECT_ICON, ..listing(CF_DIB, HGLOBAL) };
    let page = FormatListing { index: 2, ..listing(CF_TEXT, HGLOBAL) };
    let listings = [icon, page, listing(CF_UNICODETEXT, HGLOBAL), listing(CF_DIB, ISTREAM)];
    assert_eq!(readable_formats(&listings), [(CF_UNICODETEXT, HGLOBAL), (CF_DIB, ISTREAM)]);
}

#[test]
fn utf16_text_ends_at_its_nul_and_drops_an_odd_byte() {
    let utf16 = |text: &str| text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<_>>();
    assert_eq!(utf16_until_nul(&utf16("caf\u{e9} \u{1f600}\0junk")), "caf\u{e9} \u{1f600}");
    let mut odd = utf16("ab");
    odd.push(b'c');
    assert_eq!(utf16_until_nul(&odd), "ab");
    assert_eq!(utf16_until_nul(&[]), "");
}

#[test]
fn the_known_formats_are_the_formats_winit_converts() {
    let known = known_formats();
    let names: Vec<_> = known.iter().map(ClipboardFormat::name).collect();
    assert_eq!(names, [
        "CF_UNICODETEXT",
        "CF_TEXT",
        "CF_OEMTEXT",
        "CF_HDROP",
        "CF_DIBV5",
        "CF_DIB",
        "CF_TIFF",
        "CF_WAVE",
        "HTML Format",
        "Rich Text Format",
        "UniformResourceLocatorW",
        "PNG"
    ]);
    assert!(known.iter().all(|format| format.hint().is_some()));
}

#[test]
fn the_formats_winit_decodes_have_hints() {
    let hint = |id, name| ClipboardFormat::new(id, name).hint();
    assert_eq!(hint(CF_TEXT, "CF_TEXT"), Some(TypeHint::Plaintext));
    assert_eq!(hint(CF_OEMTEXT, "CF_OEMTEXT"), Some(TypeHint::Plaintext));
    assert_eq!(hint(REGISTERED_ID, "HTML Format"), Some(TypeHint::Html));
    assert_eq!(hint(REGISTERED_ID, "rich text format"), Some(TypeHint::Rtf));
    assert_eq!(hint(REGISTERED_ID, "UniformResourceLocatorW"), Some(TypeHint::UriList));
    assert_eq!(hint(CF_DIBV5, "CF_DIBV5"), Some(BMP));
    assert_eq!(hint(CF_DIB, "CF_DIB"), Some(BMP));
    assert_eq!(hint(CF_TIFF, "CF_TIFF"), Some(TypeHint::Image { extension_hint: Some("tiff") }));
    assert_eq!(hint(CF_WAVE, "CF_WAVE"), Some(TypeHint::Audio { extension_hint: Some("wav") }));
    // The ANSI form of the URL format is not decoded.
    assert_eq!(hint(REGISTERED_ID, "UniformResourceLocator"), None);
}

#[test]
fn a_hint_is_read_from_its_most_faithful_format_first() {
    let formats = [
        available(ClipboardFormat::new(CF_OEMTEXT, "CF_OEMTEXT")),
        available(ClipboardFormat::new(CF_TIFF, "CF_TIFF")),
        available(ClipboardFormat::new(CF_DIB, "CF_DIB")),
        available(ClipboardFormat::new(CF_TEXT, "CF_TEXT")),
        available(ClipboardFormat::new(REGISTERED_ID, "UniformResourceLocatorW")),
        available(ClipboardFormat::new(CF_DIBV5, "CF_DIBV5")),
        available(ClipboardFormat::new(CF_UNICODETEXT, "CF_UNICODETEXT")),
        available(ClipboardFormat::new(REGISTERED_ID + 1, "PNG")),
        available(ClipboardFormat::new(CF_HDROP, "CF_HDROP")),
    ];
    let plaintext = candidates(&formats, &TypeHint::Plaintext);
    assert_eq!(ids(&plaintext), [CF_UNICODETEXT, CF_TEXT, CF_OEMTEXT]);
    assert_eq!(ids(&candidates(&formats, &TypeHint::UriList)), [CF_HDROP, REGISTERED_ID]);
    let any_image = candidates(&formats, &TypeHint::Image { extension_hint: None });
    assert_eq!(ids(&any_image), [REGISTERED_ID + 1, CF_DIBV5, CF_DIB, CF_TIFF]);
    assert_eq!(ids(&candidates(&formats, &BMP)), [CF_DIBV5, CF_DIB]);
}

fn decoded_text(id: u16, name: &str, bytes: &[u8]) -> String {
    decoded_text_in(id, name, bytes, None)
}

/// The text of `bytes` in format `id`, from a source whose `CF_LOCALE` is `locale`.
fn decoded_text_in(id: u16, name: &str, bytes: &[u8], locale: Option<u32>) -> String {
    match decode(&ClipboardFormat::new(id, name), Cow::Borrowed(bytes), locale) {
        Some(Payload::Text(text)) => text,
        other => panic!("{other:?}"),
    }
}

/// Russian, whose ANSI code page is 1251 and OEM code page is 866.
const RUSSIAN: u32 = 0x0419;
/// Hindi, which has no ANSI or OEM code page.
const HINDI: u32 = 0x0439;
/// "Privet" in Cyrillic.
const PRIVET: &str = "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}";

#[test]
fn the_code_page_of_code_page_text_is_that_of_its_locale() {
    assert_eq!(text_code_page(KnownFormat::AnsiText, Some(RUSSIAN)), 1251);
    assert_eq!(text_code_page(KnownFormat::OemText, Some(RUSSIAN)), 866);
    // Without a locale, or with one without such a code page, it is this system's.
    assert_eq!(text_code_page(KnownFormat::AnsiText, None), CP_ACP);
    assert_eq!(text_code_page(KnownFormat::OemText, None), CP_OEMCP);
    assert_eq!(text_code_page(KnownFormat::AnsiText, Some(HINDI)), CP_ACP);
    assert_eq!(text_code_page(KnownFormat::OemText, Some(HINDI)), CP_OEMCP);
}

#[test]
fn code_page_text_is_decoded_in_the_code_page_of_its_locale() {
    let ansi = b"\xcf\xf0\xe8\xe2\xe5\xf2\0";
    assert_eq!(decoded_text_in(CF_TEXT, "CF_TEXT", ansi, Some(RUSSIAN)), PRIVET);
    let oem = b"\x8f\xe0\xa8\xa2\xa5\xe2\0";
    assert_eq!(decoded_text_in(CF_OEMTEXT, "CF_OEMTEXT", oem, Some(RUSSIAN)), PRIVET);
}

#[test]
fn code_page_text_ends_at_its_nul() {
    assert_eq!(decoded_text(CF_TEXT, "CF_TEXT", b"abc\0junk"), "abc");
    assert_eq!(decoded_text(CF_OEMTEXT, "CF_OEMTEXT", b"abc"), "abc");
    assert_eq!(decoded_text(CF_TEXT, "CF_TEXT", b""), "");
}

#[test]
fn rtf_ends_at_its_nul() {
    let rtf = decoded_text(REGISTERED_ID, "Rich Text Format", b"{\\rtf1 caf\\'e9}\0\0");
    assert_eq!(rtf, "{\\rtf1 caf\\'e9}");
}

#[test]
fn a_url_is_read_as_a_uri_list() {
    let format = ClipboardFormat::new(REGISTERED_ID, "UniformResourceLocatorW");
    let utf16 = " https://example.com/a?b \0junk".encode_utf16().flat_map(u16::to_le_bytes);
    match decode(&format, Cow::Owned(utf16.collect()), None) {
        Some(Payload::Uris(uris)) => assert_eq!(uris, ["https://example.com/a?b"]),
        other => panic!("{other:?}"),
    }
    assert!(decode(&format, Cow::Borrowed(&[0, 0]), None).is_none());
}

const DOCUMENT: &str = "<html><head><style>b{}</style></head><body><!--StartFragment--><b>x</b><!\
                        --EndFragment--></body></html>";

fn fragment() -> Range<usize> {
    let start = DOCUMENT.find("<b>").unwrap();
    start..start + "<b>x</b>".len()
}

/// `html` in the HTML Clipboard Format, with its context and fragment given as ranges of `html`,
/// or no context.
fn html_clipboard(html: &str, context: Option<Range<usize>>, fragment: Range<usize>) -> Vec<u8> {
    const DESCRIPTION_LEN: usize = 105;
    let offset = |at: usize| (DESCRIPTION_LEN + at) as i64;
    let (start_html, end_html) =
        context.map_or((-1, -1), |context| (offset(context.start), offset(context.end)));
    let (start_fragment, end_fragment) = (offset(fragment.start), offset(fragment.end));
    let data = format!(
        "Version:0.9\r\nStartHTML:{start_html:010}\r\nEndHTML:{end_html:010}\r\nStartFragment:\
         {start_fragment:010}\r\nEndFragment:{end_fragment:010}\r\n{html}"
    );
    assert_eq!(data.len(), DESCRIPTION_LEN + html.len());
    data.into_bytes()
}

#[test]
fn html_is_read_from_its_context() {
    let data = html_clipboard(DOCUMENT, Some(0..DOCUMENT.len()), fragment());
    assert_eq!(html_clipboard_document(&data), Some(DOCUMENT.as_bytes()));
}

#[test]
fn html_without_a_context_is_read_from_its_fragment() {
    let data = html_clipboard(DOCUMENT, None, fragment());
    assert_eq!(html_clipboard_document(&data), Some(&b"<b>x</b>"[..]));
}

#[test]
fn an_html_context_past_the_end_of_the_data_ends_with_the_data() {
    let mut data = html_clipboard(DOCUMENT, Some(0..DOCUMENT.len() + 40), fragment());
    // A global memory object can be larger than the data it was allocated for.
    data.extend_from_slice(&[0; 3]);
    assert_eq!(html_clipboard_document(&data), Some(DOCUMENT.as_bytes()));
}

#[test]
fn html_in_a_backwards_region_is_not_read() {
    let backwards = Range { start: 10, end: 4 };
    let data = html_clipboard(DOCUMENT, Some(backwards.clone()), fragment());
    assert_eq!(html_clipboard_document(&data), Some(&b"<b>x</b>"[..]));
    let data = html_clipboard(DOCUMENT, Some(backwards.clone()), backwards);
    assert_eq!(html_clipboard_document(&data), None);
}

#[test]
fn the_first_occurrence_of_an_html_offset_counts() {
    const HTML: &str = "<p>a</p><p>b</p>";
    // Two `StartFragment` lines, at the two paragraphs, and an `EndFragment` after both.
    let data = |starts: [usize; 2]| {
        let description = |starts: [usize; 2], end: usize| {
            [
                "StartHTML:-1".to_owned(),
                "EndHTML:-1".to_owned(),
                format!("StartFragment:{:04}", starts[0]),
                format!("StartFragment:{:04}", starts[1]),
                format!("EndFragment:{end:04}"),
            ]
            .map(|line| line + "\r\n")
            .concat()
        };
        let len = description([0, 0], 0).len();
        description(starts.map(|start| len + start), len + HTML.len()) + HTML
    };
    assert_eq!(html_clipboard_document(data([0, 8]).as_bytes()), Some(HTML.as_bytes()));
    assert_eq!(html_clipboard_document(data([8, 0]).as_bytes()), Some(&b"<p>b</p>"[..]));
}

/// The header of a packed bitmap.
#[derive(Clone, Copy)]
struct Header {
    len: u32,
    width: i32,
    height: i32,
    planes: u16,
    bit_count: u16,
    compression: u32,
    image_size: u32,
    colors_used: u32,
}

impl Header {
    /// The `BITMAPINFOHEADER` of an uncompressed bitmap.
    fn info(width: i32, height: i32, bit_count: u16) -> Self {
        let (compression, image_size, colors_used) = (BI_RGB, 0, 0);
        Self { len: 40, width, height, planes: 1, bit_count, compression, image_size, colors_used }
    }

    /// A packed bitmap of this header, followed by `rest` bytes.
    fn dib(self, rest: usize) -> Vec<u8> {
        let mut dib = vec![0; self.len as usize + rest];
        dib[..4].copy_from_slice(&self.len.to_le_bytes());
        if self.len == 12 {
            dib[4..6].copy_from_slice(&(self.width as u16).to_le_bytes());
            dib[6..8].copy_from_slice(&(self.height as u16).to_le_bytes());
            dib[8..10].copy_from_slice(&self.planes.to_le_bytes());
            dib[10..12].copy_from_slice(&self.bit_count.to_le_bytes());
        } else {
            dib[4..8].copy_from_slice(&self.width.to_le_bytes());
            dib[8..12].copy_from_slice(&self.height.to_le_bytes());
            dib[12..14].copy_from_slice(&self.planes.to_le_bytes());
            dib[14..16].copy_from_slice(&self.bit_count.to_le_bytes());
            dib[16..20].copy_from_slice(&self.compression.to_le_bytes());
            dib[20..24].copy_from_slice(&self.image_size.to_le_bytes());
            dib[32..36].copy_from_slice(&self.colors_used.to_le_bytes());
        }
        dib
    }
}

/// The `bfOffBits` of a BMP file.
fn pixels_at(bmp: &[u8]) -> u32 {
    u32::from_le_bytes(bmp[10..14].try_into().unwrap())
}

#[test]
fn a_bitmap_becomes_a_bmp_file() {
    // 2 by 2 pixels of 1 bit: two colors, then two rows, each padded to 4 bytes.
    let packed = Header::info(2, 2, 1).dib(2 * 4 + 2 * 4);
    let bmp = dib_to_bmp(&packed).unwrap();
    assert_eq!(&bmp[..2], b"BM");
    assert_eq!(u32::from_le_bytes(bmp[2..6].try_into().unwrap()) as usize, bmp.len());
    assert_eq!(pixels_at(&bmp), 14 + 40 + 2 * 4);
    assert_eq!(&bmp[14..], packed);
}

#[test]
fn the_color_table_comes_before_the_pixels() {
    // The 256 colors of a core header take three bytes each.
    let core = Header { len: 12, ..Header::info(3, 1, 8) }.dib(256 * 3 + 4);
    assert_eq!(pixels_at(&dib_to_bmp(&core).unwrap()), 14 + 12 + 256 * 3);
    // A bitmap that gives how many colors it uses has as many.
    let three = Header { colors_used: 3, ..Header::info(1, 1, 8) }.dib(3 * 4 + 4);
    assert_eq!(pixels_at(&dib_to_bmp(&three).unwrap()), 14 + 40 + 3 * 4);
    // A PNG bitmap has none.
    let png = Header { compression: BI_PNG, image_size: 10, ..Header::info(1, 1, 0) }.dib(10);
    assert_eq!(pixels_at(&dib_to_bmp(&png).unwrap()), 14 + 40);
}

#[test]
fn the_color_masks_after_an_info_header_come_before_the_pixels() {
    let three = Header { compression: BI_BITFIELDS, ..Header::info(2, 1, 32) }.dib(3 * 4 + 2 * 4);
    assert_eq!(pixels_at(&dib_to_bmp(&three).unwrap()), 14 + 40 + 3 * 4);
    let four = Header { compression: BI_ALPHABITFIELDS, ..Header::info(1, 1, 32) }.dib(4 * 4 + 4);
    assert_eq!(pixels_at(&dib_to_bmp(&four).unwrap()), 14 + 40 + 4 * 4);
    // A V5 header holds its masks. Its bitmap is top-down, with a negative height.
    let v5 = Header { len: 124, compression: BI_BITFIELDS, ..Header::info(1, -2, 32) }.dib(2 * 4);
    assert_eq!(pixels_at(&dib_to_bmp(&v5).unwrap()), 14 + 124);
}

#[test]
fn a_bitmap_that_does_not_hold_all_its_pixels_is_not_converted() {
    // Two rows of 3 pixels of 24 bits, each padded from 9 to 12 bytes.
    let rows = Header::info(3, 2, 24);
    assert!(dib_to_bmp(&rows.dib(2 * 12)).is_some());
    assert_eq!(dib_to_bmp(&rows.dib(2 * 12 - 1)), None);
    // 256 colors take 1024 bytes.
    assert_eq!(dib_to_bmp(&Header::info(1, 1, 8).dib(100)), None);
    // Compressed pixels have the size that the header gives, which it must give.
    let compressed = Header { compression: BI_RLE8, image_size: 8, ..Header::info(1, 1, 8) };
    assert!(dib_to_bmp(&compressed.dib(1024 + 8)).is_some());
    assert_eq!(dib_to_bmp(&Header { image_size: 9, ..compressed }.dib(1024 + 8)), None);
    assert_eq!(dib_to_bmp(&Header { image_size: 0, ..compressed }.dib(1024 + 8)), None);
}

#[test]
fn a_bitmap_with_an_invalid_header_is_not_converted() {
    let valid = Header::info(1, 1, 24);
    assert!(dib_to_bmp(&valid.dib(4)).is_some());
    assert_eq!(dib_to_bmp(&Header { width: 0, ..valid }.dib(4)), None);
    assert_eq!(dib_to_bmp(&Header { height: 0, ..valid }.dib(4)), None);
    assert_eq!(dib_to_bmp(&Header { planes: 0, ..valid }.dib(4)), None);
    assert_eq!(dib_to_bmp(&Header { len: 64, ..valid }.dib(4)), None);
    // Uncompressed pixels have a bit count of 1, 4, 8, 16, 24 or 32.
    assert_eq!(dib_to_bmp(&Header { bit_count: 0, ..valid }.dib(4)), None);
    assert_eq!(dib_to_bmp(&Header { colors_used: u32::MAX, ..valid }.dib(4)), None);
    let huge = Header { width: i32::MAX, height: i32::MIN, bit_count: 32, ..valid };
    assert_eq!(dib_to_bmp(&huge.dib(4)), None);
    assert_eq!(dib_to_bmp(&valid.dib(4)[..30]), None);
    assert_eq!(dib_to_bmp(&[]), None);
}
