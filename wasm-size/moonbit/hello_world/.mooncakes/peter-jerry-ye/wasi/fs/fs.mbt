///|
/// High-level file kinds mapped from WASI file types.
pub(all) enum FileKind {
  Unknown
  Regular
  Directory
  SymLink
  Socket
  Pipe
  BlockDevice
  CharDevice
} derive(Eq, Show)

///|
/// File open modes used by `open` and `open_sync`.
pub(all) enum Mode {
  ReadOnly
  WriteOnly
  ReadWrite
}

///|
/// Sync behavior for writes when opening or creating files.
pub(all) enum SyncMode {
  NoSync
  Data
  Full
}

///|
fn get_preopened_dirs() -> Array[(String, @wasi.Fd)] raise @wasi.Errno {
  let dirs = []
  for fd = 3; ; fd = fd + 1 {
    let prestats = Some(@wasi.Fd(fd).fd_prestat_get()) catch {
      @wasi.Badf => None
      e => raise e
    }
    if prestats is None {
      break
    }
    let dir_name = @wasi.Fd(fd).fd_prestat_dir_name()
    dirs.push((dir_name, @wasi.Fd(fd)))
  }
  dirs
}

///|
/// Resolve a WASI path against preopened directories.
pub fn resolve_path(path : StringView) -> (@wasi.Fd, String) raise @wasi.Errno {
  let preopened_dirs = get_preopened_dirs()
  let mut best_fd = @wasi.Fd(0)
  let mut best_path : String = ""
  let mut best_len = -1
  for i in 0..<preopened_dirs.length() {
    let dir_name = preopened_dirs[i].0
    let fd = preopened_dirs[i].1
    if path is ['/', .. path] {
      if dir_name == "/" {
        if path is "" {
          best_fd = fd
          best_path = "."
          best_len = 1
        } else {
          best_fd = fd
          best_path = path.to_string()
          best_len = 1
        }
        continue
      }
      if !dir_name.has_prefix("/") {
        continue
      }
    } else {
      if dir_name.has_prefix("/") {
        continue
      }
      if dir_name == "." {
        if best_len < 0 {
          best_fd = fd
          best_path = if path == "" { "." } else { path.to_string() }
          best_len = 0
        }
        continue
      }
    }
    if path == dir_name {
      let len = dir_name.length()
      if len > best_len {
        best_fd = fd
        best_path = "."
        best_len = len
      }
    } else if path.has_prefix(dir_name + "/") {
      let len = dir_name.length()
      if len > best_len {
        best_fd = fd
        best_path = try! path[dir_name.length() + 1:].to_string()
        best_len = len
      }
    }
  }
  if best_len < 0 {
    raise @wasi.Noent
  }
  (best_fd, best_path)
}

///|
/// Open a file using WASI preopen path resolution.
pub fn open(
  path : StringView,
  mode? : Mode = ReadOnly,
  sync? : SyncMode = NoSync,
  append? : Bool = false,
  create? : Bool = false,
  truncate? : Bool = false,
) -> File raise @wasi.Errno {
  let (base_fd, relative_path) = resolve_path(path)
  let rights = match mode {
    ReadOnly =>
      @wasi.Rights::new()
      |> @wasi.Rights::set(@wasi.Right::Fd_read)
      |> @wasi.Rights::set(@wasi.Right::Path_filestat_get)
    WriteOnly =>
      @wasi.Rights::new()
      |> @wasi.Rights::set(@wasi.Right::Fd_write)
      |> @wasi.Rights::set(@wasi.Right::Path_filestat_get)
      |> @wasi.Rights::set(@wasi.Right::Fd_filestat_set_size)
    ReadWrite =>
      @wasi.Rights::new()
      |> @wasi.Rights::set(@wasi.Right::Fd_read)
      |> @wasi.Rights::set(@wasi.Right::Fd_write)
      |> @wasi.Rights::set(@wasi.Right::Path_filestat_get)
      |> @wasi.Rights::set(@wasi.Right::Fd_filestat_set_size)
      |> @wasi.Rights::set(@wasi.Right::Fd_seek)
  }
  let rights_inheriting = @wasi.Rights::new()
    |> @wasi.Rights::set(@wasi.Right::Fd_read)
    |> @wasi.Rights::set(@wasi.Right::Fd_write)
  let mut oflags = @wasi.OpenFlags::new()
  if create {
    oflags = oflags |> @wasi.OpenFlags::set(@wasi.OpenFlag::Create)
  }
  if truncate {
    oflags = oflags |> @wasi.OpenFlags::set(@wasi.OpenFlag::Trunc)
  }
  let mut fdflags = @wasi.FdFlags::new()
  if append {
    fdflags = fdflags |> @wasi.FdFlags::set(@wasi.FdFlag::Append)
  }
  if sync is Data {
    fdflags = fdflags |> @wasi.FdFlags::set(@wasi.FdFlag::Dsync)
  } else if sync is Full {
    fdflags = fdflags |> @wasi.FdFlags::set(@wasi.FdFlag::Sync)
  }
  let fd = base_fd.path_open(
    @wasi.LookupFlags::new(),
    relative_path,
    oflags,
    rights,
    rights_inheriting,
    fdflags,
  )
  File::{ fd, read_buf: @io_buffer.Buffer::new(0) }
}

///|
/// File handle backed by a WASI file descriptor.
struct File {
  fd : @wasi.Fd
  read_buf : @io_buffer.Buffer
}

///|
/// Create or truncate a file with read/write access.
pub fn create(
  path : StringView,
  sync? : SyncMode = NoSync,
) -> File raise @wasi.Errno {
  open(path, mode=ReadWrite, sync~, create=true, truncate=true)
}

///|
/// Close the file, ignoring any close error.
pub fn File::close(self : File) -> Unit {
  self.fd.fd_close() catch {
    _ => ()
  }
}

///|
/// Return the raw WASI file descriptor number.
pub fn File::fd(self : File) -> Int {
  self.fd.0
}

///|
/// Async reader implementation for `File`.
pub impl @async_io.Reader for File with _get_internal_buffer(self) {
  self.read_buf
}

///|
/// Async read implementation for `File`.
pub impl @async_io.Reader for File with _direct_read(
  self,
  buf,
  offset~,
  max_len~,
) {
  let available = @event_loop.suspend_for_read(self.fd)
  let len = @cmp.minimum(available, max_len.to_uint64()).to_int()
  let buffer = FixedArray::make(len, b'\x00')
  let result = self.fd.fd_read([buffer]).0.reinterpret_as_int()
  buffer.blit_to(buf, len=result, dst_offset=offset)
  result
}

///|
/// Async write implementation for `File`.
pub impl @async_io.Writer for File with write_once(self, data, offset~, len~) {
  let available = @event_loop.suspend_for_write(self.fd)
  let write_len = @cmp.minimum(available, len.to_uint64()).to_int()
  let iovs = [data[offset:offset + write_len]]
  let result = self.fd.fd_write(iovs)
  result.0.reinterpret_as_int()
}

///|
/// Sync reader implementation for `File`.
pub impl @sync_io.Reader for File with _get_internal_buffer(self) {
  self.read_buf
}

///|
/// Sync read implementation for `File`.
pub impl @sync_io.Reader for File with _direct_read(
  self,
  buf,
  offset~,
  max_len~,
) {
  let buffer = FixedArray::make(max_len, b'\x00')
  let iovs = [buffer]
  let result = self.fd.fd_read(iovs).0.reinterpret_as_int()
  buffer.blit_to(buf, len=result, dst_offset=offset)
  result
}

///|
/// Sync write implementation for `File`.
pub impl @sync_io.Writer for File with write_once_sync(self, data) {
  let iovs = [data]
  let result = self.fd.fd_write(iovs)
  result.0.reinterpret_as_int()
}

///|
/// Directory handle backed by a WASI directory descriptor.
struct Directory {
  fd : @wasi.Fd
}

///|
/// Open a directory for reading entries.
pub fn opendir(path : StringView) -> Directory raise @wasi.Errno {
  let (base_fd, relative_path) = resolve_path(path)
  let rights = @wasi.Rights::new()
    |> @wasi.Rights::set(@wasi.Right::Fd_read)
    |> @wasi.Rights::set(@wasi.Right::Fd_readdir)
  let oflags = @wasi.OpenFlags::new()
    |> @wasi.OpenFlags::set(@wasi.OpenFlag::Directory)
  let fd = base_fd.path_open(
    @wasi.LookupFlags::new(),
    relative_path,
    oflags,
    rights,
    rights,
    @wasi.FdFlags::new(),
  )
  Directory::{ fd, }
}

///|
/// Close the directory, ignoring any close error.
pub fn Directory::close(self : Directory) -> Unit {
  self.fd.fd_close() catch {
    _ => ()
  }
}

///|
/// Read the entire file asynchronously.
pub async fn read_file(path : StringView) -> &@async_io.Data {
  let file = open(path, mode=ReadOnly)
  defer file.close()
  @async_io.Reader::read_all(file)
}

///|
/// Write data to a file asynchronously.
pub async fn write_file(
  path : StringView,
  data : &@async_io.Data,
  sync? : SyncMode = NoSync,
  append? : Bool = false,
  create? : Bool = false,
  truncate? : Bool = false,
) -> Unit {
  let file = open(path, mode=WriteOnly, sync~, append~, create~, truncate~)
  defer file.close()
  @async_io.Writer::write(file, data)
}

///|
fn convert_timestamp(ts : @wasi.TimeStamp) -> (Int64, Int) {
  let nanos = ts.0
  let secs = nanos / 1000000000UL
  let remainder = nanos % 1000000000UL
  (secs.reinterpret_as_int64(), remainder.to_int())
}

///|
fn filetype_to_filekind(ft : @wasi.FileType) -> FileKind {
  match ft {
    @wasi.FileType::Unknown => FileKind::Unknown
    @wasi.FileType::Block_device => FileKind::BlockDevice
    @wasi.FileType::Character_device => FileKind::CharDevice
    @wasi.FileType::Directory => FileKind::Directory
    @wasi.FileType::Regular_file => FileKind::Regular
    @wasi.FileType::Socket_dgram => FileKind::Socket
    @wasi.FileType::Socket_stream => FileKind::Socket
    @wasi.FileType::Symbolic_link => FileKind::SymLink
  }
}

///|
fn get_filestat(
  path : StringView,
  follow_symlink : Bool,
) -> @wasi.FileStat raise @wasi.Errno {
  let (base_fd, relative_path) = resolve_path(path)
  let lookup_flags = if follow_symlink {
    @wasi.LookupFlags::new()
    |> @wasi.LookupFlags::set(@wasi.LookupFlag::Symlink_follow)
  } else {
    @wasi.LookupFlags::new()
  }
  base_fd.path_filestat_get(lookup_flags, relative_path)
}

///|
/// Get access time for a path.
pub fn atime(
  path : StringView,
  follow_symlink? : Bool = true,
) -> (Int64, Int) raise @wasi.Errno {
  let stat = get_filestat(path, follow_symlink)
  convert_timestamp(stat.atim)
}

///|
/// Get creation/change time for a path.
pub fn ctime(
  path : StringView,
  follow_symlink? : Bool = true,
) -> (Int64, Int) raise @wasi.Errno {
  let stat = get_filestat(path, follow_symlink)
  convert_timestamp(stat.ctim)
}

///|
/// Get modification time for a path.
pub fn mtime(
  path : StringView,
  follow_symlink? : Bool = true,
) -> (Int64, Int) raise @wasi.Errno {
  let stat = get_filestat(path, follow_symlink)
  convert_timestamp(stat.mtim)
}

///|
/// Check if a path exists.
pub fn exists(path : StringView) -> Bool {
  try {
    let _ = get_filestat(path, true)
    true
  } catch {
    _ => false
  }
}

///|
/// Get the file kind for a path.
pub fn kind(
  path : StringView,
  follow_symlink? : Bool = true,
) -> FileKind raise @wasi.Errno {
  let stat = get_filestat(path, follow_symlink)
  filetype_to_filekind(stat.filetype)
}

///|
/// Create a directory.
pub fn mkdir(path : StringView) -> Unit raise @wasi.Errno {
  let (base_fd, relative_path) = resolve_path(path)
  base_fd.path_create_directory(relative_path)
}

///|
/// Remove a file or directory.
pub fn remove(path : StringView) -> Unit raise @wasi.Errno {
  let file_kind = kind(path, follow_symlink=false)
  let (base_fd, relative_path) = resolve_path(path)
  if file_kind is FileKind::Directory {
    base_fd.path_remove_directory(relative_path)
  } else {
    base_fd.path_unlink_file(relative_path)
  }
}

///|
/// Remove a directory, optionally recursively.
pub fn rmdir(
  path : StringView,
  recursive? : Bool = false,
) -> Unit raise @wasi.Errno {
  if recursive {
    let entries = readdir(
      path,
      include_hidden=true,
      include_special=true,
      sort=false,
    )
    for entry in entries {
      let entry_path = path.to_string() + "/" + entry
      remove(entry_path) catch {
        _ => ()
      }
    }
  }
  let (base_fd, relative_path) = resolve_path(path)
  base_fd.path_remove_directory(relative_path)
}

///|
/// Read directory entries.
pub fn readdir(
  path : StringView,
  include_hidden? : Bool = true,
  include_special? : Bool = false,
  sort? : Bool = false,
) -> Array[String] raise @wasi.Errno {
  let dir = opendir(path)
  let result : Array[String] = []
  let entries_tuple = dir.fd.fd_readdir()
  for entry in entries_tuple.0 {
    let name = entry.0
    if name == "." || name == ".." {
      continue
    }
    if !include_hidden && name.has_prefix(".") {
      continue
    }
    if !include_special {
      let entry_path = path.to_string() + "/" + name
      let entry_kind = kind(entry_path, follow_symlink=false) catch {
        _ => FileKind::Unknown
      }
      if entry_kind is FileKind::Socket {
        continue
      }
      if entry_kind is FileKind::Pipe {
        continue
      }
    }
    result.push(name)
  }
  dir.close()
  if sort {
    result.sort()
  }
  result
}

///|
/// Create a symbolic link.
pub fn symlink(
  target~ : StringView,
  path : StringView,
) -> Unit raise @wasi.Errno {
  let (base_fd, relative_path) = resolve_path(path)
  @wasi.path_symlink(target.to_string(), base_fd, relative_path)
}

///|
async fn walk_inner(
  path : StringView,
  visitor : async (StringView, Array[String]) -> Unit,
  allow_failure : Bool,
) -> Unit {
  let entries = readdir(
    path,
    include_hidden=true,
    include_special=true,
    sort=false,
  ) catch {
    e => if allow_failure { return } else { fail("walk failed: \{e}") }
  }
  visitor(path, entries)
  for entry in entries {
    let entry_path = path + "/" + entry
    let entry_kind = kind(entry_path, follow_symlink=false) catch {
      e =>
        if allow_failure {
          FileKind::Unknown
        } else {
          fail("walk failed: \{e}")
        }
    }
    if entry_kind is FileKind::Directory {
      walk_inner(entry_path, visitor, allow_failure)
    }
  }
}

///|
/// Walk a directory tree and invoke `visitor` for each directory.
pub async fn walk(
  path : StringView,
  visitor : async (StringView, Array[String]) -> Unit,
  allow_failure? : Bool,
) -> Unit {
  let allow_failure = match allow_failure {
    Some(value) => value
    None => false
  }
  walk_inner(path, visitor, allow_failure)
}

///|
/// Read the target of a symbolic link.
pub fn readlink(path : StringView) -> String raise @wasi.Errno {
  let (base_fd, relative_path) = resolve_path(path)
  let size = base_fd.path_readlink(relative_path)
  size.to_string()
}

///|
/// Best-effort check for executable access by opening the path.
pub fn can_execute(path : StringView) -> Bool {
  try {
    let file = open(path, mode=ReadOnly)
    file.close()
    true
  } catch {
    _ => false
  }
}

///|
/// Check if a path can be opened for reading.
pub fn can_read(path : StringView) -> Bool {
  try {
    let file = open(path, mode=ReadOnly)
    file.close()
    true
  } catch {
    _ => false
  }
}

///|
/// Check if a path can be opened for writing.
pub fn can_write(path : StringView) -> Bool {
  try {
    let file = open(path, mode=WriteOnly)
    file.close()
    true
  } catch {
    _ => false
  }
}

///|
/// Read the entire file synchronously.
pub fn read_file_sync(path : StringView) -> &@sync_io.Data raise {
  let file = open(path, mode=ReadOnly)
  defer file.close()
  @sync_io.Reader::read_all_sync(file)
}

///|
/// Write data to a file synchronously.
pub fn write_file_sync(
  path : StringView,
  data : &@sync_io.Data,
  sync? : SyncMode = NoSync,
  append? : Bool = false,
  create? : Bool = false,
  truncate? : Bool = false,
) -> Unit raise {
  let file = open(path, mode=WriteOnly, sync~, append~, create~, truncate~)
  defer file.close()
  @sync_io.Writer::write_sync(file, data)
}

///|
/// Get access time for an open file.
pub fn File::atime(self : File) -> (Int64, Int) raise @wasi.Errno {
  let stat = self.fd.fd_filestat_get()
  convert_timestamp(stat.atim)
}

///|
/// Get creation/change time for an open file.
pub fn File::ctime(self : File) -> (Int64, Int) raise @wasi.Errno {
  let stat = self.fd.fd_filestat_get()
  convert_timestamp(stat.ctim)
}

///|
/// Get modification time for an open file.
pub fn File::mtime(self : File) -> (Int64, Int) raise @wasi.Errno {
  let stat = self.fd.fd_filestat_get()
  convert_timestamp(stat.mtim)
}

///|
/// Read from a file at a byte offset into a buffer.
pub async fn File::read_at(
  self : File,
  buf : FixedArray[Byte],
  position~ : Int64,
  offset? : Int,
  len? : Int,
) -> Int {
  let start = match offset {
    Some(value) => value
    None => 0
  }
  let max_len = match len {
    Some(value) => value
    None => buf.length() - start
  }
  if max_len <= 0 {
    return 0
  }
  try {
    let available = @event_loop.suspend_for_read(self.fd)
    let read_len = @cmp.minimum(available, max_len.to_uint64()).to_int()
    if read_len <= 0 {
      return 0
    }
    let buffer = FixedArray::make(read_len, b'\x00')
    let size = self.fd.fd_pread(
      [buffer],
      @wasi.FileSize(position.reinterpret_as_uint64()),
    )
    let actual = size.0.reinterpret_as_int()
    buffer.blit_to(buf, len=actual, dst_offset=start)
    actual
  } catch {
    e => fail("read_at failed: \{e}")
  }
}

///|
/// Read exactly `len` bytes starting at a byte offset.
pub async fn File::read_exactly_at(
  self : File,
  len : Int,
  position~ : Int64,
) -> Bytes {
  if len <= 0 {
    return ""
  }
  let buffer = FixedArray::make(len, b'\x00')
  let mut read_total = 0
  while read_total < len {
    let read_len = self.read_at(
      buffer,
      position=position + read_total.to_int64(),
      offset=read_total,
      len=len - read_total,
    )
    if read_len <= 0 {
      fail("read_exactly_at failed: unexpected EOF")
    }
    read_total = read_total + read_len
  }
  buffer.unsafe_reinterpret_as_bytes()
}

///|
/// Get file size in bytes.
pub fn File::size(self : File) -> Int64 raise @wasi.Errno {
  let stat = self.fd.fd_filestat_get()
  stat.size.0.reinterpret_as_int64()
}

///|
/// Get the file kind from filestat.
pub fn File::kind(self : File) -> FileKind {
  try {
    let stat = self.fd.fd_filestat_get()
    filetype_to_filekind(stat.filetype)
  } catch {
    _ => FileKind::Unknown
  }
}

///|
/// Flush file contents to storage.
pub fn File::sync(
  self : File,
  only_data? : Bool = false,
) -> Unit raise @wasi.Errno {
  if only_data {
    self.fd.fd_datasync()
  } else {
    self.fd.fd_sync()
  }
}

///|
/// Write data at a byte offset, restoring the original position.
pub async fn File::write_at(
  self : File,
  data : BytesView,
  position~ : Int64,
) -> Unit {
  let len = data.length()
  if len <= 0 {
    return
  }
  let current = self.fd.fd_tell() catch { e => fail("write_at failed: \{e}") }
  try {
    let _ = self.fd.fd_seek(@wasi.FileDelta(position), @wasi.Whence::Set)

  } catch {
    e => fail("write_at failed: \{e}")
  }
  let restore = @wasi.FileDelta(current.0.reinterpret_as_int64())
  let mut err : Error? = None
  try {
    let mut written = 0
    while written < len {
      let available = @event_loop.suspend_for_write(self.fd)
      let write_len = @cmp.minimum(available, (len - written).to_uint64()).to_int()
      if write_len <= 0 {
        break
      }
      let chunk = data[written:written + write_len]
      try {
        let size = self.fd.fd_write([chunk])
        let actual = size.0.reinterpret_as_int()
        if actual <= 0 {
          break
        }
        written = written + actual
      } catch {
        e => {
          err = Some(e)
          break
        }
      }
    }
  } catch {
    e => err = Some(e)
  }
  try {
    let _ = self.fd.fd_seek(restore, @wasi.Whence::Set)

  } catch {
    _ => ()
  }
  if err is Some(e) {
    fail("write_at failed: \{e}")
  }
}

///|
/// Read all entries from an open directory.
pub fn Directory::read_all(
  self : Directory,
  include_hidden? : Bool = true,
  include_special? : Bool = false,
) -> Array[String] raise @wasi.Errno {
  let result : Array[String] = []
  let entries_tuple = self.fd.fd_readdir()
  for entry in entries_tuple.0 {
    let name = entry.0
    let dir_ent = entry.1
    if name == "." || name == ".." {
      continue
    }
    if !include_hidden && name.has_prefix(".") {
      continue
    }
    if !include_special {
      if dir_ent.d_type is @wasi.FileType::Socket_dgram {
        continue
      }
      if dir_ent.d_type is @wasi.FileType::Socket_stream {
        continue
      }
    }
    result.push(name)
  }
  result
}

///|
/// Treat the file handle as a directory.
pub fn File::as_dir(self : File) -> Directory raise @wasi.Errno {
  let stat = self.fd.fd_filestat_get()
  if not(stat.filetype is @wasi.FileType::Directory) {
    raise @wasi.Notdir
  }
  Directory::{ fd: self.fd }
}
