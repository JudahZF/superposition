//! Safe Rust ownership wrapper for the official VST3 SDK C++ shim.
//!
//! The shim contains every C++ virtual call, COM reference, SDK object, and
//! platform editor pointer. This module uses only checked POD conversions.

#![allow(unsafe_code)] // `unsafe extern` is the crate's isolated FFI boundary.

use std::{
    ffi::{CStr, CString, c_char, c_void},
    fmt,
    path::Path,
    ptr::NonNull,
};

use crate::adapter::{ParameterFlags, Vst3ParameterInfo, Vst3StateStreams};

#[repr(C)]
struct RawParameter {
    id: u32,
    step_count: i32,
    flags: u32,
    normalized: f64,
    default_normalized: f64,
    title: [c_char; 128],
    short_title: [c_char; 128],
    units: [c_char; 64],
}
#[repr(C)]
struct RawBus {
    media: u8,
    direction: u8,
    bus_type: u8,
    channels: u8,
}
#[repr(C)]
struct Handle {
    _private: [u8; 0],
}

unsafe extern "C" {
    safe fn sp_vst3_native_create(
        path: *const c_char,
        class_id: *const c_char,
        sample_rate: f64,
        maximum_frames: i32,
    ) -> *mut Handle;
    safe fn sp_vst3_native_destroy(handle: *mut Handle);
    safe fn sp_vst3_native_error(handle: *const Handle) -> *const c_char;
    safe fn sp_vst3_native_initialize_component(handle: *mut Handle) -> i32;
    safe fn sp_vst3_native_initialize_controller(handle: *mut Handle) -> i32;
    safe fn sp_vst3_native_connect(handle: *mut Handle) -> i32;
    safe fn sp_vst3_native_bus_count(handle: *mut Handle, media: u8, direction: u8) -> i32;
    safe fn sp_vst3_native_bus_info(
        handle: *mut Handle,
        media: u8,
        direction: u8,
        index: i32,
        out: *mut RawBus,
    ) -> i32;
    safe fn sp_vst3_native_set_arrangements(handle: *mut Handle, input: u8, output: u8) -> i32;
    safe fn sp_vst3_native_activate_bus(
        handle: *mut Handle,
        media: u8,
        direction: u8,
        index: i32,
        active: u8,
    ) -> i32;
    safe fn sp_vst3_native_start(handle: *mut Handle) -> i32;
    safe fn sp_vst3_native_stop(handle: *mut Handle) -> i32;
    safe fn sp_vst3_native_parameter_count(handle: *mut Handle) -> i32;
    safe fn sp_vst3_native_parameter_info(
        handle: *mut Handle,
        index: i32,
        out: *mut RawParameter,
    ) -> i32;
    safe fn sp_vst3_native_set_parameter(handle: *mut Handle, id: u32, value: f64) -> i32;
    safe fn sp_vst3_native_get_parameter(handle: *mut Handle, id: u32, out: *mut f64) -> i32;
    safe fn sp_vst3_native_format_parameter(
        handle: *mut Handle,
        id: u32,
        value: f64,
        out: *mut c_char,
        capacity: i32,
    ) -> i32;
    safe fn sp_vst3_native_get_component_state(
        handle: *mut Handle,
        data: *mut *mut u8,
        size: *mut i32,
    ) -> i32;
    safe fn sp_vst3_native_get_controller_state(
        handle: *mut Handle,
        data: *mut *mut u8,
        size: *mut i32,
    ) -> i32;
    safe fn sp_vst3_native_free_bytes(data: *mut u8);
    safe fn sp_vst3_native_set_component_state(
        handle: *mut Handle,
        data: *const u8,
        size: i32,
    ) -> i32;
    safe fn sp_vst3_native_set_component_state_on_controller(
        handle: *mut Handle,
        data: *const u8,
        size: i32,
    ) -> i32;
    safe fn sp_vst3_native_set_controller_state(
        handle: *mut Handle,
        data: *const u8,
        size: i32,
    ) -> i32;
    safe fn sp_vst3_native_latency(handle: *mut Handle) -> u32;
    safe fn sp_vst3_native_take_restart_flags(handle: *mut Handle) -> u32;
    safe fn sp_vst3_native_open_editor(
        handle: *mut Handle,
        parent: *mut c_void,
        width: *mut i32,
        height: *mut i32,
    ) -> i32;
    safe fn sp_vst3_native_close_editor(handle: *mut Handle) -> i32;
    safe fn sp_vst3_native_resize_editor(handle: *mut Handle, width: i32, height: i32) -> i32;
}

/// Native SDK failure, including a plug-in supplied VST3 result.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSdkError(String);
impl fmt::Display for NativeSdkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for NativeSdkError {}

/// An opaque, exact-class VST3 component/controller pair.
pub struct NativeSdkPlugin {
    handle: NonNull<Handle>,
}
impl NativeSdkPlugin {
    /// Loads the module and creates precisely `class_id`; it does not initialize it.
    pub fn load(
        path: &Path,
        class_id: &str,
        sample_rate_hz: f64,
        maximum_frames: usize,
    ) -> Result<Self, NativeSdkError> {
        let path = CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|_| NativeSdkError("VST3 module path contains a NUL byte".into()))?;
        let class = CString::new(class_id)
            .map_err(|_| NativeSdkError("VST3 class id contains a NUL byte".into()))?;
        let frames = i32::try_from(maximum_frames)
            .map_err(|_| NativeSdkError("VST3 maximum frame count exceeds i32".into()))?;
        NonNull::new(sp_vst3_native_create(
            path.as_ptr(),
            class.as_ptr(),
            sample_rate_hz,
            frames,
        ))
        .map(|handle| Self { handle })
        .ok_or_else(|| {
            NativeSdkError(
                "VST3 module load failed before a diagnostic handle was available".into(),
            )
        })
    }
    fn call(&mut self, result: i32) -> Result<(), NativeSdkError> {
        if result == 0 {
            Ok(())
        } else {
            Err(self.error())
        }
    }
    fn error(&self) -> NativeSdkError {
        let pointer = sp_vst3_native_error(self.handle.as_ptr());
        if pointer.is_null() {
            return NativeSdkError("native VST3 shim failed without a diagnostic".into());
        };
        let message = unsafe { CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned();
        NativeSdkError(message)
    }
    /// Executes `IComponent::initialize`.
    pub fn initialize_component(&mut self) -> Result<(), NativeSdkError> {
        self.call(sp_vst3_native_initialize_component(self.handle.as_ptr()))
    }
    /// Creates, initializes, and installs the component handler on the controller.
    pub fn initialize_controller(&mut self) -> Result<(), NativeSdkError> {
        self.call(sp_vst3_native_initialize_controller(self.handle.as_ptr()))
    }
    /// Connects both supported `IConnectionPoint` directions.
    pub fn connect(&mut self) -> Result<(), NativeSdkError> {
        self.call(sp_vst3_native_connect(self.handle.as_ptr()))
    }
    /// Returns a raw VST3 bus count for the selected media and direction.
    pub fn bus_count(&mut self, media: u8, direction: u8) -> Result<usize, NativeSdkError> {
        let count = sp_vst3_native_bus_count(self.handle.as_ptr(), media, direction);
        usize::try_from(count).map_err(|_| self.error())
    }
    /// Sets main audio arrangements and invokes `IAudioProcessor::setupProcessing`.
    pub fn set_arrangements(&mut self, input: u8, output: u8) -> Result<(), NativeSdkError> {
        self.call(sp_vst3_native_set_arrangements(
            self.handle.as_ptr(),
            input,
            output,
        ))
    }
    /// Activates a bus while inactive.
    pub fn activate_bus(
        &mut self,
        media: u8,
        direction: u8,
        index: i32,
        active: bool,
    ) -> Result<(), NativeSdkError> {
        self.call(sp_vst3_native_activate_bus(
            self.handle.as_ptr(),
            media,
            direction,
            index,
            u8::from(active),
        ))
    }
    /// Starts component processing.
    pub fn start(&mut self) -> Result<(), NativeSdkError> {
        self.call(sp_vst3_native_start(self.handle.as_ptr()))
    }
    /// Stops component processing.
    pub fn stop(&mut self) -> Result<(), NativeSdkError> {
        self.call(sp_vst3_native_stop(self.handle.as_ptr()))
    }
    /// Gets full controller parameter metadata.
    pub fn parameters(&mut self) -> Result<Vec<Vst3ParameterInfo>, NativeSdkError> {
        let count = self.call_count(sp_vst3_native_parameter_count(self.handle.as_ptr()))?;
        let mut values = Vec::with_capacity(count);
        for index in 0..count {
            let mut raw = RawParameter {
                id: 0,
                step_count: 0,
                flags: 0,
                normalized: 0.,
                default_normalized: 0.,
                title: [0; 128],
                short_title: [0; 128],
                units: [0; 64],
            };
            self.call(sp_vst3_native_parameter_info(
                self.handle.as_ptr(),
                i32::try_from(index)
                    .map_err(|_| NativeSdkError("parameter index overflow".into()))?,
                &mut raw,
            ))?;
            values.push(Vst3ParameterInfo {
                id: raw.id,
                title: chars(&raw.title),
                short_title: chars(&raw.short_title),
                unit: chars(&raw.units),
                normalized: raw.normalized,
                default_normalized: raw.default_normalized,
                step_count: raw.step_count,
                flags: ParameterFlags(raw.flags),
            });
        }
        Ok(values)
    }
    fn call_count(&self, result: i32) -> Result<usize, NativeSdkError> {
        usize::try_from(result).map_err(|_| self.error())
    }
    /// Writes a normalized controller parameter value.
    pub fn set_parameter(&mut self, id: u32, value: f64) -> Result<(), NativeSdkError> {
        self.call(sp_vst3_native_set_parameter(
            self.handle.as_ptr(),
            id,
            value,
        ))
    }
    /// Reads a normalized controller parameter value.
    pub fn parameter(&mut self, id: u32) -> Result<f64, NativeSdkError> {
        let mut value = 0.;
        self.call(sp_vst3_native_get_parameter(
            self.handle.as_ptr(),
            id,
            &mut value,
        ))?;
        Ok(value)
    }
    /// Formats a value through the controller.
    pub fn format_parameter(&mut self, id: u32, value: f64) -> Result<String, NativeSdkError> {
        let mut text = [0 as c_char; 512];
        self.call(sp_vst3_native_format_parameter(
            self.handle.as_ptr(),
            id,
            value,
            text.as_mut_ptr(),
            512,
        ))?;
        Ok(chars(&text))
    }
    /// Captures independent component and controller streams.
    pub fn capture_state(&mut self) -> Result<Vst3StateStreams, NativeSdkError> {
        Ok(Vst3StateStreams {
            component: self.state(true)?,
            controller: self.state(false)?,
        })
    }
    fn state(&mut self, component: bool) -> Result<Vec<u8>, NativeSdkError> {
        let (mut pointer, mut size) = (std::ptr::null_mut(), 0);
        let status = if component {
            sp_vst3_native_get_component_state(self.handle.as_ptr(), &mut pointer, &mut size)
        } else {
            sp_vst3_native_get_controller_state(self.handle.as_ptr(), &mut pointer, &mut size)
        };
        self.call(status)?;
        if size < 0 {
            return Err(NativeSdkError(
                "native VST3 shim returned a negative state size".into(),
            ));
        };
        let bytes = unsafe {
            std::slice::from_raw_parts(
                pointer,
                usize::try_from(size).map_err(|_| NativeSdkError("state size overflow".into()))?,
            )
        }
        .to_vec();
        sp_vst3_native_free_bytes(pointer);
        Ok(bytes)
    }
    /// Restores state in component, controller synchronization, controller-only order.
    pub fn restore_state(&mut self, state: &Vst3StateStreams) -> Result<(), NativeSdkError> {
        let component = state.component.as_ptr();
        let controller = state.controller.as_ptr();
        self.call(sp_vst3_native_set_component_state(
            self.handle.as_ptr(),
            component,
            i32::try_from(state.component.len())
                .map_err(|_| NativeSdkError("component state is too large".into()))?,
        ))?;
        self.call(sp_vst3_native_set_component_state_on_controller(
            self.handle.as_ptr(),
            component,
            i32::try_from(state.component.len())
                .map_err(|_| NativeSdkError("component state is too large".into()))?,
        ))?;
        self.call(sp_vst3_native_set_controller_state(
            self.handle.as_ptr(),
            controller,
            i32::try_from(state.controller.len())
                .map_err(|_| NativeSdkError("controller state is too large".into()))?,
        ))
    }
    /// Retrieves current processor latency.
    pub fn latency_samples(&mut self) -> u32 {
        sp_vst3_native_latency(self.handle.as_ptr())
    }
    /// Retrieves and clears restart flags from `IComponentHandler::restartComponent`.
    pub fn take_restart_flags(&mut self) -> u32 {
        sp_vst3_native_take_restart_flags(self.handle.as_ptr())
    }
    /// Attaches an editor to a worker-owned `NSView`.
    pub fn open_editor(&mut self, parent: *mut c_void) -> Result<(u32, u32), NativeSdkError> {
        let (mut w, mut h) = (0, 0);
        self.call(sp_vst3_native_open_editor(
            self.handle.as_ptr(),
            parent,
            &mut w,
            &mut h,
        ))?;
        Ok((
            u32::try_from(w).map_err(|_| self.error())?,
            u32::try_from(h).map_err(|_| self.error())?,
        ))
    }
    /// Detaches the editor.
    pub fn close_editor(&mut self) -> Result<(), NativeSdkError> {
        self.call(sp_vst3_native_close_editor(self.handle.as_ptr()))
    }
    /// Delivers an accepted native editor resize.
    pub fn resize_editor(&mut self, width: u32, height: u32) -> Result<(), NativeSdkError> {
        self.call(sp_vst3_native_resize_editor(
            self.handle.as_ptr(),
            i32::try_from(width).map_err(|_| NativeSdkError("editor width overflow".into()))?,
            i32::try_from(height).map_err(|_| NativeSdkError("editor height overflow".into()))?,
        ))
    }
}
impl Drop for NativeSdkPlugin {
    fn drop(&mut self) {
        sp_vst3_native_destroy(self.handle.as_ptr());
    }
}
fn chars(chars: &[c_char]) -> String {
    let bytes = chars
        .iter()
        .map(|byte| *byte as u8)
        .take_while(|byte| *byte != 0)
        .collect::<Vec<_>>();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn non_native_contracts_are_checked_before_loading() {
        assert!(NativeSdkPlugin::load(Path::new("bad\0"), "x", 48_000., 256).is_err());
    }
}
