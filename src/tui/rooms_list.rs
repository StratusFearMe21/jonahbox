use ratatui::{
    style::{Modifier, Style},
    widgets::{Block, Borders, List, Paragraph, StatefulWidget, Widget, Wrap},
};

pub struct RoomsList<'a>(pub &'a mut super::TuiState);

impl<'a> Widget for RoomsList<'a> {
    fn render(self, area: ratatui::prelude::Rect, buf: &mut ratatui::prelude::Buffer)
    where
        Self: Sized,
    {
        let block = Block::new().title("Rooms").borders(Borders::all());
        if self.0.state.room_map.is_empty() {
            return Paragraph::new("No rooms have been created yet")
                .block(block)
                .wrap(Wrap { trim: false })
                .render(area, buf);
        }

        let mut items = Vec::new();

        for room in self.0.state.room_map.iter() {
            items.push(format!(
                "{} - {}",
                room.room_config.code, room.room_config.app_tag
            ));
        }

        let list = List::new(items)
            .block(block)
            .highlight_style(Style::new().add_modifier(Modifier::REVERSED))
            .highlight_symbol(">> ");

        StatefulWidget::render(list, area, buf, &mut self.0.rooms_list_state);
    }
}
