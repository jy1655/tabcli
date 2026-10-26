//! The optional `failure_kind` hint of a JSON error object: the producer observed delivery
//! uncertainty, its own deadline, or an unsupported capability. It is a diagnostic hint for
//! self-test, not a Delivery record: absence means unknown or legacy, never not sent, and no
//! class permits a retry. Nothing persists it, so string-flattened errors carry none.
//!
//! A marker replaces an existing message leaf or context with the same text. Every rendering
//! and the downcasts of the errors below it are unchanged. Each class has its own type:
//! anyhow's downcast finds only the outermost layer of one type, and `chain()` cannot see a
//! context type at all, so one shared enum could hide an inner class.
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::native) enum FailureKind {
    DeliveryUncertain,
    TimedOut,
    Unsupported,
}

impl FailureKind {
    pub(in crate::native) fn as_str(self) -> &'static str {
        match self {
            Self::DeliveryUncertain => "delivery_uncertain",
            Self::TimedOut => "timed_out",
            Self::Unsupported => "unsupported",
        }
    }

    /// Only the exact public values are recognized; anything else is unknown.
    pub(in crate::native) fn from_hint(value: &serde_json::Value) -> Option<Self> {
        [Self::DeliveryUncertain, Self::TimedOut, Self::Unsupported]
            .into_iter()
            .find(|kind| value.as_str() == Some(kind.as_str()))
    }
}

macro_rules! marker {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        pub(in crate::native) struct $name(String);

        impl $name {
            pub(in crate::native) fn new(message: impl Into<String>) -> Self {
                Self(message.into())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }

        // The message's own Debug, as anyhow's message leaf prints it.
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Debug::fmt(&self.0, f)
            }
        }

        impl std::error::Error for $name {}
    };
}

marker!(
    /// Delivery settled the prompt as uncertain: it may have reached the provider.
    DeliveryUncertain
);
marker!(
    /// The producer observed its own deadline expire.
    TimedOut
);
marker!(
    /// A capability, version or platform gate refused the operation.
    Unsupported
);

fn marked<T: std::error::Error + Send + Sync + 'static>(error: &anyhow::Error) -> bool {
    // anyhow reaches a context at any depth and the innermost leaf; the chain scan adds a
    // leaf below a foreign error's source.
    error.downcast_ref::<T>().is_some() || error.chain().any(|cause| cause.is::<T>())
}

/// Delivery uncertainty first, then a deadline, then an unsupported capability.
pub(in crate::native) fn classify(error: &anyhow::Error) -> Option<FailureKind> {
    if marked::<DeliveryUncertain>(error) {
        Some(FailureKind::DeliveryUncertain)
    } else if marked::<TimedOut>(error) {
        Some(FailureKind::TimedOut)
    } else if marked::<Unsupported>(error) {
        Some(FailureKind::Unsupported)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[derive(Debug)]
    struct Inner;
    impl fmt::Display for Inner {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("inner cause")
        }
    }
    impl std::error::Error for Inner {}

    #[derive(Debug)]
    struct Foreign(anyhow::Error);
    impl fmt::Display for Foreign {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("foreign wrapper")
        }
    }
    impl std::error::Error for Foreign {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(self.0.as_ref())
        }
    }

    fn renderings(error: &anyhow::Error) -> [String; 5] {
        // A captured backtrace differs per construction site; the messages must not.
        let debug = format!("{error:?}");
        [
            error.to_string(),
            format!("{error:#}"),
            debug
                .split("\n\nStack backtrace:")
                .next()
                .unwrap()
                .to_owned(),
            format!("{error:#?}"),
            error
                .chain()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("|"),
        ]
    }

    #[test]
    fn markers_render_exactly_as_the_messages_they_replace() {
        let text = || format!("timed out after {} seconds", 30);
        // A message leaf, as anyhow! and bail! build it.
        assert_eq!(
            renderings(&anyhow::Error::new(TimedOut::new(text()))),
            renderings(&anyhow::anyhow!("{}", text()))
        );
        // A context over an anyhow error, and over a foreign std error.
        assert_eq!(
            renderings(
                &Err::<(), _>(anyhow::Error::new(Inner))
                    .context(Unsupported::new(text()))
                    .unwrap_err()
            ),
            renderings(
                &Err::<(), _>(anyhow::Error::new(Inner))
                    .context(text())
                    .unwrap_err()
            )
        );
        assert_eq!(
            renderings(
                &Err::<(), _>(Inner)
                    .context(DeliveryUncertain::new(text()))
                    .unwrap_err()
            ),
            renderings(&Err::<(), _>(Inner).context(text()).unwrap_err())
        );
        // An Option context, as remaining-time checks build it.
        assert_eq!(
            renderings(
                &None::<()>
                    .with_context(|| TimedOut::new(text()))
                    .unwrap_err()
            ),
            renderings(&None::<()>.with_context(text).unwrap_err())
        );
    }

    #[test]
    fn marker_contexts_keep_inner_downcasts_and_each_class_is_found_at_any_depth() {
        let error = Err::<(), _>(anyhow::Error::new(Inner))
            .context("middle")
            .context(Unsupported::new("unsupported gate"))
            .context("outer")
            .unwrap_err();
        assert!(error.downcast_ref::<Inner>().is_some());
        assert!(error.downcast_ref::<Unsupported>().is_some());
        assert_eq!(classify(&error), Some(FailureKind::Unsupported));
        let io = Err::<(), _>(std::io::Error::other("disk"))
            .context(TimedOut::new("timed out"))
            .unwrap_err();
        assert!(io.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(classify(&io), Some(FailureKind::TimedOut));
        // A leaf below a foreign source is found through the chain.
        let foreign = anyhow::Error::new(Foreign(anyhow::Error::new(TimedOut::new("timed out"))));
        assert_eq!(classify(&foreign), Some(FailureKind::TimedOut));
        assert_eq!(classify(&anyhow::anyhow!("timed out; unsupported")), None);
    }

    #[test]
    fn delivery_uncertainty_precedes_a_deadline_which_precedes_unsupported() {
        let timed_out = || anyhow::Error::new(TimedOut::new("deadline"));
        let unsupported_over_timeout = timed_out().context(Unsupported::new("gate"));
        assert_eq!(
            classify(&unsupported_over_timeout),
            Some(FailureKind::TimedOut)
        );
        let uncertain_over_timeout = timed_out().context(DeliveryUncertain::new("uncertain"));
        assert_eq!(
            classify(&uncertain_over_timeout),
            Some(FailureKind::DeliveryUncertain)
        );
        let all = anyhow::Error::new(Unsupported::new("gate"))
            .context(TimedOut::new("deadline"))
            .context("plain")
            .context(DeliveryUncertain::new("uncertain"));
        assert_eq!(classify(&all), Some(FailureKind::DeliveryUncertain));
        let under_uncertain = anyhow::Error::new(DeliveryUncertain::new("uncertain"))
            .context(Unsupported::new("gate"))
            .context(TimedOut::new("deadline"));
        assert_eq!(
            classify(&under_uncertain),
            Some(FailureKind::DeliveryUncertain)
        );
    }

    #[test]
    fn only_exact_public_hint_values_are_recognized() {
        for kind in [
            FailureKind::DeliveryUncertain,
            FailureKind::TimedOut,
            FailureKind::Unsupported,
        ] {
            assert_eq!(
                FailureKind::from_hint(&serde_json::json!(kind.as_str())),
                Some(kind)
            );
        }
        for value in [
            serde_json::json!(null),
            serde_json::json!(""),
            serde_json::json!("Timed_Out"),
            serde_json::json!("not_sent"),
            serde_json::json!(1),
            serde_json::json!(true),
            serde_json::json!(["timed_out"]),
            serde_json::json!({"kind": "timed_out"}),
        ] {
            assert_eq!(FailureKind::from_hint(&value), None, "{value}");
        }
    }
}
