//! Transient UI: navigators, the sidebar, pane menus, prompts, and status notices.

// clippy exempts wildcard imports from test builds, so the expectation is compiled in only
// where the lint can fire; a bare `expect` would be unfulfilled under `--cfg test`.
#![cfg_attr(
    not(test),
    expect(
        clippy::wildcard_imports,
        reason = "a part of the session actor, sharing its module's private vocabulary"
    )
)]

use super::*;

impl SessionActor {
    pub(super) fn transient_ui_active(&self) -> bool {
        self.agent_navigator.is_some()
            || self.tab_navigator.is_some()
            || self.pane_menu.is_some()
            || self.tab_rename.is_some()
            || self.close_pane_confirmation.is_some()
            || self.save_layout_prompt.is_some()
    }

    pub(super) fn clear_transient_ui(&mut self) -> bool {
        let active = self.transient_ui_active();
        self.agent_navigator = None;
        self.tab_navigator = None;
        self.pane_menu = None;
        self.tab_rename = None;
        self.close_pane_confirmation = None;
        self.save_layout_prompt = None;
        active
    }

    pub(super) fn agent_navigator_rows(&self) -> Vec<AgentNavigatorRow> {
        let mut rows = Vec::new();
        for (tab_index, tab) in self.tabs.iter().enumerate() {
            let tab_label = tab.name.as_ref().map_or_else(
                || format!("tab {}", tab_index + 1),
                |name| format!("tab {} {name}", tab_index + 1),
            );
            let mut pane_ids = tab.tree.as_ref().map_or_else(Vec::new, TiledNode::pane_ids);
            pane_ids.extend(tab.floating.pane_ids());
            pane_ids.sort_unstable();
            pane_ids.dedup();
            for pane_id in pane_ids {
                let Some(pane) = self.panes.get(&pane_id) else {
                    continue;
                };
                let Some(agent) = pane.agent.snapshot() else {
                    continue;
                };
                let metadata = pane.agent.metadata();
                rows.push(AgentNavigatorRow {
                    pane_id,
                    tab_index,
                    tab_label: tab_label.clone(),
                    // A reported title names what the agent is doing, which is more useful here
                    // than whatever the program last set as the terminal title.
                    title: metadata.title().map_or_else(
                        || {
                            pane.terminal
                                .title()
                                .map_or_else(|| format!("pane {pane_id}"), ToOwned::to_owned)
                        },
                        ToOwned::to_owned,
                    ),
                    // Most specific name wins: the one the user chose, then the one the integration
                    // reported, then the provider's. A user who named an agent `reviewer` is
                    // looking for `reviewer` in this list.
                    label: pane
                        .agent
                        .alias()
                        .map(crate::agent::AgentAlias::as_str)
                        .or_else(|| metadata.display_agent())
                        .unwrap_or(agent.label.as_str())
                        .to_owned(),
                    status_label: metadata
                        .state_label(agent.status)
                        .unwrap_or(agent.status.label())
                        .to_owned(),
                    detail: agent_row_detail(agent.message.as_deref(), metadata),
                    agent,
                });
            }
        }
        rows.sort_by_key(|row| (row.agent.status.urgency(), row.tab_index, row.pane_id));
        rows
    }

    pub(super) fn toggle_agent_navigator(&mut self) {
        if self.agent_navigator.take().is_some() {
            self.schedule_render();
            return;
        }
        self.clear_transient_ui();
        let rows = self.agent_navigator_rows();
        let focused = self.active_tab().map(|tab| tab.focused);
        let selected = focused
            .filter(|pane| rows.iter().any(|row| row.pane_id == *pane))
            .or_else(|| rows.first().map(|row| row.pane_id));
        let selected_index = selected
            .and_then(|pane| rows.iter().position(|row| row.pane_id == pane))
            .unwrap_or(0);
        self.agent_navigator = Some(AgentNavigator {
            selected,
            selected_index,
            scroll: 0,
        });
        self.force_full = true;
        self.schedule_render();
    }

    pub(super) fn agent_navigator_input(&mut self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() && self.agent_navigator.is_some() {
            let (consumed, key) = decode_agent_navigator_key(&bytes[offset..]);
            offset += consumed;
            match key {
                Some(AgentNavigatorKey::Up) => self.move_agent_navigator(-1),
                Some(AgentNavigatorKey::Down) => self.move_agent_navigator(1),
                Some(AgentNavigatorKey::Home) => self.move_agent_navigator_to(false),
                Some(AgentNavigatorKey::End) => self.move_agent_navigator_to(true),
                Some(AgentNavigatorKey::PageUp) => self.page_agent_navigator(false),
                Some(AgentNavigatorKey::PageDown) => self.page_agent_navigator(true),
                Some(AgentNavigatorKey::Activate) => self.activate_agent_navigator(),
                Some(AgentNavigatorKey::Close) => {
                    self.agent_navigator = None;
                    self.force_full = true;
                    self.schedule_render();
                }
                None => {}
            }
        }
    }

    pub(super) fn move_agent_navigator(&mut self, delta: isize) {
        let rows = self.agent_navigator_rows();
        if rows.is_empty() {
            return;
        }
        let current = self
            .agent_navigator
            .and_then(|navigator| navigator.selected)
            .and_then(|pane| rows.iter().position(|row| row.pane_id == pane))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(rows.len().saturating_sub(1));
        if let Some(navigator) = &mut self.agent_navigator {
            navigator.selected = Some(rows[next].pane_id);
            navigator.selected_index = next;
        }
        self.reveal_agent_navigator_selection(rows.len(), next);
    }

    pub(super) fn move_agent_navigator_to(&mut self, end: bool) {
        let rows = self.agent_navigator_rows();
        let Some(index) = (!rows.is_empty()).then(|| if end { rows.len() - 1 } else { 0 }) else {
            return;
        };
        if let Some(navigator) = &mut self.agent_navigator {
            navigator.selected = Some(rows[index].pane_id);
            navigator.selected_index = index;
        }
        self.reveal_agent_navigator_selection(rows.len(), index);
    }

    pub(super) fn page_agent_navigator(&mut self, down: bool) {
        let page = agent_navigator_rect(self.content_area(), self.agent_navigator_rows().len())
            .map_or(1, |rect| usize::from(rect.height.saturating_sub(2)).max(1));
        self.move_agent_navigator(if down {
            page as isize
        } else {
            -(page as isize)
        });
    }

    pub(super) fn reveal_agent_navigator_selection(&mut self, row_count: usize, index: usize) {
        let page = agent_navigator_rect(self.content_area(), row_count)
            .map_or(1, |rect| usize::from(rect.height.saturating_sub(2)).max(1));
        if let Some(navigator) = &mut self.agent_navigator {
            if index < navigator.scroll {
                navigator.scroll = index;
            } else if index >= navigator.scroll.saturating_add(page) {
                navigator.scroll = index + 1 - page;
            }
            navigator.scroll = navigator.scroll.min(row_count.saturating_sub(page));
        }
        self.schedule_render();
    }

    pub(super) fn activate_agent_navigator(&mut self) {
        let selected = self
            .agent_navigator
            .and_then(|navigator| navigator.selected);
        self.agent_navigator = None;
        let Some(pane_id) = selected else {
            self.force_full = true;
            self.schedule_render();
            return;
        };
        if let Some(pane) = self.panes.get_mut(&pane_id) {
            pane.agent.mark_seen();
        }
        // Acknowledging a finished agent turns `done` back into `idle`, which is a status
        // transition like any other and must be sequenced, not just repainted.
        self.sync_agent_status();
        if let Err(error) = self.automation_focus(pane_id) {
            self.status(&error.message);
        }
        self.force_full = true;
        self.schedule_render();
    }

    pub(super) fn agent_navigator_mouse(&mut self, mouse: MouseEvent) {
        let rows = self.agent_navigator_rows();
        let Some(rect) = agent_navigator_rect(self.content_area(), rows.len()) else {
            self.agent_navigator = None;
            return;
        };
        if mouse.kind == MouseKind::Wheel {
            self.move_agent_navigator(if mouse.button == 0 { -1 } else { 1 });
            return;
        }
        if mouse.kind != MouseKind::Press || mouse.button != 0 {
            return;
        }
        if !rect.contains(mouse.x, mouse.y) {
            self.agent_navigator = None;
            self.force_full = true;
            self.schedule_render();
            return;
        }
        if mouse.y <= rect.y || mouse.y + 1 >= rect.y + rect.height {
            return;
        }
        let scroll = self.agent_navigator.map_or(0, |navigator| navigator.scroll);
        let index = scroll + usize::from(mouse.y - rect.y - 1);
        let Some(row) = rows.get(index) else { return };
        if let Some(navigator) = &mut self.agent_navigator {
            navigator.selected = Some(row.pane_id);
            navigator.selected_index = index;
        }
        self.activate_agent_navigator();
    }

    pub(super) fn draw_agent_navigator(
        &mut self,
        screen: &mut ScreenBuffer,
        theme: crate::theme::ResolvedTheme,
    ) {
        let rows = self.agent_navigator_rows();
        let Some(rect) = agent_navigator_rect(self.content_area(), rows.len()) else {
            self.agent_navigator = None;
            return;
        };
        let page = usize::from(rect.height.saturating_sub(2)).max(1);
        let selected_index = self.agent_navigator.and_then(|navigator| {
            navigator
                .selected
                .and_then(|pane| rows.iter().position(|row| row.pane_id == pane))
                .or_else(|| {
                    (!rows.is_empty()).then_some(navigator.selected_index.min(rows.len() - 1))
                })
        });
        if let Some(navigator) = &mut self.agent_navigator {
            navigator.selected = selected_index.map(|index| rows[index].pane_id);
            navigator.selected_index = selected_index.unwrap_or(0);
            navigator.scroll = navigator.scroll.min(rows.len().saturating_sub(page));
            if let Some(index) = selected_index {
                if index < navigator.scroll {
                    navigator.scroll = index;
                } else if index >= navigator.scroll + page {
                    navigator.scroll = index + 1 - page;
                }
            }
        }
        screen.draw_frame(rect, " Agents ", theme.frame(true));
        let style = theme.status();
        let inner_width = usize::from(rect.width.saturating_sub(2));
        let blank = " ".repeat(inner_width);
        for offset in 0..page {
            let y = rect.y + 1 + offset as u16;
            screen.draw_text(rect.x + 1, y, &blank, style);
        }
        if rows.is_empty() {
            screen.draw_text(rect.x + 2, rect.y + 1, "No detected AI agent panes", style);
        } else {
            let scroll = self.agent_navigator.map_or(0, |navigator| navigator.scroll);
            for (offset, row) in rows.iter().skip(scroll).take(page).enumerate() {
                let mut text = format!(
                    "[{:<7}] {:<8} {} · pane {} · {}",
                    single_line(&row.status_label).to_ascii_uppercase(),
                    single_line(&row.label),
                    single_line(&row.tab_label),
                    row.pane_id,
                    single_line(&row.title),
                );
                if !row.detail.is_empty() {
                    text.push_str(" · ");
                    text.push_str(&single_line(&row.detail));
                }
                let y = rect.y + 1 + offset as u16;
                screen.draw_text(rect.x + 1, y, &text, style);
                if self
                    .agent_navigator
                    .and_then(|navigator| navigator.selected)
                    == Some(row.pane_id)
                {
                    screen.invert(rect.x + 1, y, rect.width.saturating_sub(2));
                }
            }
        }
        screen.cursor = None;
    }

    pub(super) fn tab_navigator_rows(&self) -> Vec<TabNavigatorRow> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(display_index, tab)| {
                let tiled = tab.tree.as_ref().map_or(0, |tree| tree.pane_ids().len());
                TabNavigatorRow {
                    tab_id: tab.id,
                    display_index,
                    name: tab.name.clone(),
                    pane_count: tiled + tab.floating.pane_ids().len(),
                    active: display_index == self.active_tab,
                }
            })
            .collect()
    }

    pub(super) fn toggle_tab_navigator(&mut self) {
        if self.tab_navigator.take().is_some() {
            self.force_full = true;
            self.schedule_render();
            return;
        }
        self.clear_transient_ui();
        let selected = self.active_tab().map(|tab| tab.id);
        self.tab_navigator = Some(TabNavigator {
            selected,
            selected_index: self.active_tab,
            scroll: 0,
        });
        self.force_full = true;
        self.schedule_render();
    }

    pub(super) fn tab_navigator_input(&mut self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() && self.tab_navigator.is_some() {
            let (consumed, key) = decode_agent_navigator_key(&bytes[offset..]);
            offset += consumed;
            match key {
                Some(AgentNavigatorKey::Up) => self.move_tab_navigator(-1),
                Some(AgentNavigatorKey::Down) => self.move_tab_navigator(1),
                Some(AgentNavigatorKey::Home) => self.move_tab_navigator_to(false),
                Some(AgentNavigatorKey::End) => self.move_tab_navigator_to(true),
                Some(AgentNavigatorKey::PageUp) => self.page_tab_navigator(false),
                Some(AgentNavigatorKey::PageDown) => self.page_tab_navigator(true),
                Some(AgentNavigatorKey::Activate) => self.activate_tab_navigator(),
                Some(AgentNavigatorKey::Close) => {
                    self.tab_navigator = None;
                    self.force_full = true;
                    self.schedule_render();
                }
                None => {}
            }
        }
    }

    pub(super) fn move_tab_navigator(&mut self, delta: isize) {
        let rows = self.tab_navigator_rows();
        if rows.is_empty() {
            return;
        }
        let current = self
            .tab_navigator
            .and_then(|navigator| navigator.selected)
            .and_then(|tab_id| rows.iter().position(|row| row.tab_id == tab_id))
            .unwrap_or(0);
        let next = current
            .saturating_add_signed(delta)
            .min(rows.len().saturating_sub(1));
        if let Some(navigator) = &mut self.tab_navigator {
            navigator.selected = Some(rows[next].tab_id);
            navigator.selected_index = next;
        }
        self.reveal_tab_navigator_selection(rows.len(), next);
    }

    pub(super) fn move_tab_navigator_to(&mut self, end: bool) {
        let rows = self.tab_navigator_rows();
        let Some(index) = (!rows.is_empty()).then(|| if end { rows.len() - 1 } else { 0 }) else {
            return;
        };
        if let Some(navigator) = &mut self.tab_navigator {
            navigator.selected = Some(rows[index].tab_id);
            navigator.selected_index = index;
        }
        self.reveal_tab_navigator_selection(rows.len(), index);
    }

    pub(super) fn page_tab_navigator(&mut self, down: bool) {
        let page = tab_navigator_rect(self.content_area(), self.tabs.len())
            .map_or(1, |rect| usize::from(rect.height.saturating_sub(2)).max(1));
        self.move_tab_navigator(if down {
            page as isize
        } else {
            -(page as isize)
        });
    }

    pub(super) fn reveal_tab_navigator_selection(&mut self, row_count: usize, index: usize) {
        let page = tab_navigator_rect(self.content_area(), row_count)
            .map_or(1, |rect| usize::from(rect.height.saturating_sub(2)).max(1));
        if let Some(navigator) = &mut self.tab_navigator {
            if index < navigator.scroll {
                navigator.scroll = index;
            } else if index >= navigator.scroll.saturating_add(page) {
                navigator.scroll = index + 1 - page;
            }
            navigator.scroll = navigator.scroll.min(row_count.saturating_sub(page));
        }
        self.schedule_render();
    }

    pub(super) fn activate_tab_navigator(&mut self) {
        let selected = self.tab_navigator.and_then(|navigator| navigator.selected);
        self.tab_navigator = None;
        let Some(index) =
            selected.and_then(|tab_id| self.tabs.iter().position(|tab| tab.id == tab_id))
        else {
            self.force_full = true;
            self.schedule_render();
            return;
        };
        if index == self.active_tab {
            self.force_full = true;
            self.schedule_render();
        } else {
            self.active_tab = index;
            self.force_full = true;
            self.relayout();
        }
    }

    pub(super) fn tab_navigator_mouse(&mut self, mouse: MouseEvent) {
        let rows = self.tab_navigator_rows();
        let Some(rect) = tab_navigator_rect(self.content_area(), rows.len()) else {
            self.tab_navigator = None;
            return;
        };
        if mouse.kind == MouseKind::Wheel {
            self.move_tab_navigator(if mouse.button == 0 { -1 } else { 1 });
            return;
        }
        if mouse.kind != MouseKind::Press || mouse.button != 0 {
            return;
        }
        if !rect.contains(mouse.x, mouse.y) {
            self.tab_navigator = None;
            self.force_full = true;
            self.schedule_render();
            return;
        }
        if mouse.y <= rect.y || mouse.y + 1 >= rect.y + rect.height {
            return;
        }
        let scroll = self.tab_navigator.map_or(0, |navigator| navigator.scroll);
        let index = scroll + usize::from(mouse.y - rect.y - 1);
        let Some(row) = rows.get(index) else { return };
        if let Some(navigator) = &mut self.tab_navigator {
            navigator.selected = Some(row.tab_id);
            navigator.selected_index = index;
        }
        self.activate_tab_navigator();
    }

    pub(super) fn sidebar_mouse(&mut self, mouse: MouseEvent, sidebar: Rect) {
        if mouse.kind == MouseKind::Wheel {
            // Clamped against the tree's height on the next draw.
            self.sidebar_scroll = if mouse.button == 0 {
                self.sidebar_scroll.saturating_sub(1)
            } else {
                self.sidebar_scroll.saturating_add(1)
            };
            self.schedule_render();
            return;
        }
        if mouse.button != 0 {
            return;
        }
        let Some(line) = self
            .sidebar_targets
            .iter()
            .find(|(row, _)| *row == mouse.y)
            .map(|(_, line)| line.clone())
        else {
            return;
        };
        let text_x = if self.tab_view == TabView::Left {
            sidebar.x
        } else {
            sidebar.x + 1
        };
        // The `+`/`-` marker is the line's first cell.
        if let Some(tab_id) = line.toggle.filter(|_| mouse.x == text_x) {
            let expanded = self.sidebar_tab_expanded(tab_id);
            self.sidebar_expanded.insert(tab_id, !expanded);
            self.schedule_render();
            return;
        }
        match line.target {
            SidebarTarget::Tab(tab_id) => {
                if let Some(index) = self.tabs.iter().position(|tab| tab.id == tab_id) {
                    self.action(Action::SelectTab(index));
                }
            }
            SidebarTarget::Pane(pane_id) => {
                // The pane may have closed since the sidebar was drawn; then there is nothing to do.
                let _ = self.automation_focus(pane_id);
            }
        }
    }

    pub(super) fn sidebar_tab_expanded(&self, tab_id: u64) -> bool {
        self.sidebar_expanded
            .get(&tab_id)
            .copied()
            .unwrap_or_else(|| self.active_tab().is_some_and(|active| active.id == tab_id))
    }

    /// The tabs as the sidebar lists them. A pane is labeled by its name, then its terminal title,
    /// then its ID; a tab by its name, or only its number.
    pub(super) fn sidebar_tabs(&self) -> Vec<SidebarTab> {
        self.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| {
                let active = index == self.active_tab;
                let panes = sync_targets(tab, &|_| false)
                    .into_iter()
                    .filter_map(|pane_id| {
                        let pane = self.panes.get(&pane_id)?;
                        let label = pane
                            .name
                            .as_ref()
                            .map(|name| single_line(name.as_str()))
                            .or_else(|| pane.terminal.title().map(single_line))
                            .filter(|label| !label.trim().is_empty())
                            .unwrap_or_else(|| format!("pane {pane_id}"));
                        Some(SidebarPane {
                            id: pane_id,
                            label,
                            focused: active && tab.focused == pane_id,
                        })
                    })
                    .collect();
                SidebarTab {
                    id: tab.id,
                    label: tab.name.as_deref().map(single_line).unwrap_or_default(),
                    active,
                    expanded: self.sidebar_tab_expanded(tab.id),
                    panes,
                }
            })
            .collect()
    }

    /// Draw the sidebar tree, leaving the bottom row to `message` when there is one.
    pub(super) fn draw_sidebar(
        &mut self,
        screen: &mut ScreenBuffer,
        theme: crate::theme::ResolvedTheme,
        message: Option<&str>,
    ) {
        self.sidebar_targets.clear();
        let (columns, rows) = (screen.columns, screen.rows);
        let (Some(text_rect), Some(separator)) = (
            self.tab_view.sidebar_text_rect(columns, rows),
            self.tab_view.separator_column(columns, rows),
        ) else {
            return;
        };
        // Forget choices for tabs that have closed, so the map stays bounded by the live tabs.
        let live: HashSet<u64> = self.tabs.iter().map(|tab| tab.id).collect();
        self.sidebar_expanded
            .retain(|tab_id, _| live.contains(tab_id));
        let style = theme.status();
        let frame = theme.frame(false);
        let blank = " ".repeat(usize::from(text_rect.width));
        for y in 0..rows {
            screen.draw_text(text_rect.x, y, &blank, style);
            screen.draw_text(
                separator,
                y,
                "│",
                crate::screen::TextStyle {
                    foreground: frame.border,
                    background: frame.background,
                },
            );
        }
        let lines = crate::tab_view::sidebar_lines(&self.sidebar_tabs());
        let height = usize::from(rows).saturating_sub(usize::from(message.is_some()));
        self.sidebar_scroll =
            crate::tab_view::clamp_scroll(self.sidebar_scroll, lines.len(), height);
        let width = usize::from(text_rect.width);
        for (offset, line) in lines
            .into_iter()
            .skip(self.sidebar_scroll)
            .take(height)
            .enumerate()
        {
            let y = offset as u16;
            screen.draw_text(text_rect.x, y, &clip_chars(&line.text, width), style);
            if line.highlight {
                screen.invert(text_rect.x, y, text_rect.width);
            }
            self.sidebar_targets.push((y, line));
        }
        if let Some(message) = message {
            screen.draw_text(text_rect.x, rows - 1, &clip_chars(message, width), style);
        }
    }

    pub(super) fn draw_tab_navigator(
        &mut self,
        screen: &mut ScreenBuffer,
        theme: crate::theme::ResolvedTheme,
    ) {
        let rows = self.tab_navigator_rows();
        let Some(rect) = tab_navigator_rect(self.content_area(), rows.len()) else {
            self.tab_navigator = None;
            return;
        };
        let page = usize::from(rect.height.saturating_sub(2)).max(1);
        let selected_index = self.tab_navigator.and_then(|navigator| {
            navigator
                .selected
                .and_then(|tab_id| rows.iter().position(|row| row.tab_id == tab_id))
                .or_else(|| {
                    (!rows.is_empty()).then_some(navigator.selected_index.min(rows.len() - 1))
                })
        });
        if let Some(navigator) = &mut self.tab_navigator {
            navigator.selected = selected_index.map(|index| rows[index].tab_id);
            navigator.selected_index = selected_index.unwrap_or(0);
            navigator.scroll = navigator.scroll.min(rows.len().saturating_sub(page));
            if let Some(index) = selected_index {
                if index < navigator.scroll {
                    navigator.scroll = index;
                } else if index >= navigator.scroll + page {
                    navigator.scroll = index + 1 - page;
                }
            }
        }
        screen.draw_frame(rect, " Tabs ", theme.frame(true));
        let style = theme.status();
        let inner_width = usize::from(rect.width.saturating_sub(2));
        let blank = " ".repeat(inner_width);
        for offset in 0..page {
            screen.draw_text(rect.x + 1, rect.y + 1 + offset as u16, &blank, style);
        }
        let scroll = self.tab_navigator.map_or(0, |navigator| navigator.scroll);
        for (offset, row) in rows.iter().skip(scroll).take(page).enumerate() {
            let marker = if row.active { '*' } else { ' ' };
            let name = row
                .name
                .as_deref()
                .map(single_line)
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| "(unnamed)".to_owned());
            let text = format!(
                "{marker} {:>2}: {name} · panes:{}",
                row.display_index + 1,
                row.pane_count
            );
            let y = rect.y + 1 + offset as u16;
            screen.draw_text(rect.x + 1, y, &text, style);
            if self.tab_navigator.and_then(|navigator| navigator.selected) == Some(row.tab_id) {
                screen.invert(rect.x + 1, y, rect.width.saturating_sub(2));
            }
        }
        screen.cursor = None;
    }

    pub(super) fn open_pane_menu(&mut self, tab_id: u64, pane_id: PaneId, origin: (u16, u16)) {
        self.clear_transient_ui();
        self.mouse_click_tracker = None;
        let menu = PaneMenu {
            tab_id,
            pane_id,
            origin,
            selected: None,
        };
        // A display too small to hold the menu leaves the click as an ordinary focus change.
        if self.pane_menu_layout(menu).is_some() {
            self.pane_menu = Some(menu);
        }
        self.force_full = true;
        self.schedule_render();
    }

    pub(super) fn close_pane_menu(&mut self) {
        self.pane_menu = None;
        self.force_full = true;
        self.schedule_render();
    }

    /// The menu for `menu.pane_id` as it stands now, or `None` once the pane or its tab is gone
    /// or no longer shown.
    ///
    /// Items follow tmux's pane menu. Splitting and swapping are for tiled panes only, and the
    /// swap pair is named after the split that holds the pane: a pane beside its sibling swaps
    /// left and right, one stacked with it swaps up and down.
    pub(super) fn pane_menu_entries(&self, menu: PaneMenu) -> Option<Vec<PaneMenuEntry>> {
        let item = |label, key, command, enabled| PaneMenuEntry::Item {
            label,
            key,
            command,
            enabled,
        };
        let pane_id = menu.pane_id;
        let tab = self.active_tab().filter(|tab| tab.id == menu.tab_id)?;
        let pane = self.panes.get(&pane_id).filter(|_| tab.contains(pane_id))?;
        let mut entries = Vec::new();
        if let Some(tree) = tab.tree.as_ref().filter(|tree| tree.contains(pane_id)) {
            entries.push(item(
                "Horizontal Split",
                b'h',
                PaneMenuCommand::Split(Axis::Horizontal),
                true,
            ));
            entries.push(item(
                "Vertical Split",
                b'v',
                PaneMenuCommand::Split(Axis::Vertical),
                true,
            ));
            if let Some(axis) = tree.parent_axis(pane_id) {
                let projections = tiled_projections(tab, self.content_area());
                let pair = match axis {
                    Axis::Horizontal => [
                        ("Swap Left", b'l', Direction::Left),
                        ("Swap Right", b'r', Direction::Right),
                    ],
                    Axis::Vertical => [
                        ("Swap Up", b'u', Direction::Up),
                        ("Swap Down", b'd', Direction::Down),
                    ],
                };
                entries.push(PaneMenuEntry::Separator);
                for (label, key, direction) in pair {
                    let enabled = directional_focus(&projections, pane_id, direction).is_some();
                    entries.push(item(label, key, PaneMenuCommand::Swap(direction), enabled));
                }
            }
            entries.push(PaneMenuEntry::Separator);
        }
        entries.push(item("Kill", b'X', PaneMenuCommand::Kill, true));
        entries.push(item(
            "Respawn",
            b'R',
            PaneMenuCommand::Respawn,
            matches!(pane.role, PaneRole::Core),
        ));
        let pane_count =
            tab.tree.as_ref().map_or(0, |tree| tree.pane_ids().len()) + tab.floating.panes().len();
        entries.push(if tab.zoomed.is_some() {
            item("Unzoom", b'z', PaneMenuCommand::ToggleZoom, true)
        } else {
            item("Zoom", b'z', PaneMenuCommand::ToggleZoom, pane_count > 1)
        });
        Some(entries)
    }

    pub(super) fn pane_menu_layout(&self, menu: PaneMenu) -> Option<(Rect, Vec<PaneMenuEntry>)> {
        let entries = self.pane_menu_entries(menu)?;
        let rect = pane_menu_rect(
            self.content_area(),
            menu.origin,
            &entries,
            &pane_menu_title(menu.pane_id),
        )?;
        Some((rect, entries))
    }

    pub(super) fn pane_menu_input(&mut self, bytes: &[u8]) {
        let mut offset = 0;
        while offset < bytes.len() {
            let Some(menu) = self.pane_menu else {
                return;
            };
            let Some(entries) = self.pane_menu_entries(menu) else {
                self.close_pane_menu();
                return;
            };
            let input = &bytes[offset..];
            if let Some(index) = entries.iter().position(|entry| {
                matches!(entry, PaneMenuEntry::Item { key, enabled: true, .. } if *key == input[0])
            }) {
                self.activate_pane_menu(index);
                return;
            }
            let (consumed, key) = decode_agent_navigator_key(input);
            offset += consumed.max(1);
            match key {
                Some(AgentNavigatorKey::Up) => self.move_pane_menu(&entries, false),
                Some(AgentNavigatorKey::Down) => self.move_pane_menu(&entries, true),
                Some(AgentNavigatorKey::Home | AgentNavigatorKey::PageUp) => {
                    self.select_pane_menu(first_enabled(&entries, (0..entries.len()).collect()));
                }
                Some(AgentNavigatorKey::End | AgentNavigatorKey::PageDown) => {
                    self.select_pane_menu(first_enabled(
                        &entries,
                        (0..entries.len()).rev().collect(),
                    ));
                }
                Some(AgentNavigatorKey::Activate) => {
                    if let Some(index) = menu.selected {
                        self.activate_pane_menu(index);
                    }
                    return;
                }
                Some(AgentNavigatorKey::Close) => {
                    self.close_pane_menu();
                    return;
                }
                None => {}
            }
        }
    }

    /// Step the selection to the next enabled item, wrapping at either end as tmux does.
    pub(super) fn move_pane_menu(&mut self, entries: &[PaneMenuEntry], down: bool) {
        let count = entries.len();
        let current = self.pane_menu.and_then(|menu| menu.selected);
        let order = (1..=count)
            .map(|step| match (current, down) {
                (None, true) => step - 1,
                (None, false) => count - step,
                (Some(index), true) => (index + step) % count,
                (Some(index), false) => (index + count - step % count) % count,
            })
            .collect();
        self.select_pane_menu(first_enabled(entries, order));
    }

    pub(super) fn select_pane_menu(&mut self, index: Option<usize>) {
        if let Some(menu) = &mut self.pane_menu
            && index.is_some()
            && menu.selected != index
        {
            menu.selected = index;
            self.schedule_render();
        }
    }

    pub(super) fn pane_menu_mouse(&mut self, mouse: MouseEvent) {
        let Some(menu) = self.pane_menu else {
            return;
        };
        let Some((rect, entries)) = self.pane_menu_layout(menu) else {
            self.close_pane_menu();
            return;
        };
        let hit = pane_menu_hit(rect, mouse.x, mouse.y).filter(|index| {
            entries
                .get(*index)
                .is_some_and(|entry| entry.enabled_command().is_some())
        });
        match mouse.kind {
            MouseKind::Move => self.select_pane_menu(hit),
            MouseKind::Release => {
                if (mouse.x, mouse.y) != menu.origin
                    && let Some(index) = hit
                {
                    self.activate_pane_menu(index);
                }
            }
            MouseKind::Press if !rect.contains(mouse.x, mouse.y) => self.close_pane_menu(),
            MouseKind::Press => {
                if let Some(index) = hit {
                    self.activate_pane_menu(index);
                }
            }
            MouseKind::Wheel => {}
        }
    }

    pub(super) fn activate_pane_menu(&mut self, index: usize) {
        let Some(menu) = self.pane_menu.take() else {
            return;
        };
        self.force_full = true;
        self.schedule_render();
        let Some(command) = self
            .pane_menu_entries(menu)
            .and_then(|entries| entries.get(index).copied())
            .and_then(PaneMenuEntry::enabled_command)
        else {
            return;
        };
        let pane_id = menu.pane_id;
        match command {
            PaneMenuCommand::Split(axis) => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.set_focus(pane_id);
                }
                self.action(Action::Split(axis));
            }
            PaneMenuCommand::Swap(direction) => self.swap_tiled_pane(pane_id, direction),
            PaneMenuCommand::Kill => self.close_pane(pane_id),
            PaneMenuCommand::Respawn => self.respawn_pane(pane_id),
            PaneMenuCommand::ToggleZoom => {
                if let Some(tab) = self.active_tab_mut() {
                    tab.set_focus(pane_id);
                }
                self.action(Action::ToggleZoom);
            }
        }
    }

    /// Trade places with the nearest tiled pane in `direction` in the active tab. Floats are
    /// not candidates: a float has no slot in the tree to trade.
    pub(super) fn swap_tiled_pane(&mut self, pane_id: PaneId, direction: Direction) {
        let area = self.content_area();
        let Some(tab) = self.active_tab_mut() else {
            return;
        };
        let Some(other) = directional_focus(&tiled_projections(tab, area), pane_id, direction)
        else {
            return;
        };
        if tab
            .tree
            .as_mut()
            .is_some_and(|tree| tree.swap(pane_id, other))
        {
            tab.zoomed = None;
            self.force_full = true;
            self.relayout();
        }
    }

    pub(super) fn draw_pane_menu(
        &mut self,
        screen: &mut ScreenBuffer,
        theme: crate::theme::ResolvedTheme,
    ) {
        let Some(menu) = self.pane_menu else {
            return;
        };
        let Some((rect, entries)) = self.pane_menu_layout(menu) else {
            self.pane_menu = None;
            return;
        };
        let frame = theme.frame(true);
        screen.draw_frame(rect, &pane_menu_title(menu.pane_id), frame);
        let style = theme.status();
        let disabled = crate::screen::TextStyle {
            foreground: theme.inactive_frame,
            background: style.background,
        };
        let rule = crate::screen::TextStyle {
            foreground: frame.border,
            background: frame.background,
        };
        let inner = usize::from(rect.width.saturating_sub(2));
        for (offset, entry) in entries.iter().enumerate() {
            let y = rect.y + 1 + offset as u16;
            match entry {
                PaneMenuEntry::Separator => {
                    let line = format!("├{}┤", "─".repeat(inner));
                    screen.draw_text(rect.x, y, &line, rule);
                }
                PaneMenuEntry::Item {
                    label,
                    key,
                    enabled,
                    ..
                } => {
                    let text = format!(
                        " {label:<width$}{} ",
                        char::from(*key),
                        width = inner.saturating_sub(3)
                    );
                    screen.draw_text(
                        rect.x + 1,
                        y,
                        &text,
                        if *enabled { style } else { disabled },
                    );
                    if menu.selected == Some(offset) {
                        screen.invert(rect.x + 1, y, rect.width.saturating_sub(2));
                    }
                }
            }
        }
        screen.cursor = None;
    }

    pub(super) fn begin_tab_rename(&mut self) {
        let Some((tab_id, name)) = self
            .active_tab()
            .map(|tab| (tab.id, tab.name.clone().unwrap_or_default()))
        else {
            return;
        };
        self.clear_transient_ui();
        self.tab_rename = Some(TabRename {
            tab_id,
            value: truncate_utf8(single_line(&name), MAX_TAB_NAME_BYTES),
            pending_utf8: Vec::new(),
        });
        self.force_full = true;
        self.schedule_render();
    }

    pub(super) fn tab_rename_input(&mut self, bytes: &[u8]) {
        let Some(mut rename) = self.tab_rename.take() else {
            return;
        };
        let action = apply_tab_rename_input(&mut rename, bytes);
        match action {
            LineEditInput::Editing => {
                if self.tabs.iter().any(|tab| tab.id == rename.tab_id) {
                    self.tab_rename = Some(rename);
                }
                self.schedule_render();
            }
            LineEditInput::Cancel => {
                self.force_full = true;
                self.schedule_render();
            }
            LineEditInput::Commit => {
                let name = rename.value.trim();
                let next = (!name.is_empty()).then(|| name.to_owned());
                if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.id == rename.tab_id)
                    && tab.name != next
                {
                    tab.name = next;
                    self.session_sequence = self.session_sequence.wrapping_add(1);
                }
                self.force_full = true;
                self.schedule_render();
            }
        }
    }

    pub(super) fn begin_close_pane_confirmation(&mut self) {
        let Some((tab_id, pane_id)) = self.active_tab().map(|tab| (tab.id, tab.focused)) else {
            return;
        };
        self.clear_transient_ui();
        self.close_pane_confirmation = Some(ClosePaneConfirmation { tab_id, pane_id });
        self.force_full = true;
        self.schedule_render();
    }

    pub(super) fn resolve_close_pane_confirmation(&mut self, confirmed: bool) {
        let Some(confirmation) = self.close_pane_confirmation.take() else {
            return;
        };
        if confirmed
            && self
                .tabs
                .iter()
                .any(|tab| tab.id == confirmation.tab_id && tab.contains(confirmation.pane_id))
        {
            self.close_pane(confirmation.pane_id);
        } else {
            self.force_full = true;
            self.schedule_render();
        }
    }

    pub(super) fn begin_save_layout(&mut self) {
        self.clear_transient_ui();
        self.save_layout_prompt = Some(SaveLayoutPrompt {
            stage: SaveLayoutStage::Editing {
                value: crate::layout_file::STARTUP_FILE.to_owned(),
            },
            pending_utf8: Vec::new(),
        });
        self.force_full = true;
        self.schedule_render();
    }

    pub(super) fn save_layout_prompt_input(&mut self, bytes: &[u8]) {
        let Some(mut prompt) = self.save_layout_prompt.take() else {
            return;
        };
        match &mut prompt.stage {
            SaveLayoutStage::Editing { value } => {
                match apply_line_edit(
                    value,
                    &mut prompt.pending_utf8,
                    MAX_LAYOUT_NAME_BYTES,
                    bytes,
                ) {
                    LineEditInput::Editing => {
                        self.save_layout_prompt = Some(prompt);
                        self.schedule_render();
                    }
                    LineEditInput::Cancel => {
                        self.force_full = true;
                        self.schedule_render();
                    }
                    LineEditInput::Commit => {
                        let target = crate::layout_file::resolve_save_path(value);
                        self.force_full = true;
                        match target {
                            // Replacing a layout the user already has is the one destructive part
                            // of this flow, so it asks first.
                            Ok(path) if path.exists() => {
                                self.save_layout_prompt = Some(SaveLayoutPrompt {
                                    stage: SaveLayoutStage::Confirm { path },
                                    pending_utf8: Vec::new(),
                                });
                                self.schedule_render();
                            }
                            Ok(path) => self.commit_save_layout(&path),
                            Err(error) => self.notice(format!("save failed: {error}")),
                        }
                    }
                }
            }
            SaveLayoutStage::Confirm { path } => {
                let path = path.clone();
                for byte in bytes {
                    match byte {
                        b'y' | b'Y' => {
                            self.force_full = true;
                            self.commit_save_layout(&path);
                            return;
                        }
                        b'n' | b'N' | 0x1b => {
                            self.force_full = true;
                            self.notice("save canceled");
                            return;
                        }
                        _ => {}
                    }
                }
                self.save_layout_prompt = Some(prompt);
            }
        }
    }

    pub(super) fn commit_save_layout(&mut self, path: &Path) {
        self.save_layout_prompt = None;
        match self.save_layout(path) {
            Ok((tabs, panes)) => {
                let name = path.file_name().map_or_else(
                    || path.display().to_string(),
                    |name| name.to_string_lossy().into_owned(),
                );
                let tab_word = if tabs == 1 { "tab" } else { "tabs" };
                let pane_word = if panes == 1 { "pane" } else { "panes" };
                self.notice(format!(
                    "saved {tabs} {tab_word}, {panes} {pane_word} to {name}"
                ));
            }
            Err(error) => self.notice(format!("save failed: {error}")),
        }
    }

    /// Show a short-lived status-row message. Nothing else about the session changes.
    pub(super) fn notice(&mut self, message: impl Into<String>) {
        self.status_notice = Some(StatusNotice {
            message: single_line(&message.into()),
            expires: Instant::now() + STATUS_NOTICE_DURATION,
        });
        self.force_full = true;
        self.schedule_render();
    }

    pub(super) fn active_status_notice(&self) -> Option<&str> {
        self.status_notice
            .as_ref()
            .filter(|notice| notice.expires > Instant::now())
            .map(|notice| notice.message.as_str())
    }

    /// Drop an expired notice and repaint once, so the status row returns to the tab list without
    /// waiting for unrelated activity.
    pub(super) fn expire_status_notice(&mut self) {
        if self
            .status_notice
            .as_ref()
            .is_some_and(|notice| notice.expires <= Instant::now())
        {
            self.status_notice = None;
            self.force_full = true;
            self.schedule_render();
        }
    }

    pub(super) fn next_notice_deadline(&self) -> Duration {
        self.status_notice.as_ref().map_or(Duration::MAX, |notice| {
            notice.expires.saturating_duration_since(Instant::now())
        })
    }

    pub(super) fn close_pane_confirmation_input(&mut self, bytes: &[u8]) {
        for byte in bytes {
            match byte {
                b'y' | b'Y' => {
                    self.resolve_close_pane_confirmation(true);
                    return;
                }
                b'n' | b'N' | 0x1b => {
                    self.resolve_close_pane_confirmation(false);
                    return;
                }
                _ => {}
            }
        }
    }
}

/// Join a block reason and metadata tokens into one navigator suffix.
///
/// The reason comes first because it is the thing the user has to act on; tokens follow in the
/// stable key order the map already keeps, so a row does not reshuffle as values update.
fn agent_row_detail(message: Option<&str>, metadata: &crate::agent::AgentMetadata) -> String {
    message
        .map(ToOwned::to_owned)
        .into_iter()
        .chain(
            metadata
                .tokens()
                .map(|(key, value)| format!("${key} {value}")),
        )
        .collect::<Vec<_>>()
        .join(" · ")
}

fn truncate_utf8(mut value: String, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value.truncate(end);
    value
}

/// A tab's tiled panes at their unzoomed rectangles: the candidates for a pane-menu swap.
fn tiled_projections(tab: &Tab, area: Rect) -> Vec<PaneProjection> {
    tab.tree.as_ref().map_or_else(Vec::new, |tree| {
        tree.geometry(area)
            .into_iter()
            .map(|(pane_id, outer)| PaneProjection {
                pane_id,
                outer,
                content: outer.content(),
                layer: PaneLayer::Tiled,
                focused: tab.focused == pane_id,
            })
            .collect()
    })
}

fn tab_navigator_rect(area: Rect, row_count: usize) -> Option<Rect> {
    agent_navigator_rect(area, row_count)
}
