//! A PJRT plugin: its client, and the buffers and executables a guest holds.

use std::ffi::{c_char, c_void};
use std::path::Path;
use std::ptr::null_mut;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use libloading::{Library, Symbol};

use crate::ffi::{self, Api, Arguments, Opaque, Slot};

/// What `PJRT_Client_Compile` takes as options, a serialized
/// `CompileOptionsProto` of one replica and one partition: an empty one is not
/// a valid default, as the CPU plugin aborts on its replica count of 0.
const ONE_REPLICA_ONE_PARTITION: [u8; 6] = [0x1a, 0x04, 0x20, 0x01, 0x28, 0x01];

/// The plugin's library, which the API table points into.
struct Plugin {
    api: *const Api,
    _library: Library,
}

// SAFETY: PJRT's calls may come from any thread.
unsafe impl Send for Plugin {}
// SAFETY: as above, and the table is never written after `GetPjrtApi`.
unsafe impl Sync for Plugin {}

impl Plugin {
    fn api(&self) -> &Api {
        // SAFETY: `GetPjrtApi` returned it, and `_library` keeps it loaded.
        unsafe { &*self.api }
    }

    /// `slot` on `arguments`, and what it reports.
    ///
    /// # Safety
    ///
    /// As [`Api::call`].
    unsafe fn call<A: Arguments>(&self, slot: Slot, arguments: &mut A) -> Result<(), String> {
        // SAFETY: the caller's.
        let error = unsafe { self.api().call(slot, arguments) };
        if error.is_null() {
            return Ok(());
        }
        let mut message = ffi::ErrorMessage::new();
        message.error = error;
        let mut destroy = ffi::ErrorDestroy::new();
        destroy.error = error;
        // SAFETY: `error` is the plugin's, and is destroyed once, after the
        // message is copied out of it.
        unsafe {
            self.api().call(Slot::ErrorMessage, &mut message);
            let text = text_at(message.message, message.message_size);
            self.api().call(Slot::ErrorDestroy, &mut destroy);
            Err(text)
        }
    }

    /// Waits for `event` and destroys it.
    fn awaited(&self, event: Opaque) -> Result<(), String> {
        let mut wait = ffi::EventAwait::new();
        wait.event = event;
        // SAFETY: `event` is the plugin's, and is destroyed once.
        unsafe {
            let waited = self.call(Slot::EventAwait, &mut wait);
            let mut destroy = ffi::EventDestroy::new();
            destroy.event = event;
            self.call(Slot::EventDestroy, &mut destroy)?;
            waited
        }
    }
}

/// # Safety
///
/// `text` points to `size` bytes.
unsafe fn text_at(text: *const c_char, size: usize) -> String {
    // SAFETY: the caller's.
    let bytes = unsafe { std::slice::from_raw_parts(text.cast::<u8>(), size) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// A plugin's connection to its devices.
pub struct Client {
    plugin: Arc<Plugin>,
    raw: Opaque,
    device: Opaque,
}

// SAFETY: as `Plugin`.
unsafe impl Send for Client {}

impl Client {
    /// Loads the plugin at `path` and opens its first device.
    pub fn open(path: &Path) -> Result<Self> {
        // SAFETY: a plugin's initializers run here, which the user named.
        let library = unsafe { Library::new(path) }
            .with_context(|| format!("cannot load the PJRT plugin {}", path.display()))?;
        // SAFETY: `GetPjrtApi` is what makes a library a PJRT plugin, and takes
        // nothing.
        let api = unsafe {
            let get: Symbol<unsafe extern "C" fn() -> *const Api> = library
                .get(b"GetPjrtApi\0")
                .with_context(|| format!("{} is not a PJRT plugin", path.display()))?;
            get()
        };
        let plugin = Arc::new(Plugin {
            api,
            _library: library,
        });
        let major = plugin.api().major_version();
        if major != 0 {
            bail!("a PJRT plugin of API major version {major} is not one this host speaks");
        }

        let mut initialize = ffi::PluginInitialize::new();
        let mut create = ffi::ClientCreate::new();
        // SAFETY: each struct is the one its slot takes.
        unsafe {
            plugin
                .call(Slot::PluginInitialize, &mut initialize)
                .map_err(anyhow::Error::msg)?;
            plugin
                .call(Slot::ClientCreate, &mut create)
                .map_err(anyhow::Error::msg)?;
        }
        let raw = create.client;
        let mut devices = ffi::ClientAddressableDevices::new();
        devices.client = raw;
        // SAFETY: as above, and the list lives as long as the client.
        let device = unsafe {
            plugin
                .call(Slot::ClientAddressableDevices, &mut devices)
                .map_err(anyhow::Error::msg)?;
            if devices.num_addressable_devices == 0 {
                bail!("the PJRT plugin {} has no device here", path.display());
            }
            *devices.addressable_devices
        };
        Ok(Self {
            plugin,
            raw,
            device,
        })
    }

    /// The plugin's name for its platform.
    pub fn platform(&self) -> Result<String, String> {
        let mut name = ffi::ClientPlatformName::new();
        name.client = self.raw;
        // SAFETY: the struct is the one the slot takes, and the name lives as long
        // as the client.
        unsafe {
            self.plugin.call(Slot::ClientPlatformName, &mut name)?;
            Ok(text_at(name.platform_name, name.platform_name_size))
        }
    }

    /// Compiles a module of MLIR text.
    pub fn compile(&self, module: &str) -> Result<Executable, String> {
        const FORMAT: &str = "mlir";
        let mut program = ffi::Program::new();
        program.code = module.as_ptr().cast();
        program.code_size = module.len();
        program.format = FORMAT.as_ptr().cast();
        program.format_size = FORMAT.len();
        let mut compile = ffi::ClientCompile::new();
        compile.client = self.raw;
        compile.program = &raw const program;
        compile.compile_options = ONE_REPLICA_ONE_PARTITION.as_ptr().cast();
        compile.compile_options_size = ONE_REPLICA_ONE_PARTITION.len();
        // SAFETY: the struct is the one the slot takes, and what it points to
        // outlives the call.
        unsafe {
            self.plugin.call(Slot::ClientCompile, &mut compile)?;
        }
        let loaded = compile.executable;

        let mut held = ffi::LoadedExecutableGetExecutable::new();
        held.loaded_executable = loaded;
        // SAFETY: as above.
        unsafe {
            self.plugin
                .call(Slot::LoadedExecutableGetExecutable, &mut held)?;
        }
        let mut outputs = ffi::ExecutableNumOutputs::new();
        outputs.executable = held.executable;
        // SAFETY: as above.
        unsafe {
            self.plugin.call(Slot::ExecutableNumOutputs, &mut outputs)?;
        }
        Ok(Executable {
            plugin: Arc::clone(&self.plugin),
            loaded,
            held: held.executable,
            outputs: outputs.num_outputs,
        })
    }

    /// A buffer of `elements`, little-endian, in row-major order.
    pub fn upload(
        &self,
        element_type: i32,
        extents: &[i64],
        elements: &[u8],
    ) -> Result<Buffer, String> {
        let mut upload = ffi::ClientBufferFromHostBuffer::new();
        upload.client = self.raw;
        upload.data = elements.as_ptr().cast::<c_void>();
        upload.element_type = element_type;
        upload.dims = extents.as_ptr();
        upload.num_dims = extents.len();
        upload.host_buffer_semantics = ffi::COPIED_DURING_CALL;
        upload.device = self.device;
        // SAFETY: the struct is the one the slot takes, and the plugin copies what
        // it points to before the event below fires.
        unsafe {
            self.plugin
                .call(Slot::ClientBufferFromHostBuffer, &mut upload)?;
        }
        self.plugin.awaited(upload.done_with_host_buffer)?;
        Ok(Buffer {
            plugin: Arc::clone(&self.plugin),
            raw: upload.buffer,
        })
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let mut destroy = ffi::ClientDestroy::new();
        destroy.client = self.raw;
        // SAFETY: the client is destroyed once.
        let destroyed = unsafe { self.plugin.call(Slot::ClientDestroy, &mut destroy) };
        destroyed.expect("a PJRT client closes");
    }
}

/// A tensor on the device.
pub struct Buffer {
    plugin: Arc<Plugin>,
    raw: Opaque,
}

// SAFETY: as `Plugin`.
unsafe impl Send for Buffer {}

impl Buffer {
    /// The elements, little-endian, in row-major order, once the device has
    /// computed them.
    pub fn to_host(&self) -> Result<Vec<u8>, String> {
        let mut size = ffi::BufferToHostBuffer::new();
        size.src = self.raw;
        // SAFETY: the struct is the one the slot takes, and a null `dst` asks only
        // for the size.
        unsafe {
            self.plugin.call(Slot::BufferToHostBuffer, &mut size)?;
        }
        let mut elements = vec![0; size.dst_size];
        let mut read = ffi::BufferToHostBuffer::new();
        read.src = self.raw;
        read.dst = elements.as_mut_ptr().cast();
        read.dst_size = elements.len();
        // SAFETY: as above, and `elements` outlives the wait below.
        unsafe {
            self.plugin.call(Slot::BufferToHostBuffer, &mut read)?;
        }
        self.plugin.awaited(read.event)?;
        Ok(elements)
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        let mut destroy = ffi::BufferDestroy::new();
        destroy.buffer = self.raw;
        // SAFETY: the buffer is destroyed once.
        let destroyed = unsafe { self.plugin.call(Slot::BufferDestroy, &mut destroy) };
        destroyed.expect("a PJRT buffer drops");
    }
}

/// A program compiled for the device.
pub struct Executable {
    plugin: Arc<Plugin>,
    loaded: Opaque,
    held: Opaque,
    outputs: usize,
}

// SAFETY: as `Plugin`.
unsafe impl Send for Executable {}

impl Executable {
    /// Runs the program on `arguments`. The results are ready to read, and
    /// reading waits for the device.
    pub fn execute(&self, arguments: &[&Buffer]) -> Result<Vec<Buffer>, String> {
        let raw: Vec<Opaque> = arguments.iter().map(|b| b.raw).collect();
        let lists: [*const Opaque; 1] = [raw.as_ptr()];
        let mut results: Vec<Opaque> = vec![null_mut(); self.outputs];
        let result_lists: [*mut Opaque; 1] = [results.as_mut_ptr()];
        let mut options = ffi::ExecuteOptions::new();
        let mut execute = ffi::LoadedExecutableExecute::new();
        execute.executable = self.loaded;
        execute.options = &raw mut options;
        execute.argument_lists = lists.as_ptr();
        execute.num_devices = 1;
        execute.num_args = raw.len();
        execute.output_lists = result_lists.as_ptr();
        // SAFETY: the struct is the one the slot takes, and what it points to
        // outlives the call.
        unsafe {
            self.plugin
                .call(Slot::LoadedExecutableExecute, &mut execute)?;
        }
        Ok(results
            .into_iter()
            .map(|raw| Buffer {
                plugin: Arc::clone(&self.plugin),
                raw,
            })
            .collect())
    }
}

impl Drop for Executable {
    fn drop(&mut self) {
        let mut loaded = ffi::LoadedExecutableDestroy::new();
        loaded.executable = self.loaded;
        let mut held = ffi::ExecutableDestroy::new();
        held.executable = self.held;
        // SAFETY: each is destroyed once.
        let destroyed = unsafe {
            self.plugin
                .call(Slot::ExecutableDestroy, &mut held)
                .and_then(|()| self.plugin.call(Slot::LoadedExecutableDestroy, &mut loaded))
        };
        destroyed.expect("a PJRT executable drops");
    }
}
