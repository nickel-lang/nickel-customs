use clap::Parser;
use gitpatch::Patch;
use miette::{IntoDiagnostic, bail};
use nickel_lang_package::{
    config::Config,
    index::{Package, PackageIndex, PreciseId, Shared},
};
use octocrab::Octocrab;
use tempfile::tempdir;

use crate::package::{IntoDiagnostic as _, ManifestChecks};

mod package;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    owner: String,

    #[arg(long)]
    repo: String,

    #[arg(long)]
    reporter: String,

    #[arg(long)]
    pr: u64,

    #[arg(long)]
    token: Option<String>,
}

/// Someone submitted a package to us. Do we think it's "their" package?
pub struct Permission {
    /// The user that submitted the package.
    user: String,
    /// The organization that owns the package.
    org: String,
    /// The repo containing the package.
    repo: String,
    /// Do we think they're allowed?
    is_allowed: bool,
}

impl Permission {
    async fn check(
        client: &Octocrab,
        user: String,
        org: String,
        repo: String,
    ) -> miette::Result<Self> {
        // It might make sense to check `client.repos(..).is_collaborator`, but that requires
        // authentication (beyond the default github CI token) and we'd prefer not to rely on it.
        let is_allowed = user == org
            || client
                .orgs(&org)
                .check_membership(&user)
                .await
                .into_diagnostic()?;
        Ok(Self {
            is_allowed,
            user,
            org,
            repo,
        })
    }
}

enum Report {
    InvalidDiff(package::Error),
    PackageReports(Vec<Box<dyn ReportItem>>),
}

impl Report {
    fn is_good(&self) -> bool {
        match self {
            Report::InvalidDiff(_) => false,
            Report::PackageReports(package_reports) => package_reports.iter().all(|r| r.is_good()),
        }
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Report::InvalidDiff(e) => writeln!(f, "❌ invalid index changes: {e}"),
            Report::PackageReports(package_reports) => {
                for r in package_reports {
                    r.format_with_indent(f, " - ")?;
                }
                Ok(())
            }
        }
    }
}

trait ReportItem {
    fn is_good(&self) -> bool;
    fn format_with_indent(&self, f: &mut std::fmt::Formatter, indent: &str) -> std::fmt::Result;
}

struct PackageReport {
    pkg: Package,
    permission: Permission,
    status: PackageStatus,
}

impl PackageReport {
    async fn new(
        client: &Octocrab,
        user: &str,
        index: &PackageIndex<Shared>,
        pkg: Package,
    ) -> miette::Result<Self> {
        let PreciseId::Github {
            org, name, path, ..
        } = &pkg.id;
        let permission =
            Permission::check(client, user.to_owned(), org.clone(), name.clone()).await?;

        let temp_dir = tempdir().into_diagnostic()?;
        let status = if let Err(e) = package::fetch(&pkg, temp_dir.path()) {
            PackageStatus::FetchFailed(e.to_string())
        } else {
            let path = temp_dir.path().join(path);
            match package::check_manifest(&pkg, &path, index) {
                Ok(c) => PackageStatus::Manifest(Box::new(c)),
                Err(e) => PackageStatus::EvalFailed(e.to_string()),
            }
        };

        Ok(Self {
            pkg,
            permission,
            status,
        })
    }
}

impl ReportItem for PackageReport {
    fn is_good(&self) -> bool {
        self.permission.is_allowed
            && match &self.status {
                PackageStatus::FetchFailed(_) | PackageStatus::EvalFailed(_) => false,
                PackageStatus::Manifest(manifest_checks) => manifest_checks.is_good(),
            }
    }

    fn format_with_indent(&self, f: &mut std::fmt::Formatter, indent: &str) -> std::fmt::Result {
        let PreciseId::Github {
            org, name, path, ..
        } = &self.pkg.id;
        let PreciseId::Github { org, name, .. } = &self.pkg.id;
        let perm = &self.permission;
        let indent_spaces = " ".repeat(indent.len());
        writeln!(
            f,
            "{}package {org}/{name}/{path}, version {}",
            indent, self.pkg.version
        )?;
        if perm.is_allowed {
            writeln!(
                f,
                "{indent_spaces}*✅ this PR is by {}, a collaborator on {}/{}",
                perm.user, perm.org, perm.repo
            )?;
        } else {
            writeln!(
                f,
                "{indent_spaces}*❌ this PR is by {}, who is not a public member of {}",
                perm.user, perm.org
            )?;
        };

        if let PackageStatus::FetchFailed(e) = &self.status {
            writeln!(f, "{indent_spaces}*❌ failed to fetch package: {e}",)?;
        } else {
            writeln!(f, "{indent_spaces}*✅ fetched package",)?;

            if let PackageStatus::EvalFailed(e) = &self.status {
                writeln!(f, "{indent_spaces}*❌ failed to evaluate manifest: {e}",)?;
            } else {
                writeln!(f, "{indent_spaces}*✅ evaluated manifest",)?;

                let PackageStatus::Manifest(checks) = &self.status else {
                    unreachable!()
                };
                checks.format(f, &format!("{indent_spaces}* "))?;
            }
        }

        Ok(())
    }
}

/// A diagnostic for showing that an unexpected path was modified.
struct PathReport {
    is_good: bool,
    path: String,
}

impl ReportItem for PathReport {
    fn is_good(&self) -> bool {
        self.is_good
    }

    fn format_with_indent(&self, f: &mut std::fmt::Formatter, indent: &str) -> std::fmt::Result {
        let sym = if self.is_good { "⚠️" } else { "❌" };
        let path = &self.path;
        writeln!(f, "{indent}{sym} this PR modifies {path}")
    }
}

enum PackageStatus {
    FetchFailed(String),
    EvalFailed(String),
    Manifest(Box<ManifestChecks>),
}

/// Checks the paths of modified files. Removes the ones that aren't modifying
/// packages and adds diagnostic messages for them.
fn check_diff_paths(patches: &mut Vec<Patch>, reports: &mut Vec<Box<dyn ReportItem>>) {
    patches.retain(|patch| {
        let path = &patch.new.path;
        let mut parts = path.split('/');
        if parts.next() != Some("b") {
            reports.push(Box::new(PathReport {
                is_good: false,
                path: patch.new.path.clone().into_owned(),
            }));
            return false;
        }

        // Trim off the "b/" for a better error message.
        let path_without_prefix = &patch.new.path[2..];
        let dir = parts.next();
        if dir != Some("github") {
            // Modifications to our CI are not necessarily bad. Any other path
            // is definitely a mistake.
            let is_good = dir == Some(".github");
            reports.push(Box::new(PathReport {
                is_good,
                path: path_without_prefix.to_owned(),
            }));
            return false;
        }
        true
    });
}

async fn make_report(diff: &str, client: &Octocrab, user: &str) -> miette::Result<Report> {
    let mut reports = Vec::new();
    let mut patches = match Patch::from_multiple(diff) {
        Ok(p) => p,
        Err(e) => return Ok(Report::InvalidDiff(e.into())),
    };
    check_diff_paths(&mut patches, &mut reports);
    let pkgs = match package::changed_packages(patches) {
        Ok(p) => p,
        Err(e) => return Ok(Report::InvalidDiff(e)),
    };

    let index = PackageIndex::refreshed(Config::new().into_diag()?).into_diag()?;
    for pkg in pkgs {
        reports.push(Box::new(
            PackageReport::new(client, user, &index, pkg).await?,
        ));
    }

    Ok(Report::PackageReports(reports))
}

#[tokio::main]
async fn main() -> miette::Result<()> {
    let args = Args::parse();
    let mut builder = Octocrab::builder();

    if let Some(tok) = args.token {
        builder = builder.personal_token(tok);
    }
    let client = builder.build().into_diagnostic()?;
    let pr_handler = client.pulls(&args.owner, &args.repo);
    let diff = pr_handler.get_diff(args.pr).await.into_diagnostic()?;
    let report = make_report(&diff, &client, &args.reporter).await?;
    println!("{report}");

    client
        .issues(&args.owner, &args.repo)
        .create_comment(args.pr, report.to_string())
        .await
        .into_diagnostic()?;

    if report.is_good() {
        Ok(())
    } else {
        bail!("Failing report")
    }
}
