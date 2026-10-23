//! Bounded, owner-acknowledged imports. Scheduling is our own design.
use super::*;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

#[derive(Default)]
struct Interest {
    folder: Option<String>,
    active: Option<String>,
    visible: Vec<String>,
}

/// The host publishes navigation without touching the worker's pending queue.
#[derive(Clone, Default)]
pub struct LightroomPriority(Arc<Mutex<Interest>>);

impl LightroomPriority {
    pub fn location(&self, folder: Option<&str>, active: Option<&str>) {
        let mut interest = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let folder = folder.map(|p| format!("{}/", normalize(p).to_lowercase().trim_end_matches('/')));
        if interest.folder != folder {
            interest.visible.clear();
        }
        interest.folder = folder;
        interest.active = active.map(stem);
    }

    pub fn visible(&self, paths: impl IntoIterator<Item = String>) {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).visible = paths.into_iter().map(|p| stem(&p)).collect();
    }
}

fn stem(path: &str) -> String {
    // Use exactly the existing sidecar ownership key, including its case folding.
    crate::sidecar::stem_key(&normalize(path))
}

struct Pending(BTreeMap<String, Vec<usize>>);
impl Pending {
    fn new(data: &CatalogImport) -> Self {
        let masters: HashMap<_, _> = data
            .photos
            .iter()
            .filter(|p| lightroom_catalog::number_for_job(&p.image, "masterImage") == 0)
            .map(|p| (p.source_id, stem(&p.path)))
            .collect();
        let mut groups: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, photo) in data.photos.iter().enumerate() {
            let master = lightroom_catalog::number_for_job(&photo.image, "masterImage");
            let key = masters.get(&master).filter(|_| master != 0).cloned().unwrap_or_else(|| stem(&photo.path));
            groups.entry(key).or_default().push(i);
        }
        Self(groups)
    }

    fn take(&mut self, priority: &LightroomPriority) -> Vec<usize> {
        let interest = priority.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut selected = Vec::new();
        // Keep RAW/JPEG sidecar partners and every virtual copy with their master. A group
        // may exceed the limit; splitting it would change sidecar ownership or copy metadata.
        while selected.len() < 16 {
            let key = interest
                .active
                .iter()
                .chain(interest.visible.iter())
                .find(|k| self.0.contains_key(*k))
                .cloned()
                .or_else(|| {
                    interest
                        .folder
                        .as_ref()
                        .and_then(|prefix| self.0.range(prefix.clone()..).next().filter(|(k, _)| k.starts_with(prefix)).map(|(k, _)| k.clone()))
                })
                .or_else(|| self.0.first_key_value().map(|(k, _)| k.clone()));
            let Some(key) = key else { break };
            if let Some(indices) = self.0.remove(&key) {
                selected.extend(indices);
            }
        }
        selected
    }
}

/// Acknowledgement from the owner, with a fresh snapshot after the previous batch committed.
/// This preserves duplicate detection and edits made while the worker was reading files.
pub struct LightroomResume {
    job: LightroomJob,
    completion: LightroomCompletion,
}
impl LightroomResume {
    pub fn new(s: &mut crate::Session, completion: LightroomCompletion) -> crate::Result<Self> {
        Ok(Self { job: LightroomJob::new(s, PathBuf::new(), false)?, completion })
    }
}

impl LightroomJob {
    /// Read originals in small priority-ordered batches. `accept` must commit on the owner
    /// thread and return its acknowledgement. Previously accepted batches survive cancellation.
    pub fn prepare_incremental(
        &mut self,
        cancel: &AtomicBool,
        priority: &LightroomPriority,
        accept: impl FnMut(PreparedLightroom) -> crate::Result<LightroomResume>,
    ) -> crate::Result<()> {
        self.prepare_with_folders(cancel, priority, |_| Ok(()), accept)
    }

    /// Publish the complete path-derived folder manifest before touching any original.
    /// The host acknowledges it so navigation is available even if the first read stalls.
    pub fn prepare_with_folders(
        &mut self,
        cancel: &AtomicBool,
        priority: &LightroomPriority,
        announce: impl FnOnce(std::collections::BTreeSet<String>) -> crate::Result<()>,
        accept: impl FnMut(PreparedLightroom) -> crate::Result<LightroomResume>,
    ) -> crate::Result<()> {
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        let data = lightroom_catalog::read_with_folders(&self.path, cancel, &self.progress.catalog_total, &self.progress.catalog_done, |folders| {
            announce(folders).map_err(|e| e.to_string())
        })
        .map_err(error)?;
        lightroom_catalog::validate_for_job(&data).map_err(error)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(cancelled());
        }
        self.prepare_data(data, cancel, priority, accept)
    }

    fn prepare_data(
        &mut self,
        mut data: CatalogImport,
        cancel: &AtomicBool,
        priority: &LightroomPriority,
        mut accept: impl FnMut(PreparedLightroom) -> crate::Result<LightroomResume>,
    ) -> crate::Result<()> {
        let archive_path = stage_archive(self.archive_dir.as_deref(), &mut data)?;
        let index_path = self.archive_dir.as_ref().map(|dir| dir.join("lightroom-index.json"));
        let mut index = lightroom_catalog::load_index(index_path.as_deref())?;
        let mut pending = Pending::new(&data);
        self.progress.catalog_done.fetch_add(1, Ordering::Relaxed);
        self.progress.stream_total.store(data.photos.len(), Ordering::Relaxed);
        self.progress.streaming.store(true, Ordering::Relaxed);
        self.progress.photos.store(true, Ordering::Relaxed);
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(cancelled());
            }
            let indices = pending.take(priority);
            let relations_only = indices.is_empty();
            let batch = if relations_only {
                std::mem::take(&mut data)
            } else {
                CatalogImport {
                    source: data.source.clone(),
                    photos: indices.iter().filter_map(|i| data.photos.get(*i).cloned()).collect(),
                    collections: Vec::new(),
                    members: Vec::new(),
                    collection_content: Vec::new(),
                    stacks: Vec::new(),
                    warnings: std::mem::take(&mut data.warnings),
                }
            };
            let (normal, missing_sources) = if relations_only {
                (Prepared::default(), HashSet::new())
            } else {
                let (paths, missing) = original_paths(&batch, cancel)?;
                (self.import.prepare_files(paths, cancel), missing)
            };
            if cancel.load(Ordering::Relaxed) {
                normal.rollback();
                return Err(cancelled());
            }
            let resume = accept(PreparedLightroom {
                data: batch,
                normal,
                index,
                archive_path: archive_path.clone(),
                token: self.token.clone(),
                missing_sources,
                update_existing: self.update_existing,
                now: self.import.now().to_string(),
                relations_only,
                cancelled: false,
            })?;
            resume.completion.finalization.finish()?;
            if relations_only {
                return Ok(());
            }
            index = resume.completion.index;
            self.import = resume.job.import;
            self.progress.stream_done.fetch_add(indices.len(), Ordering::Relaxed);
            // The final pass only links relationships. Release accepted source payloads;
            // recovery archiving was attempted before the first batch.
            for i in indices {
                if let Some(photo) = data.photos.get_mut(i) {
                    photo.settings = String::new();
                    photo.xmp = String::new();
                    photo.history.clear();
                    photo.snapshots.clear();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn data() -> CatalogImport {
        let mut data = super::super::tests::empty_data();
        for (folder, count) in [("a", 20), ("b", 20), ("b-other", 20), ("c", 20)] {
            for n in 0..count {
                let id = data.photos.len() as i64 + 1;
                let mut photo =
                    super::super::tests::prepared_for(&crate::Session::new(), &format!("/offline/{folder}/{n:03}.jpg"), 5).data.photos.remove(0);
                photo.source_id = id;
                photo.uuid = format!("stream-{id}");
                photo.settings = "s = { Exposure2012 = 0.75 }".into();
                data.photos.push(photo);
            }
        }
        data
    }

    #[test]
    fn navigation_changes_the_next_batch_and_folder_prefix_has_a_boundary() {
        let data = data();
        let priority = LightroomPriority::default();
        priority.location(Some("/offline/b"), None);
        let mut pending = Pending::new(&data);
        let first = pending.take(&priority);
        assert_eq!(first.len(), 16);
        assert!(first.iter().all(|i| data.photos[*i].path.starts_with("/offline/b/")));
        priority.location(Some("/offline/c"), None);
        priority.visible(["/offline/a/002.jpg".into()]);
        let next = pending.take(&priority);
        assert_eq!(data.photos[next[0]].path, "/offline/a/002.jpg");
        assert!(next.iter().skip(1).all(|i| data.photos[*i].path.starts_with("/offline/c/")));
        priority.location(Some("/offline/b"), None);
        let next = pending.take(&priority);
        assert!(next.iter().take(4).all(|i| data.photos[*i].path.starts_with("/offline/b/")));
        assert!(next.iter().skip(4).all(|i| data.photos[*i].path.starts_with("/offline/a/")));
    }

    #[test]
    fn a_zero_source_id_does_not_group_all_masters_into_one_batch() {
        let mut data = data();
        data.photos[0].source_id = 0;
        assert_eq!(Pending::new(&data).take(&LightroomPriority::default()).len(), 16);
    }

    #[test]
    fn sidecar_partners_and_virtual_copies_stay_together() {
        let mut data = data();
        let mut partner = data.photos[0].clone();
        partner.path = "/offline/A/000.ARW".into();
        partner.source_id = 81;
        partner.uuid = "partner".into();
        data.photos.push(partner);
        let mut copy = data.photos[0].clone();
        copy.source_id = 82;
        copy.uuid = "copy".into();
        copy.image.insert("masterImage".into(), json!(1));
        data.photos.push(copy);
        let priority = LightroomPriority::default();
        priority.location(None, Some("/offline/a/000.jpg"));
        let selected = Pending::new(&data).take(&priority);
        for i in [0, 80, 81] {
            assert!(selected.contains(&i));
        }
    }

    #[test]
    fn incremental_commit_keeps_early_edits_and_completes_relations_without_reapplying_settings() {
        let mut s = crate::Session::new();
        let mut data = data();
        data.collections.push(BTreeMap::from([("id_local".into(), json!(1)), ("name".into(), json!("album"))]));
        data.members.push(BTreeMap::from([("collection".into(), json!(1)), ("image".into(), json!(1))]));
        data.members.push(BTreeMap::from([("collection".into(), json!(1)), ("image".into(), json!(80))]));
        let priority = LightroomPriority::default();
        priority.location(Some("/offline/b"), None);
        let mut job = LightroomJob::new(&mut s, PathBuf::new(), true).unwrap();
        let mut calls = 0;
        let mut edited = None;
        job.prepare_data(data, &AtomicBool::new(false), &priority, |prepared| {
            if calls == 0 {
                assert!(prepared.data.photos.iter().all(|p| p.path.starts_with("/offline/b/")));
            } else if calls == 1 {
                assert!(prepared.data.photos.iter().all(|p| p.path.starts_with("/offline/c/")));
            }
            let completion = commit_prepared(&mut s, prepared)?;
            if calls == 0 {
                assert_eq!(s.catalog.len(), 16, "first photos usable before all 80 are read");
                let id = s.catalog.photos().next().unwrap().id;
                s.selection.ids = vec![id];
                s.selection.active = Some(id);
                s.execute("photo.rate", &json!({"rating": 2}))?;
                s.execute("develop.set", &json!({"values": {"light.exposure": 1.25}}))?;
                edited = Some(id);
                priority.location(Some("/offline/c"), None);
            }
            calls += 1;
            LightroomResume::new(&mut s, completion)
        })
        .unwrap();
        assert_eq!(s.catalog.len(), 80);
        assert_eq!(s.catalog.photo(edited.unwrap()).unwrap().rating, 2);
        assert_eq!(s.catalog.photo(edited.unwrap()).unwrap().develop.light.exposure, 1.25);
        assert_eq!(s.catalog.albums().next().unwrap().photos.len(), 2);
        assert_eq!(job.progress().step(), (LightroomStep::ReadPhotos, 80, 80));
    }

    #[test]
    fn cancellation_retains_only_accepted_batches() {
        let mut s = crate::Session::new();
        let cancel = AtomicBool::new(false);
        let mut job = LightroomJob::new(&mut s, PathBuf::new(), false).unwrap();
        let result = job.prepare_data(data(), &cancel, &LightroomPriority::default(), |prepared| {
            let completion = commit_prepared(&mut s, prepared)?;
            cancel.store(true, Ordering::Relaxed);
            LightroomResume::new(&mut s, completion)
        });
        assert!(result.is_err());
        assert_eq!(s.catalog.len(), 16);
    }
    #[test]
    fn batches_probe_originals_and_sidecars_before_registration_and_match_atomic_import() {
        let dir = std::env::temp_dir().join(format!("lcc-stream-fidelity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut data = data();
        data.photos.truncate(32);
        let xmp = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/" crs:Exposure2012="0.5" crs:Contrast2012="17"/></rdf:RDF></x:xmpmeta>"#;
        for (i, p) in data.photos.iter_mut().enumerate() {
            let path = dir.join(format!("{i:03}.jpg"));
            std::fs::write(&path, format!("synthetic {i}")).unwrap();
            std::fs::write(path.with_extension("xmp"), xmp).unwrap();
            p.path = path.to_string_lossy().into_owned();
            p.settings.clear(); // exposure must come from the sidecar, not the catalog
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let mut streamed = crate::Session::new();
        streamed.media.file_probe = Some(Arc::new(move |path| {
            counted.fetch_add(1, Ordering::Relaxed);
            Ok(crate::media::ProbeInfo {
                width: 20,
                height: 10,
                format: "JPG".into(),
                file_size: 25_000_000,
                content_hash: Some(format!("hash-{path}")),
                meta: lcc_catalog::Meta { camera: "Synthetic camera".into(), lens: "Synthetic lens".into(), iso: Some(800), ..Default::default() },
                ..Default::default()
            })
        }));
        let mut atomic = crate::Session::new();
        atomic.media.file_probe = streamed.media.file_probe.clone();
        let mut job = LightroomJob::new(&mut streamed, PathBuf::new(), false).unwrap();
        let mut batches = 0;
        job.prepare_data(data.clone(), &AtomicBool::new(false), &LightroomPriority::default(), |prepared| {
            let completion = commit_prepared(&mut streamed, prepared)?;
            if batches == 0 {
                assert_eq!(calls.load(Ordering::Relaxed), 16);
                assert_eq!(streamed.catalog.len(), 16);
                let p = streamed.catalog.photos().next().unwrap();
                assert_eq!(p.meta.camera, "Synthetic camera");
                assert_eq!(p.meta.lens, "Synthetic lens");
                assert_eq!(p.meta.iso, Some(800));
                assert_eq!(p.file_size, 25_000_000);
                assert_eq!(p.develop.light.exposure, 0.5);
            }
            batches += 1;
            LightroomResume::new(&mut streamed, completion)
        })
        .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 32);
        let mut atomic_job = LightroomJob::new(&mut atomic, PathBuf::new(), false).unwrap();
        let (paths, missing_sources) = original_paths(&data, &AtomicBool::new(false)).unwrap();
        let normal = atomic_job.import.prepare_files(paths, &AtomicBool::new(false));
        let prepared = PreparedLightroom {
            data,
            normal,
            index: ImportIndex::default(),
            archive_path: None,
            token: atomic_job.token,
            missing_sources,
            update_existing: false,
            now: atomic_job.import.now().into(),
            relations_only: false,
            cancelled: false,
        };
        commit_prepared(&mut atomic, prepared).unwrap();
        let by_path: HashMap<_, _> = atomic.catalog.photos().map(|p| (p.file_name.clone(), p)).collect();
        for photo in streamed.catalog.photos() {
            let expected = by_path.get(&photo.file_name).unwrap();
            assert_eq!(photo.meta, expected.meta);
            assert_eq!(photo.content_hash, expected.content_hash);
            assert_eq!(photo.develop, expected.develop);
            assert_eq!(std::fs::read_to_string(dir.join(&photo.file_name).with_extension("xmp")).unwrap(), xmp);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}
