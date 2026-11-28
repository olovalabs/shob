const SIDEBAR_FOLDER_PREVIEW: usize = 6;

#[derive(Clone)]
struct SidebarProjectDrag {
    id: usize,
    label: SharedString,
    tokens: ThemeTokens,
}

impl Render for SidebarProjectDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .max_w(SIDEBAR_MIN_WIDTH)
            .px(px(10.))
            .py(px(4.))
            .rounded(px(6.))
            .border_1()
            .border_color(self.tokens.sidebar_border)
            .bg(self.tokens.sidebar_bg)
            .text_color(self.tokens.sidebar_fg)
            .text_sm()
            .truncate()
            .child(self.label.clone())
    }
}

impl AppState {
    fn sidebar_folder_order(&self) -> Vec<usize> {
        let sidebar = &self.settings.sidebar;
        let mut order: Vec<_> = (0..self.projects.len()).collect();
        order.sort_by_cached_key(|&index| {
            let path = &self.projects[index].path;
            (
                !sidebar.pinned_projects.contains(path),
                sidebar.project_order.iter().position(|key| key == path).unwrap_or(usize::MAX),
            )
        });
        order
    }

    fn toggle_sidebar_folder(&mut self, id: usize, cx: &mut Context<Self>) {
        if let Some(index) = self.projects.iter().position(|project| project.id == id) {
            self.toggle_project_expanded(index, cx);
        }
    }

    fn move_sidebar_folder(&mut self, source: usize, target: usize, before: bool, cx: &mut Context<Self>) {
        if source == target {
            return;
        }
        let mut order = self.sidebar_folder_order();
        let Some(from) = order.iter().position(|&index| self.projects[index].id == source) else { return };
        let moved = order.remove(from);
        let Some(to) = order.iter().position(|&index| self.projects[index].id == target) else { return };
        let target_pinned = self.settings.sidebar.pinned_projects.contains(&self.projects[order[to]].path);
        let pinned = &mut self.settings.sidebar.pinned_projects;
        let path = &self.projects[moved].path;
        if target_pinned && !pinned.contains(path) {
            pinned.push(path.clone());
        } else if !target_pinned {
            pinned.retain(|key| key != path);
        }
        order.insert(to + usize::from(!before), moved);
        self.settings.sidebar.project_order = order.into_iter().map(|index| self.projects[index].path.clone()).collect();
        self.schedule_workspace_save(cx);
        cx.notify();
    }

    fn render_sidebar_project_folders(
        &self,
        rows: &[SidebarThreadRef],
        active: Option<(usize, usize)>,
        now: u64,
        tokens: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows_by_project: HashMap<usize, Vec<SidebarThreadRef>> = HashMap::new();
        for &row in rows {
            rows_by_project.entry(row.project_idx).or_default().push(row);
        }
        let scope = self.sidebar_scope_indices();
        let mut folders = Vec::new();
        for index in self.sidebar_folder_order() {
            let rows = rows_by_project.remove(&index).unwrap_or_default();
            if !scope.is_empty() && !scope.contains(&index) && rows.is_empty() {
                continue;
            }
            let project = &self.projects[index];
            let id = project.id;
            let key = SidebarNavKey::Folder(id);
            let show_all = self.sidebar_folder_show_all.contains(&id);
            let shown = if show_all { rows.len() } else { rows.len().min(SIDEBAR_FOLDER_PREVIEW) };
            let hidden = if project.expanded { rows.len() - shown } else { rows.len() };
            let pinned = self.settings.sidebar.pinned_projects.contains(&project.path);
            let renaming = self.renaming_project.as_ref().filter(|rename| rename.project_id == id);
            let action = sidebar_icon_action("sidebar-folder-new", "icons/square-pen.svg", "New thread", false, tokens)
                .opacity(0.)
                .focus_visible(|action| action.opacity(1.))
                .group_hover("sidebar-folder", |action| action.opacity(1.))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    if let Some(index) = this.projects.iter().position(|project| project.id == id) {
                        this.create_new_sidebar_session(Some(index), window, cx);
                    }
                }));
            let header = div()
                .id(("sidebar-project-folder", id))
                .debug_selector(move || format!("sidebar-project-folder-{id}"))
                .role(gpui::Role::Button)
                .aria_label(project.name.clone())
                .aria_expanded(project.expanded)
                .group("sidebar-folder")
                .mt(px(8.))
                .h(px(32.))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(10.))
                .rounded(px(6.))
                .cursor_pointer()
                .hover(move |row| row.bg(tokens.sidebar_item_hover))
                .focus_visible(move |row| row.bg(tokens.sidebar_item_hover))
                .child(project_favicon(project, tokens, px(16.)))
                .child(div().flex_1().min_w_0().truncate().text_sm().font_weight(gpui::FontWeight::MEDIUM)
                    .child(if let Some(rename) = renaming {
                        Input::new(&rename.input).appearance(false).into_any_element()
                    } else {
                        project.name.clone().into_any_element()
                    }))
                .when(pinned, |row| row.child(svg().path("ui_icons/pin.svg").size(px(12.)).text_color(tokens.sidebar_icon)))
                .child(div().text_xs().text_color(tokens.sidebar_muted)
                    .group_hover("sidebar-folder", |count| count.opacity(0.))
                    .when(hidden > 0, |count| count.child(hidden.to_string())))
                .child(self.sidebar_tooltip(action, ("new-in-folder", id), format!("New thread in {}", project.name), SidebarTooltipSide::Top, tokens, cx))
                .child(svg().path(if project.expanded { "icons/chevron-down.svg" } else { "icons/chevron-right.svg" })
                    .size(px(14.)).text_color(tokens.sidebar_icon))
                .when(renaming.is_none(), |header| {
                    header
                        .on_click(cx.listener(move |this, _, _, cx| this.toggle_sidebar_folder(id, cx)))
                        .on_mouse_down(MouseButton::Right, cx.listener(move |this, event: &MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.open_sidebar_folder_menu(id, event.position, window, cx);
                        }))
                        .on_key_up(cx.listener(move |this, event: &gpui::KeyUpEvent, window, cx| {
                            if sidebar_context_key(&event.keystroke) {
                                let pos = this.sidebar_row_bounds(key).map(|bounds| bounds.center()).unwrap_or_else(|| window.mouse_position());
                                this.open_sidebar_folder_menu(id, pos, window, cx);
                                cx.stop_propagation();
                            }
                        }))
                        .on_drag(SidebarProjectDrag { id, label: project.name.clone(), tokens }, |drag, _, _, cx| cx.new(|_| drag.clone()))
                        .drag_over::<SidebarProjectDrag>(move |row, drag, _, _| {
                            if drag.id == id { row } else { row.bg(tokens.sidebar_item_active) }
                        })
                        .on_drop::<SidebarProjectDrag>(cx.listener(move |this, drag: &SidebarProjectDrag, window, cx| {
                            let before = this.sidebar_row_bounds(key).is_some_and(|bounds| window.mouse_position().y < bounds.center().y);
                            this.move_sidebar_folder(drag.id, id, before, cx);
                        }))
                });
            folders.push(self.sidebar_nav_row(header, key, None, cx).into_any_element());
            for (offset, row) in rows.iter().copied().enumerate() {
                if project.expanded && (row.revealed_active || offset < shown) {
                    let item = self.render_sidebar_folder_thread(row, active, now, tokens, cx);
                    folders.push(self.sidebar_motion_row(item, self.sidebar_thread_id(row)));
                }
            }
            let more = rows.iter().skip(SIDEBAR_FOLDER_PREVIEW).filter(|row| !row.revealed_active).count();
            if project.expanded && (more > 0 || show_all) {
                let button = div().id(("sidebar-folder-more", id)).role(gpui::Role::Button).h(px(30.)).flex_none()
                    .flex().items_center().pl(px(36.)).rounded(px(6.)).text_xs().text_color(tokens.sidebar_muted)
                    .cursor_pointer().hover(move |row| row.bg(tokens.sidebar_item_hover))
                    .focus_visible(move |row| row.bg(tokens.sidebar_item_hover))
                    .child(if show_all { "Show less".into() } else { format!("Show {more} more") })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.sidebar_folder_show_all.remove(&id) {
                            this.sidebar_folder_show_all.insert(id);
                        }
                        cx.notify();
                    }));
                folders.push(self.sidebar_nav_row(button, SidebarNavKey::More(Some(id)), Some(key), cx).into_any_element());
            }
        }
        folders
    }

    fn open_sidebar_folder_menu(&mut self, id: usize, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_thread_menu = None;
        self.sidebar_folder_menu = Some((id, position));
        window.focus(&self.sidebar_menu_focus, cx);
        cx.notify();
    }

    fn render_sidebar_folder_menu(&self, tokens: ThemeTokens, cx: &mut Context<Self>) -> AnyElement {
        let Some((id, position)) = self.sidebar_folder_menu else { return div().into_any_element() };
        let Some(project) = self.projects.iter().find(|project| project.id == id) else { return div().into_any_element() };
        let new = self.sidebar_thread_menu_row("sidebar-folder-menu-new", "New thread", "icons/square-pen.svg", tokens,
            move |this, window, cx| {
                if let Some(index) = this.projects.iter().position(|project| project.id == id) {
                    this.create_new_sidebar_session(Some(index), window, cx);
                }
            }, cx).track_focus(&self.sidebar_menu_focus);
        let panel = self.sidebar_context_panel("sidebar-folder-menu", "Project actions", tokens, cx)
            .child(new);
        if project.projectless {
            return gpui::anchored().position(position).snap_to_window_with_margin(px(8.)).child(panel).into_any_element();
        }
        let pinned = self.settings.sidebar.pinned_projects.contains(&project.path);
        let path = project.path.clone();
        let copy = self.sidebar_thread_menu_row("sidebar-folder-menu-copy", "Copy path", "icons/copy.svg", tokens,
            move |_, _, cx| cx.write_to_clipboard(gpui::ClipboardItem::new_string(path.to_string_lossy().into_owned())), cx);
        let pin = self.sidebar_thread_menu_row("sidebar-folder-menu-pin", if pinned { "Unpin project" } else { "Pin project" }, "ui_icons/pin.svg", tokens,
            move |this, _, cx| {
                if let Some(project) = this.projects.iter().find(|project| project.id == id) {
                    let pinned = &mut this.settings.sidebar.pinned_projects;
                    if let Some(index) = pinned.iter().position(|key| key == &project.path) {
                        pinned.remove(index);
                    } else {
                        pinned.push(project.path.clone());
                    }
                    this.schedule_workspace_save(cx);
                    cx.notify();
                }
            }, cx);
        let rename = self.sidebar_thread_menu_row("sidebar-folder-menu-rename", "Rename project", "icons/pencil.svg", tokens,
            move |this, window, cx| this.start_renaming_project(id, window, cx), cx);
        let panel = panel.child(copy).child(pin).child(rename);
        gpui::anchored().position(position).snap_to_window_with_margin(px(8.)).child(panel).into_any_element()
    }
}
