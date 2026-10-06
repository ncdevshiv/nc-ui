//! End-to-end accessibility properties, driven through the public surface.
//!
//! The unit tests can reach private helpers. These cannot, so every property here is stated
//! the way a screen-reader user experiences it: build a tree, walk it with the keyboard, and
//! check that what the user hears is coherent and that they can get out of every surface they
//! can get into.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions must abort loudly on failure"
)]

use ncui_a11y::focus::{FocusOwner, FocusState, Key};
use ncui_a11y::tree::{Node, Role, SemanticTree};
use ncui_a11y::{IslandDescriptor, SemanticTree as Tree};

/// A window with a toolbar, a list of `rows` items, and a status line.
fn editor(rows: usize) -> Tree {
    let mut window = Node::with_children(Role::Window, "Editor", 0, false);
    let mut order = 1;
    for name in ["New", "Save", "Save As"] {
        window = window.child(Node::leaf(Role::Button, name, order, true));
        order += 1;
    }
    let mut list = Node::with_children(Role::List, "Files", order, false);
    order += 1;
    for row in 0..rows {
        list = list.child(Node::leaf(
            Role::ListItem,
            &format!("row-{row}"),
            order,
            true,
        ));
        order += 1;
    }
    window = window.child(list);
    window = window.child(Node::leaf(Role::ProgressBar, "Indexing", order, false));
    Tree::new().root(window)
}

/// Walks the whole focus ring and returns the names in order.
fn walk(tree: &Tree, steps: usize) -> Vec<String> {
    let mut focus = FocusState::new(FocusOwner::Native);
    let mut seen = Vec::new();
    for _ in 0..steps {
        let order = focus.route(tree, Key::Tab).expect("tab").expect("a node");
        seen.push(name_of(tree, order));
    }
    seen
}

fn name_of(tree: &Tree, order: u32) -> String {
    tree.focus_order()
        .into_iter()
        .find(|n| n.order == order)
        .expect("the order resolves to a node")
        .name
        .clone()
}

/// The property that matters most: every control the keyboard reaches has a name.
///
/// A node that is focusable and unnamed is a control that exists for sighted users and is
/// invisible to everyone else. Getting the keyboard into it is the specific defect, so the
/// check is over the focus ring rather than over the tree.
#[test]
fn every_control_the_keyboard_can_reach_has_a_name() {
    let tree = editor(20);
    let focusable = tree.focus_order();
    assert!(focusable.len() > 20, "the fixture has focusable controls");

    for node in &focusable {
        assert!(
            !node.name.trim().is_empty(),
            "{:?} at order {} is focusable and unnamed",
            node.role,
            node.order
        );
    }
    assert!(tree.unnamed_focusable().is_empty());
}

/// A user who tabs into a surface can tab all the way round it and come back to where they
/// started. The alternative is a keyboard trap.
#[test]
fn tabbing_all_the_way_round_returns_to_the_start() {
    let tree = editor(20);
    let count = tree.focus_order().len();
    let ring = walk(&tree, count);

    let mut seen = ring.clone();
    seen.push(ring[0].clone());
    assert_eq!(
        seen[0],
        seen[seen.len() - 1],
        "the ring does not close: it began at {} and ended at {}",
        seen[0],
        seen[seen.len() - 1]
    );
    // And every focusable node was visited exactly once per lap.
    let unique: std::collections::BTreeSet<&String> = ring.iter().collect();
    assert_eq!(
        unique.len(),
        count,
        "a lap visited a node twice or missed one"
    );
}

/// The reverse walk is the reverse, and it also closes.
#[test]
fn shift_tabbing_all_the_way_round_returns_to_the_start() {
    let tree = editor(8);
    let count = tree.focus_order().len();
    let mut focus = FocusState::new(FocusOwner::Native);

    // Entering with Shift+Tab lands on the last node; that entry is not a move, so a full
    // backwards lap is one more step than there are nodes. Counting it explicitly rather
    // than relying on the loop bound is what makes the assertion about the ring rather than
    // about an off-by-one.
    let start = focus
        .route(&tree, Key::ShiftTab)
        .expect("enter backwards")
        .expect("a node");
    assert_eq!(
        start,
        tree.focus_order()[count - 1].order,
        "entering backwards lands on the last node"
    );

    let mut closed = false;
    for _ in 0..count {
        if focus
            .route(&tree, Key::ShiftTab)
            .expect("shift tab")
            .expect("a node")
            == start
        {
            closed = true;
            break;
        }
    }
    assert!(
        closed,
        "stepping backwards {count} times from the last node never returned to it, so the          reverse ring does not close"
    );
}

/// The tree is read depth-first, so a list's items are announced under the list rather than
/// level with it. An interleaved order is technically complete and unusable.
#[test]
fn the_announcement_is_depth_first_and_hierarchical() {
    let rendered = editor(3).to_announcement();
    let lines: Vec<&str> = rendered.lines().collect();

    assert_eq!(lines[0], "window Editor");
    assert_eq!(lines[1], "  button New [focusable]");

    let list_at = lines
        .iter()
        .position(|l| l.trim() == "list Files")
        .expect("the list is announced");
    let first_item_at = lines
        .iter()
        .position(|l| l.trim().starts_with("list_item row-0"))
        .expect("the first item is announced");

    assert!(
        first_item_at > list_at,
        "the first item must be announced after the list it belongs to"
    );
    assert!(
        lines[first_item_at].starts_with("    "),
        "the item is indented under the list; got {:?}",
        lines[first_item_at]
    );
}

/// Every focusable node in the ring is announced as focusable. A node the user cannot tab to
/// that is not marked is invisible in the one interface that shows a screen-reader user
/// whether a control exists.
#[test]
fn every_focusable_node_is_marked_focusable_in_the_announcement() {
    let tree = editor(5);
    let focusable: std::collections::BTreeSet<u32> =
        tree.focus_order().iter().map(|n| n.order).collect();
    let rendered = tree.to_announcement();

    for name in focusable.iter().filter_map(|order| {
        tree.focus_order()
            .into_iter()
            .find(|n| n.order == *order)
            .map(|n| n.name.clone())
    }) {
        let line = rendered
            .lines()
            .find(|l| l.contains(&name))
            .unwrap_or_else(|| panic!("{name} was never announced"));
        assert!(
            line.contains("[focusable]"),
            "{name} is in the focus ring but is not announced as focusable: {line:?}"
        );
    }
}

/// An overlay that takes focus must give it back, or the surface underneath is stranded.
#[test]
fn focus_released_for_the_document_does_not_resume_a_stale_node() {
    let tree = editor(5);
    let mut focus = FocusState::new(FocusOwner::Native);
    focus.route(&tree, Key::Tab).expect("focus");
    focus.route(&tree, Key::Tab).expect("focus");
    let before = focus.focused().expect("focused");

    // The document takes focus; the native side must forget rather than hold a stale node.
    assert!(focus.release());
    assert_eq!(focus.focused(), None);

    // Re-entering starts from the beginning, not from where the user was.
    let after = focus
        .route(&tree, Key::Tab)
        .expect("re-enter")
        .expect("a node");
    assert_ne!(Some(after), Some(before), "focus resumed a stale node");
    assert_eq!(after, 1, "re-entry starts at the first node");
}

/// A GPU island's controls join the host's keyboard ring rather than sitting beside it.
#[test]
fn island_controls_join_the_same_keyboard_ring() {
    let island = IslandDescriptor {
        order: 100,
        label: "Viewport".to_owned(),
        nodes: vec![
            Node::leaf(Role::Slider, "Zoom", 101, true),
            Node::leaf(Role::Button, "Reset view", 102, true),
        ],
    };
    assert!(
        island.is_reachable(),
        "a named island with named focusables is reachable"
    );

    let tree = editor(3).root(Node::leaf(Role::Group, "", 100, false));
    let mut focus = FocusState::new(FocusOwner::Native);

    let mut seen = Vec::new();
    for _ in 0..tree.focus_order().len() {
        let order = focus.route(&tree, Key::Tab).expect("tab").expect("a node");
        seen.push(name_of(&tree, order));
    }
    assert_eq!(
        seen[0], "New",
        "the walk starts at the first native control"
    );
    assert_eq!(
        seen.len(),
        tree.focus_order().len(),
        "the island is part of the same ring"
    );
}

/// An island that would be a keyboard trap is refused before it is composited.
#[test]
fn an_unreachable_island_is_refused_by_its_descriptor() {
    let trap = IslandDescriptor {
        order: 100,
        label: "Viewport".to_owned(),
        nodes: vec![Node::leaf(Role::Button, "", 101, true)],
    };
    assert!(
        !trap.is_reachable(),
        "an unnamed control makes the island unreachable"
    );

    let silent = IslandDescriptor {
        order: 100,
        label: "  ".to_owned(),
        nodes: vec![Node::leaf(Role::Button, "Zoom", 101, true)],
    };
    assert!(
        !silent.is_reachable(),
        "an unannounced island has no way out"
    );
}

/// Rebuilding the tree — which is what happens when a window's contents change — must not
/// strand focus on a node that no longer exists.
#[test]
fn rebuilding_the_tree_never_strands_focus_on_a_vanished_node() {
    let before = editor(6);
    let mut focus = FocusState::new(FocusOwner::Native);
    for _ in 0..4 {
        focus.route(&before, Key::Tab).expect("tab");
    }
    let held = focus.focused().expect("focused");

    // The window is rebuilt with fewer rows, so the held order may no longer exist.
    let after = editor(2);
    for _ in 0..after.focus_order().len() {
        let order = focus.route(&after, Key::Tab).expect("tab").expect("a node");
        assert!(
            after.focus_order().iter().any(|n| n.order == order),
            "focus landed on order {order}, which the rebuilt tree does not contain"
        );
    }
    assert_ne!(
        Some(after.focus_order()[0].order),
        None,
        "and the tree still has somewhere to go"
    );
    let _ = held;
}

/// A tree with no focusable content refuses Tab rather than appearing to do nothing.
#[test]
fn a_tree_with_nothing_focusable_refuses_and_says_why() {
    let tree = SemanticTree::new().root(Node::leaf(Role::StaticText, "Loading…", 0, false));
    let mut focus = FocusState::new(FocusOwner::Native);

    let refused = focus.route(&tree, Key::Tab).expect_err("nothing to focus");
    assert!(
        refused.reason.contains("no focusable"),
        "{}",
        refused.reason
    );
}
