/**
 * exFat filesystem
 *
 * A sector contains a fixed number (per-exfat volume, power of 2) of bytes.
 *
 * A cluster contains a fixed number (per-exfat volume, power of 2) of sectors.
 *
 * The FAT is an array of u32, with each entry in the array corresponding to a cluster.
 *
 * Using FAT entries as "next pointers", the clusters are formed into chains.
 *
 * The "cluster heap" is basically the entire remainder of the storage volume (after the boot
 * sector and FAT).
 *
 * Directory & File data is stored in the cluster heap.
 *
 * General layout:
 *
 * .                     |offs| size (sectors)
 * boot sector (aka sb)  | 0  | 1
 * extended boot sectors | 1  | 8
 * oem parameters        | 9  | 1
 * reserved              | 10 | 1
 * boot checksum         | 11 | 1
 *
 * Immediately followed by a "backup boot region"
 * of the same layout
 *
 *
 * fat alignment         | 24
 * (undef contents)      |
 * first fat             | fat_offs | fat_len
 * second_fat            | fat_offs + fat_len | fat_len
 * (repeated for `number_of_fats`)
 *
 * cluster heap align    | fat_offs + fat_len * num_of_fats
 *                             | cluster_heap_offs - (fat_offs + fat_len * num_of_fats)
 * cluster heap          |
 *
 */

#[macro_use]
extern crate index_fixed;
extern crate io_at;
extern crate fmt_extra;
extern crate core;

use ::io_at::{ReadAt,WriteAt};
use ::std::io::Read;
use ::std::ops::Index;
use ::fmt_extra::AsciiStr;
use ::std::{mem,slice};

#[derive(Debug)]
pub enum BootSectorInitError {
    BadMagic(AsciiStr<[u8;8]>),
    MustBeZeroNonZero,
    FatOffsTooSmall(u32)
}

#[derive(Debug)]
pub enum BootSectorInitIoError {
    Io(::std::io::Error),
    Init(BootSectorInitError)
}

macro_rules! read_num_bytes {
    ($ty:ty, $size:expr, $src:expr) => ({
        assert!($size == ::core::mem::size_of::<$ty>());
        assert!($size <= $src.len());
        let mut data: $ty = 0;
        unsafe {
            ::core::ptr::copy_nonoverlapping(
                $src.as_ptr(),
                &mut data as *mut $ty as *mut u8,
                $size);
        }
        data.to_le()
    });
}

/**
 * An Exfat superblock. Sometimes refered to as a "boot sector". Contains all the essential items
 * for recognizing and using the filesystem.
 *
 * We store the entire thing is it's very likely that we'll need to "write-back" the entire sector
 * if anything changes (as block devices don't have byte-level access)
 *
 * As an alternative, it might make sense to construct this from any AsRef<[u8]> which can promise
 * it's long enough.
 */
pub struct BootSector {
    raw: [u8;512],
}

impl BootSector {
    /*
     * FIXME: we really need a unification of ReadAt and Read here: as we're only doing a single
     * call (and don't care where the cursor ends up), it'd be nice to allow either
     */
    /// Populate with a superblock from this `ReadAt`able thing, at a given offset
    pub fn read_at_from<R: ReadAt>(s: R, offs: u64) -> Result<Self, BootSectorInitIoError> {
        let mut sb = unsafe { BootSector { raw: ::std::mem::uninitialized() } };
        /*
         * FIXME: ReadAt does not promise that this returns all the data requested. Add a wrapper
         * here or in io-at
         */
        try!(s.read_at(&mut sb.raw, offs).map_err(|e| BootSectorInitIoError::Io(e)));
        sb.validate().map_err(|e| BootSectorInitIoError::Init(e))
    }

    /// Populate with a superblock from this `Read`able thing, at it's current offset
    pub fn read_from<R: Read>(mut s: R) -> Result<Self, BootSectorInitIoError> {
        let mut sb = unsafe { BootSector { raw: ::std::mem::uninitialized() } };
        try!(s.read_exact(&mut sb.raw).map_err(|e| BootSectorInitIoError::Io(e)));
        sb.validate().map_err(|e| BootSectorInitIoError::Init(e))
    }

    /// Create from the exact amount of data needed
    pub fn from(s: [u8;512]) -> Result<Self, BootSectorInitError> {
        /* validate BootSector */
        BootSector { raw: s }.validate()
    }

    pub fn raw(&self) -> &[u8;512] {
        &self.raw
    }

    /// A jump instruction for x86
    ///
    /// Specified to be [0xEB, 0x76, 0x90].
    ///
    /// TODO: consider testing that this matches expectations
    pub fn jump_boot(&self) -> &[u8;3] {
        index_fixed!(&self.raw();0, .. 3)
    }

    /// The string "EXFAT   " (3 trailing spaces)
    ///
    /// Used to check that a volume is using exfat
    ///
    /// offset: 3, size: 8
    pub fn magic(&self) -> &[u8;8] {
        index_fixed!(&self.raw(); 3, .. 11)
    }

    /// Offset in sectors from the start of the media on which this partition is stored.
    ///
    /// If zero (0), should be ignored.
    ///
    /// Only indented for use in BIOS bootup senarios (in which BIOS would load this data into
    /// memory)
    ///
    /// offset: 64, size: 8
    pub fn partition_offs(&self) -> u64 {
        read_num_bytes!(u64, 8, &self.raw()[64..])
    }

    /// Length in sectors of the volume
    ///
    /// At least: `2*20/(2**bytes_per_sector_shift)`
    /// At most: `2**64-1`
    ///
    /// offset: 72, size 8
    pub fn volume_len(&self) -> u64 {
        read_num_bytes!(u64, 8, &self.raw()[72..])
    }

    /// Volume-relative sector offset for the first (and perhaps only) FAT.
    ///
    /// At least: 24
    /// At most: cluster_heap_offs - (fat_len * num_fats)
    ///
    /// offset: 80, size 4
    pub fn fat_offs(&self) -> u32 {
        read_num_bytes!(u32, 4, &self.raw()[80..])
    }

    /// Length in sectors of the FAT table(s)
    ///
    /// At least: (cluster_count + 2) * 2**2 / 2**bytes_per_sector_shift
    ///     rounded up to the nearest integer
    ///     [ie: a FAT must have room for all the clusters]
    /// At most: (cluster_heap_offset - fat_offset) / num_fats
    ///     [ie: the FATs must be ordered before the cluster heap]
    ///
    /// offset: 84, size 4
    pub fn fat_len(&self) -> u32 {
        read_num_bytes!(u32, 4, &self.raw()[84..])
    }

    /// Volume-relative sector offset for the cluster heap
    ///
    /// At least: fat_offs + fat_len * num_fats
    /// [ie: the preceeding rebions]
    ///
    /// At most: min( 2**32-1 , volume_len - cluster_count * 2**sectors_per_cluster_shift )
    ///
    /// offset: 88, size 4
    pub fn cluster_heap_offs(&self) -> u32 {
        read_num_bytes!(u32, 4, &self.raw()[88..])
    }

    /// Number of cluster in the cluster heap
    ///
    /// Value: min(
    ///         volume_len - cluster_heap_offs) / 2**sectors_per_cluster_shift,
    ///         2**32 - 11
    ///     )
    ///
    /// offset: 92, size 4
    pub fn cluster_count(&self) -> u32 {
        read_num_bytes!(u32, 4, &self.raw()[92..])
    }

    /// Cluster index of the first cluster of the root directory
    ///
    /// At least: 2
    /// At most: cluster_count + 1
    ///
    /// offset: 96, size 4
    pub fn first_cluster_of_root_dir(&self) -> u32 {
        read_num_bytes!(u32, 4, &self.raw()[96..])
    }

    /// offset: 100, size 4
    pub fn volume_serial_num(&self) -> u32 {
        read_num_bytes!(u32, 4, &self.raw()[100..])
    }

    /// First byte is major version, Second byte is minor version
    ///
    /// Windows 10 as of 2016-09-10 formats disks with "1.0"
    ///
    /// At Least:  1.0
    /// At Most:  99.99
    ///
    /// offset: 104, size: 2
    pub fn file_system_rev(&self) -> u16 {
        read_num_bytes!(u16, 2, &self.raw()[104..])
    }

    /// Flags indicating tthe status of file system structures on this volume
    /// 0 = Active FAT & allocation bitmap (0 = first, 1 = second)
    /// 1 = Volume dirty (0 = claims consistency, 1 = claims inconsistency)
    /// 2 = media failure (0 = no failures reported, or known failures recorded in bad clusters,
    ///                    1 = reported failures)
    /// 3 = clear to zero (0 = nothing in particular,
    ///                    1 = impls shall set it to 0 prior to modifying any fs structures, dirs,
    ///                      or files)
    /// rest: reseved
    ///
    /// offset: 106, size: 2
    pub fn volume_flags(&self) -> u16 {
        read_num_bytes!(u16, 2, &self.raw()[106..])
    }

    /// bytes per sector in log2(N) form
    ///
    /// At least: 9 (512 bytes)
    /// At most: 12 (4096 bytes)
    ///
    /// Nothing is really restricting these things other than convention, recommend accepting
    /// larger variations.
    ///
    /// offset: 108, size: 1
    pub fn bytes_per_sector_shift(&self) -> u8 {
        self.raw()[108]
    }

    /// sectors per cluster in log2(N) form
    ///
    /// At least: 0 (1 sector)
    /// At most: 25-bytes_per_sector_shift (2**25 - bytes_per_sector = 32MB)
    ///
    /// XXX: determine the basis of the upper limit here.
    ///
    /// offset: 109, size: 1
    pub fn sectors_per_cluster_shift(&self) -> u8 {
        self.raw()[109]
    }

    /// Number of FATs and allocation bitmaps in the volume
    ///
    /// At least: 1
    /// At most: 2
    ///
    /// offset: 110, size: 1
    pub fn number_of_fats(&self) -> u8 {
        self.raw()[110]
    }

    /// INT 13h drive number, intended primarily for bios bootup
    ///
    /// Using 0x80 is common.
    ///
    /// offset: 111, size: 1
    pub fn drive_select(&self) -> u8 {
        self.raw()[111]
    }

    /// percentage of clusters in the cluster heap which are allocated
    ///
    /// At least: 0
    /// At most: 100
    ///
    /// Or set to 0xff to mark as unavalible.
    ///
    /// offset: 112, size: 1
    pub fn percent_in_use(&self) -> u8 {
        self.raw()[112]
    }

    /// Bootstrap data (jumped to by jump_code) intended for use by BIOS boot.
    ///
    /// offset 120, size 390
    pub fn boot_code(&self) -> &[u8;390] {
        index_fixed!(&self.raw(); 120, .. (120+390))
    }

    /// The value AA55
    ///
    /// offset 510, size 2
    pub fn boot_signature(&self) -> &[u8;2] {
        index_fixed!(&self.raw(); 510, .. (510+2))
    }

    fn validate(self) -> Result<Self, BootSectorInitError> {
        /* 0,1,2: jmp junk */
        /* 3-11: "EXFAT" */
        {
            let magic = self.magic();
            if magic != b"EXFAT   " {
                return Err(BootSectorInitError::BadMagic(AsciiStr(magic.clone())))
            }
        }

        /* 11..(53-11): must be zero */
        {
            let z = &self.raw()[11..(11+53)];
            for b in z {
                if *b != 0 {
                    return Err(BootSectorInitError::MustBeZeroNonZero);
                }
            }
        }

        if self.fat_offs() < 24 {
            return Err(BootSectorInitError::FatOffsTooSmall(self.fat_offs()));
        }

        {
            // self.volume_len() > (2**(20-self.bytes_per_sector_shift()))
        }

        {
            // self.fat_offs() > (self.cluster_heap_offs() - self.fat_len() * self.num_fats())
        }

        {
            // self.f
        }

        Ok(self)
    }
}

/**
 * After an exFAT bootsector, there are 8 extended boot sectors.
 *
 * These are intended to carry extra boot code.
 *
 * These can be marked with 0xAA550000 to indicate that they are, in fact, extended boot sectors.
 *
 * The purpose of marking is unclear, as is what the data represents in the case where they are
 * unmarked.
 */
#[derive(Clone,Debug)]
struct ExtendedBootSector {
    s: Vec<u8>,
    bytes_per_sector_shift: u8,
}

impl ExtendedBootSector {
    pub fn from(s: Vec<u8>, bytes_per_sector_shift: u8) -> Self {
        /* TODO: split the "kind" out early? Or late?
         * Perhaps an enum is appropriate here?
         */
        ExtendedBootSector { s: s, bytes_per_sector_shift: bytes_per_sector_shift }
    }

    pub fn raw(&self) -> &[u8] {
        self.s.as_ref()
    }

    pub fn signature(&self) -> u32 {
        let offs = 1 << self.bytes_per_sector_shift - 4;
        read_num_bytes!(u32, 4, &self.raw()[offs..])
    }

    pub fn is_extended_boot_sector(&self) -> bool {
        self.signature() == 0xAA_55_00_00
    }
}

struct ExtendedBootSectors;

struct OemParameter {
    raw: [u8;48],
}

impl OemParameter {
    pub fn is_used(&self) -> bool {
        for i in self.uuid() {
            if *i != 0 {
                return true;
            }
        }

        return false;
    }

    pub fn uuid(&self) -> &[u8;16] {
        index_fixed!(&self.raw; 0, .. 16)
    }

    pub fn data(&self) -> &[u8;32] {
        index_fixed!(&self.raw; 16, .. 48)
    }
}

/// The boot record contains a sector containing oem parameters
struct OemParameters {
    s: Vec<u8>,
}

impl OemParameters {
    pub fn read_at_from<S: ReadAt>(s: S) -> io_at::Result<Self> {
        s::
    }

    pub fn from(s: Vec<u8>) -> Self {
        OemParameters { s: s }
    }

    pub fn raw(&self) -> &[u8] {
        self.s.as_ref()
    }

    pub fn all(&self) -> &[OemParameter;10] {
        unsafe {
            ::std::mem::transmute::<*const u8, &[OemParameter;10]>
                (self.raw().as_ptr())
        }
    }
}

pub enum FsInitError {
    BootSectorInitError(BootSectorInitIoError)
}

#[derive(Debug, Clone)]
pub struct BootRegion {
    bs: BootSector,
    // ebs: ExtendedBootSectors,
    oem: OemParameters,
}

impl BootRegion {
    /*
     * TODO: consider using io_at::At adaptor instead of passing `offs` around manually.
     */
    pub fn read_at_from<S: ReadAt>(t: S, offs: u64) -> Result<Self, BootSectorInitIoError> {
        let bs = try!(BootSector::read_at_from(&t, offs).map_err(|e| FsInitError::BootSectorInitError(e)));
        /*
         * FIXME: instead of using '512' here, we need to either use the bootsector's sector side
         * or query the store for the underlying sector size
         */
        let oem = try!(OemParameters::read_at_from(&t, offs + 512 * 9));
        Ok(BootRegion { bs: bs, oem: oem })
    }
}

/// A full filesystem instance. Allows access to all aspects of the filesystem.
///
/// TODO:
///  - right now we allocate & read quite a bit on object creation. It would be useful (for
///  embedded systems and others) to allow defering or avoiding allocations instead. Can we do this
///  within the confines of our type system without too much extra overhead?
pub struct Fs<S: ReadAt> {
    // We probably don't need 2 copies of the boot region permenantly. The multiple copies are
    // really only important for initial validation of the filesystem. After that point, the ones
    // we're working with in memory will always be the same. The caveat here might be our algorithm
    // for updating the bootsectors (or pieces thereof). Perhaps we'll need to keep a second copy
    // around with the previous contents? not sure.
    boot_regions: [BootRegion;2],
    store: S,
}

impl<S: ReadAt> Fs<S> {
    pub fn from_ro(t: S) -> Result<Self, FsInitError> {
        // FIXME: using 512 here is wrong. We need to use either the media's sector size or the
        // sector size from the first bootsector.
        let br = [
            try!(BootRegion::read_at_from(&t, 0)),
            try!(BootRegion::read_at_from(&t, 512 * 24)),
        ];

        Ok(Fs { boot_regions: br, store: t })
    }

    pub fn boot_sector(&self) -> &BootSector {
        &self.boot_regions[0].bs
    }

    /*
    pub fn ext_boot_sectors(&self) -> &ExtendedBootSectors {
        /* do something?? */
    }
    */
}

/// The FAT (file allocation table) contains a contiguous series of FAT entries.
///
/// Each FAT entry is 4 bytes.
///
/// Some FAT entries are special:
///
/// 0: 0xFF_FF_FF_F8 (f8 indicates "media type")
/// 1: 0xFF_FF_FF_FF ("nothing of interest")
///
/// 2...(cluster_count + 1) : each describe a cluster in the cluster heap
///
/// Values for FAT entires:
///  2...(cluster_count+1): fat entry of the next cluster in the cluster chain
///  0xFF_FF_FF_F7: bad cluster
///  0xFF_FF_FF_FF: last cluster in the cluster chain
pub struct Fat {
    v: Vec<u32>
}

unsafe fn as_mut_bytes(v: &mut [u32]) -> &mut [u8] {
    slice::from_raw_parts_mut(mem::transmute::<*mut u32, *mut u8>(v.as_mut_ptr()), v.len() * mem::size_of::<u32>())
}

impl Fat {
    /* XXX: len must fit in memory, so it is constrained to usize.  Consider what limit exFAT
     * places on the size of the FAT in bytes.
     *
     * Due to each entry being an index & sized at 4 bytes:
     *      (u32::MAX) * 4
     *
     * There are a few "invalid values" that limit the size a bit as well.
     */
    pub fn read_at_from<T: ReadAt>(s: T, offs: u64, len: usize) -> ::io_at::Result<Self> {
        let e = len / 4;
        if (len % 4) != 0 {
            panic!("FAT length must be a multiple of 4");
        }

        let mut f = Fat { v: Vec::with_capacity(e) };
        unsafe { f.v.set_len(e) }
        try!(s.read_at(
            unsafe {
                as_mut_bytes(f.v.as_mut_slice())
            }, offs)
        );
        Ok(f)
    }

    pub fn media_type(&self) -> u8 {
        (self.v[0] & 0xff) as u8
    }

    pub fn cluster_ct(&self) -> u32 {
        self.v.len() as u32 / 4 - 2
    }

    // TODO: consider if we can get Index to work here by abusing a '&' type.
    pub fn entry(&self, e: FatEntry) -> FatEntry {
        FatEntry::from_val(self.v[e.val() as usize])
    }
}

/// A single entry in a Fat. This entry describes a cluster with the same index as this entry. This
/// structure _does not_ store that index.
#[derive(Clone,Copy,Eq,PartialEq,Debug)]
pub struct FatEntry {
    v: u32
}

// FIXME: we need to convert from LE at some point in here. Doing so in 'from_val' is probably the
// best choice, and would mean we'd want to name it something like "from_raw" instead.
impl FatEntry {
    pub fn from_val(i: u32) -> Self {
        FatEntry { v: i }
    }

    /// If true, the cluster that corresponds to this FAT entry is marked as bad.
    pub fn is_bad(&self) -> bool {
        self.v == 0xFF_FF_FF_F7
    }

    /// If true, the cluster that corresponds to this FAT entry is the last one in a cluster chain.
    pub fn is_last(&self) -> bool {
        self.v == 0xFF_FF_FF_FF
    }

    /// If not one of the exceptional cases, this is a FAT index corresponding to the next cluster
    /// in the cluster chain.
    pub fn val(&self) -> u32 {
        self.v
    }
}

/*
/// An array of Cluster fields, each being of cluster size (ie: 2**sectors_per_cluster_shift
/// sectors)
pub struct ClusterHeap {
}
*/

/// A series of `DirectoryEntry`s stored in a cluster chain
///
/// Each entry is 32 bytes
pub struct Dir {
}

pub struct DirEntry {
    v: [u8;32],
}

impl DirEntry {
    /// 0x00 = end-of-directory, all other fields reserved
    ///        subsequent DirEntries in a Dir are also given this type
    /// 0x01...0x7f: unused-dir-entry marker
    /// 0x81...0xff: regular directory entry, see 'EntryType' for breakdown.
    /// 0x80: invalid
    pub fn entry_type(&self) -> u8 {
        self.v[0]
    }

    pub fn custom_defined(&self) -> &[u8;19] {
        index_fixed!(&self.v; 1, ... 19)
    }

    pub fn first_cluster(&self) -> u32 {
        read_num_bytes!(u32, 4, &self.v[20..])
    }

    pub fn data_len(&self) -> u64 {
        read_num_bytes!(u64, 8, &self.v[24..])
    }
}

pub struct EntryType {
    raw: u8
}

impl EntryType {
    pub fn type_code(&self) -> u8 {
        self.raw & ((1 << 6) - 1)
    }

    pub fn type_importance(&self) -> u8 {
        (self.raw >> 5) & 1
    }

    pub fn type_category(&self) -> u8 {
        (self.raw >> 6) & 1
    }

    /// note: 0x1...0x7f, "unused-directory-entry" when this is false.
    pub fn in_use(&self) -> bool {
        (self.raw >> 6) & 1 != 0
    }
}

/// An iterator over a cluster chain
#[derive(Clone)]
pub struct ClusterChain<'a> {
    f: &'a Fat,
    e: FatEntry,
}

impl<'a> Iterator for ClusterChain<'a> {
    type Item = Result<FatEntry, FatEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        let c = self.e;
        if c.is_last() {
            None
        } else {
            self.e = self.f.entry(c);
            if self.e.is_bad() {
                Some(Err(c))
            } else {
                Some(Ok(c))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
    }
}
