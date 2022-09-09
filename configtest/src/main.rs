extern crate alloc;

use std::fs::File;
use std::io::{BufReader, Error, ErrorKind, Read, Seek, SeekFrom};
use bootsector::{ReadGPT, ReadMBR, SectorSize};
use ext4::{Checksums, Options, SuperBlock};
use luks2::LuksDevice;
use lvm2::Lvm2;
use positioned_io2::SeekWrapper;
use sha1::digest::Update;

use io::PartialReader;
use crate::io::{IgnoreWriteWrapper, OptimizedSeek};

#[path = "../../src/config.rs"]
mod config;
#[path = "../../src/io.rs"]
mod io;

fn main() {
    env_logger::init();

    // test_config();

    // let key = std::fs::read("/keys/keyfile_lvm").unwrap();
    //
     let mut block_device = File::open("/dev/nvme0n1").unwrap();
     let options = bootsector::Options {
         mbr: ReadMBR::Never,
         gpt: ReadGPT::RevisionOne,
         sector_size: SectorSize::GuessOrAssume,
     };
     let parts = bootsector::list_partitions(SeekWrapper::new(&mut block_device), &options).unwrap();
     println!("parts: {parts:?}");
     //let mut partition = PartialReader::new(&mut block_device, parts[1].first_byte, parts[1].len);
    
    //
    // let mut luks = LuksDevice::from_device(&mut partition, &key, 512).unwrap();
    // let master_key = luks.master_key();
    // let mut luks = LuksDevice::from_device_with_master_key(partition, master_key, 512).unwrap();
    //
    // let lvm2 = Lvm2::open(&mut luks).unwrap();
    // let lv = lvm2.open_lv_by_name("system", &mut luks).unwrap();

    // let options = Options {
    //     checksums: Checksums::Enabled,
    // };
    // let ext4 = SuperBlock::new_with_options(SeekWrapper::new(lv), &options).unwrap();
    // let entry = ext4.resolve_path("/boot2/initramfs-linux-zen.img").unwrap();
    // let inode = ext4.load_inode(entry.inode).unwrap();
    // let mut reader = ext4.open(&inode).unwrap();
    // let mut data = Vec::new();
    // reader.read_to_end(&mut data).unwrap();
    // use sha1::Digest;
    // println!("hash of loaded initrd file: {:x?}", sha1::Sha1::new().chain(&data).finalize());

    //let file = File::open("/dev/sdb2").unwrap();
    //// let file = BufReader::with_capacity(8*128*1024, file);
    //let mut optimized_seek = OptimizedSeek::new(file);
    //let file = IgnoreWriteWrapper::new(&mut optimized_seek);
    //let fat = fatfs::FileSystem::new(file, fatfs::FsOptions::new()).unwrap();
    //let uuid = fat.volume_id();
    //let uuid = format!("{:X}-{:X}", uuid >> 16, uuid as u16);
    //println!("uuid: {}", uuid);
    //println!("label: {}", fat.volume_label());
    //let load_file = |name| {
        //let mut file = fat.root_dir().open_file(name).unwrap();
        //let mut data = Vec::new();
        //let mut buf = [0u8; 4096];
        //loop {
            //use fatfs::Read as _;
            //let read = file.read(&mut buf).unwrap();
            //if read == 0 { break }
            //data.extend_from_slice(&buf[..read]);
        //}
        //data
    //};
    //println!("{:x?}", &load_file("/memtest86+.efi")[..128]);
    //println!("{}", load_file("/archiso/boot/x86_64/vmlinuz-linux").len());
    //println!("{}", load_file("/archiso/boot/intel-ucode.img").len());
    //println!("{}", load_file("/archiso/boot/amd-ucode.img").len());
    //println!("{}", load_file("/archiso/boot/x86_64/initramfs-linux.img").len());
    //drop(fat);
    //println!("total seeks: {}", optimized_seek.total_seeks());
    //println!("stopped seeks: {}", optimized_seek.stopped_seeks());
}

fn test_config() {
    use crate::config::Config;
    let config = include_bytes!("../../config-example.toml");
    let config: Config = toml::from_slice(config).unwrap();
    dbg!(config);
}
