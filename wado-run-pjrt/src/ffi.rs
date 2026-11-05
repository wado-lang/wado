//! The part of PJRT's C API (`xla/pjrt/c/pjrt_c_api.h`, version 0.117) a program
//! needs. Each call takes one struct, which opens with its own size, so a plugin
//! of another minor version reads the prefix it knows.

use std::ffi::{c_char, c_void};
use std::ptr::null_mut;

/// What a handle of the plugin's points to, which only the plugin reads.
pub type Opaque = *mut c_void;

/// A struct's opening: its size, then the extensions it starts.
#[repr(C)]
pub struct Header {
    pub struct_size: usize,
    pub extension_start: Opaque,
}

/// A call's argument struct.
///
/// # Safety
///
/// The type is `repr(C)`, opens with a [`Header`], and all zeros is a valid
/// value of it.
pub unsafe trait Arguments: Sized {
    /// All zeros but for the struct's size.
    fn new() -> Self {
        // SAFETY: the trait's contract says zeros are valid and a `Header` opens
        // the struct.
        unsafe {
            let mut arguments = std::mem::MaybeUninit::<Self>::zeroed();
            arguments.as_mut_ptr().cast::<Header>().write(Header {
                struct_size: size_of::<Self>(),
                extension_start: null_mut(),
            });
            arguments.assume_init()
        }
    }
}

macro_rules! arguments {
    ($($name:ident { $($field:ident: $ty:ty),* $(,)? })*) => {
        $(
            #[repr(C)]
            pub struct $name {
                pub header: Header,
                $(pub $field: $ty,)*
            }

            // SAFETY: `repr(C)`, a `Header` first, and every field a pointer,
            // an integer or a `bool`.
            unsafe impl Arguments for $name {}
        )*
    };
}

arguments! {
    ErrorDestroy { error: Opaque }
    ErrorMessage { error: Opaque, message: *const c_char, message_size: usize }
    PluginInitialize {}
    EventDestroy { event: Opaque }
    EventAwait { event: Opaque }
    ClientCreate {
        create_options: *const c_void,
        num_options: usize,
        kv_get_callback: Opaque,
        kv_get_user_arg: Opaque,
        kv_put_callback: Opaque,
        kv_put_user_arg: Opaque,
        client: Opaque,
        kv_try_get_callback: Opaque,
        kv_try_get_user_arg: Opaque,
    }
    ClientDestroy { client: Opaque }
    ClientPlatformName { client: Opaque, platform_name: *const c_char, platform_name_size: usize }
    Program {
        code: *const c_char,
        code_size: usize,
        format: *const c_char,
        format_size: usize,
    }
    ClientCompile {
        client: Opaque,
        program: *const Program,
        compile_options: *const c_char,
        compile_options_size: usize,
        executable: Opaque,
    }
    ClientBufferFromHostBuffer {
        client: Opaque,
        data: *const c_void,
        element_type: i32,
        dims: *const i64,
        num_dims: usize,
        byte_strides: *const i64,
        num_byte_strides: usize,
        host_buffer_semantics: i32,
        device: Opaque,
        memory: Opaque,
        device_layout: Opaque,
        done_with_host_buffer: Opaque,
        buffer: Opaque,
    }
    ClientAddressableDevices {
        client: Opaque,
        addressable_devices: *const Opaque,
        num_addressable_devices: usize,
    }
    ExecutableDestroy { executable: Opaque }
    ExecutableNumOutputs { executable: Opaque, num_outputs: usize }
    LoadedExecutableDestroy { executable: Opaque }
    LoadedExecutableGetExecutable { loaded_executable: Opaque, executable: Opaque }
    ExecuteOptions {
        send_callbacks: Opaque,
        recv_callbacks: Opaque,
        num_send_ops: usize,
        num_recv_ops: usize,
        launch_id: i32,
        non_donatable_input_indices: *const i64,
        num_non_donatable_input_indices: usize,
        context: Opaque,
        call_location: *const c_char,
        num_tasks: usize,
        task_ids: *mut i32,
        incarnation_ids: *mut i64,
        multi_slice_config: Opaque,
        use_major_to_minor_data_layout_for_callbacks: bool,
        hlo_output_callbacks: Opaque,
        num_hlo_output_callbacks: usize,
        custom_options: *const c_void,
        num_custom_options: usize,
    }
    LoadedExecutableExecute {
        executable: Opaque,
        options: *mut ExecuteOptions,
        argument_lists: *const *const Opaque,
        num_devices: usize,
        num_args: usize,
        output_lists: *const *mut Opaque,
        device_complete_events: *mut Opaque,
        execute_device: Opaque,
    }
    BufferDestroy { buffer: Opaque }
    BufferToHostBuffer {
        src: Opaque,
        host_layout: Opaque,
        dst: *mut c_void,
        dst_size: usize,
        event: Opaque,
    }
}

/// `PJRT_HostBufferSemantics_kImmutableOnlyDuringCall`: the plugin copies the
/// host's bytes before the call returns.
pub const COPIED_DURING_CALL: i32 = 0;

/// A call of the API table, by its place in `PJRT_Api`, which a new call only
/// ever extends.
#[derive(Clone, Copy)]
#[repr(usize)]
pub enum Slot {
    ErrorDestroy = 0,
    ErrorMessage = 1,
    PluginInitialize = 3,
    EventDestroy = 5,
    EventAwait = 8,
    ClientCreate = 10,
    ClientDestroy = 11,
    ClientPlatformName = 12,
    ClientAddressableDevices = 16,
    ClientCompile = 20,
    ClientBufferFromHostBuffer = 22,
    ExecutableDestroy = 40,
    ExecutableNumOutputs = 44,
    LoadedExecutableDestroy = 50,
    LoadedExecutableGetExecutable = 51,
    LoadedExecutableExecute = 55,
    BufferDestroy = 58,
    BufferToHostBuffer = 70,
}

#[repr(C)]
struct Version {
    header: Header,
    major_version: i32,
    minor_version: i32,
}

/// `PJRT_Api`: a table of calls, each taking its argument struct.
#[repr(C)]
pub struct Api {
    header: Header,
    version: Version,
    calls: [Option<unsafe extern "C" fn(*mut c_void) -> Opaque>; 1],
}

impl Api {
    /// The plugin's API version, major then minor.
    pub fn version(&self) -> (i32, i32) {
        (self.version.major_version, self.version.minor_version)
    }

    /// Calls `slot`, and returns the error it reports, null where it reports
    /// none.
    ///
    /// # Safety
    ///
    /// `A` is the struct `slot` takes, and holds handles of this plugin.
    pub unsafe fn call<A: Arguments>(&self, slot: Slot, arguments: &mut A) -> Opaque {
        let held =
            (self.header.struct_size - std::mem::offset_of!(Api, calls)) / size_of::<usize>();
        assert!(
            (slot as usize) < held,
            "a plugin's table holds every call of this file"
        );
        // SAFETY: `held` says the plugin's table reaches this entry, past the one
        // the array declares.
        unsafe {
            let call = self.calls.as_ptr().add(slot as usize).read();
            call.expect("a plugin implements every call of this file")(
                std::ptr::from_mut(arguments).cast(),
            )
        }
    }
}
