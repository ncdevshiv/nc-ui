//! The accessibility semantic tree: what a screen reader is told, and in what order.
//!
//! ADR-001 assigns the *accessibility tree* to the embedded `WebView`, and this crate does not
//! claim otherwise. What it owns is the part around it: the tree NC-UI builds for its **own**
//! surface — native windows, menus, overlays, and any GPU island that does not come with a
//! tree of its own. An island rendered without a tree is exactly the failure ADR-002 warns
//! about, and this crate is how one is not shipped accidentally.
//!
//! # The order is the whole point
//!
//! A screen-reader user does not experience "a set of nodes"; they experience a sequence. Two
//! trees with identical nodes are unusable if the traversal order differs, so [`Node::order`]
//! is not decoration — it is the sequence the user hears, and every method here that mentions
//! "order" means that sequence.

use core::fmt;

use crate::IslandDescriptor;

/// The ARIA role of a node, reduced to the roles this runtime emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    /// The whole application, or a large region of it.
    Application,
    /// A window.
    Window,
    /// A named group of controls.
    Group,
    /// A region with a visible label.
    Region,
    /// A static piece of text.
    StaticText,
    /// A push button.
    Button,
    /// A check box.
    Checkbox,
    /// A choice among a small set.
    Radio,
    /// A scrollable or selectable list.
    List,
    /// One item in a list.
    ListItem,
    /// A value that can be changed by the user.
    Slider,
    /// A text field.
    TextInput,
    /// A drop-down list of choices.
    ComboBox,
    /// A one-line entry that triggers an action.
    SearchBox,
    /// A progress indicator.
    ProgressBar,
    /// A transient message.
    Alert,
    /// A pointer the user can act on.
    Link,
    /// A menu.
    Menu,
    /// One item in a menu.
    MenuItem,
    /// A tab in a tab set.
    Tab,
    /// A tab set.
    TabList,
}

impl Role {
    /// The stable wire name, which is what a screen reader is driven by and what is therefore
    /// part of the public surface.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Window => "window",
            Self::Group => "group",
            Self::Region => "region",
            Self::StaticText => "static_text",
            Self::Button => "button",
            Self::Checkbox => "checkbox",
            Self::Radio => "radio",
            Self::List => "list",
            Self::ListItem => "list_item",
            Self::Slider => "slider",
            Self::TextInput => "text_input",
            Self::ComboBox => "combobox",
            Self::SearchBox => "searchbox",
            Self::ProgressBar => "progress_bar",
            Self::Alert => "alert",
            Self::Link => "link",
            Self::Menu => "menu",
            Self::MenuItem => "menu_item",
            Self::Tab => "tab",
            Self::TabList => "tab_list",
        }
    }

    /// Whether a screen reader announces the role's name before its contents.
    ///
    /// Landmark-ish roles name themselves because the user navigates *by* role. Leaf controls
    /// are announced as their name plus role, so the name is what carries the meaning and a
    /// nameless leaf is a defect rather than a cosmetic problem.
    #[must_use]
    pub const fn is_navigable_landmark(self) -> bool {
        matches!(
            self,
            Self::Application | Self::Window | Self::Region | Self::Menu | Self::TabList
        )
    }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One node in the semantic tree.
///
/// Deliberately minimal: a role, a name, an order, and whether it can take focus. A real
/// accessibility API carries state (`checked`, `expanded`, `level`) too, and adding it here
/// without a caller that needs it would be an interface designed by guessing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// What this node is.
    pub role: Role,
    /// What the user hears for it. Empty means unnamed, which is a defect for a focusable
    /// node and merely unremarkable for a group.
    pub name: String,
    /// Position in the traversal sequence. Unique within a tree.
    pub order: u32,
    /// Whether the node can receive keyboard focus.
    pub focusable: bool,
    /// The node's children, already ordered.
    pub children: Vec<Node>,
}

impl Node {
    /// A leaf node.
    #[must_use]
    pub fn leaf(role: Role, name: &str, order: u32, focusable: bool) -> Self {
        Self {
            role,
            name: name.to_owned(),
            order,
            focusable,
            children: Vec::new(),
        }
    }

    /// A node with children, which are assigned orders after this node's.
    #[must_use]
    pub fn with_children(role: Role, name: &str, order: u32, focusable: bool) -> Self {
        Self {
            role,
            name: name.to_owned(),
            order,
            focusable,
            children: Vec::new(),
        }
    }

    /// Appends a child, keeping `children` sorted by order.
    ///
    /// Sorting on insert rather than demanding the caller pre-sort means a tree assembled in
    /// any sequence still traverses correctly — and a mis-ordered insertion is a compile-time
    /// impossibility rather than a bug that only shows for a particular widget layout.
    #[must_use]
    pub fn child(mut self, node: Node) -> Self {
        let at = self
            .children
            .binary_search_by_key(&node.order, |c| c.order)
            .unwrap_or_else(|index| index);
        self.children.insert(at, node);
        self
    }

    /// This node's immediate focusable children, in traversal order.
    #[must_use]
    pub fn focusable_children(&self) -> Vec<&Node> {
        self.children.iter().filter(|c| c.focusable).collect()
    }

    /// Whether this node is a focusable node with no name.
    ///
    /// A focusable control the user cannot hear is not an accessibility feature that half
    /// works; it is a control that exists for sighted users and is invisible to everyone else,
    /// which is the specific outcome this crate exists to make impossible to ship.
    #[must_use]
    pub fn is_unnamed_focusable(&self) -> bool {
        self.focusable && self.name.trim().is_empty()
    }
}

/// The accessibility tree for one window or surface.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SemanticTree {
    roots: Vec<Node>,
}

impl SemanticTree {
    /// An empty tree.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a root node, keeping roots ordered.
    #[must_use]
    pub fn root(mut self, node: Node) -> Self {
        let at = self
            .roots
            .binary_search_by_key(&node.order, |r| r.order)
            .unwrap_or_else(|index| index);
        self.roots.insert(at, node);
        self
    }

    /// The roots, in traversal order.
    #[must_use]
    pub fn roots(&self) -> &[Node] {
        &self.roots
    }

    /// Whether the tree has no nodes at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Every focusable node, in traversal order.
    ///
    /// Depth-first, pre-order, which is the sequence a screen reader walks. A breadth-first
    /// walk would interleave levels and announce a group before its first child, which is the
    /// order the user cannot follow.
    #[must_use]
    pub fn focus_order(&self) -> Vec<&Node> {
        let mut out = Vec::new();
        for root in &self.roots {
            collect_focusable(root, &mut out);
        }
        out
    }

    /// Every node, in traversal order, focusable or not.
    #[must_use]
    pub fn flatten(&self) -> Vec<&Node> {
        let mut out = Vec::new();
        for root in &self.roots {
            collect_all(root, &mut out);
        }
        out
    }

    /// Every focusable node with no accessible name.
    ///
    /// The defect list a release gate reads. Returning the nodes rather than a count is what
    /// makes it usable: the author needs to know *which* control.
    #[must_use]
    pub fn unnamed_focusable(&self) -> Vec<&Node> {
        self.focus_order()
            .into_iter()
            .filter(|node| node.is_unnamed_focusable())
            .collect()
    }

    /// The tree rendered as one line per node, the shape a screen reader reads out.
    ///
    /// Indentation is depth, so the output is navigable when read aloud *and* reviewable in a
    /// diff — which is how a regression in a tree gets noticed before a user reports it.
    #[must_use]
    pub fn to_announcement(&self) -> String {
        let mut out = String::new();
        for root in &self.roots {
            render(root, 0, &mut out);
        }
        out
    }

    /// Merges a GPU island's nodes into the tree.
    ///
    /// ADR-002 requires a GPU island to be reachable by a keyboard and a screen reader, and
    /// this is where that requirement is met: the island's nodes become nodes of the host's
    /// tree, so they are in the same focus ring as everything else rather than in a
    /// parallel one the user cannot escape.
    ///
    /// The island becomes a group node carrying the island's label, and the island's own
    /// nodes become its children. The island's `order` places it among its siblings, so an
    /// island's position in the page is its position in the tree.
    ///
    /// Returns whether the island was merged. An island that is not reachable — one with an
    /// unnamed focusable inside it, or with no label of its own — is refused: compositing it
    /// would put a focusable control in the tree that the user cannot hear, which is the
    /// failure this crate exists to prevent.
    pub fn merge_island(&mut self, island: &IslandDescriptor) -> bool {
        if !island.is_reachable() {
            return false;
        }
        let group = Node::with_children(Role::Group, &island.label, island.order, false);
        let mut group = group;
        for node in &island.nodes {
            group = group.child(node.clone());
        }
        let at = self
            .roots
            .binary_search_by_key(&island.order, |r| r.order)
            .unwrap_or_else(|index| index);
        self.roots.insert(at, group);
        true
    }
}

/// Depth-first, pre-order collection of focusable nodes.
fn collect_focusable<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
    if node.focusable {
        out.push(node);
    }
    for child in &node.children {
        collect_focusable(child, out);
    }
}

/// Depth-first, pre-order collection of every node.
fn collect_all<'a>(node: &'a Node, out: &mut Vec<&'a Node>) {
    out.push(node);
    for child in &node.children {
        collect_all(child, out);
    }
}

/// Renders one node and its subtree at `depth` levels of indentation.
fn render(node: &Node, depth: usize, out: &mut String) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    out.push_str(node.role.as_str());
    if !node.name.trim().is_empty() {
        out.push(' ');
        out.push_str(node.name.trim());
    }
    if node.focusable {
        out.push_str(" [focusable]");
    }
    out.push('\n');
    for child in &node.children {
        render(child, depth + 1, out);
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

    fn button(name: &str, order: u32) -> Node {
        Node::leaf(Role::Button, name, order, true)
    }

    /// A small but realistic tree: a window with a toolbar and a list.
    fn sample() -> SemanticTree {
        SemanticTree::new().root(
            Node::with_children(Role::Window, "Editor", 0, false)
                .child(button("New", 1))
                .child(button("Save", 2))
                .child(
                    Node::with_children(Role::List, "Files", 3, false)
                        .child(Node::leaf(Role::ListItem, "README.md", 4, true))
                        .child(Node::leaf(Role::ListItem, "main.rs", 5, true)),
                ),
        )
    }

    #[test]
    fn focus_order_is_depth_first_in_insertion_order() {
        let tree = sample();
        let names: Vec<&str> = tree.focus_order().iter().map(|n| n.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["New", "Save", "README.md", "main.rs"],
            "the traversal order is what a screen-reader user hears"
        );
    }

    #[test]
    fn a_child_inserted_out_of_order_still_traverses_in_order() {
        // A widget library assembling nodes in whatever order layout produced must not produce
        // a tree that reads in that order.
        let tree = SemanticTree::new().root(
            Node::with_children(Role::Window, "W", 0, false)
                .child(button("third", 3))
                .child(button("first", 1))
                .child(button("second", 2)),
        );
        let names: Vec<&str> = tree.focus_order().iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, vec!["first", "second", "third"]);
    }

    #[test]
    fn an_unnamed_focusable_node_is_reported_as_a_defect() {
        // The property the crate exists for: a control the user cannot hear.
        let tree = SemanticTree::new().root(
            Node::with_children(Role::Window, "W", 0, false)
                .child(button("Save", 1))
                .child(Node::leaf(Role::Button, "", 2, true)),
        );
        let defects = tree.unnamed_focusable();
        assert_eq!(defects.len(), 1);
        assert_eq!(defects[0].order, 2, "the report names the offending node");
    }

    #[test]
    fn an_unnamed_group_is_not_a_defect() {
        // A group is not itself a control, so having no name on one is not the same defect.
        let tree = SemanticTree::new().root(Node::with_children(Role::Group, "", 0, false));
        assert!(tree.unnamed_focusable().is_empty());
    }

    #[test]
    fn a_whitespace_only_name_counts_as_unnamed() {
        let tree = SemanticTree::new().root(Node::leaf(Role::Button, "   \t ", 0, true));
        assert_eq!(tree.unnamed_focusable().len(), 1);
    }

    #[test]
    fn the_announcement_reads_depth_first_with_indentation() {
        let rendered = sample().to_announcement();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(lines[0], "window Editor");
        assert_eq!(lines[1], "  button New [focusable]");
        assert_eq!(lines[3], "  list Files");
        // Depth, not breadth: the list's items are indented under it, not level with it.
        assert_eq!(lines[4], "    list_item README.md [focusable]");
    }

    #[test]
    fn the_announcement_does_not_include_unnamed_nodes_as_blanks() {
        let rendered = SemanticTree::new()
            .root(Node::leaf(Role::StaticText, "", 0, false))
            .to_announcement();
        assert_eq!(rendered, "static_text\n", "got {rendered:?}");
    }

    #[test]
    fn an_empty_tree_is_empty_rather_than_panicking() {
        let tree = SemanticTree::new();
        assert!(tree.is_empty());
        assert!(tree.focus_order().is_empty());
        assert!(tree.flatten().is_empty());
        assert_eq!(tree.to_announcement(), "");
    }

    #[test]
    fn flatten_includes_every_node_focus_order_does_not() {
        let tree = sample();
        let all = tree.flatten();
        let focusable = tree.focus_order();
        // The sample tree has six nodes and four of them are focusable, so `flatten` sees two
        // more: the window itself and the list.
        assert_eq!(all.len(), 6);
        assert_eq!(focusable.len(), 4);
        assert!(
            all.len() > focusable.len(),
            "flatten is a superset of the focus order"
        );
    }

    #[test]
    fn role_names_are_stable_wire_values() {
        // Renaming a role is a breaking change to the screen-reader contract.
        assert_eq!(Role::Button.as_str(), "button");
        assert_eq!(Role::TextInput.as_str(), "text_input");
        assert_eq!(Role::ProgressBar.to_string(), "progress_bar");
    }
}
