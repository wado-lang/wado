///|
fn args_sizes_get_ffi(argc : Int, size : Int) -> UInt = "wasi_snapshot_preview1" "args_sizes_get"

///|
/// Return command-line argument data sizes.
/// 
/// @return Returns the number of arguments and the size of the argument string data, or an error.
pub fn args_sizes_get() -> (Int, Int) raise Errno {
  let argc_ptr = @ffi.malloc(4)
  defer @ffi.free(argc_ptr)
  let size_ptr = @ffi.malloc(4)
  defer @ffi.free(size_ptr)
  let ret = args_sizes_get_ffi(argc_ptr, size_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let argc = @ffi.load32(argc_ptr)
  let size = @ffi.load32(size_ptr)
  (argc, size)
}

///|
fn args_get_ffi(argv : Int, argv_buf : Int) -> UInt = "wasi_snapshot_preview1" "args_get"

///|
/// Read command-line argument data.
///
/// The size of the array should match that returned by `args_sizes_get`.
///
/// Each argument is **not** `\0` terminated.
///
/// The first argument should be a string containing the "name" of the
/// program. This need not be a usable filesystem path or even file name,
/// and may even be a fixed string. Subsequent arguments are the arguments
/// passed to the program by the user.
pub fn args_get() -> Array[String] raise Errno {
  let (argc, size) = args_sizes_get()
  if argc == 0 {
    return []
  }
  let arg_ptr_ptr = @ffi.malloc(argc * 4)
  defer @ffi.free(arg_ptr_ptr)
  let arg_buf = Bytes::make(size, b'\x00')
  let arg_buf_ptr = @ffi.bytes2ptr(arg_buf)
  defer @ffi.free(arg_buf_ptr)
  let ret = args_get_ffi(arg_ptr_ptr, arg_buf_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let args = []
  for i in 0..<argc {
    let arg_start = @ffi.load32(arg_ptr_ptr + i * 4) - arg_buf_ptr
    let arg_end = if i == argc - 1 {
      size
    } else {
      @ffi.load32(arg_ptr_ptr + (i + 1) * 4) - arg_buf_ptr
    }
    let len = arg_end - arg_start - 1 // remove trailing zero
    args.push(
      arg_buf[arg_start:arg_start + len] |> @encoding/utf8.decode_lossy(),
    )
  }
  args
}

///|
fn environ_sizes_get_ffi(argc : Int, size : Int) -> UInt = "wasi_snapshot_preview1" "environ_sizes_get"

///|
/// Return environment variable data sizes.
/// 
/// @return Returns the number of environment variables arguments and the size of the environment variable data.
pub fn environ_sizes_get() -> (Int, Int) raise Errno {
  let argc_ptr = @ffi.malloc(4)
  defer @ffi.free(argc_ptr)
  let size_ptr = @ffi.malloc(4)
  defer @ffi.free(size_ptr)
  let ret = environ_sizes_get_ffi(argc_ptr, size_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let argc = @ffi.load32(argc_ptr)
  let size = @ffi.load32(size_ptr)
  (argc, size)
}

///|
fn environ_get_ffi(environ : Int, environ_buf : Int) -> UInt = "wasi_snapshot_preview1" "environ_get"

///|
/// Read environment variable data.
/// 
/// The size of the array should match that returned by `environ_sizes_get`.
pub fn environ_get() -> Array[(String, String)] raise Errno {
  let (argc, size) = environ_sizes_get()
  if argc == 0 {
    return []
  }
  let environ_ptr_ptr = @ffi.malloc(argc * 4)
  defer @ffi.free(environ_ptr_ptr)
  let environ_buf = Bytes::make(size, b'\x00')
  let environ_buf_ptr = @ffi.bytes2ptr(environ_buf)
  defer @ffi.free(environ_buf_ptr)
  let ret = environ_get_ffi(environ_ptr_ptr, environ_buf_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let envs = []
  for i in 0..<argc {
    let env_start = @ffi.load32(environ_ptr_ptr + i * 4) - environ_buf_ptr
    let env_end = if i == argc - 1 {
      size
    } else {
      @ffi.load32(environ_ptr_ptr + (i + 1) * 4) - environ_buf_ptr
    }
    let len = env_end - env_start - 1 // remove trailing zero
    let str = environ_buf[env_start:env_start + len]
      |> @encoding/utf8.decode_lossy()
    guard str.split("=").collect() is [key, .. value]
    envs.push(
      (key.to_string(), value.iter().map(StringView::to_string).join("=")),
    )
  }
  envs
}

///|
fn clock_res_get_ffi(clock_id : UInt, resolution : Int) -> UInt = "wasi_snapshot_preview1" "clock_res_get"

///|
/// Return the resolution of a clock.
/// 
/// Implementations are required to provide a non-zero value for supported clocks.
/// For unsupported clocks, return `errno::inval`.
/// 
/// Note: This is similar to `clock_getres` in POSIX.
/// 
/// @param `self` The clock for which to return the resolution.  
/// @return The resolution of the clock, or an error if one happened.
pub fn ClockId::clock_res_get(self : ClockId) -> TimeStamp raise Errno {
  let resolution_ptr = @ffi.malloc(8)
  defer @ffi.free(resolution_ptr)
  let ret = clock_res_get_ffi(self.value(), resolution_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let resolution = @ffi.load64_u(resolution_ptr)
  resolution
}

///|
fn clock_time_get_ffi(clock_id : UInt, precision : UInt64, time : Int) -> UInt = "wasi_snapshot_preview1" "clock_time_get"

///|
/// Return the time value of a clock
/// 
/// Note: This is similar to `clock_gettime` in POSIX.
/// 
/// @param `self` The clodk for which to return the time.  
/// @param `precision` The maximum lag (exclusive) that the returned time value may have, compared to its actual value.  
/// @return The time value of the clock.
pub fn ClockId::clock_time_get(
  self : ClockId,
  precision : TimeStamp,
) -> TimeStamp raise Errno {
  let time_ptr = @ffi.malloc(8)
  defer @ffi.free(time_ptr)
  let ret = clock_time_get_ffi(self.value(), precision.0, time_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let time = @ffi.load64_u(time_ptr)
  time
}

///|
fn fd_advise_ffi(
  fd : Int,
  offset : UInt64,
  len : UInt64,
  advice : UInt,
) -> UInt = "wasi_snapshot_preview1" "fd_advise"

///|
/// Provide file advisory information on a file ddescriptor
/// 
/// Note: This is similar to `posix_fadvise` in POSIX.
/// 
/// @param `offset` The offset within the file to which the advisory applies.  
/// @param `len` The length of the region to which the advisory applies.  
/// @param `advice` The advice.
pub fn Fd::fd_advise(
  self : Fd,
  offset : FileSize,
  len : FileSize,
  advice : Advice,
) -> Unit raise Errno {
  let ret = fd_advise_ffi(self.0, offset.0, len.0, advice.value())
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn fd_allocate_ffi(fd : Int, offset : UInt64, len : UInt64) -> UInt = "wasi_snapshot_preview1" "fd_allocate"

///|
/// Force the allocaiton of space in a file.
/// 
/// Note: This is similar to `posix_fallocate` in POSIX.
/// 
/// @param `offset` The offset at which to start allocating.  
/// @param `len` The length of the area that is allocated.  
/// @return The error code.
pub fn Fd::fd_allocate(
  self : Fd,
  offset : FileSize,
  len : FileSize,
) -> Unit raise Errno {
  let ret = fd_allocate_ffi(self.0, offset.0, len.0)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn fd_close_ffi(fd : Int) -> UInt = "wasi_snapshot_preview1" "fd_close"

///|
/// Close a file descriptor.
/// 
/// Note: This is similar to `close` in POSIX.
pub fn Fd::fd_close(self : Fd) -> Unit raise Errno {
  let ret = fd_close_ffi(self.0)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn fd_datasync_ffi(fd : Int) -> UInt = "wasi_snapshot_preview1" "fd_datasync"

///|
/// Synchronize the data of a file to disk.
/// 
/// Note: This is similar to `fdatasync` in POSIX.
pub fn Fd::fd_datasync(self : Fd) -> Unit raise Errno {
  let ret = fd_datasync_ffi(self.0)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn fd_fdstat_get_ffi(fd : Int, buf : Int) -> UInt = "wasi_snapshot_preview1" "fd_fdstat_get"

///|
/// Get the attributes of a file descriptor.
/// 
/// Note: This returns similar flags to `fcntl(fd, F_GETFL)` in POSIX, as well as additional fields.
pub fn Fd::fd_fdstat_get(self : Fd) -> FdStat raise Errno {
  let buf_ptr = @ffi.malloc(24)
  defer @ffi.free(buf_ptr)
  let ret = fd_fdstat_get_ffi(self.0, buf_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let fs_filetype : FileType = @ffi.load8_u(buf_ptr)
    |> FileType::from_value
    |> Option::unwrap
  let fs_flags : FdFlags = @ffi.load16_u(buf_ptr + 2)
  let fs_rights_base : Rights = @ffi.load64_u(buf_ptr + 8)
  let fs_rights_inheriting : Rights = @ffi.load64_u(buf_ptr + 16)
  let buf = FdStat::{
    fs_filetype,
    fs_flags,
    fs_rights_base,
    fs_rights_inheriting,
  }
  buf
}

///|
fn fd_fdstat_set_flags_ffi(fd : Int, flags : UInt) -> UInt = "wasi_snapshot_preview1" "fd_fdstat_set_flags"

///|
/// Adjust the flags associated with a file descriptor.
/// 
/// Note: This is similar to `fcntl(fd, F_SETFL, flags)` in POSIX.
/// 
/// @param `flags` The desired values of the file descriptor flags.
pub fn Fd::fd_fdstat_set_flags(self : Fd, flags : FdFlags) -> Unit raise Errno {
  let ret = fd_fdstat_set_flags_ffi(self.0, flags.0)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn fd_fdstat_set_rights_ffi(
  fd : Int,
  fs_rights_base : UInt64,
  fs_rights_inheriting : UInt64,
) -> UInt = "wasi_snapshot_preview1" "fd_fdstat_set_rights"

///|
/// Adjust the rights associated with a file descriptor.
/// 
/// This can only be used to remove rights, and returns `errno::notcapable` if called in a way that would attempt to add rights.
/// 
/// @param `fs_rights_base` The desired rights of the file descriptor.
pub fn Fd::fd_fdstat_set_rights(
  self : Fd,
  fs_rights_base : Rights,
  fs_rights_inheriting : Rights,
) -> Unit raise Errno {
  let ret = fd_fdstat_set_rights_ffi(
    self.0,
    fs_rights_base.0,
    fs_rights_inheriting.0,
  )
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn fd_filestat_get_ffi(fd : Int, buf : Int) -> UInt = "wasi_snapshot_preview1" "fd_filestat_get"

///|
/// Return the attributes of an open file
pub fn Fd::fd_filestat_get(self : Fd) -> FileStat raise Errno {
  let buf_ptr = @ffi.malloc(64)
  defer @ffi.free(buf_ptr)
  let ret = fd_filestat_get_ffi(self.0, buf_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let dev : Device = @ffi.load64_u(buf_ptr)
  let ino : Inode = @ffi.load64_u(buf_ptr + 8)
  let filetype : FileType = @ffi.load8_u(buf_ptr + 16)
    |> FileType::from_value
    |> Option::unwrap
  let nlink : LinkCount = @ffi.load64_u(buf_ptr + 24)
  let size : FileSize = @ffi.load64_u(buf_ptr + 32)
  let atim : TimeStamp = @ffi.load64_u(buf_ptr + 40)
  let mtim : TimeStamp = @ffi.load64_u(buf_ptr + 48)
  let ctim : TimeStamp = @ffi.load64_u(buf_ptr + 56)
  let buf = FileStat::{ dev, ino, filetype, nlink, size, atim, mtim, ctim }
  buf
}

///|
fn fd_filestat_set_size_ffi(ffi : Int, size : UInt64) -> UInt = "wasi_snapshot_preview1" "fd_filestat_set_size"

///|
/// Adjust the size of an open file. If this increases the file's size, the extra bytes are filled with zeros.
/// 
/// Note: This is similar to `ftruncate` in POSIX.
/// 
/// @param `size` The desired file size.
pub fn Fd::fd_filestat_set_size(self : Fd, size : FileSize) -> Unit raise Errno {
  let ret = fd_filestat_set_size_ffi(self.0, size.0)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn fd_filestat_set_times_ffi(
  fd : Int,
  atim : UInt64,
  mtim : UInt64,
  fst_flags : UInt,
) -> UInt = "wasi_snapshot_preview1" "fd_filestat_set_times"

///|
/// Adjust the timestamps of an open file or directory.
/// 
/// Note: This is similar to `futimens` in POSIX.
/// 
/// @param `atim` The desired value of the data access timestamp.  
/// @param `mtim` The desired value of the data modification timestamp.  
/// @param `fst_flags` A bitmask indicating which timestamps to adjust.
pub fn Fd::fd_filestat_set_times(
  self : Fd,
  atim : TimeStamp,
  mtim : TimeStamp,
  fst_flags : FstFlags,
) -> Unit raise Errno {
  let ret = fd_filestat_set_times_ffi(self.0, atim.0, mtim.0, fst_flags.0)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn fd_pread_ffi(
  fd : Int,
  iovs : Int,
  iovs_len : Int,
  offset : UInt64,
  nread : Int,
) -> UInt = "wasi_snapshot_preview1" "fd_pread"

///|
/// Read from a file descriptor, without using and updating the file descriptor's offset.
///
/// Note: This is similar to `preadv` in Linux (and other Unix-es).
pub fn Fd::fd_pread(
  self : Fd,
  iovs : Array[FixedArray[Byte]],
  offset : FileSize,
) -> Size raise Errno {
  let iovs_len = iovs.length()
  let iovs_ptr = @ffi.malloc(iovs_len * 8)
  defer @ffi.free(iovs_ptr)
  for i, byte in iovs {
    let ptr = @ffi.byte_array2ptr(byte)
    @ffi.store32(iovs_ptr + i * 8, ptr)
    @ffi.store32(iovs_ptr + i * 8 + 4, byte.length())
  }
  defer (for i in 0..<iovs_len {
    @ffi.free(@ffi.load32(iovs_ptr + i * 8))
  })
  let nread_ptr = @ffi.malloc(4)
  defer @ffi.free(nread_ptr)
  let ret = fd_pread_ffi(self.0, iovs_ptr, iovs_len, offset.0, nread_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let nread = @ffi.load32_u(nread_ptr)
  nread
}

///|
fn fd_prestat_get_ffi(fd : Int, buf : Int) -> UInt = "wasi_snapshot_preview1" "fd_prestat_get"

///|
/// Return a description of the given preopened file descriptor.
/// 
/// Note: consider using `fd_prestat_dir_name` directly.
pub fn Fd::fd_prestat_get(self : Fd) -> PreStat raise Errno {
  let buf_ptr = @ffi.malloc(8)
  defer @ffi.free(buf_ptr)
  let ret = fd_prestat_get_ffi(self.0, buf_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  // There's no other possibility of tag...
  guard @ffi.load8_u(buf_ptr) is 0
  let size : Size = @ffi.load32_u(buf_ptr + 4)
  let buf = PreStat::Dir(PreStat_dir::{ pr_name_len: size })
  buf
}

///|
fn fd_prestat_dir_name_ffi(fd : Int, path : Int, path_len : Int) -> UInt = "wasi_snapshot_preview1" "fd_prestat_dir_name"

///|
/// Return a description of the given preopened file descriptor.
pub fn Fd::fd_prestat_dir_name(self : Fd) -> String raise Errno {
  let prestat = self.fd_prestat_get()
  guard prestat is Dir({ pr_name_len: Size(size) })
  let buffer = Bytes::make(size.reinterpret_as_int(), b'\x00')
  let buf_ptr = @ffi.bytes2ptr(buffer)
  defer @ffi.free(buf_ptr)
  let ret = fd_prestat_dir_name_ffi(self.0, buf_ptr, size.reinterpret_as_int())
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  if buffer is [.. rest, b'\x00'] {
    @encoding/utf8.decode_lossy(rest)
  } else {
    @encoding/utf8.decode_lossy(buffer)
  }
}

///|
fn fd_read_ffi(fd : Int, iovs : Int, iovs_len : Int, nread : Int) -> UInt = "wasi_snapshot_preview1" "fd_read"

///|
/// Read from a file descriptor.
///
/// Note: This is similar to `readv` in POSIX.
pub fn Fd::fd_read(
  self : Fd,
  iovs : Array[FixedArray[Byte]],
) -> Size raise Errno {
  let iovs_len = iovs.length()
  let iovs_ptr = @ffi.malloc(iovs_len * 8)
  defer @ffi.free(iovs_ptr)
  for i, byte in iovs {
    let ptr = @ffi.byte_array2ptr(byte)
    @ffi.store32(iovs_ptr + i * 8, ptr)
    @ffi.store32(iovs_ptr + i * 8 + 4, byte.length())
  }
  defer (for i in 0..<iovs_len {
    @ffi.free(@ffi.load32(iovs_ptr + i * 8))
  })
  let nread_ptr = @ffi.malloc(4)
  defer @ffi.free(nread_ptr)
  let ret = fd_read_ffi(self.0, iovs_ptr, iovs_len, nread_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let nread = @ffi.load32_u(nread_ptr)
  nread
}

///|
fn fd_readdir_ffi(
  fd : Int,
  buf : Int,
  buf_len : Int,
  cookie : UInt64,
  buf_used : Int,
) -> UInt = "wasi_snapshot_preview1" "fd_readdir"

///|
/// Read directory entries from a directory.
/// When successful, the contents of the output buffer consist of a sequence of
/// directory entries. Each directory entry consists of a `dirent` object,
/// followed by `dirent::d_namlen` bytes holding the name of the directory
/// entry.
///
/// This function fills the output buffer as much as possible, potentially
/// truncating the last directory entry. This allows the caller to grow its
/// read buffer size in case it's too small to fit a single large directory
/// entry, or skip the oversized directory entry.
///
/// Entries for the special `.` and `..` directory entries are included in the
/// sequence.
/// 
/// @param `cookie`: The location within the directory to start reading.  
/// @param `size`: The buffer used to read.  
/// @return The entries untruncated and whether there's still other entries.
pub fn Fd::fd_readdir(
  self : Fd,
  cookie? : DirCookie = DirCookie(0),
  buffer? : Bytes = Bytes::make(4096, 0),
) -> (Array[(String, DirEnt)], Bool) raise Errno {
  let buf_len = buffer.length()
  let buf_ptr = @ffi.bytes2ptr(buffer)
  defer @ffi.free(buf_ptr)
  let buf_used_ptr = @ffi.malloc(4)
  defer @ffi.free(buf_used_ptr)
  let ret = fd_readdir_ffi(self.0, buf_ptr, buf_len, cookie.0, buf_used_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let buf_used = @ffi.load32(buf_used_ptr)
  let entries = []
  for i = 0; i < buf_used; {
    if i + 24 > buf_used {
      // truncated
      break (entries, true)
    }
    let d_next : DirCookie = @ffi.load64_u(buf_ptr + i)
    let d_ino : Inode = @ffi.load64_u(buf_ptr + i + 8)
    let namlen = @ffi.load32(buf_ptr + i + 16)
    let d_namlen : DirNamlen = namlen.reinterpret_as_uint()
    let d_type : FileType = @ffi.load8_u(buf_ptr + i + 20)
      |> FileType::from_value
      |> Option::unwrap
    if i + 24 + namlen > buf_used {
      // truncated
      break (entries, true)
    }
    entries.push(
      (
        buffer[i + 24:i + 24 + namlen] |> @encoding/utf8.decode_lossy(),
        DirEnt::{ d_next, d_ino, d_namlen, d_type },
      ),
    )
    continue i + 24 + namlen
  } else {
    (entries, buf_used == buf_len)
  }
}

///|
fn fd_renumber_ffi(fd : Int, to : Int) -> UInt = "wasi_snapshot_preview1" "fd_renumber"

///|
/// Atomically replace a file descriptor by renumbering another file descriptor.
///
/// Due to the strong focus on thread safety, this environment does not provide
/// a mechanism to duplicate or renumber a file descriptor to an arbitrary
/// number, like `dup2()`. This would be prone to race conditions, as an actual
/// file descriptor with the same number could be allocated by a different
/// thread at the same time.
///
/// This function provides a way to atomically renumber file descriptors, which
/// would disappear if `dup2()` were to be removed entirely.
pub fn Fd::fd_renumber(self : Fd, to : Fd) -> Unit raise Errno {
  let ret = fd_renumber_ffi(self.0, to.0)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn fd_seek_ffi(
  fd : Int,
  offset : Int64,
  whence : UInt,
  newoffset : Int,
) -> UInt = "wasi_snapshot_preview1" "fd_seek"

///|
/// Move the offset of a file descriptor.
/// 
/// Note: This is similar to `lseek` in POSIX.
/// 
/// @param `offset` The number of bytes to move.
/// @param `whence` The base from which the offset is calculated.
/// @return The new offset of the file descriptor, relative to the start of the file.
pub fn Fd::fd_seek(
  self : Fd,
  offset : FileDelta,
  whence : Whence,
) -> FileSize raise Errno {
  let newoffset_ptr = @ffi.malloc(8)
  defer @ffi.free(newoffset_ptr)
  let ret = fd_seek_ffi(self.0, offset.0, whence.value(), newoffset_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let newoffset = @ffi.load64_u(newoffset_ptr)
  newoffset
}

///|
fn fd_sync_ffi(fd : Int) -> UInt = "wasi_snapshot_preview1" "fd_sync"

///|
/// Synchronize the data and metadata of a file to disk.
/// 
/// Note: This is similar to `fsync` in POSIX.
pub fn Fd::fd_sync(self : Fd) -> Unit raise Errno {
  let ret = fd_sync_ffi(self.0)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn fd_tell_ffi(fd : Int, newoffset : Int) -> UInt = "wasi_snapshot_preview1" "fd_tell"

///|
/// Return the current offset of a file descriptor.
/// 
/// Note: This is similar to `lseek(fd, 0, SEEK_CUR)` in POSIX.
/// 
/// @return The current offset of the file descriptor, relative to the start of the file.
pub fn Fd::fd_tell(self : Fd) -> FileSize raise Errno {
  let newoffset_ptr = @ffi.malloc(8)
  defer @ffi.free(newoffset_ptr)
  let ret = fd_tell_ffi(self.0, newoffset_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let newoffset = @ffi.load64_u(newoffset_ptr)
  newoffset
}

///|
fn fd_write_ffi(fd : Int, iovs : Int, iovs_len : Int, nwritten : Int) -> UInt = "wasi_snapshot_preview1" "fd_write"

///|
/// Write to a file descriptor.
///
/// Note: This is similar to `writev` in POSIX.
///
/// Like POSIX, any calls of `write` (and other functions to read or write)
/// for a regular file by other threads in the WASI process should not be
/// interleaved while `write` is executed.
///
/// Note: it may write less than demanded.
pub fn Fd::fd_write(self : Fd, contents : Array[BytesView]) -> Size raise Errno {
  let iovs_len = contents.length()
  let iovs_ptr = @ffi.malloc(iovs_len * 8)
  defer @ffi.free(iovs_ptr)
  for i, byte in contents {
    let ptr = @ffi.bytes2ptr(byte.data())
    @ffi.store32(iovs_ptr + i * 8, ptr + byte.start_offset())
    @ffi.store32(iovs_ptr + i * 8 + 4, byte.length())
  }
  defer (for i in contents {
    let ptr = @ffi.bytes2ptr(i.data())
    @ffi.free(ptr)
    @ffi.free(ptr)
  })
  let nwritten_ptr = @ffi.malloc(4)
  defer @ffi.free(nwritten_ptr)
  let ret = fd_write_ffi(self.0, iovs_ptr, iovs_len, nwritten_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let nwritten = @ffi.load32_u(nwritten_ptr)
  nwritten
}

///|
fn path_create_directory_ffi(fd : Int, path : Int, path_len : Int) -> UInt = "wasi_snapshot_preview1" "path_create_directory"

///|
/// Create a directory.
///
/// Note: This is similar to `mkdirat` in POSIX.
///
/// @param `path` The path at which to create the directory.
pub fn Fd::path_create_directory(self : Fd, path : String) -> Unit raise Errno {
  let path_bytes = path |> @encoding/utf8.encode()
  let path_len = path_bytes.length()
  let path_ptr = @ffi.bytes2ptr(path_bytes)
  defer @ffi.free(path_ptr)
  let ret = path_create_directory_ffi(self.0, path_ptr, path_len)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn path_filestat_get_ffi(
  fd : Int,
  flags : UInt,
  path : Int,
  path_len : Int,
  buf : Int,
) -> UInt = "wasi_snapshot_preview1" "path_filestat_get"

///|
/// Return the attributes of a file or directory.
///
/// Note: This is similar to `stat` in POSIX.
///
/// @param `flags` Flags determining the method of how the path is resolved.
/// @param `path` The path of the file or directory to inspect.
pub fn Fd::path_filestat_get(
  self : Fd,
  flags : LookupFlags,
  path : String,
) -> FileStat raise Errno {
  let path_bytes = path |> @encoding/utf8.encode()
  let path_len = path_bytes.length()
  let path_ptr = @ffi.bytes2ptr(path_bytes)
  defer @ffi.free(path_ptr)
  let buf_ptr = @ffi.malloc(64)
  defer @ffi.free(buf_ptr)
  let ret = path_filestat_get_ffi(self.0, flags.0, path_ptr, path_len, buf_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let dev : Device = @ffi.load64_u(buf_ptr)
  let ino : Inode = @ffi.load64_u(buf_ptr + 8)
  let filetype : FileType = @ffi.load8_u(buf_ptr + 16)
    |> FileType::from_value
    |> Option::unwrap
  let nlink : LinkCount = @ffi.load64_u(buf_ptr + 24)
  let size : FileSize = @ffi.load64_u(buf_ptr + 32)
  let atim : TimeStamp = @ffi.load64_u(buf_ptr + 40)
  let mtim : TimeStamp = @ffi.load64_u(buf_ptr + 48)
  let ctim : TimeStamp = @ffi.load64_u(buf_ptr + 56)
  let buf = FileStat::{ dev, ino, filetype, nlink, size, atim, mtim, ctim }
  buf
}

///|
fn path_filestat_set_times_ffi(
  fd : Int,
  flags : UInt,
  path : Int,
  path_len : Int,
  atim : UInt64,
  mtim : UInt64,
  fst_flags : UInt,
) -> UInt = "wasi_snapshot_preview1" "path_filestat_set_times"

///|
/// Adjust the timestamps of a file or directory.
///
/// Note: This is similar to `utimensat` in POSIX.
///
/// @param `flag` Flags determining the method of how the path is resolved.
/// @param `path` The path of the file or directory to operate on.
/// @param `atim` The desired values of the data access timestamp.
/// @param `mtim` The desired values of the data modification timestamp.
/// @param `fst_flags` A bitmask indicating which timestamps to adjust.
pub fn Fd::path_filestat_set_times(
  self : Fd,
  flags : LookupFlags,
  path : String,
  atim : TimeStamp,
  mtim : TimeStamp,
  fst_flags : FstFlags,
) -> Unit raise Errno {
  let path_bytes = path |> @encoding/utf8.encode()
  let path_len = path_bytes.length()
  let path_ptr = @ffi.bytes2ptr(path_bytes)
  defer @ffi.free(path_ptr)
  let ret = path_filestat_set_times_ffi(
    self.0,
    flags.0,
    path_ptr,
    path_len,
    atim.0,
    mtim.0,
    fst_flags.0,
  )
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn path_link_ffi(
  old_fd : Int,
  old_flags : UInt,
  old_path : Int,
  old_path_len : Int,
  new_fd : Int,
  new_path : Int,
  new_path_len : Int,
) -> UInt = "wasi_snapshot_preview1" "path_link"

///|
/// Create a hard link.
///
/// Note: This is similar to `linkat` in POSIX.
///
/// @param `old_flags` Flags determining the method of how the path is resolved.
/// @param `old_path` The source path from which to link.
/// @param `new_fd` The wroking directory at which the resolution of the new path starts.
/// @param `new_path` The destination path at which to create the hard link.
pub fn Fd::path_link(
  self : Fd,
  old_flags : LookupFlags,
  old_path : String,
  new_fd : Fd,
  new_path : String,
) -> Unit raise Errno {
  let old_path_bytes = old_path |> @encoding/utf8.encode()
  let old_path_len = old_path_bytes.length()
  let old_path_ptr = @ffi.bytes2ptr(old_path_bytes)
  defer @ffi.free(old_path_ptr)
  let new_path_bytes = new_path |> @encoding/utf8.encode()
  let new_path_len = new_path_bytes.length()
  let new_path_ptr = @ffi.bytes2ptr(new_path_bytes)
  defer @ffi.free(new_path_ptr)
  let ret = path_link_ffi(
    self.0,
    old_flags.0,
    old_path_ptr,
    old_path_len,
    new_fd.0,
    new_path_ptr,
    new_path_len,
  )
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn path_open_ffi(
  dirfd : Int,
  dirflags : UInt,
  path : Int,
  path_len : Int,
  oflags : UInt,
  fs_rights_base : UInt64,
  fs_rights_inheriting : UInt64,
  fs_flags : UInt,
  fd : Int,
) -> UInt = "wasi_snapshot_preview1" "path_open"

///|
/// Open a file or directory.
///
/// The returned file descriptor is not guaranteed to be the lowest-numbered
/// file descriptor not currently open; it is randomized to prevent
/// applications from depending on making assumptions about indexes, since this
/// is error-prone in multi-threaded contexts. The returned file descriptor is
/// guaranteed to be less than 2^31.
///
/// Note: This is similar to `openat` in POSIX.
///
/// @param `dirflags` Flags determining the method of how the path is resolved.
/// @param `path` The path of the file or directory to open.
/// @param `oflags` The method by which to open the file.
/// @param `fs_rights_base` The initial rights of the newly created file descriptor. The
///        implementation is allowed to return a file descriptor with fewer rights
///        than specified, if and only if those rights do not apply to the type of
///        file being opened. The *base* rights are rights that will apply to operations
///        using the file descriptor itself.
/// @param `fs_rights_inheriting` The initial rights of the newly created file descriptor. The
///        implementation is allowed to return a file descriptor with fewer rights
///        than specified, if and only if those rights do not apply to the type of
///        file being opened. The *inheriting* rights are rights that apply to
///        file descriptors derived from it.
/// @return The file descriptor of the file that has been opened.
pub fn Fd::path_open(
  self : Fd,
  dirflags : LookupFlags,
  path : String,
  oflags : OpenFlags,
  fs_rights_base : Rights,
  fs_rights_inheriting : Rights,
  fs_flags : FdFlags,
) -> Fd raise Errno {
  let path_bytes = path |> @encoding/utf8.encode()
  let path_len = path_bytes.length()
  let path_ptr = @ffi.bytes2ptr(path_bytes)
  defer @ffi.free(path_ptr)
  let fd_ptr = @ffi.malloc(4)
  defer @ffi.free(fd_ptr)
  let ret = path_open_ffi(
    self.0,
    dirflags.0,
    path_ptr,
    path_len,
    oflags.0,
    fs_rights_base.0,
    fs_rights_inheriting.0,
    fs_flags.0,
    fd_ptr,
  )
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let fd = @ffi.load32(fd_ptr)
  Fd(fd)
}

///|
fn path_readlink_ffi(
  fd : Int,
  path : Int,
  path_len : Int,
  buf : Int,
  buf_len : Int,
  buf_used : Int,
) -> UInt = "wasi_snapshot_preview1" "path_readlink"

///|
/// Read the contents of a symbolic link.
///
/// Note: This is similar to `readlinkat` in POSIX. If `buf` is not large
/// enough to store the contents of the link, the first `buf_len` bytes will be
/// written `buf`.
pub fn Fd::path_readlink(
  self : Fd,
  path : String,
  buffer? : FixedArray[Byte] = FixedArray::make(4096, 0),
) -> Size raise Errno {
  let path_bytes = path |> @encoding/utf8.encode()
  let path_len = path_bytes.length()
  let path_ptr = @ffi.bytes2ptr(path_bytes)
  defer @ffi.free(path_ptr)
  let buf_len = buffer.length()
  let buf_ptr = @ffi.byte_array2ptr(buffer)
  defer @ffi.free(buf_ptr)
  let buf_used_ptr = @ffi.malloc(4)
  defer @ffi.free(buf_used_ptr)
  let ret = path_readlink_ffi(
    self.0,
    path_ptr,
    path_len,
    buf_ptr,
    buf_len,
    buf_used_ptr,
  )
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let buf_used = @ffi.load32_u(buf_used_ptr)
  buf_used
}

///|
fn path_remove_directory_ffi(fd : Int, path : Int, path_len : Int) -> UInt = "wasi_snapshot_preview1" "path_remove_directory"

///|
/// Remove a directory.
///
/// Return `errno::notempty` if the directory is not empty.
///
/// Note: This is similar to `unlinkat(fd, path, AT_REMOVEDIR)` in POSIX.
pub fn Fd::path_remove_directory(self : Fd, path : String) -> Unit raise Errno {
  let path_bytes = path |> @encoding/utf8.encode()
  let path_len = path_bytes.length()
  let path_ptr = @ffi.bytes2ptr(path_bytes)
  defer @ffi.free(path_ptr)
  let ret = path_remove_directory_ffi(self.0, path_ptr, path_len)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn path_rename_ffi(
  old_fd : Int,
  old_path : Int,
  old_path_len : Int,
  new_fd : Int,
  new_path : Int,
  new_path_len : Int,
) -> UInt = "wasi_snapshot_preview1" "path_rename"

///|
/// Rename a file or directory.
///
/// Note: This is similar to `renameat` in POSIX.
///
/// @param `old_path` The source path of the file or directory to rename.
/// @param `new_fd` The working directory at which the resolution of the new path starts.
/// @param `new_path` The destination path of the file or directory to rename.
pub fn Fd::path_rename(
  self : Fd,
  old_path : String,
  new_fd : Fd,
  new_path : String,
) -> Unit raise Errno {
  let old_path_bytes = old_path |> @encoding/utf8.encode()
  let old_path_len = old_path_bytes.length()
  let old_path_ptr = @ffi.bytes2ptr(old_path_bytes)
  defer @ffi.free(old_path_ptr)
  let new_path_bytes = new_path |> @encoding/utf8.encode()
  let new_path_len = new_path_bytes.length()
  let new_path_ptr = @ffi.bytes2ptr(new_path_bytes)
  defer @ffi.free(new_path_ptr)
  let ret = path_rename_ffi(
    self.0,
    old_path_ptr,
    old_path_len,
    new_fd.0,
    new_path_ptr,
    new_path_len,
  )
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn path_symlink_ffi(
  old_path : Int,
  old_path_len : Int,
  fd : Int,
  new_path : Int,
  new_path_len : Int,
) -> UInt = "wasi_snapshot_preview1" "path_symlink"

///|
/// Create a symbolic link.
///
/// Note: This is similar to `symlinkat` in POSIX.
pub fn path_symlink(
  old_path : String,
  fd : Fd,
  new_path : String,
) -> Unit raise Errno {
  let old_path_bytes = old_path |> @encoding/utf8.encode()
  let old_path_len = old_path_bytes.length()
  let old_path_ptr = @ffi.bytes2ptr(old_path_bytes)
  defer @ffi.free(old_path_ptr)
  let new_path_bytes = new_path |> @encoding/utf8.encode()
  let new_path_len = new_path_bytes.length()
  let new_path_ptr = @ffi.bytes2ptr(new_path_bytes)
  defer @ffi.free(new_path_ptr)
  let ret = path_symlink_ffi(
    old_path_ptr,
    old_path_len,
    fd.0,
    new_path_ptr,
    new_path_len,
  )
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn path_unlink_file_ffi(fd : Int, path : Int, path_len : Int) -> UInt = "wasi_snapshot_preview1" "path_unlink_file"

///|
/// Unlink a file.
///
/// Return `errno::isdir` if the path refers to a directory.
///
/// Note: This is similar to `unlinkat(fd, path, 0)` in POSIX.
pub fn Fd::path_unlink_file(self : Fd, path : String) -> Unit raise Errno {
  let path_bytes = path |> @encoding/utf8.encode()
  let path_len = path_bytes.length()
  let path_ptr = @ffi.bytes2ptr(path_bytes)
  defer @ffi.free(path_ptr)
  let ret = path_unlink_file_ffi(self.0, path_ptr, path_len)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn poll_oneoff_ffi(
  in_ : Int,
  out : Int,
  nsubscriptions : Int,
  nevents : Int,
) -> UInt = "wasi_snapshot_preview1" "poll_oneoff"

///|
/// Concurrently poll for the occurrence of a set of events.
///
/// If `nsubscriptions` is 0, returns `errno::inval`.
///
/// @param `in_` The events to which to subscribe.
/// @return The events that have occurred.
pub fn poll_oneoff(in_ : Array[Subscription]) -> Array[Event] raise Errno {
  let nsubscriptions = in_.length()
  let in_ptr = @ffi.malloc(nsubscriptions * 48)
  defer @ffi.free(in_ptr)
  let out_ptr = @ffi.malloc(nsubscriptions * 32)
  defer @ffi.free(out_ptr)
  for i, sub in in_ {
    @ffi.store64_u(in_ptr + i * 48, sub.userdata.0)
    match sub.u {
      Clock({ id, timeout, precision, flags }) => {
        @ffi.store32(in_ptr + i * 48 + 8, 0)
        @ffi.store32_u(in_ptr + i * 48 + 8 + 8 + 0, id.value())
        @ffi.store64_u(in_ptr + i * 48 + 8 + 8 + 8, timeout.0)
        @ffi.store64_u(in_ptr + i * 48 + 8 + 8 + 16, precision.0)
        @ffi.store32_u(in_ptr + i * 48 + 8 + 8 + 24, flags.0)
      }
      Fd_read({ file_descriptor }) => {
        @ffi.store32(in_ptr + i * 48 + 8, 1)
        @ffi.store32(in_ptr + i * 48 + 16, file_descriptor.0)
      }
      Fd_write({ file_descriptor }) => {
        @ffi.store32(in_ptr + i * 48 + 8, 2)
        @ffi.store32(in_ptr + i * 48 + 16, file_descriptor.0)
      }
    }
  }
  let nevents_ptr = @ffi.malloc(4)
  defer @ffi.free(nevents_ptr)
  let ret = poll_oneoff_ffi(in_ptr, out_ptr, nsubscriptions, nevents_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let nevents = @ffi.load32(nevents_ptr)
  let events = []
  for i in 0..<nevents {
    let userdata : Userdata = @ffi.load64_u(out_ptr + i * 32)
    let errno : Errno = @ffi.load16_u(out_ptr + i * 32 + 8)
      |> Errno::from_value
      |> Option::unwrap
    let type_ : EventType = @ffi.load16_u(out_ptr + i * 32 + 10)
      |> EventType::from_value
      |> Option::unwrap
    let nbytes : FileSize = @ffi.load64_u(out_ptr + i * 32 + 16)
    let flags : EventRWFlags = @ffi.load32_u(out_ptr + i * 32 + 16 + 8)
    let fd_readwrite : Event_fd_readwrite = { nbytes, flags }
    events.push(Event::{ userdata, error: errno, type_, fd_readwrite })
  }
  events
}

///|
fn proc_exit_ffi(rval : UInt) = "wasi_snapshot_preview1" "proc_exit"

///|
/// Terminate the process normally. An exit code of 0 indicates successful
/// termination of the program. The meanings of other values is dependent on
/// the environment.
/// 
/// Note: the accepted range may depend on the runtime, meaning that it may
/// become an error to return a value outside the accepted range of exit codes,
/// making the actual error code environment-dependent.
pub fn[T] proc_exit(rval : UInt) -> T {
  proc_exit_ffi(rval)
  panic()
}

///|
fn proc_raise_ffi(sig : UInt) -> UInt = "wasi_snapshot_preview1" "proc_raise"

///|
/// Send a signal to the process of the calling thread.
/// 
/// Note: This is similar to `raise` in POSIX.
/// 
/// @param `sig` The signal condition to trigger.
pub fn proc_raise(sig : Signal) -> Unit raise Errno {
  let ret = proc_raise_ffi(sig.value())
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn sched_yield_ffi() -> UInt = "wasi_snapshot_preview1" "sched_yield"

///|
/// Temporarily yield execution of the calling thread.
/// 
/// Note: This is similar to `sched_yield` in POSIX.
pub fn sched_yield() -> Unit raise Errno {
  let ret = sched_yield_ffi()
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn random_get_ffi(buf : Int, buf_len : Int) -> UInt = "wasi_snapshot_preview1" "random_get"

///|
/// Write high-quality random data into a buffer.
///
/// This function blocks when the implementaiton is unable to immediately
/// provide sufficient high-quality random data.
///
/// This function may execute slowly, so when larget mounts of random data are
/// required, it's advisable to use this function to seed a pseudo-random
/// number generator, rather than to provide the random data directly.
pub fn random_get(buf : FixedArray[Byte]) -> Unit raise Errno {
  let buf_len = buf.length()
  let buf_ptr = @ffi.byte_array2ptr(buf)
  defer @ffi.free(buf_ptr)
  let ret = random_get_ffi(buf_ptr, buf_len)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}

///|
fn sock_accept_ffi(sock : Int, flags : UInt, fd : Int) -> UInt = "wasi_snapshot_preview1" "sock_accept"

///|
/// Accept a new incoming connection on a socket.
/// 
/// Note: This is similar to `accept` in POSIX.
/// 
/// @param `self` The listening socket.
/// @param `flags` The desired values of the file descriptor flags.
/// @return New socket connection.
pub fn Fd::sock_accept(self : Fd, flags : FdFlags) -> Fd raise Errno {
  let fd_ptr = @ffi.malloc(4)
  defer @ffi.free(fd_ptr)
  let ret = sock_accept_ffi(self.0, flags.0, fd_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let fd = @ffi.load32(fd_ptr)
  Fd(fd)
}

///|
fn sock_recv_ffi(
  sock : Int,
  ri_data : Int,
  ri_data_len : Int,
  ri_flags : UInt,
  size : Int,
  ro_flags : Int,
) -> UInt = "wasi_snapshot_preview1" "sock_recv"

///|
/// Receive a message from a socket.
///
/// Note: This is similar to `recv` in POSIX, though it also supports reading
/// the data into multiple buffers in the manner of `readv`.
///
/// @param `contents` List of scatter/gather vectors to which to store data.
/// @param `flags` Message flags.
/// @return Number of bytes stored in the buffers and message flags.
pub fn Fd::sock_recv(
  self : Fd,
  contents : Array[FixedArray[Byte]],
  flags : RiFlags,
) -> (Size, RoFlags) raise Errno {
  let ri_data_len = contents.length()
  let ri_data_ptr = @ffi.malloc(ri_data_len * 8)
  defer @ffi.free(ri_data_ptr)
  for i, byte in contents {
    let ptr = @ffi.byte_array2ptr(byte)
    @ffi.store32(ri_data_ptr + i * 8, ptr)
    @ffi.store32(ri_data_ptr + i * 8 + 4, byte.length())
  }
  defer (for i in contents {
    let ptr = @ffi.byte_array2ptr(i)
    @ffi.free(ptr)
  })
  let ro_flags_ptr = @ffi.malloc(4)
  defer @ffi.free(ro_flags_ptr)
  let size_ptr = @ffi.malloc(4)
  defer @ffi.free(size_ptr)
  let ret = sock_recv_ffi(
    self.0,
    ri_data_ptr,
    ri_data_len,
    flags.0,
    size_ptr,
    ro_flags_ptr,
  )
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let size = @ffi.load32_u(size_ptr)
  let ro_flags = @ffi.load32_u(ro_flags_ptr)
  (size, ro_flags)
}

///|
fn sock_send_ffi(
  sock : Int,
  si_data : Int,
  si_data_len : Int,
  si_flags : UInt,
  size : Int,
) -> UInt = "wasi_snapshot_preview1" "sock_send"

///|
/// Send a message on a socket.
///
/// Note: This is similar to `send` in POSIX, though it also supports writing
/// the data from multiple buffers in the manner of `writev`.
///
/// @param `contents` List of scatter/gather vectors to which to retrieve the data.
/// @param `flags` Message flags.
/// @return The number of bytes transmitted.
pub fn Fd::sock_send(
  self : Fd,
  contents : Array[BytesView],
  flags : SiFlags,
) -> Size raise Errno {
  let si_data_len = contents.length()
  let si_data_ptr = @ffi.malloc(si_data_len * 8)
  defer @ffi.free(si_data_ptr)
  for i, byte in contents {
    let ptr = @ffi.bytes2ptr(byte.data())
    @ffi.store32(si_data_ptr + i * 8, ptr + byte.start_offset())
    @ffi.store32(si_data_ptr + i * 8 + 4, byte.length())
  }
  defer (for i in contents {
    let ptr = @ffi.bytes2ptr(i.data())
    @ffi.free(ptr)
    @ffi.free(ptr)
  })
  let size_ptr = @ffi.malloc(4)
  defer @ffi.free(size_ptr)
  let ret = sock_send_ffi(self.0, si_data_ptr, si_data_len, flags.0, size_ptr)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
  let size = @ffi.load32_u(size_ptr)
  size
}

///|
fn sock_shutdown_ffi(sock : Int, how : UInt) -> UInt = "wasi_snapshot_preview1" "sock_shutdown"

///|
/// Shut down socket send and receive channels.
/// 
/// Note: This is similar to `shutdown` in POSIX.
/// 
/// @param `how` Which channels on the socket to shut down.
pub fn Fd::sock_shutdown(self : Fd, how : SdFlags) -> Unit raise Errno {
  let ret = sock_shutdown_ffi(self.0, how.0)
  let ret = Errno::from_value(ret).unwrap()
  if ret != Success {
    raise ret
  }
}
