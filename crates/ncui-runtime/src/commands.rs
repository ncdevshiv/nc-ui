//! Application-defined page commands.
//!
//! A command is the application's own native surface. It is declared once with the
//! capabilities it requires, registered in the same action table the page bridge enforces,
//! and executed through a pipeline that rate-limits, authorizes, requires consent where
//! needed, and audits the call.
//!
//! Commands are registered during application startup and may be disposed explicitly. The
//! registry is shared with every open window's seam, so a command registered later is
//! immediately visible to pages without rebuilding a window.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use ncui_core::{Capability, CapabilitySet};
use ncui_webview::{ActionTable, ActionTableError, SeamError};
use serde_json::Value;

/// Why a command registration was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandRegistrationError {
    /// The action table refused the name or the command already exists.
    Action(ActionTableError),
    /// The page grant does not cover the command's required capabilities.
    PageNotGranted {
        /// The command name.
        name: String,
        /// The capabilities the page would need but does not hold.
        missing: CapabilitySet,
    },
}

impl fmt::Display for CommandRegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Action(error) => write!(f, "{error}"),
            Self::PageNotGranted { name, missing } => write!(
                f,
                "command `{name}` requires {missing}, which the page grant does not include; grant it before registering the command"
            ),
        }
    }
}

impl std::error::Error for CommandRegistrationError {}

impl From<ActionTableError> for CommandRegistrationError {
    fn from(error: ActionTableError) -> Self {
        Self::Action(error)
    }
}

/// A native command body.
///
/// The handler receives only the page's payload. Application state is captured when the
/// command is registered, so a handler cannot reach the runtime through an ambient handle.
pub type CommandHandler = Arc<dyn Fn(Value) -> Result<Value, SeamError> + Send + Sync>;

/// Where a command's work happens.
#[derive(Clone)]
pub(crate) enum CommandTarget {
    /// A built-in runtime method, dispatched through the agent pipeline.
    RuntimeMethod(&'static str),
    /// An application handler.
    Native(CommandHandler),
}

/// One registered command.
#[derive(Clone)]
pub(crate) struct PageCommand {
    /// What the page must hold to call it.
    pub requires: CapabilitySet,
    /// How it is executed.
    pub target: CommandTarget,
    /// Whether the runtime supplies the calling window's id to the command.
    pub caller_window: bool,
}

#[derive(Default)]
struct Inner {
    commands: BTreeMap<String, PageCommand>,
}

/// The runtime's command registry and the action table pages are checked against.
#[derive(Clone, Default)]
pub struct PageCommands {
    inner: Arc<RwLock<Inner>>,
    actions: ActionTable,
}

impl fmt::Debug for PageCommands {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PageCommands")
            .field("registered", &self.len())
            .finish_non_exhaustive()
    }
}

impl PageCommands {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The action table shared with every window's seam.
    #[must_use]
    pub fn actions(&self) -> ActionTable {
        self.actions.clone()
    }

    /// How many commands are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .commands
            .len()
    }

    /// Whether no commands are registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Looks up a command, cloning the entry so the caller may run it without a lock.
    #[must_use]
    pub(crate) fn get(&self, name: &str) -> Option<PageCommand> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .commands
            .get(name)
            .cloned()
    }

    /// Registers a command and returns the handle that can dispose it.
    ///
    /// The returned handle does not have to be retained for the command to stay registered;
    /// it exists so a plugin or window can remove its contribution during teardown.
    ///
    /// # Errors
    ///
    /// Returns [`CommandRegistrationError::Action`] for an invalid or duplicate name.
    pub fn register<F>(
        &self,
        name: &str,
        requires: CapabilitySet,
        summary: impl Into<String>,
        handler: F,
    ) -> Result<CommandRegistration, CommandRegistrationError>
    where
        F: Fn(Value) -> Result<Value, SeamError> + Send + Sync + 'static,
    {
        self.insert(
            name,
            requires,
            summary.into(),
            CommandTarget::Native(Arc::new(handler)),
            false,
        )?;
        Ok(CommandRegistration {
            commands: self.clone(),
            name: name.to_owned(),
            active: true,
        })
    }

    /// Executes an application command on behalf of a component.
    ///
    /// `authority` is the component's own narrowed grant, never the page's and never the
    /// runtime's: a component that was not given a capability cannot spend one by asking a
    /// command that needs it. The check happens here rather than only in the caller because a
    /// second entry point that skipped it would make the component host's gate decorative.
    ///
    /// Built-in runtime methods are refused. Those are reached through the agent pipeline, with
    /// the agent's authority and the audit trail that comes with it; handing a component a route
    /// around both would be a privilege path, not a service call.
    pub(crate) fn component_call(
        &self,
        name: &str,
        authority: CapabilitySet,
        input: Value,
    ) -> Result<Value, SeamError> {
        let command = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .commands
            .get(name)
            .cloned();
        let Some(command) = command else {
            return Err(SeamError::new(
                "unknown_service",
                format!("`{name}` is not a command this runtime can serve to a component"),
            ));
        };
        if !command.requires.is_subset_of(authority) {
            let missing = CapabilitySet::from_iter_caps(
                command
                    .requires
                    .iter()
                    .filter(|capability| !authority.contains(*capability)),
            );
            return Err(SeamError::new(
                "capability_denied",
                format!(
                    "`{name}` requires {}, which the component was not granted",
                    render_set(missing)
                ),
            ));
        }
        match command.target {
            CommandTarget::Native(handler) => handler(input),
            CommandTarget::RuntimeMethod(method) => Err(SeamError::new(
                "not_available",
                format!(
                    "`{name}` is the runtime-internal method `{method}`, which a component                      cannot reach"
                ),
            )),
        }
    }

    /// Registers a built-in runtime method.
    pub(crate) fn register_runtime_method(
        &self,
        name: &str,
        requires: CapabilitySet,
        summary: impl Into<String>,
        method: &'static str,
        caller_window: bool,
    ) -> Result<(), CommandRegistrationError> {
        self.insert(
            name,
            requires,
            summary.into(),
            CommandTarget::RuntimeMethod(method),
            caller_window,
        )
    }

    fn insert(
        &self,
        name: &str,
        requires: CapabilitySet,
        summary: String,
        target: CommandTarget,
        caller_window: bool,
    ) -> Result<(), CommandRegistrationError> {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if inner.commands.contains_key(name) {
            return Err(ActionTableError::Duplicate {
                name: name.to_owned(),
            }
            .into());
        }
        self.actions.declare(name, requires, summary)?;
        inner.commands.insert(
            name.to_owned(),
            PageCommand {
                requires,
                target,
                caller_window,
            },
        );
        Ok(())
    }

    fn unregister(&self, name: &str) -> bool {
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let removed = inner.commands.remove(name).is_some();
        if removed {
            self.actions.remove(name);
        }
        removed
    }
}

/// Renders a capability set for an error message, or `nothing` for the empty set, so a
/// refusal never reads as "requires " and leaves the reader guessing.
fn render_set(set: CapabilitySet) -> String {
    if set.is_empty() {
        return "nothing".to_owned();
    }
    let mut names: Vec<&str> = set.iter().map(Capability::as_str).collect();
    names.sort_unstable();
    names.join(", ")
}

/// A command registration that can be disposed.
#[derive(Debug)]
#[must_use = "keep the handle to remove the command during teardown"]
pub struct CommandRegistration {
    commands: PageCommands,
    name: String,
    active: bool,
}

impl CommandRegistration {
    /// The command name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Removes the command from the action table and the runtime.
    ///
    /// Returns whether this call removed it; a second call is a no-op and reports `false`.
    pub fn dispose(&mut self) -> bool {
        if !self.active {
            return false;
        }
        self.active = false;
        self.commands.unregister(&self.name)
    }
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
    use ncui_core::Capability;
    use serde_json::json;

    fn requires(capability: Capability) -> CapabilitySet {
        CapabilitySet::empty().with(capability)
    }

    #[test]
    fn a_registered_command_is_visible_in_the_action_table() {
        let commands = PageCommands::new();
        let _registration = commands
            .register(
                "read_name",
                requires(Capability::InspectRuntime),
                "Read the application name",
                |_| Ok(json!({ "name": "NC-UI" })),
            )
            .expect("register");
        assert_eq!(
            commands.actions().requires("read_name"),
            Some(requires(Capability::InspectRuntime))
        );
        let definitions = commands.actions().entries();
        let definition = definitions
            .iter()
            .find(|(name, _)| name == "read_name")
            .map(|(_, definition)| definition)
            .expect("the action is advertised");
        assert_eq!(definition.summary, "Read the application name");
    }

    #[test]
    fn a_duplicate_command_is_refused() {
        let commands = PageCommands::new();
        let _first = commands
            .register("echo", CapabilitySet::empty(), "Echo", |_| Ok(Value::Null))
            .expect("first");
        let error = commands
            .register("echo", CapabilitySet::empty(), "Echo", |_| Ok(Value::Null))
            .expect_err("a duplicate must be refused");
        assert!(matches!(
            error,
            CommandRegistrationError::Action(ActionTableError::Duplicate { .. })
        ));
    }

    #[test]
    fn an_invalid_command_name_is_refused() {
        let commands = PageCommands::new();
        let too_long = "a".repeat(65);
        for name in ["", "Upper", "with space", "ncui.method", too_long.as_str()] {
            let error = commands
                .register(name, CapabilitySet::empty(), "Bad", |_| Ok(Value::Null))
                .expect_err("an invalid name must be refused");
            assert!(matches!(
                error,
                CommandRegistrationError::Action(ActionTableError::InvalidName { .. })
            ));
        }
    }

    #[test]
    fn disposing_a_registration_removes_the_command_and_the_action() {
        let commands = PageCommands::new();
        let mut registration = commands
            .register("echo", CapabilitySet::empty(), "Echo", |_| Ok(Value::Null))
            .expect("register");
        assert!(registration.dispose(), "the first dispose removes it");
        assert!(
            commands.get("echo").is_none(),
            "the command must not remain registered"
        );
        assert!(
            commands.actions().requires("echo").is_none(),
            "the action table must not advertise a disposed command"
        );
        assert!(
            !registration.dispose(),
            "a second dispose must report that it removed nothing"
        );
    }
}
