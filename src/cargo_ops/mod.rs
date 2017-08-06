use std::path::{Path, PathBuf};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::process;
use std::error::Error;

use tempdir::TempDir;
use toml::Value;
use toml::value::Table;
use cargo::core::{Package, PackageId, PackageIdSpec, PackageSet, Resolve, Workspace};
use cargo::ops::{self, Packages};
use cargo::util::{CargoError, CargoErrorKind, CargoResult, Config};
use cargo::util::graph::{Graph, Nodes};

#[derive(Debug, Serialize, Deserialize)]
struct Manifest {
    pub package: Table,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "opt_tables_last")]
    pub dependencies: Option<Table>,
    #[serde(rename = "dev-dependencies", skip_serializing_if = "Option::is_none",
            serialize_with = "opt_tables_last")]
    pub dev_dependencies: Option<Table>,
    #[serde(rename = "build-dependencies", skip_serializing_if = "Option::is_none",
            serialize_with = "opt_tables_last")]
    pub build_dependencies: Option<Table>,
    pub lib: Option<Table>,
    pub bin: Option<Vec<Table>>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "opt_tables_last")]
    pub workspace: Option<Table>,
    #[serde(skip_serializing_if = "Option::is_none", serialize_with = "opt_tables_last")]
    pub target: Option<Table>,
}

pub fn opt_tables_last<'tbl, S>(data: &'tbl Option<Table>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: ::serde::ser::Serializer,
{
    match data {
        &Some(ref d) => ::toml::ser::tables_last(d, serializer),
        &None => unreachable!(),
    }
}

pub struct TempProject<'tmp> {
    pub workspace: Workspace<'tmp>,
    pub temp_dir: TempDir,
}

impl<'tmp> TempProject<'tmp> {
    pub fn from_workspace(
        orig_workspace: &Workspace,
        config: &'tmp Config,
    ) -> CargoResult<TempProject<'tmp>> {
        let workspace_root = orig_workspace.root().to_str().ok_or_else(|| {
            CargoError::from_kind(CargoErrorKind::Msg(format!(
                "Invalid character found in path {}",
                orig_workspace.root().to_string_lossy()
            )))
        })?;

        let temp_dir = TempDir::new("cargo-outdated")?;
        for pkg in orig_workspace.members() {
            let source = String::from(pkg.root().to_string_lossy());
            let destination = source.replacen(
                workspace_root,
                &temp_dir.path().to_string_lossy().to_string(),
                1,
            );
            fs::create_dir_all(&destination)?;
            fs::copy(
                source.clone() + "/Cargo.toml",
                destination.clone() + "/Cargo.toml",
            )?;
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(destination.clone() + "/Cargo.toml")?;
            write!(
                file,
                "
[[bin]]
name = \"test\"
path = \"test.rs\"
            "
            )?;
            let lockfile = PathBuf::from(source.clone() + "/Cargo.lock");
            if lockfile.is_file() {
                fs::copy(lockfile, destination.clone() + "/Cargo.lock")?;
            }
        }

        let temp_root_manifest = String::from(temp_dir.path().to_string_lossy()) + "/Cargo.toml";
        let temp_root_manifest = PathBuf::from(temp_root_manifest);
        Ok(TempProject {
            workspace: Workspace::new(&temp_root_manifest, config)?,
            temp_dir: temp_dir,
        })
    }

    pub fn cargo_update(&mut self, config: &'tmp Config) -> CargoResult<()> {
        let root_manifest = String::from(self.workspace.root().to_string_lossy()) + "/Cargo.toml";
        if let Err(e) = process::Command::new("cargo")
            .arg("update")
            .arg("--manifest-path")
            .arg(&root_manifest)
            .output()
            .and_then(|v| if v.status.success() {
                Ok(v)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "did not exit successfully",
                ))
            }) {
            return Err(CargoError::from_kind(CargoErrorKind::Msg(format!(
                "Failed to run 'cargo update' with error '{}'",
                e.description()
            ))));
        }
        self.workspace = Workspace::new(Path::new(&root_manifest), config)?;
        Ok(())
    }

    fn write_manifest<P: AsRef<Path>>(manifest: &Manifest, path: P) -> CargoResult<()> {
        let mut file = try!(File::create(path));
        let serialized = ::toml::to_string(manifest).expect("Failed to serialized Cargo.toml");
        try!(write!(file, "{}", serialized));
        Ok(())
    }

    pub fn write_manifest_semver(&self) -> CargoResult<()> {
        let bin = {
            let mut bin = Table::new();
            bin.insert("name".to_owned(), Value::String("test".to_owned()));
            bin.insert("path".to_owned(), Value::String("test.rs".to_owned()));
            bin
        };
        for pkg in self.workspace.members() {
            let manifest_path = pkg.manifest_path();
            let mut manifest: Manifest = {
                let mut buf = String::new();
                let mut file = File::open(manifest_path)?;
                file.read_to_string(&mut buf)?;
                ::toml::from_str(&buf)?
            };
            manifest.bin = Some(vec![bin.clone()]);
            // provide lib.path
            manifest.lib.as_mut().map(|lib| {
                lib.insert("path".to_owned(), Value::String("test_lib.rs".to_owned()));
            });
            Self::write_manifest(&manifest, manifest_path)?;
        }

        Ok(())
    }

    pub fn write_manifest_latest(&self) -> CargoResult<()> {
        let bin = {
            let mut bin = Table::new();
            bin.insert("name".to_owned(), Value::String("test".to_owned()));
            bin.insert("path".to_owned(), Value::String("test.rs".to_owned()));
            bin
        };
        for pkg in self.workspace.members() {
            let manifest_path = pkg.manifest_path();
            let mut manifest: Manifest = {
                let mut buf = String::new();
                let mut file = File::open(manifest_path)?;
                file.read_to_string(&mut buf)?;
                ::toml::from_str(&buf)?
            };
            manifest.bin = Some(vec![bin.clone()]);

            // provide lib.path
            manifest.lib.as_mut().map(|lib| {
                lib.insert("path".to_owned(), Value::String("test_lib.rs".to_owned()));
            });

            // replace versions of direct dependencies
            manifest
                .dependencies
                .as_mut()
                .map(Self::replace_version_with_wildcard);
            manifest
                .dev_dependencies
                .as_mut()
                .map(Self::replace_version_with_wildcard);
            manifest
                .build_dependencies
                .as_mut()
                .map(Self::replace_version_with_wildcard);

            // replace target-specific dependencies
            manifest.target.as_mut().map(
                |ref mut t| for target in t.values_mut() {
                    if let &mut Value::Table(ref mut target) = target {
                        for dependency_tables in
                            &["dependencies", "dev-dependencies", "build-dependencies"]
                        {
                            target.get_mut(*dependency_tables).map(|dep_table| {
                                if let &mut Value::Table(ref mut dep_table) = dep_table {
                                    Self::replace_version_with_wildcard(dep_table);
                                }
                            });
                        }
                    }
                },
            );
            Self::write_manifest(&manifest, manifest_path)?;
        }
        Ok(())
    }

    fn replace_version_with_wildcard(dependencies: &mut Table) {
        let dep_names: Vec<_> = dependencies.keys().cloned().collect();
        for name in dep_names {
            let original = dependencies.get(&name).cloned().unwrap();
            match original {
                Value::String(_) => {
                    dependencies.insert(name, Value::String("*".to_owned()));
                }
                Value::Table(ref t) => {
                    let mut replaced = t.clone();
                    if replaced.contains_key("version") {
                        replaced.insert("version".to_owned(), Value::String("*".to_owned()));
                    }
                    dependencies.insert(name, Value::Table(replaced));
                }
                _ => panic!("Dependency spec is neither a string nor a table {}", name),
            }
        }
    }
}

pub fn elaborate_workspace<'elb>(
    workspace: &'elb Workspace,
    options: &super::Options,
) -> CargoResult<(Vec<PackageIdSpec>, PackageSet<'elb>, Resolve)> {
    let specs = Packages::All.into_package_id_specs(&workspace)?;
    let (packages, resolve) = ops::resolve_ws_precisely(
        &workspace,
        None,
        &options.flag_features,
        options.flag_all_features,
        options.flag_no_default_features,
        &specs,
    )?;
    Ok((specs, packages, resolve))
}

pub fn compare_versions(
    curr: &Workspace,
    compat: &Workspace,
    latest: &Workspace,
    options: &super::Options,
    config: &Config,
) -> CargoResult<()> {
    let (curr_specs, curr_pkgs, curr_resolv) = elaborate_workspace(curr, options)?;
    let (compat_specs, compat_pkgs, compat_resolv) = elaborate_workspace(compat, options)?;
    let (latest_specs, latest_pkgs, latest_resolv) = elaborate_workspace(compat, options)?;

    let curr_root = curr.current()?.package_id();
    let compat_root = compat.current()?.package_id();
    let latest_root = compat.current()?.package_id();

    compare_versions_recursive(
        &curr_root,
        &curr_pkgs,
        &curr_resolv,
        Some(&compat_root),
        &compat_pkgs,
        &compat_resolv,
        Some(&latest_root),
        &latest_pkgs,
        &latest_resolv,
    )?;

    Ok(())
}

fn compare_versions_recursive(
    curr_root: &PackageId,
    curr_pkgs: &PackageSet,
    curr_resolv: &Resolve,
    compat_root: Option<&PackageId>,
    compat_pkgs: &PackageSet,
    compat_resolv: &Resolve,
    latest_root: Option<&PackageId>,
    latest_pkgs: &PackageSet,
    latest_resolv: &Resolve,
) -> CargoResult<()> {
    let compat_version = match compat_root {
        Some(compat_root) => {
            let v = compat_pkgs.get(compat_root)?.version();
            if v != curr_pkgs.get(curr_root)?.version() {
                Some(v.to_string())
            } else {
                None
            }
        }
        None => Some("  RM  ".to_owned()),
    };
    let latest_version = match latest_root {
        Some(latest_root) => {
            let v = latest_pkgs.get(latest_root)?.version();
            if v != curr_pkgs.get(curr_root)?.version() {
                Some(v.to_string())
            } else {
                None
            }
        }
        None => Some("  RM  ".to_owned()),
    };
    let curr_name = curr_pkgs.get(curr_root)?.name();
    if compat_version.is_some() || latest_version.is_some() {
        println!(
            "{} {} {}",
            curr_name,
            compat_version.unwrap_or_else(|| "  --  ".to_owned()),
            latest_version.unwrap_or_else(|| "  --  ".to_owned())
        );
    }

    for dep in curr_resolv.deps(curr_root) {
        let dep_pkg = curr_pkgs.get(dep)?;
        let dep_name = dep_pkg.name();
        let next_compat_root =
            compat_root.and_then(|i| find_dep_by_name(dep_name, i, compat_resolv));
        let next_latest_root =
            latest_root.and_then(|i| find_dep_by_name(dep_name, i, latest_resolv));
        compare_versions_recursive(
            dep_pkg.package_id(),
            curr_pkgs,
            curr_resolv,
            next_compat_root,
            compat_pkgs,
            compat_resolv,
            next_latest_root,
            latest_pkgs,
            latest_resolv,
        )?;
    }

    Ok(())
}

fn find_dep_by_name<'fin>(
    name: &str,
    pkg: &PackageId,
    resolv: &'fin Resolve,
) -> Option<&'fin PackageId> {
    for dep in resolv.deps(pkg) {
        if dep.name() == name {
            return Some(dep);
        }
    }
    None
}
