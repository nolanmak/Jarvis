//! Linux quota observations for the identities charged by create-new setup.
//! Only the read commands Q_XGETQSTATV/Q_GETINFO/Q_XGETQUOTA are used.
use super::*;
use crate::autoship_workflow::RetainedSetupQuota;

#[cfg(target_os="linux")]
mod linux {
    use super::*;
    use std::os::{fd::AsRawFd,unix::fs::OpenOptionsExt};

    // Linux UAPI linux/dqblk_xfs.h. The X quota read ABI is also implemented
    // by generic VFS quotas, and reports block units of 512 bytes.
    #[repr(C)]
    #[derive(Default,Clone,Copy)]
    struct QuotaFile {ino:u64,blocks:u64,extents:u32,pad:u32}
    #[repr(C)]
    #[derive(Default)]
    struct QuotaState {version:i8,pad:u8,flags:u16,incore:u32,files:[QuotaFile;3],
        block_time:i32,inode_time:i32,rt_time:i32,block_warn:u16,inode_warn:u16,rt_warn:u16,
        pad3:u16,pad4:u32,pad2:[u64;7]}
    #[repr(C)]
    #[derive(Default)]
    struct QuotaInfo {block_grace:u64,inode_grace:u64,flags:u32,valid:u32}
    #[repr(C)]
    #[derive(Default,Clone)]
    struct DiskQuota {version:i8,flags:i8,mask:u16,id:u32,block_hard:u64,block_soft:u64,
        inode_hard:u64,inode_soft:u64,blocks:u64,inodes:u64,inode_time:i32,block_time:i32,
        inode_warn:u16,block_warn:u16,inode_time_hi:i8,block_time_hi:i8,rt_time_hi:i8,pad:i8,
        rt_hard:u64,rt_soft:u64,rt_blocks:u64,rt_time:i32,rt_warn:u16,pad3:i16,pad4:[u8;8]}
    #[repr(C)]
    #[derive(Default)]
    struct Fsxattr {flags:u32,extent:u32,extents:u32,project:u32,cow_extent:u32,pad:[u8;8]}

    trait Reader {
        fn flags(&self)->Result<u16>;
        fn project(&self)->Result<u32>;
        fn group(&self,meta:&std::fs::Metadata,gid:u32)->Result<u32>;
        fn quota(&self,kind:u32,id:u32)->Result<DiskQuota>;
    }
    fn bounded(path:&Path,limit:usize)->Result<String> {
        use std::io::Read;
        let mut value=String::new();std::fs::File::open(path)?.take(limit as u64+1).read_to_string(&mut value)?;
        ensure!(value.len()<=limit,"quota mount observation exceeds bound");Ok(value)
    }
    fn group_option(options:&str,explicit:bool)->Result<bool> {
        let values=options.split(|c:char|c==',' || c.is_ascii_whitespace()).collect::<Vec<_>>();
        let inherit=values.iter().any(|value|matches!(*value,"grpid"|"bsdgroups"));
        let process=values.iter().any(|value|matches!(*value,"nogrpid"|"sysvgroups"));
        ensure!(!(inherit && process) && (!explicit || inherit || process),"group allocation policy is unavailable");
        Ok(inherit)
    }
    fn xfs_group_option(mount:u64,mountinfo:&str)->Result<bool> {
        let mut found=None;
        for line in mountinfo.lines() {
            if line.split_ascii_whitespace().next().and_then(|id|id.parse::<u64>().ok())!=Some(mount) {continue;}
            ensure!(found.is_none(),"quota mount identity is ambiguous");
            let (_,tail)=line.split_once(" - ").context("quota mount observation is malformed")?;
            let fields=tail.split_ascii_whitespace().collect::<Vec<_>>();
            ensure!(fields.len()==3 && fields[0]=="xfs","quota mount filesystem differs");
            // XFS always displays grpid when enabled, including its defaults.
            found=Some(group_option(fields[2],false)?);
        }
        found.context("quota mount identity is unavailable")
    }
    struct Native(std::fs::File);
    impl Native {
        fn query<T>(&self,command:u32,kind:u32,id:u32,value:&mut T)->std::io::Result<()> {
            let result=unsafe{libc::syscall(libc::SYS_quotactl_fd,self.0.as_raw_fd(),(command<<8)|kind,id,value as *mut T)};
            if result<0 {return Err(std::io::Error::last_os_error());}Ok(())
        }
    }
    impl Reader for Native {
        fn flags(&self)->Result<u16> {
            let mut state=QuotaState {version:1,..Default::default()};
            match self.query(0x5808,0,0,&mut state) {
                Ok(())=>{ensure!(state.version==1 && state.flags & !0x3f==0,"unsupported quota state");Ok(state.flags)},
                // Linux also uses ENOSYS when all quotas are inactive. Prove
                // that case separately; never treat unavailable APIs as free.
                Err(error) if error.raw_os_error()==Some(libc::ENOSYS)=>{
                    for kind in 0..3 {
                        let mut info=QuotaInfo::default();
                        match self.query(libc::Q_GETINFO as u32,kind,0,&mut info) {
                            Err(error) if error.raw_os_error()==Some(libc::ESRCH)=>{},
                            _=>bail!("quota enforcement state is unavailable"),
                        }
                    }
                    Ok(0)
                },
                Err(error)=>Err(error.into()),
            }
        }
        fn project(&self)->Result<u32> {
            let mut attr=Fsxattr::default();
            if unsafe{libc::ioctl(self.0.as_raw_fd(),libc::_IOR::<Fsxattr>(b'X' as u32,31),&mut attr)}<0 {
                return Err(std::io::Error::last_os_error().into());
            }
            ensure!(attr.flags & 0x101==0,"realtime quota allocation is unsupported");
            Ok(if attr.flags & 0x200!=0 {attr.project}else{0})
        }
        fn group(&self,meta:&std::fs::Metadata,gid:u32)->Result<u32> {
            if meta.mode() & libc::S_ISGID!=0 || meta.gid()==gid {return Ok(meta.gid());}
            let mut fs=std::mem::MaybeUninit::<libc::statfs>::uninit();
            if unsafe{libc::fstatfs(self.0.as_raw_fd(),fs.as_mut_ptr())}<0 {return Err(std::io::Error::last_os_error().into());}
            let fs=unsafe{fs.assume_init()};
            let inherit=match fs.f_type as u64 {
                0xef53=>{
                    // mountinfo can omit an ext4 on-disk default. The ext4
                    // per-device options view explicitly reports both cases.
                    let device=std::fs::canonicalize(format!("/sys/dev/block/{}:{}",libc::major(meta.dev()),libc::minor(meta.dev())))?;
                    let name=device.file_name().and_then(|name|name.to_str()).context("quota device name unavailable")?;
                    ensure!(!name.is_empty() && name.len()<=128 && name.bytes().all(|c|c.is_ascii_alphanumeric() || b"._-".contains(&c)),"quota device name unsupported");
                    group_option(&bounded(&PathBuf::from("/proc/fs/ext4").join(name).join("options"),64*1024)?,true)?
                },
                0x58465342=>{
                    let info=bounded(Path::new(&format!("/proc/self/fdinfo/{}",self.0.as_raw_fd())),64*1024)?;
                    let ids=info.lines().filter_map(|line|line.strip_prefix("mnt_id:")).map(|id|id.trim().parse::<u64>()).collect::<std::result::Result<Vec<_>,_>>()?;
                    ensure!(ids.len()==1,"quota mount identity unavailable");
                    xfs_group_option(ids[0],&bounded(Path::new("/proc/self/mountinfo"),4*1024*1024)?)?
                },
                _=>bail!("group quota allocation policy unsupported"),
            };
            Ok(if inherit {meta.gid()}else{gid})
        }
        fn quota(&self,kind:u32,id:u32)->Result<DiskQuota> {
            let mut value=DiskQuota::default();self.query(0x5803,kind,id,&mut value)?;
            ensure!(value.version==1 && value.id==id && value.flags==[1,4,2][kind as usize],"quota identity changed");
            // These setup files are normal data, never XFS realtime files.
            Ok(value)
        }
    }
    fn minimum(a:u64,b:u64)->u64 {match (a,b) {(0,b)=>b,(a,0)=>a,(a,b)=>a.min(b)}}
    fn timer(low:i32,high:i8,big:bool)->Result<u64> {
        let value=if big {(low as u32 as i64)|((high as i64)<<32)}else{low as i64};
        u64::try_from(value).context("negative quota grace deadline")
    }
    fn observe_with(reader:&impl Reader,meta:&std::fs::Metadata,uid:u32,gid:u32)->Result<[RetainedSetupQuota;3]> {
        let flags=reader.flags()?;let mut values=Vec::with_capacity(3);
        // Validate the normal-data allocation class even when only user/group
        // quotas are enforced; realtime counters have different limits.
        let project=if flags & 0x2a!=0 {reader.project()?}else{0};
        for kind in 0..3 {
            if flags & (1<<(kind*2+1))==0 {
                values.push(RetainedSetupQuota {id:0,enabled:false,space_limit_blocks:0,space_used_bytes:0,
                    inode_limit:0,inodes_used:0,space_grace:0,inode_grace:0});continue;
            }
            let id=match kind {0=>uid,1=>reader.group(meta,gid)?,_=>project};
            let quota=reader.quota(kind,id)?;let big=quota.mask & (1<<15)!=0;
            values.push(RetainedSetupQuota {id,enabled:true,space_limit_blocks:minimum(quota.block_hard,quota.block_soft),
                space_used_bytes:quota.blocks.checked_mul(512).context("quota usage exceeds bound")?,
                inode_limit:minimum(quota.inode_hard,quota.inode_soft),inodes_used:quota.inodes,
                space_grace:timer(quota.block_time,quota.block_time_hi,big)?,inode_grace:timer(quota.inode_time,quota.inode_time_hi,big)?});
        }
        values.try_into().map_err(|_|anyhow::anyhow!("quota observation incomplete"))
    }
    pub(super) fn observe(path:&Path)->Result<[RetainedSetupQuota;3]> {
        let before=std::fs::symlink_metadata(path)?;
        ensure!(before.is_dir() && !before.file_type().is_symlink(),"quota namespace is redirected");
        let reader=Native(std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY|libc::O_NOFOLLOW|libc::O_CLOEXEC).open(path)?);
        let opened=reader.0.metadata()?;
        ensure!(opened.dev()==before.dev() && opened.ino()==before.ino(),"quota namespace changed during open");
        let result=observe_with(&reader,&opened,unsafe{libc::geteuid()},unsafe{libc::getegid()})?;
        let after=std::fs::symlink_metadata(path)?;
        ensure!(after.dev()==opened.dev() && after.ino()==opened.ino() && after.mode()==opened.mode() && after.gid()==opened.gid(),"quota namespace changed during observation");
        Ok(result)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        struct Fake {flags:u16,expected_uid:u32,expected_gid:u32,denied:bool,bsdgroups:bool}
        impl Reader for Fake {
            fn flags(&self)->Result<u16> {Ok(self.flags)}
            fn project(&self)->Result<u32> {Ok(71)}
            fn group(&self,meta:&std::fs::Metadata,gid:u32)->Result<u32> {
                Ok(if self.bsdgroups || meta.mode() & libc::S_ISGID!=0 {meta.gid()}else{gid})
            }
            fn quota(&self,kind:u32,id:u32)->Result<DiskQuota> {
                ensure!(!self.denied,"quota read refused");
                assert_eq!(id,[self.expected_uid,self.expected_gid,71][kind as usize]);
                Ok(DiskQuota {version:1,id,block_hard:200,block_soft:100,blocks:100,inode_hard:20,inode_soft:10,inodes:10,
                    block_time:100,inode_time:200,..Default::default()})
            }
        }
        #[test]
        fn quota_observation_uses_charged_ids_and_explicit_units_without_writes() {
            use std::os::unix::fs::PermissionsExt;
            let directory=tempfile::tempdir().unwrap();
            for (mode,gid) in [(0o700,77),(0o2700,std::fs::metadata(directory.path()).unwrap().gid())] {
                std::fs::set_permissions(directory.path(),std::fs::Permissions::from_mode(mode)).unwrap();
                let meta=std::fs::metadata(directory.path()).unwrap();
                let values=observe_with(&Fake {flags:0x3f,expected_uid:42,expected_gid:gid,denied:false,bsdgroups:false},&meta,42,77).unwrap();
                assert_eq!(values[0].space_limit_blocks,100);assert_eq!(values[0].space_used_bytes,51200);
                assert_eq!(values[1].inode_limit,10);assert_eq!(values[2].id,71);
                assert!(observe_with(&Fake {flags:2,expected_uid:42,expected_gid:gid,denied:true,bsdgroups:false},&meta,42,77).is_err());
                let disabled=observe_with(&Fake {flags:0x15,expected_uid:42,expected_gid:gid,denied:true,bsdgroups:false},&meta,42,77).unwrap();
                assert!(disabled.iter().all(|quota|!quota.enabled));
                assert!(std::fs::read_dir(directory.path()).unwrap().next().is_none());
            }
            assert_eq!(std::mem::size_of::<DiskQuota>(),112);assert_eq!(std::mem::size_of::<QuotaState>(),160);
            assert_eq!(timer(-1,1,true).unwrap(),0x1ffffffff);assert!(timer(-1,0,false).is_err());
        }
        #[test]
        fn quota_group_uses_bsd_mount_inheritance_even_without_setgid() {
            use std::os::unix::fs::PermissionsExt;
            let directory=tempfile::tempdir().unwrap();std::fs::set_permissions(directory.path(),std::fs::Permissions::from_mode(0o700)).unwrap();
            let meta=std::fs::metadata(directory.path()).unwrap();let gid=meta.gid().checked_add(1).unwrap();
            let values=observe_with(&Fake {flags:8,expected_uid:42,expected_gid:meta.gid(),denied:false,bsdgroups:true},&meta,42,gid).unwrap();
            assert_eq!(values[1].id,meta.gid(),"group quota did not use the actual inherited owner");
            assert!(group_option("rw\ngrpid\n",true).unwrap());
            assert!(!group_option("rw\nnogrpid\n",true).unwrap());
            assert!(group_option("rw",true).is_err());
            assert!(group_option("rw,grpid,nogrpid",true).is_err());
            let view="19 1 8:1 / /ignored rw - ext4 /dev/a rw,grpid\n20 1 8:2 / /space\\040name rw - xfs /dev/b rw,grpid\n";
            assert!(xfs_group_option(20,view).unwrap());
            assert!(!xfs_group_option(20,&view.replace("xfs /dev/b rw,grpid","xfs /dev/b rw")).unwrap());
            assert!(xfs_group_option(19,view).is_err());assert!(xfs_group_option(21,view).is_err());
            assert!(xfs_group_option(20,&format!("{view}{view}")).is_err());
        }
        #[test]
        fn quota_native_observation_is_read_only_or_explicitly_unavailable() {
            let directory=tempfile::tempdir().unwrap();
            // This exercises the real descriptor/API path without enabling,
            // changing or relying on host quotas. Denial stays unknown.
            if let Ok(values)=observe(directory.path()) {assert_eq!(values.len(),3);}
            assert!(std::fs::read_dir(directory.path()).unwrap().next().is_none());
        }
    }
}

pub(super) fn observe(path:&Path)->Result<[RetainedSetupQuota;3]> {
    #[cfg(target_os="linux")] {linux::observe(path)}
    #[cfg(not(target_os="linux"))] {let _=path;bail!("quota observation unavailable on this platform")}
}
