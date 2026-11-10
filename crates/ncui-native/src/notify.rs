//! Notifications, raised through the platform's own tool.
//!
//! # Why the platform's tool rather than a binding
//!
//! The same reasoning as the clipboard: a notification has no `std` interface on any of the
//! three platforms. On Windows it is `WinRT` (`Windows.UI.Notifications`), on macOS the
//! `UserNotifications` framework, on Linux the freedesktop D-Bus interface — and every Rust
//! crate that offers one is a binding over exactly that, which this workspace cannot reach
//! without `unsafe` outside a dependency. So the tool each platform already ships is used:
//! PowerShell on Windows, `osascript` on macOS, `notify-send` on Linux.
//!
//! # The payload is data, and each platform is treated as what it is
//!
//! This is where notifications differ from the clipboard, and it is the part worth being
//! careful about. A notification's title and body come from a page, which means they are
//! attacker-influenced, and each platform's tool reads its payload differently:
//!
//! * **Windows.** The text travels on standard input and never on the command line, and the
//!   toast document is built by setting *text nodes* on a template the platform supplies — so
//!   `<`, `&` and `"` in a notification are characters in a message rather than markup that
//!   changes what the notification is.
//! * **macOS.** `osascript` is given `AppleScript` *source*, so the payload is escaped into a
//!   string literal by [`apple_script_literal`]: a body containing `"` cannot end the literal
//!   and start a statement, and a body containing a newline cannot end the line.
//! * **Linux.** `notify-send` takes the summary and body as arguments, so the payload is passed
//!   as an argument vector after `--`: a body that begins with `-` is a message, not a flag.
//!
//! # What is not guaranteed
//!
//! That the user *sees* it. Windows and Linux both honour a do-not-disturb setting, and on
//! Windows a toast is delivered under an application identity: an application that wants its
//! own name on the notification registers one at install time, which is packaging work rather
//! than runtime work. Until then this module uses the PowerShell identity, because a
//! notification delivered under the wrong name is worth more to a user than one that is
//! silently dropped. `docs/known-gaps.md` records that.
//!
//! A missing tool is reported rather than hidden, for the same reason the clipboard reports it:
//! "the platform has no notifier" and "the notification was raised" are different answers, and
//! a caller that conflates them will believe a user was told something they were not.

use std::path::Path;
// Used by the Windows arm only: a toast is given a unique tag per notification, and no other
// platform here needs one.
#[cfg(windows)]
use std::sync::atomic::{AtomicU64, Ordering};

use crate::process::{ProcessError, run_command};

/// The longest title a notification may carry.
///
/// A notification is a message, not a document transport: a title past this is a caller that
/// has confused the two, and refusing is kinder than a platform that truncates it invisibly.
pub const MAX_TITLE_BYTES: usize = 256;

/// The longest body a notification may carry.
pub const MAX_BODY_BYTES: usize = 4096;

/// How long a notification tool may take before it is stopped.
pub const NOTIFICATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// The application identity Windows delivers a toast under.
///
/// A toast is shown against an `AppUserModelID`, and Windows drops one whose identity it does not
/// know. Registering an identity means a Start-menu shortcut or a registry entry, which is
/// install-time work; until an application has done that, this is the identity Windows already
/// knows — PowerShell's own. The notification arrives attributed to PowerShell rather than to
/// the application, which is a visible cost and a deliberate one: the alternative is a call that
/// reports success and shows nothing.
#[cfg(windows)]
pub const WINDOWS_APP_ID: &str =
    "{1AC14E77-02E7-4E5D-B744-2EB1AE5198B7}\\WindowsPowerShell\\v1.0\\powershell.exe";

/// Why a notification was not raised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationError {
    /// The title or body was longer than its bound.
    TooLong {
        /// Which part.
        part: &'static str,
        /// How long it was.
        bytes: usize,
        /// The bound.
        limit: usize,
    },
    /// The platform has no tool this host can use.
    Unavailable {
        /// What was looked for.
        tool: &'static str,
    },
    /// The tool ran and reported a failure.
    Failed {
        /// The tool.
        tool: &'static str,
        /// What it said.
        reason: String,
    },
}

impl NotificationError {
    /// The stable code a caller sees on the wire.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::TooLong { .. } => "notification_too_long",
            Self::Unavailable { .. } => "notification_unavailable",
            Self::Failed { .. } => "notification_failed",
        }
    }
}

impl core::fmt::Display for NotificationError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooLong { part, bytes, limit } => write!(
                f,
                "the notification's {part} is {bytes} bytes, over the limit of {limit}"
            ),
            Self::Unavailable { tool } => write!(
                f,
                "this system has no notifier this host can use (`{tool}` is not available)"
            ),
            Self::Failed { tool, reason } => {
                write!(f, "`{tool}` could not raise the notification: {reason}")
            }
        }
    }
}

impl std::error::Error for NotificationError {}

/// Raises notifications through the platform's tool.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Notifier {
    /// The identity a Windows toast is delivered under. Ignored elsewhere.
    app_id: String,
}

impl Notifier {
    /// A notifier using the platform's default identity.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A notifier delivered under `app_id` on Windows.
    #[must_use]
    pub fn with_app_id(app_id: &str) -> Self {
        Self {
            app_id: app_id.to_owned(),
        }
    }

    /// Raises a notification with `title` and `body`.
    ///
    /// # Errors
    ///
    /// [`NotificationError::TooLong`] before any process is started,
    /// [`NotificationError::Unavailable`] when the platform's tool cannot be found, and
    /// [`NotificationError::Failed`] when it runs and refuses.
    pub fn show(&self, title: &str, body: &str) -> Result<(), NotificationError> {
        if title.len() > MAX_TITLE_BYTES {
            return Err(NotificationError::TooLong {
                part: "title",
                bytes: title.len(),
                limit: MAX_TITLE_BYTES,
            });
        }
        if body.len() > MAX_BODY_BYTES {
            return Err(NotificationError::TooLong {
                part: "body",
                bytes: body.len(),
                limit: MAX_BODY_BYTES,
            });
        }
        self.raise(title, body)
    }

    /// The identity a Windows toast is delivered under, resolved to the platform default.
    #[must_use]
    pub fn app_id(&self) -> &str {
        if self.app_id.is_empty() {
            default_app_id()
        } else {
            self.app_id.as_str()
        }
    }

    /// Raises the notification on this platform.
    #[cfg(windows)]
    fn raise(&self, title: &str, body: &str) -> Result<(), NotificationError> {
        // The script is a constant of this program except for the identity, which is checked to
        // be an identity before it is interpolated: the *payload* arrives on standard input, so
        // no part of what a page wrote is ever parsed as PowerShell.
        let app_id = self.app_id();
        // The identity is validated by the module that owns identities, so a toast and an
        // install cannot disagree about what one is.
        crate::identity::check(app_id).map_err(|error| NotificationError::Failed {
            tool: "powershell",
            reason: error.to_string(),
        })?;
        let script = WINDOWS_SCRIPT
            .replace(APP_ID_PLACEHOLDER, app_id)
            .replace(TAG_PLACEHOLDER, &next_toast_tag());
        let outcome = run_command(
            Path::new("powershell"),
            "powershell",
            &[
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                script,
            ],
            Some(&format!("{title}\n{body}")),
            NOTIFICATION_TIMEOUT,
            MAX_BODY_BYTES,
        )
        .map_err(|error| tool_error("powershell", &error))?;
        if outcome.status != 0 || !outcome.stdout.contains(SHOWN_MARKER) {
            return Err(NotificationError::Failed {
                tool: "powershell",
                reason: first_line(&outcome.stderr, &outcome.stdout),
            });
        }
        Ok(())
    }

    /// Raises the notification on macOS.
    #[cfg(target_os = "macos")]
    fn raise(&self, title: &str, body: &str) -> Result<(), NotificationError> {
        let script = format!(
            "display notification {} with title {}",
            apple_script_literal(body),
            apple_script_literal(title)
        );
        let outcome = run_command(
            Path::new("osascript"),
            "osascript",
            &["-e".to_owned(), script],
            None,
            NOTIFICATION_TIMEOUT,
            MAX_BODY_BYTES,
        )
        .map_err(|error| tool_error("osascript", &error))?;
        if outcome.status != 0 {
            return Err(NotificationError::Failed {
                tool: "osascript",
                reason: first_line(&outcome.stderr, &outcome.stdout),
            });
        }
        Ok(())
    }

    /// Raises the notification on Linux.
    #[cfg(target_os = "linux")]
    fn raise(&self, title: &str, body: &str) -> Result<(), NotificationError> {
        let outcome = run_command(
            Path::new("notify-send"),
            "notify-send",
            &notify_send_arguments(self.app_id(), title, body),
            None,
            NOTIFICATION_TIMEOUT,
            MAX_BODY_BYTES,
        )
        .map_err(|error| tool_error("notify-send", &error))?;
        if outcome.status != 0 {
            return Err(NotificationError::Failed {
                tool: "notify-send",
                reason: first_line(&outcome.stderr, &outcome.stdout),
            });
        }
        Ok(())
    }

    /// No platform tool is known here.
    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    fn raise(&self, _title: &str, _body: &str) -> Result<(), NotificationError> {
        Err(NotificationError::Unavailable { tool: "none" })
    }
}

/// The PowerShell program that raises a toast from text arriving on standard input.
///
/// Three things here are load-bearing and each is deliberate:
///
/// * the template comes from the platform, and the title and body are appended as *text nodes*,
///   so XML metacharacters in a message stay characters;
/// * the whole script is a constant, so the payload cannot be read as code;
/// * the marker is printed only after `Show` returns, so a run that reports success is a run
///   where the platform accepted the notification.
#[cfg(windows)]
const WINDOWS_SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] > $null
[Windows.Data.Xml.Dom.XmlDocument, Windows.Data.Xml.Dom.XmlDocument, ContentType = WindowsRuntime] > $null
$payload = [Console]::In.ReadToEnd()
$split = $payload -split "`n", 2
$title = $split[0]
$body = if ($split.Count -gt 1) { $split[1] } else { '' }
$template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$nodes = $template.GetElementsByTagName('text')
$nodes.Item(0).AppendChild($template.CreateTextNode($title)) > $null
$nodes.Item(1).AppendChild($template.CreateTextNode($body)) > $null
$toast = [Windows.UI.Notifications.ToastNotification]::new($template)
$toast.Tag = '{TAG}'
$notifier = [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('{APP_ID}')
$notifier.Show($toast)
Write-Output 'ncui-toast-shown'"#;

/// Where the application identity is substituted into [`WINDOWS_SCRIPT`].
#[cfg(windows)]
const APP_ID_PLACEHOLDER: &str = "{APP_ID}";

/// Where the toast's tag is substituted into [`WINDOWS_SCRIPT`].
#[cfg(windows)]
const TAG_PLACEHOLDER: &str = "{TAG}";

/// How many toasts this process has raised, for tagging them apart.
///
/// Windows keeps an application's notifications in a store keyed by tag, and raising a second
/// toast under a tag already in that store is a constraint failure rather than a replacement —
/// which is what the first version of this module did, and what the second real-notification
/// test caught. A tag that names the process and the count is distinct for every notification
/// this process raises and cannot collide with another process's.
#[cfg(windows)]
static TOAST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// The tag the next toast is raised under.
#[cfg(windows)]
fn next_toast_tag() -> String {
    let sequence = TOAST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("ncui-{}-{sequence}", std::process::id())
}

/// What the Windows script prints once the platform has accepted the notification.
#[cfg(windows)]
const SHOWN_MARKER: &str = "ncui-toast-shown";

/// The identity a Windows toast is delivered under when the caller names none.
#[cfg(windows)]
fn default_app_id() -> &'static str {
    WINDOWS_APP_ID
}

/// The identity a Windows toast is delivered under when the caller names none.
#[cfg(not(windows))]
fn default_app_id() -> &'static str {
    ""
}

/// Escapes `text` into an `AppleScript` string literal, quotes included.
///
/// The result is a *literal*, so it begins and ends with a quote and nothing inside it can end
/// that literal: a backslash is doubled, a quote is escaped, and a line break becomes the escape
/// `AppleScript` understands, because a literal may not span lines. A body a page wrote cannot
/// therefore add a statement to the script.
#[must_use]
pub fn apple_script_literal(text: &str) -> String {
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('"');
    for character in text.chars() {
        match character {
            '\\' => literal.push_str("\\\\"),
            '"' => literal.push_str("\\\""),
            '\n' => literal.push_str("\\n"),
            '\r' => literal.push_str("\\r"),
            other => literal.push(other),
        }
    }
    literal.push('"');
    literal
}

/// The arguments `notify-send` is given, with the payload behind the option terminator.
///
/// `--` is what stops a body that begins with `-` from being read as a flag: without it,
/// `notify-send --title "-u"` style text would be a tool invocation rather than a message.
///
/// The application's identity is passed as the notification's app name when one is named, so the
/// notification is attributed to the application rather than to `notify-send` itself — which is what
/// the Windows arm achieves by delivering the toast under an identity, and what a user would expect
/// on either platform. An application that named no identity gets the tool's own default, which is
/// the behaviour this had before.
#[must_use]
pub fn notify_send_arguments(app_name: &str, title: &str, body: &str) -> Vec<String> {
    let mut arguments = Vec::with_capacity(4);
    if !app_name.is_empty() {
        arguments.push(format!("--app-name={app_name}"));
    }
    arguments.push("--".to_owned());
    arguments.push(title.to_owned());
    arguments.push(body.to_owned());
    arguments
}

/// Maps a process failure onto the notification error shape.
fn tool_error(tool: &'static str, error: &ProcessError) -> NotificationError {
    match error {
        ProcessError::NotFound { .. } => NotificationError::Unavailable { tool },
        other => NotificationError::Failed {
            tool,
            reason: other.to_string(),
        },
    }
}

/// The first line of a tool's error output, or of its ordinary output when it said nothing else.
fn first_line(stderr: &str, stdout: &str) -> String {
    let line = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("it reported no reason");
    line.chars().take(400).collect()
}

#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_apple_script_literal_cannot_be_ended_by_its_own_payload() {
        // The property the macOS path depends on: whatever a page writes, the script still has
        // one string literal in it and no statements the page chose.
        let hostile = "hello\" & (do shell script \"rm -rf /\") & \"";
        let literal = apple_script_literal(hostile);
        assert!(
            literal.starts_with('"') && literal.ends_with('"'),
            "{literal}"
        );
        let inner = &literal[1..literal.len() - 1];
        // Every quote inside the literal is escaped, so none of them ends it.
        let mut characters = inner.chars().peekable();
        while let Some(character) = characters.next() {
            if character == '\\' {
                let _ = characters.next();
                continue;
            }
            assert_ne!(
                character, '"',
                "an unescaped quote inside the literal would end it: {literal}"
            );
        }
    }

    #[test]
    fn an_apple_script_literal_keeps_the_text_a_reader_would_expect() {
        assert_eq!(apple_script_literal("plain"), "\"plain\"");
        assert_eq!(apple_script_literal("a\\b"), "\"a\\\\b\"");
        assert_eq!(apple_script_literal("a\"b"), "\"a\\\"b\"");
        // A literal may not span lines, so a line break becomes the escape form.
        assert_eq!(apple_script_literal("a\nb"), "\"a\\nb\"");
        assert_eq!(apple_script_literal("a\rb"), "\"a\\rb\"");
    }

    #[test]
    fn the_notify_send_payload_sits_behind_the_option_terminator() {
        // Without `--`, a body beginning with a dash is an option, and a page could choose the
        // tool's flags.
        let arguments = notify_send_arguments("", "title", "-u critical");
        assert_eq!(arguments[0], "--");
        assert_eq!(arguments[1], "title");
        assert_eq!(arguments[2], "-u critical");

        // An application that named an identity is the notification's app name, and the payload
        // still sits behind the terminator.
        let named = notify_send_arguments("com.example.app", "title", "-u critical");
        assert_eq!(named[0], "--app-name=com.example.app");
        assert_eq!(named[1], "--");
        assert_eq!(named[2], "title");
    }

    #[test]
    fn a_notification_longer_than_its_bound_is_refused_before_anything_runs() {
        let notifier = Notifier::new();
        let error = notifier
            .show(&"t".repeat(MAX_TITLE_BYTES + 1), "body")
            .expect_err("a title over the bound is refused");
        assert_eq!(error.code(), "notification_too_long");
        assert!(error.to_string().contains("title"), "{error}");
        let error = notifier
            .show("title", &"b".repeat(MAX_BODY_BYTES + 1))
            .expect_err("a body over the bound is refused");
        assert!(error.to_string().contains("body"), "{error}");
    }

    #[test]
    fn the_bounds_admit_an_ordinary_notification() {
        // What this asserts is about the *bounds*: a notification of exactly their size is not
        // refused for its length. Whether the platform then has a tool to raise it with is a fact
        // about the machine, and on a machine without one the answer is `Unavailable` — which is a
        // different answer from `TooLong`, and the one this test must not confuse with it.
        let notifier = Notifier::new();
        let outcome = notifier.show(&"t".repeat(MAX_TITLE_BYTES), &"b".repeat(MAX_BODY_BYTES));
        assert!(
            !matches!(outcome, Err(NotificationError::TooLong { .. })),
            "the bounds must admit a notification of exactly their size: {outcome:?}"
        );
    }

    #[test]
    fn an_app_id_is_used_when_one_is_given_and_the_platform_default_otherwise() {
        assert_eq!(Notifier::new().app_id(), default_app_id());
        assert_eq!(
            Notifier::with_app_id("com.example.app").app_id(),
            "com.example.app"
        );
    }

    #[test]
    fn an_application_identity_that_is_not_one_is_refused_before_it_reaches_the_script() {
        // The one value interpolated into the script is checked first, so a caller cannot turn an
        // identity into a statement. The rule lives in `identity`, where installing an application
        // uses the same one.
        assert!(crate::identity::check("com.example.app").is_ok());
        // The identity this module interpolates into its own script is one the validator accepts,
        // so the module cannot refuse its own default. Windows-only, because that is where the
        // script is.
        #[cfg(windows)]
        assert!(crate::identity::check(WINDOWS_APP_ID).is_ok());
        for hostile in ["", "a'; Remove-Item -Recurse / ;'b", "a\nb", "a$b"] {
            assert!(crate::identity::check(hostile).is_err(), "{hostile}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_real_notification_is_accepted_by_the_platform() {
        // The end of the chain this module can verify on a machine with no eyes on it: the
        // platform's own API accepted the notification. Whether the user then sees it depends on
        // do-not-disturb and on the identity, which is why the module documents that.
        let notifier = Notifier::new();
        notifier
            .show("NC-UI", "the notifications capability raised this")
            .expect("the platform accepts a notification");
    }

    #[cfg(windows)]
    #[test]
    fn a_payload_full_of_markup_is_a_message_rather_than_a_document() {
        // The toast is built by setting text nodes on the platform's template, so metacharacters
        // cannot change the document's structure. A raw-XML implementation would fail here.
        let notifier = Notifier::new();
        notifier
            .show(
                "<title> & \"quotes\"",
                "body with <b>markup</b>, an ampersand & a \"quote\"",
            )
            .expect("markup in a message is still a message");
    }
}
