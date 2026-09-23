//! Repository metadata collection via per-VCS adapters.
//!
//! Repositories are analyzed through platform CLIs (`git`, `hg`, `svn`)
//! following the provider-plugin pattern. An adapter whose CLI is
//! unavailable leaves that repository's metadata empty and analysis
//! continues, exactly like other optional tools.

use std::path::Path;
use std::process::Command;
use std::time::Duration;
use std::time::SystemTime;

use homelinter_scan::{RepoEntry, ToolSet, VcsKind};
use quick_xml::events::Event;
use quick_xml::Reader;

#[derive(Debug, Clone, Default)]
pub struct RepoMeta {
    pub remote_url: Option<String>,
    pub remote_host: Option<String>,
    pub remote_owner: Option<String>,
    pub default_branch: Option<String>,
    pub commit_count: Option<u64>,
    pub last_activity: Option<SystemTime>,
}

pub struct RepoAnalyzer<'a> {
    tools: &'a ToolSet,
}

impl<'a> RepoAnalyzer<'a> {
    pub fn new(tools: &'a ToolSet) -> Self {
        RepoAnalyzer { tools }
    }

    pub fn analyze(&self, repo: &RepoEntry) -> RepoMeta {
        match repo.vcs {
            VcsKind::Git => self.analyze_git(&repo.root),
            VcsKind::Hg => self.analyze_hg(&repo.root),
            VcsKind::Svn => self.analyze_svn(&repo.root),
        }
    }

    fn analyze_git(&self, root: &Path) -> RepoMeta {
        if !self.tools.has("git") {
            return RepoMeta::default();
        }
        let remote_url =
            run_vcs("git", root, &["remote", "get-url", "origin"]).map(|s| s.trim().to_string());
        let default_branch = run_vcs("git", root, &["symbolic-ref", "--short", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let commit_count = run_vcs("git", root, &["rev-list", "--count", "HEAD"])
            .and_then(|s| s.trim().parse::<u64>().ok());
        let last_activity = run_vcs("git", root, &["log", "-1", "--format=%ct"]).and_then(|s| {
            s.trim().parse::<i64>().ok().and_then(|secs| {
                SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs.max(0) as u64))
            })
        });
        RepoMeta {
            remote_host: remote_url.as_ref().and_then(|u| parse_host(u)),
            remote_owner: remote_url.as_ref().and_then(|u| parse_owner(u)),
            remote_url,
            default_branch,
            commit_count,
            last_activity,
        }
    }

    /// Mercurial: `hg paths default` for the remote URL, `hg branch` for the
    /// active branch, and the tip changeset for commit count and last
    /// activity. Mercurial revision numbers are dense local indexes, so the
    /// tip revision plus one equals the number of changesets in the clone.
    /// An empty clone has no tip (`rev` prints `-1`, which fails the u64
    /// parse) and degrades to empty metadata for those fields.
    fn analyze_hg(&self, root: &Path) -> RepoMeta {
        if !self.tools.has("hg") {
            return RepoMeta::default();
        }
        let remote_url = run_vcs("hg", root, &["paths", "default"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let default_branch = run_vcs("hg", root, &["branch"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let commit_count = run_vcs("hg", root, &["log", "-r", "tip", "--template", "{rev}"])
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|rev| rev + 1);
        let last_activity = run_vcs(
            "hg",
            root,
            &["log", "-r", "tip", "--template", "{date|hgdate}"],
        )
        .and_then(|s| parse_hgdate(&s));
        RepoMeta {
            remote_host: remote_url.as_ref().and_then(|u| parse_host(u)),
            remote_owner: remote_url.as_ref().and_then(|u| parse_owner(u)),
            remote_url,
            default_branch,
            commit_count,
            last_activity,
        }
    }

    /// Subversion: `svn info --xml` for the working-copy URL and the
    /// last-changed date. Subversion has no native default-branch concept
    /// and its revision numbers are repository-global, so the branch name is
    /// derived from the URL path when the layout is standard (trunk or
    /// `branches/<name>`), and commit count stays empty: counting log
    /// entries would require a full repository walk.
    fn analyze_svn(&self, root: &Path) -> RepoMeta {
        if !self.tools.has("svn") {
            return RepoMeta::default();
        }
        let xml = match run_vcs("svn", root, &["info", "--xml"]) {
            Some(x) => x,
            None => return RepoMeta::default(),
        };
        let info = parse_svn_info(&xml);
        let remote_url = info.url;
        let default_branch = remote_url.as_ref().and_then(|url| svn_branch_from_url(url));
        let last_activity = info.last_changed_date.and_then(|s| parse_iso_date(&s));
        RepoMeta {
            remote_host: remote_url.as_ref().and_then(|u| parse_host(u)),
            remote_owner: remote_url.as_ref().and_then(|u| parse_owner(u)),
            remote_url,
            default_branch,
            commit_count: None,
            last_activity,
        }
    }
}

/// Run one VCS CLI in the given repository root with a sanitized
/// environment, returning stdout on success. Adapter commands are
/// local-only (no network round trips): `hg paths`, `hg log`, and
/// `svn info` read the local store or working-copy metadata.
fn run_vcs(tool: &str, root: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(tool);
    match tool {
        "git" => {
            cmd.arg("-C").arg(root);
        }
        "hg" => {
            cmd.arg("-R").arg(root);
        }
        "svn" => {
            // Never prompt for credentials: auth failures degrade to empty
            // metadata instead of hanging an unattended scan.
            cmd.arg("--non-interactive");
        }
        _ => return None,
    }
    cmd.args(args);
    if tool == "svn" {
        cmd.arg(root);
    }
    cmd.env_clear();
    cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Extract the host from common remote URL forms:
/// `https://host/...`, `ssh://git@host/...`, `git@host:owner/repo.git`,
/// `svn+ssh://host/...`, `file:///path` (no host -> `None`).
fn parse_host(url: &str) -> Option<String> {
    let url = url.trim();
    if let Some(rest) = url.strip_prefix("git@") {
        return rest
            .split(':')
            .next()
            .filter(|h| !h.is_empty())
            .map(str::to_string);
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        let after_user = rest.split('@').last().unwrap_or(rest);
        return after_user
            .split(['/', ':'])
            .next()
            .filter(|h| !h.is_empty())
            .map(str::to_string);
    }
    if let Some(rest) = url.split("://").nth(1) {
        let after_user = rest.split('@').last().unwrap_or(rest);
        return after_user
            .split('/')
            .next()
            .filter(|h| !h.is_empty())
            .map(str::to_string);
    }
    None
}

/// Extract the owner/username from common remote URL forms: the first path
/// segment after the host. `git@github.com:owner/repo.git` -> `owner`,
/// `https://gitlab.com/owner/repo` -> `owner`. URLs without an owner
/// segment (e.g. `file:///srv/repo`) yield `None`.
fn parse_owner(url: &str) -> Option<String> {
    let url = url.trim();
    if url.starts_with("file://") {
        return None;
    }
    let path = if let Some(rest) = url.strip_prefix("git@") {
        rest.split_once(':').map(|(_, p)| p)
    } else if let Some((_, rest)) = url.split_once("://") {
        let after_user = rest.split('@').last().unwrap_or(rest);
        after_user.split_once('/').map(|(_, p)| p)
    } else if let Some((_, rest)) = url.split_once('@') {
        rest.split_once(':').map(|(_, p)| p)
    } else {
        None
    }?;
    let owner = path.split('/').find(|s| !s.is_empty())?;
    let owner = owner.strip_suffix(".git").unwrap_or(owner);
    if owner.is_empty() {
        None
    } else {
        Some(owner.to_string())
    }
}

/// Mercurial `{date|hgdate}` prints "<unix seconds> <utc offset minutes>";
/// only the seconds field is used.
fn parse_hgdate(s: &str) -> Option<SystemTime> {
    let secs = s.split_whitespace().next()?.parse::<i64>().ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs.max(0) as u64))
}

/// Parse the ISO-8601 timestamps svn emits, e.g.
/// `2024-03-15T10:30:00.000000Z`.
fn parse_iso_date(s: &str) -> Option<SystemTime> {
    let dt = chrono::DateTime::parse_from_rfc3339(s.trim()).ok()?;
    SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(dt.timestamp().max(0) as u64))
}

/// Subversion has no native branch concept; when a working-copy URL follows
/// the standard layout this reports the branch it is on: `trunk`, the name
/// under `branches/`, or `None` for anything else (including tags and bare
/// repository roots).
fn svn_branch_from_url(url: &str) -> Option<String> {
    let rest = url.split("://").nth(1)?;
    let mut segs = rest.split('/').skip(1).filter(|s| !s.is_empty());
    while let Some(seg) = segs.next() {
        match seg {
            "trunk" => return Some("trunk".to_string()),
            "branches" => return segs.next().map(str::to_string),
            _ => {}
        }
    }
    None
}

struct SvnInfo {
    url: Option<String>,
    last_changed_date: Option<String>,
}

/// Walk `svn info --xml` for the working-copy `<url>` and the `<commit>`
/// element's `<date>` (the last-changed date). A malformed document or a
/// missing field degrades to `None`, never an error.
fn parse_svn_info(xml: &str) -> SvnInfo {
    let mut reader = Reader::from_reader(xml.as_bytes());
    let mut buf = Vec::new();
    let mut in_commit = false;
    let mut in_url = false;
    let mut in_date = false;
    let mut out = SvnInfo {
        url: None,
        last_changed_date: None,
    };
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(el)) => match el.local_name().as_ref() {
                b"url" => in_url = true,
                b"commit" => in_commit = true,
                b"date" if in_commit => in_date = true,
                _ => {}
            },
            Ok(Event::Text(t)) => {
                let text = String::from_utf8_lossy(t.as_ref()).trim().to_string();
                if text.is_empty() {
                    buf.clear();
                    continue;
                }
                if in_url && out.url.is_none() {
                    out.url = Some(cap(&text, 2048));
                } else if in_date && out.last_changed_date.is_none() {
                    out.last_changed_date = Some(cap(&text, 64));
                }
            }
            Ok(Event::End(el)) => match el.local_name().as_ref() {
                b"url" => in_url = false,
                b"commit" => in_commit = false,
                b"date" => in_date = false,
                _ => {}
            },
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        buf.clear();
    }
    out
}

fn cap(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Datelike;

    #[test]
    fn host_parsing() {
        assert_eq!(
            parse_host("https://github.com/foo/bar.git"),
            Some("github.com".into())
        );
        assert_eq!(
            parse_host("git@github.com:foo/bar.git"),
            Some("github.com".into())
        );
        assert_eq!(
            parse_host("ssh://git@gitlab.com/x/y"),
            Some("gitlab.com".into())
        );
        assert_eq!(
            parse_host("https://user:pass@example.com/x"),
            Some("example.com".into())
        );
        assert_eq!(parse_host("file:///srv/repo"), None);
        assert_eq!(parse_host("/srv/repo"), None);
    }

    #[test]
    fn owner_parsing() {
        assert_eq!(
            parse_owner("https://github.com/foo/bar.git"),
            Some("foo".into())
        );
        assert_eq!(
            parse_owner("git@github.com:foo/bar.git"),
            Some("foo".into())
        );
        assert_eq!(parse_owner("ssh://git@gitlab.com/x/y"), Some("x".into()));
        assert_eq!(
            parse_owner("https://user:pass@example.com/x"),
            Some("x".into())
        );
        assert_eq!(parse_owner("file:///srv/repo"), None);
        assert_eq!(parse_owner("/srv/repo"), None);
    }

    #[test]
    fn svn_info_xml_parsing() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<info>
<entry kind="dir" path="." revision="120">
<url>https://svn.example.com/svn/proj/trunk</url>
<relative-url>^/proj/trunk</relative-url>
<repository>
<root>https://svn.example.com/svn/proj</root>
<uuid>abcd-efgh</uuid>
</repository>
<wc-info>
<wcroot-abspath>/srv/wc</wcroot-abspath>
<schedule>normal</schedule>
<depth>infinity</depth>
</wc-info>
<commit revision="120">
<author>alice</author>
<date>2024-03-15T10:30:00.000000Z</date>
</commit>
</entry>
</info>"#;
        let info = parse_svn_info(xml);
        assert_eq!(
            info.url.as_deref(),
            Some("https://svn.example.com/svn/proj/trunk")
        );
        let date = info.last_changed_date.as_deref().unwrap();
        assert_eq!(date, "2024-03-15T10:30:00.000000Z");
        let t = parse_iso_date(date).unwrap();
        let d = chrono::DateTime::<chrono::Utc>::from(t);
        assert_eq!((d.year(), d.month(), d.day()), (2024, 3, 15));
    }

    #[test]
    fn svn_info_xml_missing_fields() {
        let info = parse_svn_info("<info><entry/></info>");
        assert_eq!(info.url, None);
        assert_eq!(info.last_changed_date, None);
    }

    #[test]
    fn svn_branch_derivation() {
        assert_eq!(
            svn_branch_from_url("https://svn.example.com/repo/trunk"),
            Some("trunk".into())
        );
        assert_eq!(
            svn_branch_from_url("https://svn.example.com/repo/branches/feature-x"),
            Some("feature-x".into())
        );
        assert_eq!(svn_branch_from_url("https://svn.example.com/repo"), None);
        assert_eq!(
            svn_branch_from_url("https://svn.example.com/repo/tags/v1.0"),
            None
        );
    }

    #[test]
    fn hgdate_parsing() {
        let t = parse_hgdate("1788797647 -32400").unwrap();
        let d = chrono::DateTime::<chrono::Utc>::from(t);
        assert_eq!(d.timestamp(), 1788797647);
        assert!(parse_hgdate("").is_none());
        assert!(parse_hgdate("garbage").is_none());
    }

    #[test]
    fn hg_adapter_integration() {
        // hg is optional tooling: skip the live check when it is absent.
        if Command::new("hg").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("homelinter-hg-{}", std::process::id()));
        let root = dir.join("repo");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&root).unwrap();
        let init = Command::new("hg")
            .arg("init")
            .arg(&root)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !init {
            return;
        }
        std::fs::write(root.join("a.txt"), "hi\n").unwrap();
        run_vcs("hg", &root, &["add", root.join("a.txt").to_str().unwrap()]);
        run_vcs("hg", &root, &["commit", "-m", "one", "-u", "test"]);
        std::fs::write(root.join("b.txt"), "bye\n").unwrap();
        run_vcs("hg", &root, &["add", root.join("b.txt").to_str().unwrap()]);
        run_vcs("hg", &root, &["commit", "-m", "two", "-u", "test"]);

        let tools = ToolSet::detect();
        let analyzer = RepoAnalyzer::new(&tools);
        let meta = analyzer.analyze(&RepoEntry {
            root,
            vcs: VcsKind::Hg,
            marker_is_file: false,
        });
        assert_eq!(meta.default_branch.as_deref(), Some("default"));
        assert_eq!(meta.commit_count, Some(2));
        assert!(meta.last_activity.is_some());
        // No default path configured: the remote stays empty, not an error.
        assert_eq!(meta.remote_url, None);
        assert_eq!(meta.remote_host, None);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
