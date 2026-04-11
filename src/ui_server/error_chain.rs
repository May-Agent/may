//! Shared helper for formatting `std::error::Error` chains as a single
//! colon-separated line. Kept next to its only callers in `ui_server`.

use std::error::Error;

/// Formats an error and its `source()` chain as a single
/// `": "`-separated line.
///
/// Example: a `Foo` error that wraps a `Bar` error that wraps `io::Error`
/// renders as `"foo: bar: io error"`. This is the format most of the
/// HTTP/SSE error responses in `ui_server` want when returning a surfaced
/// error string to the frontend.
pub fn error_chain_text(error: &dyn Error) -> String {
    let mut chain = vec![error.to_string()];
    let mut source = error.source();
    while let Some(current) = source {
        chain.push(current.to_string());
        source = current.source();
    }
    chain.join(": ")
}

#[cfg(test)]
mod tests {
    use super::error_chain_text;
    use std::fmt;

    #[derive(Debug)]
    struct Wrapped {
        message: &'static str,
        cause: Option<Box<dyn std::error::Error + Send + Sync>>,
    }

    impl fmt::Display for Wrapped {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.message)
        }
    }

    impl std::error::Error for Wrapped {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.cause.as_deref().map(|cause| cause as _)
        }
    }

    #[test]
    fn joins_error_source_chain_with_colons() {
        let inner = Wrapped {
            message: "io failure",
            cause: None,
        };
        let middle = Wrapped {
            message: "parse failed",
            cause: Some(Box::new(inner)),
        };
        let outer = Wrapped {
            message: "request rejected",
            cause: Some(Box::new(middle)),
        };

        assert_eq!(
            error_chain_text(&outer),
            "request rejected: parse failed: io failure"
        );
    }

    #[test]
    fn single_error_without_source_renders_as_its_display_form() {
        let error = Wrapped {
            message: "boom",
            cause: None,
        };
        assert_eq!(error_chain_text(&error), "boom");
    }
}
