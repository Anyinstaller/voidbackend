use anyhow::{bail, Context, Result};
use crc32fast::Hasher;
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use zip::CompressionMethod;

use crate::path;
use crate::winutil;

pub const MAX_ZIP_ENTRIES: usize = 100_000;
pub const MAX_TOTAL_UNCOMPRESSED: u64 = 8 * 1024 * 1024 * 1024;
pub const MAX_FILE_UNCOMPRESSED: u64 = 4 * 1024 * 1024 * 1024;
pub const MAX_COMPRESSION_RATIO: u64 = 100;
pub const MAX_PAYLOAD_BYTES: u64 = 8 * 1024 * 1024 * 1024;

const READ_BUFFER: usize = 1024 * 1024;
const EOCD_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
const ZIP64_EOCD_SIGNATURE: &[u8; 4] = b"PK\x06\x06";
const ZIP64_LOCATOR_SIGNATURE: &[u8; 4] = b"PK\x06\x07";
const EOCD_LEN: usize = 22;
const MAX_COMMENT: usize = u16::MAX as usize;
const MAX_EOCD_SEARCH: usize = EOCD_LEN + MAX_COMMENT + 20;
const DOS_DIRECTORY: u32 = 0x10;
const DOS_REPARSE_POINT: u32 = 0x400;
const DOS_ATTRIBUTES_MASK: u32 = 0xFFFF;
const UNIX_FILE_TYPE_MASK: u32 = 0o170000;
const UNIX_DIRECTORY: u32 = 0o040000;
const UNIX_REGULAR: u32 = 0o100000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayloadContents {
    pub files: usize,
    pub total_size: u64,
    pub executables: Vec<PathBuf>,
}

/// Securely extract `payload.zip` into an already-created, empty private
/// staging directory. The caller owns cleanup when this function fails.
pub fn extract(payload_path: &Path, stage: &Path) -> Result<PayloadContents> {
    extract_with_progress(payload_path, stage, |_, _| {})
}

pub fn extract_with_progress<F>(
    payload_path: &Path,
    stage: &Path,
    mut progress: F,
) -> Result<PayloadContents>
where
    F: FnMut(u64, u64),
{
    winutil::require_regular_file(payload_path, "payload.zip")?;
    winutil::require_directory(stage, "staging directory")?;
    let mut file = File::open(payload_path).context("failed to open payload.zip")?;
    let metadata = file
        .metadata()
        .context("failed to inspect the opened payload.zip")?;
    if metadata.len() == 0 {
        bail!("payload.zip is empty");
    }
    if metadata.len() > MAX_PAYLOAD_BYTES {
        bail!("payload.zip is larger than {MAX_PAYLOAD_BYTES} bytes");
    }

    check_entry_count(&mut file, metadata.len())?;
    file.seek(SeekFrom::Start(0))?;
    let mut archive =
        zip::ZipArchive::new(file).context("payload.zip is not a valid ZIP archive")?;
    if archive.is_empty() {
        bail!("payload.zip must not be empty");
    }
    if archive.len() > MAX_ZIP_ENTRIES {
        bail!("payload.zip contains more than {MAX_ZIP_ENTRIES} entries");
    }

    let mut predicted_total = 0u64;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .with_context(|| format!("failed to read ZIP entry {index}"))?;
        predicted_total = predicted_total
            .checked_add(entry.size())
            .context("payload.zip uncompressed size overflows")?;
        if predicted_total > MAX_TOTAL_UNCOMPRESSED {
            bail!("payload.zip expands to more than {MAX_TOTAL_UNCOMPRESSED} bytes");
        }
    }
    progress(0, predicted_total);

    let canonical_stage = std::fs::canonicalize(stage)
        .with_context(|| format!("failed to resolve staging directory {}", stage.display()))?;
    let mut files = HashSet::new();
    let mut file_ancestors = HashSet::new();
    let mut directories = HashSet::new();
    let mut total_size = 0u64;
    let mut file_count = 0usize;
    let mut extracted_bytes = 0u64;
    let mut buffer = vec![0u8; READ_BUFFER];

    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("failed to read ZIP entry {index}"))?;
        let raw_name = entry.name().context("ZIP entry name is not valid UTF-8")?;
        if entry.encrypted() {
            bail!("payload.zip contains an encrypted entry: {raw_name}");
        }
        if raw_name.contains('\\') {
            bail!("payload.zip entry uses a backslash: {raw_name:?}");
        }
        if entry.is_symlink() {
            bail!("payload.zip contains a symbolic link: {raw_name:?}");
        }
        if entry.external_attributes() & DOS_ATTRIBUTES_MASK & DOS_REPARSE_POINT != 0 {
            bail!("payload.zip contains a reparse point: {raw_name:?}");
        }

        let unix_mode = entry.unix_mode();
        let is_directory = entry.is_dir()
            || unix_mode.is_some_and(|mode| mode & UNIX_FILE_TYPE_MASK == UNIX_DIRECTORY)
            || entry.external_attributes() & DOS_DIRECTORY != 0;

        if is_directory {
            let Some(trimmed) = raw_name.strip_suffix('/') else {
                bail!("payload.zip directory entry is malformed: {raw_name:?}");
            };
            if trimmed.is_empty() || trimmed.ends_with('/') {
                bail!("payload.zip contains an empty directory name");
            }
            if entry.size() != 0 || entry.compressed_size() != 0 {
                bail!("payload.zip directory entry has data: {raw_name:?}");
            }
            let normalized =
                path::validate_package_relative(trimmed).context("invalid ZIP directory path")?;
            if normalized != trimmed {
                bail!("ZIP paths must use '/' separators: {raw_name:?}");
            }
            let key = normalized.to_lowercase();
            if files.contains(&key) {
                bail!("payload.zip has both a file and a directory named {normalized:?}");
            }
            validate_directory_ancestors(&normalized, &files)?;
            directories.insert(key);
            create_safe_directory(stage, &canonical_stage, &normalized)?;
            continue;
        }

        if let Some(mode) = unix_mode {
            let file_type = mode & UNIX_FILE_TYPE_MASK;
            if file_type != 0 && file_type != UNIX_REGULAR {
                bail!(
                    "payload.zip contains an unsupported file type ({file_type:#o}): {raw_name:?}"
                );
            }
        }

        let normalized =
            path::validate_package_relative(raw_name.as_ref()).context("invalid ZIP entry path")?;
        if normalized != raw_name.as_ref() {
            bail!("ZIP paths must use '/' separators: {raw_name:?}");
        }
        let size = entry.size();
        let compressed = entry.compressed_size();
        if size > MAX_FILE_UNCOMPRESSED {
            bail!("payload.zip entry {normalized} is larger than {MAX_FILE_UNCOMPRESSED} bytes");
        }
        if compressed == 0 && size > 0 {
            bail!("payload.zip entry {normalized} has no compressed data");
        }
        if size > compressed.saturating_mul(MAX_COMPRESSION_RATIO) {
            bail!("payload.zip entry {normalized} exceeds the compression ratio limit");
        }
        total_size = total_size
            .checked_add(size)
            .context("payload.zip uncompressed size overflows")?;
        if total_size > MAX_TOTAL_UNCOMPRESSED {
            bail!("payload.zip expands to more than {MAX_TOTAL_UNCOMPRESSED} bytes");
        }

        match entry.compression() {
            CompressionMethod::Stored => {
                if compressed != size {
                    bail!("payload.zip stored entry {normalized} has inconsistent sizes");
                }
            }
            CompressionMethod::Deflated => {}
            other => bail!(
                "payload.zip entry {normalized} uses an unsupported compression method: {other:?}"
            ),
        }

        let key = normalized.to_lowercase();
        if !files.insert(key.clone()) {
            bail!("payload.zip contains duplicate entry: {normalized}");
        }
        if directories.contains(&key) {
            bail!("payload.zip has both a directory and a file named {normalized:?}");
        }
        validate_ancestors(&normalized, &files, &mut file_ancestors)?;

        let destination = create_safe_parent(stage, &canonical_stage, &normalized)?;
        if std::fs::symlink_metadata(&destination).is_ok() {
            bail!("payload.zip entry already exists in staging: {normalized}");
        }
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
            .with_context(|| format!("failed to create staged file {}", destination.display()))?;

        let mut hasher = Hasher::new();
        let mut actual = 0u64;
        loop {
            let count = entry
                .read(&mut buffer)
                .with_context(|| format!("failed to decompress {normalized}"))?;
            if count == 0 {
                break;
            }
            actual = actual
                .checked_add(count as u64)
                .context("payload.zip entry size overflows")?;
            if actual > size {
                bail!("payload.zip entry {normalized} expands beyond its declared size");
            }
            hasher.update(&buffer[..count]);
            output
                .write_all(&buffer[..count])
                .with_context(|| format!("failed to write staged file {normalized}"))?;
            extracted_bytes = extracted_bytes
                .checked_add(count as u64)
                .context("payload.zip extraction size overflows")?;
            progress(extracted_bytes.min(predicted_total), predicted_total);
        }
        if actual != size {
            bail!("payload.zip entry {normalized} is shorter than its declared size");
        }
        if hasher.finalize() != entry.crc32() {
            bail!("payload.zip entry {normalized} failed its CRC check");
        }
        output
            .flush()
            .with_context(|| format!("failed to flush staged file {normalized}"))?;
        output
            .sync_all()
            .with_context(|| format!("failed to sync staged file {normalized}"))?;
        file_count += 1;
    }

    for directory in &directories {
        let mut ancestor = String::new();
        for component in directory.split('/') {
            if !ancestor.is_empty() {
                ancestor.push('/');
            }
            ancestor.push_str(component);
            if files.contains(&ancestor.to_lowercase()) {
                bail!("payload.zip directory {directory:?} is nested under a file");
            }
        }
    }

    if file_count == 0 {
        bail!("payload.zip contains no files");
    }

    progress(predicted_total, predicted_total);
    let contents = verify_tree(stage)?;
    if contents.files != file_count {
        bail!(
            "payload.zip declared {file_count} file(s) but {} were extracted",
            contents.files
        );
    }
    if contents.total_size != total_size {
        bail!(
            "payload.zip declared {total_size} byte(s) but {} were extracted",
            contents.total_size
        );
    }
    Ok(contents)
}

fn validate_ancestors(
    normalized: &str,
    files: &HashSet<String>,
    file_ancestors: &mut HashSet<String>,
) -> Result<()> {
    if !file_ancestors.insert(normalized.to_lowercase()) {
        bail!("payload.zip entry {normalized} is both a file and a parent directory");
    }
    let mut ancestor = String::new();
    for component in normalized.split('/') {
        if !ancestor.is_empty() {
            ancestor.push('/');
        }
        ancestor.push_str(component);
        if ancestor == normalized {
            break;
        }
        let ancestor_key = ancestor.to_lowercase();
        if files.contains(&ancestor_key) {
            bail!("payload.zip entry {normalized} is nested under a file");
        }
        file_ancestors.insert(ancestor_key);
    }
    Ok(())
}

fn validate_directory_ancestors(normalized: &str, files: &HashSet<String>) -> Result<()> {
    let mut ancestor = String::new();
    for component in normalized.split('/') {
        if !ancestor.is_empty() {
            ancestor.push('/');
        }
        ancestor.push_str(component);
        if ancestor == normalized {
            break;
        }
        if files.contains(&ancestor.to_lowercase()) {
            bail!("payload.zip entry {normalized} is nested under a file");
        }
    }
    Ok(())
}

fn create_safe_parent(stage: &Path, canonical_stage: &Path, relative: &str) -> Result<PathBuf> {
    let relative_path = relative.replace('/', std::path::MAIN_SEPARATOR_STR);
    let destination = stage.join(relative_path);
    let parent = destination
        .parent()
        .context("staged file has no parent directory")?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("failed to create staged directory {}", parent.display()))?;
    winutil::ensure_no_reparse_components(parent)?;
    let canonical_parent = std::fs::canonicalize(parent)
        .with_context(|| format!("failed to resolve staged directory {}", parent.display()))?;
    if canonical_parent != canonical_stage && !canonical_parent.starts_with(canonical_stage) {
        bail!("ZIP entry escapes the staging directory: {relative}");
    }
    Ok(destination)
}

fn create_safe_directory(stage: &Path, canonical_stage: &Path, relative: &str) -> Result<()> {
    create_safe_parent(stage, canonical_stage, &format!("{relative}/placeholder"))?;
    Ok(())
}

/// Re-scan the extracted tree so lying metadata cannot hide a reparse point,
/// unsupported object or size-limit violation.
pub fn verify_tree(stage: &Path) -> Result<PayloadContents> {
    let mut files = 0usize;
    let mut entries = 0usize;
    let mut total_size = 0u64;
    let mut executables = Vec::new();
    let mut directories = vec![stage.to_path_buf()];

    while let Some(directory) = directories.pop() {
        for entry in std::fs::read_dir(&directory)
            .with_context(|| format!("failed to read staged directory {}", directory.display()))?
        {
            let entry = entry?;
            entries = entries.saturating_add(1);
            if entries > MAX_ZIP_ENTRIES {
                bail!("payload.zip contains more than {MAX_ZIP_ENTRIES} entries");
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path)
                .with_context(|| format!("failed to inspect staged path {}", path.display()))?;
            if metadata.file_attributes() & winutil::FILE_ATTRIBUTE_REPARSE_POINT_BITS != 0 {
                bail!("staged tree contains a reparse point: {}", path.display());
            }
            let relative = path
                .strip_prefix(stage)
                .with_context(|| format!("staged path escaped its root: {}", path.display()))?;
            let relative = relative
                .to_str()
                .context("staged path is not valid UTF-8")?
                .replace('\\', "/");
            let normalized = path::validate_package_relative(&relative)
                .context("staged tree contains an invalid relative path")?;
            if normalized != relative {
                bail!("staged tree uses a non-canonical relative path: {relative:?}");
            }
            if metadata.is_dir() {
                directories.push(path);
                continue;
            }
            if !metadata.is_file() {
                bail!(
                    "staged tree contains an unsupported object: {}",
                    path.display()
                );
            }
            if metadata.len() > MAX_FILE_UNCOMPRESSED {
                bail!("staged file is larger than {MAX_FILE_UNCOMPRESSED} bytes: {relative}");
            }
            total_size = total_size
                .checked_add(metadata.len())
                .context("staged tree size overflows")?;
            if total_size > MAX_TOTAL_UNCOMPRESSED {
                bail!("staged tree expands to more than {MAX_TOTAL_UNCOMPRESSED} bytes");
            }
            files += 1;
            if path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
            {
                executables.push(PathBuf::from(relative));
            }
        }
    }

    if files == 0 {
        bail!("payload.zip contains no files");
    }
    Ok(PayloadContents {
        files,
        total_size,
        executables,
    })
}
/// Read the end-of-central-directory record before zip allocates a map for
/// every entry, so a malicious central directory cannot force unbounded memory.
fn check_entry_count(file: &mut File, file_len: u64) -> Result<()> {
    if file_len < EOCD_LEN as u64 {
        bail!("payload.zip is not a valid ZIP archive");
    }
    let window_len = file_len.min(MAX_EOCD_SEARCH as u64) as usize;
    let window_start = file_len - window_len as u64;
    file.seek(SeekFrom::Start(window_start))?;
    let mut window = vec![0u8; window_len];
    file.read_exact(&mut window)
        .context("failed to read payload.zip end record")?;

    let mut eocd = None;
    for position in (0..=window.len() - EOCD_LEN).rev() {
        if &window[position..position + 4] != EOCD_SIGNATURE {
            continue;
        }
        let comment_len =
            u16::from_le_bytes(window[position + 20..position + 22].try_into().unwrap()) as usize;
        if position + EOCD_LEN + comment_len <= window.len() {
            eocd = Some(position);
            break;
        }
    }
    let Some(eocd) = eocd else {
        bail!("payload.zip has no end-of-central-directory record");
    };
    let total = u16::from_le_bytes(window[eocd + 10..eocd + 12].try_into().unwrap());
    let directory_size = u32::from_le_bytes(window[eocd + 12..eocd + 16].try_into().unwrap());
    let directory_offset = u32::from_le_bytes(window[eocd + 16..eocd + 20].try_into().unwrap());
    if directory_size != u32::MAX && directory_offset != u32::MAX {
        if total as usize > MAX_ZIP_ENTRIES {
            bail!("payload.zip contains more than {MAX_ZIP_ENTRIES} entries");
        }
        return Ok(());
    }

    if eocd < 20 || &window[eocd - 20..eocd - 16] != ZIP64_LOCATOR_SIGNATURE {
        bail!("payload.zip has an invalid ZIP64 locator");
    }
    let locator = eocd - 20;
    let zip64_offset = u64::from_le_bytes(window[locator + 8..locator + 16].try_into().unwrap());
    let Some(zip64_end) = zip64_offset.checked_add(56) else {
        bail!("payload.zip ZIP64 end record overflows");
    };
    if zip64_end > file_len {
        bail!("payload.zip ZIP64 end record is outside the file");
    }
    file.seek(SeekFrom::Start(zip64_offset))?;
    let mut header = [0u8; 56];
    file.read_exact(&mut header)
        .context("failed to read ZIP64 end record")?;
    if &header[0..4] != ZIP64_EOCD_SIGNATURE {
        bail!("payload.zip has an invalid ZIP64 end record");
    }
    let total = u64::from_le_bytes(header[32..40].try_into().unwrap());
    if total > MAX_ZIP_ENTRIES as u64 {
        bail!("payload.zip contains more than {MAX_ZIP_ENTRIES} entries");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use zip::write::SimpleFileOptions;

    fn write_zip(dir: &tempfile::TempDir, files: &[(&str, &[u8], CompressionMethod)]) -> PathBuf {
        write_zip_with_options(dir, files, SimpleFileOptions::default())
    }

    fn write_zip_with_options(
        dir: &tempfile::TempDir,
        files: &[(&str, &[u8], CompressionMethod)],
        base: SimpleFileOptions,
    ) -> PathBuf {
        let path = dir.path().join("payload.zip");
        let file = File::create(&path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        for (name, bytes, method) in files {
            writer
                .start_file(*name, base.compression_method(*method))
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
        path
    }

    #[test]
    fn extracts_valid_payload_and_finds_executable() {
        let dir = tempfile::tempdir().unwrap();
        let payload = write_zip(
            &dir,
            &[
                ("Example.exe", b"binary", CompressionMethod::Stored),
                ("data/file.txt", b"hello", CompressionMethod::Deflated),
            ],
        );
        let stage = dir.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        let contents = extract(&payload, &stage).unwrap();
        assert_eq!(contents.files, 2);
        assert_eq!(contents.total_size, 11);
        assert_eq!(contents.executables, vec![PathBuf::from("Example.exe")]);
        assert_eq!(
            std::fs::read(stage.join("data/file.txt")).unwrap(),
            b"hello"
        );
    }

    #[test]
    fn accepts_explicit_parent_directory_after_child_file() {
        let dir = tempfile::tempdir().unwrap();
        let payload = dir.path().join("payload.zip");
        let file = File::create(&payload).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file(
                "a/file.txt",
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .unwrap();
        writer.write_all(b"data").unwrap();
        writer
            .add_directory("a/", SimpleFileOptions::default())
            .unwrap();
        writer.finish().unwrap();

        let stage = dir.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        let contents = extract(&payload, &stage).unwrap();
        assert_eq!(contents.files, 1);
        assert_eq!(contents.total_size, 4);
    }

    #[test]
    fn rejects_traversal_and_duplicate_paths() {
        let dir = tempfile::tempdir().unwrap();
        let payload = write_zip(
            &dir,
            &[
                ("../escape.txt", b"x", CompressionMethod::Stored),
                ("A.txt", b"x", CompressionMethod::Stored),
                ("a.txt", b"y", CompressionMethod::Stored),
            ],
        );
        let stage = dir.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        assert!(extract(&payload, &stage).is_err());
    }

    #[test]
    fn rejects_compression_bomb_ratio() {
        let dir = tempfile::tempdir().unwrap();
        let zeros = vec![0u8; 200_000];
        let payload = write_zip(
            &dir,
            &[("bomb.bin", zeros.as_slice(), CompressionMethod::Deflated)],
        );
        let stage = dir.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        assert!(extract(&payload, &stage).is_err());
    }

    #[test]
    fn rejects_symlink_entries() {
        let dir = tempfile::tempdir().unwrap();
        let payload = write_zip_with_options(
            &dir,
            &[("link", b"target", CompressionMethod::Stored)],
            SimpleFileOptions::default()
                .system(zip::System::Unix)
                .external_attributes(0o120777 << 16),
        );
        let stage = dir.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        assert!(extract(&payload, &stage).is_err());
    }

    #[test]
    fn rejects_reparse_point_attributes() {
        let dir = tempfile::tempdir().unwrap();
        let payload = write_zip_with_options(
            &dir,
            &[("link", b"target", CompressionMethod::Stored)],
            SimpleFileOptions::default().external_attributes(0x400),
        );
        let stage = dir.path().join("stage");
        std::fs::create_dir(&stage).unwrap();
        assert!(extract(&payload, &stage).is_err());
    }
}
