use super::{
    Pressure,
    files::{Dir, Transcript, archive_name, digest, stream},
};
use crate::{Error, Result, Safe};
use rustix::fs::{RenameFlags, renameat_with};
use std::{
    ffi::OsStr,
    fs::OpenOptions,
    io::{ErrorKind, Read, Write},
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::Path,
};

const MANIFEST: &str = "SHA256SUMS";

/// A per-view archive is sealed and verified in a private staging directory.
/// Retrying after publication verifies the existing archive against the view.
pub(super) fn preserve<'a>(
    path: &Path,
    view: &Dir,
    conflicts: impl Iterator<Item = &'a Transcript> + Clone,
    pressure: &mut Pressure<'_>,
) -> Result<u64> {
    let parent = path.parent().ok_or(Error("missing archive parent"))?;
    let name = path.file_name().ok_or(Error("missing archive name"))?;
    let root = Dir::open(Path::new("/")).safe("cannot open archive filesystem")?;
    let parent = root
        .descend(
            parent.strip_prefix("/").safe("archive must be absolute")?,
            true,
        )
        .safe("cannot create archive directory")?;
    match parent.child(name) {
        Ok(existing) => {
            verify(&existing, view, conflicts, pressure)?;
            return Ok(0);
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(_) => return Err(Error("unsafe or unreadable existing archive")),
    }

    let temp = tempfile::Builder::new()
        .prefix(".aswap-archive-")
        .tempdir_in(parent.path())
        .safe("cannot stage archive")?;
    let temp_name = temp
        .path()
        .file_name()
        .ok_or(Error("missing staged archive name"))?;
    let staging = parent.child(temp_name).safe("cannot open staged archive")?;
    let mut manifest = Vec::new();
    for transcript in conflicts.clone() {
        pressure.check(false)?;
        let relative = archive_name(transcript);
        let (dir, name) = staging
            .parent(&relative, true)
            .safe("cannot create transcript archive path")?;
        let output = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(dir.path().join(name))
            .safe("cannot create archived transcript")?;
        let mut encoder =
            zstd::stream::write::Encoder::new(output, 3).safe("cannot start zstd compression")?;
        if stream(transcript.open(view)?, &mut encoder, pressure)? != transcript.size() {
            return Err(Error("view transcript changed during compression"));
        }
        encoder
            .finish()
            .safe("cannot finish zstd compression")?
            .sync_all()
            .safe("cannot sync archived transcript")?;
        dir.0
            .sync_all()
            .safe("cannot sync transcript archive directory")?;
        let (file, _) = dir.file(name).safe("cannot reopen archived transcript")?;
        append_sum(&mut manifest, &relative, digest(file, pressure)?);
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(staging.path().join(MANIFEST))
        .safe("cannot create archive checksum manifest")?;
    file.write_all(&manifest)
        .safe("cannot write archive checksum manifest")?;
    file.sync_all()
        .safe("cannot sync archive checksum manifest")?;
    staging.0.sync_all().safe("cannot sync staged archive")?;
    let bytes = verify(&staging, view, conflicts, pressure)?;
    pressure.check(false)?;
    renameat_with(
        &parent.0,
        temp_name,
        &parent.0,
        name,
        RenameFlags::NOREPLACE,
    )
    .safe("archive appeared or could not be published")?;
    let _ = temp.keep();
    parent.0.sync_all().safe("cannot sync archive parent")?;
    Ok(bytes)
}

fn verify<'a>(
    archive: &Dir,
    view: &Dir,
    conflicts: impl Iterator<Item = &'a Transcript>,
    pressure: &mut Pressure<'_>,
) -> Result<u64> {
    let mut expected = Vec::new();
    let mut bytes = 0;
    for transcript in conflicts {
        pressure.check(false)?;
        let relative = archive_name(transcript);
        let (file, metadata) = archive
            .relative_file(&relative)
            .safe("cannot read archived transcript")?;
        bytes += metadata.len();
        append_sum(&mut expected, &relative, digest(file, pressure)?);
        let (file, _) = archive
            .relative_file(&relative)
            .safe("cannot reopen archived transcript")?;
        let decoded =
            zstd::stream::read::Decoder::new(file).safe("cannot decode archived transcript")?;
        // Bound verification of an existing archive to the original size plus
        // one byte, including when the archive is corrupt or unrelated.
        if digest(decoded.take(transcript.size().saturating_add(1)), pressure)?
            != digest(transcript.open(view)?, pressure)?
        {
            return Err(Error("archived transcript differs from original"));
        }
    }
    let (manifest, metadata) = archive
        .file(OsStr::new(MANIFEST))
        .safe("cannot read archive checksum manifest")?;
    let mut recorded = Vec::new();
    manifest
        .take(expected.len() as u64 + 1)
        .read_to_end(&mut recorded)
        .safe("cannot read archive checksum manifest")?;
    if recorded != expected {
        return Err(Error("archive checksum manifest did not verify"));
    }
    Ok(bytes + metadata.len())
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
