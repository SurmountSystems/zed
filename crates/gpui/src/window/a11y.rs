//! Accessibility support, provided by [AccessKit][accesskit].
//!
//! There are user-facing guide-level docs [here](crate::_accessibility).
//!
//! ## Architecture
//!
//! ```text
//!                              ┌────────────────────────────────┐   ┌─────────────────────┐
//!                           ┌─▶│ AccessKit Adapter (MacOS)      │◀─▶│ MacOS System APIs   │
//!                           │  └────────────────────────────────┘   └─────────────────────┘
//!                           │
//! ┌──────┐   ┌───────────┐  │  ┌────────────────────────────────┐   ┌─────────────────────┐
//! │ GPUI │◀─▶│ AccessKit │◀─┼─▶│ AccessKit Adapter (Windows)    │◀─▶│ Windows System APIs │
//! └──────┘   └───────────┘  │  └────────────────────────────────┘   └─────────────────────┘
//!                           │
//!                           │  ┌────────────────────────────────┐   ┌─────────────────────┐
//!                           └─▶│ AccessKit Adapter (Linux)      │◀─▶│ dbus                │
//!                              └────────────────────────────────┘   └─────────────────────┘
//! ```
//!
//! In order for GPUI apps to be usable for people using assistive technology,
//! we must do a few things:
//! - Inform the system when the UI changes meaningfully. This includes:
//!   - Reporting new/removed/changed UI elements
//!   - *Not* reporting irrelevant UI changes, e.g. an invisible `div()` being
//!     added.
//!   - Reporting the appearance and capabilities of each UI element. For example:
//!     - What does this piece of text say?
//!     - How far along is this progress bar?
//!     - Can this node be focused?
//!     - Can this node have a value directly assigned? (e.g. a slider)
//! - Allowing the system to interact with the UI by dispatching actions to
//!   nodes. Note that AccessKit has its own [`Action`] type, which is not the
//!   [`crate::Action`] trait.
//! - Activate and deactivate accessibility features when requested by the
//!   system.
//!
//! Activating and deactivating at the right time is trivial, so I won't go into
//! detail here. The other two are almost orthogonal in implementation.
//!
//! The state for both lives in the [`A11y`] struct in this module.
//!
//! ### Reporting UI changes
//!
//! Every frame, we build a [`TreeUpdate`] and send it to the platform-specific
//! adapter. A [`TreeUpdate`] is a representation of a subset of the UI tree.
//! When the adapter receives the update, it diffs it against the previous
//! update, and calls platform-specific APIs to inform screen readers about the
//! changes. Nodes may have been created, destroyed, or updated.
//!
//! Each node has an ID, and this ID *should* be stable across frames. If a
//! node's ID changes, then, from AccessKit's point of view, it is a different
//! node.
//!
//! We derive the node ID from the [`GlobalElementId`] in
//! [`GlobalElementId::accesskit_node_id`]. Nodes without [`GlobalElementId`]s
//! cannot produce an AccessKit [`NodeId`], and so are not included in the
//! accessibility tree. We try to warn when using accessibility APIs on
//! [`div()`] without setting an ID.
//!
//! This all happens in [`Drawable::prepaint`]. The [`A11y`] struct maintains a
//! stack of nodes during prepainting, which we can use to calculate the
//! [`NodeId`]s, and record parent-child relationships. Once all [`Element`]s in
//! a frame have been prepainted, we send the resulting [`TreeUpdate`] object to
//! the adapter and the screen reader can announce the changes.
//!
//! #### Synthetic children
//!
//! Additionally, some nodes can register "synthetic children" using
//! [`Element::a11y_synthetic_children`]. Normally, one accesskit node is pushed
//! for every [`Element`] with a role and id. However, sometimes a single
//! element may want to produce many accesskit nodes. These extra nodes are
//! referred to as "synthetic children" of the element providing a non-default
//! [`Element::a11y_synthetic_children`] implementation.
//!
//! The user is provided a builder-style API using [`A11ySubtreeBuilder`], which
//! allows them to create push nodes that are children of the current node, as
//! well as modify the current node itself.
//!
//! GPUI calls this callback *after* prepainting (and just before popping the
//! corresponding element), since this step may need prepaint information to be
//! available. In the future, we may want to add prepaint information more
//! generally to [`Element::write_a11y_info`], but for now that's not necessary.
//!
//! ### Responding to actions
//!
//! On adapter creation, we provide a callback to the adapter, which can be used
//! to dispatch actions. This callback forwards to [`A11y::action_listeners`], a
//! mapping from [`NodeId`]s to action handlers (basically just `Box<dyn
//! Fn()>`).
//!
//! This is populated in:
//! - [`Window::on_a11y_action`], which is called by:
//! - [`Interactivity::paint`], which is called by:
//! - [`StatefulInteractiveElement::on_a11y_action`], which is a public-facing API
//!
//! These are cleared at the start of a frame, and re-populated during painting.
//!
//! [`NodeId`]: accesskit::NodeId

use crate::*;

use crate::{App, Bounds, FocusId, Pixels, SharedString, Window};
use accesskit::{Action, NodeId, TreeUpdate};
use collections::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use std::hash::{Hash, Hasher};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// The fixed AccessKit node ID used for the root of every window's a11y tree.
pub(crate) const ROOT_NODE_ID: NodeId = NodeId(0);

/// A listener for an accessibility action on a specific node.
pub(crate) type A11yActionListener =
    Box<dyn FnMut(Option<&accesskit::ActionData>, &mut Window, &mut App) + 'static>;

/// Per-window accessibility state.
///
/// Manages the AccessKit tree that is built each frame and the mappings
/// needed to dispatch incoming action requests back to the right elements.
pub(crate) struct A11y {
    /// Whether accessibility has been [forcibly disabled] for this window.
    ///
    /// [forcibly disabled]: crate::Application::new_inaccessible
    force_disabled: bool,
    /// Whether a11y features have been requested by the system.
    ///
    /// Updated by AccessKit using callbacks provided to the adapter. Can change
    /// halfway through a frame.
    active_flag: Arc<AtomicBool>,
    /// Whether a11y features are active for *this specific frame*.
    ///
    /// At the start of each frame, we load [`Self::active_flag`] (using
    /// [`Self::sync_active_flag`]) and use this to determine whether we
    /// should construct a [`TreeUpdate`] for this frame. It's important that
    /// this value is stable within a frame, because the builder API exposed by
    /// this type maintains a stack of nodes and each must be pushed and popped
    /// exactly once.
    ///
    /// At the end of the frame, we re-call [`Self::sync_active_flag`] to
    /// determine whether we should actually send the finished [`TreeUpdate`].
    active_this_frame: bool,
    pub(crate) nodes: A11yNodeBuilder,
    pub(crate) focus_ids: FxHashMap<NodeId, FocusId>,
    pub(crate) node_bounds: FxHashMap<NodeId, Bounds<Pixels>>,
    pub(crate) action_listeners: FxHashMap<NodeId, Vec<(Action, A11yActionListener)>>,
    /// The window's title, used to label the root node so assistive
    /// technology can tell windows apart.
    window_title: Option<SharedString>,
}

impl A11y {
    pub(crate) fn new(
        active_flag: Arc<AtomicBool>,
        force_disabled: bool,
        window_title: Option<SharedString>,
    ) -> Self {
        Self {
            force_disabled,
            active_flag,
            active_this_frame: false,
            nodes: A11yNodeBuilder::new(),
            focus_ids: FxHashMap::default(),
            node_bounds: FxHashMap::default(),
            action_listeners: FxHashMap::default(),
            window_title,
        }
    }

    pub(crate) fn set_window_title(&mut self, title: impl Into<SharedString>) {
        self.window_title = Some(title.into());
    }

    /// Ensures that [`Self::is_active`] returns up to date information.
    ///
    /// See the docs for [`Self::active_flag`] and [`Self::active_this_frame`]
    /// for more commentary.
    pub(crate) fn sync_active_flag(&mut self) {
        let active = !self.force_disabled && self.active_flag.load(Ordering::SeqCst);
        // Drop stale post-frame outline when AT / experimental activation ends.
        if self.active_this_frame && !active {
            self.nodes.clear_last_interactive_outline();
        }
        self.active_this_frame = active;
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active_this_frame
    }

    pub(crate) fn set_focusable(&mut self, node_id: NodeId, focus_id: FocusId) {
        self.focus_ids.insert(node_id, focus_id);
    }

    /// Report `node_id` as the currently-focused node, if it is present in the
    /// tree.
    ///
    /// Must only be called once per frame.
    pub(crate) fn set_focus(&mut self, node_id: NodeId) {
        // A focused node must have been registered as focusable this frame.
        if !self.focus_ids.contains_key(&node_id) {
            if cfg!(debug_assertions) {
                panic!("set_focus called for a node that was not registered with set_focusable");
            } else {
                log::warn!(
                    "a11y: set_focus called for a node that was not registered with \
                     set_focusable ({node_id:?})"
                );
            }
        }
        if self.nodes.has_node(node_id) {
            self.nodes.set_focus(node_id);
        }
    }

    pub(crate) fn set_active_descendant(&mut self, node_id: NodeId) {
        // The active descendant must be a descendant of the focused container,
        // not the focused node itself.
        if self.nodes.node_is_focused(node_id) {
            if cfg!(debug_assertions) {
                panic!("set_active_descendant called on the focused node");
            } else {
                log::warn!("a11y: set_active_descendant called on the focused node ({node_id:?})");
            }
            return;
        }
        if self.nodes.has_node(node_id) && self.nodes.focus_is_ancestor_of_current() {
            self.nodes.set_active_descendant(node_id);
        }
    }

    /// Clear per-frame state and push the root node to start a new frame.
    pub(crate) fn begin_frame(&mut self) {
        self.focus_ids.clear();
        self.node_bounds.clear();
        self.action_listeners.clear();
        self.nodes.begin_frame(self.window_title.as_ref());
    }

    /// Finalize the tree and produce a [`TreeUpdate`] for the platform adapter.
    pub(crate) fn end_frame(&mut self) -> TreeUpdate {
        self.nodes.finalize()
    }
}

/// Builder API for synthetic children. See the docs for
/// [`Element::a11y_synthetic_children`].
pub struct A11ySubtreeBuilder<'a> {
    parent_id: NodeId,
    nodes: &'a mut A11yNodeBuilder,
}

impl<'a> A11ySubtreeBuilder<'a> {
    pub(crate) fn new(parent_id: NodeId, nodes: &'a mut A11yNodeBuilder) -> Self {
        Self { parent_id, nodes }
    }

    /// Derive a [`NodeId`] for a synthetic child.
    ///
    /// The generated ID is based on the hash of `key`, as well as the parent's
    /// ID. This means that `key`s must be unique within the same
    /// [`Element::a11y_synthetic_children`] call, but may be duplicated across
    /// different calls.
    pub fn synthetic_node_id(&self, key: impl Hash) -> NodeId {
        let mut hasher = std::hash::DefaultHasher::default();
        self.parent_id.0.hash(&mut hasher);
        key.hash(&mut hasher);
        NodeId(hasher.finish())
    }

    /// Append a synthetic leaf node as a child of this element's node.
    ///
    /// Returns `false` if a node with this id is already present in the tree,
    /// in which case the node is discarded.
    pub fn push_child(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        self.nodes.push_leaf(id, node)
    }

    /// A mutable reference to the parent node.
    pub fn parent_node(&mut self) -> &mut accesskit::Node {
        self.nodes
            .current_node_mut()
            .expect("A11ySubtreeBuilder exists only while its element's node is on the stack")
    }
}

pub(crate) struct A11yNodeBuilder {
    ids_stack: SmallVec<[NodeId; 16]>,
    nodes_stack: SmallVec<[accesskit::Node; 16]>,
    /// This is the exact type required by accesskit, so we can't just make it a
    /// `HashMap<NodeId, Node>` to remove the need for `seen_ids`
    all_nodes: Vec<(NodeId, accesskit::Node)>,
    /// Interactive outline from the last `finalize` (rich detail by default).
    /// Prefer this over retaining the full AccessKit node list.
    last_interactive_outline: String,
    /// Compact interactive outline (role/label/value/id, optional focus `*`).
    last_compact_outline: String,
    /// Room narrative: header + landmarks + rich interactive lines.
    last_room_outline: String,
    seen_ids: FxHashSet<NodeId>,
    /// The node that GPUI considers focused. Note that this may be different to
    /// what is reported to accesskit - see [`Self::active_descendant`]
    pub(crate) focus: Option<NodeId>,
    /// If a node calls `.aria_active_descendant()`, AND an ancestor is focused,
    /// override it as the focused node. This supports the "active descendant"
    /// pattern, which allows a focused container to act as if a descendant is
    /// focused.
    pub(crate) active_descendant: Option<NodeId>,
}

impl A11yNodeBuilder {
    fn new() -> Self {
        Self {
            ids_stack: SmallVec::new(),
            nodes_stack: SmallVec::new(),
            all_nodes: Vec::new(),
            last_interactive_outline: String::new(),
            last_compact_outline: String::new(),
            last_room_outline: String::new(),
            seen_ids: FxHashSet::default(),
            focus: None,
            active_descendant: None,
        }
    }

    pub(crate) fn clear_last_interactive_outline(&mut self) {
        self.last_interactive_outline.clear();
        self.last_compact_outline.clear();
        self.last_room_outline.clear();
    }

    pub(crate) fn last_interactive_outline(&self) -> &str {
        &self.last_interactive_outline
    }

    pub(crate) fn last_outline(&self, detail: OutlineDetail) -> &str {
        match detail {
            OutlineDetail::Compact => &self.last_compact_outline,
            OutlineDetail::Rich => self.last_interactive_outline(),
            OutlineDetail::Room => &self.last_room_outline,
        }
    }

    /// True while a frame is being built (root still on the stack or leaves pending).
    pub(crate) fn has_in_progress_frame(&self) -> bool {
        !self.ids_stack.is_empty() || !self.all_nodes.is_empty()
    }

    #[must_use]
    fn can_push(&mut self, id: NodeId) -> bool {
        debug_assert!(!self.ids_stack.is_empty(), "node pushed before push_root");

        if !self.seen_ids.insert(id) {
            debug_assert!(
                false,
                "Duplicate a11y node id: {id:?}. In a release build, this node would be silently discarded from the a11y tree."
            );
            return false;
        }

        true
    }

    /// Push a new node onto the stack. It becomes a child of the current
    /// top-of-stack node.
    ///
    /// Returns `true` if the node was successfully pushed.
    pub(crate) fn push(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        if !self.can_push(id) {
            return false;
        }

        if let Some(parent) = self.nodes_stack.last_mut() {
            parent.push_child(id);
        }
        self.ids_stack.push(id);
        self.nodes_stack.push(node);
        true
    }

    /// Add a leaf node as a child of the current top-of-stack node, without
    /// pushing it onto the stack. Semantically equivalent to a [`Self::push`]
    /// followed by a [`Self::pop`].
    ///
    /// Returns `true` if the node was successfully pushed.
    pub(crate) fn push_leaf(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        if !self.can_push(id) {
            return false;
        }

        if let Some(parent) = self.nodes_stack.last_mut() {
            parent.push_child(id);
        }
        self.all_nodes.push((id, node));
        true
    }

    pub(crate) fn current_node_mut(&mut self) -> Option<&mut accesskit::Node> {
        self.nodes_stack.last_mut()
    }

    /// Pop the current node off the stack and finalize it into the all_nodes
    /// list.
    pub(crate) fn pop(&mut self) {
        debug_assert!(self.ids_stack.len() > 1, "pop would remove the root node");

        if let (Some(id), Some(node)) = (self.ids_stack.pop(), self.nodes_stack.pop()) {
            self.all_nodes.push((id, node));
        }
    }

    /// Push the root node to start a new frame.
    fn begin_frame(&mut self, window_title: Option<&SharedString>) {
        self.all_nodes.clear();
        self.ids_stack.clear();
        self.nodes_stack.clear();
        self.seen_ids.clear();
        let mut root_node = accesskit::Node::new(accesskit::Role::Window);
        if let Some(title) = window_title {
            root_node.set_label(title.to_string());
        }

        self.ids_stack.push(ROOT_NODE_ID);
        self.nodes_stack.push(root_node);
        self.focus = None;
        self.active_descendant = None;
    }

    /// Returns whether a node with the given ID has been pushed in this frame.
    pub(crate) fn has_node(&self, id: NodeId) -> bool {
        id == ROOT_NODE_ID || self.seen_ids.contains(&id)
    }

    /// Returns whether `id` is the node currently reported as focused.
    pub(crate) fn node_is_focused(&self, id: NodeId) -> bool {
        self.focus == Some(id)
    }

    pub(crate) fn focus_is_ancestor_of_current(&self) -> bool {
        let Some(focus) = self.focus else {
            return false;
        };

        // The current node is on top of the stack; everything below it is an
        // ancestor.
        let ancestor_count = self.ids_stack.len().saturating_sub(1);
        self.ids_stack[..ancestor_count].contains(&focus)
    }

    pub(crate) fn set_active_descendant(&mut self, id: NodeId) {
        if self
            .active_descendant
            .is_some_and(|existing| existing != id)
        {
            if cfg!(debug_assertions) {
                panic!("active descendant claimed by multiple nodes in one frame");
            } else {
                log::warn!(
                    "a11y: multiple nodes claimed the active descendant this frame; \
                     using last-wins ({id:?})"
                );
            }
        }
        self.active_descendant = Some(id);
    }

    pub(crate) fn set_focus(&mut self, id: NodeId) {
        if self.focus.is_some() {
            if cfg!(debug_assertions) {
                panic!("set_focus called more than once in a single frame");
            } else {
                log::warn!(
                    "a11y: set_focus called more than once in a single frame; \
                     using last-wins ({id:?})"
                );
            }
        }
        self.focus = Some(id);
    }

    fn finalize(&mut self) -> TreeUpdate {
        // Stack should contain only the root node
        debug_assert_eq!(self.ids_stack.len(), 1);
        debug_assert_eq!(self.ids_stack[0], ROOT_NODE_ID);

        if self.ids_stack.len() != 1 {
            log::error!(
                "a11y: Stack imbalance at end of frame: expected 1 (root), got {}. \
                 Some elements may have pushed without popping.",
                self.ids_stack.len()
            );
        }

        // Pop remaining nodes (should just be the root).
        while !self.ids_stack.is_empty() {
            if let (Some(id), Some(node)) = (self.ids_stack.pop(), self.nodes_stack.pop()) {
                self.all_nodes.push((id, node));
            }
        }

        let focus = match self.active_descendant {
            Some(id) if self.has_node(id) => id,
            Some(id) => {
                if cfg!(debug_assertions) {
                    panic!("active_descendant set to {id:?}, which is not in the tree");
                } else {
                    log::warn!("active_descendant set to {id:?}, which is not in the tree");
                    self.focus.unwrap_or(ROOT_NODE_ID)
                }
            }

            _ => self.focus.unwrap_or(ROOT_NODE_ID),
        };

        let nodes = std::mem::take(&mut self.all_nodes);
        // Store outline strings only (not a full-tree clone) for post-frame
        // dogfood snapshots after `mem::take` moves nodes into TreeUpdate.
        // One tree walk builds compact/rich/room so finalize is not 3× O(n).
        let tiers = format_a11y_outline_all_tiers(&nodes, Some(focus));
        self.last_compact_outline = tiers.compact;
        self.last_interactive_outline = tiers.rich;
        self.last_room_outline = tiers.room;
        let update = TreeUpdate {
            nodes,
            tree: Some(accesskit::Tree::new(ROOT_NODE_ID)),
            tree_id: accesskit::TreeId::ROOT,
            focus,
        };

        Self::repair_tree_update(update)
    }

    /// Accesskit panics on invalid [`TreeUpdate`]s. This function defensively
    /// checks invariants that accesskit panics on, and tries to fix them.
    fn repair_tree_update(mut update: TreeUpdate) -> TreeUpdate {
        let node_ids: FxHashSet<NodeId> = update.nodes.iter().map(|(id, _)| *id).collect();

        // Focus must point to a node in the tree.
        if !node_ids.contains(&update.focus) {
            log::error!(
                "a11y: Focused node {:?} is not in the tree ({} nodes). \
                 Falling back to root. This is a bug in the a11y tree builder.",
                update.focus,
                update.nodes.len()
            );
            update.focus = ROOT_NODE_ID;
        }

        // Every child reference must point to a node in the update.
        for (id, node) in &mut update.nodes {
            let has_invalid_child = node
                .children()
                .iter()
                .any(|child_id| !node_ids.contains(child_id));
            if has_invalid_child {
                let children = node.children();
                let invalid_count = children
                    .iter()
                    .filter(|child_id| !node_ids.contains(child_id))
                    .count();
                log::error!(
                    "a11y: Node {:?} references {} children not present in the tree. \
                     Stripping invalid child references.",
                    id,
                    invalid_count
                );
                let valid: Vec<NodeId> = children
                    .iter()
                    .copied()
                    .filter(|child_id| node_ids.contains(child_id))
                    .collect();
                node.set_children(valid);
            }
        }

        update
    }

    /// Collects nodes for mid-frame outline (stack + finalized leaves this frame).
    /// After `finalize`, returns empty — use [`Self::last_interactive_outline`].
    pub(crate) fn collect_snapshot_nodes(&self) -> Vec<(NodeId, accesskit::Node)> {
        let mut nodes = self.all_nodes.clone();
        for (index, &id) in self.ids_stack.iter().enumerate() {
            if let Some(node) = self.nodes_stack.get(index)
                && !nodes.iter().any(|(node_id, _)| *node_id == id)
            {
                nodes.push((id, node.clone()));
            }
        }
        nodes
    }
}

/// How much tactile detail to put in an a11y outline for dogfood / agent-stdio.
///
/// Additive **fields**: compact ⊆ rich interactive fields; room = rich interactive
/// lines + landmarks + room header. Focus `*` placement may differ when focus is a
/// room-only landmark (room stars the landmark; rich/compact star the interactive
/// ancestor). `compact` is a lean path for smoke/tokens — not required bit-identical
/// to pre-enrichment lines. Bounds on rich/room lines are **scaled/physical px**
/// (layout × window scale factor at prepaint); not CSS logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutlineDetail {
    /// Role, label, value, id; focus marked with `*`.
    Compact,
    /// Interactive nodes with bounds, states, descriptions, and actions.
    #[default]
    Rich,
    /// Room narrative: window/focus header, landmarks, then rich interactive lines.
    Room,
}

/// Options for [`format_a11y_outline`].
#[derive(Clone, Copy, Debug)]
pub struct OutlineOptions {
    /// AccessKit focus (or active descendant) for this frame.
    pub focus: Option<NodeId>,
    /// How much tactile detail to emit (compact / rich / room).
    pub detail: OutlineDetail,
}

impl Default for OutlineOptions {
    fn default() -> Self {
        Self {
            focus: None,
            detail: OutlineDetail::Rich,
        }
    }
}

/// Returns whether an AccessKit node should appear in agent-stdio UI snapshots.
pub fn is_interactive_a11y_node(node: &accesskit::Node) -> bool {
    use accesskit::Action;

    if node.is_hidden() {
        return false;
    }

    if [
        Action::Click,
        Action::Focus,
        Action::SetValue,
        Action::Expand,
        Action::Collapse,
        Action::ScrollIntoView,
    ]
    .into_iter()
    .any(|action| node.supports_action(action))
    {
        return true;
    }

    if !node.custom_actions().is_empty() {
        return true;
    }

    matches!(
        node.role(),
        accesskit::Role::Button
            | accesskit::Role::DefaultButton
            | accesskit::Role::Link
            | accesskit::Role::TextInput
            | accesskit::Role::MultilineTextInput
            | accesskit::Role::CheckBox
            | accesskit::Role::RadioButton
            | accesskit::Role::ComboBox
            | accesskit::Role::MenuItem
            | accesskit::Role::Tab
            | accesskit::Role::Switch
            | accesskit::Role::ListBoxOption
            | accesskit::Role::SearchInput
    )
}

/// Spatial / structural landmarks for `OutlineDetail::Room` (not pure chrome).
///
/// Structural roles (Heading, Dialog, AlertDialog, Toolbar, MenuBar, Menu, TabList)
/// count even when unlabeled — plan R2 treats them as spatial anchors. Label/List/
/// ListItem require non-empty label or value. If live `detail:room` looks fill with
/// unlabeled Toolbar/Menu chrome, tighten those roles to require text before widening
/// the always-on set again.
pub fn is_landmark_a11y_node(node: &accesskit::Node) -> bool {
    if node.is_hidden() || is_interactive_a11y_node(node) {
        return false;
    }

    match node.role() {
        accesskit::Role::Heading
        | accesskit::Role::Dialog
        | accesskit::Role::AlertDialog
        | accesskit::Role::Toolbar
        | accesskit::Role::MenuBar
        | accesskit::Role::Menu
        | accesskit::Role::TabList => true,
        accesskit::Role::Label | accesskit::Role::List | accesskit::Role::ListItem => {
            let label = node.label().unwrap_or_default();
            let value = node.value().unwrap_or_default();
            !label.is_empty() || !value.is_empty()
        }
        _ => false,
    }
}

const OUTLINE_STRING_MAX: usize = 80;

fn truncate_outline_str(s: &str) -> String {
    if s.chars().count() <= OUTLINE_STRING_MAX {
        return s.to_string();
    }
    let mut out: String = s
        .chars()
        .take(OUTLINE_STRING_MAX.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

fn format_node_outline_line(
    id: NodeId,
    node: &accesskit::Node,
    depth: usize,
    focused: bool,
    detail: OutlineDetail,
) -> String {
    use accesskit::Action;
    use std::fmt::Write as _;

    let indent = "  ".repeat(depth);
    let role = format!("{:?}", node.role());
    let label = node.label().unwrap_or_default();
    let value = node.value().unwrap_or_default();
    let mut line = String::new();
    // Indent first so nested focus is `  *[Button]`, not `*  [Button]`.
    let _ = write!(line, "{indent}");
    if focused {
        line.push('*');
    }
    let _ = write!(line, "[{role}]");
    if !label.is_empty() {
        let _ = write!(line, " \"{}\"", truncate_outline_str(&label));
    }
    if !value.is_empty() {
        let _ = write!(line, " value=\"{}\"", truncate_outline_str(&value));
    }

    // Bounds are AccessKit rects written at prepaint as layout × scale_factor
    // (scaled/physical px). Compact omits them to keep the lean token path.
    if detail != OutlineDetail::Compact {
        if let Some(bounds) = node.bounds() {
            let x = bounds.x0.round() as i64;
            let y = bounds.y0.round() as i64;
            let w = (bounds.x1 - bounds.x0).round().max(0.0) as i64;
            let h = (bounds.y1 - bounds.y0).round().max(0.0) as i64;
            let _ = write!(line, " @{x},{y} {w}x{h}");
        }
        if node.is_disabled() {
            line.push_str(" [disabled]");
        }
        if let Some(true) = node.is_selected() {
            line.push_str(" [selected]");
        }
        if let Some(expanded) = node.is_expanded() {
            if expanded {
                line.push_str(" [expanded]");
            } else {
                line.push_str(" [collapsed]");
            }
        }
        if let Some(toggled) = node.toggled() {
            let toggled = match toggled {
                accesskit::Toggled::True => "true",
                accesskit::Toggled::False => "false",
                accesskit::Toggled::Mixed => "mixed",
            };
            let _ = write!(line, " [toggled={toggled}]");
        }
        if let Some(description) = node.description() {
            if !description.is_empty() {
                let _ = write!(line, " desc=\"{}\"", truncate_outline_str(&description));
            }
        }
        if let Some(placeholder) = node.placeholder() {
            if !placeholder.is_empty() {
                let _ = write!(
                    line,
                    " placeholder=\"{}\"",
                    truncate_outline_str(&placeholder)
                );
            }
        }

        let mut actions = Vec::new();
        for (action, name) in [
            (Action::Click, "click"),
            (Action::Focus, "focus"),
            (Action::SetValue, "set_value"),
            (Action::Expand, "expand"),
            (Action::Collapse, "collapse"),
        ] {
            if node.supports_action(action) {
                actions.push(name);
            }
        }
        if !actions.is_empty() {
            let _ = write!(line, " [{}]", actions.join(","));
        }
    }

    // AccessKit's Debug is `#N`; avoid `##N` in dogfood outlines.
    let _ = write!(line, " #NodeId({})", u64::from(id));
    line
}

fn format_focus_summary_line(
    focus: NodeId,
    node_map: &collections::FxHashMap<NodeId, &accesskit::Node>,
) -> String {
    if let Some(node) = node_map.get(&focus) {
        let role = format!("{:?}", node.role());
        let label = node.label().unwrap_or_default();
        let value = node.value().unwrap_or_default();
        let mut line = format!("# focus: [{role}]");
        if !label.is_empty() {
            line.push_str(&format!(" \"{}\"", truncate_outline_str(&label)));
        } else if !value.is_empty() {
            line.push_str(&format!(" value=\"{}\"", truncate_outline_str(&value)));
        }
        line.push_str(&format!(" #NodeId({})", u64::from(focus)));
        line
    } else {
        format!("# focus: #NodeId({})", u64::from(focus))
    }
}

/// When focus lands on a non-interactive container, walk to the nearest
/// interactive ancestor so `*` still appears on a control line (R1).
fn resolve_interactive_focus_mark(
    focus: Option<NodeId>,
    node_map: &collections::FxHashMap<NodeId, &accesskit::Node>,
    parent_of: &collections::FxHashMap<NodeId, NodeId>,
) -> Option<NodeId> {
    let mut current = focus?;
    for _ in 0..10_000 {
        let node = node_map.get(&current)?;
        if is_interactive_a11y_node(node) {
            return Some(current);
        }
        current = *parent_of.get(&current)?;
    }
    None
}

/// True when `focus` is itself a printed body line for this outline tier.
/// If so, do not also star the nearest interactive ancestor (avoids Room dual-`*`).
fn focus_is_printed_body_line(
    focus: Option<NodeId>,
    node_map: &collections::FxHashMap<NodeId, &accesskit::Node>,
    include_landmarks: bool,
) -> bool {
    focus
        .and_then(|id| node_map.get(&id).copied())
        .is_some_and(|node| {
            is_interactive_a11y_node(node) || (include_landmarks && is_landmark_a11y_node(node))
        })
}

fn node_is_focused(
    id: NodeId,
    focus: Option<NodeId>,
    interactive_focus_mark: Option<NodeId>,
    focus_on_body: bool,
) -> bool {
    if focus == Some(id) {
        return true;
    }
    // Bubble `*` only when the exact focus node is not printed in this tier.
    !focus_on_body && interactive_focus_mark == Some(id)
}

fn build_parent_map(nodes: &[(NodeId, accesskit::Node)]) -> collections::FxHashMap<NodeId, NodeId> {
    use collections::FxHashMap;
    let mut parent_of: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    for (id, node) in nodes {
        for child_id in node.children() {
            parent_of.insert(*child_id, *id);
        }
    }
    parent_of
}

fn room_header_lines(
    focus: Option<NodeId>,
    node_map: &collections::FxHashMap<NodeId, &accesskit::Node>,
    interactive_count: usize,
    landmark_count: usize,
) -> Vec<String> {
    let mut header = Vec::new();
    let window_title = node_map
        .get(&ROOT_NODE_ID)
        .and_then(|n| n.label())
        .filter(|s| !s.is_empty())
        .map(|s| truncate_outline_str(&s))
        .unwrap_or_else(|| "(untitled)".into());
    header.push(format!("# window: \"{window_title}\""));
    // Finalize always passes Some(focus) (root fallback), so post-paint dogfood
    // rarely sees `(none)` — unfocused frames show Window/root instead. Mid-frame
    // outline paths may still pass None.
    if let Some(focus) = focus {
        header.push(format_focus_summary_line(focus, node_map));
    } else {
        header.push("# focus: (none)".into());
    }
    header.push(format!(
        "# interactive: {interactive_count}  landmarks: {landmark_count}"
    ));
    header
}

/// Compact + rich + room outlines from a **single** tree walk (finalize path).
pub(crate) struct OutlineTierStrings {
    pub compact: String,
    pub rich: String,
    pub room: String,
}

pub(crate) fn format_a11y_outline_all_tiers(
    nodes: &[(NodeId, accesskit::Node)],
    focus: Option<NodeId>,
) -> OutlineTierStrings {
    use collections::FxHashMap;

    let node_map: FxHashMap<NodeId, &accesskit::Node> =
        nodes.iter().map(|(id, node)| (*id, node)).collect();
    let parent_of = build_parent_map(nodes);
    let interactive_focus_mark = resolve_interactive_focus_mark(focus, &node_map, &parent_of);

    // Compact/rich print only interactive nodes; room also prints landmarks.
    let focus_on_interactive_body = focus_is_printed_body_line(focus, &node_map, false);
    let focus_on_room_body = focus_is_printed_body_line(focus, &node_map, true);

    let mut compact_lines = Vec::new();
    let mut rich_lines = Vec::new();
    let mut room_lines = Vec::new();
    let mut interactive_count = 0usize;
    let mut landmark_count = 0usize;

    fn visit(
        id: NodeId,
        depth: usize,
        node_map: &FxHashMap<NodeId, &accesskit::Node>,
        focus: Option<NodeId>,
        interactive_focus_mark: Option<NodeId>,
        focus_on_interactive_body: bool,
        focus_on_room_body: bool,
        compact_lines: &mut Vec<String>,
        rich_lines: &mut Vec<String>,
        room_lines: &mut Vec<String>,
        interactive_count: &mut usize,
        landmark_count: &mut usize,
    ) {
        let Some(node) = node_map.get(&id) else {
            return;
        };

        if is_interactive_a11y_node(node) {
            *interactive_count += 1;
            let rich_focused =
                node_is_focused(id, focus, interactive_focus_mark, focus_on_interactive_body);
            let room_focused =
                node_is_focused(id, focus, interactive_focus_mark, focus_on_room_body);
            compact_lines.push(format_node_outline_line(
                id,
                node,
                depth,
                rich_focused,
                OutlineDetail::Compact,
            ));
            let rich_line =
                format_node_outline_line(id, node, depth, rich_focused, OutlineDetail::Rich);
            rich_lines.push(rich_line.clone());
            // Room interactive lines match rich, but focus mark may differ when
            // focus sits on a landmark that room prints and rich does not.
            if room_focused == rich_focused {
                room_lines.push(rich_line);
            } else {
                room_lines.push(format_node_outline_line(
                    id,
                    node,
                    depth,
                    room_focused,
                    OutlineDetail::Rich,
                ));
            }
        } else if is_landmark_a11y_node(node) {
            *landmark_count += 1;
            // Landmarks: exact focus only (no ancestor bubble onto landmarks).
            let focused = focus == Some(id);
            room_lines.push(format_node_outline_line(
                id,
                node,
                depth,
                focused,
                OutlineDetail::Rich,
            ));
        }

        for child_id in node.children() {
            visit(
                *child_id,
                depth + 1,
                node_map,
                focus,
                interactive_focus_mark,
                focus_on_interactive_body,
                focus_on_room_body,
                compact_lines,
                rich_lines,
                room_lines,
                interactive_count,
                landmark_count,
            );
        }
    }

    visit(
        ROOT_NODE_ID,
        0,
        &node_map,
        focus,
        interactive_focus_mark,
        focus_on_interactive_body,
        focus_on_room_body,
        &mut compact_lines,
        &mut rich_lines,
        &mut room_lines,
        &mut interactive_count,
        &mut landmark_count,
    );

    let header = room_header_lines(focus, &node_map, interactive_count, landmark_count);
    let room = if room_lines.is_empty() {
        header.join("\n")
    } else {
        format!("{}\n{}", header.join("\n"), room_lines.join("\n"))
    };

    OutlineTierStrings {
        compact: compact_lines.join("\n"),
        rich: rich_lines.join("\n"),
        room,
    }
}

/// Formats an accessibility tree as a tactile text outline for dogfood.
pub fn format_a11y_outline(nodes: &[(NodeId, accesskit::Node)], options: OutlineOptions) -> String {
    use collections::FxHashMap;

    let node_map: FxHashMap<NodeId, &accesskit::Node> =
        nodes.iter().map(|(id, node)| (*id, node)).collect();
    let parent_of = build_parent_map(nodes);
    let interactive_focus_mark =
        resolve_interactive_focus_mark(options.focus, &node_map, &parent_of);

    let include_landmarks = options.detail == OutlineDetail::Room;
    let focus_on_body = focus_is_printed_body_line(options.focus, &node_map, include_landmarks);
    let mut body_lines = Vec::new();
    let mut interactive_count = 0usize;
    let mut landmark_count = 0usize;

    fn visit(
        id: NodeId,
        depth: usize,
        node_map: &FxHashMap<NodeId, &accesskit::Node>,
        options: OutlineOptions,
        interactive_focus_mark: Option<NodeId>,
        focus_on_body: bool,
        include_landmarks: bool,
        body_lines: &mut Vec<String>,
        interactive_count: &mut usize,
        landmark_count: &mut usize,
    ) {
        let Some(node) = node_map.get(&id) else {
            return;
        };

        if is_interactive_a11y_node(node) {
            *interactive_count += 1;
            let focused = node_is_focused(id, options.focus, interactive_focus_mark, focus_on_body);
            body_lines.push(format_node_outline_line(
                id,
                node,
                depth,
                focused,
                options.detail,
            ));
        } else if include_landmarks && is_landmark_a11y_node(node) {
            *landmark_count += 1;
            // Landmarks: exact focus only.
            let focused = options.focus == Some(id);
            body_lines.push(format_node_outline_line(
                id,
                node,
                depth,
                focused,
                options.detail,
            ));
        }

        for child_id in node.children() {
            visit(
                *child_id,
                depth + 1,
                node_map,
                options,
                interactive_focus_mark,
                focus_on_body,
                include_landmarks,
                body_lines,
                interactive_count,
                landmark_count,
            );
        }
    }

    visit(
        ROOT_NODE_ID,
        0,
        &node_map,
        options,
        interactive_focus_mark,
        focus_on_body,
        include_landmarks,
        &mut body_lines,
        &mut interactive_count,
        &mut landmark_count,
    );

    if options.detail != OutlineDetail::Room {
        return body_lines.join("\n");
    }

    let header = room_header_lines(options.focus, &node_map, interactive_count, landmark_count);
    if body_lines.is_empty() {
        header.join("\n")
    } else {
        format!("{}\n{}", header.join("\n"), body_lines.join("\n"))
    }
}

/// Formats interactive nodes (rich detail, no focus marker unless provided via
/// [`format_a11y_outline`]). Kept for call sites and tests.
pub fn interactive_a11y_outline(nodes: &[(NodeId, accesskit::Node)]) -> String {
    format_a11y_outline(nodes, OutlineOptions::default())
}

#[cfg(test)]
mod tests {
    // Import specific items rather than glob-importing `super`, which would pull
    // in gpui's own `test` attribute macro and shadow the standard one.
    use super::{A11y, A11yNodeBuilder, ROOT_NODE_ID};
    use crate::FocusId;
    use accesskit::{NodeId, Role};
    use std::sync::{Arc, atomic::AtomicBool};

    fn test_node() -> accesskit::Node {
        accesskit::Node::new(Role::GenericContainer)
    }

    fn new_builder() -> A11yNodeBuilder {
        let mut builder = A11yNodeBuilder::new();
        builder.begin_frame(None);
        builder
    }

    #[test]
    fn interactive_outline_lists_buttons_and_skips_generic_containers() {
        let mut builder = new_builder();
        let button = NodeId(1);
        let container = NodeId(2);

        let mut button_node = accesskit::Node::new(Role::Button);
        button_node.set_label("Save".to_string());
        button_node.add_action(accesskit::Action::Click);
        assert!(builder.push(button, button_node));

        let container_node = accesskit::Node::new(Role::GenericContainer);
        assert!(builder.push(container, container_node));
        builder.pop();
        builder.pop();

        let nodes = builder.collect_snapshot_nodes();
        let outline = super::interactive_a11y_outline(&nodes);
        assert!(outline.contains("Button"));
        assert!(outline.contains("Save"));
        assert!(!outline.contains("GenericContainer"));
    }

    #[test]
    fn rich_outline_includes_focus_bounds_states_and_actions() {
        use super::{OutlineDetail, OutlineOptions, format_a11y_outline};

        let button = NodeId(1);
        let container = NodeId(2);
        let mut button_node = accesskit::Node::new(Role::Button);
        button_node.set_label("Save".to_string());
        button_node.set_disabled();
        button_node.set_toggled(accesskit::Toggled::True);
        button_node.set_selected(true);
        button_node.set_expanded(false);
        button_node.set_description("Commits the draft".to_string());
        button_node.set_placeholder("unused-on-button".to_string());
        button_node.set_bounds(accesskit::Rect {
            x0: 10.0,
            y0: 20.0,
            x1: 98.0,
            y1: 48.0,
        });
        button_node.add_action(accesskit::Action::Click);
        button_node.add_action(accesskit::Action::Focus);
        let container_node = accesskit::Node::new(Role::GenericContainer);

        let nodes = vec![
            (super::ROOT_NODE_ID, {
                let mut root = accesskit::Node::new(Role::Window);
                root.set_children(vec![button, container]);
                root
            }),
            (button, button_node),
            (container, container_node),
        ];

        let outline = format_a11y_outline(
            &nodes,
            OutlineOptions {
                focus: Some(button),
                detail: OutlineDetail::Rich,
            },
        );
        assert!(
            outline
                .lines()
                .any(|line| line.trim_start().starts_with("*[Button]") && line.contains("Save")),
            "focus marker after indent: {outline}"
        );
        assert!(outline.contains("@10,20 88x28"), "bounds: {outline}");
        assert!(outline.contains("[disabled]"), "disabled: {outline}");
        assert!(outline.contains("[toggled=true]"), "toggled: {outline}");
        assert!(outline.contains("[selected]"), "selected: {outline}");
        assert!(outline.contains("[collapsed]"), "expanded=false: {outline}");
        assert!(
            outline.contains("desc=\"Commits the draft\""),
            "description: {outline}"
        );
        assert!(
            outline.contains("placeholder=\"unused-on-button\""),
            "placeholder: {outline}"
        );
        assert!(outline.contains("[click,focus]"), "actions: {outline}");
        assert!(outline.contains("Save"), "label: {outline}");
        assert!(
            !outline.contains("GenericContainer"),
            "skip non-interactive sibling: {outline}"
        );
    }

    #[test]
    fn rich_outline_marks_nearest_interactive_ancestor_when_focus_is_container() {
        use super::{OutlineDetail, OutlineOptions, format_a11y_outline};

        let button = NodeId(1);
        let inner = NodeId(2);
        let mut button_node = accesskit::Node::new(Role::Button);
        button_node.set_label("Save".to_string());
        button_node.add_action(accesskit::Action::Click);
        button_node.set_children(vec![inner]);
        let inner_node = accesskit::Node::new(Role::GenericContainer);

        let mut root = accesskit::Node::new(Role::Window);
        root.set_children(vec![button]);
        let nodes = vec![
            (super::ROOT_NODE_ID, root),
            (button, button_node),
            (inner, inner_node),
        ];

        let outline = format_a11y_outline(
            &nodes,
            OutlineOptions {
                focus: Some(inner),
                detail: OutlineDetail::Rich,
            },
        );
        assert!(
            outline.lines().any(|line| {
                line.trim_start().starts_with("*[Button]") && line.contains("Save")
            }),
            "focus on non-interactive child marks button ancestor: {outline}"
        );
        assert!(
            !outline.contains("GenericContainer"),
            "container still skipped"
        );
    }

    #[test]
    fn outline_truncates_long_description_and_placeholder() {
        use super::{OUTLINE_STRING_MAX, OutlineDetail, OutlineOptions, format_a11y_outline};

        let input = NodeId(1);
        let long: String = "x".repeat(OUTLINE_STRING_MAX + 20);
        let mut input_node = accesskit::Node::new(Role::TextInput);
        input_node.set_description(long.clone());
        input_node.set_placeholder(long);
        input_node.add_action(accesskit::Action::SetValue);
        input_node.add_action(accesskit::Action::Focus);

        let mut root = accesskit::Node::new(Role::Window);
        root.set_children(vec![input]);
        let nodes = vec![(super::ROOT_NODE_ID, root), (input, input_node)];
        let outline = format_a11y_outline(
            &nodes,
            OutlineOptions {
                focus: None,
                detail: OutlineDetail::Rich,
            },
        );
        assert!(outline.contains('…'), "truncated ellipsis: {outline}");
        assert!(
            !outline.contains(&"x".repeat(OUTLINE_STRING_MAX + 1)),
            "long strings must not appear full-length: {outline}"
        );
        assert!(
            outline.contains("[set_value,focus]") || outline.contains("set_value"),
            "{outline}"
        );
    }

    #[test]
    fn room_outline_has_header_and_landmarks() {
        use super::{OutlineDetail, OutlineOptions, format_a11y_outline};

        let heading = NodeId(1);
        let button = NodeId(2);
        let mut heading_node = accesskit::Node::new(Role::Heading);
        heading_node.set_label("Welcome".to_string());
        let mut button_node = accesskit::Node::new(Role::Button);
        button_node.set_label("Go".to_string());
        button_node.add_action(accesskit::Action::Click);

        let mut root = accesskit::Node::new(Role::Window);
        root.set_label("Zed".to_string());
        root.set_children(vec![heading, button]);

        let nodes = vec![
            (super::ROOT_NODE_ID, root),
            (heading, heading_node),
            (button, button_node),
        ];
        let outline = format_a11y_outline(
            &nodes,
            OutlineOptions {
                focus: Some(button),
                detail: OutlineDetail::Room,
            },
        );
        let lines: Vec<_> = outline.lines().collect();
        assert_eq!(lines[0], "# window: \"Zed\"", "{outline}");
        assert_eq!(lines[1], "# focus: [Button] \"Go\" #NodeId(2)", "{outline}");
        assert_eq!(lines[2], "# interactive: 1  landmarks: 1", "{outline}");
        assert!(
            outline.contains("  [Heading] \"Welcome\""),
            "landmark keeps depth indent: {outline}"
        );
        assert!(
            outline
                .lines()
                .any(|line| line.trim_start().starts_with("*[Button]") && line.contains("Go")),
            "room stars focused interactive button: {outline}"
        );
        assert!(
            !outline.contains("GenericContainer"),
            "chrome not printed: {outline}"
        );
    }

    #[test]
    fn all_tiers_match_per_detail_formatters() {
        use super::{
            OutlineDetail, OutlineOptions, format_a11y_outline, format_a11y_outline_all_tiers,
        };

        let heading = NodeId(1);
        let button = NodeId(2);
        let mut heading_node = accesskit::Node::new(Role::Heading);
        heading_node.set_label("Welcome".to_string());
        let mut button_node = accesskit::Node::new(Role::Button);
        button_node.set_label("Go".to_string());
        button_node.add_action(accesskit::Action::Click);

        let mut root = accesskit::Node::new(Role::Window);
        root.set_label("Zed".to_string());
        root.set_children(vec![heading, button]);
        let nodes = vec![
            (super::ROOT_NODE_ID, root),
            (heading, heading_node),
            (button, button_node),
        ];
        let focus = Some(button);
        let tiers = format_a11y_outline_all_tiers(&nodes, focus);

        assert_eq!(
            tiers.room,
            format_a11y_outline(
                &nodes,
                OutlineOptions {
                    focus,
                    detail: OutlineDetail::Room,
                },
            ),
            "finalize room path must match format_a11y_outline(Room)"
        );
        assert_eq!(
            tiers.rich,
            format_a11y_outline(
                &nodes,
                OutlineOptions {
                    focus,
                    detail: OutlineDetail::Rich,
                },
            ),
        );
        assert_eq!(
            tiers.compact,
            format_a11y_outline(
                &nodes,
                OutlineOptions {
                    focus,
                    detail: OutlineDetail::Compact,
                },
            ),
        );
        assert!(!tiers.rich.contains("[Heading]"), "{}", tiers.rich);
        assert!(!tiers.compact.contains("[Heading]"), "{}", tiers.compact);
        assert!(!tiers.rich.starts_with("# window:"), "{}", tiers.rich);
        assert!(tiers.room.contains("[Heading]"), "{}", tiers.room);
        assert!(tiers.room.starts_with("# window:"), "{}", tiers.room);

        // Landmark-under-interactive focus: room stars Heading, rich/compact star Button.
        // Both walkers (all_tiers vs format_a11y_outline) must stay in lockstep.
        let nested_button = NodeId(1);
        let nested_heading = NodeId(2);
        let mut nested_button_node = accesskit::Node::new(Role::Button);
        nested_button_node.set_label("Go".to_string());
        nested_button_node.add_action(accesskit::Action::Click);
        nested_button_node.set_children(vec![nested_heading]);
        let mut nested_heading_node = accesskit::Node::new(Role::Heading);
        nested_heading_node.set_label("Welcome".to_string());
        let mut nested_root = accesskit::Node::new(Role::Window);
        nested_root.set_label("Zed".to_string());
        nested_root.set_children(vec![nested_button]);
        let nested = vec![
            (super::ROOT_NODE_ID, nested_root),
            (nested_button, nested_button_node),
            (nested_heading, nested_heading_node),
        ];
        let landmark_focus = Some(nested_heading);
        let nested_tiers = format_a11y_outline_all_tiers(&nested, landmark_focus);
        for (detail, got) in [
            (OutlineDetail::Room, nested_tiers.room.as_str()),
            (OutlineDetail::Rich, nested_tiers.rich.as_str()),
            (OutlineDetail::Compact, nested_tiers.compact.as_str()),
        ] {
            let expected = format_a11y_outline(
                &nested,
                OutlineOptions {
                    focus: landmark_focus,
                    detail,
                },
            );
            assert_eq!(
                got, expected,
                "all_tiers must match format_a11y_outline({detail:?}) when focus is a landmark"
            );
        }
        assert!(
            nested_tiers
                .room
                .lines()
                .any(|line| line.trim_start().starts_with("*[Heading]")),
            "room stars landmark: {}",
            nested_tiers.room
        );
        assert!(
            nested_tiers
                .rich
                .lines()
                .any(|line| line.trim_start().starts_with("*[Button]")),
            "rich stars interactive ancestor: {}",
            nested_tiers.rich
        );
        assert!(
            !nested_tiers.rich.contains("[Heading]"),
            "rich omits landmark: {}",
            nested_tiers.rich
        );
    }

    #[test]
    fn room_landmarks_omitted_from_rich_and_compact() {
        use super::{OutlineDetail, OutlineOptions, format_a11y_outline};

        let heading = NodeId(1);
        let dialog = NodeId(2);
        let button = NodeId(3);
        let mut heading_node = accesskit::Node::new(Role::Heading);
        heading_node.set_label("Title".to_string());
        let mut dialog_node = accesskit::Node::new(Role::Dialog);
        dialog_node.set_label("Confirm".to_string());
        let mut button_node = accesskit::Node::new(Role::Button);
        button_node.set_label("Ok".to_string());
        button_node.add_action(accesskit::Action::Click);

        let mut root = accesskit::Node::new(Role::Window);
        root.set_label("App".to_string());
        root.set_children(vec![heading, dialog, button]);
        let nodes = vec![
            (super::ROOT_NODE_ID, root),
            (heading, heading_node),
            (dialog, dialog_node),
            (button, button_node),
        ];

        let room = format_a11y_outline(
            &nodes,
            OutlineOptions {
                focus: Some(button),
                detail: OutlineDetail::Room,
            },
        );
        assert!(room.contains("[Heading]"), "{room}");
        assert!(room.contains("[Dialog]"), "{room}");
        assert!(room.contains("landmarks: 2"), "{room}");

        for detail in [OutlineDetail::Rich, OutlineDetail::Compact] {
            let outline = format_a11y_outline(
                &nodes,
                OutlineOptions {
                    focus: Some(button),
                    detail,
                },
            );
            assert!(
                !outline.starts_with("# window:"),
                "header is room-only ({detail:?}): {outline}"
            );
            assert!(
                !outline.contains("[Heading]") && !outline.contains("[Dialog]"),
                "landmarks are room-only ({detail:?}): {outline}"
            );
            assert!(outline.contains("[Button]"), "{outline}");
        }
    }

    #[test]
    fn is_landmark_skips_chrome_and_unlabeled_lists() {
        use super::{is_interactive_a11y_node, is_landmark_a11y_node};

        assert!(is_landmark_a11y_node(&accesskit::Node::new(Role::Heading)));
        assert!(is_landmark_a11y_node(&accesskit::Node::new(Role::Dialog)));
        assert!(is_landmark_a11y_node(&accesskit::Node::new(
            Role::AlertDialog
        )));
        assert!(is_landmark_a11y_node(&accesskit::Node::new(Role::Toolbar)));
        assert!(is_landmark_a11y_node(&accesskit::Node::new(Role::MenuBar)));
        assert!(is_landmark_a11y_node(&accesskit::Node::new(Role::Menu)));
        assert!(is_landmark_a11y_node(&accesskit::Node::new(Role::TabList)));

        let mut labeled = accesskit::Node::new(Role::Label);
        labeled.set_label("Name".to_string());
        assert!(is_landmark_a11y_node(&labeled));
        assert!(!is_landmark_a11y_node(&accesskit::Node::new(Role::Label)));

        let mut valued_list = accesskit::Node::new(Role::List);
        valued_list.set_value("3 items".to_string());
        assert!(is_landmark_a11y_node(&valued_list));
        assert!(!is_landmark_a11y_node(&accesskit::Node::new(Role::List)));
        assert!(!is_landmark_a11y_node(&accesskit::Node::new(
            Role::ListItem
        )));

        assert!(!is_landmark_a11y_node(&accesskit::Node::new(
            Role::GenericContainer
        )));

        // Interactive filter must stay strict: buttons are never landmarks.
        let mut button = accesskit::Node::new(Role::Button);
        button.add_action(accesskit::Action::Click);
        assert!(is_interactive_a11y_node(&button));
        assert!(!is_landmark_a11y_node(&button));
    }

    #[test]
    fn room_outline_does_not_dual_star_landmark_and_interactive_ancestor() {
        use super::{OutlineDetail, OutlineOptions, format_a11y_outline};

        // Nest landmark under interactive control so resolve_interactive_focus_mark
        // would return the button (pre-fix dual-star path). Room must star only
        // the printed landmark, not the interactive ancestor.
        //
        //   Window
        //     Button
        //       Heading  (focus)
        let button = NodeId(1);
        let heading = NodeId(2);
        let mut button_node = accesskit::Node::new(Role::Button);
        button_node.set_label("Go".to_string());
        button_node.add_action(accesskit::Action::Click);
        button_node.set_children(vec![heading]);
        let mut heading_node = accesskit::Node::new(Role::Heading);
        heading_node.set_label("Welcome".to_string());

        let mut root = accesskit::Node::new(Role::Window);
        root.set_children(vec![button]);
        let nodes = vec![
            (super::ROOT_NODE_ID, root),
            (button, button_node),
            (heading, heading_node),
        ];

        let outline = format_a11y_outline(
            &nodes,
            OutlineOptions {
                focus: Some(heading),
                detail: OutlineDetail::Room,
            },
        );
        let starred: Vec<_> = outline
            .lines()
            .filter(|line| !line.starts_with('#') && line.contains('*'))
            .collect();
        assert_eq!(
            starred.len(),
            1,
            "exactly one body focus mark expected: {outline}"
        );
        assert!(
            starred[0].contains("[Heading]"),
            "landmark keeps exact focus: {outline}"
        );
        assert!(
            !outline
                .lines()
                .any(|line| line.contains('*') && line.contains("[Button]")),
            "no dual-star on interactive ancestor: {outline}"
        );

        // Rich omits landmarks: same focus should bubble `*` onto the button.
        let rich = format_a11y_outline(
            &nodes,
            OutlineOptions {
                focus: Some(heading),
                detail: OutlineDetail::Rich,
            },
        );
        assert!(
            rich.lines()
                .any(|line| line.trim_start().starts_with("*[Button]") && line.contains("Go")),
            "rich bubbles focus to interactive ancestor: {rich}"
        );
        assert!(
            !rich.contains("[Heading]"),
            "rich still skips landmark: {rich}"
        );
    }

    #[test]
    fn compact_outline_omits_bounds_and_action_list() {
        use super::{OutlineDetail, OutlineOptions, format_a11y_outline};

        let button = NodeId(1);
        let mut button_node = accesskit::Node::new(Role::Button);
        button_node.set_label("Save".to_string());
        button_node.set_bounds(accesskit::Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 10.0,
            y1: 10.0,
        });
        button_node.add_action(accesskit::Action::Click);

        let mut root = accesskit::Node::new(Role::Window);
        root.set_children(vec![button]);
        let nodes = vec![(super::ROOT_NODE_ID, root), (button, button_node)];
        let outline = format_a11y_outline(
            &nodes,
            OutlineOptions {
                focus: Some(button),
                detail: OutlineDetail::Compact,
            },
        );
        assert!(outline.contains("[Button]"), "{outline}");
        assert!(outline.contains("Save"), "{outline}");
        assert!(!outline.contains('@'), "compact has no bounds: {outline}");
        assert!(
            !outline.contains("[click]"),
            "compact has no actions: {outline}"
        );
        assert!(
            outline.contains('*'),
            "compact still marks focus: {outline}"
        );
    }

    #[test]
    fn outline_includes_value_on_text_input_and_room_focus() {
        use super::{OutlineDetail, OutlineOptions, format_a11y_outline};

        let input = NodeId(1);
        let mut input_node = accesskit::Node::new(Role::TextInput);
        input_node.set_value("fn main".to_string());
        input_node.add_action(accesskit::Action::SetValue);
        input_node.add_action(accesskit::Action::Focus);

        let mut root = accesskit::Node::new(Role::Window);
        root.set_label("Zed".to_string());
        root.set_children(vec![input]);
        let nodes = vec![(super::ROOT_NODE_ID, root), (input, input_node)];

        let rich = format_a11y_outline(
            &nodes,
            OutlineOptions {
                focus: Some(input),
                detail: OutlineDetail::Rich,
            },
        );
        assert!(
            rich.contains("value=\"fn main\""),
            "rich body carries value: {rich}"
        );
        assert!(
            rich.lines()
                .any(|line| line.trim_start().starts_with("*[TextInput]")),
            "focus mark on text input: {rich}"
        );

        let compact = format_a11y_outline(
            &nodes,
            OutlineOptions {
                focus: Some(input),
                detail: OutlineDetail::Compact,
            },
        );
        assert!(
            compact.contains("value=\"fn main\""),
            "compact also carries value (lean path is role/label/value/id): {compact}"
        );
        assert!(
            compact.lines()
                .any(|line| line.trim_start().starts_with("*[TextInput]")),
            "compact still marks focus: {compact}"
        );
        assert!(
            !compact.contains('@'),
            "compact omits bounds even when value present: {compact}"
        );

        let room = format_a11y_outline(
            &nodes,
            OutlineOptions {
                focus: Some(input),
                detail: OutlineDetail::Room,
            },
        );
        assert!(
            room.contains("# focus: [TextInput] value=\"fn main\" #NodeId(1)"),
            "room focus summary prefers value when label empty: {room}"
        );
        assert!(
            room.contains("value=\"fn main\""),
            "room body also has value: {room}"
        );
    }

    #[test]
    fn interactive_outline_available_after_finalize() {
        let mut builder = new_builder();
        let button = NodeId(1);

        let mut button_node = accesskit::Node::new(Role::Button);
        button_node.set_label("Open".to_string());
        button_node.add_action(accesskit::Action::Click);
        assert!(builder.push(button, button_node));
        builder.pop();

        let update = builder.finalize();
        // Regression: finalize takes all_nodes for TreeUpdate; mid-frame collect is empty.
        assert!(
            builder.collect_snapshot_nodes().is_empty(),
            "post-finalize mid-frame node list must be empty"
        );
        assert!(!update.nodes.is_empty());

        let outline = builder.last_interactive_outline();
        assert!(!outline.is_empty());
        assert!(
            outline.contains("[Button]"),
            "outline should include role: {outline:?}"
        );
        assert!(
            outline.contains("Open"),
            "post-frame snapshot must retain interactive label: {outline:?}"
        );
        assert!(
            outline.contains("#NodeId(1)"),
            "outline should include exact #NodeId(1): {outline:?}"
        );
    }

    #[test]
    fn post_finalize_outline_empty_when_no_interactive_nodes() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let container_node = accesskit::Node::new(Role::GenericContainer);
        assert!(builder.push(container, container_node));
        builder.pop();

        let _update = builder.finalize();
        assert!(
            builder.last_interactive_outline().is_empty(),
            "non-interactive-only trees yield empty interactive outline"
        );
    }

    #[test]
    fn begin_frame_does_not_clear_last_outline_until_next_finalize() {
        let mut builder = new_builder();
        let button = NodeId(1);
        let mut button_node = accesskit::Node::new(Role::Button);
        button_node.set_label("Keep".to_string());
        button_node.add_action(accesskit::Action::Click);
        assert!(builder.push(button, button_node));
        builder.pop();
        let _ = builder.finalize();
        assert!(builder.last_interactive_outline().contains("Keep"));

        builder.begin_frame(None);
        assert!(
            builder.last_interactive_outline().contains("Keep"),
            "last outline survives begin_frame until the next finalize replaces it"
        );
        assert!(builder.has_in_progress_frame());
    }

    fn new_a11y() -> A11y {
        let mut a11y = A11y::new(Arc::new(AtomicBool::new(true)), false, None);
        a11y.begin_frame();
        a11y
    }

    #[test]
    fn active_descendant_honored_when_container_focused() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let item = NodeId(2);

        assert!(builder.push(container, test_node()));
        builder.set_focus(container);
        assert!(builder.push(item, test_node()));

        // The item is on top of the stack; the focused container is its
        // ancestor, so the claim is honored.
        assert!(builder.focus_is_ancestor_of_current());
        builder.set_active_descendant(item);

        builder.pop(); // item
        builder.pop(); // container
        let update = builder.finalize();
        assert_eq!(update.focus, item);
    }

    #[test]
    fn active_descendant_honored_for_deep_descendant() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let group = NodeId(2);
        let item = NodeId(3);

        assert!(builder.push(container, test_node()));
        builder.set_focus(container);
        assert!(builder.push(group, test_node()));
        assert!(builder.push(item, test_node()));

        // The item is a grandchild of the focused container; depth doesn't
        // matter, the focused ancestor is still on the stack.
        assert!(builder.focus_is_ancestor_of_current());
        builder.set_active_descendant(item);

        builder.pop(); // item
        builder.pop(); // group
        builder.pop(); // container
        let update = builder.finalize();
        assert_eq!(update.focus, item);
    }

    #[test]
    fn active_descendant_ignored_when_focus_in_other_subtree() {
        let mut builder = new_builder();
        let focused_container = NodeId(1);
        let focused_leaf = NodeId(2);
        let other_container = NodeId(3);
        let other_item = NodeId(4);

        // First subtree holds real focus.
        assert!(builder.push(focused_container, test_node()));
        assert!(builder.push(focused_leaf, test_node()));
        builder.set_focus(focused_leaf);
        builder.pop(); // focused_leaf
        builder.pop(); // focused_container

        // Second subtree: its item would claim the active descendant, but the
        // focus is not on any of its ancestors, so the gate rejects it.
        assert!(builder.push(other_container, test_node()));
        assert!(builder.push(other_item, test_node()));
        assert!(!builder.focus_is_ancestor_of_current());
        builder.pop(); // other_item
        builder.pop(); // other_container

        let update = builder.finalize();
        assert_eq!(update.focus, focused_leaf);
    }

    #[test]
    fn active_descendant_ignored_when_nothing_focused() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let item = NodeId(2);

        assert!(builder.push(container, test_node()));
        assert!(builder.push(item, test_node()));

        // Nothing is focused (focus defaults to the root window node), so the
        // gate rejects the claim.
        assert!(!builder.focus_is_ancestor_of_current());
        builder.pop();
        builder.pop();

        let update = builder.finalize();
        assert_eq!(update.focus, ROOT_NODE_ID);
    }

    #[test]
    fn regular_focus_used_when_no_active_descendant() {
        let mut builder = new_builder();
        let focused = NodeId(1);

        assert!(builder.push(focused, test_node()));
        builder.set_focus(focused);
        builder.pop();

        let update = builder.finalize();
        assert_eq!(update.focus, focused);
    }

    #[test]
    fn focus_is_ancestor_excludes_self_and_non_ancestors() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let item = NodeId(2);

        assert!(builder.push(container, test_node()));
        builder.set_focus(container);

        // With the focused container itself on top, it is not its own (strict)
        // ancestor, so the gate is false.
        assert!(!builder.focus_is_ancestor_of_current());

        assert!(builder.push(item, test_node()));
        // Now the focused container is a strict ancestor of the item on top.
        assert!(builder.focus_is_ancestor_of_current());

        builder.pop();
        builder.pop();
    }

    // The double-claim guard panics only in debug builds; in release it falls
    // back to last-wins with a warning.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "active descendant claimed by multiple nodes")
    )]
    fn multiple_active_descendant_claims_panic_in_debug() {
        let mut builder = new_builder();
        builder.set_active_descendant(NodeId(1));
        builder.set_active_descendant(NodeId(2));
    }

    // Setting focus twice in one frame means two elements both claimed window
    // focus; that panics in debug and falls back to last-wins in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "set_focus called more than once")
    )]
    fn setting_focus_twice_panics_in_debug() {
        let mut builder = new_builder();
        builder.set_focus(NodeId(1));
        builder.set_focus(NodeId(2));
    }

    // Focusing a node that was never registered as focusable is a bug: panic in
    // debug, warn in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "was not registered with set_focusable")
    )]
    fn set_focus_without_set_focusable() {
        let mut a11y = new_a11y();
        let node = NodeId(1);
        assert!(a11y.nodes.push(node, test_node()));
        // set_focusable was never called for `node`.
        a11y.set_focus(node);
    }

    // The focused node cannot also be its own active descendant: panic in
    // debug, warn in release.
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "on the focused node"))]
    fn set_active_descendant_on_focused_node() {
        let mut a11y = new_a11y();
        let node = NodeId(1);
        assert!(a11y.nodes.push(node, test_node()));
        a11y.set_focusable(node, FocusId::default());
        a11y.set_focus(node);
        a11y.set_active_descendant(node);
    }

    // Two sibling children of a focused container both claim the active
    // descendant (both pass the focus gate). The second claim is a bug: panic
    // in debug, last-wins + warn in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "active descendant claimed by multiple nodes")
    )]
    fn two_siblings_claiming_active_descendant() {
        let mut a11y = new_a11y();
        let container = NodeId(1);
        let first = NodeId(2);
        let second = NodeId(3);

        assert!(a11y.nodes.push(container, test_node()));
        a11y.set_focusable(container, FocusId::default());
        a11y.set_focus(container);

        assert!(a11y.nodes.push(first, test_node()));
        a11y.set_active_descendant(first);
        a11y.nodes.pop(); // first

        assert!(a11y.nodes.push(second, test_node()));
        a11y.set_active_descendant(second);
        a11y.nodes.pop(); // second

        a11y.nodes.pop(); // container
    }

    // Node A is focused; node C (a child of the unfocused node B) claims the
    // active descendant. The final tree must still report A as focused.
    #[test]
    fn active_descendant_in_unfocused_subtree_keeps_real_focus() {
        let mut a11y = new_a11y();
        let a = NodeId(1);
        let b = NodeId(2);
        let c = NodeId(3);

        assert!(a11y.nodes.push(a, test_node()));
        a11y.set_focusable(a, FocusId::default());
        a11y.set_focus(a);
        a11y.nodes.pop(); // a

        assert!(a11y.nodes.push(b, test_node()));
        assert!(a11y.nodes.push(c, test_node()));
        a11y.set_active_descendant(c);
        a11y.nodes.pop(); // c
        a11y.nodes.pop(); // b

        let update = a11y.end_frame();
        assert_eq!(update.focus, a);
    }
}
