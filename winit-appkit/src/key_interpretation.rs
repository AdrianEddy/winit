//! Which view is interpreting a key event, and whether the text input system answered that key with
//! a composition on another view.
//!
//! The text input system "communicates primarily with the client of the activated input context"
//! (Apple, `NSTextInputContext`), and that need not be the view whose `keyDown:` is interpreting
//! the key. After a click into another window, the key typed right after it can be interpreted by
//! the new key window's view while the previous window's input context is still the activated one:
//! the input method then answers the previous view. A pending dead key, for one, composes with the
//! key and its result is committed to the previous view. The key belongs to that composition, so
//! the interpreting view must not also deliver it as a key press with its text.

use std::cell::Cell;

/// A view, by identity, for the duration of one key event's interpretation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ViewIdentity(usize);

impl ViewIdentity {
    pub(crate) fn of<T>(view: &T) -> Self {
        Self(view as *const T as usize)
    }
}

/// The key event a view is interpreting (`keyDown:` → `interpretKeyEvents:`), shared by every view
/// of the application.
#[derive(Debug, Default)]
pub(crate) struct KeyInterpretation {
    interpreting: Cell<Option<ViewIdentity>>,
    composed_elsewhere: Cell<bool>,
}

impl KeyInterpretation {
    /// `view` starts interpreting a key event. The interpretation lasts until the returned guard is
    /// dropped.
    pub(crate) fn begin(&self, view: ViewIdentity) -> Interpreting<'_> {
        Interpreting {
            state: self,
            previous: self.interpreting.replace(Some(view)),
            previous_composed_elsewhere: self.composed_elsewhere.replace(false),
        }
    }

    /// The text input system answered `view` with a composition: it set or unmarked its marked
    /// text, or inserted text that ends its marked text or the commit its composition left pending.
    /// When another view is interpreting a key, that key belongs to this composition.
    pub(crate) fn composition_answer(&self, view: ViewIdentity) {
        if self.interpreting.get().is_some_and(|interpreting| interpreting != view) {
            self.composed_elsewhere.set(true);
        }
    }
}

/// A key event's interpretation (`KeyInterpretation::begin`); dropping it ends the interpretation.
#[derive(Debug)]
pub(crate) struct Interpreting<'a> {
    state: &'a KeyInterpretation,
    previous: Option<ViewIdentity>,
    previous_composed_elsewhere: bool,
}

impl Interpreting<'_> {
    /// Whether the text input system answered the key with another view's composition so far.
    pub(crate) fn composed_elsewhere(&self) -> bool {
        self.state.composed_elsewhere.get()
    }
}

impl Drop for Interpreting<'_> {
    fn drop(&mut self) {
        self.state.interpreting.set(self.previous);
        self.state.composed_elsewhere.set(self.previous_composed_elsewhere);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: ViewIdentity = ViewIdentity(1);
    const B: ViewIdentity = ViewIdentity(2);

    #[test]
    fn a_composition_answer_on_another_view_claims_the_key() {
        let state = KeyInterpretation::default();
        let interpreting = state.begin(A);
        state.composition_answer(B);
        assert!(interpreting.composed_elsewhere());
    }

    #[test]
    fn a_composition_answer_on_the_interpreting_view_is_its_own() {
        let state = KeyInterpretation::default();
        let interpreting = state.begin(A);
        state.composition_answer(A);
        assert!(!interpreting.composed_elsewhere());
    }

    #[test]
    fn an_answer_outside_any_interpretation_claims_nothing() {
        let state = KeyInterpretation::default();
        state.composition_answer(B);
        let interpreting = state.begin(A);
        assert!(!interpreting.composed_elsewhere(), "an answer before the key is not the key's");
    }

    #[test]
    fn each_key_starts_unclaimed() {
        let state = KeyInterpretation::default();
        {
            let first = state.begin(A);
            state.composition_answer(B);
            assert!(first.composed_elsewhere());
        }
        let second = state.begin(A);
        assert!(!second.composed_elsewhere());
        drop(second);
        state.composition_answer(B);
        assert!(
            !state.begin(A).composed_elsewhere(),
            "no interpretation was running for that answer"
        );
    }

    #[test]
    fn an_inner_interpretation_hands_the_key_back_to_the_outer_view() {
        let state = KeyInterpretation::default();
        let outer = state.begin(A);
        drop(state.begin(B));
        state.composition_answer(A);
        assert!(!outer.composed_elsewhere(), "A's own answer once the inner key is over");
    }

    #[test]
    fn a_nested_interpretation_restores_the_outer_one() {
        let state = KeyInterpretation::default();
        let outer = state.begin(A);
        state.composition_answer(B);
        {
            let inner = state.begin(B);
            assert!(!inner.composed_elsewhere());
            state.composition_answer(B);
            assert!(!inner.composed_elsewhere(), "B's own answer while B interprets");
        }
        assert!(outer.composed_elsewhere(), "the outer key keeps its claim");
        state.composition_answer(A);
        assert!(outer.composed_elsewhere());
    }
}
