//! The generic adapter between one typed kit widget and the provider contract.
//!
//! gpui entities are strongly typed, while `ncui-gpui` correctly refuses to know any
//! provider's types. The adapter keeps those two facts in their proper places: the
//! provider build type-erases only the root view it hands back, and the per-window
//! instance retains the typed child entity so commands can still mean something precise.

use std::marker::PhantomData;
use std::sync::Arc;

use gpui::{
    AnyView, App, AppContext as _, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Window, div, px,
};
use nc_ui_kit::theme::Theme;
use ncui_gpui::{NativeEventSink, NativeViewContext, NativeViewInstance, NativeViewProvider};
use serde_json::Value;

use crate::KitProviderError;

/// The implementation contract for one kit widget exposed as a native view.
///
/// Implementations are stateless: every method is associated with the widget type rather
/// than with a widget instance, because per-window state belongs to the gpui entity that
/// [`KitWidget::build_child`] creates. Props parsing is part of the contract rather than
/// a detail of `build` so the same validation governs creation and later replacement.
pub trait KitWidget: Send + Sync + 'static {
    /// The concrete gpui view type this widget constructs.
    type View: Render + 'static;
    /// The typed props the widget uses after validation.
    type Props: Clone + Send + 'static;

    /// The stable provider identifier, including the `nc-ui-kit::` namespace.
    const ID: &'static str;

    /// Validates and decodes one JSON props document.
    ///
    /// Implementations must refuse unknown or ill-typed fields with
    /// [`KitProviderError::InvalidProps`] naming the field. Accepting a wrong shape
    /// silently would move a page author's mistake into user-visible behaviour, where it
    /// is much harder to diagnose.
    ///
    /// # Errors
    ///
    /// Returns the typed refusal when the JSON is not an object-shaped props document or
    /// any field fails the widget's schema.
    fn parse_props(props: &Value) -> Result<Self::Props, KitProviderError>;

    /// Constructs the typed child entity for one window.
    ///
    /// The event sink is passed explicitly because interaction callbacks inside kit
    /// widgets are where native events are born. Constructors are expected to wire real
    /// interactions to that sink, not merely to store it for later commands.
    ///
    /// # Errors
    ///
    /// Returns the typed refusal when the validated props describe a configuration the
    /// widget cannot construct.
    fn build_child(
        props: &Self::Props,
        events: Arc<NativeEventSink>,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<Entity<Self::View>, KitProviderError>;

    /// Performs one provider-defined verb on the typed child.
    ///
    /// `props` are the most recently accepted replacement props, not necessarily the
    /// user's current interaction state. Implementations should read the entity for
    /// current state and use the props only for schema-fixed facts such as the valid tab
    /// or option set.
    ///
    /// # Errors
    ///
    /// Returns the typed refusal when the verb is unknown, its payload is invalid, or
    /// the view's current state does not admit it.
    fn invoke_child(
        view: &Entity<Self::View>,
        props: &Self::Props,
        events: Arc<NativeEventSink>,
        action: &str,
        payload: &Value,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), KitProviderError>;

    /// The padding around the child in logical pixels.
    ///
    /// Controls that are small interactive objects sit inside padding; surfaces such as
    /// dialogs and tab strips override this to zero because they own the window edge.
    fn padding() -> f32 {
        20.0
    }
}

/// A stateless native-view provider for one [`KitWidget`] implementation.
///
/// The type parameter carries the whole implementation, so the value itself is a marker.
/// That is deliberate: a provider shared across windows must not accumulate per-window
/// state, and a type with no fields offers nowhere for that mistake to live.
pub struct KitProvider<W: KitWidget> {
    marker: PhantomData<fn() -> W>,
}

impl<W: KitWidget> KitProvider<W> {
    /// A provider marker ready for registration.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            marker: PhantomData,
        }
    }
}

impl<W: KitWidget> Default for KitProvider<W> {
    fn default() -> Self {
        Self::new()
    }
}

impl<W: KitWidget> core::fmt::Debug for KitProvider<W> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KitProvider").field("id", &W::ID).finish()
    }
}

impl<W: KitWidget> NativeViewProvider for KitProvider<W> {
    fn id(&self) -> &str {
        W::ID
    }

    fn build(
        &self,
        context: NativeViewContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(AnyView, Box<dyn NativeViewInstance>), String> {
        let events = Arc::clone(&context.events);
        let props = W::parse_props(&context.props).map_err(|error| error.to_string())?;
        let child = W::build_child(&props, Arc::clone(&events), window, cx)
            .map_err(|error| error.to_string())?;
        let child_view: AnyView = child.clone().into();
        let padding = W::padding();
        let root = cx.new(|cx| KitRoot::new(child_view, padding, cx));
        let root_view: AnyView = root.clone().into();
        Ok((
            root_view,
            Box::new(KitInstance::<W> {
                root,
                child,
                props,
                events,
            }),
        ))
    }
}

/// The per-window half of a provider: typed child plus the root that displays it.
struct KitInstance<W: KitWidget> {
    root: Entity<KitRoot>,
    child: Entity<W::View>,
    props: W::Props,
    events: Arc<NativeEventSink>,
}

impl<W: KitWidget> NativeViewInstance for KitInstance<W> {
    fn update_props(
        &mut self,
        props: &Value,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        let next_props = W::parse_props(props).map_err(|error| error.to_string())?;
        let child = W::build_child(&next_props, Arc::clone(&self.events), window, cx)
            .map_err(|error| error.to_string())?;
        let child_view: AnyView = child.clone().into();
        self.root.update(cx, |root, cx| {
            root.replace_child(child_view);
            cx.notify();
        });
        self.child = child;
        self.props = next_props;
        Ok(())
    }

    fn invoke(
        &mut self,
        action: &str,
        payload: &Value,
        window: &mut Window,
        cx: &mut App,
    ) -> Result<(), String> {
        W::invoke_child(
            &self.child,
            &self.props,
            Arc::clone(&self.events),
            action,
            payload,
            window,
            cx,
        )
        .map_err(|error| error.to_string())
    }
}

/// The themed root every provider shares.
///
/// The root owns the window-wide responsibilities — background, foreground, font and
/// theme redraw — so each widget implementation can remain a widget rather than a small
/// copy of the same window chrome. It is intentionally not public: the provider contract
/// type-erases it, and exposing it would invite hosts to depend on implementation chrome.
pub(crate) struct KitRoot {
    child: AnyView,
    padding: f32,
}

impl KitRoot {
    fn new(child: AnyView, padding: f32, cx: &mut Context<'_, Self>) -> Self {
        // `Theme::set_global` notifies global observers rather than every view. Observing
        // here is what makes a runtime theme change reach windows that are already open.
        cx.observe_global::<Theme>(|_, cx| cx.notify()).detach();
        Self { child, padding }
    }

    fn replace_child(&mut self, child: AnyView) {
        self.child = child;
    }
}

impl Render for KitRoot {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = Theme::get(cx);
        div()
            .size_full()
            .bg(theme.bg)
            .text_color(theme.text)
            .font_family(nc_ui_kit::tokens::SANS_FONT)
            .p(px(self.padding))
            .child(self.child.clone())
    }
}
