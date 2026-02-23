///|
pub(all) struct Size(UInt) derive(Eq, Show)

///|
/// Non-negative file size or length of a region within a file.
pub(all) struct FileSize(UInt64) derive(Eq, Show)

///|
/// Timestamp in nanoseconds.
pub(all) struct TimeStamp(UInt64) derive(Eq, Show)

///|
/// Identifiers for clocks.
/// 
/// - Realtime: The clock measuring real time. Time value zero corresponds with 1970-01-01T00:00:00Z.
/// - Monotonic: The store-wide monotonic clock, which is defined as a clock measuring
/// real time, whose value cannot be adjusted and which cannot have negative
/// clock jumps. The epoch of this clock is undefined. The absolute time
/// value of this clock therefore has no meaning.
/// - ProcessCPUTimeId: The CPU-time clock associated with the current process.
/// - ThreadCPUTimeId: The CPU-time clock associated with the current thread.
pub(all) enum ClockId {
  RealTime
  Monotonic
  ProcessCPUTimeId
  ThreadCPUTimeId
} derive(Eq, Show)

///|
/// Return the numeric clock id value.
pub fn ClockId::value(self : ClockId) -> UInt {
  match self {
    RealTime => 0
    Monotonic => 1
    ProcessCPUTimeId => 2
    ThreadCPUTimeId => 3
  }
}

///|
/// Convert a numeric clock id into `ClockId`.
pub fn ClockId::from_value(value : UInt) -> ClockId? {
  match value {
    0 => Some(RealTime)
    1 => Some(Monotonic)
    2 => Some(ProcessCPUTimeId)
    3 => Some(ThreadCPUTimeId)
    _ => None
  }
}

///|
/// Error codes returned by functions.
/// Not all of these error codes are returned by the functions provided by this
/// API; some are used in higher-level library layers, and others are provided
/// merely for alignment with POSIX.
/// 
/// See : https://github.com/WebAssembly/WASI/blob/wasi-0.1/preview1/docs.md#-errno-variant
pub(all) suberror Errno {
  /// No error occurred
  Success
  /// Argument list too long
  TooBig
  /// Permission denied
  Acces
  /// Address in use
  Addrinuse
  /// Address not available
  Addrnotavail
  /// Address family not supported
  Afnosupport
  /// Resource unavailable, or operation would block
  Again
  /// Connection already in progress
  Already
  /// Bad file descriptor
  Badf
  /// Bad message
  Badmsg
  /// Device or resource busy
  Busy
  /// Operation canceled
  Canceled
  /// No child processes
  Child
  /// Connection aborted
  Connaborted
  /// Connection refused
  Connrefused
  /// Connection reset
  Connreset
  /// Resource deadlock would occur
  Deadlock
  /// Destionation address required
  Destaddrreq
  /// Mathematics argument out of domain of function
  Dom
  /// Reserved
  Dquot
  /// File exists
  Exist
  /// Bad address
  Fault
  /// File too large
  Fbig
  /// Host is unreachable
  Hostunreach
  /// Identifier removed
  Idrm
  /// Illegal byte sequence
  Ilseq
  /// Operation in progress
  Inprogress
  /// Interrupted function
  Intr
  /// Invalid argument
  Inval
  /// I/O error
  IO
  /// Socket is connected
  Isconn
  /// Is a directory
  Isdir
  /// Too many levels of symbolic links
  Loop
  /// File descriptor value too large
  Mfile
  /// Too many links
  Mlink
  /// Message too large
  Msgsize
  /// Reserved
  Multihop
  /// Filename too long
  Nametoolong
  /// Network is down
  Netdown
  /// Connection aborted by network
  Netreset
  /// Network unreachable
  Netunreach
  /// Too many files open in system
  Nfile
  /// No buffer space available
  Nobufs
  /// No such device
  Nodev
  /// No such file or directory
  Noent
  /// Executable file format error
  Noexec
  /// No locks available
  Nolck
  /// Reserved
  Nolink
  /// Not enough space
  Nomem
  /// No message of the desired type
  Nomsg
  /// Protocol not available
  Noprotoopt
  /// No space left on device
  Nospc
  /// Function not supported
  Nosys
  /// The socket is not connected
  Notconn
  /// Not a directory or a symbolic link to a directory
  Notdir
  /// Directory not empty
  Notempty
  /// State not recoverable
  Notrecoverable
  /// Not a socket
  Notsock
  /// Not supported, or operation not supported on socket
  Notsup
  /// Inappropriate I/O control operation
  Notty
  /// No such device or address
  Nxio
  /// Value too large to be stored in data type
  Overflow
  /// Previous owner died
  Ownerdead
  /// Operation not permitted
  Perm
  /// Broken pipe
  Pipe
  /// Protocol error
  Proto
  /// Protocol not supported
  Protonosupport
  /// Protocol wrong type for socket
  Prototype
  /// Result too large
  Range
  /// Read-only file system
  Rofs
  /// Invalid seek
  Spipe
  /// No such process
  Srch
  /// Reserved
  Stale
  /// Connection timed out
  Timedout
  /// Text file busy
  Txtbsy
  /// Cross-device link
  Xdev
  /// Extension: Capabilities insufficient
  Notcapable
} derive(Eq, Show)

///|
/// Return the numeric errno value.
pub fn Errno::value(self : Errno) -> UInt {
  match self {
    Success => 0
    TooBig => 1
    Acces => 2
    Addrinuse => 3
    Addrnotavail => 4
    Afnosupport => 5
    Again => 6
    Already => 7
    Badf => 8
    Badmsg => 9
    Busy => 10
    Canceled => 11
    Child => 12
    Connaborted => 13
    Connrefused => 14
    Connreset => 15
    Deadlock => 16
    Destaddrreq => 17
    Dom => 18
    Dquot => 19
    Exist => 20
    Fault => 21
    Fbig => 22
    Hostunreach => 23
    Idrm => 24
    Ilseq => 25
    Inprogress => 26
    Intr => 27
    Inval => 28
    IO => 29
    Isconn => 30
    Isdir => 31
    Loop => 32
    Mfile => 33
    Mlink => 34
    Msgsize => 35
    Multihop => 36
    Nametoolong => 37
    Netdown => 38
    Netreset => 39
    Netunreach => 40
    Nfile => 41
    Nobufs => 42
    Nodev => 43
    Noent => 44
    Noexec => 45
    Nolck => 46
    Nolink => 47
    Nomem => 48
    Nomsg => 49
    Noprotoopt => 50
    Nospc => 51
    Nosys => 52
    Notconn => 53
    Notdir => 54
    Notempty => 55
    Notrecoverable => 56
    Notsock => 57
    Notsup => 58
    Notty => 59
    Nxio => 60
    Overflow => 61
    Ownerdead => 62
    Perm => 63
    Pipe => 64
    Proto => 65
    Protonosupport => 66
    Prototype => 67
    Range => 68
    Rofs => 69
    Spipe => 70
    Srch => 71
    Stale => 72
    Timedout => 73
    Txtbsy => 74
    Xdev => 75
    Notcapable => 76
  }
}

///|
/// Convert a numeric errno into `Errno`.
pub fn Errno::from_value(value : UInt) -> Errno? {
  match value {
    0 => Some(Success)
    1 => Some(TooBig)
    2 => Some(Acces)
    3 => Some(Addrinuse)
    4 => Some(Addrnotavail)
    5 => Some(Afnosupport)
    6 => Some(Again)
    7 => Some(Already)
    8 => Some(Badf)
    9 => Some(Badmsg)
    10 => Some(Busy)
    11 => Some(Canceled)
    12 => Some(Child)
    13 => Some(Connaborted)
    14 => Some(Connrefused)
    15 => Some(Connreset)
    16 => Some(Deadlock)
    17 => Some(Destaddrreq)
    18 => Some(Dom)
    19 => Some(Dquot)
    20 => Some(Exist)
    21 => Some(Fault)
    22 => Some(Fbig)
    23 => Some(Hostunreach)
    24 => Some(Idrm)
    25 => Some(Ilseq)
    26 => Some(Inprogress)
    27 => Some(Intr)
    28 => Some(Inval)
    29 => Some(IO)
    30 => Some(Isconn)
    31 => Some(Isdir)
    32 => Some(Loop)
    33 => Some(Mfile)
    34 => Some(Mlink)
    35 => Some(Msgsize)
    36 => Some(Multihop)
    37 => Some(Nametoolong)
    38 => Some(Netdown)
    39 => Some(Netreset)
    40 => Some(Netunreach)
    41 => Some(Nfile)
    42 => Some(Nobufs)
    43 => Some(Nodev)
    44 => Some(Noent)
    45 => Some(Noexec)
    46 => Some(Nolck)
    47 => Some(Nolink)
    48 => Some(Nomem)
    49 => Some(Nomsg)
    50 => Some(Noprotoopt)
    51 => Some(Nospc)
    52 => Some(Nosys)
    53 => Some(Notconn)
    54 => Some(Notdir)
    55 => Some(Notempty)
    56 => Some(Notrecoverable)
    57 => Some(Notsock)
    58 => Some(Notsup)
    59 => Some(Notty)
    60 => Some(Nxio)
    61 => Some(Overflow)
    62 => Some(Ownerdead)
    63 => Some(Perm)
    64 => Some(Pipe)
    65 => Some(Proto)
    66 => Some(Protonosupport)
    67 => Some(Prototype)
    68 => Some(Range)
    69 => Some(Rofs)
    70 => Some(Spipe)
    71 => Some(Srch)
    72 => Some(Stale)
    73 => Some(Timedout)
    74 => Some(Txtbsy)
    75 => Some(Xdev)
    76 => Some(Notcapable)
    _ => None
  }
}

///|
test "errno" {
  for i in 0U..<76 {
    let errno = Errno::from_value(i)
    assert_eq(errno.unwrap().value(), i)
  }
}

///|
///  File descriptor rights, determining which actions may be performed.
pub(all) struct Rights(UInt64) derive(Eq, Show)

///|
/// Create an empty set of rights.
pub fn Rights::new() -> Rights {
  0
}

///|
///  File descriptor rights, determining which actions may be performed.
pub(all) enum Right {
  Fd_datasync
  Fd_read
  Fd_seek
  Fd_fdstat_set_flags
  Fd_sync
  Fd_tell
  Fd_write
  Fd_advice
  Fd_allocate
  Path_create_directory
  Path_create_file
  Path_link_source
  Path_link_target
  Path_open
  Fd_readdir
  Path_readlink
  Path_rename_source
  Path_rename_target
  Path_filestat_get
  Path_filestat_set_size
  Path_filestat_set_times
  Fd_filestat_get
  Fd_filestat_set_size
  Fd_filestat_set_times
  Path_symlink
  Path_remove_directory
  Path_unlink_file
  Poll_fd_readwrite
  Sock_shutdown
  Sock_accept
} derive(Eq, Show)

///|
/// Return the numeric value for a single right.
pub fn Right::value(self : Right) -> UInt64 {
  match self {
    Fd_datasync => 1UL << 0
    Fd_read => 1UL << 1
    Fd_seek => 1UL << 2
    Fd_fdstat_set_flags => 1UL << 3
    Fd_sync => 1UL << 4
    Fd_tell => 1UL << 5
    Fd_write => 1UL << 6
    Fd_advice => 1UL << 7
    Fd_allocate => 1UL << 8
    Path_create_directory => 1UL << 9
    Path_create_file => 1UL << 10
    Path_link_source => 1UL << 11
    Path_link_target => 1UL << 12
    Path_open => 1UL << 13
    Fd_readdir => 1UL << 14
    Path_readlink => 1UL << 15
    Path_rename_source => 1UL << 16
    Path_rename_target => 1UL << 17
    Path_filestat_get => 1UL << 18
    Path_filestat_set_size => 1UL << 19
    Path_filestat_set_times => 1UL << 20
    Fd_filestat_get => 1UL << 21
    Fd_filestat_set_size => 1UL << 22
    Fd_filestat_set_times => 1UL << 23
    Path_symlink => 1UL << 24
    Path_remove_directory => 1UL << 25
    Path_unlink_file => 1UL << 26
    Poll_fd_readwrite => 1UL << 27
    Sock_shutdown => 1UL << 28
    Sock_accept => 1UL << 29
  }
}

///|
/// Return a new rights set with the given right enabled.
pub fn Rights::set(self : Rights, right : Right) -> Rights {
  self.0 | right.value()
}

///|
/// Return a new rights set with the given right disabled.
pub fn Rights::unset(self : Rights, right : Right) -> Rights {
  self.0 & (right.value() ^ 0)
}

///|
/// Check whether a right is enabled.
pub fn Rights::is_set(self : Rights, right : Right) -> Bool {
  (self.0 & right.value()) != 0
}

///|
/// A file descriptor handle
pub(all) struct Fd(Int) derive(Eq, Show)

///|
/// A region of memory for scatter/gather reads.
/// 
/// - `buf`: The address of the buffer to be filled.
/// - `buf_len`: The length of the buffer to be filled.
pub(all) struct IOVec {
  buf : Int
  buf_len : Size
} derive(Eq, Show)

///|
/// A region of memory for scatter/gather writes.
/// 
/// - `buf`: The address of the buffer to be written.
/// - `buf_len`: The length of the buffer to be written.
pub(all) struct CIOVec {
  buf : Int
  buf_len : Size
} derive(Eq, Show)

///|
/// Relative offset within a file.
pub(all) struct FileDelta(Int64) derive(Eq, Show)

///|
/// The position relative to which to set the offset of the file descriptor
pub(all) enum Whence {
  Set
  Cur
  End
} derive(Eq, Show)

///|
/// Return the numeric whence value.
pub fn Whence::value(self : Whence) -> UInt {
  match self {
    Set => 0
    Cur => 1
    End => 2
  }
}

///|
/// Convert a numeric whence into `Whence`.
pub fn Whence::from_value(value : UInt) -> Whence? {
  match value {
    0 => Some(Set)
    1 => Some(Cur)
    2 => Some(End)
    _ => None
  }
}

///|
/// A reference to the offset of a directory entry.
/// 
/// The value 0 signifies the start of the directory.
pub(all) struct DirCookie(UInt64) derive(Eq, Show)

///|
/// The type for the `dirent::d_namlen` field of `dirent` struct.
pub(all) struct DirNamlen(UInt) derive(Eq, Show)

///|
/// File serial number that is unique within its file system.
pub(all) struct Inode(UInt64) derive(Eq, Show)

///|
/// The type of a file descriptor or file.
/// 
/// - Unknown: The type of the file descriptor or file is unknown or is different from any of the other types specified.
/// - Block_device: The file descriptor or file refers to a block device inode.
/// - Character_device: The file descriptor or file refers to a character device inode.
/// - Directory: The file descriptor or file refers to a directory inode.
/// - Regular_file: The file descriptor or file refers to a regular file inode.
/// - Socket_dgram: The file descriptor or file refers to a datagram socket.
/// - Socket_stream: The file descriptor or file refers to a byte-stream socket.
/// - Symbolic_link: The file refers to a symbolic link inode.
pub(all) enum FileType {
  Unknown
  Block_device
  Character_device
  Directory
  Regular_file
  Socket_dgram
  Socket_stream
  Symbolic_link
} derive(Eq, Show)

///|
/// Return the numeric file type value.
pub fn FileType::value(self : FileType) -> Int {
  match self {
    Unknown => 0
    Block_device => 1
    Character_device => 2
    Directory => 3
    Regular_file => 4
    Socket_dgram => 5
    Socket_stream => 6
    Symbolic_link => 7
  }
}

///|
/// Convert a numeric file type into `FileType`.
pub fn FileType::from_value(value : Int) -> FileType? {
  match value {
    0 => Some(Unknown)
    1 => Some(Block_device)
    2 => Some(Character_device)
    3 => Some(Directory)
    4 => Some(Regular_file)
    5 => Some(Socket_dgram)
    6 => Some(Socket_stream)
    7 => Some(Symbolic_link)
    _ => None
  }
}

///|
/// A directory entry.
/// 
/// - `d_next`: The offset of the next directory entry stored in the buffer.
/// - `d_ino`: The serial number of the file referred to by this directory entry.
/// - `d_namlen`: The length of the name of the directory entry.
/// - `d_type`: The type of the file referred to by this directory entry.
pub(all) struct DirEnt {
  d_next : DirCookie
  d_ino : Inode
  d_namlen : DirNamlen
  d_type : FileType
} derive(Eq, Show)

///|
/// File or memory access pattern advisory information.
/// 
/// - Normal: The application has no advice to give on its behavior with respect to the specified data.
/// - Sequential: The application expects to access the specified data sequentially from lower offsets to higher offsets.
/// - Random: The application expects to access the specified data in a random order.
/// - Willneed: The application expects to access the specified data in the near future.
/// - Dontneed: The application expects that it will not access the specified data in the near future.
/// - Noreuse: The application expects to access the specified data once and then not reuse it afterwards.
pub(all) enum Advice {
  Normal
  Sequential
  Random
  Willneed
  Dontneed
  Noreuse
} derive(Eq, Show)

///|
/// Return the numeric advice value.
pub fn Advice::value(self : Advice) -> UInt {
  match self {
    Normal => 0
    Sequential => 1
    Random => 2
    Willneed => 3
    Dontneed => 4
    Noreuse => 5
  }
}

///|
/// Convert a numeric advice value into `Advice`.
pub fn Advice::from_value(value : UInt) -> Advice? {
  match value {
    0 => Some(Normal)
    1 => Some(Sequential)
    2 => Some(Random)
    3 => Some(Willneed)
    4 => Some(Dontneed)
    5 => Some(Noreuse)
    _ => None
  }
}

///|
/// File descriptor flag.
pub(all) struct FdFlags(UInt) derive(Eq, Show)

///|
/// Create an empty set of fd flags.
pub fn FdFlags::new() -> FdFlags {
  0
}

///|
/// File descriptor flags.
/// 
/// - Append: Append mode: Data written to the file is always appended to the file's end.
/// - Dsync: Write according to synchronized I/O data integrity completion. Only the data stored in the file is synchronized.
/// - Nonblock: Non-blocking mode.
/// - Rsync: Synchronized read I/O operations.
/// - Sync: Write according to synchronized I/O file integrity completion. In addition to synchronizing the data stored in the file, the implementation may also synchronously update the file's metadata.
pub(all) enum FdFlag {
  Append
  Dsync
  Nonblock
  Rsync
  Sync
} derive(Eq, Show)

///|
/// Return the numeric value for a single fd flag.
pub fn FdFlag::value(self : FdFlag) -> UInt {
  match self {
    Append => 1
    Dsync => 2
    Nonblock => 4
    Rsync => 8
    Sync => 16
  }
}

///|
/// Return a new fd flags set with the given flag enabled.
pub fn FdFlags::set(self : FdFlags, flag : FdFlag) -> FdFlags {
  self.0 | flag.value()
}

///|
/// Return a new fd flags set with the given flag disabled.
pub fn FdFlags::unset(self : FdFlags, flag : FdFlag) -> FdFlags {
  self.0 & (flag.value() ^ 0)
}

///|
/// Check whether an fd flag is enabled.
pub fn FdFlags::is_set(self : FdFlags, flag : FdFlag) -> Bool {
  (self.0 & flag.value()) != 0
}

///|
/// File descriptor attributes.
/// 
/// - `fs_filetype`: File type.
/// - `fs_flags`: File descriptor flags.
/// - `fs_rights_base`: Rights that apply to this file descriptor.
/// - `fs_rights_inheriting`: Maximum set of rights that may be installed on new file descriptors that are created through this file descriptor, e.g., through `path_open`.
pub(all) struct FdStat {
  fs_filetype : FileType
  fs_flags : FdFlags
  fs_rights_base : Rights
  fs_rights_inheriting : Rights
} derive(Eq, Show)

///|
/// Identifier for a device containing a file system. Can be used in combination
/// with `inode` to uniquely identify a file or directory in the filesystem.
pub(all) struct Device(UInt64) derive(Eq, Show)

///|
/// Which file time attributes to adjust
pub(all) struct FstFlags(UInt) derive(Eq, Show)

///|
/// Create an empty set of filestat flags.
pub fn FstFlags::new() -> FstFlags {
  0
}

///|
/// Which file time attributes to adjust
/// 
/// - Atim: Adjust the last access timestamp to the value stored in `filestat::atim`.
/// - Atim_now: Adjust the last access timestamp to the time of clock `clockid::realtime`.
/// - Mtim: Adjust the last modification timestamp to the value stored in `filestat::mtim`.
/// - Mtim_now: Adjust the last modification timestamp to the time of clock `clockid::realtime`.
pub(all) enum FstFlag {
  Atim
  Atim_now
  Mtim
  Mtim_now
} derive(Eq, Show)

///|
/// Return the numeric value for a filestat flag.
pub fn FstFlag::value(self : FstFlag) -> UInt {
  match self {
    Atim => 1
    Atim_now => 2
    Mtim => 4
    Mtim_now => 8
  }
}

///|
/// Return a new filestat flags set with the given flag enabled.
pub fn FstFlags::set(self : FstFlags, flag : FstFlag) -> FstFlags {
  self.0 | flag.value()
}

///|
/// Return a new filestat flags set with the given flag disabled.
pub fn FstFlags::unset(self : FstFlags, flag : FstFlag) -> FstFlags {
  self.0 & (flag.value() ^ 0)
}

///|
/// Check whether a filestat flag is enabled.
pub fn FstFlags::is_set(self : FstFlags, flag : FstFlag) -> Bool {
  (self.0 & flag.value()) != 0
}

///|
/// Flags dermining the method of how paths are resolved.
pub(all) struct LookupFlags(UInt) derive(Eq, Show)

///|
/// Create an empty set of lookup flags.
pub fn LookupFlags::new() -> LookupFlags {
  0
}

///|
pub(all) enum LookupFlag {
  Symlink_follow
} derive(Eq, Show)

///|
/// Return the numeric value for a lookup flag.
pub fn LookupFlag::value(self : LookupFlag) -> UInt {
  match self {
    Symlink_follow => 1
  }
}

///|
/// Return a new lookup flags set with the given flag enabled.
pub fn LookupFlags::set(self : LookupFlags, flag : LookupFlag) -> LookupFlags {
  self.0 | flag.value()
}

///|
/// Return a new lookup flags set with the given flag disabled.
pub fn LookupFlags::unset(self : LookupFlags, flag : LookupFlag) -> LookupFlags {
  self.0 & (flag.value() ^ 0)
}

///|
/// Check whether a lookup flag is enabled.
pub fn LookupFlags::is_set(self : LookupFlags, flag : LookupFlag) -> Bool {
  (self.0 & flag.value()) != 0
}

///|
/// Open flags used by `path_open`.
pub(all) struct OpenFlags(UInt) derive(Eq, Show)

///|
/// Create an empty set of open flags.
pub fn OpenFlags::new() -> OpenFlags {
  0
}

///|
/// Open flags used by `path_open`.
/// 
/// - Create: Create the file if it does not exist.
/// - Directory: Fail if not a directory.
/// - Excl: Fail if the file already exists.
/// - Trunc: Truncate the file to size 0.
pub(all) enum OpenFlag {
  Create
  Directory
  Excl
  Trunc
} derive(Eq, Show)

///|
/// Return the numeric value for an open flag.
pub fn OpenFlag::value(self : OpenFlag) -> UInt {
  match self {
    Create => 1
    Directory => 2
    Excl => 4
    Trunc => 8
  }
}

///|
/// Return a new open flags set with the given flag enabled.
pub fn OpenFlags::set(self : OpenFlags, flag : OpenFlag) -> OpenFlags {
  self.0 | flag.value()
}

///|
/// Return a new open flags set with the given flag disabled.
pub fn OpenFlags::unset(self : OpenFlags, flag : OpenFlag) -> OpenFlags {
  self.0 & (flag.value() ^ 0)
}

///|
/// Check whether an open flag is enabled.
pub fn OpenFlags::is_set(self : OpenFlags, flag : OpenFlag) -> Bool {
  (self.0 & flag.value()) != 0
}

///|
/// Number of hard links to an inode.
pub(all) struct LinkCount(UInt64) derive(Eq, Show)

///|
/// File attributes.
/// 
/// - `dev`: Device ID of device containing the file.
/// - `ino`: File serial number.
/// - `filetype`: File type.
/// - `nlink`: Number of hard links to the file.
/// - `size`: For regular files, the file size in bytes. For symbolic links, the length in bytes of the pathname contained in the symbolic link.
/// - `atim`: Last data access timestamp. This can be 0 if the underlying platform doesn't provide suitable timestamp for this file.
/// - `mtim`: Last data modification timestamp. This can be 0 if the underlying platform doesn't provide suitable timestamp for this file.
/// - `ctim`: Last file status change timestamp. This can be 0 if the underlying platform doesn't provide suitable timestamp for this file.
pub(all) struct FileStat {
  dev : Device
  ino : Inode
  filetype : FileType
  nlink : LinkCount
  size : FileSize
  atim : TimeStamp
  mtim : TimeStamp
  ctim : TimeStamp
} derive(Eq, Show)

///|
/// User-provided value that may be attached to objects that is retained when
/// extracted from the implementation.
pub(all) struct Userdata(UInt64) derive(Eq, Show)

///|
/// Type of a subscription to an event or its occurrence.
/// 
/// - Clock: The time value of clock `subscription_clock::id` has reached timestamp `subscription_clock::timeout`.
/// - Fd_read: File descriptor `subscription_fd_readwrite::file_descriptor` has data available for reading. This event always triggers for regular files.
/// - Fd_write: File descriptor `subscription_fd_readwrite::file_descriptor` has capacity available for writing. This event always triggers for regular files.
pub(all) enum EventType {
  Clock
  Fd_read
  Fd_write
} derive(Eq, Show)

///|
/// Return the numeric value for an event type.
pub fn EventType::value(self : EventType) -> UInt {
  match self {
    Clock => 0
    Fd_read => 1
    Fd_write => 2
  }
}

///|
/// Convert a numeric event type into `EventType`.
pub fn EventType::from_value(value : UInt) -> EventType? {
  match value {
    0 => Some(Clock)
    1 => Some(Fd_read)
    2 => Some(Fd_write)
    _ => None
  }
}

///|
/// The state of the file descriptor subscribed to with `eventtype::fd_read` or `eventtype::fd_write`.
pub(all) struct EventRWFlags(UInt) derive(Eq, Show)

///|
/// Create an empty set of event read/write flags.
pub fn EventRWFlags::new() -> EventRWFlags {
  0
}

///|
/// The state of the file descriptor subscribed to with `eventtype::fd_read` or `eventtype::fd_write`.
/// 
/// - Fd_readwrite_hangup: The peer of this socket has closed or disconnected.
pub(all) enum EventRWFlag {
  Fd_readwrite_hangup
} derive(Eq, Show)

///|
/// Return the numeric value for an event read/write flag.
pub fn EventRWFlag::value(self : EventRWFlag) -> UInt {
  match self {
    Fd_readwrite_hangup => 1
  }
}

///|
/// Return a new event read/write flags set with the given flag enabled.
pub fn EventRWFlags::set(
  self : EventRWFlags,
  flag : EventRWFlag,
) -> EventRWFlags {
  EventRWFlags(self.0 | flag.value())
}

///|
/// Return a new event read/write flags set with the given flag disabled.
pub fn EventRWFlags::unset(
  self : EventRWFlags,
  flag : EventRWFlag,
) -> EventRWFlags {
  EventRWFlags(self.0 & (flag.value() ^ 0))
}

///|
/// Check whether an event read/write flag is enabled.
pub fn EventRWFlags::is_set(self : EventRWFlags, flag : EventRWFlag) -> Bool {
  (self.0 & flag.value()) != 0
}

///|
/// The contents of an `event` when type is `eventtype::fd_read` or
/// `eventtype::fd_write`.
/// 
/// - `nbytes`: Number of bytes available for reading or writing.
/// - `flags`: The state of the file descriptor.
pub(all) struct Event_fd_readwrite {
  nbytes : FileSize
  flags : EventRWFlags
} derive(Eq, Show)

///|
/// An event that occurred.
/// 
/// - `userdata`: User-provided value that got attched to `subscription::userdata`.
/// - `error`: If non-zero, an error that occurred while processing the subscription request.
/// - `type_`: The type of the event that occurred.
/// - `fd_readwrite`: The contents of the event, if it is an `eventtype::fd_read` or `eventtype::fd_write`. `eventtype::clock` events ignore this field.
pub(all) struct Event {
  userdata : Userdata
  error : Errno
  type_ : EventType
  fd_readwrite : Event_fd_readwrite
} derive(Eq, Show)

///|
/// Flags determining how to interpret the timestamp provided in `subscription::timeout`.
pub(all) struct SubclockFlags(UInt) derive(Eq, Show)

///|
/// Create an empty set of subclock flags.
pub fn SubclockFlags::new() -> SubclockFlags {
  0
}

///|
/// Flags determining how to interpret the timestamp provided in `subscription::timeout`.
/// 
/// - Subscription_clock_abstime: 
///   If set, treat the timestamp provided in
///   `subscription_clock::timeout` as an absolute timestamp of clock
///   `subscription_clock::id`. If clear, treat the timestamp
///   provided in `subscription_clock::timeout` relative to the
///   current time value of clock `subscription_clock::id`.
pub(all) enum SubclockFlag {
  Subscription_clock_abstime
} derive(Eq, Show)

///|
/// Return the numeric value for a subclock flag.
pub fn SubclockFlag::value(self : SubclockFlag) -> UInt {
  match self {
    Subscription_clock_abstime => 1
  }
}

///|
/// Return a new subclock flags set with the given flag enabled.
pub fn SubclockFlags::set(
  self : SubclockFlags,
  flag : SubclockFlag,
) -> SubclockFlags {
  self.0 | flag.value()
}

///|
/// Return a new subclock flags set with the given flag disabled.
pub fn SubclockFlags::unset(
  self : SubclockFlags,
  flag : SubclockFlag,
) -> SubclockFlags {
  self.0 & (flag.value() ^ 0)
}

///|
/// Check whether a subclock flag is enabled.
pub fn SubclockFlags::is_set(self : SubclockFlags, flag : SubclockFlag) -> Bool {
  (self.0 & flag.value()) != 0
}

///|
/// The contents of a `subscription` when type is `eventtype::clock`.
/// 
/// - `id`: The clock against which to compare the timestamp.
/// - `timeout`: The absolute or relative timestamp.
/// - `precision`: The amount of time that the implementation may wait additionally to coalesce with other events.
/// - `flags`: Flags specifying whether the timeout is absolute or relative.
pub(all) struct Subscription_clock {
  id : ClockId
  timeout : TimeStamp
  precision : TimeStamp
  flags : SubclockFlags
} derive(Eq, Show)

///|
/// The contents of a `subscription` when type is `eventtype::fd_read` or `eventtype::fd_write`.
/// 
/// - `file_descriptor`: The file descriptor on which to wait for it to become ready for reading or writing.
pub(all) struct Subscription_fd_readwrite {
  file_descriptor : Fd
} derive(Eq, Show)

///|
/// The contents of a `subscription`.
pub(all) enum Subscription_u {
  Clock(Subscription_clock)
  Fd_read(Subscription_fd_readwrite)
  Fd_write(Subscription_fd_readwrite)
} derive(Eq, Show)

///|
/// Subscription to an event.
/// 
/// - `userdata`: User-provided value that is attached to the subscription in the implementaiton and returned through `event::userdata`.
/// - `u`: The type of the event to which to subscribe, and its contents.
pub(all) struct Subscription {
  userdata : Userdata
  u : Subscription_u
} derive(Eq, Show)

///|
/// Exit code generated by a process when exiting.
pub(all) struct ExitCode(UInt) derive(Eq, Show)

///|
/// Signal condition.
/// 
/// - None: No signal. Note that POSIX has special semantics for kill(pid, 0), so this value is reserved.
/// - Hup: Hangup. Action: Terminates the process.
/// - Inte: Terminate interrupt signal. Action: Terminates the process.
/// - Quit: Terminal quit signal. Action: Terminates the process.
/// - Ill: Illegal instruction. Action: Terminates the process.
/// - Trap: Trace/breakpoint trap. Action: Terminates the process.
/// - Abrt: Process abort signal. Action: Terminates the process.
/// - Bus: Access to an undefined portion of a memory object. Action: Terminates the process.
/// - Fpe: Erroneous arithmetic operation. Action: Terminates the process.
/// - Kill: Kill. Action: Terminates the process.
/// - Usr1: User-defined signal 1. Action: Terminates the process.
/// - Segv: Invalid memory reference. Action: Terminates the process.
/// - Usr2: User-defined signal 2. Action: Terminates the process.
/// - Pipe: Write on a pipe with no one to read it. Action: Ignored.
/// - Alrm: Alarm clock. Action: Terminates the process.
/// - Chld: Child process terminated, stopped, or continued. Action: Ignored.
/// - Cont: Continue executing, if stopped. Action: Continues the process.
/// - Stop: Stop executing. Action: Stops the process.
/// - Tstp: Terminal stop signal. Action: Stops the process.
/// - Ttin: Background process attempting read. Action: Stops the process.
/// - Ttou: Background process attempting write. Action: Stops the process.
/// - Urg: High bandwidth data is available at a socket. Action: Ignored.
/// - Xcpu: CPU time limit exceeded. Action: Terminates the process.
/// - Xfsz: File size limit exceeded. Action: Terminates the process.
/// - Vtalrm: Virtual timer expired. Action: Terminates the process.
/// - Prof: Profiling timer expired. Action: Terminates the process.
/// - Winch: Window changed. Action: Ignored.
/// - Poll: I/O possible. Action: Terminates the process.
/// - Pwr: Power failure. Action: Terminates the process.
/// - Sys: Bad system call. Action: Terminates the process.
pub(all) enum Signal {
  None
  Hup
  Inte
  Quit
  Ill
  Trap
  Abrt
  Bus
  Fpe
  Kill
  Usr1
  Segv
  Usr2
  Pipe
  Alrm
  Term
  Chld
  Cont
  Stop
  Tstp
  Ttin
  Ttou
  Urg
  Xcpu
  Xfsz
  Vtalrm
  Prof
  Winch
  Poll
  Pwr
  Sys
} derive(Eq, Show)

///|
/// Return the numeric value for a signal.
pub fn Signal::value(self : Signal) -> UInt {
  match self {
    None => 0
    Hup => 1
    Inte => 2
    Quit => 3
    Ill => 4
    Trap => 5
    Abrt => 6
    Bus => 7
    Fpe => 8
    Kill => 9
    Usr1 => 10
    Segv => 11
    Usr2 => 12
    Pipe => 13
    Alrm => 14
    Term => 15
    Chld => 16
    Cont => 17
    Stop => 18
    Tstp => 19
    Ttin => 20
    Ttou => 21
    Urg => 22
    Xcpu => 23
    Xfsz => 24
    Vtalrm => 25
    Prof => 26
    Winch => 27
    Poll => 28
    Pwr => 29
    Sys => 30
  }
}

///|
/// Convert a numeric signal into `Signal`.
pub fn Signal::from_value(value : UInt) -> Signal? {
  match value {
    0 => Some(None)
    1 => Some(Hup)
    2 => Some(Inte)
    3 => Some(Quit)
    4 => Some(Ill)
    5 => Some(Trap)
    6 => Some(Abrt)
    7 => Some(Bus)
    8 => Some(Fpe)
    9 => Some(Kill)
    10 => Some(Usr1)
    11 => Some(Segv)
    12 => Some(Usr2)
    13 => Some(Pipe)
    14 => Some(Alrm)
    15 => Some(Term)
    16 => Some(Chld)
    17 => Some(Cont)
    18 => Some(Stop)
    19 => Some(Tstp)
    20 => Some(Ttin)
    21 => Some(Ttou)
    22 => Some(Urg)
    23 => Some(Xcpu)
    24 => Some(Xfsz)
    25 => Some(Vtalrm)
    26 => Some(Prof)
    27 => Some(Winch)
    28 => Some(Poll)
    29 => Some(Pwr)
    30 => Some(Sys)
    _ => None
  }
}

///|
/// Flags provided to `sock_recv`.
pub(all) struct RiFlags(UInt) derive(Eq, Show)

///|
/// Create an empty set of recv flags.
pub fn RiFlags::new() -> RiFlags {
  0
}

///|
/// Flags provided to `sock_recv`.
/// 
/// - Recv_peek: Returns the message without removing it from the socket's receive queue.
/// - Recv_waitall: On byte-stream sockets, block until the full amount of data can be returned.
pub(all) enum RiFlag {
  Recv_peek
  Recv_waitall
} derive(Eq, Show)

///|
/// Return the numeric value for a recv flag.
pub fn RiFlag::value(self : RiFlag) -> UInt {
  match self {
    Recv_peek => 1
    Recv_waitall => 2
  }
}

///|
/// Return a new recv flags set with the given flag enabled.
pub fn RiFlags::set(self : RiFlags, flag : RiFlag) -> RiFlags {
  self.0 | flag.value()
}

///|
/// Return a new recv flags set with the given flag disabled.
pub fn RiFlags::unset(self : RiFlags, flag : RiFlag) -> RiFlags {
  self.0 & (flag.value() ^ 0)
}

///|
/// Check whether a recv flag is enabled.
pub fn RiFlags::is_set(self : RiFlags, flag : RiFlag) -> Bool {
  (self.0 & flag.value()) != 0
}

///|
/// Flags returned by `sock_recv`.
pub(all) struct RoFlags(UInt) derive(Eq, Show)

///|
/// Create an empty set of recv open flags.
pub fn RoFlags::new() -> RoFlags {
  0
}

///|
/// Flags returned by `sock_recv`.
/// 
/// - Recv_data_truncated: Returned by `sock_recv`: Message data has been truncated.
pub(all) enum RoFlag {
  Recv_data_truncated
} derive(Eq, Show)

///|
/// Return the numeric value for a recv open flag.
pub fn RoFlag::value(self : RoFlag) -> UInt {
  match self {
    Recv_data_truncated => 1
  }
}

///|
/// Return a new recv open flags set with the given flag enabled.
pub fn RoFlags::set(self : RoFlags, flag : RoFlag) -> RoFlags {
  self.0 | flag.value()
}

///|
/// Return a new recv open flags set with the given flag disabled.
pub fn RoFlags::unset(self : RoFlags, flag : RoFlag) -> RoFlags {
  self.0 & (flag.value() ^ 0)
}

///|
/// Check whether a recv open flag is enabled.
pub fn RoFlags::is_set(self : RoFlags, flag : RoFlag) -> Bool {
  (self.0 & flag.value()) != 0
}

///|
/// Flags provided to `sock_send`. As there are currently no flags defined, it must be set to zero.
pub(all) struct SiFlags(UInt) derive(Eq, Show)

///|
/// Create an empty set of send flags.
pub fn SiFlags::new() -> SiFlags {
  0
}

///|
/// Flags returned by `sock_send`. As there are currently no flags defined, it must be set to zero.
pub(all) enum SiFlag {} derive(Eq, Show)

///|
/// Return the numeric value for a send flag.
pub fn SiFlag::value(self : SiFlag) -> UInt {
  match self {
    _ => panic()
  }
}

///|
/// Return a new send flags set with the given flag enabled.
pub fn SiFlags::set(self : SiFlags, flag : SiFlag) -> SiFlags {
  self.0 | flag.value()
}

///|
/// Return a new send flags set with the given flag disabled.
pub fn SiFlags::unset(self : SiFlags, flag : SiFlag) -> SiFlags {
  self.0 & (flag.value() ^ 0)
}

///|
/// Check whether a send flag is enabled.
pub fn SiFlags::is_set(self : SiFlags, flag : SiFlag) -> Bool {
  (self.0 & flag.value()) != 0
}

///|
/// Which channels on a socket to shut down.
pub(all) struct SdFlags(UInt) derive(Eq, Show)

///|
/// Create an empty set of shutdown flags.
pub fn SdFlags::new() -> SdFlags {
  0
}

///|
/// Which channels on a socket to shut down.
/// 
/// - Rd: Disables further receive operations.
/// - Wr: Disables further send operations.
pub(all) enum SdFlag {
  Rd
  Wr
} derive(Eq, Show)

///|
/// Return the numeric value for a shutdown flag.
pub fn SdFlag::value(self : SdFlag) -> UInt {
  match self {
    Rd => 1
    Wr => 2
  }
}

///|
/// Return a new shutdown flags set with the given flag enabled.
pub fn SdFlags::set(self : SdFlags, flag : SdFlag) -> SdFlags {
  self.0 | flag.value()
}

///|
/// Return a new shutdown flags set with the given flag disabled.
pub fn SdFlags::unset(self : SdFlags, flag : SdFlag) -> SdFlags {
  self.0 & (flag.value() ^ 0)
}

///|
/// Check whether a shutdown flag is enabled.
pub fn SdFlags::is_set(self : SdFlags, flag : SdFlag) -> Bool {
  (self.0 & flag.value()) != 0
}

///|
/// Identifiers for preopened capabilites.
/// 
/// - Dir: A pre-opened directory.
pub(all) enum PreOpenType {
  Dir
} derive(Eq, Show)

///|
/// Return the numeric preopen type value.
pub fn PreOpenType::value(self : PreOpenType) -> UInt {
  match self {
    Dir => 0
  }
}

///|
/// Convert a numeric preopen type into `PreOpenType`.
pub fn PreOpenType::from_value(value : UInt) -> PreOpenType? {
  match value {
    0 => Some(Dir)
    _ => None
  }
}

///|
/// The content of a `prestat` when type is `preopentype::dir`.
/// 
/// - `pr_name_len`: The length of the directory name for use with `fd_prestat_dir_name`.
pub(all) struct PreStat_dir {
  pr_name_len : Size
} derive(Eq, Show)

///|
/// Information about a pre-opened capability.
pub(all) enum PreStat {
  Dir(PreStat_dir)
} derive(Eq, Show)

///|
/// Standard input file descriptor.
pub let stdin : Fd = 0

///|
/// Standard output file descriptor.
pub let stdout : Fd = 1

///|
/// Standard error file descriptor.
pub let stderr : Fd = 2
