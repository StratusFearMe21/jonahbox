use std::borrow::Cow;

use ratatui::{
    layout::{Constraint, Layout},
    widgets::{Block, Borders, List, Paragraph, Tabs, Widget},
};

use super::RoomTabs;

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
        match self.0.room_tab {
            RoomTabs::QRCode => {
                Paragraph::new(self.0.room_qr.as_str())
                    .alignment(ratatui::layout::Alignment::Center)
                    .render(split[1], buf);
            }
            RoomTabs::Entities => {
                let room = if let Some(selected) = self.0.rooms_list_state.selected() {
                    let Some(room) = self.0.state.room_map.iter().nth(selected) else {
                        return;
                    };
                    room
                } else {
                    return;
                };
                List::new(
                    room.entities
                        .iter()
                        .map(|e| e.key().to_owned())
                        .collect::<Vec<_>>(),
                )
                .block(Block::new().borders(Borders::all()))
                .render(split[1], buf);
            }
            RoomTabs::Players => {
                let room = if let Some(selected) = self.0.rooms_list_state.selected() {
                    let Some(room) = self.0.state.room_map.iter().nth(selected) else {
                        return;
                    };
                    room
                } else {
                    return;
                };
                List::new(
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
                .block(Block::new().borders(Borders::all()))
                .render(split[1], buf);
            }
        }
    }
}
