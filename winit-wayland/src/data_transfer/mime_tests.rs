use winit_core::data_transfer::DataTransferSendBuilder;

use super::*;

fn mime(name: &str) -> MimeType {
    MimeType::parse(name.to_owned())
}

/// The MIME types that a source offers for `hint`, in the order it offers them.
fn offered(hint: TypeHint) -> Vec<String> {
    MimeType::from_dyn(&hint).map(|mime| mime.to_string()).collect()
}

/// Text is offered under the canonical spelling of its type first, which XWayland's bridges map to
/// `UTF8_STRING`, then under the bare type and the spelling with a space.
#[test]
fn the_canonical_spelling_of_text_is_offered_first() {
    let plain = ["text/plain;charset=utf-8", "text/plain", "text/plain; charset=utf-8"];
    assert_eq!(offered(TypeHint::Plaintext), plain);
    let html = ["text/html;charset=utf-8", "text/html", "text/html; charset=utf-8"];
    assert_eq!(offered(TypeHint::Html), html);
}

/// A MIME type made from a text hint is still the bare type.
#[test]
fn a_mime_type_made_from_a_text_hint_is_the_bare_type() {
    let plain = MimeType::try_from(TypeHint::Plaintext).unwrap();
    assert_eq!(plain.to_string(), "text/plain");
    assert_eq!(MimeType::try_from(TypeHint::Html).unwrap().to_string(), "text/html");
}

/// A type is recognised as text in any spelling: the type and the parameter names in any case,
/// white space around the parameters or not, the charset UTF-8 or US-ASCII in any case, quoted or
/// not, other parameters, an empty parameter and one without a value skipped.
#[test]
fn text_is_recognised_in_any_spelling_of_its_type() {
    for spelling in [
        "text/plain",
        "text/plain;charset=utf-8",
        "text/plain; charset=utf-8",
        "text/plain;charset=UTF-8",
        "TEXT/PLAIN;CHARSET=UTF-8",
        "text/plain ; charset = utf-8",
        "text/plain;format=flowed",
        "text/plain; charset=utf-8; format=flowed",
        "text/plain;charset=utf-8;",
        "text/plain;;charset=utf-8",
        "text/plain;flowed;charset=utf-8",
        "text/plain;charset=utf-8;charset=UTF-8",
        "text/plain;charset=us-ascii",
        "text/plain;charset=US-ASCII",
    ] {
        assert_eq!(mime(spelling).hint(), Some(TypeHint::Plaintext), "{spelling}");
    }
    for spelling in ["text/html", "text/html;charset=utf-8", "TEXT/HTML; CHARSET=\"UTF-8\""] {
        assert_eq!(mime(spelling).hint(), Some(TypeHint::Html), "{spelling}");
    }
}

/// A quoted string is one parameter value, whatever it holds: a `;`, or a `\` that quotes the next
/// character.
#[test]
fn a_quoted_string_is_read_as_one_value() {
    // The charset inside the quoted value of `x` is not the type's.
    let hidden = mime("text/plain;x=\"a;charset=iso-8859-1;b\"");
    assert_eq!(hidden.hint(), Some(TypeHint::Plaintext));
    assert_eq!(mime("Text/Plain ; Charset=\"utf-8\"").hint(), Some(TypeHint::Plaintext));
    assert_eq!(mime("text/plain;charset=\"utf\\-8\"").hint(), Some(TypeHint::Plaintext));
}

/// Text in another charset, or whose charset cannot be read, is not taken for text: it could not
/// be decoded.
#[test]
fn text_in_another_or_an_unreadable_charset_is_not_taken_for_text() {
    for spelling in [
        "text/plain;charset=iso-8859-1",
        "text/html;charset=iso-8859-1",
        // Not a name of UTF-8 in the IANA registry.
        "text/plain;charset=utf8",
        "text/plain;charset=",
        "text/plain;charset=\"\"",
        "text/plain;charset=utf-8;charset=iso-8859-1",
        // The `;` and the charset inside the quoted string belong to the value of `x`.
        "text/plain;x=\"a;charset=utf-8;b\";charset=iso-8859-1",
        "text/plain;charset=\"utf-8",
        "text/plain;charset=\"utf-8\" x",
        "text/plain;charset=utf-8,foo",
        "text/plain;charset",
        // RFC 2231's form, which is not decoded.
        "text/plain;charset*=utf-8''utf-8",
        "text/plain;CHARSET*0=utf-8",
        // A charset after a comment, or after the value of another parameter.
        "text/plain;(note) charset=iso-8859-1",
        "text/plain;x=foo charset=iso-8859-1",
    ] {
        assert_eq!(mime(spelling).hint(), None, "{spelling}");
    }
    assert_eq!(mime("application/json;charset=utf-8").hint(), None, "not text");
}

/// Parameters that do not follow the grammar make a type no text, even when none of them is a
/// charset: a comment, text after a value, an empty or unterminated value, an attribute or a value
/// that is not a token, and white space other than a space or a tab around them.
#[test]
fn malformed_parameters_make_a_type_no_text() {
    for spelling in [
        "text/plain;(note) format=flowed",
        "text/plain;format=flowed fixed",
        "text/plain;format=\"flowed\" fixed",
        "text/plain;format=\"flowed",
        "text/plain;format=",
        "text/plain;for mat=flowed",
        "text/plain;format=flo@wed",
        "text/plain;(note)",
        // A no-break space, which is not linear white space.
        "text/plain;format=flowed\u{a0}",
        "text/plain;\u{a0}format=flowed",
        "text/plain;format\u{a0}=flowed",
        "text/plain;format=\u{a0}flowed",
    ] {
        assert_eq!(mime(spelling).hint(), None, "{spelling}");
    }
    assert_eq!(mime("text/plain\u{a0}").hint(), None, "a no-break space after the type");
}

/// The parameters of other types are read leniently: one that does not follow the grammar states no
/// charset, so data of such a type is decoded and sent as UTF-8; a charset that the other
/// parameters state is still read, and another charset is refused in any type, as is a charset that
/// does not follow the grammar.
#[test]
fn malformed_parameters_of_other_types_state_no_charset() {
    let decoded =
        |name: &str| MimeData::new(mime(name), Ok(b"plain".to_vec())).try_as_string().ok();
    let sent = |name: &str| send_data_bytes(&mime(name), SendData::String("zażółć".into()));

    // `/` is not a token character, so the value of `path` does not follow the grammar.
    for name in [
        "application/x-foo;path=/tmp/a",
        "application/x-foo;(note)",
        "application/x-foo;path=/tmp/a;charset=utf-8",
        // A lone `charset`, without `=`, states none.
        "application/x-foo;charset",
        // A well-formed parameter is read exactly: this `charset=` is inside the value of `x`.
        "application/x-foo;x=\"charset=iso-8859-1\"",
    ] {
        assert_eq!(decoded(name).as_deref(), Some("plain"), "{name}");
        assert_eq!(sent(name), Some("zażółć".as_bytes().to_vec()), "{name}");
    }
    // In text, they make the charset unreadable.
    assert_eq!(decoded("text/plain;path=/tmp/a"), None);
    assert_eq!(sent("text/plain;path=/tmp/a"), None);
    for name in [
        "application/x-foo;charset=iso-8859-1",
        "application/x-foo;path=/tmp/a;charset=iso-8859-1",
        "application/x-foo;charset=iso-8859-1;path=/tmp/a",
        // A charset that does not follow the grammar cannot be read.
        "application/x-foo;charset=iso-8859-1,foo",
        "application/x-foo;charset=iso-8859-1 junk",
        "application/x-foo;charset=\"iso-8859-1",
        "application/x-foo;charset*=a b",
        // A `charset=` inside another parameter that does not follow the grammar.
        "application/x-foo;(note) charset=iso-8859-1",
        "application/x-foo;x=a charset=iso-8859-1",
        "application/x-foo;x=a CharSet =iso-8859-1",
    ] {
        assert_eq!(decoded(name), None, "{name}");
        assert_eq!(sent(name), None, "{name}");
    }
}

/// The other types are recognised in any case too.
#[test]
fn other_types_are_recognised_in_any_case() {
    assert_eq!(mime("IMAGE/PNG").hint(), Some(TypeHint::Image { extension_hint: Some("png") }));
    assert_eq!(mime("Audio/Wav").hint(), Some(TypeHint::Audio { extension_hint: Some("wav") }));
    assert_eq!(mime("IMAGE/X-UNKNOWN").hint(), Some(TypeHint::Image { extension_hint: None }));
    assert_eq!(mime("Text/URI-List").hint(), Some(TypeHint::UriList));
}

/// A read by hint takes the first offered text that says it is UTF-8, before text without a charset
/// or in US-ASCII, whatever the order of the offer; within a rank, the first one offered.
#[test]
fn a_read_by_hint_prefers_text_that_says_it_is_utf8() {
    let read = |offer: &[&str], hint: TypeHint| {
        let offer: Vec<String> = offer.iter().map(|name| name.to_string()).collect();
        let offer = DataOffer::new(&offer, DataTransferId::from_raw(1));
        offer.find_type_dyn(&hint).map(|mime| mime.to_string())
    };

    let plain = ["text/plain", "text/plain;charset=utf-8", "text/plain; charset=UTF-8"];
    assert_eq!(read(&plain, TypeHint::Plaintext).as_deref(), Some("text/plain;charset=utf-8"));
    let html = ["text/html", "text/html; charset=utf-8"];
    assert_eq!(read(&html, TypeHint::Html).as_deref(), Some("text/html; charset=utf-8"));
    let no_utf8 = ["text/plain;charset=us-ascii", "text/plain"];
    assert_eq!(read(&no_utf8, TypeHint::Plaintext).as_deref(), Some("text/plain;charset=us-ascii"));
}

/// Text received under any spelling of its charset is decoded as UTF-8, and still validated as
/// such; another charset is refused.
#[test]
fn text_is_decoded_in_any_spelling_of_its_charset() {
    let decoded = |name: &str, text: &str| {
        MimeData::new(mime(name), Ok(text.as_bytes().to_vec())).try_as_string().ok()
    };

    for name in ["text/plain", "text/plain;charset=UTF-8", "text/plain; charset=\"utf-8\""] {
        assert_eq!(decoded(name, "zażółć").as_deref(), Some("zażółć"), "{name}");
    }
    assert_eq!(decoded("text/plain;charset=us-ascii", "plain").as_deref(), Some("plain"));
    assert_eq!(decoded("text/plain;charset=iso-8859-1", "plain"), None);
    assert_eq!(decoded("text/plain;charset=utf-8;charset=iso-8859-1", "plain"), None);
}

/// A request for the canonical spelling, or for any type offered for text, is answered with the
/// text in UTF-8.
#[test]
fn a_request_for_the_canonical_spelling_is_answered_in_utf8() {
    let send = DataTransferSendBuilder::new(())
        .with_type(TypeHint::Plaintext, |_, _| Some(String::from("zażółć")))
        .build();
    let answer = |name: &str| {
        // What the source does with a `send` request for that type.
        let requested = mime(name);
        let data = send.data_for_type(&requested)?;
        send_data_bytes(&requested, data)
    };

    assert_eq!(answer("text/plain;charset=utf-8"), Some("zażółć".as_bytes().to_vec()));
    assert_eq!(answer("TEXT/PLAIN;CHARSET=UTF-8"), Some("zażółć".as_bytes().to_vec()));
    for name in offered(TypeHint::Plaintext) {
        assert_eq!(answer(&name), Some("zażółć".as_bytes().to_vec()), "{name}");
    }
}

/// Text that a requestor asks for as US-ASCII by name is sent only if it is ASCII; text without a
/// charset is sent in UTF-8.
#[test]
fn text_asked_for_as_us_ascii_is_sent_only_if_it_is_ascii() {
    let sent = |name: &str, text: &str| send_data_bytes(&mime(name), SendData::String(text.into()));

    assert_eq!(sent("text/plain;charset=us-ascii", "plain"), Some(b"plain".to_vec()));
    assert_eq!(sent("text/plain;charset=us-ascii", "zażółć"), None);
    assert_eq!(sent("text/plain", "zażółć"), Some("zażółć".as_bytes().to_vec()));
}
