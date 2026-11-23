//! `ridgewarden update` end to end: curl fetches a release from a local web server, or the
//! mesh hands one over as blobs, into a private work folder; sha256sum and the release key
//! check it, and tar unpacks the binary that replaces the old one. Like the agent, it needs
//! curl, tar, gzip and sha256sum.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::Context;
use axum::Router;
use axum::routing::get;
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde_json::json;

use ridgewarden::app::App;
use ridgewarden::config::Config;
use ridgewarden::identity::Identity;
use ridgewarden::log::Log;
use ridgewarden::store::Store;
use ridgewarden::update::{self, WorkDir};

const VERSION: &str = "99.0.0";
const NEW: &[u8] = b"#!/bin/sh\necho the new build\n";
const OLD: &[u8] = b"the old build";

type Files = Vec<(String, Vec<u8>)>;

fn key() -> SigningKey {
    SigningKey::from_bytes(&[7; 32])
}

fn stranger() -> SigningKey {
    SigningKey::from_bytes(&[8; 32])
}

fn sha256(data: &[u8]) -> String {
    let mut child = Command::new("sha256sum")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(data).unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout).unwrap()[..64].to_string()
}

/// The folder a release's archive holds its binary in.
fn top(version: &str) -> String {
    update::asset_name(version)
        .trim_end_matches(".tar.gz")
        .to_string()
}

/// `<top>/ridgewarden` in a gzipped tar, made by the system's tar as the workflow does.
fn archive(dir: &Path, top: &str, binary: &[u8]) -> Vec<u8> {
    let stage = tempfile::tempdir_in(dir).unwrap();
    std::fs::create_dir(stage.path().join(top)).unwrap();
    std::fs::write(stage.path().join(top).join("ridgewarden"), binary).unwrap();
    let out = stage.path().join("out.tar.gz");
    let st = Command::new("tar")
        .arg("-czf")
        .arg(&out)
        .arg("-C")
        .arg(stage.path())
        .arg(top)
        .status()
        .unwrap();
    assert!(st.success());
    std::fs::read(out).unwrap()
}

/// A tar header, by hand, for what tar won't put in an archive on every system: links.
fn header(name: &str, kind: u8, link: &str) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(b"0000755\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(b"00000000000\0");
    h[136..148].copy_from_slice(b"00000000000\0");
    h[148..156].copy_from_slice(b"        ");
    h[156] = kind;
    h[157..157 + link.len()].copy_from_slice(link.as_bytes());
    h[257..265].copy_from_slice(b"ustar\x0000");
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    h
}

/// These headers as a gzipped tar.
fn tar_gz(headers: &[[u8; 512]]) -> Vec<u8> {
    let mut tar: Vec<u8> = headers.iter().flatten().copied().collect();
    tar.extend([0u8; 1024]);
    let mut gzip = Command::new("gzip")
        .arg("-nc")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    gzip.stdin.take().unwrap().write_all(&tar).unwrap();
    gzip.wait_with_output().unwrap().stdout
}

/// The release's files with `sums` signed by `key`.
fn signed(version: &str, archive: Vec<u8>, sums: String, key: &SigningKey) -> Files {
    vec![
        (
            "SHA256SUMS.txt.sig".into(),
            update::sign_sums(sums.as_bytes(), key).into_bytes(),
        ),
        ("SHA256SUMS.txt".into(), sums.into_bytes()),
        (update::asset_name(version), archive),
    ]
}

/// The checksum list the workflow writes for `archive`.
fn sums_of(version: &str, archive: &[u8]) -> String {
    format!("{}  {}\n", sha256(archive), update::asset_name(version))
}

/// A release as the workflow makes it: `archive` and its checksum, signed by `key`.
fn release(version: &str, archive: Vec<u8>, key: &SigningKey) -> Files {
    let sums = sums_of(version, &archive);
    signed(version, archive, sums, key)
}

/// Serves `/latest` naming `tag`, and `files` under `/download/<tag>/`.
async fn serve(tag: &str, files: Files) -> String {
    let latest = json!({ "tag_name": tag });
    let mut router = Router::new().route(
        "/latest",
        get(move || {
            let latest = latest.clone();
            async move { axum::Json(latest) }
        }),
    );
    for (name, data) in files {
        router = router.route(
            &format!("/download/{tag}/{name}"),
            get(move || {
                let data = data.clone();
                async move { data }
            }),
        );
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    url
}

/// The version `base` offers, if it is newer, asked for the way `update` asks GitHub. Off
/// the async threads, since curl blocks.
async fn offered(base: &str) -> anyhow::Result<Option<String>> {
    let base = base.to_string();
    tokio::task::spawn_blocking(move || {
        let work = WorkDir::new()?;
        let found = update::latest_from(
            &format!("{base}/latest"),
            &format!("{base}/download"),
            &work,
        )?;
        Ok(found.map(|f| f.version))
    })
    .await
    .unwrap()
}

/// What `update` does with the release `base` offers once the operator says yes: fetch it
/// into a work folder and install it over `exe`.
async fn update_from(base: &str, keys: Vec<VerifyingKey>, exe: &Path) -> anyhow::Result<()> {
    let (base, exe) = (base.to_string(), exe.to_path_buf());
    tokio::task::spawn_blocking(move || {
        let work = WorkDir::new()?;
        let found = update::latest_from(
            &format!("{base}/latest"),
            &format!("{base}/download"),
            &work,
        )?
        .context("no newer release")?;
        update::install_verified(&found, &keys, &exe)
    })
    .await
    .unwrap()
}

/// An old binary to update, in a folder of its own.
fn old_binary() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("ridgewarden");
    std::fs::write(&exe, OLD).unwrap();
    (dir, exe)
}

#[tokio::test(flavor = "multi_thread")]
async fn a_signed_release_is_downloaded_checked_and_installed() {
    let (dir, exe) = old_binary();
    let files = release(VERSION, archive(dir.path(), &top(VERSION), NEW), &key());
    let base = serve(&format!("v{VERSION}"), files).await;
    // left by an install that stopped part way, or planted to have root write through it
    let staged = exe.with_extension("new");
    let victim = dir.path().join("victim.conf");
    std::fs::write(&victim, b"keep me").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&victim, &staged).unwrap();
    #[cfg(not(unix))]
    std::fs::write(&staged, b"half a binary").unwrap();

    update_from(&base, vec![key().verifying_key()], &exe)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), NEW);
    assert!(!staged.exists());
    assert_eq!(std::fs::read(&victim).unwrap(), b"keep me");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_installed_unless_every_check_passes() {
    let (dir, exe) = old_binary();
    let good = archive(dir.path(), &top(VERSION), NEW);
    let keys = vec![key().verifying_key()];
    let mut swapped = release(VERSION, good.clone(), &key());
    swapped[2].1 = archive(dir.path(), &top(VERSION), b"another build");
    let unlisted = format!("{}  other.tar.gz\n", sha256(&good));
    let padded = format!("{}{}", sums_of(VERSION, &good), "#\n".repeat(40 * 1024));
    let mut long_sig = release(VERSION, good.clone(), &key());
    long_sig[0].1.extend(vec![b' '; 2048]);
    let secret = dir.path().join("secret");
    std::fs::write(&secret, b"not a release").unwrap();
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("ridgewarden"), b"planted").unwrap();
    let top = top(VERSION);
    let binary_link = tar_gz(&[
        header(&format!("{top}/"), b'5', ""),
        header(
            &format!("{top}/ridgewarden"),
            b'2',
            &secret.to_string_lossy(),
        ),
    ]);
    let top_link = tar_gz(&[header(&top, b'2', &elsewhere.to_string_lossy())]);
    // Windows' tar may refuse to make a link at all, which stops the install sooner
    let link_refused = if cfg!(unix) {
        "no ridgewarden binary"
    } else {
        ""
    };
    let cases = [
        (
            "does not match any release key",
            release(VERSION, good.clone(), &stranger()),
            keys.clone(),
        ),
        (
            "has no release key",
            release(VERSION, good.clone(), &key()),
            vec![],
        ),
        ("does not match its signed checksum", swapped, keys.clone()),
        (
            "is not in the signed checksum list",
            signed(VERSION, good.clone(), unlisted, &key()),
            keys.clone(),
        ),
        (
            "download failed",
            signed(VERSION, good.clone(), padded, &key()),
            keys.clone(),
        ),
        ("download failed", long_sig, keys.clone()),
        (
            "could not unpack",
            release(VERSION, b"not an archive".to_vec(), &key()),
            keys.clone(),
        ),
        (
            "no ridgewarden binary",
            release(VERSION, archive(dir.path(), "elsewhere", NEW), &key()),
            keys.clone(),
        ),
        (
            link_refused,
            release(VERSION, binary_link, &key()),
            keys.clone(),
        ),
        (
            link_refused,
            release(VERSION, top_link, &key()),
            keys.clone(),
        ),
    ];
    for (i, (why, files, keys)) in cases.into_iter().enumerate() {
        let base = serve(&format!("v{VERSION}"), files).await;
        let e = update_from(&base, keys, &exe).await.unwrap_err();
        assert!(format!("{e:#}").contains(why), "case {i}, {why}: {e:#}");
        assert_eq!(std::fs::read(&exe).unwrap(), OLD, "case {i}");
        assert!(!exe.with_extension("new").exists(), "case {i}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_install_leaves_no_staged_file() {
    let (dir, _) = old_binary();
    // a folder where the binary should be: the copy works, the rename can't
    let exe = dir.path().join("in-the-way");
    std::fs::create_dir(&exe).unwrap();
    let files = release(VERSION, archive(dir.path(), &top(VERSION), NEW), &key());
    let base = serve(&format!("v{VERSION}"), files).await;
    let e = update_from(&base, vec![key().verifying_key()], &exe)
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("replacing"), "{e:#}");
    assert!(exe.is_dir());
    assert!(!exe.with_extension("new").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_newer_release_under_a_plain_version_tag_is_fetched() {
    let (dir, _) = old_binary();
    let older = release("0.0.1", archive(dir.path(), &top("0.0.1"), NEW), &key());
    assert_eq!(offered(&serve("v0.0.1", older).await).await.unwrap(), None);
    for tag in ["v9.9.9-rc1/../../x", "latest", "v9.9", "9.9.9"] {
        let e = offered(&serve(tag, vec![]).await).await.unwrap_err();
        assert!(e.to_string().contains("is not a version"), "{tag}: {e:#}");
    }
}

/// Publish `files` on `app`'s mesh as release `version`.
fn publish(app: &App, dir: &Path, version: &str, files: Files) {
    for (name, data) in files {
        let path = dir.join(&name);
        std::fs::write(&path, data).unwrap();
        ridgewarden::blobs::add_file(
            &app.log,
            &app.blobs,
            &path,
            &format!("{}v{version}/{name}", update::BLOB_PREFIX),
            5,
        )
        .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_newest_release_that_checks_out_wins() {
    let dir = tempfile::tempdir().unwrap();
    let base = serve(
        "v99.0.5",
        release(
            "99.0.5",
            archive(dir.path(), &top("99.0.5"), b"99.0.5"),
            &key(),
        ),
    )
    .await;
    let path = dir.path().to_path_buf();
    tokio::task::spawn_blocking(move || {
        let dir = path.as_path();
        let mut cfg: Config = toml::from_str(&Config::example("N0AAA", dir)).unwrap();
        cfg.sync.discover = false;
        let log = Log::new(
            Store::open(&cfg.db_path()).unwrap(),
            Identity::from_seed("N0AAA", [1; 32]),
        )
        .unwrap();
        let app = App::new(cfg, log).unwrap();
        let ok = |v: &str| release(v, archive(dir, &top(v), v.as_bytes()), &key());
        publish(&app, dir, "98.0.0", ok("98.0.0"));
        publish(
            &app,
            dir,
            "99.1.0",
            release(
                "99.1.0",
                archive(dir, &top("99.1.0"), b"99.1.0"),
                &stranger(),
            ),
        );
        // a name that isn't a plain version, and checksums too big to be real
        publish(&app, dir, "99.2.0-x y", ok("99.2.0-x y"));
        let big = archive(dir, &top("99.3.0"), b"99.3.0");
        let padded = format!("{}{}", sums_of("99.3.0", &big), "#\n".repeat(40 * 1024));
        publish(&app, dir, "99.3.0", signed("99.3.0", big, padded, &key()));

        let work = WorkDir::new().unwrap();
        let mut found = update::from_mesh(&app, &work).unwrap();
        let mut on_mesh: Vec<&str> = found.iter().map(|f| f.version.as_str()).collect();
        on_mesh.sort();
        assert_eq!(on_mesh, ["98.0.0", "99.1.0"]);
        found.extend(
            update::latest_from(
                &format!("{base}/latest"),
                &format!("{base}/download"),
                &work,
            )
            .unwrap(),
        );

        let keys = [key().verifying_key()];
        let (pick, passed_over) = update::newest_verified(found, &keys);
        let pick = pick.unwrap();
        assert_eq!(
            (pick.version.as_str(), pick.source.as_str()),
            ("99.0.5", "GitHub")
        );
        assert_eq!(passed_over.len(), 1, "{passed_over:?}");
        assert!(
            passed_over[0].starts_with("v99.1.0 from the mesh")
                && passed_over[0].contains("does not match any release key"),
            "{passed_over:?}"
        );
        let exe = dir.join("ridgewarden");
        std::fs::write(&exe, OLD).unwrap();
        update::install_verified(&pick, &keys, &exe).unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"99.0.5");
    })
    .await
    .unwrap();
}

#[test]
fn the_work_folder_is_private_and_goes_away() {
    let work = WorkDir::new().unwrap();
    let path = work.path().to_path_buf();
    assert!(path.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(path.parent(), Some(Path::new("/var/tmp")));
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    std::fs::write(path.join("file"), b"x").unwrap();
    drop(work);
    assert!(!path.exists());
}

#[test]
#[ignore = "the_work_folder_ignores_tmpdir runs it with TMPDIR set"]
fn print_work_folder() {
    if std::env::var_os("RIDGEWARDEN_PRINT_WORK").is_some() {
        println!("work folder: {}", WorkDir::new().unwrap().path().display());
    }
}

#[test]
fn the_work_folder_ignores_tmpdir() {
    // what `su` or `sudo TMPDIR=...` could hand root: a folder anyone can change
    let open = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(open.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(WorkDir::under(open.path()).is_err());
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args(["print_work_folder", "--exact", "--ignored", "--nocapture"])
        .env("TMPDIR", open.path())
        .env("RIDGEWARDEN_PRINT_WORK", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("work folder: "))
        .unwrap_or_else(|| panic!("no work folder in {stdout}"));
    assert!(!Path::new(line).starts_with(open.path()), "{line}");
}

#[test]
fn the_folder_0_1_0_worked_in_is_cleared() {
    let data = tempfile::tempdir().unwrap();
    let old = data.path().join("update");
    std::fs::create_dir_all(old.join("github")).unwrap();
    std::fs::write(old.join("github").join("archive.tar.gz"), b"x").unwrap();
    update::clear_old_folder(data.path()).unwrap();
    assert!(!old.exists());
    update::clear_old_folder(data.path()).unwrap();

    // a link planted there goes, and nothing it points at
    #[cfg(unix)]
    {
        let keep = data.path().join("keep");
        std::fs::create_dir(&keep).unwrap();
        std::fs::write(keep.join("file"), b"x").unwrap();
        std::os::unix::fs::symlink(&keep, &old).unwrap();
        update::clear_old_folder(data.path()).unwrap();
        assert!(!old.exists() && keep.join("file").exists());
    }
}
