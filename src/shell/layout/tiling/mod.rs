// SPDX-License-Identifier: GPL-3.0-only

use crate::{
    shell::{focus::FocusDirection, layout::Orientation},
    utils::prelude::*,
};

use atomic_float::AtomicF64;
use id_tree::{InsertBehavior, MoveBehavior, Node, NodeId, NodeIdError, RemoveBehavior, Tree};
use smithay::{
    desktop::{layer_map_for_output, Kind, Space, Window},
    input::{
        pointer::{Focus, GrabStartData as PointerGrabStartData},
        Seat,
    },
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::{
        ResizeEdge, State as XdgState,
    },
    utils::{IsAlive, Rectangle, Serial},
};
use std::{
    cell::RefCell,
    collections::HashSet,
    sync::{atomic::Ordering, Arc},
};

mod grabs;
pub use self::grabs::*;

#[derive(Debug)]
pub struct TilingLayout {
    gaps: (i32, i32),
    trees: Vec<Tree<Data>>,
    pub windows: HashSet<Window>,
}

#[derive(Debug)]
pub enum Data {
    Fork {
        orientation: Orientation,
        ratio: Arc<AtomicF64>,
    },
    Stack {
        active: usize,
        len: usize,
    },
    Window(Window),
}

#[derive(Debug, Clone)]
pub struct WindowInfo {
    node: NodeId,
    output: usize,
}

impl Data {
    fn fork() -> Data {
        Data::Fork {
            orientation: Orientation::Vertical,
            ratio: Arc::new(AtomicF64::new(0.5)),
        }
    }
}

impl TilingLayout {
    pub fn new() -> TilingLayout {
        TilingLayout {
            gaps: (0, 4),
            trees: Vec::new(),
            windows: HashSet::new(),
        }
    }
}

impl TilingLayout {
    pub fn map_window<'a>(
        &mut self,
        space: &mut Space,
        window: Window,
        seat: &Seat<State>,
        focus_stack: impl Iterator<Item = &'a Window> + 'a,
    ) {
        self.map_window_internal(space, &window, Some(seat), Some(focus_stack));
        self.windows.insert(window);
        self.refresh(space);
    }

    pub fn move_focus<'a>(
        &mut self,
        direction: FocusDirection,
        seat: &Seat<State>,
        space: &mut Space,
        focus_stack: impl Iterator<Item = &'a Window> + 'a,
    ) -> Option<Window> {
        let output = super::output_from_seat(Some(seat), space);
        let idx = space
            .outputs()
            .position(|o| Some(o) == output.as_ref())
            .unwrap_or(0);
        let tree = TilingLayout::active_tree(&mut self.trees, idx);
        if let Some(last_active) = TilingLayout::last_active_window(tree, focus_stack) {
            let mut node_id = last_active;
            while let Some((fork, child)) = TilingLayout::find_fork(tree, node_id) {
                if let &Data::Fork {
                    ref orientation, ..
                } = tree.get(&fork).unwrap().data()
                {
                    // found a fork
                    // which child are we?
                    let first = tree.children_ids(&fork).unwrap().next() == Some(&child);
                    let focus_subtree = match (first, orientation, direction) {
                        (true, Orientation::Horizontal, FocusDirection::Down)
                        | (true, Orientation::Vertical, FocusDirection::Right) => {
                            tree.children_ids(&fork).unwrap().skip(1).next()
                        }
                        (false, Orientation::Horizontal, FocusDirection::Up)
                        | (false, Orientation::Vertical, FocusDirection::Left) => {
                            tree.children_ids(&fork).unwrap().next()
                        }
                        _ => None, // continue iterating
                    };

                    if focus_subtree.is_some() {
                        let mut node_id = focus_subtree;
                        while node_id.is_some()
                            && !matches!(
                                tree.get(node_id.as_ref().unwrap()).unwrap().data(),
                                Data::Window(_)
                            )
                        {
                            // TODO, if ndoe_id is a stack, we want to assign the active node instead of the first
                            node_id = tree.children_ids(node_id.as_ref().unwrap()).unwrap().next();
                        }
                        if let Some(Data::Window(window)) =
                            node_id.and_then(|i| tree.get(&i).ok()).map(|n| n.data())
                        {
                            return Some(window.clone());
                        }
                    }
                }
                node_id = fork;
            }
        }

        None
    }

    pub fn update_orientation<'a>(
        &mut self,
        new_orientation: Orientation,
        seat: &Seat<State>,
        space: &mut Space,
        focus_stack: impl Iterator<Item = &'a Window> + 'a,
    ) {
        let output = super::output_from_seat(Some(seat), space);
        let idx = space
            .outputs()
            .position(|o| Some(o) == output.as_ref())
            .unwrap_or(0);
        let tree = TilingLayout::active_tree(&mut self.trees, idx);
        if let Some(last_active) = TilingLayout::last_active_window(tree, focus_stack) {
            if let Some((fork, _child)) = TilingLayout::find_fork(tree, last_active) {
                if let &mut Data::Fork {
                    ref mut orientation,
                    ..
                } = tree.get_mut(&fork).unwrap().data_mut()
                {
                    *orientation = new_orientation;
                }
            }
        }
        self.refresh(space);
    }

    pub fn refresh<'a>(&mut self, space: &mut Space) {
        let active_outputs = space.outputs().count();
        if self.trees.len() > active_outputs {
            for tree in self
                .trees
                .drain(active_outputs..self.trees.len())
                .collect::<Vec<_>>()
                .into_iter()
            {
                if let Some(root_id) = tree.root_node_id() {
                    for node in tree.traverse_pre_order(root_id).unwrap() {
                        if let Data::Window(window) = node.data() {
                            self.map_window_internal(
                                space,
                                window,
                                None,
                                Option::<std::iter::Empty<&Window>>::None,
                            );
                        }
                    }
                }
            }
        }

        let mut changed = false;
        while let Some(dead_windows) = Some(TilingLayout::update_space_positions(
            &mut self.trees,
            space,
            self.gaps,
        ))
        .filter(|v| !v.is_empty())
        {
            for window in dead_windows {
                self.unmap_window_internal(&window);
            }
            changed = true;
        }
        if changed {
            for window in &self.windows {
                update_reactive_popups(space, window);
            }
        }
    }

    pub fn unmap_window(&mut self, space: &mut Space, window: &Window) {
        self.unmap_window_internal(window);
        space.unmap_window(window);
        #[allow(irrefutable_let_patterns)]
        if let Kind::Xdg(xdg) = &window.toplevel() {
            xdg.with_pending_state(|state| {
                state.states.unset(XdgState::TiledLeft);
                state.states.unset(XdgState::TiledRight);
                state.states.unset(XdgState::TiledTop);
                state.states.unset(XdgState::TiledBottom);
            });
        }
        self.windows.remove(window);
        self.refresh(space);
    }

    pub fn resize_request(
        state: &mut State,
        window: &Window,
        seat: &Seat<State>,
        serial: Serial,
        start_data: PointerGrabStartData<State>,
        edges: ResizeEdge,
    ) {
        // it is so stupid, that we have to do this here. TODO: Refactor grabs
        let workspace = state
            .common
            .shell
            .space_for_window_mut(window.toplevel().wl_surface())
            .unwrap();
        let space = &mut workspace.space;
        let trees = &mut workspace.tiling_layer.trees;

        if let Some(pointer) = seat.get_pointer() {
            if let Some(info) = window.user_data().get::<RefCell<WindowInfo>>() {
                let output = info.borrow().output;
                let tree = TilingLayout::active_tree(trees, output);
                let mut node_id = info.borrow().node.clone();

                while let Some((fork, child)) = TilingLayout::find_fork(tree, node_id) {
                    if let &Data::Fork {
                        ref orientation,
                        ref ratio,
                    } = tree.get(&fork).unwrap().data()
                    {
                        // found a fork
                        // which child are we?
                        let first = tree.children_ids(&fork).unwrap().next() == Some(&child);
                        match (first, orientation, edges) {
                            (true, Orientation::Horizontal, ResizeEdge::Bottom)
                            | (false, Orientation::Horizontal, ResizeEdge::Top)
                            | (true, Orientation::Vertical, ResizeEdge::Right)
                            | (false, Orientation::Vertical, ResizeEdge::Left) => {
                                let output = space.outputs().nth(output).cloned();
                                if let Some(output) = output {
                                    let grab = ResizeForkGrab {
                                        start_data,
                                        orientation: *orientation,
                                        initial_ratio: ratio.load(Ordering::SeqCst),
                                        initial_size: layer_map_for_output(&output)
                                            .non_exclusive_zone()
                                            .size,
                                        ratio: ratio.clone(),
                                    };

                                    pointer.set_grab(state, grab, serial, Focus::Clear);
                                }
                                return;
                            }
                            _ => {} // continue iterating
                        }
                    }
                    node_id = fork;
                }
            }
        }
    }

    fn active_tree<'a>(trees: &'a mut Vec<Tree<Data>>, output: usize) -> &'a mut Tree<Data> {
        while trees.len() <= output {
            trees.push(Tree::new())
        }
        &mut trees[output]
    }

    fn last_active_window<'a>(
        tree: &mut Tree<Data>,
        mut focus_stack: impl Iterator<Item = &'a Window>,
    ) -> Option<NodeId> {
        let last_active = focus_stack
            .find_map(|window| tree.root_node_id()
                .and_then(|root| tree.traverse_pre_order_ids(root).unwrap()
                    .find(|id| matches!(tree.get(id).map(|n| n.data()), Ok(Data::Window(w)) if w == window))
                )
            );

        last_active
    }

    fn find_fork(tree: &mut Tree<Data>, mut node_id: NodeId) -> Option<(NodeId, NodeId)> {
        while let Some(parent_id) = tree.get(&node_id).unwrap().parent().cloned() {
            if let &Data::Fork { .. } = tree.get(&parent_id).unwrap().data() {
                return Some((parent_id, node_id));
            }
            node_id = parent_id;
        }
        None
    }

    fn map_window_internal<'a>(
        &mut self,
        space: &mut Space,
        window: &Window,
        seat: Option<&Seat<State>>,
        focus_stack: Option<impl Iterator<Item = &'a Window> + 'a>,
    ) {
        let output = super::output_from_seat(seat, space);
        let idx = space
            .outputs()
            .position(|o| Some(o) == output.as_ref())
            .unwrap_or(0);
        let tree = TilingLayout::active_tree(&mut self.trees, idx);
        let new_window = Node::new(Data::Window(window.clone()));

        let last_active = focus_stack.and_then(|mut iter|
            iter.find_map(|window| tree.root_node_id()
                .and_then(|root| tree.traverse_pre_order_ids(root).unwrap()
                    .find(|id| matches!(tree.get(id).map(|n| n.data()), Ok(Data::Window(w)) if w == window))
                )
            )
        );
        let window_id = if let Some(ref node_id) = last_active {
            let parent_id = tree.get(node_id).unwrap().parent().cloned();
            if let Some(stack_id) =
                parent_id.filter(|id| matches!(tree.get(id).unwrap().data(), Data::Stack { .. }))
            {
                // we add to the stack
                let window_id = tree
                    .insert(new_window, InsertBehavior::UnderNode(&stack_id))
                    .unwrap();
                if let Data::Stack {
                    ref mut len,
                    ref mut active,
                } = tree.get_mut(&stack_id).unwrap().data_mut()
                {
                    *active = *len;
                    *len += 1;
                }
                Ok(window_id)
            } else {
                // we create a new fork
                TilingLayout::new_fork(tree, node_id, new_window)
            }
        } else {
            // nothing? then we add to the root
            if let Some(root_id) = tree.root_node_id().cloned() {
                TilingLayout::new_fork(tree, &root_id, new_window)
            } else {
                tree.insert(new_window, InsertBehavior::AsRoot)
            }
        }
        .unwrap();

        {
            let user_data = window.user_data();
            let window_info = WindowInfo {
                node: window_id.clone(),
                output: idx,
            };
            // insert or update
            if !user_data.insert_if_missing(|| RefCell::new(window_info.clone())) {
                *user_data.get::<RefCell<WindowInfo>>().unwrap().borrow_mut() = window_info;
            }
        }
    }

    fn unmap_window_internal(&mut self, window: &Window) {
        if let Some(info) = window.user_data().get::<RefCell<WindowInfo>>() {
            let output = info.borrow().output;
            let tree = TilingLayout::active_tree(&mut self.trees, output);

            let node_id = info.borrow().node.clone();
            let parent_id = tree
                .get(&node_id)
                .ok()
                .and_then(|node| node.parent())
                .cloned();
            let parent_parent_id = parent_id.as_ref().and_then(|parent_id| {
                tree.get(parent_id)
                    .ok()
                    .and_then(|node| node.parent())
                    .cloned()
            });

            // remove self
            slog_scope::debug!("Remove window {:?}", window);
            let _ = tree.remove_node(node_id.clone(), RemoveBehavior::DropChildren);

            // fixup parent node
            match parent_id {
                Some(id) if matches!(tree.get(&id).unwrap().data(), Data::Fork { .. }) => {
                    slog_scope::debug!("Removing Fork");
                    let other_child = tree.children_ids(&id).unwrap().cloned().next().unwrap();
                    let fork_pos = parent_parent_id.as_ref().and_then(|parent_id| {
                        tree.children_ids(parent_id).unwrap().position(|i| i == &id)
                    });
                    let _ = tree.remove_node(id.clone(), RemoveBehavior::OrphanChildren);
                    tree.move_node(
                        &other_child,
                        parent_parent_id
                            .as_ref()
                            .map(|parent_id| MoveBehavior::ToParent(parent_id))
                            .unwrap_or(MoveBehavior::ToRoot),
                    )
                    .unwrap();
                    if let Some(old_pos) = fork_pos {
                        tree.make_nth_sibling(&other_child, old_pos).unwrap();
                    }
                }
                Some(id) if matches!(tree.get(&id).unwrap().data(), Data::Stack { .. }) => {
                    if tree.children_ids(&id).unwrap().count() == 0 {
                        slog_scope::debug!("Removing Stack");
                        // remove stack
                        let _ = tree.remove_node(id.clone(), RemoveBehavior::DropChildren);
                        // TODO now we need to untangle the parent_parent
                        // So we should REFACTOR this unmap function to not only work with windows
                    } else {
                        // fixup stack values
                        if let Data::Stack {
                            ref mut active,
                            ref mut len,
                        } = tree.get_mut(&id).unwrap().data_mut()
                        {
                            *len -= 1;
                            *active = std::cmp::max(*active, *len - 1);
                        }
                    }
                }
                None => {} // root
                _ => unreachable!(),
            }
        }
    }

    fn new_fork(
        tree: &mut Tree<Data>,
        old_id: &NodeId,
        new: Node<Data>,
    ) -> Result<NodeId, NodeIdError> {
        let new_fork = Node::new(Data::fork());
        let old = tree.get(old_id)?;
        let parent_id = old.parent().cloned();
        let pos = parent_id.as_ref().and_then(|parent_id| {
            tree.children_ids(parent_id)
                .unwrap()
                .position(|id| id == old_id)
        });

        let fork_id = tree
            .insert(
                new_fork,
                if let Some(parent) = parent_id.as_ref() {
                    InsertBehavior::UnderNode(parent)
                } else {
                    InsertBehavior::AsRoot
                },
            )
            .unwrap();

        tree.move_node(old_id, MoveBehavior::ToParent(&fork_id))
            .unwrap();
        // keep position
        if let Some(old_pos) = pos {
            tree.make_nth_sibling(&fork_id, old_pos).unwrap();
        }
        tree.insert(new, InsertBehavior::UnderNode(&fork_id))
    }

    fn update_space_positions(
        trees: &mut Vec<Tree<Data>>,
        space: &mut Space,
        gaps: (i32, i32),
    ) -> Vec<Window> {
        let mut dead_windows = Vec::new();
        let (outer, inner) = gaps;
        for (idx, output) in space
            .outputs()
            .cloned()
            .enumerate()
            .collect::<Vec<_>>()
            .into_iter()
        {
            let tree = TilingLayout::active_tree(trees, idx);
            if let Some(root) = tree.root_node_id() {
                let mut stack = Vec::new();

                let mut geo = Some(layer_map_for_output(&output).non_exclusive_zone());
                // TODO saturate? minimum?
                if let Some(mut geo) = geo.as_mut() {
                    geo.loc.x += outer;
                    geo.loc.y += outer;
                    geo.size.w -= outer * 2;
                    geo.size.h -= outer * 2;
                }

                for node in tree.traverse_pre_order(root).unwrap() {
                    let geo = stack.pop().unwrap_or(geo);
                    match node.data() {
                        Data::Fork { orientation, ratio } => {
                            if let Some(geo) = geo {
                                match orientation {
                                    Orientation::Horizontal => {
                                        let top_size = (
                                            geo.size.w,
                                            ((geo.size.h as f64) * ratio.load(Ordering::SeqCst))
                                                .ceil()
                                                as i32,
                                        );
                                        stack.push(Some(Rectangle::from_loc_and_size(
                                            (geo.loc.x, geo.loc.y + top_size.1),
                                            (geo.size.w, geo.size.h - top_size.1),
                                        )));
                                        stack.push(Some(Rectangle::from_loc_and_size(
                                            geo.loc, top_size,
                                        )));
                                    }
                                    Orientation::Vertical => {
                                        let left_size = (
                                            ((geo.size.w as f64) * ratio.load(Ordering::SeqCst))
                                                .ceil()
                                                as i32,
                                            geo.size.h,
                                        );
                                        stack.push(Some(Rectangle::from_loc_and_size(
                                            (geo.loc.x + left_size.0, geo.loc.y),
                                            (geo.size.w - left_size.0, geo.size.h),
                                        )));
                                        stack.push(Some(Rectangle::from_loc_and_size(
                                            geo.loc, left_size,
                                        )));
                                    }
                                }
                            } else {
                                stack.push(None);
                                stack.push(None);
                            }
                        }
                        Data::Stack { active, len } => {
                            for i in 0..*len {
                                if i == *active {
                                    stack.push(geo);
                                } else {
                                    stack.push(None);
                                }
                            }
                        }
                        Data::Window(window) => {
                            if window.alive() {
                                if let Some(geo) = geo {
                                    #[allow(irrefutable_let_patterns)]
                                    if let Kind::Xdg(xdg) = &window.toplevel() {
                                        if xdg.current_state().states.contains(XdgState::Fullscreen)
                                            || xdg.with_pending_state(|pending| {
                                                pending.states.contains(XdgState::Fullscreen)
                                            })
                                        {
                                            continue;
                                        }
                                        xdg.with_pending_state(|state| {
                                            state.size = Some(
                                                (geo.size.w - inner * 2, geo.size.h - inner * 2)
                                                    .into(),
                                            );
                                            state.states.set(XdgState::TiledLeft);
                                            state.states.set(XdgState::TiledRight);
                                            state.states.set(XdgState::TiledTop);
                                            state.states.set(XdgState::TiledBottom);
                                        });
                                        xdg.send_configure();
                                    }
                                    space.map_window(
                                        &window,
                                        (geo.loc.x + inner, geo.loc.y + inner),
                                        None,
                                        false,
                                    );
                                }
                            } else {
                                dead_windows.push(window.clone());
                            }
                        }
                    }
                }
            }
        }
        dead_windows
    }
}
