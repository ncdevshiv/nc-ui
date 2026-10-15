//! Window management, the first half of the application surface.
//!
//! ADR-001 splits the system: the `WebView` owns the page, NC-UI owns the runtime around it.
//! This module is the part of that split which decides *which windows exist*. It does not
//! own an engine — it holds a [`ncui_webview::WindowRegistry`] driven through the
//! [`ncui_webview::HostFactory`] port, so a real embedding layer (`ncui-webview-wry`) plugs
//! in at the seam rather than being special-cased here.
//!
//! The registry is inside a `Mutex` because the JSON-RPC surface is reached from several
//! connections at once (see [`crate::transport`]) and two `ncui.window.create` calls racing
//! must not both believe they opened the first window.

use std::fmt;
use std::sync::Mutex;

use ncui_core::{CapabilitySet, WindowId};
use ncui_webview::{HostFactory, MountedComponent, WindowRegistry, WindowRejection, WindowSummary};

/// The window surface available to agents.
#[derive(Default)]
pub struct Windows {
    registry: Mutex<Option<WindowRegistry>>,
    factory: Mutex<Option<Box<dyn HostFactory>>>,
    manifests: Mutex<Vec<MountedComponent>>,
}

impl fmt::Debug for Windows {
    /// Reports the surface's observable shape rather than dumping every mounted component,
    /// which would itself stall a surface being inspected.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Windows")
            .field("open", &self.len().unwrap_or(0))
                .field(
                    "host_installed",
                    &self
                        .factory
                        .lock()
                        .is_ok_and(|guard| guard.is_some()),
                )
            .finish_non_exhaustive()
    }
}

impl Windows {
    /// An empty surface. No registry is installed, so every window call is refused until
    /// one is — a runtime without a host must say so rather than inventing windows.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs a registry, which also installs the host that will open windows.
    ///
    /// A registry starts with an empty navigation allow-list and an empty page grant: both
    /// are widened deliberately by the caller, because a default that permits anything is
    /// the failure this project's capability module exists to prevent.
    #[must_use]
    pub fn with_registry(mut self, registry: WindowRegistry) -> Self {
        self.registry = Mutex::new(Some(registry));
        self
    }

    /// Installs the host factory that `open` uses to spawn engine hosts.
    ///
    /// A runtime without a factory refuses every `ncui.window.create` rather than inventing
    /// windows, so a factory is mandatory before the window surface can be used.
    #[must_use]
    pub fn with_factory(mut self, factory: Box<dyn HostFactory>) -> Self {
        self.factory = Mutex::new(Some(factory));
        self
    }

    /// Runs `body` against the registry, if one is installed.
    ///
    /// A poisoned lock is recovered rather than propagated: a panic in one request must not
    /// make every later window call fail with an opaque message, and the registry stays
    /// consistent because every mutation is a whole map operation.
    fn with_registry_mut<T>(
        &self,
        body: impl FnOnce(&mut WindowRegistry) -> Result<T, WindowRejection>,
    ) -> Result<T, WindowRejection> {
        let mut guard = self
            .registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match guard.as_mut() {
            Some(registry) => body(registry),
            // Refusing rather than answering empty: an agent told "no windows are open"
            // when the truth is "this runtime has no host" would act on the wrong premise.
            None => Err(WindowRejection::EngineUnavailable {
                reason: Some(
                    "no webview host is installed; window operations are unavailable".to_owned(),
                ),
            }),
        }
    }

    /// Opens a window through the registry's own factory.
    pub fn open(
        &self,
        url: &str,
        grant: Option<CapabilitySet>,
    ) -> Result<WindowId, WindowRejection> {
        let factory = self
            .factory
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(factory) = factory.as_deref() else {
            return Err(WindowRejection::EngineUnavailable {
                reason: Some(
                    "no webview host is installed; window operations are unavailable".to_owned(),
                ),
            });
        };
        self.with_registry_mut(|registry| registry.open(factory, url, grant))
    }

    /// Lists every open window.
    pub fn summaries(&self) -> Result<Vec<WindowSummary>, WindowRejection> {
        self.with_registry_mut(|registry| Ok(registry.summaries()))
    }

    /// Closes a window, reporting whether it was open.
    pub fn close(&self, id: WindowId) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| Ok(registry.close(id).is_some()))
    }

    /// Mounts a component into a window.
    ///
    /// Returns the window's component list when the window exists. Mounting into an unknown
    /// window is a refusal rather than a silent success, because an agent that believes a
    /// component mounted must be able to see it.
    pub fn mount(
        &self,
        window: WindowId,
        component: MountedComponent,
    ) -> Result<Vec<MountedComponent>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry = registry.get_mut(window).ok_or_else(|| {
                WindowRejection::EngineUnavailable {
                    reason: Some(format!("window {window} is not open")),
                }
            })?;
            entry.mount(component.clone());
            // Record the mount in the window-manifest index so `ncui.components.list`
            // reports what the host mounted, not what a page claims. A remount replaces
            // the prior entry rather than duplicating it.
            let mut manifests = self
                .manifests
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            manifests.retain(|c| c.id != component.id);
            manifests.push(component);
            Ok(entry.components.clone())
        })
    }

    /// Unmounts a component from a window.
    pub fn unmount(
        &self,
        window: WindowId,
        component: ncui_core::ComponentId,
    ) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| {
            let entry = registry.get_mut(window).ok_or_else(|| {
                WindowRejection::EngineUnavailable {
                    reason: Some(format!("window {window} is not open")),
                }
            })?;
            let removed = entry.unmount(component).is_some();
            if removed {
                let mut manifests = self
                    .manifests
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                manifests.retain(|c| c.id != component);
            }
            Ok(removed)
        })
    }

    /// Every component mounted in any window, each tagged with its window.
    pub fn components(&self) -> Result<Vec<ComponentEntry>, WindowRejection> {
        self.with_registry_mut(|registry| {
            let mut entries = Vec::new();
            for window in registry.all() {
                for component in &window.components {
                    entries.push(ComponentEntry {
                        window: window.id,
                        id: component.id,
                        name: component.name.clone(),
                        abi_version: component.abi_version.clone(),
                        granted: component.granted,
                    });
                }
            }
            Ok(entries)
        })
    }

    /// How many windows are open.
    pub fn len(&self) -> Result<usize, WindowRejection> {
        self.with_registry_mut(|registry| Ok(registry.len()))
    }

    /// Whether no windows are open.
    pub fn is_empty(&self) -> Result<bool, WindowRejection> {
        self.with_registry_mut(|registry| Ok(registry.is_empty()))
    }

    /// Every component recorded in the window-manifest index, cloned out.
    ///
    /// This is what `ncui.components.list` reports: the host's own record of mounted
    /// components, not a page's claim about them.
    pub fn manifests(&self) -> Result<Vec<MountedComponent>, WindowRejection> {
        let manifests = self
            .manifests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(manifests.clone())
    }
}

/// One component, tagged with the window that holds it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ComponentEntry {
    /// The window this component is mounted in.
    pub window: WindowId,
    /// The component's identity within that window.
    pub id: ncui_core::ComponentId,
    /// The component's name, as the host knows it.
    pub name: String,
    /// The negotiated ABI version, when one was agreed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abi_version: Option<String>,
    /// The capabilities the component was granted after narrowing.
    #[serde(default, skip_serializing_if = "CapabilitySet::is_empty")]
    pub granted: CapabilitySet,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ncui_core::{Capability, ComponentId};
    use ncui_webview::{MountedComponent, WindowRegistry};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// A factory that counts its invocations, so a test can prove one window is one host.
    #[derive(Debug, Default)]
    struct Counting {
        spawned: Arc<AtomicUsize>,
    }

    impl Clone for Counting {
        fn clone(&self) -> Self {
            Self {
                spawned: Arc::clone(&self.spawned),
            }
        }
    }

    impl HostFactory for Counting {
        fn spawn(
            &self,
            _window: WindowId,
            _url: &str,
        ) -> Option<Box<dyn ncui_webview::WebViewBackend>> {
            self.spawned.fetch_add(1, Ordering::Relaxed);
            Some(Box::new(Fake))
        }
    }

    #[derive(Debug)]
    struct Fake;

    impl ncui_webview::WebViewBackend for Fake {
        fn last_error(&self) -> Option<String> {
            None
        }
        fn is_alive(&self) -> bool {
            true
        }
    }

    #[test]
    fn opening_a_window_without_a_registry_is_refused_rather_than_reporting_empty() {
        let windows = Windows::new();
        let error = windows
            .open("https://app.example.com", None)
            .expect_err("a runtime with no host must not pretend to open a window");
        assert!(
            matches!(error, WindowRejection::EngineUnavailable { .. }),
            "got: {error}"
        );
        let listed = windows.summaries().expect_err("nothing to list");
        assert!(
            matches!(listed, WindowRejection::EngineUnavailable { .. }),
            "got: {listed}"
        );
        assert!(
            windows.components().is_err(),
            "an empty surface must not report an empty component list as if it were real"
        );
    }

    #[test]
    fn opening_a_window_with_a_factory_registers_exactly_one_window() {
        let factory = Counting::default();
        let windows = Windows::new()
            .with_registry(WindowRegistry::new())
            .with_factory(Box::new(factory.clone()));
        let id = windows
            .open("https://app.example.com", None)
            .expect("a live factory opens a window");
        assert_eq!(factory.spawned.load(Ordering::Relaxed), 1, "one window is one engine");
        let summaries = windows.summaries().expect("a registry is installed");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].id, id);
        assert_eq!(windows.len().expect("installed"), 1);
    }

    #[test]
    fn closing_a_window_reports_whether_it_was_open() {
        let windows = Windows::new()
            .with_registry(WindowRegistry::new())
            .with_factory(Box::new(Counting::default()));
        let id = windows
            .open("https://app.example.com", None)
            .expect("opens");
        assert!(windows.close(id).expect("closes"));
        assert!(
            !windows.close(id).expect("the second close is a refusal"),
            "an agent must not be told it closed a window twice"
        );
        assert_eq!(windows.len().expect("installed"), 0);
    }

    #[test]
    fn a_mounted_component_is_listed_with_its_window() {
        let windows = Windows::new()
            .with_registry(WindowRegistry::new())
            .with_factory(Box::new(Counting::default()));
        let id = windows
            .open("https://app.example.com", None)
            .expect("opens");
        windows
            .mount(
                id,
                MountedComponent {
                    id: ComponentId::new(),
                    name: "chart".to_owned(),
                    abi_version: Some("1.0".to_owned()),
                    granted: CapabilitySet::empty().with(Capability::InspectRuntime),
                },
            )
            .expect("mounting into an open window works");
        let components = windows.components().expect("listed");
        assert_eq!(components.len(), 1, "the component is reported");
        assert_eq!(components[0].name, "chart");
        assert_eq!(components[0].window, id, "the component names its window");
        assert_eq!(
            components[0].abi_version.as_deref(),
            Some("1.0"),
            "the negotiated version travels with the component"
        );
    }

    #[test]
    fn mounting_into_an_unknown_window_is_a_refusal() {
        let windows = Windows::new()
            .with_registry(WindowRegistry::new())
            .with_factory(Box::new(Counting::default()));
        let error = windows
            .mount(
                WindowId::new(),
                MountedComponent {
                    id: ComponentId::new(),
                    name: "chart".to_owned(),
                    abi_version: None,
                    granted: CapabilitySet::empty(),
                },
            )
            .expect_err("a component must not mount into a window that does not exist");
        assert!(
            error.to_string().contains("is not open"),
            "the refusal must name the window; got: {error}"
        );
    }

    #[test]
    fn unmounting_removes_the_component_and_reports_it() {
        let windows = Windows::new()
            .with_registry(WindowRegistry::new())
            .with_factory(Box::new(Counting::default()));
        let id = windows
            .open("https://app.example.com", None)
            .expect("opens");
        let component = ComponentId::new();
        windows
            .mount(
                id,
                MountedComponent {
                    id: component,
                    name: "chart".to_owned(),
                    abi_version: None,
                    granted: CapabilitySet::empty(),
                },
            )
            .expect("mounts");
        assert!(
            windows.unmount(id, component).expect("unmounts"),
            "the component was mounted, so unmounting it succeeds"
        );
        assert!(
            !windows.unmount(id, component).expect("the second unmount is a refusal"),
            "an agent must not be told it unmounted what was already gone"
        );
        assert!(windows.components().expect("listed").is_empty());
    }

    #[test]
    fn the_same_component_mounted_twice_is_listed_once() {
        // A remount replaces rather than duplicates: an agent counting components must not
        // see one instance as two.
        let windows = Windows::new()
            .with_registry(WindowRegistry::new())
            .with_factory(Box::new(Counting::default()));
        let id = windows
            .open("https://app.example.com", None)
            .expect("opens");
        let component = ComponentId::new();
        for name in ["first", "second"] {
            windows
                .mount(
                    id,
                    MountedComponent {
                        id: component,
                        name: name.to_owned(),
                        abi_version: None,
                        granted: CapabilitySet::empty(),
                    },
                )
                .expect("mounts");
        }
        let components = windows.components().expect("listed");
        assert_eq!(components.len(), 1, "a remount replaces the entry");
        assert_eq!(components[0].name, "second", "the latest mount wins");
    }
}
