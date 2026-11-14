//! Background jobs. The UI thread never touches the network or the install
//! state: each job runs on a worker thread that loads its own [`Session`]
//! from the host config, and sends log lines and one result back over a
//! channel. Only one job runs at a time (the app enforces it).

use crate::deploy::{self, Edited, Item};
use crate::gitops::{self, Incoming, Outgoing};
use crate::ops::{ApplyReport, Log, Session};
use crate::plan::{Mode, Options, Plan};
use crate::schedule;
use anyhow::Result;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

/// Name of worker threads, so the panic hook can tell them apart.
pub const JOB_THREAD: &str = "dfutil-job";

/// What a worker needs to load its own session.
#[derive(Clone, Debug)]
pub struct JobContext {
    pub host_path: PathBuf,
    pub home: Option<PathBuf>,
    pub offline: bool,
}

impl JobContext {
    pub fn from_session(session: &Session) -> Self {
        Self {
            host_path: session.host_path.clone(),
            home: session.home_override.clone(),
            offline: session.remote.offline,
        }
    }
}

/// Something the user (or a refresh) asked the worker to do.
#[derive(Clone, Debug)]
pub enum JobRequest {
    /// Version lookups for every tool (network).
    LoadPlan,
    /// Deploy dry run plus the capture preview.
    LoadDotfiles,
    /// Incoming (fetches) and outgoing changes of every repo.
    LoadSync,
    LoadSchedule,
    /// Applies a plan the user confirmed.
    Apply(Plan),
    Reinstall(Vec<String>),
    Install(Vec<String>),
    Uninstall(Vec<String>),
    Rollback(String),
    Deploy,
    Capture,
    SetPalette(String),
    Sync,
    Update,
}

impl JobRequest {
    /// Short description for the status line and the log.
    pub fn label(&self) -> String {
        match self {
            JobRequest::LoadPlan => "looking up tool versions".into(),
            JobRequest::LoadDotfiles => "checking dotfiles".into(),
            JobRequest::LoadSync => "checking repos for changes".into(),
            JobRequest::LoadSchedule => "reading the update schedule".into(),
            JobRequest::Apply(plan) => format!("applying {} change(s)", plan.changes().count()),
            JobRequest::Reinstall(names) => format!("reinstalling {}", names.join(", ")),
            JobRequest::Install(names) => format!("installing {}", names.join(", ")),
            JobRequest::Uninstall(names) => format!("uninstalling {}", names.join(", ")),
            JobRequest::Rollback(name) => format!("rolling back {name}"),
            JobRequest::Deploy => "deploying dotfiles".into(),
            JobRequest::Capture => "capturing edits".into(),
            JobRequest::SetPalette(name) => format!("switching palette to {name}"),
            JobRequest::Sync => "syncing repos".into(),
            JobRequest::Update => "updating (sync, then install changes)".into(),
        }
    }

    /// Whether the job changes anything on disk. Mutating jobs make the UI
    /// reload its session and refresh its views afterwards.
    pub fn mutates(&self) -> bool {
        !matches!(
            self,
            JobRequest::LoadPlan
                | JobRequest::LoadDotfiles
                | JobRequest::LoadSync
                | JobRequest::LoadSchedule
        )
    }
}

/// What a finished job produced.
#[derive(Debug)]
pub enum JobOutput {
    Plan(Plan),
    Dotfiles {
        items: Vec<Item>,
        edited: Vec<Edited>,
        problems: Vec<String>,
    },
    Sync {
        incoming: Vec<Incoming>,
        outgoing: Vec<Outgoing>,
    },
    Schedule(schedule::Status),
    Report(ApplyReport),
    Deployed(Vec<Item>),
    Captured(Vec<Edited>),
    Message(String),
}

/// Messages from a worker to the UI.
#[derive(Debug)]
pub enum JobEvent {
    Log(String),
    /// The job's result; the error text is already formatted.
    Done(Result<JobOutput, String>),
}

/// Runs one request to completion on the current thread.
pub fn run(request: JobRequest, context: &JobContext, log: Log) -> Result<JobOutput> {
    let mut session = Session::load(&context.host_path, context.home.as_deref(), context.offline)?;
    match request {
        JobRequest::LoadPlan => Ok(JobOutput::Plan(session.plan(&Options::default(), true)?)),
        JobRequest::LoadDotfiles => {
            let mut problems = Vec::new();
            let items = session
                .deploy(&deploy::Options {
                    dry_run: true,
                    force: false,
                })
                .unwrap_or_else(|error| {
                    problems.push(format!("deploy preview: {error:#}"));
                    Vec::new()
                });
            let edited = session.capture(false).unwrap_or_else(|error| {
                problems.push(format!("capture preview: {error:#}"));
                Vec::new()
            });
            Ok(JobOutput::Dotfiles {
                items,
                edited,
                problems,
            })
        }
        JobRequest::LoadSync => Ok(JobOutput::Sync {
            // An offline session never fetches.
            incoming: gitops::incoming(&session.workspace, !context.offline),
            outgoing: gitops::outgoing(&session.workspace),
        }),
        JobRequest::LoadSchedule => Ok(JobOutput::Schedule(schedule::status(
            &session.workspace.paths.home,
        ))),
        JobRequest::Apply(plan) => {
            let _locks = session.lock()?;
            Ok(JobOutput::Report(session.apply_plan(&plan, true, log)?))
        }
        JobRequest::Reinstall(names) => {
            let names: BTreeSet<String> = names.into_iter().collect();
            let options = Options {
                mode: Mode::Reinstall(Some(names.clone())),
                only: Some(names),
            };
            let _locks = session.lock()?;
            let plan = session.plan(&options, false)?;
            Ok(JobOutput::Report(session.apply_plan(&plan, false, log)?))
        }
        JobRequest::Install(names) => Ok(JobOutput::Report(session.install(&names, log)?)),
        JobRequest::Uninstall(names) => Ok(JobOutput::Report(session.uninstall(&names, log)?)),
        JobRequest::Rollback(name) => {
            let version = session.rollback(&name)?;
            Ok(JobOutput::Message(format!("{name} is back on {version}")))
        }
        JobRequest::Deploy => Ok(JobOutput::Deployed(
            session.deploy(&deploy::Options::default())?,
        )),
        JobRequest::Capture => Ok(JobOutput::Captured(session.capture(true)?)),
        JobRequest::SetPalette(name) => Ok(JobOutput::Deployed(session.set_palette(&name)?)),
        JobRequest::Sync => Ok(JobOutput::Report(session.sync(log)?)),
        JobRequest::Update => Ok(JobOutput::Report(session.update(log)?)),
    }
}

/// Formats a panic payload for the log.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|text| text.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".into())
}

/// Runs `request` on a new worker thread. Log lines and the result arrive on
/// `sender`; a panic in the job becomes an error result instead of a hang.
pub fn spawn(request: JobRequest, context: JobContext, sender: Sender<JobEvent>) {
    let thread_sender = sender.clone();
    let spawned = std::thread::Builder::new()
        .name(JOB_THREAD.into())
        .spawn(move || {
            let log_sender = Mutex::new(thread_sender.clone());
            let log = move |line: String| {
                if let Ok(sender) = log_sender.lock() {
                    sender.send(JobEvent::Log(line)).ok();
                }
            };
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run(request, &context, &log)
            }));
            let result = match outcome {
                Ok(result) => result.map_err(|error| format!("{error:#}")),
                Err(payload) => Err(format!("the job panicked: {}", panic_text(&*payload))),
            };
            thread_sender.send(JobEvent::Done(result)).ok();
        });
    if let Err(error) = spawned {
        sender
            .send(JobEvent::Done(Err(format!(
                "couldn't start a worker thread: {error}"
            ))))
            .ok();
    }
}
