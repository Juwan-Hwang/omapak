use anyhow::Result;
use omapak_core::{
    FileStat, LinterRun, LinterStatus, ManifestInfo, SourceStats, StaticAdvisory, StaticReport,
};
use std::path::Path;
use std::process::Command;
use walkdir::WalkDir;

pub fn run(app_dir: &Path, source_dir: Option<&Path>) -> Result<StaticReport> {
    let mut report = StaticReport::default();

    let manifest_path = omapak_core::find_manifest(app_dir);
    let manifest: Option<ManifestInfo> = manifest_path.as_ref().and_then(|p| {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|t| omapak_core::parse_manifest(&t).ok())
    });

    if let Some(m) = &manifest {
        for arg in omapak_core::risky_finish_args(m) {
            report.advisories.push(StaticAdvisory {
                kind: "finish-args".into(),
                detail: format!("{arg} punches a hole in the sandbox — confirm it's justified"),
            });
        }
        if m.modules.is_empty() {
            report.advisories.push(StaticAdvisory {
                kind: "manifest".into(),
                detail: "no modules — nothing will be built".into(),
            });
        }
        let unpinned: Vec<String> = m
            .modules
            .iter()
            .flat_map(|modu| {
                modu.sources.iter().filter_map(|s| {
                    (s.url.is_some() && s.pinned.is_none()).then(|| modu.name.clone())
                })
            })
            .collect();
        if !unpinned.is_empty() {
            report.advisories.push(StaticAdvisory {
                kind: "manifest".into(),
                detail: format!("unpinned upstream sources: {}", unpinned.join(", ")),
            });
        }
        // The desktop entry, the appstream file and the icon are the
        // submission's own; fetched from a URL they make the build depend on
        // a repo omapak does not control. LocalPDF_Studio's asset repo was
        // deleted and took every main build with it until they were vendored.
        let remote_assets = omapak_core::remote_packaging_assets(m);
        if !remote_assets.is_empty() {
            let urls: Vec<&str> = remote_assets.iter().map(|(_, u)| u.as_str()).collect();
            report.advisories.push(StaticAdvisory {
                kind: "packaging-assets".into(),
                detail: format!(
                    "desktop entry, appstream metadata or icon fetched from a \
                     repo omapak does not control — ship {} in the app \
                     directory instead; a moved branch or a deleted repo \
                     takes the build down with it",
                    urls.join(", ")
                ),
            });
        }
    }

    report.manifest = manifest;
    report.metadata_present = app_dir.join("metadata.yml").is_file();
    // TUI apps (metadata.yml tags) may ship no artwork: the build injects
    // the omapak catchall icon at export time. GUI apps keep the
    // requirement — an icon is a fair ask for anything graphical.
    report.tui = omapak_core::load_metadata(app_dir)
        .map(|m| {
            m.tags
                .iter()
                .any(|t| matches!(t.to_lowercase().as_str(), "tui" | "terminal"))
        })
        .unwrap_or(false);
    if report.tui {
        let installs_icon = manifest_path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .is_some_and(|text| text.contains("icons/hicolor"));
        if !installs_icon {
            report.advisories.push(StaticAdvisory {
                kind: "icon".into(),
                detail: "TUI app with no icon install detected — omapak's \
                         catchall TUI icon is injected automatically at build time"
                    .into(),
            });
        }
    }
    if let Some(dup) = find_duplicate(app_dir) {
        report.advisories.push(StaticAdvisory {
            kind: "duplicate".into(),
            detail: format!("already packaged as {dup} — updates belong in apps/{dup}"),
        });
        report.duplicate_of = Some(dup);
    }
    if !report.metadata_present {
        // Not gated (the build is the one gate), but without metadata.yml
        // the site catalog generator skips the app entirely — it merged
        // and published while being invisible on omapak.org.
        report.advisories.push(StaticAdvisory {
            kind: "metadata".into(),
            detail: "no metadata.yml — the app will not appear in the site catalog".into(),
        });
    } else if has_unparsable_metadata(app_dir) {
        // The quiet case: the file is there, so `metadata_present` is true and
        // nothing gates; but the judge reads it through `load_metadata`, which
        // returns None on a YAML error — so the app was scored with no summary,
        // description, license or tags at all, and the report says nothing
        // about it. The catalog generator drops the app in the same situation.
        report.advisories.push(StaticAdvisory {
            kind: "metadata".into(),
            detail: "metadata.yml does not parse — the judge sees no summary, \
                     description, license or tags, and the site catalog skips \
                     the app"
                .into(),
        });
    }
    let appstream = find_appstream(app_dir);
    report.appstream_present = appstream.is_some();

    if let Some(mpath) = &manifest_path {
        report.linters.push(lint_manifest(mpath));
    }
    if let Some(apath) = &appstream {
        report.linters.push(lint_appstream(apath));
    }

    report.source_stats = Some(collect_stats(app_dir, source_dir));
    Ok(report)
}

/// True when `metadata.yml` is present but does not parse. Kept as its own
/// function so the quiet case has a name: a file that exists, passes every
/// gate, and silently delivers nothing to the judge.
fn has_unparsable_metadata(app_dir: &Path) -> bool {
    app_dir.join("metadata.yml").is_file() && omapak_core::load_metadata(app_dir).is_none()
}

fn find_appstream(app_dir: &Path) -> Option<std::path::PathBuf> {
    WalkDir::new(app_dir)
        .max_depth(2)
        .into_iter()
        .flatten()
        .find(|e| {
            let n = e.file_name().to_string_lossy();
            e.path().is_file() && (n.ends_with(".metainfo.xml") || n.ends_with(".appdata.xml"))
        })
        .map(|e| e.path().to_path_buf())
}

/// True when the app's appstream metadata declares at least one screenshot.
/// The judge prompt tells the model whether screenshots exist — a hardcoded
/// "no" asked the wrong question for every app that ships any — and the
/// ui_ux rubric scores the metadata path differently when they do.
pub fn appstream_has_screenshots(app_dir: &Path) -> bool {
    let Some(path) = find_appstream(app_dir) else {
        return false;
    };
    let Ok(xml) = std::fs::read_to_string(&path) else {
        return false;
    };
    xml.contains("<screenshots") && xml.contains("<image")
}

/// Canonical identity of an app's upstream project: the submitter's
/// declared source_repo, normalized. Deliberately NOT derived from
/// manifest source URLs — those include fonts, vendored tarballs and
/// shared libraries that routinely repeat across unrelated apps.
fn upstream_key(app_dir: &Path) -> Option<String> {
    let meta = omapak_core::load_metadata(app_dir)?;
    normalize_repo(&meta.source_repo)
}

fn normalize_repo(url: &str) -> Option<String> {
    let u = url.trim().trim_end_matches('/');
    let u = u.strip_suffix(".git").unwrap_or(u);
    let u = if let Some(rest) = u.strip_prefix("git@github.com:") {
        format!("github.com/{rest}")
    } else if let Some(rest) = u.strip_prefix("https://") {
        rest.to_string()
    } else if let Some(rest) = u.strip_prefix("http://") {
        rest.to_string()
    } else {
        u.to_string()
    };
    let mut parts = u.split('/');
    let host = parts.next()?.to_lowercase();
    if !matches!(
        host.as_str(),
        "github.com" | "gitlab.com" | "codeberg.org" | "git.sr.ht" | "sourceforge.net"
    ) {
        return Some(u.to_lowercase());
    }
    let owner = parts.next()?.to_lowercase();
    let repo = parts.next()?.to_lowercase();
    Some(format!("{host}/{owner}/{repo}"))
}

fn find_duplicate(app_dir: &Path) -> Option<String> {
    let key = upstream_key(app_dir)?;
    let siblings = app_dir.parent()?;
    for entry in std::fs::read_dir(siblings).ok()?.flatten() {
        let path = entry.path();
        if !path.is_dir() || path == app_dir {
            continue;
        }
        if upstream_key(&path).is_some_and(|other| other == key) {
            return Some(entry.file_name().to_string_lossy().into_owned());
        }
    }
    None
}

fn lint_manifest(path: &Path) -> LinterRun {
    run_linter(
        "flatpak-builder-lint",
        &["manifest".to_string(), path.to_string_lossy().into_owned()],
    )
}

fn lint_appstream(path: &Path) -> LinterRun {
    // flatpak-builder-lint 3.0.0 (what the workflow installs from git)
    // renamed this subcommand from `flatpakmetainfo`; older installs only
    // know the old name. An unknown subcommand surfaces as an argparse
    // "invalid choice" failure, not as NotFound — so treat that as "try
    // the other name" rather than a lint result.
    for subcommand in ["appstream", "flatpakmetainfo"] {
        let run = run_linter(
            "flatpak-builder-lint",
            &[subcommand.to_string(), path.to_string_lossy().into_owned()],
        );
        let invalid_subcommand = run
            .findings
            .iter()
            .any(|f| f.contains("invalid choice") && f.contains(subcommand));
        if run.status == LinterStatus::NotFound || invalid_subcommand {
            continue;
        }
        return run;
    }
    run_linter(
        "appstreamcli",
        &[
            "validate".to_string(),
            "--no-net".to_string(),
            path.to_string_lossy().into_owned(),
        ],
    )
}

fn run_linter(tool: &str, args: &[String]) -> LinterRun {
    let Ok(out) = Command::new(tool).args(args).output() else {
        return LinterRun {
            tool: tool.into(),
            status: LinterStatus::NotFound,
            findings: vec![],
        };
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        LinterRun {
            tool: tool.into(),
            status: LinterStatus::Pass,
            findings: vec![],
        }
    } else {
        let mut findings: Vec<String> = stdout
            .lines()
            .chain(stderr.lines())
            .filter(|l| {
                !l.trim().is_empty()
                    && !l.contains("Traceback")
                    && !l.contains("File \"")
                    && !l.contains("import main")
                    && !l.contains("ModuleNotFoundError")
            })
            .map(String::from)
            .collect();
        findings.truncate(20);
        LinterRun {
            tool: tool.into(),
            status: LinterStatus::Failed,
            findings,
        }
    }
}

fn collect_stats(app_dir: &Path, source_dir: Option<&Path>) -> SourceStats {
    let mut files = 0u64;
    let mut bytes = 0u64;
    let mut all: Vec<FileStat> = Vec::new();
    for dir in [Some(app_dir), source_dir].into_iter().flatten() {
        for entry in WalkDir::new(dir)
            .into_iter()
            .filter_entry(|e| {
                let n = e.file_name().to_string_lossy();
                n != ".git" && n != "node_modules" && n != "target"
            })
            .flatten()
        {
            if entry.file_type().is_file() {
                let Ok(meta) = entry.metadata() else { continue };
                files += 1;
                bytes += meta.len();
                all.push(FileStat {
                    path: entry
                        .path()
                        .strip_prefix(dir.parent().unwrap_or(dir))
                        .unwrap_or(entry.path())
                        .to_string_lossy()
                        .into_owned(),
                    bytes: meta.len(),
                });
            }
        }
    }
    if files == 0 {
        return SourceStats {
            files: 0,
            bytes: 0,
            largest: vec![],
            commit_count: None,
            last_commit_date: None,
        };
    }
    all.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    all.truncate(5);
    let (commit_count, last_commit_date) = git_signals(source_dir.unwrap_or(app_dir));
    SourceStats {
        files,
        bytes,
        largest: all,
        commit_count,
        last_commit_date,
    }
}

fn git_signals(dir: &Path) -> (Option<u64>, Option<String>) {
    let count = Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "rev-list", "--count", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok());
    let last = Command::new("git")
        .args(["-C", &dir.to_string_lossy(), "log", "-1", "--format=%cI"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    (count, last)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_screenshots_from_appstream_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("io.example.Alpha.metainfo.xml");
        let write = |body: &str| std::fs::write(&file, body).unwrap();

        write(
            "<component><screenshots><image>https://example.com/1.png</image></screenshots></component>",
        );
        assert!(appstream_has_screenshots(dir.path()));

        // An empty element is not evidence of a screenshot.
        write("<component><screenshots/></component>");
        assert!(!appstream_has_screenshots(dir.path()));

        write("<component><description><p>no shots here</p></description></component>");
        assert!(!appstream_has_screenshots(dir.path()));

        // No appstream metadata at all.
        std::fs::remove_file(&file).unwrap();
        assert!(!appstream_has_screenshots(dir.path()));
    }

    #[test]
    fn normalizes_repo_identity_across_spellings() {
        assert_eq!(
            normalize_repo("https://github.com/Kesomannen/gale"),
            normalize_repo("git@github.com:Kesomannen/gale.git")
        );
        assert_eq!(
            normalize_repo("https://github.com/opengeos/GeoLibre/"),
            normalize_repo("https://github.com/opengeos/geolibre")
        );
        // release/download URLs collapse to owner/repo
        assert_eq!(
            normalize_repo("https://github.com/debba/tabularis/releases/download/v0.24.0/x"),
            normalize_repo("https://github.com/debba/tabularis")
        );
        // non-forge hosts compare as whole normalized strings
        assert_eq!(
            normalize_repo("https://Example.com/Project"),
            normalize_repo("example.com/Project/")
        );
    }

    #[test]
    fn flags_duplicate_sibling_by_source_repo() {
        let apps = tempfile::tempdir().unwrap();
        let a = apps.path().join("io.example.Alpha");
        let b = apps.path().join("com.example.AlphaClone");
        for dir in [&a, &b] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join("metadata.yml"),
                "submitter: x\nsource_repo: https://github.com/Someone/alpha\nsummary: s\n",
            )
            .unwrap();
        }
        assert_eq!(
            find_duplicate(&a).as_deref(),
            Some("com.example.AlphaClone")
        );
        assert_eq!(find_duplicate(&b).as_deref(), Some("io.example.Alpha"));

        // a different project is not a duplicate
        let c = apps.path().join("io.example.Beta");
        std::fs::create_dir_all(&c).unwrap();
        std::fs::write(
            c.join("metadata.yml"),
            "submitter: x\nsource_repo: https://github.com/Someone/beta\nsummary: s\n",
        )
        .unwrap();
        assert_eq!(find_duplicate(&c), None);
    }

    /// The shape that hit a real submission (io.github.alexwest1981.NovaCut,
    /// 2026-09-18): a folded description whose continuation lines lost their
    /// indentation, so they sit at column 0 and the YAML does not parse. The
    /// app was scored with no summary, description, license or tags, and
    /// nothing anywhere said so.
    #[test]
    fn flags_metadata_that_exists_but_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("io.example.Broken");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("metadata.yml"),
            "submitter: x\nsource_repo: https://github.com/Someone/broken\nsummary: s\nlicense: MIT\n\
             tags: [utility]\ndescription: >-\n  A folded description.\nAnd a line that lost its indent.\n",
        )
        .unwrap();
        assert!(has_unparsable_metadata(&app));

        // …and the same file with the indentation intact is not flagged.
        std::fs::write(
            app.join("metadata.yml"),
            "submitter: x\nsource_repo: https://github.com/Someone/broken\nsummary: s\nlicense: MIT\n\
             tags: [utility]\ndescription: >-\n  A folded description.\n  And a line that kept it.\n",
        )
        .unwrap();
        assert!(!has_unparsable_metadata(&app));

        // No file at all is the other advisory's business, not this one's.
        std::fs::remove_file(app.join("metadata.yml")).unwrap();
        assert!(!has_unparsable_metadata(&app));
    }
}
