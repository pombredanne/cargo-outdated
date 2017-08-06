use std::path::{Path, PathBuf};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::process;
use std::error::Error;

use tempdir::TempDir;
use toml::Value;
use toml::value::Table;
use cargo::core::{Package, PackageId, Workspace};
use cargo::ops::{self, Packages};
use cargo::util::{CargoError, CargoErrorKind, CargoResult, Config};

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

pub fn opt_tables_last<'a, S>(data: &'a Option<Table>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: ::serde::ser::Serializer,
{
    match data {
        &Some(ref d) => ::toml::ser::tables_last(d, serializer),
        &None => unreachable!(),
    }
}

pub struct TempProject<'a> {
    pub workspace: Workspace<'a>,
    temp_dir: TempDir,
}

impl<'a> TempProject<'a> {
    pub fn from_workspace(
        orig_workspace: &Workspace,
        config: &'a Config,
    ) -> CargoResult<TempProject<'a>> {
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

    pub fn cargo_update(&self) -> CargoResult<()> {
        if let Err(e) = process::Command::new("cargo")
            .arg("update")
            .arg("--manifest-path")
            .arg(
                &(String::from(self.workspace.root().to_string_lossy()) + "/Cargo.toml"),
            )
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
