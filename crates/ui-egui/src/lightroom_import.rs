//! Background Lightroom catalog inspection and import.
//!
//! Reading a catalog, probing its originals and staging its archive can take minutes on a
//! removable or network drive.  This task keeps that work off the owner thread, commits prepared
//! data only on the owner thread, then finishes the archive index on a worker.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use lcc_engine::activity::{Cancel, TaskGuard};
use lcc_engine::lightroom_job::{LightroomProgress, LightroomStep};
use serde_json::{Value, json};

use crate::LccApp;

#[derive(Clone, Copy)]
enum Kind {
    Inspect,
    Import,
}

enum Message {
    Folders(std::collections::BTreeSet<String>, lcc_engine::lightroom_job::LightroomLibraryToken, mpsc::Sender<Result<(), String>>),
    Inspected(Result<Value, String>),
    Batch(Box<lcc_engine::lightroom_job::PreparedLightroom>, mpsc::Sender<Result<lcc_engine::lightroom_job::LightroomResume, String>>),
    Finished(Result<(), String>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Reading,
    Committing,
}

/// A Lightroom task in flight.  The receiver is polled from the egui owner thread; the activity stack shows it.
pub struct LightroomTask {
    kind: Kind,
    path: PathBuf,
    cancel: Arc<AtomicBool>,
    progress: LightroomProgress,
    rx: mpsc::Receiver<Message>,
    phase: Phase,
    cancelled: bool,
    guard: TaskGuard,
    priority: lcc_engine::lightroom_job::LightroomPriority,
    partial: Value,
}

impl LightroomTask {
    #[cfg(not(target_arch = "wasm32"))]
    fn new(
        s: &lcc_engine::Session,
        kind: Kind,
        path: PathBuf,
        cancel: Arc<AtomicBool>,
        progress: LightroomProgress,
        rx: mpsc::Receiver<Message>,
    ) -> Self {
        let label = match kind {
            Kind::Inspect => "Reading Lightroom catalog",
            Kind::Import => "Importing Lightroom catalog",
        };
        let guard = s.activity.start("lightroom", label, Cancel::Flag(cancel.clone()));
        let task = Self {
            kind,
            path,
            cancel,
            progress,
            rx,
            phase: Phase::Reading,
            cancelled: false,
            guard,
            priority: Default::default(),
            partial: json!({}),
        };
        task.refresh();
        task
    }

    /// Move to `phase`; only reading the catalog can stop, adding the photos runs to the end.
    fn enter(&mut self, phase: Phase) {
        self.phase = phase;
        self.guard.set_cancellable(phase == Phase::Reading);
        self.refresh();
    }

    /// Bring the activity row up to date: while reading, the step under way and its own count
    /// (reading the photos takes nearly all of an import, so it is counted, not waited out);
    /// then what is being done.
    fn refresh(&self) {
        let (step, done, total) = self.progress.step();
        let detail = match (self.phase, self.kind, step) {
            (Phase::Reading, Kind::Inspect, _) => "",
            (Phase::Reading, Kind::Import, LightroomStep::Catalog) => crate::i18n::tr("Reading Lightroom catalog…"),
            (Phase::Reading, Kind::Import, LightroomStep::ReadPhotos) => crate::i18n::tr("Reading photos…"),
            (Phase::Reading, Kind::Import, LightroomStep::AddPhotos) => crate::i18n::tr("Adding photos…"),
            (Phase::Committing, ..) => crate::i18n::tr("Adding Lightroom photos…"),
        };
        if self.phase == Phase::Reading {
            self.guard.progress(done as u64, total as u64);
        } else {
            self.guard.progress(0, 0);
        }
        self.guard.detail(detail);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn status(&self) -> Value {
        let kind = match self.kind {
            Kind::Inspect => "inspect",
            Kind::Import => "import",
        };
        let phase = match self.phase {
            Phase::Reading => "reading",
            Phase::Committing => "committing",
        };
        let (step, done, total) = self.progress.step();
        let step = match step {
            LightroomStep::Catalog => "catalog",
            LightroomStep::ReadPhotos => "readPhotos",
            LightroomStep::AddPhotos => "addPhotos",
        };
        json!({
            "running": true,
            "kind": kind,
            "path": self.path,
            "phase": phase,
            "step": step,
            "done": done,
            "total": total,
            "fraction": self.progress.fraction(),
            "imported": self.partial["imported"].as_u64().unwrap_or(0),
            "cancelled": self.cancelled || self.cancel.load(Ordering::Relaxed),
        })
    }

    fn cancel(&mut self) {
        self.cancelled = true;
        self.cancel.store(true, Ordering::Relaxed);
    }
}

impl Drop for LightroomTask {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(target_arch = "wasm32")]
fn unsupported_wasm() -> Result<Value, String> {
    Err("Lightroom catalog import is unavailable in browser builds; open the catalog in the native app".into())
}

/// Whether a Lightroom task currently owns the catalog transition.
pub fn is_running(app: &LccApp) -> bool {
    app.lightroom.is_some()
}

#[cfg(not(target_arch = "wasm32"))]
fn busy(app: &LccApp) -> Result<(), String> {
    if app.lightroom.is_some() {
        return Err("a Lightroom catalog task is already running".into());
    }
    if app.import.is_some() || app.scan.is_some() {
        return Err("an import is running".into());
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_inspect(
    path: PathBuf,
    cancel: Arc<AtomicBool>,
    progress: LightroomProgress,
    tx: mpsc::Sender<Message>,
    ctx: egui::Context,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("lc-lightroom-inspect".into())
        .spawn(move || {
            let result = lcc_engine::guard::catch("Lightroom inspect", || {
                lcc_engine::lightroom_catalog::read_with_progress(&path, &cancel, &progress.catalog_total, &progress.catalog_done)
            })
            .and_then(|r| r.map(inspect_report));
            let _ = tx.send(Message::Inspected(result));
            ctx.request_repaint();
        })
        .map(|_| ())
        .map_err(|e| format!("could not start Lightroom inspect: {e}"))
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_prepare(
    mut job: lcc_engine::lightroom_job::LightroomJob,
    priority: lcc_engine::lightroom_job::LightroomPriority,
    cancel: Arc<AtomicBool>,
    tx: mpsc::Sender<Message>,
    ctx: egui::Context,
) -> Result<(), String> {
    std::thread::Builder::new()
        .name("lc-lightroom-import".into())
        .spawn(move || {
            let result = lcc_engine::guard::catch("Lightroom import", || {
                let token = job.token().clone();
                job.prepare_with_folders(
                    &cancel,
                    &priority,
                    |folders| {
                        let (ack, rx) = mpsc::channel();
                        tx.send(Message::Folders(folders, token, ack))
                            .map_err(|_| lcc_engine::EngineError::Cancelled("Lightroom owner closed".into()))?;
                        ctx.request_repaint();
                        rx.recv()
                            .map_err(|_| lcc_engine::EngineError::Cancelled("Lightroom owner closed".into()))?
                            .map_err(lcc_engine::EngineError::Cancelled)
                    },
                    |prepared| {
                        let (ack, rx) = mpsc::channel();
                        if let Err(mpsc::SendError(Message::Batch(prepared, _))) = tx.send(Message::Batch(Box::new(prepared), ack)) {
                            prepared.rollback();
                            return Err(lcc_engine::EngineError::Cancelled("Lightroom owner closed".into()));
                        }
                        ctx.request_repaint();
                        rx.recv()
                            .map_err(|e| lcc_engine::EngineError::BadParams { cmd: "library.importLightroom".into(), msg: e.to_string() })?
                            .map_err(|msg| lcc_engine::EngineError::BadParams { cmd: "library.importLightroom".into(), msg })
                    },
                )
            })
            .and_then(|r| r.map_err(|e| e.to_string()));
            let _ = tx.send(Message::Finished(result));
            ctx.request_repaint();
        })
        .map(|_| ())
        .map_err(|e| format!("could not start Lightroom import: {e}"))
}

#[cfg(not(target_arch = "wasm32"))]
fn inspect_report(data: lcc_engine::lightroom_catalog::CatalogImport) -> Value {
    json!({
        "photos": data.photos.len(),
        "collections": data.collections.iter().filter(|r| r.get("systemOnly").and_then(Value::as_f64).unwrap_or(0.0) == 0.0).count(),
        "missing": data.photos.iter().filter(|p| !p.original_is_file()).map(|p| &p.path).collect::<Vec<_>>(),
        "warnings": data.warnings,
    })
}

/// The task is over: keep its result (with the catalog's `path`) for `status` and say how it went.
fn terminal(app: &mut LccApp, ctx: &egui::Context, task: &LightroomTask, mut value: Value) {
    if matches!(task.kind, Kind::Import) {
        app.session.catalog.set_import_folders(Default::default());
    }
    let kind = task.kind;
    if let Some(o) = value.as_object_mut() {
        o.entry("path").or_insert_with(|| json!(task.path));
        if let Some(library) = app.session.library.as_ref().filter(|l| l.on_disk) {
            o.entry("library").or_insert_with(|| json!(library.dir.to_string_lossy()));
        }
    }
    let mut message = if let Some(e) = value.get("error").and_then(Value::as_str) {
        let warning = value.get("indexWarning").and_then(Value::as_str).map_or(String::new(), |w| format!("; index warning: {w}"));
        if matches!(kind, Kind::Inspect) {
            crate::i18n::tr_format!("Lightroom inspection failed: {e}{warning}", e = e, warning = warning)
        } else {
            crate::i18n::tr_format!("Lightroom import failed: {e}{warning}", e = e, warning = warning)
        }
    } else if value.get("cancelled").and_then(Value::as_bool).unwrap_or(false) {
        crate::i18n::tr(if matches!(kind, Kind::Inspect) { "Lightroom inspection cancelled" } else { "Lightroom import cancelled" }).into()
    } else if matches!(kind, Kind::Inspect) {
        let photos = value["photos"].as_u64().unwrap_or(0);
        let collections = value["collections"].as_u64().unwrap_or(0);
        let missing = value["missing"].as_array().map_or(0, Vec::len);
        crate::i18n::tr_format!(
            "Lightroom catalog inspected: {photos} photos, {collections} collections, {missing} missing",
            photos = photos,
            collections = collections,
            missing = missing
        )
    } else {
        let report = value.get("report").unwrap_or(&value);
        let imported = report["imported"].as_u64().unwrap_or(0);
        let failed = report["failed"].as_array().map_or(0, Vec::len);
        let warning = value.get("indexWarning").and_then(Value::as_str);
        if let Some(warning) = warning {
            crate::i18n::tr_format!(
                "Lightroom import complete: {imported} photos added; index warning: {warning}",
                imported = imported,
                warning = warning
            )
        } else if failed == 0 {
            crate::i18n::tr_format!("Lightroom import complete: {imported} photos added", imported = imported)
        } else {
            crate::i18n::tr_format!("Lightroom import complete: {imported} photos added, {failed} failed", imported = imported, failed = failed)
        }
    };
    let retained = value.get("report").and_then(|r| r.get("imported")).and_then(Value::as_u64).unwrap_or(0);
    if retained > 0 && (value.get("error").is_some() || value.get("cancelled") == Some(&Value::Bool(true))) {
        message.push_str(&format!("; {retained} already imported photos remain in the library"));
    }
    if matches!(kind, Kind::Import) && (retained > 0 || value.get("cancelled") != Some(&Value::Bool(true))) {
        app.lightroom_result = Some(result_dialog(&value, &message));
        show_result(app);
    }
    app.lightroom_last = Some(value);
    app.lightroom = None;
    app.toast_for(ctx, message, 6.0);
}

fn show_result(app: &mut LccApp) {
    if app.ui.dialog.is_none() {
        app.ui.dialog = app.lightroom_result.take();
    }
}

fn result_dialog(value: &Value, summary: &str) -> crate::state::Dialog {
    let report = value.get("report").unwrap_or(value);
    let mut details = Vec::new();
    if let Some(photos) = report["photos"].as_u64() {
        details.push(crate::i18n::tr_format!(
            "{photos} catalog records processed; {collections} collections.",
            photos = photos,
            collections = report["collections"].as_u64().unwrap_or(0)
        ));
    }
    if let Some(failures) = report["failed"].as_array() {
        details
            .extend(failures.iter().take(4).map(|failure| format!("{}: {}", failure[0].as_str().unwrap_or(""), failure[1].as_str().unwrap_or(""))));
    }
    let missing = report["missing"].as_array().map_or(0, Vec::len);
    if missing > 0 {
        details.push(crate::i18n::tr_format!(
            "{missing} originals are missing. Their catalog records are available; use Find Missing Photos to relink them.",
            missing = missing
        ));
    }
    if let Some(warning) = value.get("indexWarning").and_then(Value::as_str) {
        details.push(warning.to_string());
    }
    if let Some(warnings) = report["warnings"].as_array() {
        // Results can contain one warning per image; keep the dialog bounded.
        details.extend(warnings.iter().take(8).filter_map(Value::as_str).map(str::to_string));
        if warnings.len() > 8 {
            details.push(crate::i18n::tr_format!("{count} more warnings.", count = warnings.len() - 8));
        }
    }
    let first = value.get("error").is_none().then(|| report["mapping"].as_object()?.values().filter_map(Value::as_u64).min()).flatten();
    crate::state::Dialog::LightroomResult {
        summary: summary.to_string(),
        library: report["library"].as_str().or_else(|| value["library"].as_str()).map(str::to_string),
        details,
        first,
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn start_inspect(app: &mut LccApp, path: PathBuf, ctx: &egui::Context) -> Result<Value, String> {
    busy(app)?;
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = LightroomProgress::default();
    let (tx, rx) = mpsc::channel();
    spawn_inspect(path.clone(), cancel.clone(), progress.clone(), tx, ctx.clone())?;
    app.lightroom_last = None;
    app.lightroom = Some(LightroomTask::new(&app.session, Kind::Inspect, path.clone(), cancel, progress, rx));
    Ok(json!({"running": true, "kind": "inspect", "path": path}))
}

#[cfg(not(target_arch = "wasm32"))]
fn start_import(app: &mut LccApp, path: PathBuf, update_existing: bool, ctx: &egui::Context) -> Result<Value, String> {
    busy(app)?;
    let job = lcc_engine::lightroom_job::LightroomJob::new(&mut app.session, path.clone(), update_existing).map_err(|e| e.to_string())?;
    let cancel = Arc::new(AtomicBool::new(false));
    let progress = job.progress();
    let (tx, rx) = mpsc::channel();
    let priority = lcc_engine::lightroom_job::LightroomPriority::default();
    update_location(&priority, &app.session);
    spawn_prepare(job, priority.clone(), cancel.clone(), tx, ctx.clone())?;
    app.lightroom_last = None;
    app.lightroom = Some(LightroomTask::new(&app.session, Kind::Import, path.clone(), cancel, progress, rx));
    if let Some(task) = &mut app.lightroom {
        task.priority = priority;
    }
    Ok(json!({"running": true, "kind": "import", "path": path}))
}

/// Run Lightroom UI commands before the generic engine dispatcher.
pub fn command(app: &mut LccApp, id: &str, p: &Value, ctx: &egui::Context) -> Result<Value, String> {
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (app, id, p, ctx);
        return unsupported_wasm();
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = p.get("path").and_then(Value::as_str).map(PathBuf::from);
        let wait = p.get("wait").and_then(Value::as_bool).unwrap_or(false);
        let status = p.get("status").and_then(Value::as_bool).unwrap_or(false) || path.is_none();
        if status {
            if p.get("cancel").and_then(Value::as_bool).unwrap_or(false)
                && let Some(task) = app.lightroom.as_mut()
            {
                task.cancel();
            }
            if wait && app.lightroom.is_some() {
                return wait_for(app, ctx, std::time::Duration::from_secs(600));
            }
            return Ok(app.lightroom.as_ref().map(LightroomTask::status).or_else(|| app.lightroom_last.clone()).unwrap_or(Value::Null));
        }
        let Some(path) = path else { return Err("missing `path`".into()) };
        let result = if id == "library.inspectLightroom" {
            start_inspect(app, path, ctx)
        } else {
            start_import(app, path, p.get("updateExisting").and_then(Value::as_bool).unwrap_or(false), ctx)
        }?;
        if wait {
            return wait_for(app, ctx, std::time::Duration::from_secs(600));
        }
        Ok(result)
    }
}

/// Poll owner-thread work, commit a prepared import, and finish its archive index.
pub fn tick(app: &mut LccApp, ctx: &egui::Context) {
    show_result(app);
    let Some(mut task) = app.lightroom.take() else { return };
    update_location(&task.priority, &app.session);
    let message = match task.rx.try_recv() {
        Ok(m) => m,
        Err(mpsc::TryRecvError::Empty) => {
            task.refresh();
            app.lightroom = Some(task);
            return;
        }
        Err(mpsc::TryRecvError::Disconnected) => {
            terminal(app, ctx, &task, json!({"error": "Lightroom worker stopped unexpectedly", "report": task.partial}));
            return;
        }
    };
    // The task's cancel flag says whether it was cancelled (the worker only stops for it), not an
    // error's words: a real failure that mentions "cancelled" is still reported as a failure.
    match message {
        Message::Folders(folders, token, ack) => {
            if task.cancel.load(Ordering::Relaxed) || !token.matches(&app.session) {
                let _ = ack.send(Err("Lightroom import cancelled or library changed".into()));
            } else {
                app.session.catalog.set_import_folders(folders);
                let _ = ack.send(Ok(()));
            }
            app.lightroom = Some(task);
            ctx.request_repaint();
        }
        Message::Inspected(result) => match result {
            Ok(report) if !task.cancel.load(Ordering::Relaxed) => terminal(app, ctx, &task, report),
            Ok(_) => terminal(app, ctx, &task, json!({"cancelled": true})),
            Err(_) if task.cancel.load(Ordering::Relaxed) => terminal(app, ctx, &task, json!({"cancelled": true})),
            Err(e) => terminal(app, ctx, &task, json!({"error": e})),
        },
        Message::Batch(prepared, ack) => {
            if task.cancel.load(Ordering::Relaxed) || !prepared.token().matches(&app.session) {
                prepared.rollback();
                let _ = ack.send(Err("Lightroom import cancelled or library changed".into()));
                app.lightroom = Some(task);
                return;
            }
            task.enter(Phase::Committing);
            let mut completion = None;
            let result = app.session.execute_fn("library.importLightroom", |s| {
                let done = lcc_engine::lightroom_job::commit_prepared(s, *prepared)?;
                let report = done.report.clone();
                completion = Some(done);
                Ok(report)
            });
            if let Some(done) = completion {
                merge_report(&mut task.partial, &done.report);
                let resume = lcc_engine::lightroom_job::LightroomResume::new(&mut app.session, done).map_err(|e| e.to_string());
                let _ = ack.send(match result {
                    Ok(_) => resume,
                    Err(e) => Err(e.to_string()),
                });
            } else {
                let _ = ack.send(Err(result.err().map_or_else(|| "Lightroom import did not commit".into(), |e| e.to_string())));
            }
            task.enter(Phase::Reading);
            app.lightroom = Some(task);
        }
        Message::Finished(result) => {
            let value = match result {
                Ok(()) => task.partial.clone(),
                Err(_) if task.cancel.load(Ordering::Relaxed) => json!({"cancelled": true, "report": task.partial}),
                Err(e) => json!({"error": e, "report": task.partial}),
            };
            terminal(app, ctx, &task, value);
        }
    }
    ctx.request_repaint();
}

/// Wait for a task in control/headless mode while still applying owner-thread completion.
#[cfg(not(target_arch = "wasm32"))]
pub fn wait_for(app: &mut LccApp, ctx: &egui::Context, timeout: std::time::Duration) -> Result<Value, String> {
    let start = std::time::Instant::now();
    while app.lightroom.is_some() {
        tick(app, ctx);
        if start.elapsed() >= timeout {
            return Err("timed out waiting for Lightroom task".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    Ok(app.lightroom_last.clone().unwrap_or(Value::Null))
}

#[cfg(target_arch = "wasm32")]
pub fn wait_for(_app: &mut LccApp, _ctx: &egui::Context, _timeout: std::time::Duration) -> Result<Value, String> {
    unsupported_wasm()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lcc_engine::lightroom_catalog::CatalogImport;

    /// A task as `start_import` makes it, fed by a channel the test holds instead of a worker.
    fn fake_import(h: &crate::headless::Headless) -> (LightroomTask, mpsc::Sender<Message>, Arc<AtomicBool>, LightroomProgress) {
        let (tx, rx) = mpsc::channel();
        let (cancel, progress) = (Arc::new(AtomicBool::new(false)), LightroomProgress::default());
        let task = LightroomTask::new(&h.app.session, Kind::Import, "fixture.lrcat".into(), cancel.clone(), progress.clone(), rx);
        (task, tx, cancel, progress)
    }

    #[test]
    fn lightroom_announced_folders_are_selectable_before_photos_and_cleared_on_failure() {
        let mut h = demo();
        let (task, tx, _, _) = fake_import(&h);
        let job = lcc_engine::lightroom_job::LightroomJob::new(&mut h.app.session, PathBuf::new(), false).unwrap();
        let (ack, rx) = mpsc::channel();
        let initial = h.app.session.catalog.len();
        h.app.lightroom = Some(task);
        tx.send(Message::Folders(["/offline/a".into(), "/offline/b".into()].into(), job.token().clone(), ack)).unwrap();
        tick(&mut h.app, &h.view.ctx);
        assert!(rx.recv().unwrap().is_ok());
        assert_eq!(h.app.session.catalog.len(), initial, "manifest is not a fake imported photo");
        let folders = h.app.caches.folder_tree(&h.app.session.catalog);
        assert!(serde_json::to_string(&folders).unwrap().contains("/offline/b"));
        h.app.session.execute("library.source", &json!({"kind":"libraryFolder", "path":"/offline/b"})).unwrap();
        assert!(h.app.session.visible().is_empty());
        assert_eq!(h.app.session.source, lcc_engine::LibrarySource::LibraryFolder);
        update_location(&h.app.lightroom.as_ref().unwrap().priority, &h.app.session);
        let serialized = serde_json::to_string(&h.app.session.catalog).unwrap();
        assert!(!serialized.contains("/offline/b"), "pending folders must not survive an interrupted import as library data");
        tx.send(Message::Finished(Err("read failed".into()))).unwrap();
        tick(&mut h.app, &h.view.ctx);
        assert!(!h.app.session.catalog.has_import_folders());
        assert!(h.app.session.execute("library.source", &json!({"kind":"libraryFolder", "path":"/offline/b"})).is_err());
    }

    fn demo() -> crate::headless::Headless {
        let app = crate::LccApp::new(lcc_engine::Session::with_demo(), crate::Services { png: None, ..Default::default() });
        let mut h = crate::headless::Headless::new(app, [1200.0, 800.0], 1.0);
        // no render left running at exit (see tests_activity::demo)
        h.settle(std::time::Duration::from_secs(20));
        h
    }

    #[test]
    fn lightroom_row_is_not_cancellable_while_committing() {
        let mut h = demo();
        let (task, tx, cancel, progress) = fake_import(&h);
        h.app.lightroom = Some(task);
        progress.catalog_total.store(10, Ordering::Relaxed);
        progress.catalog_done.store(3, Ordering::Relaxed);
        h.step();
        let rows = h.app.session.activity.list();
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].kind, rows[0].label.as_str()), ("lightroom", "Importing Lightroom catalog"));
        assert_eq!((rows[0].done, rows[0].total), (3, 10));
        assert!(rows[0].cancellable, "reading the catalog can stop");
        h.app.lightroom.as_mut().unwrap().enter(Phase::Committing);
        let row = h.app.session.activity.list().remove(0);
        assert!(!row.cancellable, "once photos are being added it runs to the end");
        assert!(h.app.session.activity.cancel(row.id).is_err());
        assert!(!cancel.load(Ordering::Relaxed));
        tx.send(Message::Finished(Ok(()))).unwrap();
        h.step();
        assert!(h.app.lightroom.is_none());
        assert!(h.app.session.activity.list().is_empty(), "the row goes with the task");
    }

    #[test]
    fn lightroom_cancel_from_the_stack_stops_the_read() {
        let mut h = demo();
        let (task, tx, cancel, _) = fake_import(&h);
        h.app.lightroom = Some(task);
        h.step();
        let id = h.app.session.activity.list()[0].id;
        h.app.session.activity.cancel(id).unwrap();
        assert!(cancel.load(Ordering::Relaxed), "the worker sees the stack's ✕");
        tx.send(Message::Finished(Err("cancelled".into()))).unwrap();
        h.step();
        assert!(h.app.lightroom.is_none());
        assert_eq!(h.app.lightroom_last, Some(serde_json::json!({"cancelled": true, "path": "fixture.lrcat", "report": {}})));
        assert!(h.app.session.activity.list().is_empty());
    }

    /// Whether the task was cancelled is the task's flag, not an error's words (#1046): a cancel
    /// reads as one whatever the message says, and a failure that mentions "cancelled" is a failure.
    #[test]
    fn lightroom_cancel_is_the_flag_not_the_words() {
        let mut h = demo();
        let (task, tx, cancel, _) = fake_import(&h);
        h.app.lightroom = Some(task);
        cancel.store(true, Ordering::Relaxed);
        tx.send(Message::Finished(Err("Lightroom-Import abgebrochen".into()))).unwrap();
        h.step();
        assert_eq!(h.app.lightroom_last, Some(serde_json::json!({"cancelled": true, "path": "fixture.lrcat", "report": {}})));

        for message in
            [Message::Finished(Err("the drive was ejected: copy cancelled by the system".into())), Message::Inspected(Err("cancelled".into()))]
        {
            let (task, tx, _, _) = fake_import(&h);
            h.app.lightroom = Some(task);
            tx.send(message).unwrap();
            h.step();
            let last = h.app.lightroom_last.clone().unwrap_or_default();
            assert!(last.get("cancelled").is_none() && last["error"].as_str().is_some_and(|e| e.contains("cancelled")), "{last}");
        }
    }

    #[test]
    fn lightroom_result_stays_readable_and_shows_photos_hidden_by_a_filter() {
        let mut h = demo();
        let id = h.app.session.catalog.photos().next().unwrap().id.0;
        h.app.session.source = lcc_engine::LibrarySource::Picks;
        h.app.session.filter.text = "no-photo-has-this-name".into();
        h.app.session.filter.only = vec![lcc_catalog::PhotoId(u64::MAX)];
        h.app.session.filter.labels = vec![lcc_catalog::ColorLabel::Red];
        h.app.session.filter.rule_set = Some(lcc_catalog::RuleSet {
            mode: lcc_catalog::rules::Match::All,
            rules: vec![lcc_catalog::rules::Rule::Field { field: "rating".into(), op: "is".into(), value: json!(5) }],
        });
        let (task, _tx, _, _) = fake_import(&h);
        let value = json!({"photos": 1, "imported": 1, "collections": 0, "missing": ["D:/Photos/a.NEF"], "mapping": {"1": id}, "library": "C:/Pictures/Photo Library"});
        terminal(&mut h.app, &h.view.ctx, &task, value);
        assert!(h.app.ui.toast.as_ref().unwrap().0.contains("1 photos added"));
        h.step();
        h.step();
        let text = h.painted_text().join(" ");
        assert!(text.contains("Destination library"), "{text}");
        assert!(text.contains("C:/Pictures/Photo Library"), "{text}");
        assert!(text.contains("1 originals are missing"), "{text}");
        assert!(text.contains("Show in Library"), "{text}");
        // The activity and toast can go away; the modal result remains until dismissed.
        h.app.ui.toast = None;
        h.step();
        assert!(h.app.ui.dialog.is_some());
        let dialog = h.app.ui.dialog.clone().unwrap();
        crate::panels::dialogs::confirm_dialog(&mut h.app, &dialog).unwrap();
        assert_eq!(h.app.session.source, lcc_engine::LibrarySource::All);
        assert_eq!(h.app.session.filter, lcc_catalog::Filter::default());
        assert_eq!(h.app.session.selection.active.map(|p| p.0), Some(id));
        assert_eq!(h.app.ui.view, crate::state::ViewMode::PhotoGrid);
        assert!(!h.app.session.visible().is_empty());
    }

    #[test]
    fn lightroom_failure_stays_readable_without_offering_a_success_action() {
        let mut h = demo();
        let (task, _tx, _, _) = fake_import(&h);
        terminal(&mut h.app, &h.view.ctx, &task, json!({"error": "catalog or WAL exceeds 1 GiB import limit"}));
        h.step();
        h.step();
        assert!(h.painted_text().join(" ").contains("exceeds 1 GiB import limit"));
        assert!(matches!(h.app.ui.dialog, Some(crate::state::Dialog::LightroomResult { first: None, .. })));
        assert!(!h.app.widgets.iter().any(|(id, _)| id == "button:dialogCancel"));
    }

    #[test]
    fn lightroom_result_waits_for_an_existing_dialog() {
        let mut h = demo();
        h.app.ui.dialog = Some(crate::state::Dialog::About);
        let (task, _tx, _, _) = fake_import(&h);
        terminal(&mut h.app, &h.view.ctx, &task, json!({"error": "catalog could not be read"}));
        assert!(matches!(h.app.ui.dialog, Some(crate::state::Dialog::About)));
        assert!(h.app.lightroom_result.is_some());
        h.app.ui.dialog = None;
        tick(&mut h.app, &h.view.ctx);
        assert!(matches!(h.app.ui.dialog, Some(crate::state::Dialog::LightroomResult { .. })));
        assert!(h.app.lightroom_result.is_none());
    }

    #[test]
    fn lightroom_cancel_reports_retained_batches() {
        let mut h = demo();
        let (mut task, tx, cancel, _) = fake_import(&h);
        merge_report(&mut task.partial, &json!({"imported": 16, "photos": 16, "mapping": {"1": 1}, "missing": [], "failed": []}));
        merge_report(&mut task.partial, &json!({"imported": 4, "mapping": {"2": 2}, "warnings": ["source warning"]}));
        assert_eq!(task.status()["imported"], 20);
        h.app.lightroom = Some(task);
        cancel.store(true, Ordering::Relaxed);
        tx.send(Message::Finished(Err("cancelled".into()))).unwrap();
        h.step();
        assert_eq!(h.app.lightroom_last.as_ref().unwrap()["report"]["imported"], 20);
        assert!(h.app.ui.toast.as_ref().unwrap().0.contains("20 already imported"));
        assert!(matches!(h.app.ui.dialog, Some(crate::state::Dialog::LightroomResult { .. })));
    }

    #[test]
    fn inspect_report_keeps_empty_catalog_shape() {
        let report = inspect_report(CatalogImport {
            source: "synthetic.lrcat".into(),
            photos: Vec::new(),
            collections: Vec::new(),
            members: Vec::new(),
            collection_content: Vec::new(),
            stacks: Vec::new(),
            warnings: vec!["warning".into()],
        });
        assert_eq!(report["photos"], 0);
        assert_eq!(report["collections"], 0);
        assert_eq!(report["missing"], serde_json::json!([]));
        assert_eq!(report["warnings"], serde_json::json!(["warning"]));
    }
}

fn update_location(priority: &lcc_engine::lightroom_job::LightroomPriority, s: &lcc_engine::Session) {
    let folder = match s.source {
        lcc_engine::LibrarySource::Folder => s.browse.as_ref().map(|b| b.path.as_str()),
        lcc_engine::LibrarySource::LibraryFolder => s.library_folder.as_deref(),
        _ => s.filter.library_folder.as_deref(),
    };
    let active = s.active().and_then(|id| s.catalog.photo(id)).and_then(|p| match &p.source {
        lcc_catalog::Source::File { path } => Some(path.as_str()),
        _ => None,
    });
    priority.location(folder, active);
}

pub(crate) fn prioritize_visible(app: &LccApp, ids: &std::collections::HashSet<lcc_catalog::PhotoId>) {
    if let Some(task) = &app.lightroom {
        task.priority.visible(ids.iter().filter_map(|id| app.session.catalog.photo(*id)).filter_map(|p| match &p.source {
            lcc_catalog::Source::File { path } => Some(path.clone()),
            _ => None,
        }));
    }
}

fn merge_report(total: &mut Value, batch: &Value) {
    let Some(fields) = batch.as_object() else { return };
    for (key, value) in fields {
        match key.as_str() {
            "imported" | "historySteps" | "snapshots" | "preservedExistingEdits" => {
                total[key] = json!(total[key].as_u64().unwrap_or(0).saturating_add(value.as_u64().unwrap_or(0)));
            }
            "missing" | "failed" | "warnings" => {
                if !total[key].is_array() {
                    total[key] = json!([]);
                }
                if let (Some(to), Some(from)) = (total[key].as_array_mut(), value.as_array()) {
                    to.extend(from.iter().cloned());
                }
            }
            "mapping" | "unmapped" => {
                if !total[key].is_object() {
                    total[key] = json!({});
                }
                if let (Some(to), Some(from)) = (total[key].as_object_mut(), value.as_object()) {
                    to.extend(from.clone());
                }
            }
            _ => {
                total[key] = value.clone();
            }
        }
    }
    total["photos"] = json!(total["mapping"].as_object().map_or(0, |m| m.len()));
}
