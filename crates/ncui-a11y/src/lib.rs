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
//! Text shaping, bidi, and screen-reader *output* encoding are the `WebView`'s.
//!
//! What this crate provides instead is the strategy a GPU island must satisfy to be
//! reachable: an [`IslandDescriptor`] describing what the island exposes, and the
//! [`tree::SemanticTree::merge_island`] that folds those nodes into the host's own tree so
//! the island's controls sit in the same focus ring as everything else. An island that
//! cannot produce a descriptor is refused rather than composited and left unreachable.

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
    /// The two failure modes are a nameless island, which announces as nothing, and an unnamed
    /// focusable node, which is a keyboard trap with no way out. Both are refused here.
    ///
    /// What is deliberately *not* required is something focusable. An island whose content is a list
    /// of rows has nothing to focus and is still reachable: a screen reader reads a labelled list by
    /// walking it, which is how every list on every platform is reached. Requiring focusable content
    /// refused exactly the islands this runtime can produce — a region of text — while catching
    /// nothing that the unnamed-focusable check does not already catch.
    ///
    /// Keyboard reachability is a separate question with a separate answer: the host puts an
    /// island's focusable nodes into its own focus order when it merges them, so an island with
    /// nothing focusable simply contributes nothing to that walk.
    #[must_use]
    pub fn is_reachable(&self) -> bool {
        // The unnamed check walks the whole subtree. Checking only the island's top-level
        // nodes let a nested control hide: an island whose root is a group passes the check
        // while an unnamed button sits two levels down inside it, which is precisely the
        // shape a real toolbar or panel takes.
        if self.nodes.iter().any(Self::contains_unnamed_focusable) {
            return false;
        }
        // A named island with nothing in it is a label announcing an absence, which is worse than
        // no island at all: it tells the reader there is something to hear and there is not.
        !self.label.trim().is_empty() && !self.nodes.is_empty()
    }

    /// Whether `node`'s subtree holds a focusable node with no name.
    fn contains_unnamed_focusable(node: &Node) -> bool {
        node.is_unnamed_focusable() || node.children.iter().any(Self::contains_unnamed_focusable)
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
    fn a_named_island_of_text_is_reachable() {
        // A list of rows is reached by reading it, not by focusing it. The old rule refused this
        // shape, which is the shape a region of text has.
        let notes = island(
            "Notes",
            vec![
                Node::with_children(Role::List, "Notes", 1, false)
                    .child(Node::leaf(Role::ListItem, "first", 2, false))
                    .child(Node::leaf(Role::ListItem, "second", 3, false)),
            ],
        );
        assert!(notes.is_reachable());

        // An island with no content at all is not a destination for anyone, and a label announcing
        // an absence is worse than no island.
        let empty = island("Decoration", Vec::new());
        assert!(!empty.is_reachable());
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
        // that reaches native controls. The merge is done by the tree, not by the test
        // inserting the nodes by hand — a test that assembles the tree itself proves only
        // that the test's assembly was right.
        let island = island("Toolbar", vec![Node::leaf(Role::Button, "Italic", 3, true)]);
        let mut tree = SemanticTree::new().root(
            Node::with_children(Role::Window, "W", 0, false)
                .child(Node::leaf(Role::Button, "Save", 1, true))
                .child(Node::leaf(Role::Button, "Save As", 2, true)),
        );
        assert!(
            tree.merge_island(&island),
            "a reachable island must be merged into the tree"
        );

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

    #[test]
    fn an_unreachable_island_is_not_merged_into_the_tree() {
        // The refusal matters as much as the merge: a composited surface with a control
        // nobody can hear must not reach the focus ring.
        let island = island(
            "Toolbar",
            vec![
                Node::leaf(Role::Button, "Bold", 1, true),
                Node::leaf(Role::Button, "", 2, true),
            ],
        );
        let mut tree = SemanticTree::new();
        assert!(
            !tree.merge_island(&island),
            "an island with an unnamed control must be refused"
        );
        assert!(tree.is_empty(), "a refused island must leave no trace");
    }

    #[test]
    fn a_merged_island_becomes_a_group_in_the_announcement() {
        // The island is announced as a group with its own label, so a screen-reader user
        // hears where they are.
        let island = island("Toolbar", vec![Node::leaf(Role::Button, "Bold", 1, true)]);
        let mut tree = SemanticTree::new();
        assert!(tree.merge_island(&island));
        let announcement = tree.to_announcement();
        assert!(
            announcement.contains("group Toolbar"),
            "the island must be announced: {announcement}"
        );
        assert!(
            announcement.contains("button Bold"),
            "the island's controls must be announced: {announcement}"
        );
    }
}
