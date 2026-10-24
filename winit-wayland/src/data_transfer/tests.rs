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

fn text_offer() -> DataOffer {
    DataOffer::new(&["text/plain".to_owned()], DataTransferId::from_raw(1))
}

/// The source actions of a data transfer, as the application sees them.
fn source_actions(offer: &DataOffer) -> Option<Vec<DndAction>> {
    let transfer: Box<dyn DataTransfer> = Box::new(offer.clone());
    transfer.source_actions().map(<[DndAction]>::to_vec)
}

/// A drag offered at version 3 or later reports each action its source allows.
#[test]
fn a_drag_reports_the_actions_its_source_allows() {
    let (copy, move_, ask) = (WlDndAction::Copy, WlDndAction::Move, WlDndAction::Ask);
    let reported = |actions| source_actions(&text_offer().dragged(3, actions));

    assert_eq!(reported(copy), Some(vec![DndAction::Copy]));
    assert_eq!(reported(move_), Some(vec![DndAction::Move]));
    assert_eq!(reported(ask), Some(vec![DndAction::Ask]));
    assert_eq!(reported(copy | move_), Some(vec![DndAction::Move, DndAction::Copy]));
    assert_eq!(reported(copy | ask), Some(vec![DndAction::Copy, DndAction::Ask]));
    assert_eq!(reported(move_ | ask), Some(vec![DndAction::Move, DndAction::Ask]));
    let all = Some(vec![DndAction::Move, DndAction::Copy, DndAction::Ask]);
    assert_eq!(reported(copy | move_ | ask), all);
    assert_eq!(source_actions(&text_offer().dragged(4, copy)), Some(vec![DndAction::Copy]));
}

/// A source that allows no action reports an empty set, which is known, rather than an unknown
/// one.
#[test]
fn a_drag_whose_source_allows_nothing_reports_an_empty_set() {
    let offer = text_offer().dragged(3, WlDndAction::empty());
    assert_eq!(source_actions(&offer), Some(vec![]));
}

/// An offer before version 3 does not report the source actions, which are unknown; the target
/// negotiates with none, as before.
#[test]
fn a_drag_offered_before_version_3_has_unknown_source_actions() {
    let offer = text_offer().dragged(2, WlDndAction::Copy | WlDndAction::Move);
    assert_eq!(source_actions(&offer), None);
    assert_eq!(offer.actions_allowed_by_source(), WlDndAction::empty());
}

/// The source actions of an offer that is not a drag, such as the clipboard's, are unknown.
#[test]
fn an_offer_that_is_not_a_drag_has_unknown_source_actions() {
    assert_eq!(source_actions(&text_offer()), None);
}

/// The source changes the actions it allows during the drag: a data transfer obtained after the
/// `source_actions` event sees the change, one obtained before keeps its snapshot, and the
/// actions negotiated with the target are taken from the new set.
#[test]
fn a_change_of_the_source_actions_shows_in_a_new_snapshot() {
    let mut offer = text_offer().dragged(3, WlDndAction::Copy);
    let before: Box<dyn DataTransfer> = Box::new(offer.clone());

    offer.set_source_actions(WlDndAction::Copy | WlDndAction::Move);

    assert_eq!(source_actions(&offer), Some(vec![DndAction::Move, DndAction::Copy]));
    assert_eq!(before.source_actions(), Some(&[DndAction::Copy][..]));
    assert_eq!(offer.actions_allowed_by_source(), WlDndAction::Copy | WlDndAction::Move);
}
