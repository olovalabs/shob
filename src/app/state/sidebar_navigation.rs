fn sidebar_context_key(stroke: &gpui::Keystroke) -> bool {
    !stroke.modifiers.alt && !stroke.modifiers.control && !stroke.modifiers.platform
        && (matches!(stroke.key.as_str(), "context_menu" | "menu")
            || stroke.key == "f10" && stroke.modifiers.shift)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SidebarNavKey {
    Thread(usize),
    Folder(usize),
    Shelf(&'static str),
    More(Option<usize>),
}

struct SidebarNavRow {
    focus: gpui::FocusHandle,
    bounds: Rc<std::cell::Cell<Bounds<Pixels>>>,
    parent: Option<SidebarNavKey>,
    frame: u64,
}

#[derive(Default)]
struct SidebarNavigation {
    rows: HashMap<SidebarNavKey, SidebarNavRow>,
    order: Vec<SidebarNavKey>,
    frame: u64,
    search: bool,
}

#[derive(Default)]
struct SidebarSelection {
    members: Rc<HashSet<usize>>,
    anchor: Option<usize>,
}

impl SidebarSelection {
    fn toggle(&mut self, id: usize, open: Option<usize>) {
        let members = Rc::make_mut(&mut self.members);
        if members.is_empty() && let Some(open) = open.filter(|open| *open != id) {
            members.insert(open);
        }
        if !members.remove(&id) {
            members.insert(id);
            self.anchor = Some(id);
        }
    }

    fn select_range(&mut self, id: usize, open: Option<usize>, order: &[SidebarNavKey]) {
        let anchor = if self.members.is_empty() { open } else { self.anchor };
        let members = Rc::make_mut(&mut self.members);
        let target = order.iter().position(|key| *key == SidebarNavKey::Thread(id));
        let start = anchor.and_then(|anchor| {
            order.iter().position(|key| *key == SidebarNavKey::Thread(anchor))
        });
        if let (Some(start), Some(target)) = (start, target) {
            members.extend(order[start.min(target)..=start.max(target)].iter().filter_map(|key| {
                if let SidebarNavKey::Thread(id) = key { Some(*id) } else { None }
            }));
            self.anchor = anchor;
        } else {
            members.insert(id);
            self.anchor = Some(id);
        }
    }
}

impl AppState {
    fn sidebar_navigation_begin(&self, search: bool) {
        let mut navigation = self.sidebar_navigation.borrow_mut();
        navigation.frame = navigation.frame.wrapping_add(1);
        navigation.order.clear();
        navigation.search = search;
    }

    fn sidebar_navigation_finish(&self) {
        let mut navigation = self.sidebar_navigation.borrow_mut();
        let frame = navigation.frame;
        navigation.rows.retain(|_, row| row.frame == frame);
    }

    fn sidebar_nav_row(
        &self,
        element: gpui::Stateful<Div>,
        key: SidebarNavKey,
        parent: Option<SidebarNavKey>,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<Div> {
        let (focus, bounds) = {
            let mut navigation = self.sidebar_navigation.borrow_mut();
            navigation.order.push(key);
            let frame = navigation.frame;
            let row = navigation.rows.entry(key).or_insert_with(|| SidebarNavRow {
                focus: cx.focus_handle(),
                bounds: Rc::default(),
                parent,
                frame,
            });
            row.parent = parent;
            row.frame = frame;
            (row.focus.clone(), row.bounds.clone())
        };
        element
            .track_focus(&focus)
            .tab_index(0)
            .relative()
            .child(canvas(move |bounds_now, _, _| bounds.set(bounds_now), |_, _, _, _| {}).absolute().inset_0())
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                if focus.is_focused(window) && !event.keystroke.modifiers.modified()
                    && this.navigate_sidebar_row(key, &event.keystroke.key, window, cx)
                {
                    cx.stop_propagation();
                }
            }))
    }

    fn sidebar_row_bounds(&self, key: SidebarNavKey) -> Option<Bounds<Pixels>> {
        self.sidebar_navigation.borrow().rows.get(&key).map(|row| row.bounds.get())
    }

    fn focus_sidebar_row(&self, target: SidebarNavKey, window: &mut Window, cx: &mut App) {
        let navigation = self.sidebar_navigation.borrow();
        let Some(row) = navigation.rows.get(&target) else { return };
        window.focus(&row.focus, cx);
        let scroll = if navigation.search {
            self.sidebar_search.as_ref().map(|search| &search.scroll)
        } else {
            Some(&self.sidebar_inbox_scroll)
        };
        if let Some(scroll) = scroll {
            let viewport = scroll.bounds();
            let bounds = row.bounds.get();
            let dy = if bounds.top() < viewport.top() {
                viewport.top() - bounds.top()
            } else if bounds.bottom() > viewport.bottom() {
                viewport.bottom() - bounds.bottom()
            } else {
                px(0.)
            };
            if dy != px(0.) {
                let mut offset = scroll.offset();
                offset.y = (offset.y + dy).min(px(0.));
                scroll.set_offset(offset);
            }
        }
    }

    fn navigate_sidebar_row(
        &mut self,
        key: SidebarNavKey,
        stroke: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let expand = match key {
            SidebarNavKey::Folder(id) => self.projects.iter().find(|p| p.id == id).map(|p| p.expanded),
            SidebarNavKey::Shelf("Working") => Some(self.settings.sidebar.working_expanded),
            SidebarNavKey::Shelf("Snoozed") => Some(self.settings.sidebar.snoozed_expanded),
            SidebarNavKey::Shelf("Settled") => Some(self.settings.sidebar.settled_expanded),
            _ => None,
        };
        if matches!((stroke, expand), ("left", Some(true)) | ("right", Some(false))) {
            match key {
                SidebarNavKey::Folder(id) => self.toggle_sidebar_folder(id, cx),
                SidebarNavKey::Shelf("Working") => self.toggle_sidebar_working(cx),
                SidebarNavKey::Shelf("Snoozed") => self.toggle_sidebar_shelf(SidebarShelf::Snoozed, cx),
                SidebarNavKey::Shelf("Settled") => self.toggle_sidebar_shelf(SidebarShelf::Settled, cx),
                _ => {}
            }
            return true;
        }
        let target = {
            let navigation = self.sidebar_navigation.borrow();
            let order = &navigation.order;
            let Some(index) = order.iter().position(|candidate| *candidate == key) else { return false };
            match stroke {
                "home" => order.first().copied(),
                "end" => order.last().copied(),
                "up" => order.get(index.saturating_sub(1)).copied(),
                "down" => order.get((index + 1).min(order.len() - 1)).copied(),
                "left" => navigation.rows.get(&key).and_then(|row| row.parent),
                "right" => order[index + 1..].iter().copied().find(|candidate| {
                    navigation.rows.get(candidate).and_then(|row| row.parent) == Some(key)
                }),
                _ => return false,
            }
        };
        if let Some(target) = target {
            self.focus_sidebar_row(target, window, cx);
        }
        true
    }

    fn select_sidebar_thread(&mut self, id: usize, modifiers: gpui::Modifiers, cx: &mut Context<Self>) -> bool {
        let toggle = if cfg!(target_os = "macos") { modifiers.platform } else { modifiers.control };
        if !toggle && !modifiers.shift {
            self.sidebar_selection = SidebarSelection::default();
            return false;
        }
        let navigation = self.sidebar_navigation.borrow();
        let open = self.active_session().map(|session| session.id).filter(|&id| {
            navigation.rows.contains_key(&SidebarNavKey::Thread(id))
        });
        if toggle {
            self.sidebar_selection.toggle(id, open);
        } else {
            self.sidebar_selection.select_range(id, open, &navigation.order);
        }
        cx.notify();
        true
    }

    fn sidebar_action_selection(&self, origin: usize, members: &HashSet<usize>) -> Vec<usize> {
        if !members.contains(&origin) {
            return vec![origin];
        }
        self.sidebar_navigation.borrow().order.iter().filter_map(|key| match key {
            SidebarNavKey::Thread(id) if members.contains(id) => Some(*id),
            _ => None,
        }).collect()
    }

    fn toggle_sidebar_working(&mut self, cx: &mut Context<Self>) {
        self.settings.sidebar.working_expanded = !self.settings.sidebar.working_expanded;
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn sidebar_bulk_action(
        &mut self,
        origin: usize,
        cx: &mut Context<Self>,
        mut apply: impl FnMut(&mut Self, usize, usize, &mut Context<Self>),
    ) {
        for id in self.sidebar_action_selection(origin, &self.sidebar_selection.members) {
            if let Some(&(project, session)) = self.session_index.get(&id) {
                apply(self, project, session, cx);
            }
        }
    }

    fn sidebar_keyboard_command(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let stroke = &event.keystroke;
        if stroke.key == "f6" && !stroke.modifiers.alt {
            if self.sidebar_focus.contains_focused(window, cx) {
                if let Some(pane) = self.focused_pane_id {
                    self.focus_workspace_pane(pane, window, cx);
                }
            } else {
                self.open_sidebar_search(window, cx);
            }
            return true;
        }
        let modified = if cfg!(target_os = "macos") {
            stroke.modifiers.platform
        } else {
            stroke.modifiers.control
        };
        if !modified || stroke.modifiers.alt {
            return false;
        }
        let key = stroke.key.as_str();
        if !matches!(key, "[" | "]") && !(key == "s" && stroke.modifiers.shift) {
            return false;
        }
        if self.focused_pane_id.and_then(|id| self.pane_ref(id))
            .is_some_and(|pane| pane.terminal.read(cx).focus_handle().is_focused(window))
        {
            return false;
        }
        if key == "s" {
            if let Some((project, session)) = self.sidebar_active_thread_key() {
                self.settle_sidebar_thread(project, session, cx);
            }
        } else if stroke.modifiers.shift {
            let mut ids: Vec<_> = self.sidebar_navigation.borrow().order.iter().filter_map(|key| match key {
                SidebarNavKey::Thread(id) => Some(*id),
                _ => None,
            }).collect();
            if key == "[" { ids.reverse(); }
            let id = self.active_session().and_then(|current| ids.iter().position(|id| *id == current.id))
                .and_then(|index| ids.get(index + 1)).or_else(|| ids.first());
            if let Some(&(project, session)) = id.and_then(|id| self.session_index.get(id)) {
                self.select_session(project, session, window, cx);
            }
        } else if let Some((position, project, session)) = self.navigation_target(key == "[") {
            self.navigation_position = position;
            self.select_session(project, session, window, cx);
        }
        true
    }
}
