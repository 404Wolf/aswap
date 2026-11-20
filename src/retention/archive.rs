use super::{
    Pressure,
    files::{ArchiveEntry, Dir, archive_name, digest, is_sqlite_header, stream},
};
use crate::{Error, Result, Safe, shared};
use rustix::fs::{RenameFlags, renameat_with};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{ErrorKind, Read, Seek, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

const MANIFEST: &str = "SHA256SUMS";
type Payloads = BTreeMap<PathBuf, ([u8; 32], u64)>;

#[derive(Eq, PartialEq)]
struct ArchivedFile {
    identity: (u64, u64),
    hash: [u8; 32],
    bytes: u64,
}

type ArchivedFiles = BTreeMap<PathBuf, ArchivedFile>;

#[derive(Eq, PartialEq)]
struct Seal {
    manifest: [u8; 32],
    files: ArchivedFiles,
}

pub(super) struct Receipt {
    path: PathBuf,
    identity: (u64, u64),
    seal: Seal,
    pub(super) bytes_added: u64,
}

impl Receipt {
    pub(super) fn recheck(&self, pressure: &mut Pressure<'_>) -> Result<()> {
        let dir = Dir::open(&self.path).safe("cannot recheck published archive directory")?;
        if identity(&dir)? != self.identity {
            return Err(Error("published archive identity or manifest changed"));
        }
        if read_seal(&dir, false, pressure)? != self.seal {
            return Err(Error("published archive payload identity changed"));
        }
        Ok(())
    }
}

pub(super) fn verify_orphans(
    path: &Path,
    orphans: &BTreeSet<PathBuf>,
    pressure: &mut Pressure<'_>,
) -> Result<()> {
    if orphans.is_empty() {
        return Ok(());
    }
    let archive = Dir::open(path).safe("orphan SQLite sidecar has no verified archive snapshot")?;
    let seal = read_seal(&archive, false, pressure)?;
    check_orphans(&archive, &seal, orphans)
}

fn check_orphans(archive: &Dir, seal: &Seal, orphans: &BTreeSet<PathBuf>) -> Result<()> {
    for database in orphans {
        let relative = archive_name(database);
        if !seal.files.contains_key(&relative) {
            return Err(Error(
                "orphan SQLite sidecar has no verified archive snapshot",
            ));
        }
        let (file, _) = archive
            .relative_file(&relative)
            .safe("cannot open orphan SQLite archive snapshot")?;
        let mut decoded = zstd::stream::read::Decoder::new(file)
            .safe("cannot decode orphan SQLite archive snapshot")?;
        let mut header = [0; 16];
        decoded
            .read_exact(&mut header)
            .safe("cannot read archived SQLite header")?;
        if !is_sqlite_header(&header) {
            return Err(Error(
                "orphan SQLite sidecar has no verified archive snapshot",
            ));
        }
    }
    Ok(())
}

/// Seal verified contents in a private directory before publishing the name.
/// A retry verifies the complete archive and compares surviving sources.
pub(super) fn preserve<'a>(
    path: &Path,
    view: &Dir,
    entries: impl Iterator<Item = ArchiveEntry<'a>>,
    orphans: &BTreeSet<PathBuf>,
    pressure: &mut Pressure<'_>,
) -> Result<Receipt> {
    let parent_path = path.parent().ok_or(Error("missing archive parent"))?;
    let name = path.file_name().ok_or(Error("missing archive name"))?;
    let root = Dir::open(Path::new("/")).safe("cannot open archive filesystem")?;
    let parent = root
        .descend(
            parent_path
                .strip_prefix("/")
                .safe("archive must be absolute")?,
            true,
        )
        .safe("cannot create archive directory")?;
    let existing = match parent.child(name) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(_) => return Err(Error("unsafe or unreadable existing archive")),
    };
    let mut payloads = Payloads::new();
    let (archive, bytes_added, seal) = if let Some(archive) = existing {
        for entry in entries {
            let file = payload(&entry, view, &parent, pressure)?;
            let length = file.metadata().safe("cannot inspect archive source")?.len();
            payloads.insert(
                archive_name(&entry.file.path),
                (digest(file, pressure)?, length),
            );
        }
        let seal = verify(&archive, &payloads, pressure)?;
        check_orphans(&archive, &seal, orphans)?;
        (archive, 0, seal)
    } else {
        if !orphans.is_empty() {
            return Err(Error(
                "orphan SQLite sidecar has no verified archive snapshot",
            ));
        }
        let temp = tempfile::Builder::new()
            .prefix(".aswap-archive-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(parent.path())
            .safe("cannot stage archive")?;
        let temp_name = temp
            .path()
            .file_name()
            .ok_or(Error("missing staged archive name"))?;
        let staging = parent.child(temp_name).safe("cannot open staged archive")?;
        for entry in entries {
            let relative = archive_name(&entry.file.path);
            let (dir, name) = staging
                .parent(&relative, true)
                .safe("cannot create archive payload directory")?;
            let mut file = payload(&entry, view, &parent, pressure)?;
            let original = digest(&mut file, pressure)?;
            file.rewind().safe("cannot rewind archive source")?;
            let output = private_file(&dir.path().join(name))?;
            let mut encoder = zstd::stream::write::Encoder::new(output, 3)
                .safe("cannot start zstd compression")?;
            let length = stream(&mut file, &mut encoder, pressure)?;
            encoder
                .finish()
                .safe("cannot finish zstd compression")?
                .sync_all()
                .safe("cannot sync archived file")?;
            payloads.insert(relative, (original, length));
        }
        let files = archived_files(&staging, false, pressure)?;
        let bytes: u64 = files.values().map(|file| file.bytes).sum();
        let manifest = manifest_for(&files);
        let mut file = private_file(&staging.path().join(MANIFEST))?;
        file.write_all(&manifest)
            .safe("cannot write archive checksum manifest")?;
        file.sync_all()
            .safe("cannot sync archive checksum manifest")?;
        let seal = verify(&staging, &payloads, pressure)?;
        renameat_with(
            &parent.0,
            temp_name,
            &parent.0,
            name,
            RenameFlags::NOREPLACE,
        )
        .safe("archive appeared or could not be published")?;
        let _ = temp.keep();
        (staging, bytes + manifest.len() as u64, seal)
    };

    // Retry this even when the archive already exists. A prior rename may
    // have succeeded before its parent fsync failed.
    archive.0.sync_all().safe("cannot sync archive directory")?;
    for ancestor in parent_path.ancestors() {
        Dir::open(ancestor)
            .and_then(|dir| dir.0.sync_all())
            .safe("cannot sync archive parent chain")?;
    }
    let receipt = Receipt {
        path: path.to_owned(),
        identity: identity(&archive)?,
        seal,
        bytes_added,
    };
    let current = Dir::open(path).safe("cannot reopen published archive")?;
    if identity(&current)? != receipt.identity {
        return Err(Error("published archive identity or manifest changed"));
    }
    Ok(receipt)
}

fn private_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .safe("cannot create private archive file")
}

fn payload(
    entry: &ArchiveEntry<'_>,
    view: &Dir,
    parent: &Dir,
    pressure: &mut Pressure<'_>,
) -> Result<File> {
    let source = entry.file.open(view)?;
    if !entry.sqlite {
        return Ok(source);
    }

    // SQLite can create or update sidecars when opening even a read-only DB.
    // Feed it stable, private scratch copies of the DB and its WAL/journal
    // so the original tree stays untouched. Only the online snapshot is sealed.
    let scratch = tempfile::Builder::new()
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir_in(parent.path())
        .safe("cannot create private SQLite scratch directory")?;
    let name = entry
        .file
        .path
        .file_name()
        .ok_or(Error("missing SQLite database name"))?;
    let input = scratch.path().join(name);
    copy_input(source, entry.file.size(), &input, pressure)?;
    for sidecar in &entry.sidecars {
        let path = scratch.path().join(
            sidecar
                .path
                .file_name()
                .ok_or(Error("missing SQLite sidecar name"))?,
        );
        copy_input(sidecar.open(view)?, sidecar.size(), &path, pressure)?;
        sidecar.open(view)?;
    }
    entry.file.open(view)?;
    let db =
        rusqlite::Connection::open(&input).safe("cannot open private SQLite snapshot source")?;
    let snapshot = tempfile::NamedTempFile::new_in(scratch.path())
        .safe("cannot stage private SQLite snapshot")?;
    shared::sqlite_snapshot_from(&db, snapshot.path(), || pressure.check(false))?;
    snapshot
        .reopen()
        .safe("cannot read private SQLite snapshot")
}

fn copy_input(source: File, length: u64, dest: &Path, pressure: &mut Pressure<'_>) -> Result<()> {
    if stream(source, private_file(dest)?, pressure)? != length {
        return Err(Error("view SQLite input changed during copy"));
    }
    Ok(())
}

fn identity(dir: &Dir) -> Result<(u64, u64)> {
    let m = dir.0.metadata().safe("cannot inspect archive directory")?;
    Ok((m.dev(), m.ino()))
}

fn manifest_for(files: &ArchivedFiles) -> Vec<u8> {
    let mut manifest = Vec::new();
    for (path, file) in files {
        append_sum(&mut manifest, path, file.hash);
    }
    manifest
}

fn archived_files(dir: &Dir, sync: bool, pressure: &mut Pressure<'_>) -> Result<ArchivedFiles> {
    let mut files = BTreeMap::new();
    inventory(dir, Path::new(""), &mut files, sync, pressure)?;
    Ok(files)
}

fn inventory(
    dir: &Dir,
    relative: &Path,
    files: &mut ArchivedFiles,
    sync: bool,
    pressure: &mut Pressure<'_>,
) -> Result<()> {
    if relative.components().count() > 128
        || dir
            .0
            .metadata()
            .safe("cannot inspect archive directory")?
            .mode()
            & 0o777
            != 0o700
    {
        return Err(Error("unsafe archive directory"));
    }
    for name in dir.names().safe("cannot list archive")? {
        pressure.check(false)?;
        if relative.as_os_str().is_empty() && name == MANIFEST {
            continue;
        }
        let m =
            fs::symlink_metadata(dir.path().join(&name)).safe("cannot inspect archive entry")?;
        let path = relative.join(&name);
        if m.is_dir() {
            let child = dir
                .contained_child(&name)
                .safe("cannot open archive directory")?;
            let before = files.len();
            inventory(&child, &path, files, sync, pressure)?;
            if before == files.len() {
                return Err(Error("unexpected empty archive directory"));
            }
        } else {
            let (file, m) = dir.file(&name).safe("cannot read archived file")?;
            if m.mode() & 0o777 != 0o600 || path.extension() != Some(OsStr::new("zst")) {
                return Err(Error("unsafe archive file"));
            }
            let hash = digest(&file, pressure)?;
            let after = file.metadata().safe("cannot recheck archived file")?;
            if (
                m.len(),
                m.mtime(),
                m.mtime_nsec(),
                m.ctime(),
                m.ctime_nsec(),
            ) != (
                after.len(),
                after.mtime(),
                after.mtime_nsec(),
                after.ctime(),
                after.ctime_nsec(),
            ) {
                return Err(Error("archived file changed during verification"));
            }
            files.insert(
                path,
                ArchivedFile {
                    identity: (m.dev(), m.ino()),
                    hash,
                    bytes: m.len(),
                },
            );
            if sync {
                file.sync_all().safe("cannot sync archived file")?;
            }
        }
    }
    if sync {
        dir.0.sync_all().safe("cannot sync archive directory")?;
    }
    Ok(())
}

fn read_seal(archive: &Dir, sync: bool, pressure: &mut Pressure<'_>) -> Result<Seal> {
    let files = archived_files(archive, sync, pressure)?;
    let expected = manifest_for(&files);
    let (manifest, m) = archive
        .file(OsStr::new(MANIFEST))
        .safe("cannot read archive checksum manifest")?;
    let mut recorded = Vec::new();
    (&manifest)
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut recorded)
        .safe("cannot read archive checksum manifest")?;
    if m.mode() & 0o777 != 0o600 || recorded != expected {
        return Err(Error("archive checksum manifest did not verify"));
    }
    if sync {
        manifest
            .sync_all()
            .safe("cannot sync archive checksum manifest")?;
    }
    Ok(Seal {
        manifest: digest(recorded.as_slice(), pressure)?,
        files,
    })
}

fn verify(archive: &Dir, payloads: &Payloads, pressure: &mut Pressure<'_>) -> Result<Seal> {
    let seal = read_seal(archive, true, pressure)?;
    for (relative, (original, length)) in payloads {
        let (file, _) = archive
            .relative_file(relative)
            .safe("cannot read archived file")?;
        let decoded = zstd::stream::read::Decoder::new(file).safe("cannot decode archived file")?;
        if digest(decoded.take(length.saturating_add(1)), pressure)? != *original {
            return Err(Error("archived file differs from original"));
        }
    }
    Ok(seal)
}

fn append_sum(manifest: &mut Vec<u8>, path: &Path, digest: [u8; 32]) {
    let path = path.as_os_str().as_bytes();
    // GNU sha256sum format, including its filename escaping rules.
    if path.iter().any(|b| matches!(b, b'\\' | b'\n' | b'\r')) {
        manifest.push(b'\\');
    }
    const HEX: &[u8] = b"0123456789abcdef";
    for byte in digest {
        manifest.extend_from_slice(&[HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]]);
    }
    manifest.extend_from_slice(b"  ");
    for byte in path {
        match byte {
            b'\\' => manifest.extend_from_slice(b"\\\\"),
            b'\n' => manifest.extend_from_slice(b"\\n"),
            b'\r' => manifest.extend_from_slice(b"\\r"),
            _ => manifest.push(*byte),
        }
    }
    manifest.push(b'\n');
}
