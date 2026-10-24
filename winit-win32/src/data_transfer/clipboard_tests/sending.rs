//! The formats that a data object of this process offers, and which offered type supplies the
//! data of each.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use windows_sys::Win32::Globalization::GetSystemDefaultLCID;
use windows_sys::Win32::System::Com::TYMED_HGLOBAL;
use windows_sys::Win32::System::Ole::{CF_DIB, CF_LOCALE, CF_OEMTEXT, CF_TEXT, CF_UNICODETEXT};
use winit_core::data_transfer::{
    DataTransferSend, DataTransferSendBuilder, SendData, TransferType, TypeHint, TypedData,
};
use winit_core::window::WindowId;

use super::test_source::utf16_text;
use super::{DRAG, FILE_URI, PNG};
use crate::clipboard_format::{
    ClipboardFormat, KnownFormat, RegisteredFormats, code_page_bytes, text_code_page,
};
use crate::data_transfer::{
    Advertised, ComPtr, DataTransferState, LockedGlobal, SourceDataObject, advertised_formats,
    ensure_ole_initialized, get_medium,
};
use crate::definitions::IDataObject;

/// Russian, whose ANSI code page is 1251 and OEM code page is 866.
const RUSSIAN: u32 = 0x0419;
/// "Privet" in Cyrillic.
const PRIVET: &str = "\u{41f}\u{440}\u{438}\u{432}\u{435}\u{442}";

/// Registered formats that winit sends, with ids that the tests of what is advertised use.
fn registered() -> RegisteredFormats {
    RegisteredFormats {
        html: Some(ClipboardFormat::new(0xc001, "HTML Format")),
        rtf: Some(ClipboardFormat::new(0xc002, "Rich Text Format")),
        png: Some(ClipboardFormat::new(0xc003, "PNG")),
    }
}

const CUSTOM: u16 = 0xc123;

/// The data of a type that is advertised but never read.
fn unread(_: &(), _: &dyn TransferType) -> Option<SendData> {
    None
}

fn converted(id: u16, name: &str, hint: TypeHint) -> Advertised {
    Advertised::Converted { format: ClipboardFormat::new(id, name), hint }
}

#[test]
fn a_format_that_winit_does_not_convert_is_advertised_raw() {
    let custom = ClipboardFormat::new(CUSTOM, "application/x-example");
    let send = DataTransferSendBuilder::new(()).with_type(custom.clone(), unread);
    assert_eq!(advertised_formats(&send, &registered()), [(CUSTOM, Advertised::Raw(custom))]);
}

#[test]
fn a_format_that_its_hint_converts_to_is_advertised_converted() {
    let text = ClipboardFormat::new(CF_TEXT, "CF_TEXT");
    let send = DataTransferSendBuilder::new(())
        .with_type(ClipboardFormat::new(CF_UNICODETEXT, "CF_UNICODETEXT"), unread)
        .with_type(ClipboardFormat::new(0xc001, "HTML Format"), unread)
        // Text is converted to `CF_UNICODETEXT`, so `CF_TEXT` is the application's own data.
        .with_type(text.clone(), unread);
    assert_eq!(advertised_formats(&send, &registered()), [
        (CF_UNICODETEXT, converted(CF_UNICODETEXT, "CF_UNICODETEXT", TypeHint::Plaintext)),
        (0xc001, converted(0xc001, "HTML Format", TypeHint::Html)),
        (CF_TEXT, Advertised::Raw(text)),
        (CF_LOCALE, Advertised::Locale(unsafe { GetSystemDefaultLCID() })),
    ]);
}

#[test]
fn raw_code_page_text_is_advertised_with_a_locale() {
    let text = ClipboardFormat::new(CF_OEMTEXT, "CF_OEMTEXT");
    let send = DataTransferSendBuilder::new(()).with_type(text.clone(), unread);
    let system = Advertised::Locale(unsafe { GetSystemDefaultLCID() });
    assert_eq!(advertised_formats(&send, &registered()), [
        (CF_OEMTEXT, Advertised::Raw(text.clone())),
        (CF_LOCALE, system)
    ]);
    // An application that offers a `CF_LOCALE` gives its own.
    let locale = ClipboardFormat::new(CF_LOCALE, "CF_LOCALE");
    let send = DataTransferSendBuilder::new(())
        .with_type(text.clone(), unread)
        .with_type(locale.clone(), unread);
    assert_eq!(advertised_formats(&send, &registered()), [
        (CF_OEMTEXT, Advertised::Raw(text)),
        (CF_LOCALE, Advertised::Raw(locale))
    ]);
    // Converted text needs none.
    let send = DataTransferSendBuilder::new(()).with_type(TypeHint::Plaintext, unread);
    let advertised = advertised_formats(&send, &registered());
    assert!(advertised.iter().all(|(id, _)| *id != CF_LOCALE), "{advertised:?}");
}

#[test]
fn a_format_is_advertised_once() {
    let send = DataTransferSendBuilder::new(())
        .with_type(TypeHint::Plaintext, unread)
        .with_type(ClipboardFormat::new(CF_UNICODETEXT, "CF_UNICODETEXT"), unread)
        .with_type(TypeHint::Image { extension_hint: None }, unread)
        .with_type(PNG, unread);
    assert_eq!(advertised_formats(&send, &registered()), [
        (CF_UNICODETEXT, converted(CF_UNICODETEXT, "CF_UNICODETEXT", TypeHint::Plaintext)),
        (0xc003, converted(0xc003, "PNG", PNG)),
    ]);
}

#[test]
fn rtf_is_advertised_as_rich_text_format() {
    let send = DataTransferSendBuilder::new(()).with_type(TypeHint::Rtf, unread);
    let rtf = converted(0xc002, "Rich Text Format", TypeHint::Rtf);
    assert_eq!(advertised_formats(&send, &registered()), [(0xc002, rtf)]);
}

/// The data object of the types that `send` offers.
fn data_object(send: Box<dyn DataTransferSend>) -> ComPtr<IDataObject> {
    ensure_ole_initialized().unwrap();
    let source = SourceDataObject::new(send);
    unsafe { ComPtr::from_borrowed(source.interface_ptr().cast()) }.unwrap()
}

/// The bytes that `data_object` gives for format `id`, in global memory.
fn served(data_object: &ComPtr<IDataObject>, id: u16) -> Vec<u8> {
    let medium = get_medium(data_object, id, TYMED_HGLOBAL as u32).unwrap();
    LockedGlobal::lock(unsafe { medium.0.u.hGlobal }).unwrap().bytes().to_vec()
}

/// The data of an offered type, which names it.
fn named(name: &'static str) -> impl Fn(&(), &dyn TransferType) -> Option<SendData> + Copy {
    move |_, _| Some(SendData::String(name.to_owned()))
}

fn named_bytes(name: &'static str) -> impl Fn(&(), &dyn TransferType) -> Option<SendData> + Copy {
    move |_, _| Some(SendData::Bytes(name.as_bytes().to_vec()))
}

fn file_list(_: &(), _: &dyn TransferType) -> Option<SendData> {
    Some(SendData::Uris(vec![FILE_URI.to_owned()]))
}

#[test]
fn a_raw_bitmap_is_supplied_by_its_own_type_and_not_by_an_image_type() {
    let png = RegisteredFormats::get().png.clone().unwrap();
    let dib = ClipboardFormat::new(CF_DIB, "CF_DIB");
    let any_image = TypeHint::Image { extension_hint: None };
    for image_first in [true, false] {
        let mut send = DataTransferSendBuilder::new(());
        if image_first {
            send.add_type(any_image, named_bytes("image"))
                .add_type(dib.clone(), named_bytes("dib"));
        } else {
            send.add_type(dib.clone(), named_bytes("dib"))
                .add_type(any_image, named_bytes("image"));
        }
        let data_object = data_object(send.build());
        assert_eq!(served(&data_object, CF_DIB), b"dib", "image first: {image_first}");
        assert_eq!(served(&data_object, png.id()), b"image", "image first: {image_first}");
    }
}

#[test]
fn text_is_supplied_by_the_type_that_offers_its_format() {
    let ansi = ClipboardFormat::new(CF_TEXT, "CF_TEXT");
    let unicode = ClipboardFormat::new(CF_UNICODETEXT, "CF_UNICODETEXT");
    // The types in the order they are offered, and the one that supplies `CF_UNICODETEXT`.
    let orders = [
        (["plain", "ansi", "unicode"], "plain"),
        (["ansi", "plain", "unicode"], "plain"),
        (["unicode", "ansi", "plain"], "unicode"),
    ];
    for (order, unicode_from) in orders {
        let mut send = DataTransferSendBuilder::new(());
        for name in order {
            match name {
                "plain" => send.add_type(TypeHint::Plaintext, named(name)),
                "ansi" => send.add_type(ansi.clone(), named(name)),
                _ => send.add_type(unicode.clone(), named(name)),
            };
        }
        let data_object = data_object(send.build());
        // `CF_TEXT` is sent in the ANSI code page, as text formats are, with a NUL.
        assert_eq!(served(&data_object, CF_TEXT), b"ansi\0", "{order:?}");
        assert_eq!(served(&data_object, CF_UNICODETEXT), utf16_text(unicode_from), "{order:?}");
    }
}

#[test]
fn a_url_is_supplied_by_its_own_type_and_not_by_a_uri_list() {
    ensure_ole_initialized().unwrap();
    let url_format = ClipboardFormat::register("UniformResourceLocatorW").unwrap();
    for list_first in [true, false] {
        let mut send = DataTransferSendBuilder::new(());
        let url = named("https://example.com/");
        if list_first {
            send.add_type(TypeHint::UriList, file_list).add_type(url_format.clone(), url);
        } else {
            send.add_type(url_format.clone(), url).add_type(TypeHint::UriList, file_list);
        }
        let state = DataTransferState::default();
        state.register_drag(DRAG, WindowId::from_raw(1), data_object(send.build()));
        // A URI list is read from the file list before the URL.
        let files = state.read(DRAG, &TypeHint::UriList).unwrap();
        assert_eq!(files.try_as_uris().unwrap(), [FILE_URI], "list first: {list_first}");
        let url = state.read(DRAG, &url_format).unwrap();
        assert_eq!(
            url.try_as_uris().unwrap(),
            ["https://example.com/"],
            "list first: {list_first}"
        );
    }
}

#[test]
fn sent_formats_read_back() {
    ensure_ole_initialized().unwrap();
    // Registered formats stay registered until the session ends; this name is the tests' own.
    let custom = ClipboardFormat::register("winit-win32 test format").unwrap();
    let send = DataTransferSendBuilder::new(())
        .with_type(custom.clone(), named_bytes("\0\x01\x02"))
        .with_type(TypeHint::Rtf, named("{\\rtf1 x}"))
        .with_type(TypeHint::Html, named("<b>y</b>"))
        .build();
    let state = DataTransferState::default();
    state.register_drag(DRAG, WindowId::from_raw(1), data_object(send));

    let bytes = state.read(DRAG, &custom).unwrap();
    assert_eq!(bytes.try_as_bytes().unwrap(), [0, 1, 2]);
    let rtf = state.read(DRAG, &TypeHint::Rtf).unwrap();
    assert_eq!(rtf.try_as_string().unwrap(), "{\\rtf1 x}");
    let html = state.read(DRAG, &TypeHint::Html).unwrap();
    let document =
        ["<html><body>", "<!--StartFragment--><b>y</b><!--EndFragment-->", "</body></html>"];
    assert_eq!(html.try_as_string().unwrap(), document.join("\r\n"));
}

#[test]
fn empty_data_is_sent_and_read_back() {
    ensure_ole_initialized().unwrap();
    let custom = ClipboardFormat::register("winit-win32 test format").unwrap();
    let send = DataTransferSendBuilder::new(())
        .with_type(custom.clone(), named_bytes(""))
        .with_type(PNG, named_bytes(""))
        .build();
    let state = DataTransferState::default();
    state.register_drag(DRAG, WindowId::from_raw(1), data_object(send));

    assert_eq!(state.read(DRAG, &custom).unwrap().try_as_bytes().unwrap(), b"");
    assert_eq!(state.read(DRAG, &PNG).unwrap().try_as_bytes().unwrap(), b"");
}

#[test]
fn raw_code_page_text_is_sent_in_the_code_pages_of_the_locale_it_is_sent_with() {
    let ansi = ClipboardFormat::new(CF_TEXT, "CF_TEXT");
    let oem = ClipboardFormat::new(CF_OEMTEXT, "CF_OEMTEXT");
    // The locale of this system, which winit gives.
    let send = DataTransferSendBuilder::new(())
        .with_type(ansi.clone(), named(PRIVET))
        .with_type(oem.clone(), named(PRIVET))
        .build();
    let system = data_object(send);
    let locale = u32::from_le_bytes(served(&system, CF_LOCALE).try_into().unwrap());
    assert_eq!(locale, unsafe { GetSystemDefaultLCID() });
    let code_page = text_code_page(KnownFormat::AnsiText, Some(locale));
    assert_eq!(served(&system, CF_TEXT), code_page_bytes(PRIVET, code_page).unwrap());
    let code_page = text_code_page(KnownFormat::OemText, Some(locale));
    assert_eq!(served(&system, CF_OEMTEXT), code_page_bytes(PRIVET, code_page).unwrap());

    // The locale that the application gives, Russian, whose code pages are 1251 and 866.
    let russian =
        |_: &(), _: &dyn TransferType| Some(SendData::Bytes(RUSSIAN.to_le_bytes().into()));
    let send = DataTransferSendBuilder::new(())
        .with_type(ansi, named(PRIVET))
        .with_type(oem, named(PRIVET))
        .with_type(ClipboardFormat::new(CF_LOCALE, "CF_LOCALE"), russian)
        .build();
    let given = data_object(send);
    assert_eq!(served(&given, CF_LOCALE), RUSSIAN.to_le_bytes());
    assert_eq!(served(&given, CF_TEXT), b"\xcf\xf0\xe8\xe2\xe5\xf2\0");
    assert_eq!(served(&given, CF_OEMTEXT), b"\x8f\xe0\xa8\xa2\xa5\xe2\0");
}

#[test]
fn raw_code_page_bytes_are_sent_without_asking_for_the_locale() {
    let asked = Arc::new(AtomicUsize::new(0));
    let locale = {
        let asked = Arc::clone(&asked);
        move |_: &(), _: &dyn TransferType| {
            asked.fetch_add(1, Ordering::SeqCst);
            Some(SendData::Bytes(RUSSIAN.to_le_bytes().into()))
        }
    };
    let send = DataTransferSendBuilder::new(())
        .with_type(ClipboardFormat::new(CF_TEXT, "CF_TEXT"), named_bytes("abc\0"))
        .with_type(ClipboardFormat::new(CF_LOCALE, "CF_LOCALE"), locale)
        .build();
    let data_object = data_object(send);
    assert_eq!(served(&data_object, CF_TEXT), b"abc\0");
    assert_eq!(asked.load(Ordering::SeqCst), 0);
}
