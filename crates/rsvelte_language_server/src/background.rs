//! Loading the TypeScript projects no editor document has asked for yet.
//!
//! Projects load lazily: the first document a project owns pays for it
//! (`TsgoRuntime::ensure_project_for`). Workspace-wide requests — workspace
//! symbols, references, rename, call hierarchy — only see the projects tsgo
//! has, so once the editor's first project has been served and the editor
//! is quiet, the server loads the remaining projects that own a component,
//! one at a time, on this module's thread: discovery, projection on a couple
//! of threads and the body writes all happen here, and the main loop only
//! installs the finished overlay. A workspace-wide request that arrives
//! before they are all loaded loads the rest first.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use lsp_types::{FileChangeType, Uri};

use crate::tsgo_overlay::{ProjectSpec, TsgoOverlay, component_projects};

/// How long the editor must have been quiet before the next background step.
pub const IDLE: Duration = Duration::from_millis(1000);

/// Projection threads a background load uses, leaving the editor's work the
/// rest of the machine.
pub const PROJECTION_THREADS: usize = 2;

/// Work for the background thread.
pub enum Job {
    /// Find the projects that own a component in these workspace folders.
    Enumerate {
        generation: u64,
        roots: Vec<PathBuf>,
    },
    /// Load one project and write its bodies.
    Load { generation: u64, spec: ProjectSpec },
}

/// What the background thread finished.
pub enum Done {
    Enumerated {
        generation: u64,
        projects: Vec<ProjectSpec>,
    },
    Loaded {
        generation: u64,
        spec: ProjectSpec,
        loaded: Result<Box<LoadedProject>, String>,
    },
}

/// A project loaded off the main loop, its bodies on disk.
pub struct LoadedProject {
    pub overlay: TsgoOverlay,
    /// The body writes tsgo has to hear about.
    pub events: Vec<(Uri, FileChangeType)>,
    pub elapsed: Duration,
}

/// The background thread. It ends when this handle is dropped.
pub struct Worker {
    jobs: Sender<Job>,
    done: Receiver<Done>,
}

impl Worker {
    #[must_use]
    pub fn spawn() -> Option<Self> {
        let (jobs, job_receiver) = unbounded::<Job>();
        let (done_sender, done) = unbounded();
        std::thread::Builder::new()
            .name("rsvelte-background-projects".to_string())
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                for job in job_receiver {
                    let done = match job {
                        Job::Enumerate { generation, roots } => Done::Enumerated {
                            generation,
                            projects: roots
                                .iter()
                                .flat_map(|root| component_projects(root))
                                .collect(),
                        },
                        Job::Load { generation, spec } => {
                            let loaded = load(&spec, PROJECTION_THREADS).map(Box::new);
                            Done::Loaded {
                                generation,
                                spec,
                                loaded,
                            }
                        }
                    };
                    if done_sender.send(done).is_err() {
                        return;
                    }
                }
            })
            .ok()?;
        Some(Self { jobs, done })
    }

    pub fn submit(&self, job: Job) -> bool {
        self.jobs.send(job).is_ok()
    }

    /// Receiver suitable for the server's `select!` loop.
    #[must_use]
    pub const fn done(&self) -> &Receiver<Done> {
        &self.done
    }
}

/// Load the project `spec` names and write its bodies (shared by the
/// background thread and a workspace-wide request that cannot wait for it).
pub fn load(spec: &ProjectSpec, threads: usize) -> Result<LoadedProject, String> {
    let started = Instant::now();
    let mut overlay = TsgoOverlay::build_for(spec).map_err(|error| error.to_string())?;
    overlay
        .load_with_threads(threads)
        .map_err(|error| error.to_string())?;
    let events = crate::server::persist_bodies(&overlay);
    Ok(LoadedProject {
        overlay,
        events,
        elapsed: started.elapsed(),
    })
}
