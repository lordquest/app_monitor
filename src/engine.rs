//! Background work: resolving releases and downloading files.
//!
//! All I/O happens on a shared tokio runtime; the UI talks to it through an
//! `mpsc` channel and never blocks.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

use tokio::runtime::Runtime;
use tokio::sync::Semaphore;

use crate::download::{self, DlSpec, Progress};
use crate::http::Http;
use crate::model::{Release, Track};

/// How many items may be resolved at the same time.
///
/// A refresh used to spawn one task per tracked item with no ceiling; with a few
/// dozen items that is hundreds of concurrent requests against the same two or
/// three sites, which trips their bot protection (403 / Cloudflare) and makes the
/// whole batch slower than resolving them a few at a time.
const RESOLVE_CONCURRENCY: usize = 6;

pub enum UiMsg {
    Resolved(String, Box<Release>),
    ResolveFailed(String, String),
    DlProgress {
        id: String,
        p: Progress,
    },
    DlDone {
        id: String,
        path: String,
        #[allow(dead_code)]
        size: u64,
        sha_ok: Option<bool>,
        #[allow(dead_code)]
        sha_actual: Option<String>,
        downloaded: u64,
    },
    DlFailed {
        id: String,
        error: String,
        downloaded: u64,
        total: u64,
    },
    DlCanceled {
        id: String,
        paused: bool,
        downloaded: u64,
        total: u64,
    },
}

pub struct Engine {
    pub tx: Sender<UiMsg>,
    pub rx: Receiver<UiMsg>,
    rt: Arc<Runtime>,
    resolve_sem: Arc<Semaphore>,
}

impl Engine {
    pub fn new() -> Self {
        let rt = Runtime::new().expect("tokio runtime");
        let (tx, rx) = mpsc::channel();
        Self {
            tx,
            rx,
            rt: Arc::new(rt),
            resolve_sem: Arc::new(Semaphore::new(RESOLVE_CONCURRENCY)),
        }
    }

    pub fn start_resolve(&self, http: Arc<Http>, track: Track) {
        let tx = self.tx.clone();
        let sem = self.resolve_sem.clone();
        self.rt.spawn(async move {
            // Queue behind the semaphore instead of opening another connection:
            // the permit is released when this future ends.
            let Ok(_permit) = sem.acquire().await else {
                return;
            };
            match crate::provider::resolve(&http, &track).await {
                Ok(r) => {
                    let _ = tx.send(UiMsg::Resolved(track.id.clone(), Box::new(r)));
                }
                Err(e) => {
                    let _ = tx.send(UiMsg::ResolveFailed(track.id.clone(), e));
                }
            }
        });
    }

    pub fn start_download(
        &self,
        http: Arc<Http>,
        spec: DlSpec,
        cancel: Arc<std::sync::atomic::AtomicBool>,
        paused: Arc<std::sync::atomic::AtomicBool>,
    ) {
        let tx = self.tx.clone();
        self.rt.spawn(async move {
            let id = spec.id.clone();
            let tx_prog = tx.clone();
            let outcome = download::run(&http, &spec, cancel, paused, move |p| {
                let _ = tx_prog.send(UiMsg::DlProgress {
                    id: id.clone(),
                    p,
                });
            })
            .await;
            match outcome {
                download::Outcome::Done {
                    path,
                    size,
                    sha_ok,
                    sha_actual,
                } => {
                    let _ = tx.send(UiMsg::DlDone {
                        id: spec.id.clone(),
                        path,
                        size,
                        sha_ok,
                        sha_actual,
                        downloaded: size,
                    });
                }
                download::Outcome::Canceled {
                    paused,
                    downloaded,
                    total,
                } => {
                    let _ = tx.send(UiMsg::DlCanceled {
                        id: spec.id.clone(),
                        paused,
                        downloaded,
                        total,
                    });
                }
                download::Outcome::Failed {
                    error,
                    downloaded,
                    total,
                } => {
                    let _ = tx.send(UiMsg::DlFailed {
                        id: spec.id.clone(),
                        error,
                        downloaded,
                        total,
                    });
                }
            }
        });
    }
}
