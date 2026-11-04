//! Unpacking downloaded release assets: tar.gz, tar.xz, zip, single gzip
//! files, and plain files.

use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};

/// Unpacks `file` (downloaded as `asset_name`) into the empty directory
/// `destination`. A non-archive is copied in under its asset name.
pub fn unpack(file: &Path, asset_name: &str, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    let lower = asset_name.to_lowercase();
    if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        let decoder = flate2::read::GzDecoder::new(BufReader::new(File::open(file)?));
        untar(decoder, destination)
    } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
        // Decompress to a temporary file rather than memory: some toolchains
        // are hundreds of megabytes unpacked.
        let temporary =
            tempfile::NamedTempFile::new_in(destination.parent().unwrap_or(destination))?;
        {
            let mut input = BufReader::new(File::open(file)?);
            let mut output = BufWriter::new(temporary.as_file());
            lzma_rs::xz_decompress(&mut input, &mut output)
                .map_err(|error| anyhow::anyhow!("decompressing {asset_name}: {error}"))?;
        }
        untar(File::open(temporary.path())?, destination)
    } else if lower.ends_with(".tar") {
        untar(File::open(file)?, destination)
    } else if lower.ends_with(".zip") {
        unzip(file, destination)
    } else if let Some(stem) = asset_name.strip_suffix(".gz") {
        let mut decoder = flate2::read::GzDecoder::new(BufReader::new(File::open(file)?));
        let mut output = File::create(destination.join(safe_file_name(stem)?))?;
        std::io::copy(&mut decoder, &mut output)
            .with_context(|| format!("decompressing {asset_name}"))?;
        Ok(())
    } else {
        std::fs::copy(file, destination.join(safe_file_name(asset_name)?))
            .with_context(|| format!("copying {asset_name}"))?;
        Ok(())
    }
}

fn safe_file_name(name: &str) -> Result<&str> {
    if name.is_empty() || name.contains('/') || name == "." || name == ".." {
        bail!("refusing unsafe asset file name `{name}`");
    }
    Ok(name)
}

fn untar(reader: impl std::io::Read, destination: &Path) -> Result<()> {
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);
    // `unpack` refuses entries that would land outside `destination`.
    archive
        .unpack(destination)
        .with_context(|| format!("unpacking into {}", destination.display()))
}

fn unzip(file: &Path, destination: &Path) -> Result<()> {
    let mut archive = zip::ZipArchive::new(BufReader::new(File::open(file)?))?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        // `enclosed_name` rejects absolute paths and `..` components.
        let Some(relative) = entry.enclosed_name() else {
            bail!(
                "zip entry `{}` has an unsafe path",
                entry.name().unwrap_or_default()
            );
        };
        let path = destination.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut output = File::create(&path)?;
        std::io::copy(&mut entry, &mut output)?;
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o777))?;
        }
    }
    Ok(())
}

/// The directory that holds an unpacked asset's content: the single
/// top-level directory when the archive has exactly one, else the root.
pub fn content_root(unpacked: &Path) -> Result<PathBuf> {
    let entries: Vec<PathBuf> = std::fs::read_dir(unpacked)?
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .collect();
    match entries.as_slice() {
        [only] if only.is_dir() && !only.is_symlink() => Ok(only.clone()),
        _ => Ok(unpacked.to_path_buf()),
    }
}

/// Finds a file named `name` under `root`, preferring the shallowest match
/// and, among equals, an executable one.
pub fn find_file(root: &Path, name: &str) -> Option<PathBuf> {
    let mut matches = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|file_name| file_name == name) {
                matches.push(path);
            }
        }
    }
    matches.sort_by_key(|path| {
        (
            path.components().count(),
            !crate::util::is_executable(path),
            path.clone(),
        )
    });
    matches.into_iter().next()
}

/// All regular files under `root`.
pub fn list_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => stack.push(entry.path()),
                Ok(file_type) if file_type.is_file() => files.push(entry.path()),
                _ => {}
            }
        }
    }
    files.sort();
    files
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::Write;

    /// Builds a tar.gz in `dir` holding `entries` (path, contents, mode).
    pub fn make_tar_gz(dir: &Path, name: &str, entries: &[(&str, &str, u32)]) -> PathBuf {
        let path = dir.join(name);
        let encoder = flate2::write::GzEncoder::new(
            File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut builder = tar::Builder::new(encoder);
        for (entry_path, contents, mode) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(*mode);
            header.set_cksum();
            builder
                .append_data(&mut header, entry_path, contents.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap();
        path
    }

    #[test]
    fn unpacks_tar_gz_and_strips_a_single_top_directory() {
        let dir = tempfile::tempdir().unwrap();
        let archive = make_tar_gz(
            dir.path(),
            "tool-1.0-x86_64.tar.gz",
            &[
                ("tool-1.0/tool", "#!/bin/sh\n", 0o755),
                ("tool-1.0/doc/README", "hi", 0o644),
            ],
        );
        let out = dir.path().join("out");
        unpack(&archive, "tool-1.0-x86_64.tar.gz", &out).unwrap();
        let root = content_root(&out).unwrap();
        assert!(root.ends_with("tool-1.0"));
        let found = find_file(&root, "tool").unwrap();
        assert!(crate::util::is_executable(&found));
    }

    #[test]
    fn unpacks_zip_with_modes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool.zip");
        {
            let mut writer = zip::ZipWriter::new(File::create(&path).unwrap());
            let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
            writer.start_file("bin/tool", options).unwrap();
            writer.write_all(b"binary").unwrap();
            writer.finish().unwrap();
        }
        let out = dir.path().join("out");
        unpack(&path, "tool.zip", &out).unwrap();
        let found = find_file(&out, "tool").unwrap();
        assert!(found.ends_with("bin/tool"));
        assert!(crate::util::is_executable(&found));
    }

    #[test]
    fn unpacks_tar_xz() {
        let dir = tempfile::tempdir().unwrap();
        let tar_path = dir.path().join("plain.tar");
        {
            let mut builder = tar::Builder::new(File::create(&tar_path).unwrap());
            let mut header = tar::Header::new_gnu();
            header.set_size(2);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(&mut header, "node-v1/bin/node", &b"hi"[..])
                .unwrap();
            builder.finish().unwrap();
        }
        let xz_path = dir.path().join("node.tar.xz");
        lzma_rs::xz_compress(
            &mut BufReader::new(File::open(&tar_path).unwrap()),
            &mut File::create(&xz_path).unwrap(),
        )
        .unwrap();
        let out = dir.path().join("out");
        unpack(&xz_path, "node.tar.xz", &out).unwrap();
        assert!(content_root(&out).unwrap().join("bin/node").exists());
    }

    #[test]
    fn plain_files_and_single_gzip_files() {
        let dir = tempfile::tempdir().unwrap();
        let raw = dir.path().join("download");
        std::fs::write(&raw, "wasm").unwrap();
        let out = dir.path().join("out");
        unpack(&raw, "zjstatus.wasm", &out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("zjstatus.wasm")).unwrap(),
            "wasm"
        );

        let gz = dir.path().join("gz");
        let mut encoder =
            flate2::write::GzEncoder::new(File::create(&gz).unwrap(), flate2::Compression::fast());
        encoder.write_all(b"elf").unwrap();
        encoder.finish().unwrap();
        let out = dir.path().join("out2");
        unpack(&gz, "tool-linux-amd64.gz", &out).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("tool-linux-amd64")).unwrap(),
            "elf"
        );
        assert!(unpack(&raw, "../evil", &dir.path().join("out3")).is_err());
    }
}
