//! Provides types for installing packages to the user toolchain.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{read_dir, rename, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::str;

use hex;
use semver::Version;
use sha1::{Digest, Sha1};

use crate::distro::{download_tool_error, Distro, Fetched};
use crate::error::ErrorDetails;
use crate::fs::{ensure_containing_dir_exists, read_file_opt};
use crate::hook::ToolHooks;
use crate::inventory::Collection;
use crate::manifest::Manifest;
use crate::path;
use crate::platform::{Image, PlatformSpec};
use crate::session::Session;
use crate::shim;
use crate::style::progress_bar;
use crate::tool::ToolSpec;
use crate::version::VersionSpec;
use archive::{Archive, Tarball};
use tempfile::tempdir_in;

use notion_fail::{throw, Fallible, ResultExt};

fn install_error(error: &io::Error) -> ErrorDetails {
    if let Some(inner_err) = error.get_ref() {
        ErrorDetails::PackageInstallIoError {
            error: inner_err.to_string(),
        }
    } else {
        ErrorDetails::PackageInstallIoError {
            error: error.to_string(),
        }
    }
}

/// A provisioned Package distribution.
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct PackageDistro {
    pub name: String,
    pub shasum: String,
    pub tarball_url: String,
    pub version: Version,
    pub image_dir: PathBuf,
    pub shasum_file: PathBuf,
    pub distro_file: PathBuf,
}

/// A package version.
#[derive(Eq, PartialEq, Clone, Debug)]
pub struct PackageVersion {
    pub name: String,
    pub version: Version,
    // map of binary names to locations
    pub bins: HashMap<String, String>,
    image_dir: PathBuf,
}

/// Programs used to install packages.
enum Installer {
    Npm,
    Yarn,
}

/// Configuration information about an installed package.
pub struct PackageConfig {
    /// The package name
    pub name: String,
    /// The package version
    pub version: Version,
    /// The platform used to install this package
    pub platform: PlatformSpec,
    /// The binaries installed by this package
    pub bins: Vec<String>,
}

/// Configuration information about an installed binary from a package.
pub struct BinConfig {
    /// The binary name
    pub name: String,
    /// The package that installed this binary
    pub package: String,
    /// The package version
    pub version: Version,
    /// The relative path of the binary in the installed package
    pub path: String,
    /// The platform used to install this binary
    pub platform: PlatformSpec,
}

impl Distro for PackageDistro {
    type VersionDetails = PackageVersion;
    type ResolvedVersion = PackageEntry;

    fn new(
        name: String,
        entry: Self::ResolvedVersion,
        _hooks: Option<&ToolHooks<Self>>,
    ) -> Fallible<Self> {
        let version = entry.version;
        Ok(PackageDistro {
            name: name.to_string(),
            shasum: entry.shasum,
            version: version.clone(),
            tarball_url: entry.tarball,
            image_dir: path::package_image_dir(&name, &version.to_string())?,
            distro_file: path::package_distro_file(&name, &version.to_string())?,
            shasum_file: path::package_distro_shasum(&name, &version.to_string())?,
        })
    }

    fn fetch(self, _collection: &Collection<Self>) -> Fallible<Fetched<PackageVersion>> {
        let archive = self.load_or_fetch_archive()?;

        let bar = progress_bar(
            archive.action(),
            &format!("{}-v{}", self.name, self.version),
            archive
                .uncompressed_size()
                .unwrap_or(archive.compressed_size()),
        );

        let temp = tempdir_in(path::tmp_dir()?).unknown()?;
        archive
            .unpack(temp.path(), &mut |_, read| {
                bar.inc(read as u64);
            })
            .unknown()?;
        bar.finish();

        ensure_containing_dir_exists(&self.image_dir)?;

        let unpack_dir = find_unpack_dir(temp.path())?;
        rename(unpack_dir, &self.image_dir).unknown()?;

        // save the shasum in a file
        let mut f = File::create(&self.shasum_file).unknown()?;
        f.write_all(self.shasum.as_bytes()).unknown()?;
        f.sync_all().unknown()?;

        let pkg_info = Manifest::for_dir(&self.image_dir).with_context(|error| {
            ErrorDetails::DepPackageReadError {
                error: error.to_string(),
            }
        })?;
        let bin_map = pkg_info.bin;
        if bin_map.is_empty() {
            throw!(ErrorDetails::NoPackageExecutables);
        }

        for (bin_name, _bin_path) in bin_map.iter() {
            // check for conflicts with installed bins
            // some packages may install bins with the same name
            let bin_config_file = path::user_tool_bin_config(&bin_name)?;
            if bin_config_file.exists() {
                let bin_config = BinConfig::from_file(bin_config_file)?;
                throw!(ErrorDetails::BinaryAlreadyInstalled {
                    bin_name: bin_name.to_string(),
                    package: bin_config.package,
                    version: bin_config.version.to_string()
                });
            }
        }

        Ok(Fetched::Now(PackageVersion::new(
            self.name.clone(),
            self.version.clone(),
            bin_map,
        )?))
    }

    fn version(&self) -> &Version {
        &self.version
    }
}

impl PackageDistro {
    /// Loads the package tarball from disk, or fetches from URL.
    fn load_or_fetch_archive(&self) -> Fallible<Box<Archive>> {
        // try to use existing downloaded package
        if self.downloaded_pkg_is_ok() {
            Tarball::load(File::open(&self.distro_file).unknown()?).unknown()
        } else {
            // otherwise have to download
            ensure_containing_dir_exists(&self.distro_file)?;
            Tarball::fetch(&self.tarball_url, &self.distro_file).with_context(download_tool_error(
                ToolSpec::Package(self.name.to_string(), VersionSpec::exact(&self.version)),
                self.tarball_url.to_string(),
            ))
        }
    }

    /// Verify downloaded package, returning a PackageVersion if it is ok.
    fn downloaded_pkg_is_ok(&self) -> bool {
        let mut buffer = Vec::new();

        if let Ok(mut distro) = File::open(&self.distro_file) {
            if let Ok(Some(stored_shasum)) = read_file_opt(&self.shasum_file) {
                if distro.read_to_end(&mut buffer).is_ok() {
                    // calculate the shasum
                    let mut hasher = Sha1::new();
                    hasher.input(buffer);
                    let result = hasher.result();
                    let calculated_shasum = hex::encode(&result);

                    return stored_shasum == calculated_shasum;
                }
            }
        }

        // the files don't exist, or the shasum doesn't match
        false
    }
}

// Figure out the unpacked package directory name dynamically, because
// packages typically extract to a "package" directory, but not always
fn find_unpack_dir(in_dir: &Path) -> Fallible<PathBuf> {
    let mut dirs = Vec::new();
    for entry in read_dir(in_dir).unknown()? {
        let entry = entry.unknown()?;
        dirs.push(entry.path());
    }
    if dirs.len() == 1 {
        Ok(dirs[0].to_path_buf())
    } else {
        // there is more than just a directory here, something is wrong
        throw!(ErrorDetails::PackageUnpackError);
    }
}

impl PackageVersion {
    pub fn new(name: String, version: Version, bins: HashMap<String, String>) -> Fallible<Self> {
        let image_dir = path::package_image_dir(&name, &version.to_string())?;
        Ok(PackageVersion {
            name,
            version,
            bins,
            image_dir,
        })
    }

    // parse the "engines" string to a VersionSpec, for matching against available Node versions
    pub fn engines_spec(&self) -> Fallible<VersionSpec> {
        let manifest = Manifest::for_dir(&self.image_dir)?;
        let engines = match manifest.engines() {
            Some(e) => e,
            None => "*".to_string(), // if nothing specified, can use any version of Node
        };
        Ok(VersionSpec::Semver(VersionSpec::parse_requirements(
            engines,
        )?))
    }

    pub fn install(&self, platform: &PlatformSpec, session: &mut Session) -> Fallible<()> {
        let image = platform.checkout(session)?;
        // use yarn if it is installed, otherwise default to npm
        let mut install_cmd = if image.yarn.is_some() {
            install_command_for(
                Installer::Yarn,
                &self.image_dir.clone().into_os_string(),
                &image.path()?,
            )
        } else {
            install_command_for(
                Installer::Npm,
                &self.image_dir.clone().into_os_string(),
                &image.path()?,
            )
        };

        let output = install_cmd.output().with_context(install_error)?;
        if !output.status.success() {
            throw!(ErrorDetails::PackageInstallFailed {
                cmd: format!("{:?}", install_cmd),
                status: output.status
            });
        }

        self.write_config_and_shims(&platform)?;

        Ok(())
    }

    fn package_config(&self, platform_spec: &PlatformSpec) -> PackageConfig {
        PackageConfig {
            name: self.name.to_string(),
            version: self.version.clone(),
            platform: platform_spec.clone(),
            bins: self
                .bins
                .iter()
                .map(|(name, _path)| name.to_string())
                .collect(),
        }
    }

    fn bin_config(
        &self,
        bin_name: String,
        bin_path: String,
        platform_spec: &PlatformSpec,
    ) -> BinConfig {
        BinConfig {
            name: bin_name,
            package: self.name.to_string(),
            version: self.version.clone(),
            path: bin_path,
            platform: platform_spec.clone(),
        }
    }

    fn write_config_and_shims(&self, platform_spec: &PlatformSpec) -> Fallible<()> {
        self.package_config(&platform_spec).to_serial().write()?;
        for (bin_name, bin_path) in self.bins.iter() {
            self.bin_config(bin_name.to_string(), bin_path.to_string(), &platform_spec)
                .to_serial()
                .write()?;
            // create a link to the shim executable
            shim::create(&bin_name)?;
        }
        Ok(())
    }
}

impl Installer {
    pub fn cmd(&self) -> Command {
        match self {
            Installer::Npm => {
                let mut command = Command::new("npm");
                command.args(&["install", "--only=production"]);
                command
            }
            Installer::Yarn => {
                let mut command = Command::new("yarn");
                command.args(&["install", "--production"]);
                command
            }
        }
    }
}

/// Information about a user tool.
pub struct UserTool {
    pub bin_path: PathBuf,
    pub image: Image,
}

impl UserTool {
    pub fn from_config(bin_config: BinConfig, session: &mut Session) -> Fallible<Option<Self>> {
        let image_dir =
            path::package_image_dir(&bin_config.package, &bin_config.version.to_string())?;
        // canonicalize because path is relative, and sometimes uses '.' char
        let bin_path = image_dir.join(bin_config.path).canonicalize().unknown()?;

        Ok(Some(UserTool {
            bin_path,
            image: bin_config.platform.checkout(session)?,
        }))
    }
}

pub fn user_tool(tool_name: &str, session: &mut Session) -> Fallible<Option<UserTool>> {
    let bin_config_file = path::user_tool_bin_config(tool_name)?;
    if bin_config_file.exists() {
        let bin_config = BinConfig::from_file(bin_config_file)?;
        Ok(UserTool::from_config(bin_config, session)?)
    } else {
        Ok(None) // no config means the tool is not installed
    }
}

// build a package install command using the specified directory and path
fn install_command_for(installer: Installer, in_dir: &OsStr, path_var: &OsStr) -> Command {
    let mut command = installer.cmd();
    command.current_dir(in_dir);
    command.env("PATH", path_var);
    // connect stdout and stderr to the current stdout and stderr for this process
    // (so the user can see the install progress in real time)
    command.stdout(Stdio::inherit());
    command.stderr(Stdio::inherit());
    command
}

/// Index of versions of a specific package.
pub struct PackageIndex {
    pub latest: Version,
    pub entries: Vec<PackageEntry>,
}

#[derive(Debug)]
pub struct PackageEntry {
    pub version: Version,
    pub tarball: String,
    pub shasum: String,
}
