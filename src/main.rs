use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;

use ridgewarden::config::Config;
use ridgewarden::usage::{self, human_bytes};
use ridgewarden::{agent, blobs, diff, qr, trust, update, util};
use ridgewarden_client::tui::Fetch;

mod tui;

#[derive(Parser)]
#[command(version, about = "LAN-side edge agent for AREDN meshes")]
struct Cli {
    /// config file
    #[arg(
        long,
        short,
        global = true,
        default_value = "/etc/ridgewarden/ridgewarden.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Write a starter config and create this station's signing key
    Init {
        #[arg(long)]
        callsign: String,
        #[arg(long, default_value = "/var/lib/ridgewarden")]
        data_dir: PathBuf,
        /// the AREDN node on this LAN
        #[arg(long)]
        node_url: Option<String>,
    },
    /// Run the agent: poll the node, probe services, sync with peers, serve the dashboard
    Run,
    /// Show what changed on the mesh, as seen from this node
    Diff {
        #[arg(long, default_value_t = 24)]
        hours: i64,
    },
    /// Print this station's callsign and public key
    Whoami,
    /// Show whether the agent is running, its node, peers, and disk use per class
    Status,
    /// Watch this agent live in the terminal: its node, peers, log, blobs and events
    Top {
        /// seconds between reads
        #[arg(long, default_value_t = 2, value_parser = clap::value_parser!(u64).range(1..=600))]
        every: u64,
    },
    /// Print a one-line invite another site can join with, and its QR code
    Invite {
        /// this site's address on the mesh (defaults to public_url in the config)
        #[arg(long)]
        url: Option<String>,
    },
    /// Join the mesh group: trust the inviting station and everyone it trusts
    Join {
        invite: String,
        /// accept without asking
        #[arg(long)]
        yes: bool,
    },
    /// Revoke this station's key, e.g. after its SD card was stolen. Events it writes after
    /// this are kept but no longer believed anywhere. Make a new key afterwards.
    Revoke {
        #[arg(long)]
        reason: String,
        /// revoke without asking
        #[arg(long)]
        yes: bool,
    },
    /// Check for a newer signed release (GitHub or the mesh) and install it if you say yes
    Update {
        /// only report whether an update is available
        #[arg(long)]
        check: bool,
        /// install without asking
        #[arg(long)]
        yes: bool,
        /// also republish the release on the mesh for sites without internet
        #[arg(long)]
        publish: bool,
    },
    /// Check a release's SHA256SUMS.txt against its .sig with this build's release keys
    VerifyRelease { sums: PathBuf },
    /// Make a release signing key (maintainers only)
    ReleaseKey {
        /// where to write the private key; keep it secret
        #[arg(long)]
        out: PathBuf,
    },
    /// Sign a SHA256SUMS.txt with the release key from an environment variable (used by CI)
    ReleaseSign {
        file: PathBuf,
        #[arg(long, default_value = "RIDGEWARDEN_RELEASE_KEY")]
        key_env: String,
    },
    /// Share and fetch files over the mesh
    Blob {
        #[command(subcommand)]
        cmd: BlobCmd,
    },
    /// Show or change who this site trusts
    Trust {
        #[command(subcommand)]
        cmd: TrustCmd,
    },
}

#[derive(Subcommand)]
enum BlobCmd {
    /// Publish a file: chunk it into this site's store and sign its manifest
    Add {
        file: PathBuf,
        /// name other sites see (defaults to the file name)
        #[arg(long)]
        name: Option<String>,
        /// 0 to 9: sites fetch lower numbers first; most files are 5
        #[arg(long, default_value_t = blobs::DEFAULT_PRIORITY, value_parser = clap::value_parser!(u8).range(0..=9))]
        priority: u8,
    },
    /// List blobs this site knows about
    List,
    /// Fetch a blob from other sites
    Want {
        root: String,
        /// ask this peer first (repeat for more, in order), e.g. http://10.1.2.3:8790
        #[arg(long = "from")]
        from: Vec<String>,
        /// ask only the --from peers; chunks they don't have wait
        #[arg(long, requires = "from")]
        only: bool,
    },
    /// Stop fetching or keeping a blob and free its disk space
    Unwant { root: String },
    /// Write a complete blob out to a file
    Export { root: String, path: PathBuf },
}

#[derive(Subcommand)]
enum TrustCmd {
    /// List trusted stations and why each is trusted
    List,
    /// Add a station to the signed trust list
    Add {
        key: String,
        #[arg(long)]
        call: Option<String>,
    },
    /// Remove a station from the signed trust list
    Remove { key: String },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // top draws the whole terminal, so a log line would land in the middle of it
    if !matches!(cli.cmd, Cmd::Top { .. }) {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "ridgewarden=info".into()),
            )
            .init();
    }
    match cli.cmd {
        Cmd::Init {
            callsign,
            data_dir,
            node_url,
        } => init(&cli.config, &callsign, &data_dir, node_url),
        Cmd::Run => {
            let cfg = Config::load(&cli.config)?;
            tokio::runtime::Runtime::new()?.block_on(agent::run(cfg))
        }
        Cmd::Diff { hours } => {
            let app = agent::open(Config::load(&cli.config)?)?;
            match diff::changes(&app.log.store.conn(), util::now_ms(), hours)? {
                None => println!("Not enough history yet."),
                Some(ch) if ch.lines.is_empty() => println!("No changes."),
                Some(ch) => {
                    println!(
                        "{}: {} compared with {}",
                        ch.node,
                        util::short_time(ch.to_ts),
                        util::short_time(ch.from_ts)
                    );
                    for l in ch.lines {
                        println!("  {l}");
                    }
                }
            }
            Ok(())
        }
        Cmd::Revoke { reason, yes } => {
            let app = agent::open(Config::load(&cli.config)?)?;
            println!(
                "This tells every site to stop believing anything {} writes from now on.",
                app.log.id.author.call
            );
            if !yes && !confirm("Revoke this station's key?")? {
                println!("Not revoked.");
                return Ok(());
            }
            app.log
                .append("identity.revoke", json!({"reason": reason}))?;
            println!("Revoked. Move identity.key aside and run init again to make a new key.");
            Ok(())
        }
        Cmd::Invite { url } => {
            let cfg = Config::load(&cli.config)?;
            let Some(url) = url.or_else(|| cfg.public_url.clone()) else {
                bail!(
                    "give --url http://<this site's mesh address>:8790, or set public_url in the config"
                );
            };
            let app = agent::open(cfg)?;
            let line = trust::make_invite(&app.log, &url)?;
            println!("{line}");
            // a pipe or file gets just the line
            if std::io::stdout().is_terminal() {
                print_qr(&line);
            }
            Ok(())
        }
        Cmd::Join { invite, yes } => {
            let app = agent::open(Config::load(&cli.config)?)?;
            let preview = ridgewarden::invite::read(&invite)?;
            println!("Invite from {} (key {}).", preview.by.call, preview.by.key);
            println!(
                "Accepting means this site trusts {} to say who is on the trust list, and trusts the {} station(s) it lists until its trust list arrives.",
                preview.by.call,
                preview.stations.len()
            );
            if !yes && !confirm("Accept?")? {
                println!("Not joined.");
                return Ok(());
            }
            let inv = trust::join(&app.log, &invite)?;
            println!(
                "Joined: now trusting {} and the {} station(s) it trusts.",
                inv.by.call,
                inv.stations.len()
            );
            for p in &inv.peers {
                println!("Will sync with {p} (restart the agent to start).");
            }
            Ok(())
        }
        Cmd::Update {
            check,
            yes,
            publish,
        } => run_update(&cli.config, check, yes, publish),
        Cmd::VerifyRelease { sums } => {
            let sig = std::fs::read_to_string(sums.with_extension("txt.sig"))
                .context("reading the signature next to the checksum file")?;
            update::verify_sums(&std::fs::read(&sums)?, &sig, &update::release_keys()?)?;
            println!("Release signature OK.");
            Ok(())
        }
        Cmd::ReleaseKey { out } => {
            let public = update::new_release_key(&out)?;
            println!(
                "Private key written to {} (only readable by you). Keep it secret.",
                out.display()
            );
            println!("Public key: {public}");
            println!("Set it as RIDGEWARDEN_RELEASE_KEYS when building releases.");
            Ok(())
        }
        Cmd::ReleaseSign { file, key_env } => {
            let key = std::env::var(&key_env).with_context(|| format!("{key_env} is not set"))?;
            let sig = update::sign_sums(&std::fs::read(&file)?, &update::load_release_key(&key)?);
            let out = file.with_extension("txt.sig");
            std::fs::write(&out, sig)?;
            println!("Wrote {}", out.display());
            Ok(())
        }
        Cmd::Blob { cmd } => {
            let app = agent::open(Config::load(&cli.config)?)?;
            match cmd {
                BlobCmd::Add {
                    file,
                    name,
                    priority,
                } => {
                    let name = name.unwrap_or_else(|| {
                        file.file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default()
                    });
                    let root = blobs::add_file(&app.log, &app.blobs, &file, &name, priority)?;
                    println!("{root}");
                }
                BlobCmd::List => {
                    let conn = app.log.store.conn();
                    let mut st = conn.prepare(
                        "SELECT root, name, size, wanted, complete FROM blobs ORDER BY added_ms DESC",
                    )?;
                    let rows = st.query_map([], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, bool>(3)?,
                            r.get::<_, bool>(4)?,
                        ))
                    })?;
                    for r in rows {
                        let (root, name, size, wanted, complete) = r?;
                        let state = match (complete, wanted) {
                            (true, _) => "here",
                            (false, true) => "fetching",
                            (false, false) => "available",
                        };
                        println!("{}  {state:<9} {:>12}  {name}", &root[..16], size);
                    }
                }
                BlobCmd::Want { root, from, only } => {
                    let sources = blobs::sources_from(&from)?;
                    let conn = app.log.store.conn();
                    blobs::prefer_trusted(&conn, &root, &app.log.trusted())?;
                    let found = if sources.is_empty() {
                        blobs::want(&conn, &root, true)?
                    } else {
                        blobs::want_from(&conn, &root, &sources, only)?
                    };
                    if !found {
                        bail!("no manifest for that blob yet; it arrives with the next sync");
                    }
                    println!("Queued; the running agent fetches it within its rate and window.");
                }
                BlobCmd::Unwant { root } => {
                    match blobs::unwant(&app.log.store.conn(), &app.blobs, &root)? {
                        Some(n) => println!("No longer wanted; freed {}.", human_bytes(n)),
                        None => bail!("no manifest for that blob"),
                    }
                }
                BlobCmd::Export { root, path } => {
                    let Some(m) = blobs::manifest(&app.log.store.conn(), &root)? else {
                        bail!("no manifest for that blob");
                    };
                    if let Err(e) = app.blobs.export(&m, &path) {
                        blobs::recheck(&app.log.store.conn(), &app.blobs, &m)?;
                        return Err(e);
                    }
                    println!("Wrote {} ({} bytes)", path.display(), m.size);
                }
            }
            Ok(())
        }
        Cmd::Trust { cmd } => {
            let cfg = Config::load(&cli.config)?;
            let pinned = cfg.trust.keys.clone();
            let app = agent::open(cfg)?;
            match cmd {
                TrustCmd::List => {
                    for (call, key, why) in trust::explain(&app.log, &pinned)? {
                        println!("{call:<12} {}  {why}", &key[..16]);
                    }
                }
                TrustCmd::Add { key, call } => {
                    let v = trust::change_trust_list(&app.log, &key, call.as_deref(), true)?;
                    println!(
                        "Trust list v{v} published; other sites pick it up on their next sync."
                    );
                }
                TrustCmd::Remove { key } => {
                    let v = trust::change_trust_list(&app.log, &key, None, false)?;
                    println!("Trust list v{v} published without that station.");
                }
            }
            Ok(())
        }
        Cmd::Status => status(&cli.config),
        Cmd::Top { every } => tui::run(Config::load(&cli.config)?, Duration::from_secs(every)),
        Cmd::Whoami => {
            let app = agent::open(Config::load(&cli.config)?)?;
            println!("{} {}", app.log.id.author.call, app.log.id.author.key);
            Ok(())
        }
    }
}

fn print_qr(line: &str) {
    match qr::encode(line.as_bytes()) {
        Ok(q) => {
            println!();
            print!("{}", q.to_terminal());
            println!(
                "Scan it on the new site, or paste the line above into `ridgewarden join`. If it wraps, make the terminal at least {} columns wide.",
                q.size + 8
            );
        }
        Err(e) => println!("No QR code: {e:#}. Paste the line instead."),
    }
}

fn status(config: &Path) -> Result<()> {
    let app = agent::open(Config::load(config)?)?;
    let now = util::now_ms();
    let a = &app.log.id.author;
    println!("{} on {}, key {}", a.call, app.node_name(), a.key);

    let last_poll = ridgewarden::node::last_poll(&app.log.store.conn())?;
    println!(
        "Node {}: {}",
        app.cfg.node.url,
        tui::poll_state(last_poll, now)
    );

    let base = tui::local_base(&app.cfg);
    let fetch = Fetch::new(Duration::from_secs(3))?;
    match tokio::runtime::Runtime::new()?.block_on(tui::running_peers(&fetch, &base)) {
        Ok(peers) => {
            println!("Agent: running, dashboard on {base}/");
            if peers.is_empty() {
                println!(
                    "Peers: none yet (add [sync] peers, join with an invite, or advertise a ridgewarden service)"
                );
            }
            for p in peers {
                println!("  {:<10} {}  {}", p.who(), p.url, p.state(now));
            }
        }
        Err(e) => println!(
            "Agent: not answering on {base} ({}); {}",
            ridgewarden::app::short_err(&e),
            tui::START
        ),
    }

    let heads = app.log.heads()?;
    let stations = heads.iter().filter(|h| !h.key.contains(':')).count();
    println!(
        "Log: {} station(s) writing, {} trusted, {} fork point(s)",
        stations,
        app.log.trusted().len(),
        app.log.fork_count()?
    );
    let u = usage::measure(&app)?;
    let size = |n: i64| human_bytes(n.max(0) as u64);
    println!("Disk:");
    println!(
        "  main log     {:>9} events  {:>9}  {}",
        u.main.events,
        size(u.main.bytes),
        u.main_outlook()
    );
    println!(
        "  telemetry    {:>9} events  {:>9}  {}, keeping {} days",
        u.telemetry.events,
        size(u.telemetry.bytes),
        u.telemetry_outlook(),
        u.telemetry_days
    );
    println!(
        "  blobs        {:>9} chunks  {:>9}",
        u.blob_chunks,
        human_bytes(u.blob_bytes)
    );
    println!(
        "  database file                {:>9}",
        human_bytes(u.db_bytes)
    );
    println!(
        "  other files                  {:>9}  exports, imports",
        human_bytes(u.files_bytes)
    );
    println!(
        "  total on disk                {:>9}",
        human_bytes(u.on_disk())
    );
    Ok(())
}

fn confirm(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

fn run_update(config: &Path, check: bool, yes: bool, publish: bool) -> Result<()> {
    let app = agent::open(Config::load(config)?)?;
    if let Err(e) = update::clear_old_folder(&app.cfg.data_dir) {
        println!("{e:#}");
    }
    let work = update::WorkDir::new()?;
    removed_on_ctrl_c(work.path())?;
    let mut tried = update::from_mesh(&app, &work)?;
    match update::from_github(&work) {
        Ok(Some(f)) => tried.push(f),
        Ok(None) => {}
        Err(e) => println!("Couldn't check GitHub: {e:#}"),
    }
    let keys = update::release_keys()?;
    // newest first, from the mesh or GitHub alike, and the first that checks out wins, so
    // one bad or tampered release can't block updating
    let (found, passed_over) = update::newest_verified(tried, &keys);
    for why in passed_over {
        println!("Skipping {why}");
    }
    let Some(found) = found else {
        println!(
            "No newer verified release found (you have v{}).",
            env!("CARGO_PKG_VERSION")
        );
        return Ok(());
    };
    println!(
        "ridgewarden v{} is available from {} (you have v{}). Signature and checksum verified.",
        found.version,
        found.source,
        env!("CARGO_PKG_VERSION")
    );
    if publish {
        let n = update::publish_on_mesh(&app, &found)?;
        println!("Republished {n} file(s) on the mesh.");
    }
    if check {
        return Ok(());
    }
    if !yes && !confirm("Install it now?")? {
        println!("Not installed.");
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    update::install_verified(&found, &keys, &exe)?;
    println!(
        "Installed {}. Restart the agent to use it: sudo systemctl restart ridgewarden",
        exe.display()
    );
    Ok(())
}

/// Ctrl-C, at the prompt say, ends the process without dropping the work folder, so it goes
/// then too.
fn removed_on_ctrl_c(work: &Path) -> Result<()> {
    let work = work.to_path_buf();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    std::thread::spawn(move || {
        // if the handler can't be set up, Ctrl-C just ends the process as it always did
        if rt.block_on(tokio::signal::ctrl_c()).is_ok() {
            // on the way out; nothing more can be done if this fails
            let _ = std::fs::remove_dir_all(&work);
            std::process::exit(130);
        }
    });
    Ok(())
}

fn init(path: &Path, callsign: &str, data_dir: &Path, node_url: Option<String>) -> Result<()> {
    if path.exists() {
        bail!("{} already exists; not overwriting it", path.display());
    }
    let mut text = Config::example(callsign, data_dir);
    if let Some(u) = node_url {
        text = text.replace(
            "url = \"http://localnode.local.mesh\"",
            &format!("url = {}", toml_str(&u)),
        );
    }
    let cfg: Config = toml::from_str(&text).context("generated config did not parse")?;
    cfg.validate()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, &text)?;
    let id = ridgewarden::identity::Identity::load_or_create(&cfg.key_path(), callsign)?;
    ridgewarden::agent::ensure_token(&cfg)?;
    println!("Wrote {}", path.display());
    println!("Key {} for {}", cfg.key_path().display(), id.author.call);
    println!("Public key: {}", id.author.key);
    println!(
        "App token {} (give its path to apps that write events)",
        cfg.token_path().display()
    );
    Ok(())
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
}
