use ratatui::widgets::{Paragraph, Widget};

pub struct QrView<'a>(pub &'a mut super::TuiState);

impl<'a> Widget for QrView<'a> {
    fn render(self, area: ratatui::prelude::Rect, buf: &mut ratatui::prelude::Buffer)
    where
        Self: Sized,
    {
        Paragraph::new(self.0.room_qr.as_str())
            .alignment(ratatui::layout::Alignment::Center)
            .render(area, buf);
    }
}
