//! Accessibility for the surface NC-UI owns.
//!
//! ADR-001 assigns the *accessibility tree* to the embedded `WebView`, and this crate does not
//! claim that tree. What it provides is the half around it: the semantic tree for NC-UI's own
//! widgets — native windows, menus, overlays, and GPU islands — plus the focus and keyboard
//! routing that make those widgets reachable.
//!
//! # Why this exists on its own
//!
//! `remaining-work.md` records the sequencing: accessibility is cheaper before the GPU islands
//! that would otherwise ship without it, and doing islands first means retrofitting a semantic
//! tree onto a rendering path that has none. That ordering only holds if the tree is something
//! that *can* be built without a GPU — which is what this crate is.
//!
//! # The three properties it holds
//!
//! 1. **Every focusable node has a name.** A control that cannot be announced is a control
//!    that exists only for sighted users. [`SemanticTree::unnamed_focusable`] is the release
//!    gate that makes that a build failure rather than a bug report.
//! 2. **Focus never strands.** Tab and Shift+Tab wrap, a node that disappears while focused
//!    hands focus to a live one, and focus is releasable when the document takes it.
//! 3. **Order is explicit.** Traversal is depth-first in a stated order, because a tree with
//!    the right nodes and the wrong sequence is unusable.
//!
//! # What is deliberately absent
//!
//! Text shaping, bidi, and screen-reader *output* encoding are the `WebView`'s. An island
//! an [`IslandDescriptor`] so it can be checked for the properties above, and the island half of
//! `a11y.support` stays unimplemented until islands exist.

pub mod focus;
pub mod tree;

pub use focus::{FocusOwner, FocusRefused, FocusState, Key};
pub use tree::{Node, Role, SemanticTree};

/// What a GPU island must provide to be reachable by a keyboard or a screen reader.
///
/// ADR-002 warns that a GPU surface with no accessibility strategy is the failure to avoid, and
/// this is that strategy expressed as data: an island that cannot produce one of these is
/// refused, rather than composited and left unreachable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IslandDescriptor {
    /// The island's place in the traversal sequence, relative to its siblings.
    pub order: u32,
    /// The label a screen reader announces for the island as a whole.
    pub label: String,
    /// The nodes the island exposes, which are merged into the host's semantic tree.
    pub nodes: Vec<Node>,
}

impl IslandDescriptor {
    /// Whether the island satisfies the properties this crate holds.
    ///
    /// The two conditions are exactly the failure modes: a nameless island announces as
    /// nothing, and an island with focusable nodes and no label is a keyboard trap with no
    /// way out.
    #[must_use]
    pub fn is_reachable(&self) -> bool {
        // The unnamed check walks the whole subtree. Checking only the island's top-level
        // nodes let a nested control hide: an island whose root is a group passes the check
        // while an unnamed button sits two levels down inside it, which is precisely the
        // shape a real toolbar or panel takes.
        if self.nodes.iter().any(Self::contains_unnamed_focusable) {
            return false;
        }
        let has_focusable = self.nodes.iter().any(Self::contains_focusable);
        has_focusable && !self.label.trim().is_empty()
    }

    /// Whether `node`'s subtree holds a focusable node with no name.
    fn contains_unnamed_focusable(node: &Node) -> bool {
        node.is_unnamed_focusable() || node.children.iter().any(Self::contains_unnamed_focusable)
    }

    /// Whether `node`'s subtree contains anything focusable.
    ///
    /// Free rather than a method: it never reads `self`, and taking `&self` for a pure walk
    /// of a node that is not part of the island is misleading.
    fn contains_focusable(node: &Node) -> bool {
        node.focusable || node.children.iter().any(Self::contains_focusable)
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

    fn island(label: &str, nodes: Vec<Node>) -> IslandDescriptor {
        IslandDescriptor {
            order: 10,
            label: label.to_owned(),
            nodes,
        }
    }

    #[test]
    fn a_named_island_with_named_focusables_is_reachable() {
        let island = island("Toolbar", vec![Node::leaf(Role::Button, "Bold", 1, true)]);
        assert!(island.is_reachable());
    }

    #[test]
    fn an_island_with_an_unnamed_focusable_is_not_reachable() {
        // The ADR-002 failure, stated as a test: a composited surface with a control nobody
        // can hear.
        let island = island(
            "Toolbar",
            vec![
                Node::leaf(Role::Button, "Bold", 1, true),
                Node::leaf(Role::Button, "", 2, true),
            ],
        );
        assert!(!island.is_reachable());
    }

    #[test]
    fn an_unnamed_focusable_nested_deep_inside_an_island_still_fails() {
        let island = island(
            "Panel",
            vec![Node::with_children(Role::Group, "", 1, false).child(
                Node::with_children(Role::List, "", 2, false).child(Node::leaf(
                    Role::ListItem,
                    "",
                    3,
                    true,
                )),
            )],
        );
        assert!(
            !island.is_reachable(),
            "nesting must not hide an unnamed control"
        );
    }

    #[test]
    fn an_island_with_no_focusable_content_is_not_reachable() {
        let island = island(
            "Decoration",
            vec![Node::leaf(Role::StaticText, "sparkle", 1, false)],
        );
        assert!(
            !island.is_reachable(),
            "an island with nothing to focus is not a keyboard destination"
        );
    }

    #[test]
    fn an_island_with_a_focusable_and_no_label_is_not_reachable() {
        let island = island("   ", vec![Node::leaf(Role::Button, "Bold", 1, true)]);
        assert!(
            !island.is_reachable(),
            "a keyboard trap with no announced way out is the defect"
        );
    }

    #[test]
    fn island_nodes_merge_into_the_host_focus_order() {
        // The end-to-end property: an island's controls are reachable by the same Tab walk
        // that reaches native controls.
        let island = island("Toolbar", vec![Node::leaf(Role::Button, "Italic", 3, true)]);
        let tree = SemanticTree::new()
            .root(
                Node::with_children(Role::Window, "W", 0, false)
                    .child(Node::leaf(Role::Button, "Save", 1, true))
                    .child(Node::leaf(Role::Button, "Save As", 2, true)),
            )
            .root(island.nodes.into_iter().next().expect("a node"));

        let mut focus = FocusState::new(FocusOwner::Native);
        let mut seen = Vec::new();
        for _ in 0..3 {
            let order = focus.route(&tree, Key::Tab).expect("tab").expect("a node");
            let name = tree
                .focus_order()
                .into_iter()
                .find(|n| n.order == order)
                .expect("the order resolves to a node")
                .name
                .clone();
            seen.push(name);
        }
        assert_eq!(
            seen,
            vec!["Save", "Save As", "Italic"],
            "island controls are in the walk"
        );
    }
}
