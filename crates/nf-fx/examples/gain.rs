//! Independent stereo gain CLAP plugin. Build with `cargo build -p nf-fx --example gain`, then copy `gain.dll` to `gain.clap`.
//! This binary uses the public CLAP ABI only. It shares no processing or transport code with the host.
use std::ffi::{CStr, c_char, c_void};
use std::ptr::{null, null_mut};

use clap_sys::entry::*;
use clap_sys::events::*;
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::params::*;
use clap_sys::factory::plugin_factory::*;
use clap_sys::host::*;
use clap_sys::plugin::*;
use clap_sys::process::*;
use clap_sys::version::*;

struct Features([*const c_char; 2]);
// Both pointers are static (including the null terminator required by CLAP).
unsafe impl Sync for Features {}
static FEATURES: Features = Features([c"audio-effect".as_ptr(), null()]);
static DESCRIPTOR: clap_plugin_descriptor = clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: c"org.noise-filter.gain".as_ptr(),
    name: c"Gain".as_ptr(),
    vendor: c"noise-filter".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"1.0".as_ptr(),
    description: c"Linear stereo gain".as_ptr(),
    features: FEATURES.0.as_ptr(),
};
struct Gain {
    plugin: clap_plugin,
    value: f64,
}
unsafe fn instance<'a>(p: *const clap_plugin) -> &'a mut Gain {
    unsafe { &mut *(*p).plugin_data.cast() }
}
unsafe extern "C" fn init(_: *const clap_plugin) -> bool {
    true
}
unsafe extern "C" fn destroy(p: *const clap_plugin) {
    unsafe {
        drop(Box::from_raw((*p).plugin_data.cast::<Gain>()));
    }
}
unsafe extern "C" fn activate(_: *const clap_plugin, _: f64, _: u32, _: u32) -> bool {
    true
}
unsafe extern "C" fn noop(_: *const clap_plugin) {}
unsafe extern "C" fn get_extension(_: *const clap_plugin, id: *const c_char) -> *const c_void {
    match unsafe { CStr::from_ptr(id) } {
        id if id == CLAP_EXT_AUDIO_PORTS => (&raw const PORTS).cast(),
        id if id == CLAP_EXT_PARAMS => (&raw const PARAMS).cast(),
        _ => null(),
    }
}
unsafe fn apply(p: *const clap_plugin, events: *const clap_input_events) {
    unsafe {
        for i in 0..(*events).size.unwrap()(events) {
            let event = (*events).get.unwrap()(events, i);
            if !event.is_null() && (*event).space_id == CLAP_CORE_EVENT_SPACE_ID && (*event).type_ == CLAP_EVENT_PARAM_VALUE {
                let value = &*event.cast::<clap_event_param_value>();
                if value.param_id == 0 {
                    instance(p).value = value.value.clamp(0., 2.);
                }
            }
        }
    }
}
unsafe extern "C" fn process(p: *const clap_plugin, process: *const clap_process) -> i32 {
    unsafe {
        let block = &*process;
        apply(p, block.in_events);
        let gain = instance(p).value as f32;
        for ch in 0..2 {
            let src = *(*block.audio_inputs).data32.add(ch);
            let dst = *(*block.audio_outputs).data32.add(ch);
            for i in 0..block.frames_count as usize {
                *dst.add(i) = *src.add(i) * gain;
            }
        }
        CLAP_PROCESS_CONTINUE
    }
}
unsafe extern "C" fn count_ports(_: *const clap_plugin, _: bool) -> u32 {
    1
}
unsafe extern "C" fn port(_: *const clap_plugin, i: u32, _: bool, p: *mut clap_audio_port_info) -> bool {
    if i != 0 {
        return false;
    }
    unsafe {
        let mut info: clap_audio_port_info = std::mem::zeroed();
        info.name[..4].copy_from_slice(&[77, 97, 105, 110]);
        info.id = 0;
        info.channel_count = 2;
        info.port_type = CLAP_PORT_STEREO.as_ptr();
        info.flags = CLAP_AUDIO_PORT_IS_MAIN;
        info.in_place_pair = 0;
        p.write(info);
    }
    true
}
static PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports { count: Some(count_ports), get: Some(port) };
unsafe extern "C" fn count_params(_: *const clap_plugin) -> u32 {
    1
}
unsafe extern "C" fn param(_: *const clap_plugin, i: u32, p: *mut clap_param_info) -> bool {
    if i != 0 {
        return false;
    }
    unsafe {
        let mut info: clap_param_info = std::mem::zeroed();
        info.name[..4].copy_from_slice(&[71, 97, 105, 110]);
        info.flags = CLAP_PARAM_IS_AUTOMATABLE;
        info.min_value = 0.;
        info.max_value = 2.;
        info.default_value = 1.;
        p.write(info);
    }
    true
}
unsafe extern "C" fn value(p: *const clap_plugin, i: u32, v: *mut f64) -> bool {
    if i != 0 {
        return false;
    }
    unsafe {
        v.write(instance(p).value);
    }
    true
}
unsafe extern "C" fn to_text(_: *const clap_plugin, id: u32, value: f64, out: *mut c_char, len: u32) -> bool {
    let bytes = format!("{value:.3}");
    if id != 0 || bytes.len() >= len as usize {
        return false;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr().cast(), out, bytes.len());
        *out.add(bytes.len()) = 0;
    }
    true
}
unsafe extern "C" fn from_text(_: *const clap_plugin, id: u32, text: *const c_char, out: *mut f64) -> bool {
    let Ok(v) = unsafe { CStr::from_ptr(text) }.to_string_lossy().parse::<f64>() else { return false };
    if id != 0 || !(0.0..=2.0).contains(&v) {
        return false;
    }
    unsafe {
        out.write(v);
    }
    true
}
unsafe extern "C" fn flush(p: *const clap_plugin, events: *const clap_input_events, _: *const clap_output_events) {
    unsafe { apply(p, events) }
}
static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(count_params),
    get_info: Some(param),
    get_value: Some(value),
    value_to_text: Some(to_text),
    text_to_value: Some(from_text),
    flush: Some(flush),
};
unsafe extern "C" fn count(_: *const clap_plugin_factory) -> u32 {
    1
}
unsafe extern "C" fn descriptor(_: *const clap_plugin_factory, i: u32) -> *const clap_plugin_descriptor {
    if i == 0 { &DESCRIPTOR } else { null() }
}
unsafe extern "C" fn create(_: *const clap_plugin_factory, _: *const clap_host, id: *const c_char) -> *const clap_plugin {
    if unsafe { CStr::from_ptr(id) } != c"org.noise-filter.gain" {
        return null();
    }
    let gain = Box::new(Gain {
        plugin: clap_plugin {
            desc: &DESCRIPTOR,
            plugin_data: null_mut(),
            init: Some(init),
            destroy: Some(destroy),
            activate: Some(activate),
            deactivate: Some(noop),
            start_processing: Some(init),
            stop_processing: Some(noop),
            reset: Some(noop),
            process: Some(process),
            get_extension: Some(get_extension),
            on_main_thread: Some(noop),
        },
        value: 1.,
    });
    let raw = Box::into_raw(gain);
    unsafe {
        (*raw).plugin.plugin_data = raw.cast();
        &raw const (*raw).plugin
    }
}
static FACTORY: clap_plugin_factory =
    clap_plugin_factory { get_plugin_count: Some(count), get_plugin_descriptor: Some(descriptor), create_plugin: Some(create) };
unsafe extern "C" fn entry_init(_: *const c_char) -> bool {
    true
}
unsafe extern "C" fn deinit() {}
unsafe extern "C" fn factory(id: *const c_char) -> *const c_void {
    if unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID { (&raw const FACTORY).cast() } else { null() }
}
#[unsafe(no_mangle)]
pub static clap_entry: clap_plugin_entry =
    clap_plugin_entry { clap_version: CLAP_VERSION, init: Some(entry_init), deinit: Some(deinit), get_factory: Some(factory) };
