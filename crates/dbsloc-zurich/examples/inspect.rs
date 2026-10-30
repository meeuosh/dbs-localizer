//! Local verification helper; never prints identifying DICOM metadata.
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let source = args.first().ok_or("usage: inspect PATIENT [TEMP_DATASET SUBJECT] [--mri INDEX --mri-type TYPE --ct INDEX --previews DIR]")?;
    let mut dataset = None;
    let mut subject = None;
    let mut manual_mri = None;
    let mut manual_ct = None;
    let mut mri_type = None;
    let mut previews = None;
    let mut i = 1;
    if args.get(i).is_some_and(|v| !v.starts_with("--")) {
        dataset = Some(PathBuf::from(&args[i]));
        i += 1;
        subject = Some(args.get(i).ok_or("explicit subject required")?.clone());
        i += 1;
    }
    while i < args.len() {
        let value = args.get(i + 1).ok_or("option needs a value")?;
        match args[i].as_str() {
            "--mri" => manual_mri = Some(value.parse::<usize>()?),
            "--ct" => manual_ct = Some(value.parse::<usize>()?),
            "--mri-type" => mri_type = Some(value.clone()),
            "--previews" => previews = Some(PathBuf::from(value)),
            _ => return Err("unknown option".into()),
        }
        i += 2;
    }
    let cancel = AtomicBool::new(false);
    let mut scan = dbsloc_zurich::scan_patient(&PathBuf::from(source), &cancel, |p| {
        if p.done == p.total {
            eprintln!("{}: {}/{}", p.label, p.done, p.total);
        }
    })?;
    for (i, c) in scan.candidates.iter().enumerate() {
        println!(
            "{i}: session={} {:?} slices={} spacing={:?} evidence={:?} eligible={} type={:?}",
            c.session_number,
            c.modality,
            c.slice_count,
            c.spacing_mm,
            c.evidence,
            c.eligible,
            c.suffix
        );
        if c.modality == dbsloc_zurich::Modality::Mr {
            println!("  type reason: {}", c.suffix_reason);
        }
    }
    println!(
        "suggested MRI={:?}, CT={:?}",
        scan.preop_mri, scan.postop_ct
    );
    if let Some(dir) = previews {
        std::fs::create_dir_all(&dir)?;
        for (i, c) in scan
            .candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| c.eligible)
        {
            let Some(p) = &c.preview else {
                continue;
            };
            let mut finite: Vec<_> = p.pixels.iter().copied().filter(|v| v.is_finite()).collect();
            finite.sort_by(f32::total_cmp);
            if finite.is_empty() {
                continue;
            }
            let min = finite[finite.len() / 100];
            let max = finite[finite.len() * 99 / 100].max(min + 1.);
            let bytes: Vec<u8> = p
                .pixels
                .iter()
                .map(|v| ((v - min) / (max - min) * 255.).clamp(0., 255.) as u8)
                .collect();
            let mut f = std::fs::File::create(dir.join(format!(
                "candidate-{i:03}-session-{:03}.pgm",
                c.session_number
            )))?;
            writeln!(f, "P5\n{} {}\n255", p.width, p.height)?;
            f.write_all(&bytes)?;
        }
        println!("anonymous candidate previews written");
    }
    let mri = manual_mri.or(scan.preop_mri);
    let ct = manual_ct.or(scan.postop_ct);
    if let Some(suffix) = mri_type {
        let c = scan
            .candidates
            .get_mut(mri.ok_or("--mri-type needs a selected MRI")?)
            .ok_or("MRI index out of range")?;
        c.suffix = Some(suffix);
    }
    if manual_mri.is_some() || manual_ct.is_some() {
        println!("explicit technical test choices MRI={mri:?}, CT={ct:?}; this does not certify clinical scan classification");
    }
    if let Some(dataset) = dataset {
        let subject = subject.ok_or("explicit subject required")?;
        let temp = tempfile::tempdir()?;
        let items = dbsloc_zurich::prepare_selection(
            &scan,
            mri.ok_or("no MRI suggestion; review and use --mri INDEX --mri-type TYPE")?,
            ct.ok_or("no CT suggestion; review and use --ct INDEX")?,
            &temp.path().join("prepared"),
            &cancel,
            |p| {
                if p.done == p.total {
                    eprintln!("{}: {}/{}", p.label, p.done, p.total);
                }
            },
        )?;
        let plan = dbsloc_pipeline::import::plan_import(&dataset, &subject, &items)?;
        dbsloc_pipeline::import::run_import(&plan, false, &cancel, &mut |_| {})?;
        let resolved = dbsloc_pipeline::bids::resolve_subject_with(&dataset, &subject, true)?;
        println!(
            "imported subject {}; resolver found postop {:?}",
            plan.subject_folder(),
            resolved.postop_modality
        );
    }
    Ok(())
}
