use crate::{Error, Result, Safe};
use std::{
    ffi::OsString,
    fs,
    os::unix::ffi::OsStringExt,
    path::{Path, PathBuf},
};

#[derive(Default)]
pub(super) struct Mounts(Vec<PathBuf>);

pub(super) fn read() -> Result<Mounts> {
    Mounts::parse(&fs::read("/proc/self/mountinfo").safe("cannot read Linux mount table")?)
}

impl Mounts {
    pub(super) fn parse(bytes: &[u8]) -> Result<Self> {
        let paths = bytes
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut mount = line
                    .split(|&byte| byte == b' ')
                    .nth(4)
                    .ok_or(Error("invalid Linux mount table"))?;
                let mut path = Vec::new();
                while let Some((&byte, rest)) = mount.split_first() {
                    mount = rest;
                    if byte != b'\\' {
                        path.push(byte);
                        continue;
                    }
                    let Some(escape) = mount.get(..3) else {
                        return Err(Error("invalid Linux mount path"));
                    };
                    path.push(match escape {
                        b"040" => b' ',
                        b"011" => b'\t',
                        b"012" => b'\n',
                        b"134" => b'\\',
                        _ => return Err(Error("invalid Linux mount path")),
                    });
                    mount = &mount[3..];
                }
                let path = PathBuf::from(OsString::from_vec(path));
                if !path.is_absolute() {
                    return Err(Error("invalid Linux mount path"));
                }
                Ok(path)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self(paths))
    }

    pub(super) fn check(&self, view: &Path) -> Result<()> {
        if self.0.iter().any(|mount| mount.starts_with(view)) {
            return Err(Error("view contains a mount point"));
        }
        Ok(())
    }
}
