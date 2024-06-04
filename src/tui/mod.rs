use std::{
    io::{stdout, BufWriter, Write},
    time::Duration,
};

use crossterm::{
    event::{self, KeyCode, KeyEvent, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    QueueableCommand,
};
use futures_util::StreamExt;
use qrcode::{render::unicode, QrCode};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout},
    widgets::ListState,
    Frame, Terminal,
};
use tokio::{signal, sync::watch::Receiver};

use self::{room_view::RoomView, rooms_list::RoomsList, tracing_writer::TuiWriter};

mod room_view;
mod rooms_list;
pub mod tracing_writer;

struct TuiState {
    log_scroll_state: Option<usize>,
    rooms_list_state: ListState,
    log: TuiWriter,
    room_qr: String,
    axum_handle: axum_server::Handle,
    state: crate::State,
    app_focus: AppFocus,
    room_tab: RoomTabs,
}

#[derive(PartialEq, Eq)]
enum AppFocus {
    RoomList,
    Tracing,
}

#[repr(usize)]
#[derive(Clone, Copy)]
enum RoomTabs {
    QRCode,
    Entities,
    Players,
}

impl RoomTabs {
    const fn list() -> [&'static str; 3] {
        ["QR Code", "Entities", "Players"]
    }

    pub fn next(&mut self) {
        match self {
            RoomTabs::QRCode => *self = RoomTabs::Entities,
            RoomTabs::Entities => *self = RoomTabs::Players,
            RoomTabs::Players => *self = RoomTabs::QRCode,
        }
    }
}

impl TuiState {
    fn rooms_list<'a>(&'a mut self) -> RoomsList<'a> {
        RoomsList(self)
    }

    fn room_view<'a>(&'a mut self) -> RoomView<'a> {
        RoomView(self)
    }

    fn arrow_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Left => {
                self.app_focus = AppFocus::RoomList;
                self.log_scroll_state = None;
                self.generate_room_qr();
                return;
            }
            KeyCode::Right => {
                self.app_focus = AppFocus::Tracing;
                self.log_scroll_state = Some(self.log.0.buffer.count());
                self.room_qr.clear();
                return;
            }
            _ => {}
        }
        match self.app_focus {
            AppFocus::RoomList => {
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

                self.generate_room_qr();
            }
            AppFocus::Tracing => match code {
                KeyCode::Up => {
                    let state = self
                        .log_scroll_state
                        .get_or_insert_with(|| self.log.0.buffer.count());
                    *state = state.saturating_sub(1);
                }
                KeyCode::Down => {
                    let count = self.log.0.buffer.count();
                    let state = self.log_scroll_state.get_or_insert(count);
                    *state = state.saturating_add(1);

                    if *state > count {
                        *state = count;
                    }
                }
                _ => {}
            },
        }
    }

    fn generate_room_qr(&mut self) {
        self.room_qr.clear();
        let room = if let Some(selected) = self.rooms_list_state.selected() {
            let Some(room) = self.state.room_map.iter().nth(selected) else {
                self.rooms_list_state.select(None);
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

    async fn close_room(&mut self) {
        let room = if let Some(selected) = self.rooms_list_state.selected() {
            let Some(room) = self.state.room_map.iter().nth(selected) else {
                return;
            };
            room.close().await;
            room
        } else {
            return;
        }
        .key()
        .to_owned();

        self.state.room_map.remove(&room);
        self.generate_room_qr();
    }
}

pub async fn tui(
    axum_handle: axum_server::Handle,
    state: crate::State,
    log: TuiWriter,
    mut rx: Receiver<()>,
) -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut tui_state = TuiState {
        room_qr: String::new(),
        log_scroll_state: None,
        rooms_list_state: ListState::default(),
        axum_handle,
        state,
        app_focus: AppFocus::RoomList,
        log,
        room_tab: RoomTabs::QRCode,
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
        terminal.draw(|frame| ui(frame, &mut tui_state))?;
        let mut event = None;
        tokio::select! {
            _ = &mut ctrl_c => break,
            _ = &mut terminate => break,
            c = rx.changed() => c.unwrap(),
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
                event::Event::Key(KeyEvent {
                    code: KeyCode::Tab, ..
                }) => {
                    tui_state.room_tab.next();
                }
                event::Event::Key(KeyEvent {
                    code: KeyCode::Backspace | KeyCode::Delete,
                    ..
                }) => {
                    tui_state.close_room().await;
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
        room.close().await;
    }
    tui_state
        .axum_handle
        .graceful_shutdown(Some(Duration::from_secs(10))); // 10 secs is how long docker will wait
                                                           // to force shutdown

    Ok(())
}

fn ui(frame: &mut Frame, tui_state: &mut TuiState) {
    let split_layout = Layout::new(
        ratatui::layout::Direction::Horizontal,
        [Constraint::Ratio(1, 4), Constraint::Ratio(3, 4)],
    )
    .split(frame.size());
    let room_log_split = Layout::new(
        ratatui::layout::Direction::Vertical,
        [
            Constraint::Length(tui_state.room_qr.lines().count() as u16),
            Constraint::Min(0),
        ],
    )
    .split(split_layout[1]);
    frame.render_stateful_widget(
        &tui_state.log,
        room_log_split[1],
        &mut tui_state.log_scroll_state,
    );
    frame.render_widget(tui_state.rooms_list(), split_layout[0]);
    frame.render_widget(tui_state.room_view(), room_log_split[0]);
}
