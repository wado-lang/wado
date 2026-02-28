# WASI Standard Library

## wasi:cli

### Enums

```wado
pub enum ErrorCode { Io, IllegalByteSequence, Pipe }
```
## wasi:filesystem

### Structs

```wado
pub struct DescriptorStat {
    type: DescriptorType,
    link_count: LinkCount,
    size: Filesize,
    data_access_timestamp: Option<Instant>,
    data_modification_timestamp: Option<Instant>,
    status_change_timestamp: Option<Instant>,
}
```

```wado
pub struct DirectoryEntry {
    type: DescriptorType,
    name: String,
}
```

```wado
pub struct MetadataHashValue {
    lower: u64,
    upper: u64,
}
```

### Types

```wado
pub type Filesize = u64;
pub type LinkCount = u64;
```

### Enums

```wado
pub enum DescriptorType { Unknown, BlockDevice, CharacterDevice, Directory, Fifo, SymbolicLink, RegularFile, Socket }
```

```wado
pub enum ErrorCode { Access, Already, BadDescriptor, Busy, Deadlock, Quota, Exist, FileTooLarge, IllegalByteSequence, InProgress, Interrupted, Invalid, Io, IsDirectory, Loop, TooManyLinks, MessageSize, NameTooLong, NoDevice, NoEntry, NoLock, InsufficientMemory, InsufficientSpace, NotDirectory, NotEmpty, NotRecoverable, Unsupported, NoTty, NoSuchDevice, Overflow, NotPermitted, Pipe, ReadOnly, InvalidSeek, TextFileBusy, CrossDevice }
```

```wado
pub enum Advice { Normal, Sequential, Random, WillNeed, DontNeed, NoReuse }
```

### Variants

```wado
pub variant NewTimestamp {
    NoChange,
    Now,
    Timestamp(Instant),
}
```

### Flags

```wado
pub flags DescriptorFlags {
    Read,
    Write,
    FileIntegritySync,
    DataIntegritySync,
    RequestedWriteSync,
    MutateDirectory,
}
```

```wado
pub flags PathFlags {
    SymlinkFollow,
}
```

```wado
pub flags OpenFlags {
    Create,
    Directory,
    Exclusive,
    Truncate,
}
```
## wasi:http

### Structs

```wado
pub struct DnsErrorPayload {
    rcode: Option<String>,
    info_code: Option<u16>,
}
```

```wado
pub struct TlsAlertReceivedPayload {
    alert_id: Option<u8>,
    alert_message: Option<String>,
}
```

```wado
pub struct FieldSizePayload {
    field_name: Option<String>,
    field_size: Option<u32>,
}
```

### Types

```wado
pub type FieldName = String;
pub type FieldValue = Array<u8>;
pub type Headers = Fields;
pub type Trailers = Fields;
pub type StatusCode = u16;
```

### Variants

```wado
pub variant Method {
    Get,
    Head,
    Post,
    Put,
    Delete,
    Connect,
    Options,
    Trace,
    Patch,
    Other(String),
}
```

```wado
pub variant Scheme {
    Http,
    Https,
    Other(String),
}
```

```wado
pub variant ErrorCode {
    DnsTimeout,
    DnsError(DnsErrorPayload),
    DestinationNotFound,
    DestinationUnavailable,
    DestinationIpProhibited,
    DestinationIpUnroutable,
    ConnectionRefused,
    ConnectionTerminated,
    ConnectionTimeout,
    ConnectionReadTimeout,
    ConnectionWriteTimeout,
    ConnectionLimitReached,
    TlsProtocolError,
    TlsCertificateError,
    TlsAlertReceived(TlsAlertReceivedPayload),
    HttpRequestDenied,
    HttpRequestLengthRequired,
    HttpRequestBodySize(Option<u64>),
    HttpRequestMethodInvalid,
    HttpRequestUriInvalid,
    HttpRequestUriTooLong,
    HttpRequestHeaderSectionSize(Option<u32>),
    HttpRequestHeaderSize(Option<FieldSizePayload>),
    HttpRequestTrailerSectionSize(Option<u32>),
    HttpRequestTrailerSize(FieldSizePayload),
    HttpResponseIncomplete,
    HttpResponseHeaderSectionSize(Option<u32>),
    HttpResponseHeaderSize(FieldSizePayload),
    HttpResponseBodySize(Option<u64>),
    HttpResponseTrailerSectionSize(Option<u32>),
    HttpResponseTrailerSize(FieldSizePayload),
    HttpResponseTransferCoding(Option<String>),
    HttpResponseContentCoding(Option<String>),
    HttpResponseTimeout,
    HttpUpgradeFailed,
    HttpProtocolError,
    LoopDetected,
    ConfigurationError,
    InternalError(Option<String>),
}
```

```wado
pub variant HeaderError {
    InvalidSyntax,
    Forbidden,
    Immutable,
}
```

```wado
pub variant RequestOptionsError {
    NotSupported,
    Immutable,
}
```
## wasi:clocks

### Structs

```wado
pub struct Instant {
    seconds: i64,
    nanoseconds: u32,
}
```

### Types

```wado
pub type Duration = u64;
pub type Mark = u64;
```
## wasi:random
## wasi:sockets

### Structs

```wado
pub struct Ipv4SocketAddress {
    port: u16,
    address: Ipv4Address,
}
```

```wado
pub struct Ipv6SocketAddress {
    port: u16,
    flow_info: u32,
    address: Ipv6Address,
    scope_id: u32,
}
```

### Types

```wado
pub type Ipv4Address = [u8, u8, u8, u8];
pub type Ipv6Address = [u16, u16, u16, u16, u16, u16, u16, u16];
```

### Enums

```wado
pub enum ErrorCode { Unknown, AccessDenied, NotSupported, InvalidArgument, OutOfMemory, Timeout, InvalidState, AddressNotBindable, AddressInUse, RemoteUnreachable, ConnectionRefused, ConnectionReset, ConnectionAborted, DatagramTooLarge }
```

```wado
pub enum IpAddressFamily { Ipv4, Ipv6 }
```

```wado
pub enum ErrorCode { Unknown, AccessDenied, InvalidArgument, NameUnresolvable, TemporaryResolverFailure, PermanentResolverFailure }
```

### Variants

```wado
pub variant IpAddress {
    Ipv4(Ipv4Address),
    Ipv6(Ipv6Address),
}
```

```wado
pub variant IpSocketAddress {
    Ipv4(Ipv4SocketAddress),
    Ipv6(Ipv6SocketAddress),
}
```
