//! Hospital-folder discovery stays outside the ordinary image import path.

use super::*;
use dbsloc_zurich::{Candidate, ElectrodeEvidence, Modality, Progress, Scan};
use tempfile::TempDir;

#[derive(Default)]
pub(super) struct ZurichPanel {
    picker: Option<mpsc::Receiver<Option<PathBuf>>>,
    job: Option<Job>,
    scan: Option<Arc<Scan>>,
    mri: Option<usize>,
    ct: Option<usize>,
    mri_suffix: Option<String>,
    reviewed: bool,
    textures: [Option<(usize, egui::TextureHandle)>; 2],
}

struct Prepared {
    directory: TempDir,
    items: Vec<ImportItem>,
}

enum Event {
    Progress(Progress),
    Scanned(Result<Scan, String>),
    Prepared(Result<Prepared, String>),
}

struct Job {
    rx: mpsc::Receiver<Event>,
    cancel: Arc<AtomicBool>,
    progress: Progress,
}

impl Drop for Job {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
    }
}

impl ZurichPanel {
    pub(super) fn busy(&self) -> bool {
        self.picker.is_some() || self.job.is_some()
    }

    pub(super) fn reviewing(&self) -> bool {
        self.scan.is_some() || self.busy()
    }

    pub(super) fn cancel(&mut self) {
        self.picker = None;
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::SeqCst);
        }
    }

    fn selection(&self) -> Option<(usize, usize)> {
        let scan = self.scan.as_ref()?;
        let (mri, ct) = (self.mri?, self.ct?);
        let a = scan.candidates.get(mri)?;
        let b = scan.candidates.get(ct)?;
        (mri != ct
            && a.modality == Modality::Mr
            && b.modality == Modality::Ct
            && a.eligible
            && b.eligible
            && self.mri_suffix.is_some())
        .then_some((mri, ct))
    }

    fn reviewed_scan(&self) -> Option<(Scan, usize, usize)> {
        if !self.reviewed {
            return None;
        }
        let (mri, ct) = self.selection()?;
        let mut scan = self.scan.as_deref()?.clone();
        scan.candidates.get_mut(mri)?.suffix = self.mri_suffix.clone();
        Some((scan, mri, ct))
    }
}

impl ImportWizard {
    fn pick_zurich(&mut self, ctx: &egui::Context) {
        if self.busy() || self.pending_pick.is_some() || !self.rows.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.zurich.picker = Some(rx);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let path = rfd::FileDialog::new()
                .set_title("Select Zurich patient folder (CT and MR / MRI)")
                .pick_folder();
            let _ = tx.send(path);
            ctx.request_repaint();
        });
    }

    pub(super) fn scan_zurich(&mut self, path: PathBuf, ctx: &egui::Context) {
        self.error = None;
        self.zurich.scan = None;
        self.zurich.textures = [None, None];
        self.zurich.reviewed = false;
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = dbsloc_zurich::scan_patient(&path, &worker_cancel, |progress| {
                let _ = tx.send(Event::Progress(progress));
                ctx.request_repaint();
            })
            .map_err(|e| e.to_string());
            let _ = tx.send(Event::Scanned(result));
            ctx.request_repaint();
        });
        self.zurich.job = Some(Job {
            rx,
            cancel,
            progress: Progress {
                done: 0,
                total: 0,
                label: "Indexing DICOM headers".to_owned(),
            },
        });
    }

    fn prepare_zurich(&mut self, ctx: &egui::Context) {
        if self.busy() || !self.rows.is_empty() {
            return;
        }
        let Some((scan, mri, ct)) = self.zurich.reviewed_scan() else {
            return;
        };
        self.error = None;
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let worker_cancel = Arc::clone(&cancel);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let result = (|| {
                let directory = tempfile::Builder::new()
                    .prefix("dbsloc-zurich-")
                    .tempdir()
                    .map_err(|e| format!("Cannot create temporary import directory: {e}"))?;
                let output = directory.path().join("prepared");
                let items = dbsloc_zurich::prepare_selection(
                    &scan,
                    mri,
                    ct,
                    &output,
                    &worker_cancel,
                    |progress| {
                        let _ = tx.send(Event::Progress(progress));
                        ctx.request_repaint();
                    },
                )
                .map_err(|e| e.to_string())?;
                Ok(Prepared { directory, items })
            })();
            let _ = tx.send(Event::Prepared(result));
            ctx.request_repaint();
        });
        self.zurich.job = Some(Job {
            rx,
            cancel,
            progress: Progress {
                done: 0,
                total: 2,
                label: "Converting selected scans".to_owned(),
            },
        });
    }

    pub(super) fn poll_zurich(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.zurich.picker {
            match rx.try_recv() {
                Ok(path) => {
                    self.zurich.picker = None;
                    if let Some(path) = path {
                        self.scan_zurich(path, ctx);
                    }
                }
                Err(mpsc::TryRecvError::Disconnected) => self.zurich.picker = None,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        loop {
            let Some(job) = &self.zurich.job else {
                return;
            };
            let cancelled = job.cancel.load(Ordering::SeqCst);
            let event = match job.rx.try_recv() {
                Ok(event) => event,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.zurich.job = None;
                    if !cancelled {
                        self.error =
                            Some("Zurich import worker stopped before finishing".to_owned());
                    }
                    return;
                }
            };
            match event {
                Event::Progress(progress) => {
                    if let Some(job) = &mut self.zurich.job {
                        job.progress = progress;
                    }
                }
                Event::Scanned(result) => {
                    self.zurich.job = None;
                    if !cancelled {
                        match result {
                            Ok(scan) => {
                                self.zurich.mri = scan.preop_mri;
                                self.zurich.ct = scan.postop_ct;
                                self.zurich.mri_suffix = scan
                                    .preop_mri
                                    .and_then(|i| scan.candidates.get(i))
                                    .and_then(|candidate| candidate.suffix.clone());
                                self.zurich.scan = Some(Arc::new(scan));
                                self.zurich.reviewed = false;
                            }
                            Err(error) => self.error = Some(error),
                        }
                    }
                    return;
                }
                Event::Prepared(result) => {
                    self.zurich.job = None;
                    if !cancelled {
                        match result {
                            Ok(prepared) => self.accept_zurich(prepared),
                            Err(error) => self.error = Some(error),
                        }
                    }
                    return;
                }
            }
        }
    }

    fn accept_zurich(&mut self, prepared: Prepared) {
        if !self.rows.is_empty() || prepared.items.len() != 2 {
            self.error =
                Some("Zurich import requires an empty wizard and exactly two scans".to_owned());
            return;
        }
        let input = self.inputs.len();
        let path = prepared.directory.path().join("prepared");
        self.inputs.push(Input {
            path,
            kind: InputKind::Nifti,
            subject: String::new(),
            warning: None,
        });
        for item in prepared.items {
            let info = import::series_info(&item.source, item.sidecar.as_deref());
            let label = format!("{} {}", item.session.label(), item.suffix);
            self.rows.push(Row {
                item,
                include: true,
                info,
                label,
                input,
            });
        }
        self.temp_dirs.push(prepared.directory.keep());
        self.subject.clear();
        self.subject_edited = true;
        self.keep_sourcedata = false;
        self.per_folder = false;
        self.zurich.scan = None;
        self.zurich.textures = [None, None];
        self.notice =
            Some("Selected scans ready. Enter a study subject ID on the next step.".to_owned());
        self.inputs_changed();
    }

    pub(super) fn zurich_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let idle = !self.busy() && self.pending_pick.is_none();
        if ui
            .add_enabled(
                idle && self.rows.is_empty(),
                egui::Button::new("Zurich patient folder…"),
            )
            .on_hover_text(
                "Choose one patient with CT and MR / MRI session folders. Highest session number is oldest. Start with an empty image list.",
            )
            .clicked()
        {
            self.pick_zurich(ctx);
        }
        if let Some(job) = &self.zurich.job {
            let progress = &job.progress;
            let fraction = if progress.total == 0 {
                0.0
            } else {
                (progress.done as f32 / progress.total as f32).clamp(0.0, 1.0)
            };
            ui.add(egui::ProgressBar::new(fraction).text(format!(
                "{} · {} / {}",
                progress.label, progress.done, progress.total
            )))
            .on_hover_text("Source files stay unchanged; Cancel stops the current job");
        }
        let Some(scan) = self.zurich.scan.as_ref().map(Arc::clone) else {
            return;
        };
        let pal = palette(ui);
        ui.label(egui::RichText::new("Review scan suggestions before importing").strong());
        ui.label(
            egui::RichText::new(
                "Oldest eligible session first · electrode evidence is a heuristic",
            )
            .color(pal.text_muted),
        );
        ui.add_enabled_ui(idle, |ui| {
            let mut changed = false;
            let actions_height = ui.spacing().interact_size.y * 2.0
                + ui.spacing().item_spacing.y * 4.0;
            let review_height = (ui.available_height() - actions_height).max(0.0);
            egui::ScrollArea::vertical()
                .id_salt("zurich_candidates")
                .max_height(review_height)
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    ui.columns(2, |columns| {
                        changed |= candidate_selector(
                            &mut columns[0],
                            "Pre-op MRI",
                            Modality::Mr,
                            &scan,
                            &mut self.zurich.mri,
                            &mut self.zurich.textures[0],
                            Some(&mut self.zurich.mri_suffix),
                        );
                        changed |= candidate_selector(
                            &mut columns[1],
                            "Post-op CT",
                            Modality::Ct,
                            &scan,
                            &mut self.zurich.ct,
                            &mut self.zurich.textures[1],
                            None,
                        );
                    });
                    if !scan.warnings.is_empty() {
                        ui.collapsing(format!("Scan notes ({})", scan.warnings.len()), |ui| {
                            for warning in &scan.warnings {
                                ui.label(egui::RichText::new(warning).color(pal.warn));
                            }
                        })
                        .header_response
                        .on_hover_text("Unreadable or unsupported images and other discovery notes");
                    }
                });
            if changed {
                self.zurich.reviewed = false;
            }
            ui.checkbox(&mut self.zurich.reviewed, "I reviewed both scans")
                .on_hover_text("Check the previews, acquisition dates and evidence. You can override suggestions using the scan lists.");
            let ready = self.zurich.selection().is_some() && self.zurich.reviewed && self.rows.is_empty();
            if ui.add_enabled(ready, egui::Button::new("Use selected scans"))
                .on_hover_text("Convert only these two series, then continue through subject, destination and final import review")
                .clicked()
            {
                self.prepare_zurich(ctx);
            }
        });
    }
}

fn candidate_label(index: usize, candidate: &Candidate) -> String {
    format!(
        "#{} · session {} · {} · {} slices",
        index + 1,
        candidate.session_number,
        candidate
            .suffix
            .as_deref()
            .unwrap_or(match candidate.modality {
                Modality::Ct => "CT",
                Modality::Mr => "MRI type unknown",
            }),
        candidate.slice_count,
    )
}

fn candidate_selector(
    ui: &mut egui::Ui,
    label: &str,
    modality: Modality,
    scan: &Scan,
    selected: &mut Option<usize>,
    texture: &mut Option<(usize, egui::TextureHandle)>,
    suffix: Option<&mut Option<String>>,
) -> bool {
    ui.label(egui::RichText::new(label).strong());
    let before = *selected;
    let selected_text = selected
        .and_then(|index| {
            scan.candidates
                .get(index)
                .map(|candidate| candidate_label(index, candidate))
        })
        .unwrap_or_else(|| "Choose a scan…".to_owned());
    egui::ComboBox::from_id_salt(("zurich_scan", label))
        .width(ui.available_width())
        .selected_text(selected_text)
        .show_ui(ui, |ui| {
            ui.selectable_value(selected, None, "Choose a scan…")
                .on_hover_text("Leave this target unassigned");
            for (index, candidate) in scan.candidates.iter().enumerate() {
                if candidate.modality != modality {
                    continue;
                }
                let supported = candidate.eligible;
                ui.add_enabled_ui(supported, |ui| {
                    ui.selectable_value(selected, Some(index), candidate_label(index, candidate))
                        .on_hover_text(format!(
                            "{}\n{}\n{}",
                            candidate.study_date.as_deref().unwrap_or("Date unknown"),
                            candidate.description,
                            candidate.reason
                        ));
                });
            }
        })
        .response
        .on_hover_text("Sessions sorted by discovery; higher session number is older. Unsupported volumes cannot be selected.");
    if before != *selected {
        *texture = None;
    }
    let mut type_changed = false;
    if let Some(suffix) = suffix {
        if before != *selected {
            *suffix = selected
                .and_then(|i| scan.candidates.get(i))
                .and_then(|candidate| candidate.suffix.clone());
        }
        let candidate = selected.and_then(|i| scan.candidates.get(i));
        let inferred = candidate.and_then(|candidate| candidate.suffix.as_ref());
        let title = match (suffix.as_ref(), inferred) {
            (Some(chosen), Some(guess)) if chosen == guess => "Suggested MRI type",
            (Some(_), _) => "MRI type (manual)",
            _ => "MRI type",
        };
        let reason = candidate
            .map(|candidate| candidate.suffix_reason.as_str())
            .unwrap_or("Select an MRI to see its type suggestion");
        ui.label(title).on_hover_text(reason);
        let previous = suffix.clone();
        egui::ComboBox::from_id_salt("zurich_mri_type")
            .selected_text(suffix.as_deref().unwrap_or("Choose MRI type…"))
            .show_ui(ui, |ui| {
                ui.selectable_value(suffix, None, "Choose MRI type…")
                    .on_hover_text("Required when scan metadata does not identify the MRI type");
                for value in import::PREOP_SUFFIXES {
                    ui.selectable_value(suffix, Some((*value).to_owned()), *value)
                        .on_hover_text(
                            "Confirm this MRI contrast from the scan protocol and preview",
                        );
                }
            })
            .response
            .on_hover_text(format!(
                "{reason}\nMRI contrast controls the imported image type; review or change the suggestion before continuing"
            ));
        type_changed = previous != *suffix;
    }
    let Some((index, candidate)) = selected.and_then(|index| {
        scan.candidates
            .get(index)
            .map(|candidate| (index, candidate))
    }) else {
        return before != *selected || type_changed;
    };
    ui.add(
        egui::Label::new(format!(
            "{} · {}",
            candidate.study_date.as_deref().unwrap_or("Date unknown"),
            if candidate.description.is_empty() {
                "Unnamed series"
            } else {
                &candidate.description
            }
        ))
        .wrap(),
    );
    ui.label(format!(
        "{} slices · {:.2} × {:.2} × {:.2} mm",
        candidate.slice_count,
        candidate.spacing_mm[0],
        candidate.spacing_mm[1],
        candidate.spacing_mm[2]
    ));
    let evidence = match candidate.evidence {
        ElectrodeEvidence::Detected => "Electrode suggested",
        ElectrodeEvidence::LikelyAbsent => "Likely no electrode",
        ElectrodeEvidence::Unknown => "Electrode status unknown",
    };
    let pal = palette(ui);
    ui.label(egui::RichText::new(evidence).color(pal.info))
        .on_hover_text(&candidate.reason);
    ui.add(egui::Label::new(&candidate.reason).wrap());
    if let Some(preview) = &candidate.preview {
        if texture.as_ref().map(|(cached, _)| *cached) != Some(index) {
            if let Some(image) = preview_image(preview, modality) {
                *texture = Some((
                    index,
                    ui.ctx().load_texture(
                        format!("zurich_preview_{label}"),
                        image,
                        egui::TextureOptions::LINEAR,
                    ),
                ));
            }
        }
        if let Some((_, texture)) = texture {
            let max_size = egui::vec2(ui.available_width(), em(ui) * 14.0);
            ui.add(egui::Image::new(&*texture).fit_to_exact_size(
                texture.size_vec2()
                    * (max_size.x / texture.size_vec2().x).min(max_size.y / texture.size_vec2().y),
            ))
            .on_hover_text(
                "Middle-slice preview. Open the converted image with View for three-plane review.",
            );
        }
    } else {
        ui.label(egui::RichText::new("Preview unavailable").color(pal.warn));
    }
    before != *selected || type_changed
}

fn preview_image(preview: &dbsloc_zurich::Preview, modality: Modality) -> Option<egui::ColorImage> {
    let count = preview.width.checked_mul(preview.height)?;
    if count != preview.pixels.len() || count == 0 || preview.width > 2048 || preview.height > 2048
    {
        return None;
    }
    let (low, high) = match modality {
        Modality::Ct => (-160.0, 240.0),
        Modality::Mr => {
            let high = preview
                .pixels
                .iter()
                .copied()
                .filter(|p| p.is_finite())
                .fold(1.0_f32, f32::max);
            (0.0, high)
        }
    };
    let pixels: Vec<u8> = preview
        .pixels
        .iter()
        .map(|value| (((*value - low) / (high - low)).clamp(0.0, 1.0) * 255.0) as u8)
        .collect();
    Some(egui::ColorImage::from_gray(
        [preview.width, preview.height],
        &pixels,
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn candidate(modality: Modality, suffix: Option<&str>) -> Candidate {
        Candidate {
            session_number: 3,
            session_path: PathBuf::new(),
            series_uid: String::new(),
            modality,
            description: String::new(),
            study_date: None,
            slice_count: 100,
            spacing_mm: [1.0; 3],
            evidence: ElectrodeEvidence::Unknown,
            reason: String::new(),
            eligible: true,
            suffix: suffix.map(str::to_owned),
            suffix_reason: "Acquisition parameter guess; review".to_owned(),
            preview: None,
            slices: Vec::new(),
            quality: 80.0,
            explicit_preop: false,
            explicit_postop: false,
        }
    }

    #[test]
    fn unnamed_scans_in_same_session_have_distinct_compact_labels() {
        let first = candidate(Modality::Mr, Some("T1w"));
        let second = first.clone();
        let a = candidate_label(0, &first);
        let b = candidate_label(1, &second);
        assert_ne!(a, b);
        assert!(a.contains("#1") && b.contains("#2"));
        assert!(a.contains("session 3") && a.contains("T1w") && a.contains("100 slices"));
    }

    #[test]
    fn review_action_stays_visible_with_large_previews_in_small_windows() {
        for size in [egui::vec2(1440.0, 900.0), egui::vec2(900.0, 620.0)] {
            let mut mri = candidate(Modality::Mr, Some("T1w"));
            mri.preview = Some(dbsloc_zurich::Preview {
                width: 16,
                height: 16,
                pixels: vec![30.0; 16 * 16],
            });
            let mut ct = candidate(Modality::Ct, Some("CT"));
            ct.preview = mri.preview.clone();
            let mut wizard = ImportWizard::new("");
            wizard.zurich = ZurichPanel {
                scan: Some(Arc::new(Scan {
                    candidates: vec![mri, ct],
                    preop_mri: Some(0),
                    postop_ct: Some(1),
                    warnings: Vec::new(),
                })),
                mri: Some(0),
                ct: Some(1),
                mri_suffix: Some("T1w".to_owned()),
                ..Default::default()
            };
            let ctx = egui::Context::default();
            let mut action_visible = false;
            for _ in 0..3 {
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                        ..Default::default()
                    },
                    |ui| {
                        ui.add_space(ui.spacing().interact_size.y * 4.0);
                        wizard.show(ui, &ctx);
                    },
                );
                action_visible = output.shapes.iter().any(|clipped| {
                    matches!(&clipped.shape, egui::Shape::Text(text) if text.galley.job.text == "Use selected scans")
                        && clipped.clip_rect.contains_rect(clipped.shape.visual_bounding_rect())
                });
            }
            assert!(action_visible, "Use selected scans clipped at {size:?}");
        }
    }

    #[test]
    fn reviewed_mri_override_reaches_preparation_without_changing_suggestion() {
        let scan = Arc::new(Scan {
            candidates: vec![
                candidate(Modality::Mr, Some("T2w")),
                candidate(Modality::Ct, Some("CT")),
            ],
            preop_mri: Some(0),
            postop_ct: Some(1),
            warnings: Vec::new(),
        });
        let mut panel = ZurichPanel {
            scan: Some(Arc::clone(&scan)),
            mri: Some(0),
            ct: Some(1),
            mri_suffix: Some("T1w".to_owned()),
            ..Default::default()
        };
        assert!(panel.reviewed_scan().is_none());
        panel.reviewed = true;
        let (prepared_scan, mri, ct) = panel.reviewed_scan().unwrap();
        assert_eq!((mri, ct), (0, 1));
        assert_eq!(prepared_scan.candidates[mri].suffix.as_deref(), Some("T1w"));
        assert_eq!(scan.candidates[0].suffix.as_deref(), Some("T2w"));
        panel.mri_suffix = None;
        assert!(panel.reviewed_scan().is_none());
    }

    fn prepared() -> Prepared {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("prepared");
        std::fs::create_dir(&path).unwrap();
        let mri = super::super::tests::write_nii(&path, "mri.nii");
        let ct = super::super::tests::write_nii(&path, "ct.nii");
        Prepared {
            directory,
            items: vec![
                ImportItem {
                    source: mri,
                    sidecar: None,
                    session: Session::Preop,
                    suffix: "T1w".to_owned(),
                    acq: Some(Acq::Iso),
                },
                ImportItem {
                    source: ct,
                    sidecar: None,
                    session: Session::Postop,
                    suffix: "CT".to_owned(),
                    acq: None,
                },
            ],
        }
    }

    #[test]
    fn prepared_scans_require_explicit_id_and_use_existing_import() {
        let root = tempfile::tempdir().unwrap();
        let mut wizard = ImportWizard::new(&root.path().to_string_lossy());
        wizard.accept_zurich(prepared());
        assert_eq!(wizard.rows.len(), 2);
        assert_eq!(wizard.included_count(), 2);
        assert!(wizard
            .inputs
            .iter()
            .all(|input| input.kind == InputKind::Nifti && input.subject.is_empty()));
        assert!(!wizard.keep_sourcedata);
        assert!(wizard.subject.is_empty());
        assert!(wizard.subject_edited);
        assert!(wizard.can_advance(), "{:?}", wizard.rules_problem);
        wizard.advance();
        assert_eq!(wizard.step, Step::Subject);
        assert!(!wizard.subject_ok());
        wizard.subject = "003".to_owned();
        assert!(wizard.subject_ok());
        wizard.advance();
        assert_eq!(wizard.import_gate(), ImportGate::Ready);
        wizard.request_import(None);
        let ctx = egui::Context::default();
        let start = Instant::now();
        while wizard.busy() && start.elapsed() < Duration::from_secs(10) {
            wizard.poll(&ctx);
            thread::sleep(Duration::from_millis(5));
        }
        wizard.poll(&ctx);
        assert!(wizard.error.is_none(), "{:?}", wizard.error);
        assert!(matches!(
            wizard.take_outcome(),
            ImportWizardAction::Imported { subject_folder, .. } if subject_folder == "sub-003"
        ));
        assert!(root.path().join("rawdata/sub-003/rawimages.json").is_file());
    }

    #[test]
    fn cancelled_completion_drops_temp_files_without_adding_rows() {
        let mut wizard = ImportWizard::new("");
        let prepared = prepared();
        let path = prepared.directory.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        wizard.zurich.job = Some(Job {
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Progress {
                done: 0,
                total: 2,
                label: "test".to_owned(),
            },
        });
        assert!(wizard.busy());
        assert!(!wizard.can_advance());
        wizard.cancel_job();
        tx.send(Event::Prepared(Ok(prepared))).ok();
        wizard.poll(&egui::Context::default());
        assert!(!wizard.busy());
        assert!(wizard.rows.is_empty());
        assert!(wizard.error.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn failed_preparation_keeps_existing_rows_unchanged() {
        let mut wizard = ImportWizard::new("");
        let (tx, rx) = mpsc::channel();
        wizard.zurich.job = Some(Job {
            rx,
            cancel: Arc::new(AtomicBool::new(false)),
            progress: Progress {
                done: 0,
                total: 2,
                label: "test".to_owned(),
            },
        });
        tx.send(Event::Prepared(Err("conversion failed".to_owned())))
            .ok();
        wizard.poll(&egui::Context::default());
        assert!(!wizard.busy());
        assert!(wizard.rows.is_empty());
        assert!(wizard.temp_dirs.is_empty());
        assert_eq!(wizard.error.as_deref(), Some("conversion failed"));
    }

    #[test]
    fn dropping_wizard_cancels_worker_and_cleans_queued_result() {
        let mut wizard = ImportWizard::new("");
        let prepared = prepared();
        let path = prepared.directory.path().to_path_buf();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        wizard.zurich.job = Some(Job {
            rx,
            cancel: Arc::clone(&cancel),
            progress: Progress {
                done: 0,
                total: 2,
                label: "test".to_owned(),
            },
        });
        tx.send(Event::Prepared(Ok(prepared))).ok();
        drop(wizard);
        assert!(cancel.load(Ordering::SeqCst));
        assert!(!path.exists());
    }
}
