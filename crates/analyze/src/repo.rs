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

#[derive(Debug, Clone, Default)]
pub struct RepoMeta {
    pub remote_url: Option<String>,
    pub remote_host: Option<String>,
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
            VcsKind::Hg => self.analyze_via("hg", &repo.root),
            VcsKind::Svn => self.analyze_via("svn", &repo.root),
        }
    }

    fn analyze_via(&self, _tool: &str, _root: &Path) -> RepoMeta {
        // hg/svn adapters are scaffolds: metadata stays empty until those
        // adapters are implemented, and analysis never fails because of it.
        RepoMeta::default()
    }

    fn analyze_git(&self, root: &Path) -> RepoMeta {
        if !self.tools.has("git") {
            return RepoMeta::default();
        }
        let mut meta = RepoMeta::default();
        meta.remote_url =
            run_git(root, &["remote", "get-url", "origin"]).map(|s| s.trim().to_string());
        if let Some(url) = &meta.remote_url {
            meta.remote_host = parse_host(url);
        }
        meta.default_branch = run_git(root, &["symbolic-ref", "--short", "HEAD"])
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        meta.commit_count = run_git(root, &["rev-list", "--count", "HEAD"])
            .and_then(|s| s.trim().parse::<u64>().ok());
        meta.last_activity = run_git(root, &["log", "-1", "--format=%ct"]).and_then(|s| {
            s.trim().parse::<i64>().ok().and_then(|secs| {
                SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(secs as u64))
            })
        });
        meta
    }
}

fn run_git(root: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(root).args(args);
    cmd.env_clear();
    cmd.env("PATH", std::env::var_os("PATH").unwrap_or_default());
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Extract the host from common remote URL forms:
/// `https://host/...`, `ssh://git@host/...`, `git@host:owner/repo.git`.
fn parse_host(url: &str) -> Option<String> {
    let url = url.trim();
    if url.starts_with("git@") {
        let rest = &url[4..];
        return rest.split(':').next().map(str::to_string);
    }
    if let Some(rest) = url.strip_prefix("ssh://") {
        let after_user = rest.split('@').last().unwrap_or(rest);
        return after_user.split(['/', ':']).next().map(str::to_string);
    }
    if let Some(rest) = url.split("://").nth(1) {
        let after_user = rest.split('@').last().unwrap_or(rest);
        return after_user.split('/').next().map(str::to_string);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
