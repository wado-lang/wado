//! A PJRT plugin: its client, and the buffers and executables a guest holds.

use std::ffi::{c_char, c_void};
use std::path::Path;
use std::ptr::null_mut;
use std::sync::{Arc, mpsc};

use anyhow::{Context as _, Result, bail};
use libloading::{Library, Symbol};

use crate::ffi::{self, Api, Arguments, Opaque, Slot};
use crate::host::{Analysis, Cost};

/// What a compile takes as options, a serialized `CompileOptionsProto` of one
/// replica and one partition: an empty one is not a valid default, as the CPU
/// plugin aborts on its replica count of 0.
const ONE_REPLICA_ONE_PARTITION: [u8; 6] = [0x1a, 0x04, 0x20, 0x01, 0x28, 0x01];

/// A module of MLIR text, as a compile takes it. The text must outlive it.
fn mlir_program(module: &str) -> ffi::Program {
    const FORMAT: &str = "mlir";
    let mut program = ffi::Program::new();
    program.code = module.as_ptr().cast();
    program.code_size = module.len();
    program.format = FORMAT.as_ptr().cast();
    program.format_size = FORMAT.len();
    program
}

/// The stack of the thread a plugin runs on. It reaches what a large program's
/// compile recurses to, and stays unmapped until then.
const STACK_BYTES: usize = 64 << 20;

/// What crosses to the plugin's thread and back. Its sender waits for the
/// answer, so nothing here is used on two threads at once.
struct Crossing<T>(T);

// SAFETY: as the struct says.
unsafe impl<T> Send for Crossing<T> {}

/// One call of the plugin.
struct Request {
    entry: ffi::Call,
    arguments: *mut c_void,
    reported: mpsc::SyncSender<Crossing<Opaque>>,
}

/// Starts the thread that makes every call of a plugin. A guest runs on a
/// wasmtime fiber, whose stack a plugin cannot walk: `libtpu.so` samples a hash
/// table now and then by following frame pointers from wherever it is, and
/// reading past a fiber's frames faults.
fn plugin_thread() -> Result<mpsc::Sender<Crossing<Request>>> {
    let (calls, requests) = mpsc::channel::<Crossing<Request>>();
    std::thread::Builder::new()
        .name("pjrt-plugin".to_owned())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            for Crossing(request) in requests {
                // SAFETY: `Plugin::enter`'s caller vouches for the pair, and
                // waits while the arguments are in use.
                let error = unsafe { (request.entry)(request.arguments) };
                request
                    .reported
                    .send(Crossing(error))
                    .expect("the caller waits for the answer");
            }
        })
        .context("cannot start the plugin's thread")?;
    Ok(calls)
}

/// The plugin's library, which the API table points into.
pub struct Plugin {
    api: *const Api,
    calls: mpsc::Sender<Crossing<Request>>,
    _library: Library,
}

// SAFETY: PJRT's calls may come from any thread.
unsafe impl Send for Plugin {}
// SAFETY: as above, and the table is never written after `GetPjrtApi`.
unsafe impl Sync for Plugin {}

impl Plugin {
    /// Loads and initializes the plugin at `path`.
    pub fn open(path: &Path) -> Result<Arc<Self>> {
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
        let plugin = Arc::new(Self {
            api,
            calls: plugin_thread()?,
            _library: library,
        });
        let major = plugin.api().major_version();
        if major != 0 {
            bail!("a PJRT plugin of API major version {major} is not one this host speaks");
        }
        let mut initialize = ffi::PluginInitialize::new();
        // SAFETY: the struct is the one the slot takes.
        unsafe { plugin.call(Slot::PluginInitialize, &mut initialize) }
            .map_err(anyhow::Error::msg)?;
        Ok(plugin)
    }

    /// Compiles `module` for `topology`, which needs no device, and reports
    /// what the plugin says of it.
    pub fn analyzed(&self, module: &str, topology: &str) -> Result<Analysis, String> {
        let mut create = ffi::TopologyDescriptionCreate::new();
        create.topology_name = topology.as_ptr().cast();
        create.topology_name_size = topology.len();
        // SAFETY: the struct is the one the slot takes, and the name outlives the
        // call.
        unsafe {
            self.call(Slot::TopologyDescriptionCreate, &mut create)?;
        }
        let program = mlir_program(module);
        let mut compile = ffi::Compile::new();
        compile.topology = create.topology;
        compile.program = &raw const program;
        compile.compile_options = ONE_REPLICA_ONE_PARTITION.as_ptr().cast();
        compile.compile_options_size = ONE_REPLICA_ONE_PARTITION.len();
        // SAFETY: as above, and the module outlives the call.
        let compiled = unsafe { self.call(Slot::Compile, &mut compile) };
        let analysis = compiled.and_then(|()| {
            let analysis = self.analysis(compile.executable);
            let mut destroy = ffi::ExecutableDestroy::new();
            destroy.executable = compile.executable;
            // SAFETY: the executable is destroyed once.
            unsafe { self.call(Slot::ExecutableDestroy, &mut destroy) }?;
            analysis
        });
        let mut destroy = ffi::TopologyDescriptionDestroy::new();
        destroy.topology = create.topology;
        // SAFETY: the topology is destroyed once.
        unsafe { self.call(Slot::TopologyDescriptionDestroy, &mut destroy) }?;
        analysis
    }

    /// What the plugin reports of `executable`.
    fn analysis(&self, executable: Opaque) -> Result<Analysis, String> {
        let mut memory = ffi::ExecutableGetCompiledMemoryStats::new();
        memory.executable = executable;
        let mut cost = ffi::ExecutableGetCostAnalysis::new();
        cost.executable = executable;
        // SAFETY: each struct is the one its slot takes, and the properties live
        // as long as the executable.
        let properties = unsafe {
            self.call(Slot::ExecutableGetCompiledMemoryStats, &mut memory)?;
            self.call(Slot::ExecutableGetCostAnalysis, &mut cost)?;
            if cost.num_properties == 0 {
                &[]
            } else {
                std::slice::from_raw_parts(cost.properties, cost.num_properties)
            }
        };
        let costs = properties
            .iter()
            .filter_map(|property| {
                let value = property.number()?;
                // SAFETY: the plugin's name, of the size it says.
                let name = unsafe { text_at(property.name, property.name_size) };
                Some(Cost { name, value })
            })
            .collect();
        Ok(Analysis {
            argument_bytes: memory.argument_size_in_bytes,
            output_bytes: memory.output_size_in_bytes,
            alias_bytes: memory.alias_size_in_bytes,
            temporary_bytes: memory.temp_size_in_bytes,
            costs,
        })
    }

    fn api(&self) -> &Api {
        // SAFETY: `GetPjrtApi` returned it, and `_library` keeps it loaded.
        unsafe { &*self.api }
    }

    /// `slot` on `arguments`, and what it reports.
    ///
    /// # Safety
    ///
    /// As [`Self::enter`].
    unsafe fn call<A: Arguments>(&self, slot: Slot, arguments: &mut A) -> Result<(), String> {
        // SAFETY: the caller's.
        let error = unsafe { self.enter(slot, arguments) };
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
            self.enter(Slot::ErrorMessage, &mut message);
            let text = text_at(message.message, message.message_size);
            self.enter(Slot::ErrorDestroy, &mut destroy);
            Err(text)
        }
    }

    /// `slot` on `arguments`, run on the plugin's thread, and the error it
    /// reports.
    ///
    /// # Safety
    ///
    /// `A` is the struct `slot` takes, and holds handles of this plugin.
    unsafe fn enter<A: Arguments>(&self, slot: Slot, arguments: &mut A) -> Opaque {
        let (reported, report) = mpsc::sync_channel(1);
        self.calls
            .send(Crossing(Request {
                entry: self.api().entry(slot),
                arguments: std::ptr::from_mut(arguments).cast(),
                reported,
            }))
            .expect("the plugin's thread lives as long as the plugin");
        report.recv().expect("the plugin's thread answers").0
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
    /// Opens the first device of `plugin`.
    pub fn open(plugin: Arc<Plugin>) -> Result<Self, String> {
        let mut create = ffi::ClientCreate::new();
        // SAFETY: the struct is the one the slot takes.
        unsafe {
            plugin.call(Slot::ClientCreate, &mut create)?;
        }
        let raw = create.client;
        let mut devices = ffi::ClientAddressableDevices::new();
        devices.client = raw;
        // SAFETY: as above, and the list lives as long as the client.
        let device = unsafe {
            plugin.call(Slot::ClientAddressableDevices, &mut devices)?;
            if devices.num_addressable_devices == 0 {
                return Err("the PJRT plugin has no device here".to_owned());
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
        let program = mlir_program(module);
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
    /// Runs the program on `arguments`, donating each it updates in place but
    /// those `kept` holds the index of. The results are ready to read, and
    /// reading waits for the device.
    pub fn execute(&self, arguments: &[&Buffer], kept: &[i64]) -> Result<Vec<Buffer>, String> {
        let raw: Vec<Opaque> = arguments.iter().map(|b| b.raw).collect();
        let lists: [*const Opaque; 1] = [raw.as_ptr()];
        let mut results: Vec<Opaque> = vec![null_mut(); self.outputs];
        let result_lists: [*mut Opaque; 1] = [results.as_mut_ptr()];
        let mut options = ffi::ExecuteOptions::new();
        options.non_donatable_input_indices = kept.as_ptr();
        options.num_non_donatable_input_indices = kept.len();
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
