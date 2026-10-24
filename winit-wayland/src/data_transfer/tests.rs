use super::*;

/// A MIME type created by name detects its hint like a type offered by another client, and reads
/// its name back unchanged.
#[test]
fn a_mime_type_created_by_name_detects_its_hint() {
    let plain = MimeType::new("text/plain");
    assert_eq!(plain.as_str(), "text/plain");
    assert_eq!(plain.hint(), Some(TypeHint::Plaintext));

    let image = MimeType::new("image/x-example");
    assert_eq!(image.hint(), Some(TypeHint::Image { extension_hint: None }));

    let custom = MimeType::new("application/x-example");
    assert_eq!(custom.as_str(), "application/x-example");
    assert_eq!(custom.to_string(), "application/x-example");
    assert_eq!(custom.hint(), None);
}

/// A data transfer that offers an application-defined MIME type advertises exactly that type.
#[test]
fn a_custom_mime_type_is_advertised_as_itself() {
    let custom = MimeType::new("application/x-example");
    let advertised: Vec<MimeType> = MimeType::from_dyn(&custom).collect();
    assert_eq!(advertised, [MimeType::new("application/x-example")]);
}
