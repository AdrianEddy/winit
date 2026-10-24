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

#[derive(Debug, PartialEq)]
struct TestDrag {
    id: u32,
    dropped: bool,
}

fn test_drag(id: u32, dropped: bool) -> TestDrag {
    TestDrag { id, dropped }
}

/// A drag started while an earlier one is still finishing (dropped, waiting for the target) leaves
/// the earlier one in place; each ends on its own, found by identity.
#[test]
fn a_drag_finishing_its_drop_outlives_a_newer_drag() {
    let mut drags = SendDrags::default();
    assert!(drags.start(test_drag(1, true), |drag| drag.dropped).is_empty());
    assert!(drags.start(test_drag(2, false), |drag| drag.dropped).is_empty());

    drags.find_mut(|drag| drag.id == 2).unwrap().dropped = true;
    assert_eq!(drags.take(|drag| drag.id == 1), Some(test_drag(1, true)));
    assert_eq!(drags.take(|drag| drag.id == 2), Some(test_drag(2, true)));
    assert_eq!(drags.take(|drag| drag.id == 1), None, "a drag ends once");
}

/// An earlier drag that was never dropped gets no further events once a newer one starts, so the
/// newer start returns it to be ended; a dropped one keeps waiting for its target.
#[test]
fn a_new_drag_ends_an_earlier_drag_that_was_never_dropped() {
    let mut drags = SendDrags::default();
    drags.start(test_drag(1, false), |drag| drag.dropped);
    drags.find_mut(|drag| drag.id == 1).unwrap().dropped = true;
    assert!(drags.start(test_drag(2, false), |drag| drag.dropped).is_empty());

    let abandoned = drags.start(test_drag(3, false), |drag| drag.dropped);
    assert_eq!(abandoned, [test_drag(2, false)]);
    assert_eq!(drags.take(|drag| drag.id == 1), Some(test_drag(1, true)));
    assert_eq!(drags.take(|drag| drag.id == 3), Some(test_drag(3, false)));
    assert_eq!(drags.take(|drag| drag.id == 2), None);
}
