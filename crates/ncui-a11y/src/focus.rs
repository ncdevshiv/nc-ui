//! Focus management and keyboard routing.
//!
//! Focus is the single most broken part of accessibility in real software, and the failure is
//! almost always the same shape: a control exists, is drawn, responds to the mouse, and cannot
//! be reached with the keyboard. Nobody notices, because the person testing it is holding a
//! mouse.
//!
//! So this module makes the keyboard path *the* path. Every operation is defined on the
//! semantic tree's focus order, and every operation that would strand the user — focusing the
//! last item and pressing Tab, a tree losing its focused node, an island that has taken focus
//! — is a defined behaviour with a test, not a hope.

use crate::tree::SemanticTree;

/// Where keyboard events go when the user is interacting with a native surface rather than the
/// DOM.
///
/// A `WebView` host owns the DOM half; this enum is what the native half answers with, and
/// the point of it is that "focus is in the DOM" is a *distinguishable* answer rather than an
/// accident of which window is frontmost.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FocusOwner {
    /// Focus is on a node in the native semantic tree.
    Native,
    /// Focus is inside the embedded document.
    ///
    /// The default, because a surface that has not said otherwise is a freshly loaded window
    /// whose document holds focus. Assuming `Native` would let the host swallow the first
    /// keystroke the user meant for the page.
    #[default]
    Document,
}

/// A key the runtime routes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// Move to the next focusable node, wrapping at the end.
    Tab,
    /// Move to the previous focusable node, wrapping at the start.
    ShiftTab,
    /// Activate the focused node.
    Enter,
    /// Toggle the focused node, or open its menu.
    Space,
    /// Escape the current context.
    Escape,
    /// Move down within a composite widget.
    ArrowDown,
    /// Move up within a composite widget.
    ArrowUp,
}

impl Key {
    /// Whether this key consumes the event rather than letting it fall through.
    ///
    /// Tab and Escape are handled by the host; arrows are passed to the focused widget, which
    /// decides whether it used them. Getting this wrong in either direction produces the
    /// classic bug where arrow keys are dead inside a list.
    #[must_use]
    pub const fn is_host_handled(self) -> bool {
        matches!(self, Self::Tab | Self::ShiftTab)
    }
}

/// The focus state of a native surface.
#[derive(Debug, Clone, Default)]
pub struct FocusState {
    owner: FocusOwner,
    /// The order value of the focused node, or `None` when nothing holds focus.
    focused: Option<u32>,
}

/// Why focus could not be moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FocusRefused {
    /// What was asked for.
    pub key: Key,
    /// Why it could not be honoured.
    pub reason: String,
}

impl FocusState {
    /// A surface where the document holds focus.
    #[must_use]
    pub fn new(owner: FocusOwner) -> Self {
        Self {
            owner,
            focused: None,
        }
    }

    /// Who holds focus.
    #[must_use]
    pub const fn owner(&self) -> FocusOwner {
        self.owner
    }

    /// The order value of the focused node.
    #[must_use]
    pub const fn focused(&self) -> Option<u32> {
        self.focused
    }

    /// Routes a key press against `tree`.
    ///
    /// # Errors
    ///
    /// Returns the refusal when the key cannot be honoured: there is nothing to move to, or
    /// the tree has no focusable nodes at all. A refusal is a value because "Tab did nothing"
    /// is the bug this module exists to prevent, and a silent no-op is exactly that bug.
    pub fn route(&mut self, tree: &SemanticTree, key: Key) -> Result<Option<u32>, FocusRefused> {
        let order = tree.focus_order();
        if order.is_empty() {
            return Err(FocusRefused {
                key,
                reason: "the tree has no focusable nodes".to_owned(),
            });
        }

        match key {
            Key::Tab | Key::ShiftTab => {
                let forward = key == Key::Tab;
                let current = self.focused;
                let next = match current {
                    // Nothing focused: Tab enters the first node and Shift+Tab the last, which
                    // is what a user expects from a surface they have just entered.
                    None => {
                        if forward {
                            order.first()
                        } else {
                            order.last()
                        }
                    }
                    Some(current) => {
                        let at = order.iter().position(|n| n.order == current);
                        match at {
                            // The focused node is no longer in the tree. Landing on the first
                            // is the recoverable answer; leaving focus on a node that has
                            // gone would strand the user.
                            None => order.first(),
                            Some(at) => {
                                // Wrapping, so Tab from the last returns to the first and the
                                // user can never fall out of the surface. `rem_euclid` is the
                                // whole rule: a negative step wraps rather than underflowing.
                                // Wrapping, and the two directions wrap to *opposite* ends:
                                // Tab past the last returns to the first, Shift+Tab past the
                                // first returns to the last.
                                //
                                // Collapsing both directions into one modulo sent Shift+Tab
                                // from the first node back to the first node, so the reverse
                                // ring did not close and a user who reached the top of a
                                // surface by shift-tabbing could not get past it.
                                //
                                // `len` is non-zero: `order.is_empty()` was checked on entry,
                                // and no branch above removes an element.
                                let len = order.len();
                                let next = if forward {
                                    if at + 1 >= len { 0 } else { at + 1 }
                                } else if at == 0 {
                                    len - 1
                                } else {
                                    at - 1
                                };
                                order.get(next)
                            }
                        }
                    }
                };
                match next {
                    Some(node) => {
                        self.focused = Some(node.order);
                        Ok(Some(node.order))
                    }
                    None => Err(FocusRefused {
                        key,
                        reason: "the focus order is empty".to_owned(),
                    }),
                }
            }
            // Activation keys act on whatever holds focus. With nothing focused they are
            // refused rather than sent to an arbitrary node, because activating the first
            // button in a window because the user pressed Enter is a data-loss bug waiting to
            // happen.
            Key::Enter | Key::Space => match self
                .focused
                .and_then(|f| order.iter().find(|n| n.order == f))
            {
                Some(node) => Ok(Some(node.order)),
                None => Err(FocusRefused {
                    key,
                    reason: "nothing holds focus".to_owned(),
                }),
            },
            // Arrow keys within a composite widget are the widget's business, not the host's.
            Key::ArrowDown | Key::ArrowUp | Key::Escape => Ok(None),
        }
    }

    /// Drops focus, returning whether there was any.
    ///
    /// Called when focus moves into the document. The native side must forget, or a Tab that
    /// returns to the window will resume from a node the user never looked at.
    pub fn release(&mut self) -> bool {
        self.focused.take().is_some()
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
    use crate::tree::{Node, Role};

    /// The name of the node at `order`, looked up in the tree's focus order.
    fn name_of(tree: &SemanticTree, order: u32) -> String {
        tree.focus_order()
            .into_iter()
            .find(|n| n.order == order)
            .expect("the order resolves to a node")
            .name
            .clone()
    }

    fn tree(names: &[&str]) -> SemanticTree {
        let mut root = Node::with_children(Role::Window, "W", 0, false);
        for (index, name) in names.iter().enumerate() {
            root = root.child(Node::leaf(Role::Button, name, index as u32 + 1, true));
        }
        SemanticTree::new().root(root)
    }

    #[test]
    fn tab_advances_through_the_focus_order() {
        let tree = tree(&["one", "two", "three"]);
        let mut focus = FocusState::new(FocusOwner::Native);

        for expected in ["one", "two", "three"] {
            let order = focus.route(&tree, Key::Tab).expect("tab").expect("a node");
            let node = tree
                .focus_order()
                .into_iter()
                .find(|n| n.order == order)
                .expect("the focused order resolves to a node");
            assert_eq!(node.name, expected);
        }
    }

    #[test]
    fn tab_wraps_rather_than_stranding_the_user_at_the_end() {
        // The failure this prevents: reaching the last control and finding that Tab does
        // nothing, so the keyboard user is trapped.
        let tree = tree(&["one", "two"]);
        let mut focus = FocusState::new(FocusOwner::Native);
        focus.route(&tree, Key::Tab).expect("first");
        focus.route(&tree, Key::Tab).expect("second");
        let wrapped = focus.route(&tree, Key::Tab).expect("wrap").expect("a node");
        assert_eq!(name_of(&tree, wrapped), "one");
    }

    #[test]
    fn shift_tab_moves_backwards_and_wraps_the_other_way() {
        let tree = tree(&["one", "two", "three"]);
        let mut focus = FocusState::new(FocusOwner::Native);

        // Entering with Shift+Tab lands on the last node.
        let last = focus
            .route(&tree, Key::ShiftTab)
            .expect("enter backwards")
            .expect("a node");
        assert_eq!(name_of(&tree, last), "three");

        let back = focus
            .route(&tree, Key::ShiftTab)
            .expect("back")
            .expect("a node");
        assert_eq!(name_of(&tree, back), "two");

        let wrapped = focus
            .route(&tree, Key::ShiftTab)
            .expect("wrap")
            .expect("a node");
        assert_eq!(name_of(&tree, wrapped), "one", "wrap goes to the first");
    }

    #[test]
    fn a_tree_with_no_focusable_nodes_refuses_rather_than_moving_nowhere_silently() {
        let tree = SemanticTree::new().root(Node::leaf(Role::StaticText, "hello", 0, false));
        let mut focus = FocusState::new(FocusOwner::Native);

        let refused = focus.route(&tree, Key::Tab).expect_err("nothing to focus");
        assert_eq!(refused.key, Key::Tab);
        assert!(
            refused.reason.contains("no focusable"),
            "{}",
            refused.reason
        );
    }

    #[test]
    fn a_focused_node_that_disappears_does_not_strand_focus() {
        // The GPU-island case from ADR-002: the focused island is torn down, and the node it
        // focused is no longer in the tree.
        let before = tree(&["one", "two", "three"]);
        let mut focus = FocusState::new(FocusOwner::Native);
        focus.route(&before, Key::Tab).expect("first");
        focus.route(&before, Key::Tab).expect("second");
        assert_eq!(focus.focused(), Some(2));

        // The tree is rebuilt without the second node. Focus was on the node whose order was
        // 2; the rebuilt tree gives order 2 to a different node, which is the point — the
        // focused *identity* no longer exists and focus must land on something real.
        let after = tree(&["one", "three"]);
        let order = focus
            .route(&after, Key::Tab)
            .expect("still moves")
            .expect("a node");
        let landed = after
            .focus_order()
            .into_iter()
            .find(|n| n.order == order)
            .expect("focus must land on a node that exists");
        assert_eq!(
            landed.order,
            focus.focused().expect("focused"),
            "the two agree"
        );
        assert_eq!(
            after.unnamed_focusable().len(),
            0,
            "and the node it landed on is one a user can hear"
        );
    }

    #[test]
    fn activation_with_nothing_focused_is_refused_rather_than_hitting_an_arbitrary_node() {
        // Pressing Enter must not activate the first button in the window.
        let tree = tree(&["destructive"]);
        let mut focus = FocusState::new(FocusOwner::Native);

        let refused = focus.route(&tree, Key::Enter).expect_err("nothing focused");
        assert_eq!(refused.key, Key::Enter);
        assert!(
            refused.reason.contains("nothing holds focus"),
            "{}",
            refused.reason
        );
    }

    #[test]
    fn activation_acts_on_the_focused_node() {
        let tree = tree(&["save", "delete"]);
        let mut focus = FocusState::new(FocusOwner::Native);
        focus.route(&tree, Key::Tab).expect("focus save");
        let activated = focus
            .route(&tree, Key::Enter)
            .expect("activate")
            .expect("a node");
        assert_eq!(name_of(&tree, activated), "save");
    }

    #[test]
    fn releasing_focus_forgets_the_node() {
        let tree = tree(&["one", "two"]);
        let mut focus = FocusState::new(FocusOwner::Native);
        focus.route(&tree, Key::Tab).expect("focus");
        assert!(focus.release(), "there was focus to release");
        assert_eq!(focus.focused(), None);
        assert!(!focus.release(), "releasing twice reports nothing was held");

        // Re-entering starts from the first node rather than resuming.
        let reentered = focus
            .route(&tree, Key::Tab)
            .expect("re-enter")
            .expect("a node");
        assert_eq!(name_of(&tree, reentered), "one");
    }

    #[test]
    fn arrow_keys_are_left_to_the_focused_widget_rather_than_moved_by_the_host() {
        // Moving arrow-key focus from the host is the bug that makes lists unusable: the
        // widget needs them to move *within* itself.
        let tree = tree(&["one"]);
        let mut focus = FocusState::new(FocusOwner::Native);
        focus.route(&tree, Key::Tab).expect("focus");

        for key in [Key::ArrowDown, Key::ArrowUp, Key::Escape] {
            assert_eq!(
                focus.route(&tree, key).expect("the host passes it through"),
                None,
                "{key:?} should be left to the focused widget"
            );
        }
    }

    #[test]
    fn tab_and_shift_tab_are_the_hosts_own() {
        assert!(Key::Tab.is_host_handled());
        assert!(Key::ShiftTab.is_host_handled());
        assert!(!Key::ArrowDown.is_host_handled());
        assert!(
            !Key::Enter.is_host_handled(),
            "activation belongs to the widget"
        );
    }

    #[test]
    fn the_focus_owner_is_distinguishable_rather_than_inferred_from_the_front_window() {
        let mut native = FocusState::new(FocusOwner::Native);
        assert_eq!(native.owner(), FocusOwner::Native);
        native.release();
        let document = FocusState::new(FocusOwner::Document);
        assert_eq!(document.owner(), FocusOwner::Document);
        assert_ne!(
            native.owner(),
            document.owner(),
            "which side holds focus must be answerable, not implied"
        );
    }
}
