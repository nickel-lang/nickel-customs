use std::path::Path;

use gitpatch::Patch;
use miette::{IntoDiagnostic as _, bail};
use nickel_lang_core::error::report::report_as_str;
use nickel_lang_git::{Spec, Target};
use nickel_lang_package::{
    IndexDependency, ManifestFile,
    index::{Id, Package, PackageIndex, PreciseId, Shared, serialize::PackageFormat},
    manifest::MANIFEST_NAME,
    version::SemVer,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("failed to parse diff: {0}")]
    Patch(String),
    #[error("expected new files to start with \"b/github\", got \"{0}\"")]
    BadPrefix(String),
    #[error("missing org, got \"{0}\"")]
    MissingOrg(String),
    #[error("missing repo, got \"{0}\"")]
    MissingRepo(String),
    #[error("you can't delete a line: \"{0}\"")]
    Deletion(String),
    #[error("invalid package spec: {0}")]
    Deserialize(#[from] serde_json::Error),
    #[error("org/name mismatch: path was \"{path}\", package was \"{package}\"")]
    OrgNameMismatch { path: String, package: String },
    #[error("path too deep: expected three components, got \"{path}\"")]
    PathToDeep { path: String },
}

impl<'a> From<gitpatch::ParseError<'a>> for Error {
    fn from(e: gitpatch::ParseError<'a>) -> Self {
        Error::Patch(e.to_string())
    }
}

pub fn changed_packages(diff: &str) -> Result<Vec<Package>, Error> {
    let patches = Patch::from_multiple(diff)?;
    let mut ret = Vec::new();
    for patch in patches {
        let path = patch.new.path;
        let mut parts = path.split('/');
        if parts.next() != Some("b") {
            return Err(Error::BadPrefix(path.into_owned()));
        }
        if parts.next() != Some("github") {
            return Err(Error::BadPrefix(path.into_owned()));
        }
        let Some(path_org) = parts.next() else {
            return Err(Error::MissingOrg(path.into_owned()));
        };
        let Some(path_name) = parts.next() else {
            return Err(Error::MissingRepo(path.into_owned()));
        };
        if parts.next().is_some() {
            return Err(Error::PathToDeep {
                path: path.into_owned(),
            });
        }

        for line in patch.hunks.iter().flat_map(|h| h.lines.iter()) {
            match line {
                gitpatch::Line::Add(line) => {
                    let package: PackageFormat = serde_json::from_str(line)?;
                    let package = Package::from(package);
                    let id = Id::from(package.id.clone());
                    let package_path = format!("github/{path_org}/{path_name}");
                    if id.path().to_str() != Some(package_path.as_ref()) {
                        return Err(Error::OrgNameMismatch {
                            path: package_path,
                            package: id.path().display().to_string(),
                        });
                    }
                    ret.push(package);
                }
                gitpatch::Line::Remove(line) => {
                    return Err(Error::Deletion((*line).to_owned()));
                }
                gitpatch::Line::Context(_) => {}
            }
        }
    }
    Ok(ret)
}

/// Fetches a package.
///
/// This uses `nickel_lang_git`, with essentially the same code as nickel's package manager.
/// In particular, this should catch any portability issues like illegal windows filenames.
pub fn fetch(pkg: &Package, path: &Path) -> miette::Result<()> {
    let PreciseId::Github {
        org,
        name,
        commit,
        path: _,
    } = &pkg.id;
    let url = format!("https://github.com/{org}/{name}.git");
    let spec = Spec {
        url: url.try_into().into_diagnostic()?,
        target: Target::Commit(*commit),
    };
    nickel_lang_git::fetch(&spec, path).into_diagnostic()?;
    Ok(())
}

// TODO: license checks, sanity checks for minimal_nickel_version. Anything else?
// TODO: handle failure to fetch here also
pub struct ManifestChecks {
    package_version: SemVer,
    manifest_version: SemVer,
    dependencies: Vec<DependencyChecks>,
}

impl ManifestChecks {
    pub fn is_good(&self) -> bool {
        self.package_version == self.manifest_version
            && self.dependencies.iter().all(|d| d.is_good())
    }

    pub fn format(&self, f: &mut std::fmt::Formatter, indent: &str) -> std::fmt::Result {
        if self.package_version == self.manifest_version {
            writeln!(f, "{indent}✅ manifest version matches")?;
        } else {
            writeln!(
                f,
                "{indent}❌ index version {} doesn't match manifest version {}",
                self.package_version, self.manifest_version
            )?;
        }

        if self.dependencies.is_empty() {
            writeln!(f, "{indent}✅ no dependencies to check")?;
        } else {
            writeln!(f, "{indent}checking dependencies:")?;
            let indent = &format!("{indent}- ");
            for dep in &self.dependencies {
                dep.format(f, indent)?;
            }
        }

        Ok(())
    }
}

pub struct DependencyChecks {
    dep: IndexDependency,
    known_versions: Vec<SemVer>,
    has_match: bool,
}

impl DependencyChecks {
    pub fn is_good(&self) -> bool {
        self.has_match
    }

    pub fn format(&self, f: &mut std::fmt::Formatter, indent: &str) -> std::fmt::Result {
        if self.has_match {
            writeln!(f, "{indent}✅ {} {}", self.dep.id, self.dep.version)?;
        } else if self.known_versions.is_empty() {
            writeln!(f, "{indent}❌ {} doesn't exist in the index", self.dep.id)?;
        } else {
            let known_versions = self
                .known_versions
                .iter()
                .fold(String::new(), |mut acc, v| {
                    acc.push_str(&format!(", {v}"));
                    acc
                });
            writeln!(
                f,
                "{indent}❌ {} {} doesn't match any versions: known versions are {}",
                self.dep.id, self.dep.version, known_versions
            )?;
        }
        Ok(())
    }
}

// This error handling is inconvenient because nickel's errors aren't Send + Sync.
// Maybe they should be? Or there should be convenience wrappers?
pub trait IntoDiagnostic<T> {
    fn into_diag(self) -> Result<T, miette::Error>;
}

impl<T> IntoDiagnostic<T> for Result<T, nickel_lang_package::error::Error> {
    fn into_diag(self) -> Result<T, miette::Error> {
        match self {
            Ok(m) => Ok(m),
            Err(nickel_lang_package::error::Error::ManifestEval {
                mut files, error, ..
            }) => {
                bail!(report_as_str(&mut files, *error, Default::default()))
            }
            Err(e) => bail!(e.to_string()),
        }
    }
}

/// Runs sanity checks against a package manifest.
pub fn check_manifest(
    pkg: &Package,
    path: &Path,
    index: &PackageIndex<Shared>,
) -> miette::Result<ManifestChecks> {
    let mut path = path.to_owned();
    path.push(MANIFEST_NAME);

    // TODO: report manifest eval errors better
    // FIXME: use the subdir here if necessary
    let manifest = ManifestFile::from_path(&path).into_diag()?;

    let mut dependencies = Vec::new();
    for dep in pkg.dependencies.values() {
        let available: Vec<_> = index.available_versions(&dep.id).into_diag()?.collect();
        dependencies.push(DependencyChecks {
            dep: dep.clone(),
            has_match: available.iter().any(|v| dep.version.matches(v)),
            known_versions: available,
        });
    }

    Ok(ManifestChecks {
        package_version: pkg.version.clone(),
        manifest_version: manifest.version,
        dependencies,
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use gix::ObjectId;
    use nickel_lang_package::version::SemVer;

    use super::*;

    const SAMPLE_DIFF: &str = r#"
diff --git a/github/nickel-lang/nickel-schemastore b/github/nickel-lang/nickel-schemastore
index df1cd2a..2229806 100644
--- a/github/nickel-lang/nickel-schemastore
+++ b/github/nickel-lang/nickel-schemastore
@@ -1 +1,2 @@
 {"id":{"github":{"org":"nickel-lang","name":"nickel-schemastore","commit":"3ac728792d4a71f53897b185445b77029c3ce245"}},"version":{"major":0,"minor":1,"patch":0,"pre":""},"minimal_nickel_version":{"major":1,"minor":11,"patch":0,"pre":""},"dependencies":{},"authors":["Théophane Hufschmitt","Yann Hamdaoui <yann.hamdaoui@tweag.io>"],"description":"A nickel package containing contracts autogenerated from the Schemastore JSON Schema repository via json-schema-to-nickel.","keywords":["schemastore","schemas","json-schema","contracts"],"license":"MIT","v":0}
+{"id":{"github":{"org":"nickel-lang","name":"nickel-schemastore","commit":"5b5edcba47eb5f957a34a6224b3d9b976a4fc911"}},"version":{"major":0,"minor":2,"patch":0,"pre":""},"minimal_nickel_version":{"major":1,"minor":11,"patch":0,"pre":""},"dependencies":{},"authors":["Théophane Hufschmitt","Yann Hamdaoui <yann.hamdaoui@tweag.io>"],"description":"Nickel contracts autogenerated from the Schemastore JSON Schema repository via json-schema-to-nickel","keywords":["schemastore","schemas","json-schema","contracts"],"license":"MIT","v":0}
"#;

    const SAMPLE_DIFF_WITH_SUBDIR: &str = r#"
diff --git a/github/nickel-lang/json-schema-to-nickel%@lib b/github/nickel-lang/json-schema-to-nickel%@lib
new file mode 100644
index 0000000..17e1150
--- /dev/null
+++ b/github/nickel-lang/json-schema-to-nickel%@lib
@@ -0,0 +1 @@
+{"id":{"github":{"org":"nickel-lang","name":"json-schema-to-nickel","path":"lib","commit":"7d7c007c1de43aa448df633ddbcb33b54385d8a0"}},"version":{"major":0,"minor":1,"patch":0,"pre":""},"minimal_nickel_version":{"major":1,"minor":12,"patch":0,"pre":""},"dependencies":{},"authors":["The json-schema-to-nickel authors"],"description":"A library of predicates for JSON schema","keywords":[],"license":"","v":0}
"#;

    #[test]
    fn test_changed_packages() {
        let packages = changed_packages(SAMPLE_DIFF).unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(packages[0].version, SemVer::new(0, 2, 0));
    }

    #[test]
    fn test_changed_packages_with_subdir() {
        let packages = changed_packages(SAMPLE_DIFF_WITH_SUBDIR).unwrap();
        assert_eq!(packages.len(), 1);
        assert_eq!(
            packages[0].id,
            PreciseId::Github {
                org: "nickel-lang".to_owned(),
                name: "json-schema-to-nickel".to_owned(),
                path: PathBuf::from("lib").try_into().unwrap(),
                commit: ObjectId::from_hex(b"7d7c007c1de43aa448df633ddbcb33b54385d8a0").unwrap(),
            }
        );
    }
}
