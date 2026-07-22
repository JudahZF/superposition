//! `CoreAudio` output boundary for Superposition on macOS.
//!
//! Phase 1 harness APIs (`ActiveOutput`, `PhaseOneRenderer`) remain available for xtask
//! evidence. The product path implements [`sp_audio_io::AudioEndpoint`] in [`product`].

mod product;
mod rack_dispatcher;

pub use product::{
    MacOsAudioEndpoint, PreparedProductScene, ProductControl, ProductControlReceiver,
    ProductRackDiagnostics, ProductRenderer, ProductTelemetry, current_product_parameters,
    prepare_product_scenes, product_midi_mappings,
};
pub use rack_dispatcher::{
    RackAutomationEvents, RackDispatchTelemetry, RackSharedMemoryDispatcher,
};

use std::{
    error::Error,
    ffi::{CStr, c_char, c_void},
    fmt, mem,
    ptr::NonNull,
};

use sp_audio_io::{
    AudioDeviceCapabilities, AudioDeviceId, AudioDeviceInfo, AudioEndpointEvent, AudioRouteConfig,
};

#[cfg(test)]
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

const STATUS_UNSUPPORTED_FORMAT: i32 = -70_000;
const RENDERED: u32 = 1;

/// The only callback sizes supported by the Phase 1 output harness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhaseOneFrames {
    /// A callback contains 128 stereo frames.
    Frames128,
    /// A callback contains 256 stereo frames.
    Frames256,
}

impl PhaseOneFrames {
    /// Returns this callback size as a frame count.
    #[must_use]
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::Frames128 => 128,
            Self::Frames256 => 256,
        }
    }

    fn from_u32(frames: u32) -> Option<Self> {
        match frames {
            128 => Some(Self::Frames128),
            256 => Some(Self::Frames256),
            _ => None,
        }
    }

    const fn interleaved_sample_count(self) -> usize {
        match self {
            Self::Frames128 => 256,
            Self::Frames256 => 512,
        }
    }
}

/// Fixed Phase 1 output settings and an explicit device-reconfiguration choice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhaseOneConfig {
    frames: PhaseOneFrames,
    allow_device_reconfiguration: bool,
}

impl PhaseOneConfig {
    /// Creates a verify-only configuration for 48 kHz interleaved stereo output.
    ///
    /// The device is never reconfigured unless [`Self::allow_device_reconfiguration`] is
    /// called on this value.
    #[must_use]
    pub const fn new(frames: PhaseOneFrames) -> Self {
        Self {
            frames,
            allow_device_reconfiguration: false,
        }
    }

    /// Explicitly permits requesting the fixed Phase 1 sample rate and frame count.
    ///
    /// Immutable constraints, including stereo output, frame-size range, and fixed-size
    /// callback behavior, are checked before either mutable property is changed. A failed
    /// setup reports acknowledged mutations and the final observed device state; it does
    /// not roll shared device settings back.
    #[must_use]
    pub const fn allow_device_reconfiguration(mut self) -> Self {
        self.allow_device_reconfiguration = true;
        self
    }

    /// Returns the required callback size.
    #[must_use]
    pub const fn frames(self) -> PhaseOneFrames {
        self.frames
    }

    /// Returns whether this configuration permits a device format request.
    #[must_use]
    pub const fn allows_device_reconfiguration(self) -> bool {
        self.allow_device_reconfiguration
    }
}

/// Whether a renderer supplied sound or intentionally left its output silent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderDisposition {
    /// The renderer filled the whole supplied buffer with output samples.
    Rendered,
    /// The callback must leave the complete supplied buffer silent.
    Silence,
}

/// A validated mutable interleaved stereo `f32` output buffer.
pub struct InterleavedStereoF32<'a> {
    samples: &'a mut [f32],
    frames: PhaseOneFrames,
}

impl<'a> InterleavedStereoF32<'a> {
    /// Validates an interleaved sample slice for the supplied Phase 1 callback size.
    ///
    /// # Errors
    ///
    /// Returns [`InterleavedStereoF32Error`] when `samples` does not contain exactly two
    /// samples for every requested frame.
    pub fn new(
        samples: &'a mut [f32],
        frames: PhaseOneFrames,
    ) -> Result<Self, InterleavedStereoF32Error> {
        let expected_samples = frames.interleaved_sample_count();
        if samples.len() != expected_samples {
            return Err(InterleavedStereoF32Error::SampleCount {
                expected: expected_samples,
                actual: samples.len(),
            });
        }
        Ok(Self { samples, frames })
    }

    /// Returns the validated callback size.
    #[must_use]
    pub const fn frames(&self) -> PhaseOneFrames {
        self.frames
    }

    /// Returns the mutable interleaved sample storage.
    pub fn samples_mut(&mut self) -> &mut [f32] {
        self.samples
    }
}

/// A failure to construct [`InterleavedStereoF32`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InterleavedStereoF32Error {
    /// The provided storage did not have exactly two samples per frame.
    SampleCount {
        /// The required sample count.
        expected: usize,
        /// The supplied sample count.
        actual: usize,
    },
}

impl fmt::Display for InterleavedStereoF32Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SampleCount { expected, actual } => write!(
                formatter,
                "expected {expected} interleaved stereo samples, got {actual}"
            ),
        }
    }
}

impl Error for InterleavedStereoF32Error {}

/// Supplies samples to the Phase 1 real-time render callback.
///
/// Implementations execute on `CoreAudio`'s real-time thread. They must not allocate, lock,
/// log, wait, panic, or otherwise perform an operation that can block. Returning
/// [`RenderDisposition::Rendered`] promises that every sample in `output` has been written.
/// A panic cannot unwind across this non-unwinding C callback boundary and aborts the process;
/// the harness deliberately does not run panic recovery machinery on the real-time thread.
pub trait PhaseOneRenderer: Send + 'static {
    /// Renders a single validated interleaved stereo callback buffer.
    fn render(&mut self, output: InterleavedStereoF32<'_>) -> RenderDisposition;
}

/// A fixed borrowed duplex callback block. The AUHAL shim owns the preallocated capture
/// scratch buffer and converts the selected device format to interleaved product stereo before
/// this value is constructed.
pub struct DuplexStereoF32<'a> {
    input: &'a [f32],
    output: InterleavedStereoF32<'a>,
}

impl DuplexStereoF32<'_> {
    /// Captured interleaved product-stereo input for this exact block.
    #[must_use]
    pub fn input(&self) -> &[f32] {
        self.input
    }

    /// Mutable interleaved product-stereo output for this exact block.
    pub fn output_mut(&mut self) -> &mut [f32] {
        self.output.samples_mut()
    }

    /// Fixed frame count shared by input and output.
    #[must_use]
    pub const fn frames(&self) -> PhaseOneFrames {
        self.output.frames()
    }
}

/// Product renderer contract for the duplex AUHAL path.
///
/// This method is called by the `CoreAudio` thread. It receives only borrowed preallocated
/// buffers, and therefore must not allocate, lock, wait, or make control-plane calls.
pub trait DuplexRenderer: Send + 'static {
    /// Processes one fixed-size captured block and writes its playback block.
    fn render(&mut self, block: DuplexStereoF32<'_>) -> RenderDisposition;
}

/// `CoreAudio` device metadata with the stable identifier and directional capabilities needed to
/// select a route. A device ID is `coreaudio:<AudioDeviceID>` and remains stable for the life of
/// the `CoreAudio` device object; callers must handle a later `DeviceLost` event.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MacOsAudioDevice {
    /// Platform-neutral visible metadata.
    pub info: AudioDeviceInfo,
    /// Directional channels and default-device flags.
    pub capabilities: AudioDeviceCapabilities,
    /// Fixed product callback sizes supported by this device at 48 kHz.
    pub supported_buffer_frames: Vec<u32>,
}

fn clamp_channels(channels: u32) -> u16 {
    u16::try_from(channels.min(u32::from(u16::MAX))).expect("clamped to u16 range")
}

/// Lists hardware devices directly from `CoreAudio`. No audio unit is opened by enumeration.
///
/// # Errors
/// Returns [`CoreAudioError`] when the system device query fails.
pub fn enumerate_devices() -> Result<Vec<MacOsAudioDevice>, CoreAudioError> {
    let count = unsafe { sp_audio_device_count() };
    let mut devices = Vec::with_capacity(usize::try_from(count).unwrap_or(0));
    for index in 0..count {
        let mut raw = RawEnumeratedDevice::default();
        let status = unsafe { sp_audio_device_at(index, &raw mut raw) };
        if status != 0 {
            return Err(CoreAudioError::System {
                operation: CoreAudioOperation::EnumerateDevices,
                status,
            });
        }
        let name = unsafe { CStr::from_ptr(raw.name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let id = AudioDeviceId::new(format!("coreaudio:{}", raw.device_id));
        devices.push(MacOsAudioDevice {
            info: AudioDeviceInfo {
                id,
                name,
                max_output_channels: clamp_channels(raw.output_channels),
            },
            capabilities: AudioDeviceCapabilities {
                max_input_channels: clamp_channels(raw.input_channels),
                max_output_channels: clamp_channels(raw.output_channels),
                is_default_input: raw.is_default_input != 0,
                is_default_output: raw.is_default_output != 0,
            },
            supported_buffer_frames: supported_buffer_frames(&raw),
        });
    }
    Ok(devices)
}

fn supported_buffer_frames(raw: &RawEnumeratedDevice) -> Vec<u32> {
    [
        (raw.supports_128_frames != 0).then_some(128),
        (raw.supports_256_frames != 0).then_some(256),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// A started same-device AUHAL capture/playback stream. It mutes immediately after `CoreAudio`
/// signals device loss or a relevant format/topology change; recovery is control-plane work.
pub struct ActiveDuplex<R: DuplexRenderer> {
    output: Option<NonNull<RawAudioOutput>>,
    renderer: Option<Box<R>>,
    route: AudioRouteConfig,
    last_telemetry: CallbackTelemetry,
}

impl<R: DuplexRenderer> ActiveDuplex<R> {
    /// Starts a selected same-device route after validating the fixed 48 kHz/128-or-256 contract.
    ///
    /// # Errors
    /// Returns [`CoreAudioError`] when the route is unsupported or the unit cannot start.
    pub fn start(
        route: AudioRouteConfig,
        renderer: R,
        allow_device_reconfiguration: bool,
    ) -> Result<Self, CoreAudioError> {
        let format = route.format;
        if !format.is_product_format() {
            return Err(CoreAudioError::UnsupportedRoute);
        }
        let input = route.input.as_ref().ok_or(CoreAudioError::InputRequired)?;
        let input_id = parse_core_audio_device_id(input)?;
        let output_id = parse_core_audio_device_id(&route.output)?;
        if input_id != output_id {
            return Err(CoreAudioError::SeparateDuplexDevicesUnsupported);
        }
        let mut renderer = Box::new(renderer);
        let mut output = std::ptr::null_mut();
        let result = unsafe {
            sp_audio_duplex_create(
                &raw mut output,
                input_id,
                format.max_frames_per_callback,
                u8::from(allow_device_reconfiguration),
                std::ptr::from_mut(renderer.as_mut()).cast::<c_void>(),
                duplex_render_trampoline::<R>,
            )
        };
        let output = NonNull::new(output);
        if result.status != 0 {
            if let Some(native) = output {
                if result.renderer_retired != 0 && result.native_releasable != 0 {
                    unsafe { sp_audio_output_release(native.as_ptr()) };
                } else {
                    mem::forget(renderer);
                }
            }
            return Err(core_audio_error(result));
        }
        let Some(output) = output else {
            return Err(CoreAudioError::NativeInvariant);
        };
        Ok(Self {
            output: Some(output),
            renderer: Some(renderer),
            route,
            last_telemetry: CallbackTelemetry::default(),
        })
    }

    /// The selected route.
    #[must_use]
    pub fn route(&self) -> &AudioRouteConfig {
        &self.route
    }

    /// Non-blocking device-loss/change notification. Once an event is observed, the native
    /// callback stays muted until this stream is stopped and a newly validated stream is started.
    pub fn poll_event(&mut self) -> Option<AudioEndpointEvent> {
        let output = self.output?;
        let event = unsafe { sp_audio_output_take_device_event(output.as_ptr()) };
        match event {
            1 => Some(AudioEndpointEvent::DeviceLost {
                device: self.route.output.clone(),
            }),
            2 => Some(AudioEndpointEvent::DeviceConfigurationChanged {
                device: self.route.output.clone(),
            }),
            _ => None,
        }
    }

    /// A coherent callback telemetry snapshot.
    #[must_use]
    pub fn telemetry(&self) -> CallbackTelemetry {
        self.output.map_or(self.last_telemetry, |o| unsafe {
            sp_audio_output_telemetry(o.as_ptr()).into()
        })
    }

    /// Retires the callback and native unit before dropping the renderer.
    ///
    /// # Errors
    /// Returns [`CoreAudioError`] when the native unit cannot be destroyed cleanly.
    pub fn stop(&mut self) -> Result<(), CoreAudioError> {
        let Some(output) = self.output else {
            return Ok(());
        };
        let result = unsafe { sp_audio_output_destroy(output.as_ptr()) };
        if result.status != 0 || result.renderer_retired == 0 || result.native_releasable == 0 {
            return Err(core_audio_error(result));
        }
        self.last_telemetry = unsafe { sp_audio_output_telemetry(output.as_ptr()).into() };
        self.output = None;
        unsafe { sp_audio_output_release(output.as_ptr()) };
        drop(self.renderer.take());
        Ok(())
    }
}

impl<R: DuplexRenderer> Drop for ActiveDuplex<R> {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn parse_core_audio_device_id(id: &AudioDeviceId) -> Result<u32, CoreAudioError> {
    id.as_str()
        .strip_prefix("coreaudio:")
        .and_then(|raw| raw.parse().ok())
        .ok_or(CoreAudioError::InvalidDeviceId)
}

unsafe extern "C" fn duplex_render_trampoline<R: DuplexRenderer>(
    renderer: *mut c_void,
    input: *const f32,
    output: *mut f32,
    frames: u32,
) -> u32 {
    let Some(frames) = PhaseOneFrames::from_u32(frames) else {
        return 0;
    };
    if renderer.is_null() || input.is_null() || output.is_null() {
        return 0;
    }
    let samples = frames.interleaved_sample_count();
    let input = unsafe { std::slice::from_raw_parts(input, samples) };
    let output = unsafe { std::slice::from_raw_parts_mut(output, samples) };
    let Ok(output) = InterleavedStereoF32::new(output, frames) else {
        return 0;
    };
    let renderer = unsafe { &mut *renderer.cast::<R>() };
    match renderer.render(DuplexStereoF32 { input, output }) {
        RenderDisposition::Rendered => RENDERED,
        RenderDisposition::Silence => 0,
    }
}

/// A strictly observed device and audio-unit format report.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceFormatReport {
    /// Exact nominal device sample rate in hertz.
    pub sample_rate_hz: f64,
    /// Total physical output channels from the device stream configuration.
    ///
    /// The AUHAL client stream remains fixed stereo and is explicitly mapped to physical
    /// channels 1–2, so a device with more than two physical outputs remains eligible.
    pub channel_count: u32,
    /// Current device buffer frame count.
    pub current_frames_per_slice: u32,
    /// Minimum supported device buffer frame count.
    pub supported_minimum_frames_per_slice: u32,
    /// Maximum supported configurable device buffer frame count.
    pub supported_maximum_frames_per_slice: u32,
    /// Largest callback the device can deliver at its current setting.
    ///
    /// This equals `current_frames_per_slice` for fixed-size devices. When the variable-size
    /// property exists, it is that property's documented largest delivered buffer size.
    pub maximum_callback_frames_per_slice: u32,
    /// Maximum frames per slice read back from the configured AUHAL unit.
    pub audio_unit_maximum_frames_per_slice: u32,
    /// Whether the device may deliver callbacks up to the reported callback maximum.
    pub uses_variable_buffer_frame_sizes: bool,
}

impl DeviceFormatReport {
    /// Returns whether this report supports the selected fixed stereo Phase 1 client format.
    #[must_use]
    pub const fn matches_phase_one(self, frames: PhaseOneFrames) -> bool {
        self.sample_rate_hz.to_bits() == 48_000.0_f64.to_bits()
            && self.channel_count >= 2
            && self.current_frames_per_slice == frames.as_u32()
            && self.supported_minimum_frames_per_slice <= frames.as_u32()
            && frames.as_u32() <= self.supported_maximum_frames_per_slice
            && self.maximum_callback_frames_per_slice == frames.as_u32()
            && self.audio_unit_maximum_frames_per_slice >= self.maximum_callback_frames_per_slice
            && !self.uses_variable_buffer_frame_sizes
    }
}

/// One `CoreAudio` lifecycle or device-format operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CoreAudioOperation {
    /// Resolving the current default output device.
    ResolveDefaultDevice,
    /// Reading the device format.
    ReadDeviceFormat,
    /// Enumerating hardware devices.
    EnumerateDevices,
    /// Requesting a 48 kHz device sample rate.
    RequestSampleRate,
    /// Requesting the selected device buffer frame count.
    RequestFrameCount,
    /// Strictly verifying the final device format.
    VerifyDeviceFormat,
    /// Creating the AUHAL audio unit.
    CreateUnit,
    /// Binding the AUHAL audio unit to the device.
    BindDevice,
    /// Disabling AUHAL input.
    DisableInput,
    /// Enabling AUHAL output.
    EnableOutput,
    /// Setting the interleaved stereo Float32 client format.
    ConfigureClientFormat,
    /// Setting the audio-unit maximum frames per slice.
    SetMaximumFrames,
    /// Attaching the output render callback.
    AttachCallback,
    /// Initializing the AUHAL audio unit.
    Initialize,
    /// Starting the audio unit.
    Start,
    /// Stopping the audio unit.
    Stop,
    /// Detaching the output callback.
    DetachCallback,
    /// Uninitializing the audio unit.
    Uninitialize,
    /// Disposing the audio unit.
    Dispose,
    /// Allocating native output state.
    AllocateState,
    /// Reading device frame-size capabilities.
    ReadFrameCapabilities,
    /// Reading back the configured audio-unit maximum.
    VerifyUnitMaximum,
    /// Reading supported nominal sample rates.
    ReadSampleRateCapabilities,
    /// Waiting for a requested sample rate to converge.
    WaitSampleRate,
    /// Waiting for a requested frame count to converge.
    WaitFrameCount,
    /// Reading the nominal device sample rate.
    ReadNominalSampleRate,
    /// Reading the output stream configuration and channel count.
    ReadStreamConfiguration,
    /// Reading the current device frame count.
    ReadCurrentFrameCount,
    /// Reading the supported device frame-count range.
    ReadFrameRange,
    /// Reading variable-buffer callback capability.
    ReadVariableFrameCapability,
    /// Waiting for all already-entered callbacks to leave.
    QuiesceCallbacks,
    /// Adding the nominal-rate property listener.
    AddSampleRateListener,
    /// Removing the nominal-rate property listener.
    RemoveSampleRateListener,
    /// Adding the frame-count property listener.
    AddFrameCountListener,
    /// Removing the frame-count property listener.
    RemoveFrameCountListener,
    /// An operation code the Rust boundary does not recognize.
    Unknown,
}

impl CoreAudioOperation {
    fn from_raw(operation: u32) -> Self {
        match operation {
            1 => Self::ResolveDefaultDevice,
            2 => Self::ReadDeviceFormat,
            41 => Self::EnumerateDevices,
            3 => Self::RequestSampleRate,
            4 => Self::RequestFrameCount,
            5 => Self::VerifyDeviceFormat,
            6 => Self::CreateUnit,
            7 => Self::BindDevice,
            8 => Self::DisableInput,
            9 => Self::EnableOutput,
            10 => Self::ConfigureClientFormat,
            11 => Self::SetMaximumFrames,
            12 => Self::AttachCallback,
            13 => Self::Initialize,
            14 => Self::Start,
            15 => Self::Stop,
            16 => Self::DetachCallback,
            17 => Self::Uninitialize,
            18 => Self::Dispose,
            19 => Self::AllocateState,
            20 => Self::ReadFrameCapabilities,
            21 => Self::VerifyUnitMaximum,
            24 => Self::ReadSampleRateCapabilities,
            25 => Self::WaitSampleRate,
            26 => Self::WaitFrameCount,
            27 => Self::ReadNominalSampleRate,
            28 => Self::ReadStreamConfiguration,
            29 => Self::ReadCurrentFrameCount,
            30 => Self::ReadFrameRange,
            31 => Self::ReadVariableFrameCapability,
            36 => Self::QuiesceCallbacks,
            37 => Self::AddSampleRateListener,
            38 => Self::RemoveSampleRateListener,
            39 => Self::AddFrameCountListener,
            40 => Self::RemoveFrameCountListener,
            _ => Self::Unknown,
        }
    }

    #[cfg(test)]
    const fn as_raw(self) -> u32 {
        match self {
            Self::ResolveDefaultDevice => 1,
            Self::ReadDeviceFormat => 2,
            Self::EnumerateDevices => 41,
            Self::RequestSampleRate => 3,
            Self::RequestFrameCount => 4,
            Self::VerifyDeviceFormat => 5,
            Self::CreateUnit => 6,
            Self::BindDevice => 7,
            Self::DisableInput => 8,
            Self::EnableOutput => 9,
            Self::ConfigureClientFormat => 10,
            Self::SetMaximumFrames => 11,
            Self::AttachCallback => 12,
            Self::Initialize => 13,
            Self::Start => 14,
            Self::Stop => 15,
            Self::DetachCallback => 16,
            Self::Uninitialize => 17,
            Self::Dispose => 18,
            Self::AllocateState => 19,
            Self::ReadFrameCapabilities => 20,
            Self::VerifyUnitMaximum => 21,
            Self::ReadSampleRateCapabilities => 24,
            Self::WaitSampleRate => 25,
            Self::WaitFrameCount => 26,
            Self::ReadNominalSampleRate => 27,
            Self::ReadStreamConfiguration => 28,
            Self::ReadCurrentFrameCount => 29,
            Self::ReadFrameRange => 30,
            Self::ReadVariableFrameCapability => 31,
            Self::QuiesceCallbacks => 36,
            Self::AddSampleRateListener => 37,
            Self::RemoveSampleRateListener => 38,
            Self::AddFrameCountListener => 39,
            Self::RemoveFrameCountListener => 40,
            Self::Unknown => 0,
        }
    }
}

/// One operation/status pair returned by `CoreAudio`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoreAudioFailure {
    /// The operation that failed.
    pub operation: CoreAudioOperation,
    /// The raw `OSStatus` value.
    pub status: i32,
}

/// A typed failure reported while configuring or controlling `CoreAudio`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CoreAudioError {
    /// The output device did not strictly satisfy the Phase 1 format requirements.
    UnsupportedDeviceFormat {
        /// The exact format observed after any acknowledged mutation.
        report: DeviceFormatReport,
    },
    /// One `CoreAudio` operation failed without a secondary recovery failure.
    System {
        /// The operation that failed.
        operation: CoreAudioOperation,
        /// The raw `OSStatus` value.
        status: i32,
    },
    /// Setup failed after a shared-device mutation; no rollback was attempted.
    ReconfigurationFailed {
        /// The setup operation that originally failed.
        initial: CoreAudioFailure,
        /// A later native lifecycle cleanup failure, if any.
        cleanup: Option<CoreAudioFailure>,
        /// Listener-removal failure, if any.
        listener_cleanup: Option<CoreAudioFailure>,
        /// The final full device-state read failure, if any.
        final_state: Option<CoreAudioFailure>,
        /// The final complete device state, when the read succeeded.
        final_report: Option<DeviceFormatReport>,
        /// Requested and notification-acknowledged shared-device mutations.
        mutation_flags: u32,
        /// Whether the C shim nevertheless proved callback retirement.
        callback_retired: bool,
    },
    /// Teardown failed before the C shim could prove callback retirement.
    CallbackRetirementUncertain {
        /// The teardown operation that failed.
        operation: CoreAudioOperation,
        /// The raw `OSStatus` value.
        status: i32,
    },
    /// The C shim reported success without returning valid native ownership.
    NativeInvariant,
    /// The route did not use the fixed product format.
    UnsupportedRoute,
    /// Duplex capture requires an explicit input device.
    InputRequired,
    /// A persisted ID was not issued by this `CoreAudio` backend.
    InvalidDeviceId,
    /// This direct AUHAL backend intentionally does not clock two separate devices.
    SeparateDuplexDevicesUnsupported,
}

impl fmt::Display for CoreAudioError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedDeviceFormat { report } => write!(
                formatter,
                "Phase 1 requires exact 48 kHz fixed-size stereo client callbacks; device reported \
                 {} Hz, {} physical output channels, current/supported/callback-max {}/{}/{}/{} frames, variable={}",
                report.sample_rate_hz,
                report.channel_count,
                report.current_frames_per_slice,
                report.supported_minimum_frames_per_slice,
                report.supported_maximum_frames_per_slice,
                report.maximum_callback_frames_per_slice,
                report.uses_variable_buffer_frame_sizes
            ),
            Self::System { operation, status } => {
                write!(
                    formatter,
                    "CoreAudio {operation:?} failed with OSStatus {status}"
                )
            }
            Self::ReconfigurationFailed {
                initial,
                cleanup,
                listener_cleanup,
                final_state,
                final_report,
                mutation_flags,
                callback_retired,
            } => write!(
                formatter,
                "CoreAudio {:?} failed with OSStatus {}; cleanup={cleanup:?}, listener_cleanup={listener_cleanup:?}, \
                 final_state={final_state:?}, final_report={final_report:?}, mutation_flags={mutation_flags:#x}, callback_retired={callback_retired}",
                initial.operation, initial.status
            ),
            Self::CallbackRetirementUncertain { operation, status } => write!(
                formatter,
                "CoreAudio {operation:?} failed with OSStatus {status}; callback retirement is \
                 not guaranteed"
            ),
            Self::NativeInvariant => {
                formatter.write_str("C shim returned invalid native ownership")
            }
            Self::UnsupportedRoute => {
                formatter.write_str("route must use fixed 48 kHz stereo with 128 or 256 frames")
            }
            Self::InputRequired => formatter.write_str("duplex route requires an input device"),
            Self::InvalidDeviceId => formatter.write_str("invalid CoreAudio device identifier"),
            Self::SeparateDuplexDevicesUnsupported => formatter
                .write_str("separate input/output devices are not supported by direct AUHAL"),
        }
    }
}

impl Error for CoreAudioError {}

/// A coherent fixed-counter callback telemetry snapshot.
///
/// The C render callback is the only sequence writer; control-thread code only reads snapshots.
/// A lock-free writer guard rejects a concurrent or reentrant callback before Rust is called, so
/// each accepted callback has exactly one serialized writer. Readers retry until they observe one
/// completed sequence, so every returned value satisfies
/// `callbacks == rendered + silenced + invalid_*`. The 128/256 histogram counts only valid
/// buffers and therefore sums to `rendered + silenced`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CallbackTelemetry {
    /// Number of serialized callbacks accepted by the telemetry writer guard.
    pub callbacks: u64,
    /// Number of callbacks whose renderer returned [`RenderDisposition::Rendered`].
    pub rendered: u64,
    /// Number of callbacks left silent by the renderer.
    pub silenced: u64,
    /// Number of callbacks rejected for an unexpected frame count.
    pub invalid_frames: u64,
    /// Number of callbacks rejected for a null or non-single buffer list.
    pub invalid_buffers: u64,
    /// Number of callbacks rejected for a non-stereo buffer.
    pub invalid_channels: u64,
    /// Number of callbacks rejected for a non-exact byte count.
    pub invalid_bytes: u64,
    /// Valid 128-frame callback count.
    pub frame_histogram_128: u64,
    /// Valid 256-frame callback count.
    pub frame_histogram_256: u64,
}

impl CallbackTelemetry {
    /// Returns whether the snapshot satisfies the C writer's classification invariants.
    #[must_use]
    pub const fn is_coherent(self) -> bool {
        self.callbacks
            == self
                .rendered
                .wrapping_add(self.silenced)
                .wrapping_add(self.invalid_frames)
                .wrapping_add(self.invalid_buffers)
                .wrapping_add(self.invalid_channels)
                .wrapping_add(self.invalid_bytes)
            && self
                .frame_histogram_128
                .wrapping_add(self.frame_histogram_256)
                == self.rendered.wrapping_add(self.silenced)
    }
}

impl From<RawCallbackTelemetry> for CallbackTelemetry {
    fn from(raw: RawCallbackTelemetry) -> Self {
        Self {
            callbacks: raw.callbacks,
            rendered: raw.rendered,
            silenced: raw.silenced,
            invalid_frames: raw.invalid_frames,
            invalid_buffers: raw.invalid_buffers,
            invalid_channels: raw.invalid_channels,
            invalid_bytes: raw.invalid_bytes,
            frame_histogram_128: raw.frame_histogram_128,
            frame_histogram_256: raw.frame_histogram_256,
        }
    }
}

/// A started AUHAL output whose renderer remains alive until C confirms callback retirement.
pub struct ActiveOutput<R: PhaseOneRenderer> {
    output: Option<NonNull<RawAudioOutput>>,
    renderer: Option<Box<R>>,
    device_format: DeviceFormatReport,
    last_telemetry: CallbackTelemetry,
    #[cfg(test)]
    test_hook: Option<Arc<TestHook>>,
}

impl<R: PhaseOneRenderer> ActiveOutput<R> {
    /// Resolves the default device, checks the requested Phase 1 format, and starts AUHAL.
    ///
    /// Call this only from a control thread. Explicit device changes wait for bounded
    /// property notifications and are intentionally not real-time operations.
    ///
    /// # Errors
    ///
    /// Returns a typed `CoreAudio` failure when the device cannot be verified or started. If
    /// cleanup cannot prove callback retirement, the native state, renderer, and any callback
    /// observer are deliberately leaked together rather than risking a use-after-free.
    pub fn start(config: PhaseOneConfig, renderer: R) -> Result<Self, CoreAudioError> {
        Self::start_inner(config, renderer, &StartBackend::Device)
    }

    /// Returns the exact format observed after setup.
    #[must_use]
    pub const fn device_format(&self) -> DeviceFormatReport {
        self.device_format
    }

    /// Returns a coherent callback telemetry snapshot.
    ///
    /// After successful [`Self::stop`], this returns the final snapshot captured after callback
    /// retirement and before the C telemetry storage was released.
    #[must_use]
    pub fn telemetry(&self) -> CallbackTelemetry {
        self.output.map_or(self.last_telemetry, |output| {
            // SAFETY: the output remains owned by this value. C only performs atomic reads.
            unsafe { sp_audio_output_telemetry(output.as_ptr()).into() }
        })
    }

    /// Stops, detaches, drains callbacks, uninitializes, and disposes the AUHAL output.
    ///
    /// Call this only from a control thread. Callback draining uses a bounded poll and never
    /// runs on the render thread.
    ///
    /// # Errors
    ///
    /// Returns the failing `CoreAudio` operation. The renderer is dropped only after stop,
    /// callback detach, in-flight callback drain, uninitialization, and disposal all succeed.
    /// Any uncertainty disables this value and deliberately leaks the complete native/Rust
    /// ownership cluster so the teardown error cannot be masked by user `Drop` code.
    pub fn stop(&mut self) -> Result<(), CoreAudioError> {
        let Some(output) = self.output else {
            return Ok(());
        };
        // SAFETY: this value exclusively owns the native state until release or abandonment.
        let result = unsafe { sp_audio_output_destroy(output.as_ptr()) };
        self.apply_destroy_result(result)
    }

    fn start_inner(
        config: PhaseOneConfig,
        renderer: R,
        backend: &StartBackend,
    ) -> Result<Self, CoreAudioError> {
        let mut renderer = Box::new(renderer);
        let mut output = std::ptr::null_mut();
        #[cfg(test)]
        let test_hook = backend.test_hook();
        let fake_config = backend.fake_config();
        #[cfg(test)]
        let (observer_context, lifecycle_observer, retirement_observer) = test_hook
            .as_ref()
            .map_or((std::ptr::null_mut(), None, None), |hook| {
                (
                    Arc::as_ptr(hook).cast_mut().cast::<c_void>(),
                    Some(test_lifecycle_observer as LifecycleObserver),
                    Some(test_retirement_observer as RetirementObserver),
                )
            });
        #[cfg(not(test))]
        let (observer_context, lifecycle_observer, retirement_observer) =
            (std::ptr::null_mut(), None, None);

        // SAFETY: the boxed renderer is stable. C returns explicit ownership/retirement flags.
        let result = unsafe {
            sp_audio_output_create(
                &raw mut output,
                config.frames.as_u32(),
                u8::from(config.allow_device_reconfiguration),
                std::ptr::from_mut(renderer.as_mut()).cast::<c_void>(),
                render_trampoline::<R>,
                fake_config,
                observer_context,
                lifecycle_observer,
                retirement_observer,
            )
        };
        let native = NonNull::new(output);
        if result.status != 0 {
            let error = core_audio_error(result);
            match (
                native,
                result.renderer_retired != 0,
                result.native_releasable != 0,
            ) {
                (Some(native), true, true) => {
                    // SAFETY: C reported the full quiescence sequence and native disposal.
                    unsafe { sp_audio_output_release(native.as_ptr()) };
                }
                (Some(_), _, _) | (None, false, _) => {
                    // Cleanup did not establish full quiescence. The native pointer, renderer,
                    // and observer clone are deliberately leaked as one ownership cluster.
                    mem::forget(renderer);
                    #[cfg(test)]
                    if let Some(hook) = test_hook {
                        mem::forget(hook);
                    }
                    return Err(error);
                }
                (None, true, _) => {}
            }
            return Err(error);
        }

        let Some(output) = native else {
            mem::forget(renderer);
            #[cfg(test)]
            if let Some(hook) = test_hook {
                mem::forget(hook);
            }
            return Err(CoreAudioError::NativeInvariant);
        };
        Ok(Self {
            output: Some(output),
            renderer: Some(renderer),
            device_format: result.report.into(),
            last_telemetry: CallbackTelemetry::default(),
            #[cfg(test)]
            test_hook,
        })
    }

    fn apply_destroy_result(&mut self, result: RawAudioResult) -> Result<(), CoreAudioError> {
        if result.status != 0 {
            let error = core_audio_error(result);
            // Teardown errors take precedence over user destructors. Full quiescence was not
            // established, so make all safe methods inactive and leak the complete cluster.
            self.leak_ownership_cluster();
            return Err(error);
        }
        if result.renderer_retired == 0 || result.native_releasable == 0 {
            self.leak_ownership_cluster();
            return Err(CoreAudioError::NativeInvariant);
        }

        let output = self.output.take().expect("releasable native output exists");
        // C completed stop, detach, callback drain, uninitialize, and disposal. Capture the
        // final counters before releasing their storage.
        self.last_telemetry = unsafe { sp_audio_output_telemetry(output.as_ptr()).into() };
        let renderer = self.renderer.take();
        #[cfg(test)]
        let test_hook = self.test_hook.take();
        // SAFETY: native ownership is cleared from `self`, and C reported it releasable.
        unsafe { sp_audio_output_release(output.as_ptr()) };
        #[cfg(test)]
        drop(test_hook);
        // User destructor runs only after no dangling native pointer remains in `self`.
        drop(renderer);
        Ok(())
    }

    fn leak_ownership_cluster(&mut self) {
        // `NonNull` has no destructor. Removing it disables every safe Rust entry point while
        // leaving the C wrapper and any AudioUnit reachable through it allocated together.
        let _ = self.output.take();
        if let Some(renderer) = self.renderer.take() {
            mem::forget(renderer);
        }
        #[cfg(test)]
        if let Some(hook) = self.test_hook.take() {
            mem::forget(hook);
        }
    }

    #[cfg(test)]
    fn start_test(
        config: PhaseOneConfig,
        renderer: R,
        fake: TestFakeConfig,
        hook: Arc<TestHook>,
    ) -> Result<Self, CoreAudioError> {
        Self::start_inner(
            config,
            renderer,
            &StartBackend::Fake {
                config: fake.raw,
                hook,
            },
        )
    }

    #[cfg(test)]
    fn invoke_test_callback(
        &mut self,
        frames: u32,
        buffer_count: u32,
        channels: u32,
        byte_size: u32,
        samples: &mut [f32],
    ) -> Result<(), TestInvokeError> {
        if buffer_count > 2 {
            return Err(TestInvokeError::TooManyBuffers);
        }
        let capacity = samples
            .len()
            .checked_mul(mem::size_of::<f32>())
            .and_then(|bytes| u32::try_from(bytes).ok())
            .ok_or(TestInvokeError::CapacityOverflow)?;
        if byte_size > capacity {
            return Err(TestInvokeError::ByteSizeExceedsSlice {
                byte_size,
                capacity,
            });
        }
        let output = self.output.ok_or(TestInvokeError::Inactive)?;
        // SAFETY: both Rust and C reject byte sizes beyond the supplied slice capacity.
        let status = unsafe {
            sp_audio_test_invoke(
                output.as_ptr(),
                frames,
                buffer_count,
                channels,
                byte_size,
                samples.as_mut_ptr(),
                capacity,
            )
        };
        match status {
            0 => Ok(()),
            -2 => Err(TestInvokeError::ByteSizeExceedsSlice {
                byte_size,
                capacity,
            }),
            _ => Err(TestInvokeError::NativeRejected),
        }
    }
}

impl<R: PhaseOneRenderer> Drop for ActiveOutput<R> {
    fn drop(&mut self) {
        let Some(output) = self.output else {
            return;
        };
        // SAFETY: this is the final Rust owner of the native state.
        let result = unsafe { sp_audio_output_destroy(output.as_ptr()) };
        if result.status != 0 || result.renderer_retired == 0 || result.native_releasable == 0 {
            // Never split a failed native teardown into separately freed Rust/C pieces.
            self.leak_ownership_cluster();
            return;
        }

        let output = self.output.take().expect("native output remains owned");
        self.last_telemetry = unsafe { sp_audio_output_telemetry(output.as_ptr()).into() };
        let renderer = self.renderer.take();
        #[cfg(test)]
        let test_hook = self.test_hook.take();
        unsafe { sp_audio_output_release(output.as_ptr()) };
        #[cfg(test)]
        drop(test_hook);
        drop(renderer);
    }
}

enum StartBackend {
    Device,
    #[cfg(test)]
    Fake {
        config: RawFakeConfig,
        hook: Arc<TestHook>,
    },
}

impl StartBackend {
    fn fake_config(&self) -> *const RawFakeConfig {
        match self {
            Self::Device => std::ptr::null(),
            #[cfg(test)]
            Self::Fake { config, .. } => std::ptr::from_ref(config),
        }
    }

    #[cfg(test)]
    fn test_hook(&self) -> Option<Arc<TestHook>> {
        match self {
            Self::Device => None,
            Self::Fake { hook, .. } => Some(Arc::clone(hook)),
        }
    }
}

fn failure(operation: u32, status: i32) -> Option<CoreAudioFailure> {
    (status != 0).then_some(CoreAudioFailure {
        operation: CoreAudioOperation::from_raw(operation),
        status,
    })
}

fn core_audio_error(result: RawAudioResult) -> CoreAudioError {
    let cleanup = failure(result.cleanup_operation, result.cleanup_status);
    let listener_cleanup = failure(
        result.listener_cleanup_operation,
        result.listener_cleanup_status,
    );
    let final_state = failure(result.final_state_operation, result.final_state_status);
    if cleanup.is_some()
        || listener_cleanup.is_some()
        || final_state.is_some()
        || result.mutation_flags != 0
    {
        return CoreAudioError::ReconfigurationFailed {
            initial: CoreAudioFailure {
                operation: CoreAudioOperation::from_raw(result.operation),
                status: result.status,
            },
            cleanup,
            listener_cleanup,
            final_state,
            final_report: (result.device_state_available != 0).then(|| result.report.into()),
            mutation_flags: result.mutation_flags,
            callback_retired: result.renderer_retired != 0,
        };
    }
    if result.status == STATUS_UNSUPPORTED_FORMAT {
        return CoreAudioError::UnsupportedDeviceFormat {
            report: result.report.into(),
        };
    }
    let operation = CoreAudioOperation::from_raw(result.operation);
    if result.renderer_retired == 0 {
        CoreAudioError::CallbackRetirementUncertain {
            operation,
            status: result.status,
        }
    } else {
        CoreAudioError::System {
            operation,
            status: result.status,
        }
    }
}

unsafe extern "C" fn render_trampoline<R: PhaseOneRenderer>(
    renderer: *mut c_void,
    samples: *mut f32,
    frames: u32,
) -> u32 {
    let Some(frames) = PhaseOneFrames::from_u32(frames) else {
        return 0;
    };
    if renderer.is_null() || samples.is_null() {
        return 0;
    }
    // SAFETY: C validates the exact mutable byte region before calling this trampoline.
    let samples =
        unsafe { std::slice::from_raw_parts_mut(samples, frames.interleaved_sample_count()) };
    let Ok(output) = InterleavedStereoF32::new(samples, frames) else {
        return 0;
    };
    // SAFETY: C has exclusive callback-time access and retires the callback before Rust drops R.
    let renderer = unsafe { &mut *renderer.cast::<R>() };
    match renderer.render(output) {
        RenderDisposition::Rendered => RENDERED,
        RenderDisposition::Silence => 0,
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawDeviceFormatReport {
    sample_rate_hz: f64,
    channel_count: u32,
    current_frames_per_slice: u32,
    supported_minimum_frames_per_slice: u32,
    supported_maximum_frames_per_slice: u32,
    maximum_callback_frames_per_slice: u32,
    audio_unit_maximum_frames_per_slice: u32,
    uses_variable_buffer_frame_sizes: u32,
}

impl From<RawDeviceFormatReport> for DeviceFormatReport {
    fn from(raw: RawDeviceFormatReport) -> Self {
        Self {
            sample_rate_hz: raw.sample_rate_hz,
            channel_count: raw.channel_count,
            current_frames_per_slice: raw.current_frames_per_slice,
            supported_minimum_frames_per_slice: raw.supported_minimum_frames_per_slice,
            supported_maximum_frames_per_slice: raw.supported_maximum_frames_per_slice,
            maximum_callback_frames_per_slice: raw.maximum_callback_frames_per_slice,
            audio_unit_maximum_frames_per_slice: raw.audio_unit_maximum_frames_per_slice,
            uses_variable_buffer_frame_sizes: raw.uses_variable_buffer_frame_sizes != 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawEnumeratedDevice {
    device_id: u32,
    input_channels: u32,
    output_channels: u32,
    is_default_input: u32,
    is_default_output: u32,
    supports_128_frames: u32,
    supports_256_frames: u32,
    name: [c_char; 256],
}

impl Default for RawEnumeratedDevice {
    fn default() -> Self {
        Self {
            device_id: 0,
            input_channels: 0,
            output_channels: 0,
            is_default_input: 0,
            is_default_output: 0,
            supports_128_frames: 0,
            supports_256_frames: 0,
            name: [0; 256],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawCallbackTelemetry {
    callbacks: u64,
    rendered: u64,
    silenced: u64,
    invalid_frames: u64,
    invalid_buffers: u64,
    invalid_channels: u64,
    invalid_bytes: u64,
    frame_histogram_128: u64,
    frame_histogram_256: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawAudioResult {
    status: i32,
    operation: u32,
    listener_cleanup_status: i32,
    listener_cleanup_operation: u32,
    cleanup_status: i32,
    cleanup_operation: u32,
    final_state_status: i32,
    final_state_operation: u32,
    mutation_flags: u32,
    device_state_available: u32,
    renderer_retired: u32,
    native_releasable: u32,
    report: RawDeviceFormatReport,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct RawFakeConfig {
    enabled: u32,
    fail_allocation: u32,
    sample_rate_hz: f64,
    channel_count: u32,
    current_frames_per_slice: u32,
    supported_minimum_frames_per_slice: u32,
    supported_maximum_frames_per_slice: u32,
    maximum_callback_frames_per_slice: u32,
    uses_variable_buffer_frame_sizes: u32,
    supports_48000: u32,
    failure_operation: u32,
    failure_status: i32,
    cleanup_failure_operation: u32,
    cleanup_failure_status: i32,
    callback_on_stop: u32,
    sample_rate_notification_polls: u32,
    frame_count_notification_polls: u32,
    sample_rate_coupled_frame_count: u32,
    final_state_failure_operation: u32,
    final_state_failure_status: i32,
    quiescence_convergence_polls: u32,
}

#[repr(C)]
struct RawAudioOutput {
    _private: [u8; 0],
}

type LifecycleObserver = unsafe extern "C" fn(*mut c_void, u32);
type RetirementObserver = unsafe extern "C" fn(*mut c_void);

unsafe extern "C" {
    fn sp_audio_output_create(
        output: *mut *mut RawAudioOutput,
        frames: u32,
        allow_reconfiguration: u8,
        renderer: *mut c_void,
        render: unsafe extern "C" fn(*mut c_void, *mut f32, u32) -> u32,
        fake_config: *const RawFakeConfig,
        observer_context: *mut c_void,
        lifecycle_observer: Option<LifecycleObserver>,
        retirement_observer: Option<RetirementObserver>,
    ) -> RawAudioResult;
    fn sp_audio_output_destroy(output: *mut RawAudioOutput) -> RawAudioResult;
    fn sp_audio_output_release(output: *mut RawAudioOutput);
    fn sp_audio_output_telemetry(output: *const RawAudioOutput) -> RawCallbackTelemetry;
    fn sp_audio_device_count() -> u32;
    fn sp_audio_device_at(index: u32, device: *mut RawEnumeratedDevice) -> i32;
    fn sp_audio_output_take_device_event(output: *mut RawAudioOutput) -> u32;
    fn sp_audio_duplex_create(
        output: *mut *mut RawAudioOutput,
        device: u32,
        frames: u32,
        allow_reconfiguration: u8,
        renderer: *mut c_void,
        render: unsafe extern "C" fn(*mut c_void, *const f32, *mut f32, u32) -> u32,
    ) -> RawAudioResult;

    #[cfg(test)]
    fn sp_audio_test_invoke(
        output: *mut RawAudioOutput,
        frames: u32,
        buffer_count: u32,
        channels: u32,
        byte_size: u32,
        samples: *mut f32,
        data_capacity_bytes: u32,
    ) -> i32;
}

#[cfg(test)]
struct TestHook {
    packed_lifecycle: AtomicU64,
    renderer_retired: AtomicBool,
}

#[cfg(test)]
impl TestHook {
    fn new() -> Self {
        Self {
            packed_lifecycle: AtomicU64::new(0),
            renderer_retired: AtomicBool::new(false),
        }
    }

    fn lifecycle(&self) -> Vec<u32> {
        const COUNT_SHIFT: u32 = 60;
        const EVENT_BITS: u32 = 4;
        let packed = self.packed_lifecycle.load(Ordering::Acquire);
        let count = usize::try_from(packed >> COUNT_SHIFT).expect("lifecycle count fits usize");
        (0..count)
            .map(|index| {
                let shift =
                    u32::try_from(index).expect("test lifecycle count fits u32") * EVENT_BITS;
                u32::try_from((packed >> shift) & 0xf).expect("four-bit event fits u32")
            })
            .collect()
    }
}

#[cfg(test)]
unsafe extern "C" fn test_lifecycle_observer(context: *mut c_void, event: u32) {
    const COUNT_SHIFT: u32 = 60;
    const EVENT_BITS: u32 = 4;
    const MAX_EVENTS: u64 = 15;
    if context.is_null() || event > 0xf {
        return;
    }
    // SAFETY: startup retains this Arc until C releases or abandons its observer pointer.
    let hook = unsafe { &*context.cast::<TestHook>() };
    let mut packed = hook.packed_lifecycle.load(Ordering::Relaxed);
    loop {
        let count = packed >> COUNT_SHIFT;
        if count >= MAX_EVENTS {
            return;
        }
        let shift = u32::try_from(count).expect("test lifecycle count fits u32") * EVENT_BITS;
        let events = packed & ((1_u64 << COUNT_SHIFT) - 1);
        let next = events | (u64::from(event) << shift) | ((count + 1) << COUNT_SHIFT);
        match hook.packed_lifecycle.compare_exchange_weak(
            packed,
            next,
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(current) => packed = current,
        }
    }
}

#[cfg(test)]
unsafe extern "C" fn test_retirement_observer(context: *mut c_void) {
    if context.is_null() {
        return;
    }
    // SAFETY: see `test_lifecycle_observer`.
    let hook = unsafe { &*context.cast::<TestHook>() };
    hook.renderer_retired.store(true, Ordering::Release);
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct TestFakeConfig {
    raw: RawFakeConfig,
}

#[cfg(test)]
impl TestFakeConfig {
    fn new(frames: PhaseOneFrames) -> Self {
        Self {
            raw: RawFakeConfig {
                enabled: 1,
                fail_allocation: 0,
                sample_rate_hz: 48_000.0,
                channel_count: 2,
                current_frames_per_slice: frames.as_u32(),
                supported_minimum_frames_per_slice: 64,
                supported_maximum_frames_per_slice: 1_024,
                maximum_callback_frames_per_slice: frames.as_u32(),
                uses_variable_buffer_frame_sizes: 0,
                supports_48000: 1,
                failure_operation: 0,
                failure_status: 0,
                cleanup_failure_operation: 0,
                cleanup_failure_status: 0,
                callback_on_stop: 0,
                sample_rate_notification_polls: 0,
                frame_count_notification_polls: 0,
                sample_rate_coupled_frame_count: 0,
                final_state_failure_operation: 0,
                final_state_failure_status: 0,
                quiescence_convergence_polls: 0,
            },
        }
    }

    fn fail(mut self, operation: CoreAudioOperation, status: i32) -> Self {
        self.raw.failure_operation = operation.as_raw();
        self.raw.failure_status = status;
        self
    }

    fn fail_cleanup(mut self, operation: CoreAudioOperation, status: i32) -> Self {
        self.raw.cleanup_failure_operation = operation.as_raw();
        self.raw.cleanup_failure_status = status;
        self
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TestInvokeError {
    Inactive,
    TooManyBuffers,
    CapacityOverflow,
    ByteSizeExceedsSlice { byte_size: u32, capacity: u32 },
    NativeRejected,
}

#[cfg(test)]
mod tests {
    use super::{
        ActiveOutput, CallbackTelemetry, CoreAudioError, CoreAudioFailure, CoreAudioOperation,
        InterleavedStereoF32, PhaseOneConfig, PhaseOneFrames, PhaseOneRenderer,
        RawEnumeratedDevice, RenderDisposition, TestFakeConfig, TestHook, TestInvokeError,
        supported_buffer_frames,
    };

    use std::{
        ffi::c_void,
        fs::File,
        io::Read,
        os::{fd::FromRawFd, unix::process::ExitStatusExt},
        panic::{AssertUnwindSafe, catch_unwind},
        process::Command,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        thread,
    };

    #[test]
    fn exposes_only_supported_product_buffer_sizes() {
        let raw = RawEnumeratedDevice {
            supports_128_frames: 0,
            supports_256_frames: 1,
            ..RawEnumeratedDevice::default()
        };
        assert_eq!(supported_buffer_frames(&raw), [256]);
    }

    unsafe extern "C" {
        fn pipe(file_descriptors: *mut i32) -> i32;
        #[link_name = "write"]
        fn c_write(file_descriptor: i32, buffer: *const c_void, count: usize) -> isize;
        #[link_name = "close"]
        fn c_close(file_descriptor: i32) -> i32;
    }

    struct TestRenderer {
        calls: Arc<AtomicUsize>,
        disposition: RenderDisposition,
    }

    impl PhaseOneRenderer for TestRenderer {
        fn render(&mut self, mut output: InterleavedStereoF32<'_>) -> RenderDisposition {
            self.calls.fetch_add(1, Ordering::Relaxed);
            for sample in output.samples_mut() {
                *sample = 0.25;
            }
            self.disposition
        }
    }

    struct DropCounterRenderer {
        drops: Arc<AtomicUsize>,
    }

    impl PhaseOneRenderer for DropCounterRenderer {
        fn render(&mut self, _output: InterleavedStereoF32<'_>) -> RenderDisposition {
            RenderDisposition::Silence
        }
    }

    impl Drop for DropCounterRenderer {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    struct BlockingRenderer {
        entered: Arc<AtomicBool>,
        release: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }

    impl PhaseOneRenderer for BlockingRenderer {
        fn render(&mut self, mut output: InterleavedStereoF32<'_>) -> RenderDisposition {
            self.calls.fetch_add(1, Ordering::Relaxed);
            self.entered.store(true, Ordering::Release);
            while !self.release.load(Ordering::Acquire) {
                std::hint::spin_loop();
            }
            output.samples_mut().fill(0.5);
            RenderDisposition::Rendered
        }
    }

    fn renderer() -> TestRenderer {
        TestRenderer {
            calls: Arc::new(AtomicUsize::new(0)),
            disposition: RenderDisposition::Rendered,
        }
    }

    fn start_test<R: PhaseOneRenderer>(
        frames: PhaseOneFrames,
        renderer: R,
        fake: TestFakeConfig,
    ) -> (ActiveOutput<R>, Arc<TestHook>) {
        let hook = Arc::new(TestHook::new());
        let output = ActiveOutput::start_test(
            PhaseOneConfig::new(frames),
            renderer,
            fake,
            Arc::clone(&hook),
        )
        .expect("fake backend starts");
        (output, hook)
    }

    #[test]
    fn fixed_stereo_client_accepts_a_multichannel_output_device() {
        let mut fake = TestFakeConfig::new(PhaseOneFrames::Frames128);
        fake.raw.channel_count = 64;
        let (mut output, _) = start_test(PhaseOneFrames::Frames128, renderer(), fake);
        assert_eq!(output.device_format().channel_count, 64);
        assert!(
            output
                .device_format()
                .matches_phase_one(PhaseOneFrames::Frames128)
        );

        let mut samples = vec![0.0; 256];
        output
            .invoke_test_callback(128, 1, 2, 1_024, &mut samples)
            .expect("fixed stereo client callback");
        assert_eq!(output.telemetry().invalid_channels, 0);
        output.stop().expect("fake output stops");
    }

    #[test]
    fn valid_callback_renders_and_reports_a_coherent_snapshot() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut output, _) = start_test(
            PhaseOneFrames::Frames128,
            TestRenderer {
                calls: Arc::clone(&calls),
                disposition: RenderDisposition::Rendered,
            },
            TestFakeConfig::new(PhaseOneFrames::Frames128),
        );
        let mut samples = vec![1.0; 256];
        output
            .invoke_test_callback(128, 1, 2, 1_024, &mut samples)
            .expect("valid callback");

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(samples, vec![0.25; 256]);
        let telemetry = output.telemetry();
        assert!(telemetry.is_coherent());
        assert_eq!(
            telemetry,
            CallbackTelemetry {
                callbacks: 1,
                rendered: 1,
                frame_histogram_128: 1,
                ..CallbackTelemetry::default()
            }
        );
    }

    #[test]
    fn live_telemetry_snapshots_remain_coherent_during_callbacks() {
        let (output, _) = start_test(
            PhaseOneFrames::Frames128,
            renderer(),
            TestFakeConfig::new(PhaseOneFrames::Frames128),
        );
        let output_pointer = output
            .output
            .expect("test output is active")
            .as_ptr()
            .expose_provenance();
        let done = Arc::new(AtomicBool::new(false));
        let callback_done = Arc::clone(&done);
        let callback_thread = thread::spawn(move || {
            let output_pointer =
                std::ptr::with_exposed_provenance_mut::<super::RawAudioOutput>(output_pointer);
            let mut samples = [0.0_f32; 256];
            for _ in 0..10_000 {
                // SAFETY: the owning `ActiveOutput` remains alive until this thread joins, and
                // this is the sole callback writer for its renderer and telemetry state.
                let status = unsafe {
                    super::sp_audio_test_invoke(
                        output_pointer,
                        128,
                        1,
                        2,
                        1_024,
                        samples.as_mut_ptr(),
                        1_024,
                    )
                };
                assert_eq!(status, 0);
            }
            callback_done.store(true, Ordering::Release);
        });

        while !done.load(Ordering::Acquire) {
            assert!(output.telemetry().is_coherent());
        }
        callback_thread.join().expect("callback thread joins");
        assert_eq!(output.telemetry().callbacks, 10_000);
        assert!(output.telemetry().is_coherent());
    }

    #[test]
    fn concurrent_callback_is_rejected_before_reentering_renderer() {
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let (output, _) = start_test(
            PhaseOneFrames::Frames128,
            BlockingRenderer {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                calls: Arc::clone(&calls),
            },
            TestFakeConfig::new(PhaseOneFrames::Frames128),
        );
        let pointer = output
            .output
            .expect("test output is active")
            .as_ptr()
            .expose_provenance();
        let first_callback = thread::spawn(move || {
            let output = std::ptr::with_exposed_provenance_mut::<super::RawAudioOutput>(pointer);
            let mut samples = [0.0_f32; 256];
            // SAFETY: the owning output outlives this joined thread.
            let status = unsafe {
                super::sp_audio_test_invoke(output, 128, 1, 2, 1_024, samples.as_mut_ptr(), 1_024)
            };
            assert_eq!(status, 0);
        });

        while !entered.load(Ordering::Acquire) {
            thread::yield_now();
        }
        let output_pointer = output.output.expect("test output remains active");
        let mut rejected_samples = [1.0_f32; 256];
        // SAFETY: C's lock-free writer guard rejects this overlapping entry before Rust.
        let status = unsafe {
            super::sp_audio_test_invoke(
                output_pointer.as_ptr(),
                128,
                1,
                2,
                1_024,
                rejected_samples.as_mut_ptr(),
                1_024,
            )
        };
        assert_eq!(status, 0);
        assert!(rejected_samples.iter().all(|sample| sample.to_bits() == 0));
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        release.store(true, Ordering::Release);
        first_callback.join().expect("first callback joins");
        assert_eq!(output.telemetry().callbacks, 1);
        assert!(output.telemetry().is_coherent());
    }

    #[test]
    fn silence_disposition_rezeros_renderer_output() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut output, _) = start_test(
            PhaseOneFrames::Frames128,
            TestRenderer {
                calls: Arc::clone(&calls),
                disposition: RenderDisposition::Silence,
            },
            TestFakeConfig::new(PhaseOneFrames::Frames128),
        );
        let mut samples = vec![1.0; 256];
        output
            .invoke_test_callback(128, 1, 2, 1_024, &mut samples)
            .expect("valid silent callback");

        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(samples.iter().all(|sample| *sample == 0.0));
        assert_eq!(output.telemetry().silenced, 1);
        assert!(output.telemetry().is_coherent());
    }

    #[test]
    fn lifecycle_orders_initialize_start_stop_detach_uninitialize_dispose() {
        let (mut output, hook) = start_test(
            PhaseOneFrames::Frames128,
            renderer(),
            TestFakeConfig::new(PhaseOneFrames::Frames128),
        );
        assert_eq!(hook.lifecycle(), [1, 2]);
        output.stop().expect("fake output stops");
        assert_eq!(hook.lifecycle(), [1, 2, 3, 4, 5, 6]);
        let mut samples = [0.0_f32; 256];
        assert_eq!(
            output.invoke_test_callback(128, 1, 2, 1_024, &mut samples),
            Err(TestInvokeError::Inactive)
        );
    }

    #[test]
    fn invalid_callback_shapes_are_silent_and_classified() {
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut output, _) = start_test(
            PhaseOneFrames::Frames128,
            TestRenderer {
                calls: Arc::clone(&calls),
                disposition: RenderDisposition::Rendered,
            },
            TestFakeConfig::new(PhaseOneFrames::Frames128),
        );
        let mut large = vec![1.0; 512];
        output
            .invoke_test_callback(256, 1, 2, 2_048, &mut large)
            .expect("invalid frame callback is safely classified");
        assert!(large.iter().all(|sample| *sample == 0.0));

        let mut samples = vec![1.0; 256];
        output
            .invoke_test_callback(128, 2, 2, 1_024, &mut samples)
            .expect("invalid buffer count");
        samples.fill(1.0);
        output
            .invoke_test_callback(128, 1, 1, 1_024, &mut samples)
            .expect("invalid channels");
        samples.fill(1.0);
        output
            .invoke_test_callback(128, 1, 2, 1_020, &mut samples)
            .expect("invalid byte count");

        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let telemetry = output.telemetry();
        assert!(telemetry.is_coherent());
        assert_eq!(telemetry.callbacks, 4);
        assert_eq!(telemetry.invalid_frames, 1);
        assert_eq!(telemetry.invalid_buffers, 1);
        assert_eq!(telemetry.invalid_channels, 1);
        assert_eq!(telemetry.invalid_bytes, 1);
    }

    #[test]
    fn safe_fake_helper_rejects_byte_size_beyond_slice_capacity() {
        let (mut output, _) = start_test(
            PhaseOneFrames::Frames128,
            renderer(),
            TestFakeConfig::new(PhaseOneFrames::Frames128),
        );
        let mut samples = vec![1.0; 16];
        assert_eq!(
            output.invoke_test_callback(128, 1, 2, 65, &mut samples),
            Err(TestInvokeError::ByteSizeExceedsSlice {
                byte_size: 65,
                capacity: 64,
            })
        );
        assert!(
            samples
                .iter()
                .all(|sample| sample.to_bits() == 1.0_f32.to_bits())
        );
        assert_eq!(output.telemetry(), CallbackTelemetry::default());
    }

    #[test]
    fn fake_reports_each_production_device_property_read_operation() {
        let operations = [
            CoreAudioOperation::ReadNominalSampleRate,
            CoreAudioOperation::ReadStreamConfiguration,
            CoreAudioOperation::ReadCurrentFrameCount,
            CoreAudioOperation::ReadFrameRange,
            CoreAudioOperation::ReadVariableFrameCapability,
        ];
        for (index, operation) in operations.into_iter().enumerate() {
            let status = -100 - i32::try_from(index).expect("small index");
            let hook = Arc::new(TestHook::new());
            let result = ActiveOutput::start_test(
                PhaseOneConfig::new(PhaseOneFrames::Frames128),
                renderer(),
                TestFakeConfig::new(PhaseOneFrames::Frames128).fail(operation, status),
                hook,
            );
            assert!(matches!(
                result,
                Err(CoreAudioError::System {
                    operation: actual,
                    status: actual_status,
                }) if actual == operation && actual_status == status
            ));
        }
    }

    #[test]
    fn variable_frame_devices_are_rejected_before_reconfiguration() {
        let hook = Arc::new(TestHook::new());
        let mut fake = TestFakeConfig::new(PhaseOneFrames::Frames128);
        fake.raw.uses_variable_buffer_frame_sizes = 1;
        fake.raw.current_frames_per_slice = 256;
        fake.raw.maximum_callback_frames_per_slice = 256;
        fake.raw.maximum_callback_frames_per_slice = 260;
        let result = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128).allow_device_reconfiguration(),
            renderer(),
            fake,
            Arc::clone(&hook),
        );
        assert!(matches!(
            result,
            Err(CoreAudioError::UnsupportedDeviceFormat { report })
                if report.uses_variable_buffer_frame_sizes
                    && report.supported_maximum_frames_per_slice == 1_024
                    && report.maximum_callback_frames_per_slice == 260
        ));
        assert!(hook.lifecycle().is_empty());
    }

    #[test]
    fn report_distinguishes_current_device_maximum_and_audio_unit_maximum() {
        let (output, _) = start_test(
            PhaseOneFrames::Frames128,
            renderer(),
            TestFakeConfig::new(PhaseOneFrames::Frames128),
        );
        let report = output.device_format();
        assert_eq!(report.current_frames_per_slice, 128);
        assert_eq!(report.supported_maximum_frames_per_slice, 1_024);
        assert_eq!(report.maximum_callback_frames_per_slice, 128);
        assert_eq!(report.audio_unit_maximum_frames_per_slice, 128);
        assert!(report.matches_phase_one(PhaseOneFrames::Frames128));
    }

    #[test]
    fn fake_models_delayed_property_convergence_and_operation_timeouts() {
        let mut delayed = TestFakeConfig::new(PhaseOneFrames::Frames128);
        delayed.raw.sample_rate_hz = 44_100.0;
        delayed.raw.current_frames_per_slice = 256;
        delayed.raw.maximum_callback_frames_per_slice = 256;
        delayed.raw.sample_rate_notification_polls = 3;
        delayed.raw.frame_count_notification_polls = 4;
        let hook = Arc::new(TestHook::new());
        let mut output = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128).allow_device_reconfiguration(),
            renderer(),
            delayed,
            Arc::clone(&hook),
        )
        .expect("delayed properties converge within the bound");
        assert!(
            output
                .device_format()
                .matches_phase_one(PhaseOneFrames::Frames128)
        );
        assert_eq!(hook.lifecycle(), [9, 7, 10, 12, 8, 13, 11, 14, 1, 2]);
        output.stop().expect("delayed setup output stops");

        let mut sample_timeout = TestFakeConfig::new(PhaseOneFrames::Frames128);
        sample_timeout.raw.sample_rate_hz = 44_100.0;
        sample_timeout.raw.sample_rate_notification_polls = u32::MAX;
        let hook = Arc::new(TestHook::new());
        let result = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128).allow_device_reconfiguration(),
            renderer(),
            sample_timeout,
            Arc::clone(&hook),
        );
        assert!(matches!(
            result,
            Err(CoreAudioError::ReconfigurationFailed {
                initial: CoreAudioFailure {
                    operation: CoreAudioOperation::WaitSampleRate,
                    status: -70_002
                },
                mutation_flags: 1,
                ..
            })
        ));
        assert_eq!(hook.lifecycle(), [9, 7, 11]);

        let mut frame_timeout = TestFakeConfig::new(PhaseOneFrames::Frames128);
        frame_timeout.raw.current_frames_per_slice = 256;
        frame_timeout.raw.maximum_callback_frames_per_slice = 256;
        frame_timeout.raw.frame_count_notification_polls = u32::MAX;
        let result = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128).allow_device_reconfiguration(),
            renderer(),
            frame_timeout,
            Arc::new(TestHook::new()),
        );
        assert!(matches!(
            result,
            Err(CoreAudioError::ReconfigurationFailed {
                initial: CoreAudioFailure {
                    operation: CoreAudioOperation::WaitFrameCount,
                    status: -70_002
                },
                mutation_flags: 4,
                ..
            })
        ));
    }

    #[test]
    fn failed_setup_retains_acknowledged_mutations_without_rollback() {
        let hook = Arc::new(TestHook::new());
        let mut fake =
            TestFakeConfig::new(PhaseOneFrames::Frames128).fail(CoreAudioOperation::Start, -4_242);
        fake.raw.sample_rate_hz = 44_100.0;
        fake.raw.current_frames_per_slice = 256;
        fake.raw.maximum_callback_frames_per_slice = 256;
        let result = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128).allow_device_reconfiguration(),
            renderer(),
            fake,
            Arc::clone(&hook),
        );
        assert!(matches!(
            result,
            Err(CoreAudioError::ReconfigurationFailed {
                initial: CoreAudioFailure {
                    operation: CoreAudioOperation::Start,
                    status: -4_242
                },
                mutation_flags: 0b1111,
                ..
            })
        ));
        assert_eq!(hook.lifecycle(), [9, 7, 10, 12, 8, 13, 11, 14, 1, 4, 5, 6]);
    }

    #[test]
    fn sample_rate_notification_can_couple_a_fresh_frame_size_request() {
        let hook = Arc::new(TestHook::new());
        let mut fake = TestFakeConfig::new(PhaseOneFrames::Frames128);
        fake.raw.sample_rate_hz = 44_100.0;
        // Already at the target frame count until the rate notification couples a new size.
        fake.raw.current_frames_per_slice = 128;
        fake.raw.maximum_callback_frames_per_slice = 128;
        fake.raw.sample_rate_coupled_frame_count = 256;
        let mut output = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128).allow_device_reconfiguration(),
            renderer(),
            fake,
            Arc::clone(&hook),
        )
        .expect("coupled frame size is reconfigured after the rate notification");
        assert!(
            output
                .device_format()
                .matches_phase_one(PhaseOneFrames::Frames128)
        );
        assert_eq!(hook.lifecycle(), [9, 7, 10, 12, 8, 13, 11, 14, 1, 2]);
        output.stop().expect("coupled reconfiguration output stops");
    }

    #[test]
    fn final_device_state_read_failure_is_preserved_on_setup_error() {
        let mut fake =
            TestFakeConfig::new(PhaseOneFrames::Frames128).fail(CoreAudioOperation::Start, -91);
        fake.raw.sample_rate_hz = 44_100.0;
        fake.raw.final_state_failure_operation = CoreAudioOperation::ReadDeviceFormat.as_raw();
        fake.raw.final_state_failure_status = -92;
        let result = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128).allow_device_reconfiguration(),
            renderer(),
            fake,
            Arc::new(TestHook::new()),
        );
        assert!(matches!(
            result,
            Err(CoreAudioError::ReconfigurationFailed {
                initial: CoreAudioFailure {
                    operation: CoreAudioOperation::Start,
                    status: -91,
                },
                final_state: Some(CoreAudioFailure {
                    operation: CoreAudioOperation::ReadDeviceFormat,
                    status: -92,
                }),
                final_report: None,
                mutation_flags: 0b0011,
                ..
            })
        ));
    }

    #[test]
    fn failed_start_cleanup_reports_live_native_ownership_and_leaks_renderer() {
        struct CountDrop(Arc<AtomicUsize>);
        impl PhaseOneRenderer for CountDrop {
            fn render(&mut self, _output: InterleavedStereoF32<'_>) -> RenderDisposition {
                RenderDisposition::Silence
            }
        }
        impl Drop for CountDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let hook = Arc::new(TestHook::new());
        let fake = TestFakeConfig::new(PhaseOneFrames::Frames128)
            .fail(CoreAudioOperation::Start, -51)
            .fail_cleanup(CoreAudioOperation::DetachCallback, -52);
        let result = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128),
            CountDrop(Arc::clone(&drops)),
            fake,
            Arc::clone(&hook),
        );
        let error = result.err();
        assert!(
            matches!(
                error,
                Some(CoreAudioError::ReconfigurationFailed {
                    initial: CoreAudioFailure {
                        operation: CoreAudioOperation::Start,
                        status: -51,
                    },
                    cleanup: Some(CoreAudioFailure {
                        operation: CoreAudioOperation::DetachCallback,
                        status: -52,
                    }),
                    callback_retired: false,
                    ..
                })
            ),
            "{error:?}"
        );
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert_eq!(hook.lifecycle(), [1]);
        assert!(!hook.renderer_retired.load(Ordering::Acquire));
    }

    #[test]
    fn uninitialize_and_dispose_failures_leak_whole_cluster_without_retry() {
        let cases: &[(CoreAudioOperation, i32, &[u32])] = &[
            (CoreAudioOperation::Uninitialize, -81, &[1, 2, 3, 4]),
            (CoreAudioOperation::Dispose, -82, &[1, 2, 3, 4, 5]),
        ];
        for &(operation, status, expected_lifecycle) in cases {
            let drops = Arc::new(AtomicUsize::new(0));
            let hook = Arc::new(TestHook::new());
            let mut output = ActiveOutput::start_test(
                PhaseOneConfig::new(PhaseOneFrames::Frames128),
                DropCounterRenderer {
                    drops: Arc::clone(&drops),
                },
                TestFakeConfig::new(PhaseOneFrames::Frames128).fail_cleanup(operation, status),
                Arc::clone(&hook),
            )
            .expect("fake output starts");

            assert!(matches!(
                output.stop(),
                Err(CoreAudioError::CallbackRetirementUncertain {
                    operation: actual,
                    status: actual_status,
                }) if actual == operation && actual_status == status
            ));
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            assert!(!hook.renderer_retired.load(Ordering::Acquire));
            assert_eq!(hook.lifecycle(), expected_lifecycle);

            let mut samples = [1.0_f32; 256];
            assert_eq!(
                output.invoke_test_callback(128, 1, 2, 1_024, &mut samples),
                Err(TestInvokeError::Inactive)
            );
            // The first error deliberately abandoned retry ownership in favor of a complete
            // cluster leak, so neither a second stop nor Drop re-enters native teardown.
            assert!(output.stop().is_ok());
            assert_eq!(hook.lifecycle(), expected_lifecycle);
            drop(output);
            assert_eq!(drops.load(Ordering::Relaxed), 0);
        }
    }

    #[test]
    fn drop_teardown_failure_keeps_native_wrapper_unit_and_renderer_together() {
        let drops = Arc::new(AtomicUsize::new(0));
        let hook = Arc::new(TestHook::new());
        let output = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128),
            DropCounterRenderer {
                drops: Arc::clone(&drops),
            },
            TestFakeConfig::new(PhaseOneFrames::Frames128)
                .fail_cleanup(CoreAudioOperation::Dispose, -84),
            Arc::clone(&hook),
        )
        .expect("fake output starts");

        drop(output);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert!(!hook.renderer_retired.load(Ordering::Acquire));
        assert_eq!(hook.lifecycle(), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn callback_quiescence_timeout_leaks_cluster_and_disables_test_invoke() {
        let drops = Arc::new(AtomicUsize::new(0));
        let hook = Arc::new(TestHook::new());
        let mut fake = TestFakeConfig::new(PhaseOneFrames::Frames128);
        fake.raw.quiescence_convergence_polls = u32::MAX;
        let mut output = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128),
            DropCounterRenderer {
                drops: Arc::clone(&drops),
            },
            fake,
            Arc::clone(&hook),
        )
        .expect("fake output starts");

        assert!(matches!(
            output.stop(),
            Err(CoreAudioError::CallbackRetirementUncertain {
                operation: CoreAudioOperation::QuiesceCallbacks,
                status: -70_005,
            })
        ));
        assert_eq!(hook.lifecycle(), [1, 2, 3, 4]);
        assert!(!hook.renderer_retired.load(Ordering::Acquire));
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        let mut samples = [0.0_f32; 256];
        assert_eq!(
            output.invoke_test_callback(128, 1, 2, 1_024, &mut samples),
            Err(TestInvokeError::Inactive)
        );
        drop(output);
        assert_eq!(drops.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn teardown_detects_an_actually_entered_callback() {
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let hook = Arc::new(TestHook::new());
        let mut output = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128),
            BlockingRenderer {
                entered: Arc::clone(&entered),
                release: Arc::clone(&release),
                calls: Arc::clone(&calls),
            },
            TestFakeConfig::new(PhaseOneFrames::Frames128),
            Arc::clone(&hook),
        )
        .expect("fake output starts");
        let pointer = output
            .output
            .expect("test output is active")
            .as_ptr()
            .expose_provenance();
        let callback = thread::spawn(move || {
            let output = std::ptr::with_exposed_provenance_mut::<super::RawAudioOutput>(pointer);
            let mut samples = [0.0_f32; 256];
            // SAFETY: teardown leaks the complete cluster on timeout, so this entered callback
            // retains valid native and renderer storage until it is released below.
            let status = unsafe {
                super::sp_audio_test_invoke(output, 128, 1, 2, 1_024, samples.as_mut_ptr(), 1_024)
            };
            assert_eq!(status, 0);
        });
        while !entered.load(Ordering::Acquire) {
            thread::yield_now();
        }

        assert!(matches!(
            output.stop(),
            Err(CoreAudioError::CallbackRetirementUncertain {
                operation: CoreAudioOperation::QuiesceCallbacks,
                status: -70_005,
            })
        ));
        assert_eq!(hook.lifecycle(), [1, 2, 3, 4]);
        release.store(true, Ordering::Release);
        callback
            .join()
            .expect("entered callback leaves leaked cluster");
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn teardown_error_precedes_panicking_renderer_drop() {
        struct PanicDrop;
        impl PhaseOneRenderer for PanicDrop {
            fn render(&mut self, _output: InterleavedStereoF32<'_>) -> RenderDisposition {
                RenderDisposition::Silence
            }
        }
        impl Drop for PanicDrop {
            fn drop(&mut self) {
                panic!("renderer drop must not run after teardown failure");
            }
        }

        let mut output = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128),
            PanicDrop,
            TestFakeConfig::new(PhaseOneFrames::Frames128)
                .fail_cleanup(CoreAudioOperation::Uninitialize, -83),
            Arc::new(TestHook::new()),
        )
        .expect("fake output starts");
        let result = catch_unwind(AssertUnwindSafe(|| output.stop()));
        assert!(matches!(
            result,
            Ok(Err(CoreAudioError::CallbackRetirementUncertain {
                operation: CoreAudioOperation::Uninitialize,
                status: -83,
            }))
        ));
        // Output ownership was deliberately leaked, so dropping the now-inactive Rust shell
        // cannot invoke the panicking renderer destructor or native teardown again.
        drop(output);
    }

    #[test]
    fn final_telemetry_is_captured_after_callback_retirement() {
        let mut fake = TestFakeConfig::new(PhaseOneFrames::Frames128);
        fake.raw.callback_on_stop = 1;
        let calls = Arc::new(AtomicUsize::new(0));
        let (mut output, hook) = start_test(
            PhaseOneFrames::Frames128,
            TestRenderer {
                calls: Arc::clone(&calls),
                disposition: RenderDisposition::Rendered,
            },
            fake,
        );
        assert_eq!(output.telemetry().callbacks, 0);
        output.stop().expect("stop succeeds");
        assert!(hook.renderer_retired.load(Ordering::Acquire));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(output.telemetry().callbacks, 1);
        assert!(output.telemetry().is_coherent());
    }

    #[test]
    fn renderer_drops_only_after_callback_retirement() {
        struct ObserveDrop {
            hook: Arc<TestHook>,
            drops: Arc<AtomicUsize>,
        }
        impl PhaseOneRenderer for ObserveDrop {
            fn render(&mut self, _output: InterleavedStereoF32<'_>) -> RenderDisposition {
                RenderDisposition::Silence
            }
        }
        impl Drop for ObserveDrop {
            fn drop(&mut self) {
                assert!(self.hook.renderer_retired.load(Ordering::Acquire));
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }

        let hook = Arc::new(TestHook::new());
        let drops = Arc::new(AtomicUsize::new(0));
        let mut output = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128),
            ObserveDrop {
                hook: Arc::clone(&hook),
                drops: Arc::clone(&drops),
            },
            TestFakeConfig::new(PhaseOneFrames::Frames128),
            Arc::clone(&hook),
        )
        .expect("fake backend starts");
        output.stop().expect("fake output stops");
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn panicking_renderer_drop_cannot_leave_a_dangling_native_owner() {
        struct PanicDrop;
        impl PhaseOneRenderer for PanicDrop {
            fn render(&mut self, _output: InterleavedStereoF32<'_>) -> RenderDisposition {
                RenderDisposition::Silence
            }
        }
        impl Drop for PanicDrop {
            fn drop(&mut self) {
                panic!("intentional renderer drop panic");
            }
        }

        let (mut output, _) = start_test(
            PhaseOneFrames::Frames128,
            PanicDrop,
            TestFakeConfig::new(PhaseOneFrames::Frames128),
        );
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = output.stop();
        }));
        assert!(panic.is_err());
        assert!(output.stop().is_ok());
    }

    #[test]
    fn allocation_failure_returns_renderer_ownership_without_leaking() {
        struct CountDrop(Arc<AtomicUsize>);
        impl PhaseOneRenderer for CountDrop {
            fn render(&mut self, _output: InterleavedStereoF32<'_>) -> RenderDisposition {
                RenderDisposition::Silence
            }
        }
        impl Drop for CountDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let hook = Arc::new(TestHook::new());
        let mut fake = TestFakeConfig::new(PhaseOneFrames::Frames128);
        fake.raw.fail_allocation = 1;
        let result = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128),
            CountDrop(Arc::clone(&drops)),
            fake,
            hook,
        );
        assert!(matches!(
            result,
            Err(CoreAudioError::System {
                operation: CoreAudioOperation::AllocateState,
                ..
            })
        ));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn fractional_sample_rate_is_reported_exactly_and_reconfigured_when_allowed() {
        let mut fake = TestFakeConfig::new(PhaseOneFrames::Frames128);
        fake.raw.sample_rate_hz = 47_999.5;
        let hook = Arc::new(TestHook::new());
        let result = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128),
            renderer(),
            fake,
            hook,
        );
        assert!(matches!(
            result,
            Err(CoreAudioError::UnsupportedDeviceFormat { report })
                if report.sample_rate_hz.to_bits() == 47_999.5_f64.to_bits()
        ));

        let hook = Arc::new(TestHook::new());
        let output = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128).allow_device_reconfiguration(),
            renderer(),
            fake,
            Arc::clone(&hook),
        )
        .expect("fractional rate is explicitly reconfigured");
        assert_eq!(
            output.device_format().sample_rate_hz.to_bits(),
            48_000.0_f64.to_bits()
        );
        assert_eq!(hook.lifecycle().first(), Some(&9));
    }

    #[test]
    fn both_reconfiguration_request_failures_are_injectable() {
        let mut fake = TestFakeConfig::new(PhaseOneFrames::Frames128)
            .fail(CoreAudioOperation::RequestSampleRate, -61);
        fake.raw.sample_rate_hz = 44_100.0;
        fake.raw.current_frames_per_slice = 256;
        fake.raw.maximum_callback_frames_per_slice = 256;
        let hook = Arc::new(TestHook::new());
        let result = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128).allow_device_reconfiguration(),
            renderer(),
            fake,
            Arc::clone(&hook),
        );
        assert!(matches!(
            result,
            Err(CoreAudioError::System {
                operation: CoreAudioOperation::RequestSampleRate,
                status: -61,
            })
        ));
        assert_eq!(hook.lifecycle(), [9, 11]);

        let mut fake = TestFakeConfig::new(PhaseOneFrames::Frames128)
            .fail(CoreAudioOperation::RequestFrameCount, -62);
        fake.raw.sample_rate_hz = 44_100.0;
        fake.raw.current_frames_per_slice = 256;
        fake.raw.maximum_callback_frames_per_slice = 256;
        let hook = Arc::new(TestHook::new());
        let result = ActiveOutput::start_test(
            PhaseOneConfig::new(PhaseOneFrames::Frames128).allow_device_reconfiguration(),
            renderer(),
            fake,
            Arc::clone(&hook),
        );
        assert!(matches!(
            result,
            Err(CoreAudioError::ReconfigurationFailed {
                initial: CoreAudioFailure {
                    operation: CoreAudioOperation::RequestFrameCount,
                    status: -62
                },
                mutation_flags: 3,
                ..
            })
        ));
        assert_eq!(hook.lifecycle(), [9, 7, 10, 12, 11, 14]);
    }

    struct PanicRenderer {
        marker_file_descriptor: i32,
    }

    impl PhaseOneRenderer for PanicRenderer {
        fn render(&mut self, _output: InterleavedStereoF32<'_>) -> RenderDisposition {
            const MARKER: [u8; 1] = [0xa5];
            // SAFETY: the parent passes an inherited writable pipe descriptor. The one-byte
            // syscall proves the callback entered before the deliberate non-unwinding panic.
            let written = unsafe {
                c_write(
                    self.marker_file_descriptor,
                    MARKER.as_ptr().cast::<c_void>(),
                    MARKER.len(),
                )
            };
            if written != 1 {
                std::process::exit(78);
            }
            panic!("intentional real-time renderer panic");
        }
    }

    #[test]
    fn renderer_panic_aborts_instead_of_attempting_recovery() {
        const CHILD: &str = "SP_AUDIO_RENDERER_PANIC_CHILD";
        const MARKER_FD: &str = "SP_AUDIO_RENDERER_PANIC_MARKER_FD";
        if std::env::var_os(CHILD).is_some() {
            let marker_file_descriptor = std::env::var(MARKER_FD)
                .expect("marker descriptor is supplied")
                .parse::<i32>()
                .expect("marker descriptor is numeric");
            let (mut output, _) = start_test(
                PhaseOneFrames::Frames128,
                PanicRenderer {
                    marker_file_descriptor,
                },
                TestFakeConfig::new(PhaseOneFrames::Frames128),
            );
            let mut samples = vec![0.0; 256];
            let _ = output.invoke_test_callback(128, 1, 2, 1_024, &mut samples);
            std::process::exit(77);
        }

        let mut pipe_descriptors = [-1_i32; 2];
        // SAFETY: `pipe` initializes both descriptors on a zero return.
        assert_eq!(unsafe { pipe(pipe_descriptors.as_mut_ptr()) }, 0);
        let mut child = Command::new(std::env::current_exe().expect("test executable"))
            .arg("--exact")
            .arg("tests::renderer_panic_aborts_instead_of_attempting_recovery")
            .arg("--nocapture")
            .env(CHILD, "1")
            .env(MARKER_FD, pipe_descriptors[1].to_string())
            .spawn()
            .expect("spawn panic child");
        // SAFETY: the parent no longer writes; the child inherited its own descriptor copy.
        assert_eq!(unsafe { c_close(pipe_descriptors[1]) }, 0);
        let status = child.wait().expect("wait for panic child");
        // SAFETY: the read descriptor is uniquely transferred into `File` exactly once.
        let mut marker_file = unsafe { File::from_raw_fd(pipe_descriptors[0]) };
        let mut marker = [0_u8; 1];
        marker_file
            .read_exact(&mut marker)
            .expect("renderer callback wrote entry marker");

        assert_eq!(marker, [0xa5]);
        assert_eq!(status.signal(), Some(6), "child status was {status:?}");
    }
}
