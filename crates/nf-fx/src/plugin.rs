//! CLAP calls run only inside the helper. Main-thread lifecycle and audio-thread lifecycle have separate owners.
use std::error::Error;
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::Path;
use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering::*;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};

use clap_sys::audio_buffer::*;
use clap_sys::entry::*;
use clap_sys::events::*;
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::latency::*;
use clap_sys::ext::params::*;
use clap_sys::ext::thread_check::*;
use clap_sys::factory::plugin_factory::*;
use clap_sys::host::*;
use clap_sys::plugin::*;
use clap_sys::process::*;
use clap_sys::version::*;
use libloading::Library;
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

use super::transport::{BLOCK, PARAMS};
use super::{EffectInfo, Parameter};

struct HostState {
    main: u32,
    audio: AtomicU32,
    callback: AtomicBool,
    restart: AtomicBool,
}
unsafe fn state<'a>(host: *const clap_host) -> &'a HostState {
    unsafe { &*(*host).host_data.cast() }
}
unsafe extern "C" fn is_main(host: *const clap_host) -> bool {
    unsafe { GetCurrentThreadId() == state(host).main }
}
unsafe extern "C" fn is_audio(host: *const clap_host) -> bool {
    unsafe { GetCurrentThreadId() == state(host).audio.load(Relaxed) }
}
static THREADS: clap_host_thread_check = clap_host_thread_check { is_main_thread: Some(is_main), is_audio_thread: Some(is_audio) };
unsafe extern "C" fn extension(_: *const clap_host, id: *const c_char) -> *const c_void {
    if !id.is_null() && unsafe { CStr::from_ptr(id) } == CLAP_EXT_THREAD_CHECK { (&raw const THREADS).cast() } else { null() }
}
unsafe extern "C" fn request_callback(host: *const clap_host) {
    unsafe {
        state(host).callback.store(true, Release);
    }
}
unsafe extern "C" fn request_restart(host: *const clap_host) {
    unsafe {
        state(host).restart.store(true, Release);
    }
}
unsafe extern "C" fn request_process(_: *const clap_host) {} // The host processes every mic block, including silence.
fn text(p: *const c_char) -> Result<String, String> {
    if p.is_null() {
        return Err("Missing CLAP descriptor string".into());
    }
    Ok(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned())
}

struct Entry {
    _library: Library,
    entry: &'static clap_plugin_entry,
}
impl Drop for Entry {
    fn drop(&mut self) {
        unsafe {
            if let Some(f) = self.entry.deinit {
                f()
            }
        }
    }
}
struct Instance {
    raw: &'static clap_plugin,
    active: bool,
    _host: Box<clap_host>,
    state: Box<HostState>,
    _entry: Entry,
}
impl Instance {
    fn ext<T>(&self, name: &CStr) -> Option<&T> {
        unsafe { self.raw.get_extension?(self.raw, name.as_ptr()).cast::<T>().as_ref() }
    }
    fn channels(&self, input: bool) -> Result<u32, String> {
        let ports = self.ext::<clap_plugin_audio_ports>(CLAP_EXT_AUDIO_PORTS).ok_or("Effect has no CLAP audio ports")?;
        let count = ports.count.ok_or("Missing port count")?;
        let get = ports.get.ok_or("Missing port get")?;
        let mut port: clap_audio_port_info = unsafe { std::mem::zeroed() };
        if unsafe { count(self.raw, input) } != 1
            || !unsafe { get(self.raw, 0, input, &mut port) }
            || port.flags & CLAP_AUDIO_PORT_IS_MAIN == 0
            || !matches!(port.channel_count, 1 | 2)
        {
            return Err("Effect needs one mono or stereo main input and output".into());
        }
        Ok(port.channel_count)
    }
    fn parameters(&self) -> Result<Vec<Parameter>, String> {
        let Some(params) = self.ext::<clap_plugin_params>(CLAP_EXT_PARAMS) else { return Ok(vec![]) };
        let count = unsafe { params.count.ok_or("Missing parameter count")?(self.raw) };
        if count > 65536 {
            return Err("Invalid CLAP parameter count".into());
        }
        let get = params.get_info.ok_or("Missing CLAP parameter information")?;
        let mut result = vec![];
        for index in 0..count {
            let mut p: clap_param_info = unsafe { std::mem::zeroed() };
            if !unsafe { get(self.raw, index, &mut p) } {
                return Err(format!("Cannot read effect parameter {index}"));
            }
            if p.flags & (CLAP_PARAM_IS_READONLY | CLAP_PARAM_IS_HIDDEN) != 0 {
                continue;
            }
            if result.len() == PARAMS {
                return Err(format!("This host supports up to {PARAMS} editable effect parameters"));
            }
            let name: Vec<u8> = p.name.iter().take_while(|&&b| b != 0).map(|&b| b as u8).collect();
            result.push(Parameter {
                id: p.id,
                name: String::from_utf8_lossy(&name).into_owned(),
                min: p.min_value,
                max: p.max_value,
                default: p.default_value,
                stepped: p.flags & CLAP_PARAM_IS_STEPPED != 0,
            });
        }
        Ok(result)
    }
}
impl Drop for Instance {
    fn drop(&mut self) {
        unsafe {
            if self.active
                && let Some(f) = self.raw.deactivate
            {
                f(self.raw);
            }
            if let Some(f) = self.raw.destroy {
                f(self.raw);
            }
        }
    }
}

pub(super) struct Plugin {
    instance: Instance,
    info: EffectInfo,
    inputs: u32,
    outputs: u32,
}
impl Plugin {
    pub fn open(path: &Path, id: Option<&str>) -> Result<Self, String> {
        let name = CString::new(path.to_str().ok_or("Plugin path is not UTF-8")?).map_err(|e| e.to_string())?;
        unsafe {
            let lib = Library::new(path).map_err(|e| {
                let cause = e.source().unwrap_or(&e);
                let advice = if cause.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied) {
                    " Choose a plugin location readable by your normal Windows user."
                } else {
                    ""
                };
                format!("Cannot load CLAP effect '{}': {cause}.{advice}", path.display())
            })?;
            let entry = (*lib.get::<*const clap_plugin_entry>(b"clap_entry\0").map_err(|_| "Effect does not export clap_entry")?)
                .as_ref()
                .ok_or("Null CLAP entry")?;
            if !clap_version_is_compatible(entry.clap_version) || !entry.init.ok_or("Missing CLAP entry initialization")?(name.as_ptr()) {
                return Err("Incompatible CLAP entry or initialization failed".into());
            }
            let entry = Entry { _library: lib, entry };
            let factory = entry.entry.get_factory.ok_or("Missing CLAP factory")?(CLAP_PLUGIN_FACTORY_ID.as_ptr())
                .cast::<clap_plugin_factory>()
                .as_ref()
                .ok_or("Missing CLAP plugin factory")?;
            let count = factory.get_plugin_count.ok_or("Missing plugin count")?(factory);
            if count > 65536 {
                return Err("Invalid CLAP factory count".into());
            }
            if count > 1 && id.is_none() {
                return Err(
                    "This file contains multiple CLAP plugins. Choose a single-effect file or set effect.plugin_id in the config.".into()
                );
            }
            let desc = (0..count)
                .filter_map(|i| factory.get_plugin_descriptor.and_then(|f| f(factory, i).as_ref()))
                .find(|d| {
                    !d.id.is_null()
                        && clap_version_is_compatible(d.clap_version)
                        && id.is_none_or(|id| CStr::from_ptr(d.id).to_bytes() == id.as_bytes())
                })
                .ok_or("No matching plugin in the CLAP factory")?;
            let mut state = Box::new(HostState {
                main: GetCurrentThreadId(),
                audio: AtomicU32::new(0),
                callback: AtomicBool::new(false),
                restart: AtomicBool::new(false),
            });
            let host = Box::new(clap_host {
                clap_version: CLAP_VERSION,
                host_data: (&raw mut *state).cast(),
                name: c"noise-filter".as_ptr(),
                vendor: c"noise-filter".as_ptr(),
                url: c"https://github.com/jlpetz/acx-ll-cable-filter".as_ptr(),
                version: c"0.1.0".as_ptr(),
                get_extension: Some(extension),
                request_restart: Some(request_restart),
                request_process: Some(request_process),
                request_callback: Some(request_callback),
            });
            let raw = factory.create_plugin.ok_or("Missing create_plugin")?(factory, &*host, desc.id)
                .as_ref()
                .ok_or("The CLAP factory could not create that effect")?;
            let mut instance = Instance { raw, active: false, _host: host, state, _entry: entry };
            if !raw.init.ok_or("Missing plugin initialization")?(raw) {
                return Err("CLAP effect initialization failed".into());
            }
            if raw.deactivate.is_none()
                || raw.destroy.is_none()
                || raw.reset.is_none()
                || raw.start_processing.is_none()
                || raw.stop_processing.is_none()
                || raw.process.is_none()
            {
                return Err("Incomplete CLAP processing interface".into());
            }
            let inputs = instance.channels(true)?;
            let outputs = instance.channels(false)?;
            let info = EffectInfo { id: text(desc.id)?, name: text(desc.name)?, params: instance.parameters()? };
            info.validate()?;
            if !raw.activate.ok_or("Missing CLAP activate")?(raw, 48000., BLOCK as u32, BLOCK as u32) {
                return Err("Effect refused 48 kHz, 480-frame activation".into());
            }
            instance.active = true;
            if let Some(latency) = instance.ext::<clap_plugin_latency>(CLAP_EXT_LATENCY) {
                let frames = latency.get.ok_or("Missing CLAP latency query")?(raw);
                if frames != 0 {
                    return Err(format!("Effect adds {frames} samples of latency. This host currently supports zero-latency effects."));
                }
            }
            Ok(Self { instance, info, inputs, outputs })
        }
    }
    pub fn info(&self) -> &EffectInfo {
        &self.info
    }
    pub fn split(&self) -> (Main<'_>, Audio<'_>) {
        let main = Main { raw: self.instance.raw, state: &self.instance.state };
        (main, Audio { main, params: &self.info.params, inputs: self.inputs, outputs: self.outputs })
    }
}
#[derive(Clone, Copy)]
pub(super) struct Main<'a> {
    raw: &'a clap_plugin,
    state: &'a HostState,
}
impl Main<'_> {
    pub fn restart_requested(&self) -> bool {
        self.state.restart.load(Acquire)
    }
    pub fn callbacks(&self) {
        if self.state.callback.swap(false, AcqRel)
            && let Some(f) = self.raw.on_main_thread
        {
            unsafe { f(self.raw) }
        }
    }
}
pub(super) struct Audio<'a> {
    main: Main<'a>,
    params: &'a [Parameter],
    inputs: u32,
    outputs: u32,
}
impl<'a> Audio<'a> {
    pub fn start(self) -> Result<Processor<'a>, String> {
        unsafe {
            self.main.state.audio.store(GetCurrentThreadId(), Relaxed);
            if !self.main.raw.start_processing.unwrap()(self.main.raw) {
                return Err("Effect refused start_processing".into());
            }
        }
        let events = Vec::with_capacity(self.params.len());
        Ok(Processor { audio: self, time: 0, prior: [u64::MAX; PARAMS], events })
    }
}
pub(super) struct Processor<'a> {
    audio: Audio<'a>,
    time: i64,
    prior: [u64; PARAMS],
    events: Vec<clap_event_param_value>,
}
unsafe extern "C" fn event_count(events: *const clap_input_events) -> u32 {
    unsafe { (&*(*events).ctx.cast::<Vec<clap_event_param_value>>()).len() as u32 }
}
unsafe extern "C" fn event_get(events: *const clap_input_events, index: u32) -> *const clap_event_header {
    unsafe { (&*(*events).ctx.cast::<Vec<clap_event_param_value>>()).get(index as usize).map_or(null(), |e| &e.header) }
}
unsafe extern "C" fn output_event(_events: *const clap_output_events, _event: *const clap_event_header) -> bool {
    false
}
impl Processor<'_> {
    pub fn reset(&mut self) {
        unsafe { self.audio.main.raw.reset.unwrap()(self.audio.main.raw) }
        self.prior.fill(u64::MAX);
    }
    pub fn process(&mut self, input: &[f32; BLOCK], out: &mut [f32; BLOCK], values: &[AtomicU64; PARAMS]) -> Result<(), String> {
        self.events.clear();
        for (i, p) in self.audio.params.iter().enumerate() {
            let bits = values[i].load(Acquire);
            if self.prior[i] == bits {
                continue;
            }
            self.prior[i] = bits;
            self.events.push(clap_event_param_value {
                header: clap_event_header {
                    size: size_of::<clap_event_param_value>() as u32,
                    time: 0,
                    space_id: CLAP_CORE_EVENT_SPACE_ID,
                    type_: CLAP_EVENT_PARAM_VALUE,
                    flags: 0,
                },
                param_id: p.id,
                cookie: null_mut(),
                note_id: -1,
                port_index: -1,
                channel: -1,
                key: -1,
                value: f64::from_bits(bits),
            });
        }
        let events = clap_input_events { ctx: (&raw mut self.events).cast(), size: Some(event_count), get: Some(event_get) };
        let outputs = clap_output_events { ctx: null_mut(), try_push: Some(output_event) };
        // CLAP buffers are mutable pointers. Supply independent non-aliased left and right input buffers, even for dual mono.
        let mut left = *input;
        let mut right = left;
        let mut right_out = [0.; BLOCK];
        out.fill(0.);
        let mut ins = [left.as_mut_ptr(), right.as_mut_ptr()];
        let mut outs = [out.as_mut_ptr(), right_out.as_mut_ptr()];
        let input = clap_audio_buffer {
            data32: ins.as_mut_ptr(),
            data64: null_mut(),
            channel_count: self.audio.inputs,
            latency: 0,
            constant_mask: 0,
        };
        let mut output = clap_audio_buffer {
            data32: outs.as_mut_ptr(),
            data64: null_mut(),
            channel_count: self.audio.outputs,
            latency: 0,
            constant_mask: 0,
        };
        let process = clap_process {
            steady_time: self.time,
            frames_count: BLOCK as u32,
            transport: null(),
            audio_inputs: &input,
            audio_outputs: &mut output,
            audio_inputs_count: 1,
            audio_outputs_count: 1,
            in_events: &events,
            out_events: &outputs,
        };
        let raw = self.audio.main.raw;
        let result = unsafe { raw.process.unwrap()(raw, &process) };
        if !(CLAP_PROCESS_CONTINUE..=CLAP_PROCESS_SLEEP).contains(&result) {
            return Err("CLAP effect returned a processing error".into());
        }
        if self.audio.outputs == 2 {
            for (a, b) in out.iter_mut().zip(right_out) {
                *a = *a * 0.5 + b * 0.5;
            }
        }
        if !out.iter().all(|v| v.is_finite()) {
            return Err("CLAP effect produced nonfinite samples".into());
        }
        self.time += BLOCK as i64;
        Ok(())
    }
}
impl Drop for Processor<'_> {
    fn drop(&mut self) {
        unsafe {
            self.audio.main.raw.stop_processing.unwrap()(self.audio.main.raw);
        }
        self.audio.main.state.audio.store(0, Relaxed);
    }
}
