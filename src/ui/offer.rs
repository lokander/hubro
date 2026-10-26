//! Whether the UI offers a control at all, and why not when it shows one it
//! won't let you use.

/// How a control appears: absent, shown but refused with its reason, or
/// usable.
///
/// [`Hidden`](Self::Hidden) is for where the operation makes no sense at all —
/// renaming a view's column, exporting an object with no rows — so the
/// surface keeps its shape instead of growing a permanently dead button.
/// [`Disabled`](Self::Disabled) is the ordinary refusal: visible, with the
/// sentence that explains it, because a button that is merely missing
/// explains nothing. `R` is that sentence: a `&'static str` where the reason
/// is one of a fixed set, a `String` where it is composed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Offer<R = &'static str> {
    Hidden,
    Disabled(R),
    Offered,
}

impl<R> Offer<R> {
    /// Whether the control is drawn at all, usable or not.
    pub(crate) fn is_shown(&self) -> bool {
        !matches!(self, Offer::Hidden)
    }

    /// Why a shown control is refused; `None` when it is usable or hidden.
    pub(crate) fn refusal(&self) -> Option<&R> {
        match self {
            Offer::Disabled(reason) => Some(reason),
            Offer::Hidden | Offer::Offered => None,
        }
    }

    /// A shown control: refused for `refusal` when there is one, offered
    /// otherwise.
    pub(crate) fn shown(refusal: Option<R>) -> Self {
        match refusal {
            Some(reason) => Offer::Disabled(reason),
            None => Offer::Offered,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_disabled_offer_carries_a_refusal_and_only_a_hidden_one_is_absent() {
        let hidden: Offer = Offer::Hidden;
        assert!(!hidden.is_shown());
        assert_eq!(hidden.refusal(), None);
        let disabled: Offer = Offer::shown(Some("read-only"));
        assert_eq!(disabled, Offer::Disabled("read-only"));
        assert!(disabled.is_shown());
        assert_eq!(disabled.refusal(), Some(&"read-only"));
        let offered: Offer = Offer::shown(None);
        assert_eq!(offered, Offer::Offered);
        assert!(offered.is_shown());
        assert_eq!(offered.refusal(), None);
    }
}
