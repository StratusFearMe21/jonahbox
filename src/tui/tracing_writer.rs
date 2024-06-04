use std::{fmt::Display, ops::Range, sync::Arc};

use ratatui::{
    layout::Margin,
    style::{Color, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarState, StatefulWidget, Widget, Wrap},
};
use tokio::sync::watch::Sender;
use tracing::{field::Visit, Level, Subscriber};
use tracing_subscriber::{
    fmt::{FormatEvent, FormatFields, FormattedFields},
    registry::LookupSpan,
};

pub struct TuiWriterImpl {
    pub buffer: boxcar::Vec<Line<'static>>,
    pub notifier: Sender<()>,
}

impl TuiWriterImpl {
    fn new(notifier: Sender<()>) -> Self {
        Self {
            buffer: boxcar::Vec::default(),
            notifier,
        }
    }
}

#[derive(Clone)]
pub struct TuiWriter(pub Arc<TuiWriterImpl>);

impl TuiWriter {
    pub fn new(notifier: Sender<()>) -> Self {
        TuiWriter(Arc::new(TuiWriterImpl::new(notifier)))
    }
}

impl<C, N> FormatEvent<C, N> for TuiWriter
where
    C: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, C, N>,
        _writer: tracing_subscriber::fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        let mut line = Line::default();
        let meta = event.metadata();
        line.spans.push(Span::styled(
            format!("{:>5} ", meta.level()),
            Style::new().fg(level_to_color(event.metadata().level())),
        ));
        if let Some(scope) = ctx.event_scope() {
            let mut seen = false;

            for span in scope.from_root() {
                line.spans.push(Span::styled(
                    format!("{}", span.metadata().name()),
                    Style::new().bold(),
                ));
                seen = true;

                let ext = span.extensions();
                if let Some(fields) = &ext.get::<FormattedFields<N>>() {
                    if !fields.is_empty() {
                        line.spans.push(Span::styled("{", Style::new().bold()));
                        line.spans
                            .push(Span::styled(format!("{}", fields), Style::new()));
                        line.spans.push(Span::styled("}", Style::new().bold()));
                    }
                }
                line.spans.push(Span::styled(":", Style::new().dim()));
            }

            if seen {
                line.spans.push(Span::styled(" ", Style::new()));
            }
        };

        line.spans.push(Span::styled(
            format!("{}: ", meta.target()),
            Style::new().dim(),
        ));

        // if let Some(filename) = meta.file() {
        //     line.spans.push(Span::styled(
        //         format!(
        //             "{}:{}",
        //             filename,
        //             if line_number.is_some() { "" } else { " " }
        //         ),
        //         Style::new().dim(),
        //     ));
        // }

        // if let Some(line_number) = meta.line() {
        //     line.spans.push(Span::styled(
        //         format!("{}: ", line_number),
        //         Style::new().dim(),
        //     ));
        // }

        event.record(&mut TuiVisitor(&mut line));

        self.0.buffer.push(line);
        let _ = self.0.notifier.send(());
        Ok(())
    }
}

impl TuiWriter {
    pub fn text(&self, range: Range<usize>) -> Text<'static> {
        let mut lines = Vec::new();

        for i in range {
            if let Some(text) = self.0.buffer.get(i) {
                lines.push(text.clone())
            } else {
                break;
            }
        }
        Text::from(lines)
    }
}

impl<'a> StatefulWidget for &'a TuiWriter {
    type State = Option<usize>;

    fn render(
        self,
        area: ratatui::prelude::Rect,
        buf: &mut ratatui::prelude::Buffer,
        state: &mut Self::State,
    ) {
        Block::new()
            .title("Log")
            .borders(Borders::all())
            .border_style(if state.is_some() {
                Style::new().dim().green()
            } else {
                Style::new()
            })
            .render(area, buf);
        let paragraph_area = area.inner(&Margin::new(1, 1));
        let len = self.0.buffer.count();
        let scroll_bar = Scrollbar::new(ratatui::widgets::ScrollbarOrientation::VerticalRight);
        let mut scroll_bar_state = ScrollbarState::new(len);
        if let Some(scroll) = *state {
            scroll_bar_state = scroll_bar_state.position(scroll);
        } else {
            scroll_bar_state.last();
        }
        let text = self.text(
            state
                .unwrap_or(len)
                .saturating_sub(paragraph_area.height as usize)..state.unwrap_or(len),
        );
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        let line_count = paragraph.line_count(paragraph_area.width) as u16;
        let scroll_y = if line_count > paragraph_area.height {
            line_count - paragraph_area.height
        } else {
            0
        };
        let paragraph = paragraph.scroll((scroll_y, 0));

        paragraph.render(paragraph_area, buf);
        scroll_bar.render(area, buf, &mut scroll_bar_state);
    }
}

struct ErrorSourceList<'a>(&'a (dyn std::error::Error + 'static));

impl<'a> Display for ErrorSourceList<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut list = f.debug_list();
        let mut curr = Some(self.0);
        while let Some(curr_err) = curr {
            list.entry(&format_args!("{}", curr_err));
            curr = curr_err.source();
        }
        list.finish()
    }
}

struct TuiVisitor<'a>(&'a mut Line<'static>);

impl<'a> Visit for TuiVisitor<'a> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.record_debug(field, &format_args!("{}", value))
        } else {
            self.record_debug(field, &value)
        }
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        if let Some(source) = value.source() {
            self.0
                .spans
                .push(Span::styled(format!("{}, ", value), Style::new()));
            self.0.spans.push(Span::styled(
                format!("{}.sources", field),
                Style::new().bold(),
            ));
            self.0.spans.push(Span::styled(
                format!(": {}", ErrorSourceList(source)),
                Style::new(),
            ));
        } else {
            self.record_debug(field, &format_args!("{}", value))
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self
                .0
                .spans
                .push(Span::styled(format!("{:?} ", value), Style::new())),
            // Skip fields that are actually log metadata that have already been handled
            name if name.starts_with("log.") => {}
            name if name.starts_with("r#") => {
                self.0.spans.push(Span::styled(
                    format!("{}", &name[2..]),
                    Style::new().italic(),
                ));
                self.0
                    .spans
                    .push(Span::styled(format!("={:?} ", value), Style::new()));
            }
            name => {
                self.0
                    .spans
                    .push(Span::styled(format!("{}", name), Style::new().italic()));
                self.0
                    .spans
                    .push(Span::styled(format!("={:?} ", value), Style::new()));
            }
        };
    }
}

fn level_to_color(level: &Level) -> Color {
    match *level {
        Level::ERROR => Color::LightRed,
        Level::WARN => Color::LightYellow,
        Level::INFO => Color::LightGreen,
        Level::DEBUG => Color::LightBlue,
        Level::TRACE => Color::LightCyan,
    }
}
