//! `library-pull --jobs FILE.json`: on the cloud render machine, download Motion Library files
//! straight from the library's Worker (R2) to the paths the project expects, so a card applied on
//! the Mac never crosses Elias's home connection a second time (the server's bandwidth is cheap).
//!
//! - A card's files sit in the Mac's library cache as `<cache>/blobs/<sha256>/<name>`
//!   ([`blob_of`]); the mirror sends those here instead of uploading them.
//! - The Worker and the READ token come from the same settings as the app's Motion Library panel:
//!   `EFFECTCRAFT_LIBRARY_REMOTE` / `EFFECTCRAFT_LIBRARY_TOKEN`, else `motion-library.json` in the
//!   settings folder (`cloud.sh connect` installs it there, over ssh stdin, mode 0600).
//! - Every blob is checked against its sha256 and size before it replaces anything; it lands at
//!   `dest` with the Mac's mtime, and its hash is remembered for later checks.
//!
//! Jobs: `[{dest, hash, size, mtime, name}]`. Prints `{"results": [{dest, ok, bytes, error?}]}` on
//! stdout, progress on stderr. A failed job is the mirror's cue to upload that file from the Mac.

use std::time::{Duration, UNIX_EPOCH};

use effectcraft_engine::cloud_assets;
use effectcraft_motionlib::{BlobRef, Remote};
use serde_json::{Value, json};

use crate::{Args, Failure};

/// One blob's download (the library's files are at most 90 MB).
const TIMEOUT: Duration = Duration::from_secs(600);

/// The blob hash of a file in a Motion Library cache (`…/blobs/<sha256>/<name>`).
pub fn blob_of(path: &str) -> Option<String> {
    let mut parts = path.rsplit('/');
    let (_name, hash, blobs) = (parts.next().filter(|n| !n.is_empty())?, parts.next()?, parts.next()?);
    (blobs == "blobs" && hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then(|| hash.to_string())
}

pub fn run(args: &Args) -> Result<(), Failure> {
    let Some(file) = args.opt("--jobs") else { return Err(Failure::Usage("library-pull --jobs FILE.json".into())) };
    let text = std::fs::read(file).map_err(|e| Failure::Error(format!("{file}: {e}")))?;
    let jobs: Vec<Value> = serde_json::from_slice(&text).map_err(|e| Failure::Error(format!("{file}: {e}")))?;
    let results = match remote() {
        Ok(remote) => pull_all(remote.as_ref(), &jobs),
        Err(e) => {
            note(&format!("❌ Motion Library: {e}"));
            jobs.iter().map(|j| json!({"dest": j["dest"], "ok": false, "error": e})).collect()
        }
    };
    crate::emit(&json!({"results": results}), true);
    Ok(())
}

/// The Worker, as the app's panel finds it (environment first, then `motion-library.json`).
fn remote() -> Result<std::sync::Arc<dyn Remote>, String> {
    let env = |n: &str| std::env::var(n).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let file: Value = effectcraft_host::config_dir()
        .and_then(|d| std::fs::read(d.join(effectcraft_engine::motion_library::CONFIG)).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    let remote = env("EFFECTCRAFT_LIBRARY_REMOTE").or_else(|| file["remote"].as_str().map(str::to_string)).unwrap_or_default();
    let token = env("EFFECTCRAFT_LIBRARY_TOKEN").or_else(|| file["token"].as_str().map(str::to_string)).unwrap_or_default();
    if remote.is_empty() {
        return Err("not set up on this machine (no motion-library.json; cloud.sh connect installs the read token)".into());
    }
    effectcraft_motionlib::remote_for(&remote, Some(&token))
}

pub fn pull_all(remote: &dyn Remote, jobs: &[Value]) -> Vec<Value> {
    jobs.iter()
        .map(|job| {
            let dest = job["dest"].as_str().unwrap_or("").to_string();
            match pull(remote, job) {
                Ok(bytes) => json!({"dest": dest, "ok": true, "bytes": bytes}),
                Err(e) => {
                    note(&format!("❌ {}: {e}", file_name(&dest)));
                    json!({"dest": dest, "ok": false, "error": e})
                }
            }
        })
        .collect()
}

fn pull(remote: &dyn Remote, job: &Value) -> Result<u64, String> {
    let dest = job["dest"].as_str().filter(|d| d.starts_with('/')).ok_or("no absolute dest")?;
    let lower = dest.to_ascii_lowercase();
    if lower.ends_with(".ecproj") || lower.ends_with(".aep") {
        return Err("project files are never replaced from the library".into());
    }
    let hash = job["hash"].as_str().map(str::to_ascii_lowercase).ok_or("no blob hash")?;
    let size = job["size"].as_u64().ok_or("no size")?;
    let name = job["name"].as_str().map(str::to_string).unwrap_or_else(|| file_name(dest).to_string());
    let mut blob = BlobRef { hash, size, gz: false, name: name.clone() };
    if !blob.valid() {
        return Err("bad blob hash".into());
    }
    // Media blobs are stored as they are, text ones gzipped: try both keys.
    let mut data = None;
    for gz in [false, true] {
        blob.gz = gz;
        if let Some(stored) = effectcraft_motionlib::get_blocking(remote, &blob.remote_key(), TIMEOUT)? {
            data = Some(effectcraft_motionlib::decode_blob(&blob, &stored)?);
            break;
        }
    }
    let data = data.ok_or_else(|| format!("not in the library ({})", remote.describe()))?;
    let part = format!("{dest}.ec-part");
    if let Some(dir) = std::path::Path::new(dest).parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    if let Err(e) = std::fs::write(&part, &data) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("cannot write {part}: {e}"));
    }
    if let Some(m) = job["mtime"].as_i64().filter(|m| *m > 0)
        && let Ok(f) = std::fs::File::options().write(true).open(&part)
    {
        let _ = f.set_modified(UNIX_EPOCH + Duration::from_secs(m.unsigned_abs()));
    }
    if let Err(e) = std::fs::rename(&part, dest) {
        let _ = std::fs::remove_file(&part);
        return Err(format!("cannot move it into place: {e}"));
    }
    cloud_assets::remember_hash(dest, &blob.hash);
    note(&format!("✅ {name}: {} from the Motion Library (sha256 checked)", cloud_assets::human_bytes(size)));
    Ok(size)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn note(s: &str) {
    eprintln!("{s}");
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;

    use super::*;

    pub(crate) const TOKEN: &str = "read-token";

    /// A fake Worker: `GET /blobs/<key>` with the bearer token, from an in-memory map.
    pub(crate) fn fake_worker(blobs: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let path = line.split_whitespace().nth(1).unwrap_or("").trim_start_matches('/').to_string();
                let mut authed = false;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h == "\r\n" {
                        break;
                    }
                    authed |= h.eq_ignore_ascii_case(&format!("authorization: bearer {TOKEN}\r\n"));
                }
                let mut s = stream;
                let (status, body) = match blobs.iter().find(|(k, _)| *k == path) {
                    _ if !authed => ("401 Unauthorized", vec![]),
                    Some((_, b)) => ("200 OK", b.clone()),
                    None => ("404 Not Found", vec![]),
                };
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = s.write_all(&body);
            }
        });
        format!("http://{addr}")
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ec-library-pull-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn blob_paths_are_recognised() {
        let h = "a".repeat(64);
        assert_eq!(blob_of(&format!("/Users/elias/Library/Caches/EffectCraft/Motion Library/blobs/{h}/glow.png")), Some(h.clone()));
        assert_eq!(blob_of(&format!("/x/blobs/{h}/")), None);
        assert_eq!(blob_of(&format!("/x/media/{h}/a.png")), None);
        assert_eq!(blob_of("/x/blobs/notahash/a.png"), None);
        assert_eq!(blob_of(&format!("/x/blobs/{}/a.png", "A".repeat(64))), None);
        assert_eq!(blob_of("a.png"), None);
    }

    #[test]
    fn pulls_plain_and_gzipped_blobs_and_checks_them() {
        let png = b"not really a png".to_vec();
        let json_text = b"{\"layers\": []}".to_vec();
        let (hp, hj) = (effectcraft_motionlib::sha256_hex(&png), effectcraft_motionlib::sha256_hex(&json_text));
        let gz = effectcraft_motionlib::encode_blob(&json_text, true).unwrap();
        let wrong = b"other bytes!!!!!".to_vec();
        let hw = effectcraft_motionlib::sha256_hex(b"what the Mac has");
        let url = fake_worker(vec![(format!("blobs/{hp}"), png.clone()), (format!("blobs/{hj}.gz"), gz), (format!("blobs/{hw}"), wrong)]);
        let dir = tmp("pull");
        let remote = effectcraft_motionlib::remote_for(&url, Some(TOKEN)).unwrap();
        let d = |n: &str| dir.join(n).to_string_lossy().into_owned();
        let jobs = vec![
            json!({"dest": d("a/glow.png"), "hash": hp, "size": png.len(), "mtime": 1_700_000_000, "name": "glow.png"}),
            json!({"dest": d("b/anim.json"), "hash": hj, "size": json_text.len(), "name": "anim.json"}),
            json!({"dest": d("c/damaged.png"), "hash": hw, "size": 16}),
            json!({"dest": d("d/missing.png"), "hash": "b".repeat(64), "size": 3}),
            json!({"dest": d("e/x.ecproj"), "hash": hp, "size": png.len()}),
        ];
        let r = pull_all(remote.as_ref(), &jobs);
        let ok: Vec<bool> = r.iter().map(|v| v["ok"] == true).collect();
        assert_eq!(ok, [true, true, false, false, false], "{r:?}");
        assert_eq!(std::fs::read(dir.join("a/glow.png")).unwrap(), png);
        let mtime = std::fs::metadata(dir.join("a/glow.png")).unwrap().modified().unwrap();
        assert_eq!(mtime, UNIX_EPOCH + Duration::from_secs(1_700_000_000));
        assert_eq!(std::fs::read(dir.join("b/anim.json")).unwrap(), json_text);
        assert!(!dir.join("c/damaged.png").exists() && !dir.join("c/damaged.png.ec-part").exists());
        assert!(r[3]["error"].as_str().unwrap().contains("not in the library"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_wrong_token_fails_every_job_without_writing() {
        let png = b"x".to_vec();
        let h = effectcraft_motionlib::sha256_hex(&png);
        let url = fake_worker(vec![(format!("blobs/{h}"), png)]);
        let dir = tmp("token");
        let remote = effectcraft_motionlib::remote_for(&url, Some("publish-token-never-here")).unwrap();
        let dest = dir.join("x.png").to_string_lossy().into_owned();
        let r = pull_all(remote.as_ref(), &[json!({"dest": dest, "hash": h, "size": 1})]);
        assert_eq!(r[0]["ok"], false);
        assert!(!dir.join("x.png").exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
