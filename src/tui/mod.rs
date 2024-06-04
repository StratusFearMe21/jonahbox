use std::{
    io::{stdout, BufWriter, Write},
    time::Duration,
};

use crossterm::{
    event::{self, KeyCode, KeyEvent, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    QueueableCommand,
};
use futures_util::{FutureExt, SinkExt, StreamExt};
use qrcode::{render::unicode, QrCode};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    style::{Style, Stylize},
    text::Span,
    widgets::ListState,
    Frame, Terminal,
};
use tokio::{signal, sync::mpsc::UnboundedReceiver};

use self::{qr_view::QrView, rooms_list::RoomsList, tracing_writer::TuiWriter};

mod qr_view;
mod rooms_list;
pub mod tracing_writer;

struct TuiState {
    log_scroll_state: Option<usize>,
    rooms_list_state: ListState,
    room_qr: String,
    axum_handle: axum_server::Handle,
    state: crate::State,
}

impl TuiState {
    fn rooms_list<'a>(&'a mut self) -> RoomsList<'a> {
        RoomsList(self)
    }

    fn qr_view<'a>(&'a mut self) -> QrView<'a> {
        QrView(self)
    }

    fn arrow_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up => {
                self.rooms_list_state.select(Some(
                    self.rooms_list_state
                        .selected()
                        .map(|s| s.saturating_sub(1))
                        .unwrap_or_default(),
                ));
            }
            KeyCode::Down => {
                self.rooms_list_state.select(Some(
                    self.rooms_list_state
                        .selected()
                        .map(|s| s + 1)
                        .unwrap_or_default(),
                ));
            }
            _ => {}
        }

        let room = if let Some(selected) = self.rooms_list_state.selected() {
            let Some(room) = self.state.room_map.iter().nth(selected) else {
                return;
            };
            room
        } else {
            return;
        };
        self.room_qr = QrCode::new(format!(
            "https://{}?code={}",
            self.state.config.accessible_host, room.room_config.code
        ))
        .unwrap()
        .render::<unicode::Dense1x2>()
        .build();
    }
}

pub async fn tui(
    axum_handle: axum_server::Handle,
    state: crate::State,
    log: TuiWriter,
    mut rx: UnboundedReceiver<()>,
) -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut tui_state = TuiState {
        room_qr: String::new(),
        log_scroll_state: None,
        rooms_list_state: ListState::default(),
        axum_handle,
        state,
    };
    let mut stdout = BufWriter::new(stdout().lock());
    stdout.queue(EnterAlternateScreen)?.flush()?;

    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut event_stream = crossterm::event::EventStream::new();
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::pin!(ctrl_c);
    tokio::pin!(terminate);

    terminal.clear()?;
    loop {
        terminal.draw(|frame| ui(frame, &log, &mut tui_state))?;
        let mut event = None;
        tokio::select! {
            _ = &mut ctrl_c => break,
            _ = &mut terminate => break,
            _ = rx.recv() => {}
            e = event_stream.next() => event = e
        }

        if let Some(event) = event {
            let event = event?;

            match event {
                event::Event::Key(KeyEvent {
                    code: KeyCode::Char('q') | KeyCode::Esc,
                    ..
                }) => {
                    break;
                }
                event::Event::Key(
                    event @ KeyEvent {
                        code: KeyCode::Up | KeyCode::Down | KeyCode::Left | KeyCode::Right,
                        kind: KeyEventKind::Press,
                        ..
                    },
                ) => {
                    tui_state.arrow_key(event.code);
                }
                _ => {}
            }
        }
    }

    tracing::info!("Received termination signal shutting down");
    disable_raw_mode()?;
    terminal
        .backend_mut()
        .queue(LeaveAlternateScreen)?
        .flush()?;
    for room in tui_state.state.room_map.iter() {
        for connection in room.connections.iter() {
            if let Some(socket) = connection.socket.lock().await.as_mut() {
                socket.close().await.unwrap()
            }
        }
    }
    tui_state
        .axum_handle
        .graceful_shutdown(Some(Duration::from_secs(10))); // 10 secs is how long docker will wait
                                                           // to force shutdown

    Ok(())
}

fn ui(frame: &mut Frame, log: &TuiWriter, tui_state: &mut TuiState) {
    let main_layout = Layout::new(
        ratatui::layout::Direction::Vertical,
        [Constraint::Min(0), Constraint::Length(1)],
    )
    .split(frame.size());
    let split_layout = Layout::new(
        ratatui::layout::Direction::Horizontal,
        [Constraint::Ratio(1, 4), Constraint::Ratio(3, 4)],
    )
    .split(main_layout[0]);
    let qr_log_split = Layout::new(
        ratatui::layout::Direction::Vertical,
        [Constraint::Ratio(2, 3), Constraint::Ratio(1, 3)],
    )
    .split(split_layout[1]);
    frame.render_stateful_widget(log, qr_log_split[1], &mut tui_state.log_scroll_state);
    if tui_state
        .axum_handle
        .listening()
        .now_or_never()
        .flatten()
        .is_some()
    {
        frame.render_widget(
            Span::styled("●", Style::new().green()),
            main_layout[1].inner(&ratatui::layout::Margin {
                horizontal: 1,
                vertical: 0,
            }),
        );
    } else {
        frame.render_widget(
            Span::styled("●", Style::new().red()),
            main_layout[1].inner(&ratatui::layout::Margin {
                horizontal: 1,
                vertical: 0,
            }),
        );
    }
    frame.render_widget(tui_state.rooms_list(), split_layout[0]);
    frame.render_widget(tui_state.qr_view(), qr_log_split[0]);
}
