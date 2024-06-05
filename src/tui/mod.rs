use std::{
    io::{stdout, BufWriter, Write},
    sync::Arc,
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
    widgets::{Block, Borders, ListState},
    Frame, Terminal,
};
use tokio::{signal, sync::watch::Receiver};

use crate::Room;

use self::{room_view::RoomView, rooms_list::RoomsList, tracing_writer::TuiWriter};

mod room_view;
mod rooms_list;
pub mod tracing_writer;

struct TuiState {
    log_scroll_state: Option<usize>,
    rooms_list_state: ListState,
    rooms_entity_list_state: ListState,
    log: Option<TuiWriter>,
    axum_handle: axum_server::Handle,
    state: crate::State,
    app_focus: AppFocus,
    room_tab: RoomTabs,
}

#[derive(PartialEq, Eq)]
enum AppFocus {
    RoomList,
    Tracing,
    EntityList,
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
                return;
            }
            KeyCode::Right => {
                self.app_focus = AppFocus::Tracing;
                self.log_scroll_state = Some(
                    self.log
                        .as_ref()
                        .map(|l| l.0.buffer.count())
                        .unwrap_or_default(),
                );
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
            AppFocus::EntityList => match code {
                KeyCode::Up => {
                    self.rooms_entity_list_state.select(Some(
                        self.rooms_entity_list_state
                            .selected()
                            .map(|s| s.saturating_sub(1))
                            .unwrap_or_default(),
                    ));
                }
                KeyCode::Down => {
                    self.rooms_entity_list_state.select(Some(
                        self.rooms_entity_list_state
                            .selected()
                            .map(|s| s + 1)
                            .unwrap_or_default(),
                    ));
                }
                _ => {}
            },
            AppFocus::Tracing => match code {
                KeyCode::Up => {
                    let state = self.log_scroll_state.get_or_insert_with(|| {
                        self.log
                            .as_ref()
                            .map(|l| l.0.buffer.count())
                            .unwrap_or_default()
                    });
                    *state = state.saturating_sub(1);
                }
                KeyCode::Down => {
                    let count = self
                        .log
                        .as_ref()
                        .map(|l| l.0.buffer.count())
                        .unwrap_or_default();
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

    fn selected_room<'a>(&'a mut self) -> Option<Arc<Room>> {
        let room = if let Some(selected) = self.rooms_list_state.selected() {
            let Some(room) = self.state.room_map.iter().nth(selected) else {
                self.rooms_list_state.select(None);
                return None;
            };
            room
        } else {
            return None;
        };

        Some(Arc::clone(room.value()))
    }

    fn generate_room_qr(&mut self) -> String {
        if let Some(room) = self.selected_room() {
            QrCode::new(format!(
                "https://{}?code={}",
                self.state.config.accessible_host, room.room_config.code
            ))
            .unwrap()
            .render::<unicode::Dense1x2>()
            .build()
        } else {
            String::new()
        }
    }

    async fn close_room(&mut self) {
        let room = {
            let Some(room) = self.selected_room() else {
                return;
            };
            room.close().await.unwrap();
            room.room_config.code.clone()
        };

        self.state.room_map.remove(&room);
        self.generate_room_qr();
    }

    fn selected_room_receiver(&mut self) -> Option<Receiver<()>> {
        self.selected_room().map(|r| r.channel.subscribe())
        // room.changed().await
    }
}

pub async fn tui(
    axum_handle: axum_server::Handle,
    state: crate::State,
    log: Option<TuiWriter>,
    mut rx: Receiver<()>,
    enable_tui: bool,
) -> std::io::Result<()> {
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

    let mut tui_state = TuiState {
        log_scroll_state: None,
        rooms_list_state: ListState::default(),
        rooms_entity_list_state: ListState::default(),
        axum_handle,
        state,
        app_focus: AppFocus::RoomList,
        log,
        room_tab: RoomTabs::QRCode,
    };

    if enable_tui {
        enable_raw_mode()?;

        let mut stdout = BufWriter::new(stdout().lock());
        stdout.queue(EnterAlternateScreen)?.flush()?;

        let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

        let mut event_stream = crossterm::event::EventStream::new();

        terminal.clear()?;
        loop {
            terminal.draw(|frame| ui(frame, &mut tui_state))?;
            let mut event = None;
            if let Some(mut room) = tui_state.selected_room_receiver() {
                tokio::select! {
                    _ = &mut ctrl_c => break,
                    _ = &mut terminate => break,
                    c = rx.changed() => c.unwrap(),
                    t = room.changed() => t.unwrap(),
                    e = event_stream.next() => event = e
                }
            } else {
                tokio::select! {
                    _ = &mut ctrl_c => break,
                    _ = &mut terminate => break,
                    c = rx.changed() => c.unwrap(),
                    e = event_stream.next() => event = e
                }
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

                        match tui_state.room_tab {
                            RoomTabs::Entities | RoomTabs::Players => {
                                tui_state.app_focus = AppFocus::EntityList
                            }
                            RoomTabs::QRCode => {
                                tui_state.app_focus = AppFocus::RoomList;
                                tui_state.rooms_entity_list_state.select(None);
                            }
                        }
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

        disable_raw_mode()?;
        terminal
            .backend_mut()
            .queue(LeaveAlternateScreen)?
            .flush()?;
    } else {
        tokio::select! {
            _ = &mut ctrl_c => {},
            _ = &mut terminate => {},
        }
    }
    tracing::info!("Received termination signal shutting down");
    futures_util::future::try_join_all(
        tui_state
            .state
            .room_map
            .iter()
            .map(|r| Arc::clone(r.value()))
            .map(|r| async move { r.close().await }),
    )
    .await
    .unwrap();
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
        if tui_state.rooms_list_state.selected().is_some() {
            [Constraint::Ratio(3, 4), Constraint::Ratio(1, 4)]
        } else {
            [Constraint::Length(0), Constraint::Min(0)]
        },
    )
    .split(split_layout[1]);
    if let Some(ref tui_state_log) = tui_state.log {
        frame.render_stateful_widget(
            tui_state_log,
            room_log_split[1],
            &mut tui_state.log_scroll_state,
        );
    } else {
        frame.render_widget(
            Block::new().borders(Borders::all()).title("Log (disabled)"),
            room_log_split[1],
        );
    }
    frame.render_widget(tui_state.rooms_list(), split_layout[0]);
    frame.render_widget(tui_state.room_view(), room_log_split[0]);
}
