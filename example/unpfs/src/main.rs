
extern crate nix;
extern crate rs9p;

use std::fs;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::io::{self, Seek, SeekFrom, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use rs9p::*;

#[macro_use]
mod utils;
use utils::*;

struct UnpfsFid {
    realpath: PathBuf,
    file: Option<fs::File>,
    readdir: Option<std::iter::Enumerate<fs::ReadDir>>,
    peeked_dir: Option<DirEntry>,
}

impl UnpfsFid {
    fn new<P: ?Sized>(path: &P) -> UnpfsFid where P: AsRef<OsStr> {
        UnpfsFid {
            realpath: Path::new(path).to_path_buf(),
            file: None,
            readdir: None,
            peeked_dir: None,
        }
    }
}

struct Unpfs {
    realroot: PathBuf,
}

impl Unpfs {
    fn new(mountpoint: &str) -> Unpfs {
        Unpfs { realroot: PathBuf::from(mountpoint), }
    }
}

impl rs9p::Filesystem for Unpfs {
    type Fid = UnpfsFid;

    fn rattach(&mut self, fid: &mut Fid<Self::Fid>, _afid: Option<&mut Fid<Self::Fid>>, _uname: &str, _aname: &str, _n_uname: u32) -> Result<Fcall> {
        fid.aux = Some(UnpfsFid::new(&self.realroot));
        Ok(Fcall::Rattach { qid: try!(get_qid(&self.realroot)) })
    }

    fn rwalk(&mut self, fid: &mut Fid<Self::Fid>, newfid: &mut Fid<Self::Fid>, wnames: &[String]) -> Result<Fcall> {
        let mut wqids = Vec::new();
        let mut path = fid.aux().realpath.clone();

        for (i, name) in wnames.iter().enumerate() {
            path.push(name);
            let qid = match get_qid(&path) {
                Ok(qid) => qid,
                Err(e) => if i == 0 { return Err(e) } else { break },
            };
            wqids.push(qid);
        }
        newfid.aux = Some(UnpfsFid::new(&path));

        Ok(Fcall::Rwalk { wqids: wqids })
    }

    fn rgetattr(&mut self, fid: &mut Fid<Self::Fid>, req_mask: u64) -> Result<Fcall> {
        let attr = try!(fs::symlink_metadata(&fid.aux().realpath));
        Ok(Fcall::Rgetattr {
            valid: req_mask,
            qid: try!(get_qid(&fid.aux().realpath)),
            stat: to_stat(&attr)
        })
    }

    fn rsetattr(&mut self, fid: &mut Fid<Self::Fid>, valid: u32, stat: &SetAttr) -> Result<Fcall> {
        if valid & setattr::MODE >= 1 { try!(chmod(&fid.aux().realpath, stat.mode)); }
        if valid & setattr::UID >= 1 { try!(chown(&fid.aux().realpath, Some(stat.uid), None)); }
        if valid & setattr::GID >= 1 { try!(chown(&fid.aux().realpath, None, Some(stat.gid))); }
        if valid & setattr::SIZE >= 1 {}
        if valid & setattr::ATIME >= 1 {}
        if valid & setattr::MTIME >= 1 {}
        if valid & setattr::CTIME >= 1 {}
        Ok(Fcall::Rsetattr)
    }

    fn rreadlink(&mut self, fid: &mut Fid<Self::Fid>) -> Result<Fcall> {
        let link = try!(fs::read_link(&fid.aux().realpath));
        let target = pathconv(&link, &self.realroot);
        Ok(Fcall::Rreadlink { target: target.to_str().unwrap().to_owned() })
    }

    fn rreaddir(&mut self, fid: &mut Fid<Self::Fid>, offset: u64, count: u32) -> Result<Fcall> {
        let aux = fid.aux();
        let mut dirents = DirEntryData::new();

        if offset == 0 {
            aux.readdir = Some(try!(fs::read_dir(&aux.realpath)).enumerate());
            dirents.push(try!(get_dirent(&".", 0)));
            dirents.push(try!(get_dirent(&"..", 1)));
        }

        if let Some(ref dirent) = aux.peeked_dir {
            dirents.push(dirent.clone());
        }
        aux.peeked_dir = None;

        for (i, entry) in aux.readdir.as_mut().unwrap() {
            let path = try!(entry.as_ref()).path();
            let dirent = try!(get_dirent(&path, 2 + i as u64));
            if dirents.size() + dirent.size() > count {
                aux.peeked_dir = Some(dirent);
                break;
            }
            dirents.push(dirent);
        }

        Ok(Fcall::Rreaddir { data: dirents })
    }

    fn rlopen(&mut self, fid: &mut Fid<Self::Fid>, flags: u32) -> Result<Fcall> {
        let qid = try!(get_qid(&fid.aux().realpath));

        if !(qid.typ & qt::DIR >= 1) {
            let oflags = nix::fcntl::OFlag::from_bits_truncate(flags as i32);
            let omode = nix::sys::stat::Mode::from_bits_truncate(0);
            let fd = try!(nix::fcntl::open(&fid.aux().realpath, oflags, omode));
            fid.aux().file = unsafe { Some(fs::File::from_raw_fd(fd)) };
        }

        Ok(Fcall::Rlopen {
            qid: qid,
            iounit: 8192 - rs9p::IOHDRSZ
        })
    }

    fn rlcreate(&mut self, fid: &mut Fid<Self::Fid>, name: &str, flags: u32, mode: u32, _gid: u32) -> Result<Fcall> {
        let path = fid.aux().realpath.join(name);
        let oflags = nix::fcntl::OFlag::from_bits_truncate(flags as i32);
        let omode = nix::sys::stat::Mode::from_bits_truncate(mode);
        let fd = try!(nix::fcntl::open(&path, oflags, omode));

        fid.aux = Some(UnpfsFid::new(&path));
        fid.aux().file = unsafe { Some(fs::File::from_raw_fd(fd)) };

        Ok(Fcall::Rlcreate {
            qid: try!(get_qid(&path)),
            iounit: 8192 - rs9p::IOHDRSZ
        })
    }

    fn rread(&mut self, fid: &mut Fid<Self::Fid>, offset: u64, count: u32) -> Result<Fcall> {
        let file = fid.aux().file.as_mut().unwrap();
        try!(file.seek(SeekFrom::Start(offset)));

        let mut buf = vec![0u8; count as usize];
        let bytes = try!(file.read(&mut buf[..]));
        buf.truncate(bytes);

        Ok(Fcall::Rread { data: Data::new(buf) })
    }

    fn rwrite(&mut self, fid: &mut Fid<Self::Fid>, offset: u64, data: &Data) -> Result<Fcall> {
        let file = fid.aux().file.as_mut().unwrap();
        try!(file.seek(SeekFrom::Start(offset)));

        let bytes = try!(file.write(data.data()));

        Ok(Fcall::Rwrite { count: bytes as u32 })
    }

    fn rmkdir(&mut self, dfid: &mut Fid<Self::Fid>, name: &str, _mode: u32, _gid: u32) -> Result<Fcall> {
        let path = dfid.aux().realpath.join(name);
        try!(fs::create_dir(&path));
        Ok(Fcall::Rmkdir { qid: try!(get_qid(&path)) })
    }

    fn rrenameat(&mut self, olddir: &mut Fid<Self::Fid>, oldname: &str, newdir: &mut Fid<Self::Fid>, newname: &str) -> Result<Fcall> {
        let oldpath = olddir.aux().realpath.join(oldname);
        let newpath = newdir.aux().realpath.join(newname);
        try!(fs::rename(&oldpath, &newpath));
        Ok(Fcall::Rrenameat)
    }

    fn runlinkat(&mut self, dirfid: &mut Fid<Self::Fid>, name: &str, _flags: u32) -> Result<Fcall> {
        let path = dirfid.aux().realpath.join(name);
        let attr = try!(fs::symlink_metadata(&path));
        if attr.is_dir() {
            try!(fs::remove_dir(&path));
        } else {
            try!(fs::remove_file(&path));
        }
        Ok(Fcall::Runlinkat)
    }

    fn rfsync(&mut self, fid: &mut Fid<Self::Fid>) -> Result<Fcall> {
        try!(fsync(fid.aux().file.as_mut().unwrap().as_raw_fd()));
        Ok(Fcall::Rfsync)
    }

    fn rclunk(&mut self, _: &mut Fid<Self::Fid>) -> Result<Fcall> {
        Ok(Fcall::Rclunk)
    }
}

fn unpfs_main(args: Vec<String>) -> io::Result<i32> {
    if args.len() < 3 {
        println!("Usage: {} proto!address!port mountpoint", args[0]);
        println!("  where: proto = tcp | unix");
        return Ok(-1);
    }

    let mountpoint = &args[2];
    try!(fs::metadata(mountpoint).and_then(|m| {
        if m.is_dir() {
            Ok(())
        } else {
            io_error!(Other, "mount point must be a directory")
        }
    }));

    println!("[*] Ready to accept clients: {}", args[1]);
    try!(rs9p::srv_mt(Unpfs::new(mountpoint), &args[1]));

    return Ok(0);
}

fn main() {
    let args = std::env::args().collect();
    let exit_code = match unpfs_main(args) {
        Ok(code) => code,
        Err(e) => { println!("Error: {:?}", e); -1 }
    };
    std::process::exit(exit_code);
}
