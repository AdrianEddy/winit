use std::collections::hash_map::DefaultHasher;

use windows_sys::Win32::System::Com::{DVASPECT_ICON, TYMED_GDI};

use super::*;

const HGLOBAL: u32 = TYMED_HGLOBAL as u32;
const ISTREAM: u32 = TYMED_ISTREAM as u32;
const GDI: u32 = TYMED_GDI as u32;

/// An id in the range of registered formats, which no test registers.
const REGISTERED_ID: u16 = 0xc123;

const PNG: TypeHint = TypeHint::Image { extension_hint: Some("png") };

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
    assert_eq!(names, ["CF_UNICODETEXT", "CF_HDROP", "PNG"]);
    assert!(known.iter().all(|format| format.hint().is_some()));
}
