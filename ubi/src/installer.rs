use crate::{extension::Extension, ubi::Download};
use anyhow::{anyhow, Context, Result};
use binstall_tar::Archive;
use bzip2::read::BzDecoder;
use flate2::read::GzDecoder;
use log::{debug, info};
use std::{
    collections::HashSet,
    ffi::OsString,
    fmt::Debug,
    fs::{self, create_dir_all, File},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use strum::IntoEnumIterator;
use xz2::read::XzDecoder;
use zip::{read::ZipFile, ZipArchive};

#[cfg(target_family = "unix")]
use std::fs::{set_permissions, Permissions};
#[cfg(target_family = "unix")]
use std::os::unix::fs::PermissionsExt;

pub(crate) trait Installer: Debug {
    fn install(&self, download: &Download) -> Result<()>;
}

#[derive(Debug)]
pub(crate) struct ExeInstaller {
    install_path: PathBuf,
    exe_file_stem: String,
    is_windows: bool,
    extensions: Vec<&'static str>,
}

#[derive(Debug)]
pub(crate) struct ArchiveInstaller {
    install_root: PathBuf,
}

impl ExeInstaller {
    pub(crate) fn new(install_path: PathBuf, exe: String, is_windows: bool) -> Self {
        let extensions = if is_windows {
            Extension::iter()
                .filter(super::extension::Extension::is_windows_only)
                .map(|e| e.extension())
                .collect()
        } else {
            vec![]
        };

        ExeInstaller {
            install_path,
            exe_file_stem: exe,
            is_windows,
            extensions,
        }
    }

    fn extract_executable(&self, downloaded_file: &Path) -> Result<Option<PathBuf>> {
        match Extension::from_path(downloaded_file)? {
            Some(
                Extension::Tar
                | Extension::TarBz
                | Extension::TarBz2
                | Extension::TarGz
                | Extension::TarXz
                | Extension::Tbz
                | Extension::Tgz
                | Extension::Txz,
            ) => Ok(Some(self.extract_executable_from_tarball(downloaded_file)?)),
            Some(Extension::Bz | Extension::Bz2) => {
                self.unbzip(downloaded_file)?;
                Ok(None)
            }
            Some(Extension::Gz) => {
                self.ungzip(downloaded_file)?;
                Ok(None)
            }
            Some(Extension::Xz) => {
                self.unxz(downloaded_file)?;
                Ok(None)
            }
            Some(Extension::Zip) => Ok(Some(self.extract_executable_from_zip(downloaded_file)?)),
            Some(Extension::AppImage | Extension::Bat | Extension::Exe | Extension::Pyz) | None => {
                Ok(Some(self.copy_executable(downloaded_file)?))
            }
        }
    }

    fn extract_executable_from_tarball(&self, downloaded_file: &Path) -> Result<PathBuf> {
        debug!(
            "extracting executable from tarball at {}",
            downloaded_file.display(),
        );

        // Iterating through the archive both here and in `best_match_from_tarball` is really
        // gross. But this is necessary because the underlying `Entry` structs returned by
        // `arch.entries` are only valid for the duration of the loop iteration. That's because they
        // rely on the position of the underlying file handle. It'd be nice to just be able to seek
        // that handle back to the start of the file, but the readers provided by various decoders,
        // like `BzDecoder`, do not implement the `Seek` trait.
        //
        // So the only viable solution is find the entry, then _re-open_ the file and go through the
        // entries again until we find the one we want.
        if let Some(idx) = self.best_match_from_tarball(downloaded_file)? {
            let mut arch = tar_reader_for(downloaded_file)?;
            for (i, entry) in arch.entries()?.enumerate() {
                let mut entry = entry?;
                if i != idx {
                    continue;
                }

                let entry_path = entry.path()?;
                let mut install_path = self.install_path.clone();
                if let Some(ext) = Extension::from_path(entry_path.as_ref())? {
                    if ext.should_preserve_extension_on_install() {
                        debug!("preserving the {} extension on install", ext.extension());
                        install_path.set_extension(ext.extension_without_dot());
                    }
                }

                debug!(
                    "extracting tarball entry named {} to {}",
                    entry_path.display(),
                    install_path.display(),
                );
                self.create_install_dir()?;
                entry.unpack(&install_path).unwrap();

                return Ok(install_path);
            }
        }

        self.could_not_find_archive_matches_error()
    }

    fn best_match_from_tarball<'a>(&self, downloaded_file: &Path) -> Result<Option<usize>> {
        let mut arch = tar_reader_for(downloaded_file)?;
        let mut possible_matches: Vec<usize> = vec![];
        for (i, entry) in arch.entries()?.enumerate() {
            let entry = entry?;
            if !entry.header().entry_type().is_file() {
                continue;
            }

            let path = entry.path()?;
            debug!("found tarball entry with path {}", path.display());
            if let Some(file_name) = path.file_name() {
                if let Some(file_name) = file_name.to_str() {
                    if self.archive_member_is_exact_match(file_name) {
                        debug!("found tar file entry with exact match: {}", file_name);
                        return Ok(Some(i));
                    } else if self.archive_member_is_partial_match(file_name) {
                        // This checks if the entry is marked as an executable, but a tarball
                        // created on Windows may not have file modes set.
                        if self.is_windows || entry.header().mode()? & 0o111 != 0 {
                            debug!("found tar file entry with partial match: {}", file_name);
                            possible_matches.push(i);
                        }
                    }
                }
            }
        }

        Ok(possible_matches.into_iter().next())
    }

    fn extract_executable_from_zip(&self, downloaded_file: &Path) -> Result<PathBuf> {
        debug!(
            "extracting executable from zip file at {}",
            downloaded_file.display()
        );

        let mut zip = ZipArchive::new(open_file(downloaded_file)?)?;
        if let Some(mut zf) = self.best_match_from_zip_archive(&mut zip)? {
            let zf_path = Path::new(zf.name());
            let mut install_path = self.install_path.clone();
            if let Some(ext) = Extension::from_path(zf_path)? {
                if ext.should_preserve_extension_on_install() {
                    debug!("preserving the {} extension on install", ext.extension());
                    install_path.set_extension(ext.extension_without_dot());
                }
            }

            debug!(
                "extracting zip file entry named {} to {}",
                zf.name(),
                install_path.display(),
            );
            let mut buffer: Vec<u8> = Vec::with_capacity(usize::try_from(zf.size())?);
            zf.read_to_end(&mut buffer)?;
            self.create_install_dir()?;

            File::create(&install_path)?.write_all(&buffer)?;

            return Ok(install_path);
        }

        self.could_not_find_archive_matches_error()
    }

    fn best_match_from_zip_archive<'a>(
        &self,
        zip: &'a mut ZipArchive<File>,
    ) -> Result<Option<ZipFile<'a>>> {
        let mut possible_matches: Vec<usize> = vec![];
        for i in 0..zip.len() {
            let zf = zip.by_index(i)?;
            if zf.is_file() {
                let path = PathBuf::from(zf.name());
                if let Some(file_name) = path.file_name() {
                    if let Some(file_name) = file_name.to_str() {
                        if self.archive_member_is_exact_match(file_name) {
                            debug!("found zip file entry with exact match: {}", file_name);
                            // It'd be nicer to immediately return `zf`, but that runs into lifetime
                            // issues, because `zip.by_index` takes `&mut self`. Yeesh.
                            possible_matches.push(i);
                            break;
                        } else if self.archive_member_is_partial_match(file_name) {
                            debug!("found zip file entry with partial match: {}", file_name);
                            // Note that we don't test if the file is executable on Unix systems
                            // because preserving the mode is not a standard Zip behavior, AFAICT.
                            possible_matches.push(i);
                        }
                    }
                }
            }
        }

        if let Some(i) = possible_matches.first() {
            return Ok(Some(zip.by_index(*i)?));
        }

        Ok(None)
    }

    fn archive_member_is_exact_match(&self, file_name: &str) -> bool {
        if self.extensions.is_empty() {
            return file_name == self.exe_file_stem;
        }

        self.extensions
            .iter()
            .map(|&ext| format!("{}{}", self.exe_file_stem.to_lowercase(), ext))
            .any(|n| n == file_name)
    }

    fn archive_member_is_partial_match(&self, file_name: &str) -> bool {
        if !file_name.starts_with(&self.exe_file_stem) {
            return false;
        }
        if self.extensions.is_empty() {
            return true;
        }
        self.extensions
            .iter()
            .any(|&ext| file_name.to_lowercase().ends_with(ext))
    }

    fn could_not_find_archive_matches_error(&self) -> Result<PathBuf> {
        let expect_names = if self.extensions.is_empty() {
            format!("{}*", self.exe_file_stem)
        } else {
            self.extensions
                .iter()
                .map(|ext| format!("{}*{}", self.exe_file_stem, ext))
                .collect::<Vec<_>>()
                .join(" ")
        };

        debug!("could not find any entries matching [{}]", expect_names);
        Err(anyhow!(
            "could not find any files matching [{}] in the downloaded archive file",
            expect_names,
        ))
    }

    fn unbzip(&self, downloaded_file: &Path) -> Result<()> {
        debug!("uncompressing executable from bzip file");
        let reader = BzDecoder::new(open_file(downloaded_file)?);
        self.write_to_install_path(reader)
    }

    fn ungzip(&self, downloaded_file: &Path) -> Result<()> {
        debug!("uncompressing executable from gzip file");
        let reader = GzDecoder::new(open_file(downloaded_file)?);
        self.write_to_install_path(reader)
    }

    fn unxz(&self, downloaded_file: &Path) -> Result<()> {
        debug!("uncompressing executable from xz file");
        let reader = XzDecoder::new(open_file(downloaded_file)?);
        self.write_to_install_path(reader)
    }

    fn write_to_install_path(&self, mut reader: impl Read) -> Result<()> {
        self.create_install_dir()?;
        let mut writer = File::create(&self.install_path)
            .with_context(|| format!("Cannot write to {}", self.install_path.display()))?;
        std::io::copy(&mut reader, &mut writer)?;
        Ok(())
    }

    fn copy_executable(&self, exe_file: &Path) -> Result<PathBuf> {
        debug!("copying executable to final location");
        self.create_install_dir()?;

        let mut install_path = self.install_path.clone();
        if let Some(ext) = Extension::from_path(exe_file)? {
            if ext.should_preserve_extension_on_install() {
                debug!("preserving the {} extension on install", ext.extension());
                install_path.set_extension(ext.extension_without_dot());
            }
        }
        std::fs::copy(exe_file, &install_path).context(format!(
            "error copying file from {} to {}",
            exe_file.display(),
            install_path.display()
        ))?;

        Ok(install_path)
    }

    fn create_install_dir(&self) -> Result<()> {
        let Some(path) = self.install_path.parent() else {
            return Err(anyhow!(
                "install path at {} has no parent",
                self.install_path.display()
            ));
        };

        debug!("creating directory at {}", path.display());
        create_dir_all(path)
            .with_context(|| format!("could not create a directory at {}", path.display()))
    }

    #[cfg(target_family = "windows")]
    fn chmod_executable(_exe: &Path) -> Result<()> {
        Ok(())
    }

    #[cfg(target_family = "unix")]
    fn chmod_executable(exe: &Path) -> Result<()> {
        match set_permissions(exe, Permissions::from_mode(0o755)) {
            Ok(()) => Ok(()),
            Err(e) => Err(anyhow::Error::new(e)),
        }
    }
}

impl Installer for ExeInstaller {
    fn install(&self, download: &Download) -> Result<()> {
        let exe = self.extract_executable(&download.archive_path)?;
        let real_exe = exe.as_deref().unwrap_or(&self.install_path);
        Self::chmod_executable(real_exe)?;
        info!("Installed executable into {}", real_exe.display());

        Ok(())
    }
}

impl ArchiveInstaller {
    pub(crate) fn new(install_path: PathBuf) -> Self {
        ArchiveInstaller {
            install_root: install_path,
        }
    }

    fn extract_entire_archive(&self, downloaded_file: &Path) -> Result<()> {
        match Extension::from_path(downloaded_file)? {
            Some(
                Extension::Tar
                | Extension::TarBz
                | Extension::TarBz2
                | Extension::TarGz
                | Extension::TarXz
                | Extension::Tbz
                | Extension::Tgz
                | Extension::Txz,
            ) => self.extract_entire_tarball(downloaded_file)?,
            Some(Extension::Zip) => self.extract_entire_zip(downloaded_file)?,
            _ => {
                return Err(anyhow!(
                    concat!(
                        "the downloaded release asset, {}, does not appear to be an",
                        " archive file so we cannopt extract all of its contents",
                    ),
                    downloaded_file.display(),
                ))
            }
        }

        if self.should_move_up_one_dir()? {
            Self::move_contents_up_one_dir(&self.install_root)?;
        } else {
            debug!("extracted archive did not contain a common top-level directory");
        }

        Ok(())
    }

    fn extract_entire_tarball(&self, downloaded_file: &Path) -> Result<()> {
        debug!("extracting entire tarball at {}", downloaded_file.display(),);

        let mut arch = tar_reader_for(downloaded_file)?;
        arch.unpack(&self.install_root)?;

        Ok(())
    }

    // We do this because some projects use a top-level dir like `project-x86-64-Linux`, which is
    // pretty annoying to work with. In this case, it's a lot easier to install this into
    // `~/bin/project` so the directory tree ends up with the same structure on all platforms.
    fn should_move_up_one_dir(&self) -> Result<bool> {
        let mut prefixes: HashSet<OsString> = HashSet::new();
        for entry in fs::read_dir(&self.install_root).with_context(|| {
            format!(
                "could not read {} after unpacking the tarball into this directory",
                self.install_root.display(),
            )
        })? {
            let full_path = entry
                .context("could not get path for tarball entry")?
                .path();

            // If the entry is a file in the top-level of the install dir, then there's no common
            // directory prefix.
            if full_path.is_file()
                && full_path
                    .parent()
                    .expect("path of entry in install root somehow has no parent")
                    == self.install_root
            {
                return Ok(false);
            }

            let path = if let Ok(path) = full_path.strip_prefix(&self.install_root) {
                path
            } else {
                &full_path
            };

            if let Some(prefix) = path.components().next() {
                prefixes.insert(prefix.as_os_str().to_os_string());
            } else {
                return Err(anyhow!("directory entry has no path components"));
            }
        }

        // If all the entries
        Ok(prefixes.len() == 1)
    }

    fn move_contents_up_one_dir(path: &Path) -> Result<()> {
        let mut entries = fs::read_dir(path)?;
        let top_level_path = if let Some(dir_entry) = entries.next() {
            let dir_entry = dir_entry?;
            dir_entry.path()
        } else {
            return Err(anyhow!("no directory found in path"));
        };

        debug!(
            "moving extracted archive contents up one directory from {} to {}",
            top_level_path.display(),
            path.display(),
        );

        for entry in fs::read_dir(&top_level_path)? {
            let entry = entry?;
            let target = path.join(entry.file_name());
            fs::rename(entry.path(), target)?;
        }

        fs::remove_dir(top_level_path)?;

        Ok(())
    }

    fn extract_entire_zip(&self, downloaded_file: &Path) -> Result<()> {
        debug!(
            "extracting entire zip file at {}",
            downloaded_file.display(),
        );

        let mut zip = ZipArchive::new(open_file(downloaded_file)?)?;
        Ok(zip.extract(&self.install_root)?)
    }
}

impl Installer for ArchiveInstaller {
    fn install(&self, download: &Download) -> Result<()> {
        self.extract_entire_archive(&download.archive_path)?;
        info!(
            "Installed contents of archive file into {}",
            self.install_root.display()
        );

        Ok(())
    }
}

fn tar_reader_for(downloaded_file: &Path) -> Result<Archive<Box<dyn Read>>> {
    let file = open_file(downloaded_file)?;

    let ext = downloaded_file.extension();
    match ext {
        Some(ext) => match ext.to_str() {
            Some("tar") => Ok(Archive::new(Box::new(file))),
            Some("bz" | "tbz" | "bz2" | "tbz2") => Ok(Archive::new(Box::new(BzDecoder::new(file)))),
            Some("gz" | "tgz") => Ok(Archive::new(Box::new(GzDecoder::new(file)))),
            Some("xz" | "txz") => Ok(Archive::new(Box::new(XzDecoder::new(file)))),
            Some(e) => Err(anyhow!(
                "don't know how to uncompress a tarball with extension = {}",
                e,
            )),
            None => Err(anyhow!(
                "tarball {:?} has a non-UTF-8 extension",
                downloaded_file,
            )),
        },
        None => Ok(Archive::new(Box::new(file))),
    }
}

fn open_file(path: &Path) -> Result<File> {
    File::open(path).with_context(|| format!("Failed to open file at {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_family = "unix")]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;
    use test_case::test_case;
    use test_log::test;

    #[test_case("test-data/project.AppImage", Some("AppImage"))]
    #[test_case("test-data/project.bat", Some("bat"))]
    #[test_case("test-data/project.bz", None)]
    #[test_case("test-data/project.bz2", None)]
    #[test_case("test-data/project.exe", Some("exe"))]
    #[test_case("test-data/project.gz", None)]
    #[test_case("test-data/project.pyz", Some("pyz"))]
    #[test_case("test-data/project.tar", None)]
    #[test_case("test-data/project.tar.bz", None)]
    #[test_case("test-data/project.tar.bz2", None)]
    #[test_case("test-data/project.tar.gz", None)]
    #[test_case("test-data/project.tar.xz", None)]
    #[test_case("test-data/project.xz", None)]
    #[test_case("test-data/project.zip", None)]
    #[test_case("test-data/project", None)]
    // These are archive files that just contain a partial match for the expected executable.
    #[test_case("test-data/project-with-partial-match.tar.gz", None)]
    #[test_case("test-data/project-with-partial-match.zip", None)]
    fn exe_installer(archive_path: &str, installed_extension: Option<&str>) -> Result<()> {
        crate::test_case::init_logging();

        let td = tempdir()?;
        let path_without_subdir = td.path().to_path_buf();
        test_installer(
            archive_path,
            installed_extension,
            path_without_subdir,
            false,
        )?;

        let td = tempdir()?;
        let mut path_with_subdir = td.path().to_path_buf();
        path_with_subdir.push("subdir");
        test_installer(archive_path, installed_extension, path_with_subdir, false)
    }

    // These tests check that we look for project.bat and project.exe in archive files when running
    // on Windows.
    #[test_case("test-data/windows-project-bat.tar.gz", "bat")]
    #[test_case("test-data/windows-project-exe.tar.gz", "exe")]
    #[test_case("test-data/windows-project-bat.zip", "bat")]
    #[test_case("test-data/windows-project-exe.zip", "exe")]
    // And these check that we match project-with-stuff.exe.
    #[test_case("test-data/windows-project-exe-with-partial-match.tar.gz", "exe")]
    #[test_case("test-data/windows-project-exe-with-partial-match.zip", "exe")]
    fn exe_installer_on_windows(archive_path: &str, extension: &str) -> Result<()> {
        crate::test_case::init_logging();

        let td = tempdir()?;
        let install_dir = td.path().to_path_buf();

        test_installer(archive_path, Some(extension), install_dir, true)
    }

    fn test_installer(
        archive_path: &str,
        installed_extension: Option<&str>,
        install_dir: PathBuf,
        is_windows: bool,
    ) -> Result<()> {
        let exe_file_stem = "project";

        let mut install_path = install_dir;
        install_path.push("project");

        let installer =
            ExeInstaller::new(install_path.clone(), exe_file_stem.to_string(), is_windows);
        installer.install(&Download {
            // It doesn't matter what we use here. We're not actually going to
            // put anything in this temp dir.
            _temp_dir: tempdir()?,
            archive_path: PathBuf::from(archive_path),
        })?;

        let mut expect_install_path = install_path.clone();
        if let Some(installed_extension) = installed_extension {
            let path = PathBuf::from(format!("foo.{installed_extension}"));
            let ext = Extension::from_path(&path).unwrap().unwrap();
            if ext.should_preserve_extension_on_install() {
                expect_install_path.set_extension(ext.extension_without_dot());
            }
        }

        assert!(
            fs::exists(&expect_install_path)?,
            "{} file exists",
            expect_install_path.display()
        );
        // Testing the installed file's length is a shortcut to make sure we install the file we
        // expected to install.
        let expect_len = if expect_install_path.extension().unwrap_or_default() == "pyz" {
            fs::metadata(archive_path)?.len()
        } else {
            3
        };
        let meta = expect_install_path.metadata()?;
        assert_eq!(meta.len(), expect_len);
        #[cfg(target_family = "unix")]
        assert!(meta.permissions().mode() & 0o111 != 0);

        Ok(())
    }

    #[test_case("test-data/project.tar")]
    #[test_case("test-data/project.tar.bz")]
    #[test_case("test-data/project.tar.bz2")]
    #[test_case("test-data/project.tar.gz")]
    #[test_case("test-data/project.tar.xz")]
    #[test_case("test-data/project.zip")]
    fn archive_installer(archive_path: &str) -> Result<()> {
        crate::test_case::init_logging();

        let td = tempdir()?;
        let mut path_without_subdir = td.path().to_path_buf();
        path_without_subdir.push("project");
        let mut path_with_subdir = td.path().to_path_buf();
        path_with_subdir.extend(&["subdir", "project"]);

        for install_root in [path_without_subdir, path_with_subdir] {
            let installer = ArchiveInstaller::new(install_root.clone());
            installer.install(&Download {
                // It doesn't matter what we use here. We're not actually going to
                // put anything in this temp dir.
                _temp_dir: tempdir()?,
                archive_path: PathBuf::from(archive_path),
            })?;

            assert!(install_root.exists());
            assert!(install_root.is_dir());

            let bin_dir = install_root.join("bin");
            assert!(bin_dir.exists());
            assert!(bin_dir.is_dir());

            let exe = bin_dir.join("project");
            assert!(exe.exists());
            assert!(exe.is_file());
        }

        Ok(())
    }

    // This tests a bug in the initial implementation where a tarball that just contained files
    // caused us to try to move its contents up to a directory that didn't exist.
    #[test]
    fn archive_installer_one_file_in_archive_root() -> Result<()> {
        let td = tempdir()?;
        let mut path_without_subdir = td.path().to_path_buf();
        path_without_subdir.push("project");
        let mut path_with_subdir = td.path().to_path_buf();
        path_with_subdir.extend(&["subdir", "project"]);

        for install_root in [path_without_subdir, path_with_subdir] {
            let installer = ArchiveInstaller::new(install_root.clone());
            installer.install(&Download {
                // It doesn't matter what we use here. We're not actually going to
                // put anything in this temp dir.
                _temp_dir: tempdir()?,
                archive_path: PathBuf::from("test-data/project-with-one-file.tar.gz"),
            })?;

            assert!(install_root.exists());
            assert!(install_root.is_dir());

            let exe = install_root.join("project");
            assert!(exe.exists());
            assert!(exe.is_file());
        }

        Ok(())
    }

    #[test]
    fn archive_installer_no_root_path() -> Result<()> {
        let td = tempdir()?;
        let mut path_without_subdir = td.path().to_path_buf();
        path_without_subdir.push("project");
        let mut path_with_subdir = td.path().to_path_buf();
        path_with_subdir.extend(&["subdir", "project"]);

        for install_root in [path_without_subdir, path_with_subdir] {
            let installer = ArchiveInstaller::new(install_root.clone());
            installer.install(&Download {
                // It doesn't matter what we use here. We're not actually going to
                // put anything in this temp dir.
                _temp_dir: tempdir()?,
                archive_path: PathBuf::from("test-data/no-shared-root.tar.gz"),
            })?;

            assert!(install_root.exists());
            assert!(install_root.is_dir());

            let bin_dir = install_root.join("bin");
            assert!(bin_dir.exists());
            assert!(bin_dir.is_dir());

            let exe = bin_dir.join("project");
            assert!(exe.exists());
            assert!(exe.is_file());

            let readme = install_root.join("README.md");
            assert!(readme.exists());
            assert!(readme.is_file());
        }

        Ok(())
    }
}
