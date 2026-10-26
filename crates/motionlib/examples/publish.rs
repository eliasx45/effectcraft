//! Publish a Motion Library folder without the app (the Mac that keeps the library):
//!
//! ```sh
//! cargo run --release -p effectcraft-motionlib --example publish -- \
//!     --library "$HOME/Documents/Elias Motion Library/effectcraft" \
//!     --remote https://motion-library.<account>.workers.dev \
//!     [--folders "Devin Jatho,SceneYard"] [--max-mb 90] [--max-preview-mb 2] [--folder-max "SceneYard=5"] [--include-video] [--catalog-all] [--dry-run] [--force]
//! ```
//!
//! The token comes from `EFFECTCRAFT_LIBRARY_TOKEN` (never an argument: it would show in `ps`).
//! `--remote` may be a folder (the local backend). `effectcraft-cli exec library.publish` does the
//! same from the app's engine.

use effectcraft_motionlib::publish::{Options, publish};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let opt = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let (Some(library), Some(remote)) = (opt("--library"), opt("--remote").or_else(|| std::env::var("EFFECTCRAFT_LIBRARY_REMOTE").ok())) else {
        eprintln!(
            "usage: publish --library <folder with library.json> --remote <url or folder> [--folders a,b] [--max-mb N] [--max-preview-mb N] [--folder-max \"Folder=MB,...\"] [--include-video] [--catalog-all] [--dry-run] [--force]"
        );
        std::process::exit(2);
    };
    let folders = opt("--folders").unwrap_or_else(|| "Devin Jatho,SceneYard".into()).split(',').map(|f| f.trim().to_string()).collect();
    let mut o = Options::new(library, folders);
    if let Some(mb) = opt("--max-mb").and_then(|m| m.parse::<f64>().ok()).filter(|m| *m > 0.0) {
        o.max_file_bytes = (mb * 1e6) as u64;
    }
    if let Some(mb) = opt("--max-preview-mb").and_then(|m| m.parse::<f64>().ok()).filter(|m| *m >= 0.0) {
        o.max_preview_bytes = (mb * 1e6) as u64;
    }
    // --folder-max "SceneYard=5,Devin Jatho=90" (MB per folder).
    for part in opt("--folder-max").unwrap_or_default().split(',').filter(|p| !p.trim().is_empty()) {
        if let Some((f, mb)) = part.rsplit_once('=')
            && let Ok(mb) = mb.trim().parse::<f64>()
        {
            o.folder_max_bytes.push((f.trim().to_string(), (mb * 1e6) as u64));
        }
    }
    o.include_video = flag("--include-video");
    o.dry_run = flag("--dry-run");
    o.force = flag("--force");
    o.catalog_all = flag("--catalog-all");
    let token = std::env::var("EFFECTCRAFT_LIBRARY_TOKEN").ok();
    let remote = match effectcraft_motionlib::remote_for(&remote, token.as_deref()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    match publish(remote.as_ref(), &o, &mut |line| eprintln!("{line}")) {
        Ok(r) => {
            let mb = |b: u64| b as f64 / 1e6;
            println!(
                "{} version {} (was {}): {} cards, {} need footage; {} new · {} changed · {} removed",
                if r.dry_run {
                    "dry run:"
                } else if r.unchanged {
                    "unchanged:"
                } else {
                    "published"
                },
                r.version,
                r.previous,
                r.cards,
                r.needs_footage,
                r.new,
                r.changed,
                r.removed
            );
            if r.on_mac > 0 {
                println!("listed only (on the Mac): {} cards", r.on_mac);
            }
            println!("uploads: {} blobs, {:.1} MB (catalog total {} blobs, {:.1} MB)", r.uploads, mb(r.upload_bytes), r.blobs, mb(r.total_bytes));
            let mut by_reason = std::collections::BTreeMap::<String, (usize, u64)>::new();
            for s in &r.skipped {
                let e = by_reason.entry(s.reason.clone()).or_default();
                e.0 += 1;
                e.1 += s.size;
            }
            for (reason, (n, bytes)) in by_reason {
                println!("left out ({reason}): {n} files, {:.1} MB", mb(bytes));
            }
            for e in &r.errors {
                println!("error: {e}");
            }
            if flag("--json")
                && let Ok(j) = serde_json::to_string_pretty(&r)
            {
                println!("{j}");
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
