//! Terminal UI: a panel-based full-screen view over the same [`Session`] API the
//! CLI uses.
//!
//! The pieces are split so none of the logic needs a terminal to be tested:
//! [`App`] holds state and turns keys and job messages into state changes,
//! [`ui::draw`] renders an `&App`, [`job`] runs the slow work on a worker
//! thread, and `run` below is the thin event loop gluing them together.

mod app;
mod input;
mod job;
mod mouse;
mod state;
#[cfg(test)]
mod tests;
mod ui;

pub use app::{App, Effect};
pub use job::{JobEvent, JobRequest};

use crate::ops::Session;
use anyhow::{Context, Result};
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

/// Opens the UI on `session` and runs until the user quits.
pub fn run(session: Session) -> Result<()> {
    let mut app = App::new(session);
    let (sender, receiver) = mpsc::channel();
    let mut terminal =
        ratatui::try_init().context("starting the terminal UI (is stdout a terminal?)")?;
    // `try_init` made the panic hook restore the terminal. A panic on a worker
    // thread is caught and reported as a failed job, so it must not tear the
    // screen down; leave the hook to every other thread.
    let restoring = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if std::thread::current().name() != Some(job::JOB_THREAD) {
            let _ = execute!(std::io::stdout(), DisableMouseCapture);
            restoring(info);
        }
    }));
    let result = execute!(std::io::stdout(), EnableMouseCapture)
        .context("turning on mouse reporting")
        .and_then(|()| event_loop(&mut terminal, &mut app, &sender, &receiver));
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    drop(std::panic::take_hook());
    result
}

fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    sender: &Sender<JobEvent>,
    receiver: &Receiver<JobEvent>,
) -> Result<()> {
    let start = |request: JobRequest, app: &App| {
        job::spawn(request, app.context(), sender.clone());
    };
    loop {
        if let Effect::Start(request) = app.pump() {
            start(request, app);
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(Duration::from_millis(100))? {
            let effect = match event::read()? {
                Event::Key(key) if key.kind != KeyEventKind::Release => app.handle_key(key),
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                _ => Effect::None,
            };
            match effect {
                Effect::Quit => return Ok(()),
                Effect::Start(request) => start(request, app),
                Effect::None => {}
            }
        }
        while let Ok(event) = receiver.try_recv() {
            app.on_event(event);
        }
        app.frame = app.frame.wrapping_add(1);
    }
}
