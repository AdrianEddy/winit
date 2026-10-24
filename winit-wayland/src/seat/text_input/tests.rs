use super::*;

fn delete(before: usize, after: usize) -> Option<DeleteSurroundingText> {
    Some(DeleteSurroundingText { before, after })
}

fn done(data: &mut TextInputDataInner) -> Vec<Ime> {
    data.take_done_events().collect()
}

/// One `delete_surrounding_text`, then two `done`s: the deletion is delivered once. A later new
/// deletion is delivered once too.
#[test]
fn a_pending_delete_is_delivered_by_one_done_only() {
    let mut data = TextInputDataInner::default();
    data.pending_delete = delete(2, 1);
    assert_eq!(done(&mut data), [Ime::DeleteSurrounding { before_bytes: 2, after_bytes: 1 }]);
    assert!(done(&mut data).is_empty(), "a second `done` carries no deletion");
    data.pending_delete = delete(3, 0);
    assert_eq!(done(&mut data), [Ime::DeleteSurrounding { before_bytes: 3, after_bytes: 0 }]);
    assert!(done(&mut data).is_empty());
}

/// A `done` applies its pending state in text-input-v3's order — the delete, the commit (which
/// ends the preedit), the new preedit — and each of them once.
#[test]
fn a_done_applies_its_pending_state_in_the_protocols_order() {
    let mut data = TextInputDataInner::default();
    data.pending_delete = delete(1, 0);
    data.pending_commit = Some("ab".to_owned());
    data.pending_preedit =
        Some(Preedit { text: "c".to_owned(), cursor_begin: Some(1), cursor_end: None });
    assert_eq!(done(&mut data), [
        Ime::DeleteSurrounding { before_bytes: 1, after_bytes: 0 },
        Ime::Commit("ab".to_owned()),
        Ime::Preedit("c".to_owned(), Some((1, 1))),
    ]);
    assert_eq!(
        done(&mut data),
        [Ime::Preedit(String::new(), None)],
        "the preedit it left is cleared"
    );
    assert!(done(&mut data).is_empty());
}

/// A commit that ends a shown preedit is delivered alone: the commit replaces the preedit, so no
/// empty preedit precedes it, and the next empty `done` has nothing left to clear.
#[test]
fn a_commit_ends_a_shown_preedit_without_a_clear() {
    let mut data =
        TextInputDataInner { last_preedit_empty: false, ..TextInputDataInner::default() };
    data.pending_commit = Some("x".to_owned());
    assert_eq!(done(&mut data), [Ime::Commit("x".to_owned())]);
    assert!(done(&mut data).is_empty(), "the commit left no preedit to clear");
}
