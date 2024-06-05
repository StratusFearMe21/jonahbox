use std::borrow::Cow;

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style, Stylize},
    widgets::{Block, Borders, List, Paragraph, StatefulWidget, Tabs, Widget},
};

use super::{AppFocus, RoomTabs};

pub struct RoomView<'a>(pub &'a mut super::TuiState);

impl<'a> Widget for RoomView<'a> {
    fn render(self, area: ratatui::prelude::Rect, buf: &mut ratatui::prelude::Buffer)
    where
        Self: Sized,
    {
        let split = Layout::new(
            ratatui::layout::Direction::Vertical,
            [Constraint::Length(1), Constraint::Min(0)],
        )
        .split(area);
        Tabs::new(RoomTabs::list())
            .select(self.0.room_tab as usize)
            .render(split[0], buf);
        let mut block = Block::new().borders(Borders::all());
        if self.0.app_focus == AppFocus::EntityList {
            block = block.border_style(Style::new().dim().green());
        }
        match self.0.room_tab {
            RoomTabs::QRCode => {
                Paragraph::new(self.0.generate_room_qr())
                    .alignment(ratatui::layout::Alignment::Center)
                    .render(split[1], buf);
            }
            RoomTabs::Entities => {
                let Some(room) = self.0.selected_room() else {
                    return;
                };
                let list = List::new(
                    room.entities
                        .iter()
                        .map(|e| e.key().to_owned())
                        .collect::<Vec<_>>(),
                )
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol(">> ")
                .block(block);
                if self
                    .0
                    .rooms_entity_list_state
                    .selected()
                    .is_some_and(|l| list.len() <= l)
                {
                    self.0.rooms_entity_list_state.select(None);
                }
                let new_split = Layout::new(
                    ratatui::layout::Direction::Horizontal,
                    [Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)],
                )
                .split(split[1]);
                let alt_split = [split[1], Rect::ZERO];
                let split = if self.0.rooms_entity_list_state.selected().is_some() {
                    new_split.as_ref()
                } else {
                    &alt_split
                };
                StatefulWidget::render(list, split[0], buf, &mut self.0.rooms_entity_list_state);
                if split[1] != Rect::ZERO {
                    let entity = room
                        .entities
                        .iter()
                        .nth(self.0.rooms_entity_list_state.selected().unwrap())
                        .unwrap();
                    Paragraph::new(serde_json::to_string_pretty(entity.value()).unwrap())
                        .block(Block::new().borders(Borders::all()))
                        .render(split[1], buf)
                }
            }
            RoomTabs::Players => {
                let Some(room) = self.0.selected_room() else {
                    return;
                };
                let list = List::new(
                    room.connections
                        .iter()
                        .map(|e| {
                            if e.value().profile.name.is_empty() {
                                Cow::Borrowed("Host")
                            } else {
                                Cow::Owned(e.value().profile.name.clone())
                            }
                        })
                        .collect::<Vec<_>>(),
                )
                .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
                .highlight_symbol(">> ")
                .block(block);
                if self
                    .0
                    .rooms_entity_list_state
                    .selected()
                    .is_some_and(|l| list.len() <= l)
                {
                    self.0.rooms_entity_list_state.select(None);
                }
                let new_split = Layout::new(
                    ratatui::layout::Direction::Horizontal,
                    [Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)],
                )
                .split(split[1]);
                let alt_split = [split[1], Rect::ZERO];
                let split = if self.0.rooms_entity_list_state.selected().is_some() {
                    new_split.as_ref()
                } else {
                    &alt_split
                };
                StatefulWidget::render(list, split[0], buf, &mut self.0.rooms_entity_list_state);
            }
        }
    }
}
