use super::{
    Pressure,
    files::{ArchiveEntry, Dir, archive_name, digest, stream},
};
use crate::{Error, Result, Safe, shared};
use rustix::fs::{RenameFlags, renameat_with};
use std::{
    collections::BTreeMap,
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

/// Preserve raw orphan sidecars even when an existing archive has an online
/// snapshot of the database. The snapshot may precede the orphan's commits.
pub(super) fn preserve<'a>(
    path: &Path,
    view: &Dir,
    entries: impl Iterator<Item = ArchiveEntry<'a>>,
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
        Ok(dir) => {
            let seal = read_seal(&dir, true, pressure)?;
            Some((dir, seal))
        }
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(_) => return Err(Error("unsafe or unreadable existing archive")),
    };

    let mut staging: Option<Staging> = None;
    for entry in entries {
        let mut file = payload(&entry, view, &parent, pressure)?;
        let length = file.metadata().safe("cannot inspect archive source")?.len();
        let expected = (digest(&mut file, pressure)?, length);
        let plain = archive_name(&entry.file.path);
        let relative = if entry.orphan_sidecar {
            if let Some((dir, seal)) = &existing
                && seal.files.contains_key(&plain)
                && payload_matches(dir, &plain, expected, pressure)?
            {
                continue;
            }
            // Preserve later versions at this relative sidecar path alongside
            // earlier protected copies, without overwriting any payload.
            Path::new("orphan-sidecars")
                .join(hex(expected.0))
                .join(plain)
        } else {
            plain
        };
        if let Some((dir, seal)) = &existing {
            if seal.files.contains_key(&relative) {
                verify_payload(dir, &relative, expected, pressure)?;
                continue;
            }
            if !entry.orphan_sidecar {
                return Err(Error("existing archive lacks a required payload"));
            }
        }
        if staging.is_none() {
            staging = Some(Staging::new(&parent, existing.as_ref(), pressure)?);
        }
        file.rewind().safe("cannot rewind archive source")?;
        staging
            .as_mut()
            .unwrap()
            .compress(&relative, file, expected, pressure)?;
    }

    let (archive, seal, bytes_added, old_generation) = match staging {
        None => {
            let (dir, seal) = existing.ok_or(Error("archive has no payloads"))?;
            (dir, seal, 0, None)
        }
        Some(stage) => stage.publish(&parent, name, existing.as_ref(), pressure)?,
    };

    // Both the original archive and its new generation are kept until the
    // exchange is durable. On failure the caller keeps the source view too.
    let committed = (|| {
        archive.0.sync_all().safe("cannot sync archive directory")?;
        for ancestor in parent_path.ancestors() {
            Dir::open(ancestor)
                .and_then(|dir| dir.0.sync_all())
                .safe("cannot sync archive parent chain")?;
        }
        let current = Dir::open(path).safe("cannot reopen published archive")?;
        let archive_identity = identity(&archive)?;
        if identity(&current)? != archive_identity {
            return Err(Error("published archive identity or manifest changed"));
        }
        Ok(Receipt {
            path: path.to_owned(),
            identity: archive_identity,
            seal,
            bytes_added,
        })
    })();
    if let Some(old) = old_generation
        && committed.is_err()
    {
        let _ = old.keep();
    }
    committed
}

struct Staging {
    temp: tempfile::TempDir,
    dir: Dir,
    payloads: Payloads,
}

type Published = (Dir, Seal, u64, Option<tempfile::TempDir>);

impl Staging {
    fn new(
        parent: &Dir,
        existing: Option<&(Dir, Seal)>,
        pressure: &mut Pressure<'_>,
    ) -> Result<Self> {
        let temp = tempfile::Builder::new()
            .prefix(".aswap-archive-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(parent.path())
            .safe("cannot stage archive")?;
        let name = temp
            .path()
            .file_name()
            .ok_or(Error("missing staged archive name"))?;
        let dir = parent.child(name).safe("cannot open staged archive")?;
        if let Some((previous, seal)) = existing {
            for path in seal.files.keys() {
                pressure.check(false)?;
                let (from, source) = previous
                    .parent(path, false)
                    .safe("cannot locate existing archive payload")?;
                let (to, target) = dir
                    .parent(path, true)
                    .safe("cannot stage existing archive payload")?;
                fs::hard_link(from.path().join(source), to.path().join(target))
                    .safe("cannot retain existing archive payload")?;
            }
        }
        Ok(Self {
            temp,
            dir,
            payloads: Payloads::new(),
        })
    }

    fn compress(
        &mut self,
        relative: &Path,
        file: File,
        expected: ([u8; 32], u64),
        pressure: &mut Pressure<'_>,
    ) -> Result<()> {
        let (dir, name) = self
            .dir
            .parent(relative, true)
            .safe("cannot create archive payload directory")?;
        let output = private_file(&dir.path().join(name))?;
        let mut encoder =
            zstd::stream::write::Encoder::new(output, 3).safe("cannot start zstd compression")?;
        if stream(file, &mut encoder, pressure)? != expected.1 {
            return Err(Error("archive source changed during compression"));
        }
        encoder
            .finish()
            .safe("cannot finish zstd compression")?
            .sync_all()
            .safe("cannot sync archived file")?;
        self.payloads.insert(relative.to_owned(), expected);
        Ok(())
    }

    fn publish(
        self,
        parent: &Dir,
        name: &OsStr,
        existing: Option<&(Dir, Seal)>,
        pressure: &mut Pressure<'_>,
    ) -> Result<Published> {
        let files = archived_files(&self.dir, false, pressure)?;
        let manifest = manifest_for(&files);
        let bytes = total_bytes(&files, &manifest);
        let mut file = private_file(&self.dir.path().join(MANIFEST))?;
        file.write_all(&manifest)
            .safe("cannot write archive checksum manifest")?;
        file.sync_all()
            .safe("cannot sync archive checksum manifest")?;
        let seal = read_seal(&self.dir, true, pressure)?;
        for (relative, &expected) in &self.payloads {
            verify_payload(&self.dir, relative, expected, pressure)?;
        }

        let temp_name = self
            .temp
            .path()
            .file_name()
            .ok_or(Error("missing staged archive name"))?;
        let (flags, bytes_added) = if let Some((previous, old)) = existing {
            let named = parent
                .child(name)
                .safe("cannot recheck archive before exchange")?;
            if identity(&named)? != identity(previous)?
                || read_seal(&named, false, pressure)? != *old
                || old
                    .files
                    .iter()
                    .any(|(path, file)| seal.files.get(path) != Some(file))
            {
                return Err(Error("existing archive changed before exchange"));
            }
            let previous_bytes = total_bytes(&old.files, &manifest_for(&old.files));
            (RenameFlags::EXCHANGE, bytes.saturating_sub(previous_bytes))
        } else {
            (RenameFlags::NOREPLACE, bytes)
        };
        renameat_with(&parent.0, temp_name, &parent.0, name, flags)
            .safe("archive appeared or could not be published")?;
        let old_generation = if existing.is_some() {
            Some(self.temp)
        } else {
            let _ = self.temp.keep();
            None
        };
        Ok((self.dir, seal, bytes_added, old_generation))
    }
}

fn total_bytes(files: &ArchivedFiles, manifest: &[u8]) -> u64 {
    files.values().map(|file| file.bytes).sum::<u64>() + manifest.len() as u64
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

fn payload_matches(
    archive: &Dir,
    relative: &Path,
    expected: ([u8; 32], u64),
    pressure: &mut Pressure<'_>,
) -> Result<bool> {
    let (file, _) = archive
        .relative_file(relative)
        .safe("cannot read archived file")?;
    let decoded = zstd::stream::read::Decoder::new(file).safe("cannot decode archived file")?;
    Ok(digest(decoded.take(expected.1.saturating_add(1)), pressure)? == expected.0)
}

fn verify_payload(
    archive: &Dir,
    relative: &Path,
    expected: ([u8; 32], u64),
    pressure: &mut Pressure<'_>,
) -> Result<()> {
    if !payload_matches(archive, relative, expected, pressure)? {
        return Err(Error("archived file differs from original"));
    }
    Ok(())
}

fn hex(digest: [u8; 32]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut text = String::with_capacity(64);
    for byte in digest {
        text.push(char::from(HEX[(byte >> 4) as usize]));
        text.push(char::from(HEX[(byte & 15) as usize]));
    }
    text
}

fn append_sum(manifest: &mut Vec<u8>, path: &Path, digest: [u8; 32]) {
    let path = path.as_os_str().as_bytes();
    // GNU sha256sum format, including its filename escaping rules.
    if path.iter().any(|b| matches!(b, b'\\' | b'\n' | b'\r')) {
        manifest.push(b'\\');
    }
    manifest.extend_from_slice(hex(digest).as_bytes());
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
