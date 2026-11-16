//! Launching an application onto a session's display, in the helper.

use askama::Template;
use waterkit_automation::{Error, Input, LaunchConfig, LaunchedApplication};

use super::proto::{LaunchedApp, LaunchedWindow};

/// The launch `app` asks for: a path to an `.app` bundle when it holds a `/`
/// or ends in `.app`, a bundle id when it is in reverse-DNS form (a `.` and
/// no whitespace), otherwise an application name. Each item is a URL when it
/// has a `scheme://` prefix, otherwise a file path.
fn config(app: &str, items: &[String], display: u32) -> LaunchConfig {
    let bundle_extension = std::path::Path::new(app)
        .extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("app"));
    let config = if app.contains('/') || bundle_extension {
        LaunchConfig::bundle(app)
    } else if app.contains('.') && !app.contains(char::is_whitespace) {
        LaunchConfig::bundle_id(app)
    } else {
        LaunchConfig::name(app)
    };
    items.iter().fold(config.display(display), |config, item| {
        if is_url(item) {
            config.url(item)
        } else {
            config.file(item)
        }
    })
}

fn is_url(item: &str) -> bool {
    item.split_once("://").is_some_and(|(scheme, _)| {
        scheme.starts_with(|c: char| c.is_ascii_alphabetic())
            && scheme
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    })
}

/// What the model is told when an application name matches nothing or
/// several applications.
#[derive(Template)]
#[template(path = "computer_launch_name.txt", escape = "none")]
struct NameMiss<'a> {
    name: &'a str,
    ambiguous: bool,
    suggestions: &'a [String],
}

fn describe(error: &Error) -> String {
    let miss = match error {
        Error::AppNameNotFound { name, suggestions } => NameMiss {
            name,
            ambiguous: false,
            suggestions,
        },
        Error::AppNameAmbiguous { name, suggestions } => NameMiss {
            name,
            ambiguous: true,
            suggestions,
        },
        other => return other.to_string(),
    };
    miss.render().expect("the name-miss template renders")
}

/// Launches `app` onto `display` and reports what opened. It waits on the
/// calling thread's run loop, so it runs on a blocking thread, never the
/// async runtime's or the main thread's.
///
/// # Errors
/// A message for the model when the launch fails; for an application name
/// that matches nothing or several applications, it lists the closest names.
pub async fn launch(app: String, items: Vec<String>, display: u32) -> Result<LaunchedApp, String> {
    blocking::unblock(move || {
        let launched = LaunchedApplication::launch(&config(&app, &items, display))
            .map_err(|e| describe(&e))?;
        let origin = Input::new(display).map_err(|e| e.to_string())?.bounds();
        Ok(LaunchedApp {
            bundle_id: launched.bundle_id().to_owned(),
            pid: launched.pid(),
            windows: launched
                .windows()
                .iter()
                .map(|window| {
                    let frame = window.bounds();
                    display_local(
                        window.id(),
                        (frame.x(), frame.y(), frame.width(), frame.height()),
                        (origin.x(), origin.y()),
                    )
                })
                .collect(),
        })
    })
    .await
}

/// A window with frame `(x, y, width, height)` in global display
/// coordinates, in the space of the display at `origin`: the space
/// `Input` takes its points in.
fn display_local(
    id: u32,
    (x, y, width, height): (f64, f64, f64, f64),
    (origin_x, origin_y): (f64, f64),
) -> LaunchedWindow {
    LaunchedWindow {
        id,
        x: x - origin_x,
        y: y - origin_y,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_are_reported_relative_to_the_display_origin() {
        let window = display_local(7, (2000.0, 150.0, 800.0, 600.0), (1920.0, -100.0));
        assert_eq!(
            (window.id, window.x, window.y, window.width, window.height),
            (7, 80.0, 250.0, 800.0, 600.0)
        );
    }

    #[test]
    fn items_split_into_urls_and_files() {
        assert!(is_url("https://example.com"));
        assert!(is_url("x-custom+v1://a"));
        assert!(!is_url("notes.txt"));
        assert!(!is_url("/tmp/a://b"));
        assert!(!is_url("://x"));
    }

    #[test]
    fn app_forms_resolve_to_bundle_id_path_or_name() {
        let items = ["https://example.com".to_owned(), "a.txt".to_owned()];
        assert_eq!(
            config("com.google.Chrome", &items, 7),
            LaunchConfig::bundle_id("com.google.Chrome")
                .display(7)
                .url("https://example.com")
                .file("a.txt")
        );
        for path in ["/Applications/Google Chrome.app", "Chrome.app"] {
            assert_eq!(
                config(path, &[], 7),
                LaunchConfig::bundle(path).display(7),
                "{path}"
            );
        }
        for name in ["Safari", "Google Chrome", "Dr. Cleaner Pro"] {
            assert_eq!(
                config(name, &[], 7),
                LaunchConfig::name(name).display(7),
                "{name}"
            );
        }
    }

    #[test]
    fn a_name_miss_lists_the_closest_names() {
        let suggestions = vec!["Safari".to_owned(), "Safari Technology Preview".to_owned()];
        let missing = describe(&Error::AppNameNotFound {
            name: "Safri".to_owned(),
            suggestions: suggestions.clone(),
        });
        assert!(
            missing.contains("No installed application is named \"Safri\""),
            "{missing}"
        );
        assert!(
            missing.contains("Safari, Safari Technology Preview"),
            "{missing}"
        );
        let tie = describe(&Error::AppNameAmbiguous {
            name: "Pre".to_owned(),
            suggestions,
        });
        assert!(
            tie.contains("\"Pre\" matches several installed applications"),
            "{tie}"
        );
        assert!(tie.contains("Safari, Safari Technology Preview"), "{tie}");
    }

    #[test]
    fn a_name_miss_without_suggestions_names_none() {
        let text = describe(&Error::AppNameNotFound {
            name: "Zzz".to_owned(),
            suggestions: Vec::new(),
        });
        assert_eq!(
            text.trim_end(),
            "No installed application is named \"Zzz\"."
        );
    }
}
