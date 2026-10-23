//! Drive harness (feature `harness`): IPC commands that open a view by URI and write a JSON
//! snapshot of the current view, so a script can drive ncspot and check what it shows.

use serde::Serialize;

/// One list in the current view.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TabState {
    pub name: String,
    /// Rows loaded so far.
    pub rows: usize,
    /// Rows that can be played.
    pub playable: usize,
    /// Total rows the source reports, `None` when the list has no pagination (fully loaded).
    pub max: Option<usize>,
    /// Whether a page is being fetched.
    pub busy: bool,
}

/// What a view reports to the harness.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HarnessState {
    pub view: String,
    /// Name of the selected tab, for tabbed views.
    pub tab: Option<String>,
    pub tabs: Vec<TabState>,
}

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::activity::ACTIVITY;
use crate::internal_api::{Area, AreaCounts};
use crate::rate_limit::{ClientLabel, Stamp};
use crate::spotify_api::{WebApi, WebCalls};

/// Internal API counters per area.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Areas {
    pub metadata: AreaCounts,
}

/// Which clients are inside a rate-limit window.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Throttled {
    pub shared: bool,
    pub personal: bool,
    pub internal: bool,
}

/// The snapshot `harness dump` writes. Field order is the JSON order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Dump {
    /// Increases on every dump, so a reader can tell a fresh file from a stale one.
    pub seq: u64,
    pub view: Option<String>,
    pub title: String,
    pub tab: Option<String>,
    pub tabs: Vec<TabState>,
    pub in_flight: usize,
    /// Milliseconds since the last call ended; `None` before the first one.
    pub quiet_ms: Option<u64>,
    /// The last command's error, as shown in the status bar.
    pub error: Option<String>,
    pub areas: Areas,
    pub web: WebCalls,
    pub throttled: Throttled,
}

static SEQ: AtomicU64 = AtomicU64::new(0);

/// Snapshot the view state plus the counters of `api` and the call gauge.
pub fn build_dump(
    title: String,
    state: Option<HarnessState>,
    error: Option<String>,
    api: &WebApi,
) -> Dump {
    let limited = api.rate_limit_status();
    let throttled = |label: ClientLabel| limited.iter().any(|(l, _)| *l == label);
    let (view, tab, tabs) = match state {
        Some(state) => (Some(state.view), state.tab, state.tabs),
        None => (None, None, Vec::new()),
    };
    Dump {
        seq: SEQ.fetch_add(1, Ordering::Relaxed) + 1,
        view,
        title,
        tab,
        tabs,
        in_flight: ACTIVITY.in_flight(),
        quiet_ms: ACTIVITY
            .quiet_for(Stamp::now())
            .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        error,
        areas: Areas {
            metadata: api.area_counts(Area::Metadata),
        },
        web: api.web_calls(),
        throttled: Throttled {
            shared: throttled(ClientLabel::Shared),
            personal: throttled(ClientLabel::Personal),
            internal: throttled(ClientLabel::Internal),
        },
    }
}

/// Write `dump` to `path` atomically: a reader sees the old file or the new one, never half.
pub fn write_dump(path: &Path, dump: &Dump) -> std::io::Result<()> {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = std::path::PathBuf::from(tmp);
    let json = serde_json::to_vec(dump).map_err(std::io::Error::other)?;
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&json)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::internal_api::AreaCounts;
    use crate::spotify_api::WebCalls;

    fn sample() -> Dump {
        Dump {
            seq: 7,
            view: Some("album".into()),
            title: "T".into(),
            tab: Some("Tracks".into()),
            tabs: vec![TabState {
                name: "Tracks".into(),
                rows: 12,
                playable: 11,
                max: Some(12),
                busy: false,
            }],
            in_flight: 0,
            quiet_ms: Some(850),
            error: None,
            areas: Areas {
                metadata: AreaCounts {
                    internal_ok: 14,
                    fallback: 0,
                    latched: false,
                },
            },
            web: WebCalls {
                shared: 0,
                personal: 2,
            },
            throttled: Throttled {
                shared: false,
                personal: false,
                internal: false,
            },
        }
    }

    #[test]
    fn dump_serializes_to_the_documented_shape() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert_eq!(
            json,
            r#"{"seq":7,"view":"album","title":"T","tab":"Tracks","tabs":[{"name":"Tracks","rows":12,"playable":11,"max":12,"busy":false}],"in_flight":0,"quiet_ms":850,"error":null,"areas":{"metadata":{"internal_ok":14,"fallback":0,"latched":false}},"web":{"shared":0,"personal":2},"throttled":{"shared":false,"personal":false,"internal":false}}"#
        );
    }

    #[test]
    fn write_dump_replaces_the_file_atomically() {
        let dir = std::env::temp_dir().join(format!("ncspot-harness-dump-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dump.json");
        write_dump(&path, &sample()).unwrap();
        let mut second = sample();
        second.seq = 8;
        write_dump(&path, &second).unwrap();
        let read: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(read["seq"], 8);
        assert!(
            !dir.join("dump.json.tmp").exists(),
            "temp file renamed away"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn build_dump_reads_the_counters_and_bumps_seq() {
        let api = crate::spotify_api::WebApi::new(&crate::config::Config::new_for_test());
        let first = build_dump("x".into(), None, Some("boom".into()), &api);
        let second = build_dump("x".into(), None, None, &api);
        assert!(second.seq > first.seq);
        assert_eq!(first.error.as_deref(), Some("boom"));
        assert_eq!(first.view, None);
        assert!(first.tabs.is_empty());
        assert_eq!(first.web, WebCalls::default());
        assert!(!first.throttled.shared && !first.throttled.personal && !first.throttled.internal);
    }
}
