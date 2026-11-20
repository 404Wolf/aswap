use super::Pressure;
use crate::{Error, Result, Safe, provider::Provider, shared};
use ring::digest::{Context, SHA256};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, File, Metadata, OpenOptions},
    io::{self, ErrorKind, Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
};

const TRANSCRIPTS: &[&str] = &["sessions", "projects", "archived_sessions"];

/// All child operations are anchored to an open directory. Each directory
/// component and each regular file is opened with O_NOFOLLOW.
pub(super) struct Dir(pub(super) File);

pub(super) struct Home {
    pub(super) path: PathBuf,
    pub(super) dir: Dir,
}

impl Dir {
    pub(super) fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::other("expected an absolute directory"));
        }
        let root = Self(File::open("/")?);
        root.descend(path.strip_prefix("/").map_err(io::Error::other)?, false)
    }

    pub(super) fn path(&self) -> PathBuf {
        PathBuf::from(format!("/proc/self/fd/{}", self.0.as_raw_fd()))
    }

    pub(super) fn child(&self, name: &OsStr) -> io::Result<Self> {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(self.path().join(name))
            .map(Self)
    }

    pub(super) fn descend(&self, path: &Path, create: bool) -> io::Result<Self> {
        let mut dir = Self(self.0.try_clone()?);
        for component in path.components() {
            let Component::Normal(name) = component else {
                return Err(io::Error::other("invalid directory component"));
            };
            dir = match dir.child(name) {
                Err(error) if create && error.kind() == ErrorKind::NotFound => {
                    match fs::DirBuilder::new()
                        .mode(0o700)
                        .create(dir.path().join(name))
                    {
                        Ok(()) => {}
                        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                        Err(error) => return Err(error),
                    }
                    dir.0.sync_all()?;
                    dir.child(name)?
                }
                result => result?,
            };
        }
        Ok(dir)
    }

    pub(super) fn names(&self) -> io::Result<Vec<OsString>> {
        let mut names = fs::read_dir(self.path())?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<io::Result<Vec<_>>>()?;
        names.sort_unstable();
        Ok(names)
    }

    pub(super) fn file(&self, name: &OsStr) -> io::Result<(File, Metadata)> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(self.path().join(name))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::other("expected a regular file"));
        }
        Ok((file, metadata))
    }

    pub(super) fn relative_file(&self, path: &Path) -> io::Result<(File, Metadata)> {
        let (dir, name) = self.parent(path, false)?;
        dir.file(name)
    }

    pub(super) fn parent<'a>(&self, path: &'a Path, create: bool) -> io::Result<(Self, &'a OsStr)> {
        let dir = self.descend(path.parent().unwrap_or(Path::new("")), create)?;
        let name = path
            .file_name()
            .ok_or_else(|| io::Error::other("missing filename"))?;
        Ok((dir, name))
    }
}

#[derive(Clone, Eq, PartialEq)]
struct Version {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}
impl From<&Metadata> for Version {
    fn from(m: &Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            changed: (m.ctime(), m.ctime_nsec()),
        }
    }
}

pub(super) struct Transcript {
    pub(super) path: PathBuf,
    source: Version,
}
impl Transcript {
    pub(super) fn size(&self) -> u64 {
        self.source.size
    }

    pub(super) fn open(&self, view: &Dir) -> Result<File> {
        let (file, metadata) = view
            .relative_file(&self.path)
            .safe("cannot read view transcript")?;
        if Version::from(&metadata) != self.source {
            return Err(Error("view transcript changed"));
        }
        Ok(file)
    }
}

pub(super) struct Inventory {
    pub(super) transcripts: Vec<Transcript>,
    pub(super) bytes: u64,
    pub(super) shared_links: Vec<(OsString, PathBuf)>,
    directories: Vec<(PathBuf, Version)>,
}

impl Inventory {
    pub(super) fn scan(
        view: &Dir,
        provider: Provider,
        pressure: &mut Pressure<'_>,
    ) -> Result<Self> {
        let mut inventory = Self {
            transcripts: Vec::new(),
            bytes: 0,
            shared_links: Vec::new(),
            directories: Vec::new(),
        };
        let version = Version::from(&view.0.metadata().safe("cannot inspect view")?);
        inventory.visit(view, Path::new(""), version, provider, pressure)?;
        Ok(inventory)
    }

    fn visit(
        &mut self,
        dir: &Dir,
        relative: &Path,
        version: Version,
        provider: Provider,
        pressure: &mut Pressure<'_>,
    ) -> Result<()> {
        if relative.components().count() > 128 {
            return Err(Error("view directory is too deeply nested"));
        }
        let device = version.device;
        self.directories.push((relative.to_owned(), version));
        let top = relative.as_os_str().is_empty();
        let transcript_dir = relative
            .components()
            .next()
            .is_some_and(|part| TRANSCRIPTS.iter().any(|name| part.as_os_str() == *name));
        for name in dir.names().safe("cannot list view directory")? {
            pressure.check(false)?;
            let path = dir.path().join(&name);
            let metadata = fs::symlink_metadata(&path).safe("cannot inspect view entry")?;
            let protected = transcript_dir || (top && TRANSCRIPTS.iter().any(|root| name == *root));
            if metadata.dev() != device {
                return Err(Error("view contains a mounted filesystem"));
            }
            if metadata.is_dir() {
                let child = dir.child(&name).safe("cannot open view directory")?;
                if Version::from(&child.0.metadata().safe("cannot inspect view directory")?)
                    != Version::from(&metadata)
                {
                    return Err(Error("view directory changed"));
                }
                self.visit(
                    &child,
                    &relative.join(&name),
                    Version::from(&metadata),
                    provider,
                    pressure,
                )?;
            } else if metadata.is_file() {
                self.bytes = self.bytes.saturating_add(metadata.len());
                if protected {
                    if top || shared::private(provider, &name) {
                        return Err(Error(
                            "unexpected private or non-directory transcript entry",
                        ));
                    }
                    self.transcripts.push(Transcript {
                        path: relative.join(name),
                        source: Version::from(&metadata),
                    });
                }
            } else if protected {
                if !top || !metadata.is_symlink() {
                    return Err(Error("unmanaged link or special file in transcript tree"));
                }
                // Recognize ordinary links created by View::prepare without
                // opening the link's target. Other links need manual review.
                let target = fs::read_link(path).safe("cannot inspect shared transcript link")?;
                if !target.is_absolute()
                    || target.file_name() != Some(&name)
                    || target.components().any(|c| c == Component::ParentDir)
                {
                    return Err(Error("unrecognized shared transcript link"));
                }
                self.shared_links.push((name, target));
            }
        }
        Ok(())
    }

    pub(super) fn verify(&self, view: &Dir, pressure: &mut Pressure<'_>) -> Result<()> {
        for (relative, version) in &self.directories {
            pressure.check(false)?;
            let dir = view
                .descend(relative, false)
                .safe("cannot recheck view directory")?;
            if Version::from(&dir.0.metadata().safe("cannot inspect view directory")?) != *version {
                return Err(Error("view directory changed"));
            }
        }
        for transcript in &self.transcripts {
            pressure.check(false)?;
            transcript.open(view)?;
        }
        Ok(())
    }

    /// Compare the indexed source to the shared rollout, without following a
    /// view link. A recognized shared link names the same file by construction.
    pub(super) fn verify_rollout(
        &self,
        relative: &Path,
        view: &Dir,
        home: &Home,
        preview: bool,
        pressure: &mut Pressure<'_>,
    ) -> Result<()> {
        const UNSAFE: Error = Error("Codex index rollout cannot be safely mapped to shared home");
        let source = self.transcripts.iter().find(|file| file.path == relative);
        let (canonical, metadata) = match home.dir.relative_file(relative) {
            Ok(file) => file,
            Err(error) if preview && source.is_some() && error.kind() == ErrorKind::NotFound => {
                return Ok(()); // The plan will publish this file before updating the row.
            }
            Err(_) => return Err(UNSAFE),
        };
        let version = Version::from(&metadata);
        if let Some(source) = source {
            if source.size() != metadata.len()
                || digest(source.open(view)?, pressure)? != digest(&canonical, pressure)?
            {
                return Err(UNSAFE);
            }
        } else {
            let top = relative.components().next().ok_or(UNSAFE)?.as_os_str();
            if !self
                .shared_links
                .iter()
                .any(|(name, target)| name == top && *target == home.path.join(name))
            {
                return Err(UNSAFE);
            }
            digest(&canonical, pressure)?;
        }
        if Version::from(&canonical.metadata().safe("cannot inspect Codex rollout")?) != version {
            return Err(UNSAFE);
        }
        Ok(())
    }
}

struct Witness {
    index: usize,
    version: Version,
}

pub(super) struct Plan {
    pub(super) move_bytes: u64,
    pub(super) archive_bytes: u64,
    unique: Vec<usize>,
    conflicts: Vec<usize>,
    shared_files: Vec<Witness>,
}

impl Plan {
    pub(super) fn classify(
        inventory: &Inventory,
        view: &Dir,
        home: Option<&Dir>,
        pressure: &mut Pressure<'_>,
    ) -> Result<Self> {
        let mut plan = Self {
            move_bytes: 0,
            archive_bytes: 0,
            unique: Vec::new(),
            conflicts: Vec::new(),
            shared_files: Vec::new(),
        };
        for (index, transcript) in inventory.transcripts.iter().enumerate() {
            pressure.check(false)?;
            let home = home.ok_or(Error("shared home is unavailable"))?;
            match home.relative_file(&transcript.path) {
                Ok((shared, metadata)) => {
                    let version = Version::from(&metadata);
                    if metadata.len() == transcript.size()
                        && digest(transcript.open(view)?, pressure)? == digest(&shared, pressure)?
                    {
                        if Version::from(
                            &shared.metadata().safe("cannot inspect shared transcript")?,
                        ) != version
                        {
                            return Err(Error("shared transcript changed"));
                        }
                        plan.shared_files.push(Witness { index, version });
                    } else {
                        plan.archive_bytes += transcript.size();
                        plan.conflicts.push(index);
                    }
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    plan.move_bytes += transcript.size();
                    plan.unique.push(index);
                }
                Err(_) => return Err(Error("unsafe or unreadable shared transcript path")),
            }
        }
        Ok(plan)
    }

    pub(super) fn conflicts<'a>(
        &'a self,
        inventory: &'a Inventory,
    ) -> impl Iterator<Item = &'a Transcript> + Clone {
        self.conflicts.iter().map(|&i| &inventory.transcripts[i])
    }

    pub(super) fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    pub(super) fn publish(
        &mut self,
        inventory: &Inventory,
        view: &Dir,
        home: &Dir,
        moved: &mut u64,
        pressure: &mut Pressure<'_>,
    ) -> Result<()> {
        for &index in &self.unique {
            pressure.check(false)?;
            let transcript = &inventory.transcripts[index];
            let (parent, name) = home
                .parent(&transcript.path, true)
                .safe("cannot create shared transcript directory")?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent.path())
                .safe("cannot stage shared transcript")?;
            let mut source = transcript.open(view)?;
            if stream(&mut source, &mut temporary, pressure)? != transcript.size()
                || Version::from(&source.metadata().safe("cannot inspect view transcript")?)
                    != transcript.source
            {
                return Err(Error("view transcript changed"));
            }
            temporary
                .as_file()
                .sync_all()
                .safe("cannot sync shared transcript")?;
            let published = temporary
                .persist_noclobber(parent.path().join(name))
                .safe("shared transcript appeared or could not be published")?;
            *moved += transcript.size();
            parent
                .0
                .sync_all()
                .safe("cannot sync shared transcript directory")?;
            self.shared_files.push(Witness {
                index,
                version: Version::from(
                    &published
                        .metadata()
                        .safe("cannot inspect published transcript")?,
                ),
            });
        }
        Ok(())
    }

    pub(super) fn verify_shared(
        &self,
        inventory: &Inventory,
        home: &Dir,
        pressure: &mut Pressure<'_>,
    ) -> Result<()> {
        for witness in &self.shared_files {
            pressure.check(false)?;
            let transcript = &inventory.transcripts[witness.index];
            let (parent, name) = home
                .parent(&transcript.path, false)
                .safe("cannot recheck shared transcript directory")?;
            let (file, metadata) = parent.file(name).safe("cannot recheck shared transcript")?;
            if Version::from(&metadata) != witness.version {
                return Err(Error("shared transcript changed"));
            }
            file.sync_all().safe("cannot sync shared transcript")?;
            parent
                .0
                .sync_all()
                .safe("cannot sync shared transcript directory")?;
        }
        Ok(())
    }
}

pub(super) fn stream(
    mut source: impl Read,
    mut destination: impl Write,
    pressure: &mut Pressure<'_>,
) -> Result<u64> {
    let mut buffer = [0; 128 * 1024];
    let mut total = 0;
    loop {
        pressure.check(false)?;
        let count = source
            .read(&mut buffer)
            .safe("cannot read transcript data")?;
        if count == 0 {
            return Ok(total);
        }
        destination
            .write_all(&buffer[..count])
            .safe("cannot write transcript data")?;
        total += count as u64;
    }
}

pub(super) fn digest(source: impl Read, pressure: &mut Pressure<'_>) -> Result<[u8; 32]> {
    struct Hash(Context);
    impl Write for Hash {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut hash = Hash(Context::new(&SHA256));
    stream(source, &mut hash, pressure)?;
    Ok(hash.0.finish().as_ref().try_into().expect("SHA-256 length"))
}

pub(super) fn archive_name(transcript: &Transcript) -> PathBuf {
    let mut name = transcript.path.as_os_str().to_owned();
    name.push(".zst");
    PathBuf::from(name)
}
