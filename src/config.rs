//! `ridgewarden.toml`, the core agent's config. Each app has its own config file.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// the licensee's callsign; every event this agent writes is signed under it
    pub callsign: String,
    #[serde(default = "d_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default = "d_listen")]
    pub listen: SocketAddr,
    /// how other sites reach this core over the mesh, e.g. "http://n0call-pi.local.mesh:8790";
    /// put into invites this site hands out
    #[serde(default)]
    pub public_url: Option<String>,
    pub node: NodeCfg,
    #[serde(default)]
    pub sync: SyncCfg,
    #[serde(default)]
    pub crawl: CrawlCfg,
    #[serde(default)]
    pub trust: TrustCfg,
    #[serde(default)]
    pub retention: RetentionCfg,
    #[serde(default)]
    pub blobs: BlobsCfg,
    /// apps running next to this core, linked from its dashboard
    #[serde(default, rename = "app")]
    pub apps: Vec<AppLink>,
}

/// Whose events the apps believe. Every signed event is stored and relayed regardless;
/// this only decides what the apps build their pages from. This station's own key is
/// always trusted. Add the public keys (`ridgewarden whoami`) of the other club agents.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields)]
pub struct TrustCfg {
    #[serde(default)]
    pub keys: Vec<String>,
    /// keys whose events this site refuses to store or pass on at all
    #[serde(default)]
    pub blocked: Vec<String>,
}

/// Moving files between sites. Fetching never starts on its own except for blobs whose
/// names match `auto_want` and that were published by a trusted station.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct BlobsCfg {
    /// average rate cap, kilobits per second, so bulk files never crowd out other traffic
    #[serde(default = "d_rate")]
    pub rate_kbps: u64,
    /// only fetch between these UTC hours, e.g. "1-6"; empty means any time
    #[serde(default)]
    pub window: String,
    /// fetch blobs from trusted stations whose names start with any of these (by default
    /// ridgewarden releases, so sites without internet can still update)
    #[serde(default = "d_auto_want")]
    pub auto_want: Vec<String>,
    /// a lower cap, in kbps, for chunks from particular peers (by base URL), for slow links
    #[serde(default)]
    pub peer_rate_kbps: std::collections::BTreeMap<String, u64>,
}

impl Default for BlobsCfg {
    fn default() -> Self {
        Self {
            rate_kbps: d_rate(),
            window: String::new(),
            auto_want: d_auto_want(),
            peer_rate_kbps: Default::default(),
        }
    }
}

fn d_auto_want() -> Vec<String> {
    vec![crate::update::BLOB_PREFIX.to_string()]
}

/// 10 Gbps, far above any mesh link; it keeps the bytes-per-second sums in range.
const MAX_KBPS: u64 = 10_000_000;

fn d_rate() -> u64 {
    2000
}

impl BlobsCfg {
    /// The fetch window as (start, end) UTC hours.
    pub fn window(&self) -> Result<Option<(u32, u32)>> {
        if self.window.trim().is_empty() {
            return Ok(None);
        }
        let (a, b) = self
            .window
            .split_once('-')
            .context("blobs.window looks like \"1-6\" (UTC hours)")?;
        let (a, b): (u32, u32) = (a.trim().parse()?, b.trim().parse()?);
        if a > 23 || b > 24 || a == b {
            bail!("blobs.window hours must be 0-24 and differ");
        }
        Ok(Some((a, b)))
    }
}

/// How long this station keeps its own telemetry (polls, probes) before a checkpoint lets
/// every site prune it. Everything else is kept.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct RetentionCfg {
    #[serde(default = "d_30")]
    pub telemetry_days: u64,
}

impl Default for RetentionCfg {
    fn default() -> Self {
        Self { telemetry_days: 30 }
    }
}

fn d_30() -> u64 {
    30
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct NodeCfg {
    /// base URL of the node; `/a/sysinfo` is appended
    #[serde(default = "d_node_url")]
    pub url: String,
    /// AREDN asks that you agree a schedule with the node's owner. 60 s is the floor.
    #[serde(default = "d_300")]
    pub poll_interval_s: u64,
    #[serde(default = "d_20")]
    pub timeout_s: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct SyncCfg {
    #[serde(default = "d_120")]
    pub interval_s: u64,
    /// how many peers to pull from each round
    #[serde(default = "d_2")]
    pub fanout: usize,
    /// peers to always try, e.g. "http://10.1.2.3:8790"
    #[serde(default)]
    pub peers: Vec<String>,
    /// find peers from services named "ridgewarden..." in the node's service list
    #[serde(default = "d_true")]
    pub discover: bool,
    /// pull other stations' telemetry (polls, probes); a data-mule car turns this off
    #[serde(default = "d_true")]
    pub telemetry: bool,
}

/// Polling other nodes across the mesh, so links between sites without an agent show up
/// too. Off by default: it means polling nodes other people own, so agree it with them,
/// and turn it on for one or two hub agents, not every agent.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct CrawlCfg {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "d_900")]
    pub interval_s: u64,
    /// `{node}` is replaced with the node name
    #[serde(default = "d_crawl_url")]
    pub url_template: String,
    /// nodes to crawl, by name
    #[serde(default)]
    pub nodes: Vec<String>,
    /// also crawl any node that a crawled node reports as a neighbor
    #[serde(default)]
    pub follow_links: bool,
    /// upper bound per round when following links
    #[serde(default = "d_50")]
    pub max_nodes: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct AppLink {
    pub name: String,
    pub url: String,
}

fn d_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/ridgewarden")
}
fn d_listen() -> SocketAddr {
    "0.0.0.0:8790".parse().unwrap()
}
fn d_node_url() -> String {
    "http://localnode.local.mesh".into()
}
fn d_crawl_url() -> String {
    "http://{node}.local.mesh".into()
}
fn d_900() -> u64 {
    900
}
fn d_300() -> u64 {
    300
}
fn d_120() -> u64 {
    120
}
fn d_20() -> u64 {
    20
}
fn d_2() -> usize {
    2
}
fn d_50() -> usize {
    50
}
fn d_true() -> bool {
    true
}

impl Default for SyncCfg {
    fn default() -> Self {
        Self {
            interval_s: 120,
            fanout: 2,
            peers: vec![],
            discover: true,
            telemetry: true,
        }
    }
}

impl Default for CrawlCfg {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_s: 900,
            url_template: d_crawl_url(),
            nodes: vec![],
            follow_links: false,
            max_nodes: 50,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<()> {
        if !ridgewarden_client::event::valid_call(&self.callsign.trim().to_ascii_uppercase()) {
            bail!(
                "callsign {:?} is not a callsign (letters, digits, '/' and '-')",
                self.callsign
            );
        }
        if self.node.poll_interval_s < 60 {
            bail!("node.poll_interval_s must be at least 60 (be polite to the node)");
        }
        if self.crawl.enabled && self.crawl.interval_s < 60 {
            bail!("crawl.interval_s must be at least 60");
        }
        self.blobs.window()?;
        if !(1..=MAX_KBPS).contains(&self.blobs.rate_kbps) {
            bail!("blobs.rate_kbps must be 1 to {MAX_KBPS}");
        }
        for (url, kbps) in &self.blobs.peer_rate_kbps {
            if crate::blobs::peer_url(url).is_none() {
                bail!(
                    "blobs.peer_rate_kbps keys are peer addresses like \"http://10.1.2.3:8790\", not {url:?}"
                );
            }
            if !(1..=MAX_KBPS).contains(kbps) {
                bail!("blobs.peer_rate_kbps for {url} must be 1 to {MAX_KBPS}");
            }
        }
        if self.retention.telemetry_days == 0 {
            bail!("retention.telemetry_days must be at least 1");
        }
        for k in self.trust.keys.iter().chain(&self.trust.blocked) {
            if k.len() != 64 || !k.bytes().all(|b| b.is_ascii_hexdigit()) {
                bail!("trust.keys entries are 64-hex-digit public keys, not {k:?}");
            }
        }
        if self.sync.interval_s < 5 {
            bail!("sync.interval_s must be at least 5");
        }
        Ok(())
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("ridgewarden.db")
    }

    pub fn key_path(&self) -> PathBuf {
        self.data_dir.join("identity.key")
    }

    /// Apps that write events present this token to the core's local API.
    /// Where apps put files for `/local/v1/blob/add`.
    pub fn imports_dir(&self) -> PathBuf {
        self.data_dir.join("imports")
    }

    pub fn blob_dir(&self) -> PathBuf {
        self.data_dir.join("blobs")
    }

    pub fn token_path(&self) -> PathBuf {
        self.data_dir.join("local.token")
    }

    pub fn example(callsign: &str, data_dir: &Path) -> String {
        format!(
            r#"# ridgewarden core agent configuration
callsign = "{callsign}"
data_dir = "{dir}"
listen = "0.0.0.0:8790"
# how other sites reach this core over the mesh (used in invites)
# public_url = "http://n0call-pi.local.mesh:8790"

[node]
# the AREDN node on this LAN; ridgewarden only reads its HTTP API
url = "http://localnode.local.mesh"
# agree the schedule with the node's owner; 60 s minimum
poll_interval_s = 300

[sync]
interval_s = 120
fanout = 2
peers = []
discover = true
# pull other stations' polls and probes; a data-mule car sets this false to carry less
telemetry = true

[crawl]
# poll other nodes for their links too; agree this with their owners first
enabled = false
interval_s = 900
url_template = "http://{{node}}.local.mesh"
nodes = []
follow_links = false

[trust]
# public keys of the other club agents whose data the apps should believe
# (run `ridgewarden whoami` on each); this station's own key is always trusted
keys = []
blocked = []

[blobs]
# bulk files (offline content, updates) move at most this fast, in this UTC window
rate_kbps = 2000
window = ""
auto_want = ["ridgewarden-release/"]
# a lower cap for chunks fetched over a slow link, by the peer's address
# [blobs.peer_rate_kbps]
# "http://10.1.2.3:8790" = 500

[retention]
# your own polls and probes are pruned everywhere after this many days
telemetry_days = 30

# apps running next to this core, linked from its dashboard
# [[app]]
# name = "Service health"
# url = "http://localhost:8791/"
"#,
            dir = data_dir.display().to_string().replace('\\', "/")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let c: Config = toml::from_str(&Config::example("N0CALL", Path::new("/tmp/x"))).unwrap();
        c.validate().unwrap();
        assert_eq!(c.node.poll_interval_s, 300);
        assert_eq!(c.crawl.url_template, "http://{node}.local.mesh");
    }

    #[test]
    fn peer_rates_need_peer_addresses() {
        let base = Config::example("N0CALL", Path::new("/tmp/x"));
        let with = |table: &str| {
            let text = base.replace(
                "# [blobs.peer_rate_kbps]\n# \"http://10.1.2.3:8790\" = 500",
                table,
            );
            toml::from_str::<Config>(&text).unwrap().validate()
        };
        with("[blobs.peer_rate_kbps]\n\"http://10.1.2.3:8790\" = 500").unwrap();
        assert!(with("[blobs.peer_rate_kbps]\n\"10.1.2.3\" = 500").is_err());
        assert!(with("[blobs.peer_rate_kbps]\n\"http://10.1.2.3:8790\" = 0").is_err());
    }

    #[test]
    fn rejects_rude_poll_interval() {
        let mut c: Config =
            toml::from_str(&Config::example("N0CALL", Path::new("/tmp/x"))).unwrap();
        c.node.poll_interval_s = 10;
        assert!(c.validate().is_err());
    }
}
