#include <AudioToolbox/AudioToolbox.h>
#include <CoreAudio/CoreAudio.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define SP_AUDIO_STATUS_UNSUPPORTED_FORMAT ((int32_t)-70000)
#define SP_AUDIO_STATUS_COMPONENT_NOT_FOUND ((int32_t)-70001)
#define SP_AUDIO_STATUS_PROPERTY_TIMEOUT ((int32_t)-70002)
#define SP_AUDIO_STATUS_CALLBACK_QUIESCE_TIMEOUT ((int32_t)-70005)
#define SP_RENDERED ((uint32_t)1)
#define SP_PROPERTY_POLL_LIMIT ((uint32_t)100)
#define SP_QUIESCE_POLL_LIMIT ((uint32_t)100)
#define SP_FAKE_NEVER_NOTIFIES UINT32_MAX
#define SP_MUTATION_SAMPLE_RATE_REQUESTED ((uint32_t)1 << 0)
#define SP_MUTATION_SAMPLE_RATE_ACKNOWLEDGED ((uint32_t)1 << 1)
#define SP_MUTATION_FRAME_COUNT_REQUESTED ((uint32_t)1 << 2)
#define SP_MUTATION_FRAME_COUNT_ACKNOWLEDGED ((uint32_t)1 << 3)

_Static_assert(ATOMIC_LONG_LOCK_FREE == 2 &&
                   ATOMIC_LLONG_LOCK_FREE == 2,
               "callback telemetry requires lock-free 64-bit atomics");
_Static_assert(ATOMIC_INT_LOCK_FREE == 2,
               "callback telemetry requires a lock-free writer guard");

enum SpAudioOperation {
    SP_AUDIO_OPERATION_NONE = 0,
    SP_AUDIO_OPERATION_RESOLVE_DEFAULT_DEVICE = 1,
    SP_AUDIO_OPERATION_READ_DEVICE_FORMAT = 2,
    SP_AUDIO_OPERATION_REQUEST_SAMPLE_RATE = 3,
    SP_AUDIO_OPERATION_REQUEST_FRAME_COUNT = 4,
    SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT = 5,
    SP_AUDIO_OPERATION_CREATE_UNIT = 6,
    SP_AUDIO_OPERATION_BIND_DEVICE = 7,
    SP_AUDIO_OPERATION_DISABLE_INPUT = 8,
    SP_AUDIO_OPERATION_ENABLE_OUTPUT = 9,
    SP_AUDIO_OPERATION_CONFIGURE_CLIENT_FORMAT = 10,
    SP_AUDIO_OPERATION_SET_MAXIMUM_FRAMES = 11,
    SP_AUDIO_OPERATION_ATTACH_CALLBACK = 12,
    SP_AUDIO_OPERATION_INITIALIZE = 13,
    SP_AUDIO_OPERATION_START = 14,
    SP_AUDIO_OPERATION_STOP = 15,
    SP_AUDIO_OPERATION_DETACH_CALLBACK = 16,
    SP_AUDIO_OPERATION_UNINITIALIZE = 17,
    SP_AUDIO_OPERATION_DISPOSE = 18,
    SP_AUDIO_OPERATION_ALLOCATE_STATE = 19,
    SP_AUDIO_OPERATION_READ_FRAME_CAPABILITIES = 20,
    SP_AUDIO_OPERATION_VERIFY_UNIT_MAXIMUM = 21,
    SP_AUDIO_OPERATION_READ_SAMPLE_RATE_CAPABILITIES = 24,
    SP_AUDIO_OPERATION_WAIT_SAMPLE_RATE = 25,
    SP_AUDIO_OPERATION_WAIT_FRAME_COUNT = 26,
    SP_AUDIO_OPERATION_READ_NOMINAL_SAMPLE_RATE = 27,
    SP_AUDIO_OPERATION_READ_STREAM_CONFIGURATION = 28,
    SP_AUDIO_OPERATION_READ_CURRENT_FRAME_COUNT = 29,
    SP_AUDIO_OPERATION_READ_FRAME_RANGE = 30,
    SP_AUDIO_OPERATION_READ_VARIABLE_FRAME_CAPABILITY = 31,
    SP_AUDIO_OPERATION_QUIESCE_CALLBACKS = 36,
    SP_AUDIO_OPERATION_ADD_SAMPLE_RATE_LISTENER = 37,
    SP_AUDIO_OPERATION_REMOVE_SAMPLE_RATE_LISTENER = 38,
    SP_AUDIO_OPERATION_ADD_FRAME_COUNT_LISTENER = 39,
    SP_AUDIO_OPERATION_REMOVE_FRAME_COUNT_LISTENER = 40,
    SP_AUDIO_OPERATION_REMOVE_DEVICE_CHANGE_LISTENER = 42,
};

enum SpLifecycleEvent {
    SP_LIFECYCLE_INITIALIZE = 1,
    SP_LIFECYCLE_START = 2,
    SP_LIFECYCLE_STOP = 3,
    SP_LIFECYCLE_DETACH = 4,
    SP_LIFECYCLE_UNINITIALIZE = 5,
    SP_LIFECYCLE_DISPOSE = 6,
    SP_LIFECYCLE_REQUEST_SAMPLE_RATE = 7,
    SP_LIFECYCLE_REQUEST_FRAME_COUNT = 8,
    SP_LIFECYCLE_ADD_SAMPLE_RATE_LISTENER = 9,
    SP_LIFECYCLE_NOTIFY_SAMPLE_RATE = 10,
    SP_LIFECYCLE_REMOVE_SAMPLE_RATE_LISTENER = 11,
    SP_LIFECYCLE_ADD_FRAME_COUNT_LISTENER = 12,
    SP_LIFECYCLE_NOTIFY_FRAME_COUNT = 13,
    SP_LIFECYCLE_REMOVE_FRAME_COUNT_LISTENER = 14,
};

typedef uint32_t (*SpRenderCallback)(void *renderer, float *interleaved,
                                     uint32_t frames);
typedef uint32_t (*SpMultichannelRenderCallback)(void *renderer, const float *input,
                                                 uint32_t input_channels, float *output,
                                                 uint32_t output_channels, uint32_t frames);
typedef void (*SpLifecycleCallback)(void *context, uint32_t event);
typedef void (*SpRetirementCallback)(void *context);

typedef struct {
    double sample_rate_hz;
    uint32_t channel_count;
    uint32_t current_frames_per_slice;
    uint32_t supported_minimum_frames_per_slice;
    uint32_t supported_maximum_frames_per_slice;
    uint32_t maximum_callback_frames_per_slice;
    uint32_t audio_unit_maximum_frames_per_slice;
    uint32_t uses_variable_buffer_frame_sizes;
} SpDeviceFormatReport;

typedef struct {
    uint64_t callbacks;
    uint64_t rendered;
    uint64_t silenced;
    uint64_t invalid_frames;
    uint64_t invalid_buffers;
    uint64_t invalid_channels;
    uint64_t invalid_bytes;
    uint64_t frame_histogram_32;
    uint64_t frame_histogram_64;
    uint64_t frame_histogram_128;
    uint64_t frame_histogram_256;
} SpCallbackTelemetry;

typedef struct {
    int32_t status;
    uint32_t operation;
    int32_t listener_cleanup_status;
    uint32_t listener_cleanup_operation;
    int32_t cleanup_status;
    uint32_t cleanup_operation;
    int32_t final_state_status;
    uint32_t final_state_operation;
    uint32_t mutation_flags;
    uint32_t device_state_available;
    uint32_t renderer_retired;
    uint32_t native_releasable;
    SpDeviceFormatReport report;
} SpAudioResult;

typedef struct {
    int32_t status;
    uint32_t operation;
} SpNativeFailure;

typedef struct {
    uint32_t enabled;
    uint32_t fail_allocation;
    double sample_rate_hz;
    uint32_t channel_count;
    uint32_t current_frames_per_slice;
    uint32_t supported_minimum_frames_per_slice;
    uint32_t supported_maximum_frames_per_slice;
    uint32_t maximum_callback_frames_per_slice;
    uint32_t uses_variable_buffer_frame_sizes;
    uint32_t supports_48000;
    uint32_t failure_operation;
    int32_t failure_status;
    uint32_t cleanup_failure_operation;
    int32_t cleanup_failure_status;
    uint32_t callback_on_stop;
    uint32_t sample_rate_notification_polls;
    uint32_t frame_count_notification_polls;
    uint32_t sample_rate_coupled_frame_count;
    uint32_t final_state_failure_operation;
    int32_t final_state_failure_status;
    uint32_t quiescence_convergence_polls;
} SpFakeConfig;

typedef struct {
    _Atomic uint32_t generation;
} SpPropertyListenerState;

typedef struct {
    _Atomic uint32_t active_callbacks;
    _Atomic uint32_t writer_active;
    _Atomic uint64_t sequence;
    _Atomic uint64_t callbacks;
    _Atomic uint64_t rendered;
    _Atomic uint64_t silenced;
    _Atomic uint64_t invalid_frames;
    _Atomic uint64_t invalid_buffers;
    _Atomic uint64_t invalid_channels;
    _Atomic uint64_t invalid_bytes;
    _Atomic uint64_t frame_histogram_32;
    _Atomic uint64_t frame_histogram_64;
    _Atomic uint64_t frame_histogram_128;
    _Atomic uint64_t frame_histogram_256;
} SpAtomicTelemetry;

typedef struct SpAudioOutput {
    AudioUnit unit;
    AudioDeviceID device;
    uint32_t frames;
    uint8_t fake_backend;
    uint8_t started;
    uint8_t callback_attached;
    uint8_t initialized;
    uint8_t unit_created;
    uint8_t renderer_retired;
    uint8_t retirement_notified;
    uint8_t sample_rate_listener_attached;
    uint8_t frame_count_listener_attached;
    uint32_t mutation_flags;
    SpPropertyListenerState sample_rate_listener;
    SpPropertyListenerState frame_count_listener;
    void *renderer;
    SpRenderCallback render;
    SpMultichannelRenderCallback multichannel_render;
    uint8_t duplex;
    uint8_t multichannel;
    AudioDeviceID input_device;
    AudioDeviceID output_device;
    AudioDeviceID aggregate_device;
    uint32_t input_channels;
    uint32_t output_channels;
    uint32_t output_channel_offset;
    uint32_t input_channel_offset;
    uint8_t device_listener_attached[3][5];
    _Atomic uint32_t device_event;
    _Atomic uint32_t device_event_pending;
    float input_scratch[256 * 64];
    void *observer_context;
    SpLifecycleCallback lifecycle_observer;
    SpRetirementCallback retirement_observer;
    SpFakeConfig fake;
    SpDeviceFormatReport fake_report;
    SpAtomicTelemetry telemetry;
} SpAudioOutput;

static SpAudioResult sp_result_ok(SpDeviceFormatReport report) {
    SpAudioResult result;
    memset(&result, 0, sizeof(result));
    result.report = report;
    return result;
}

static SpAudioResult sp_result_error(int32_t status, uint32_t operation,
                                     SpDeviceFormatReport report) {
    SpAudioResult result = sp_result_ok(report);
    result.status = status;
    result.operation = operation;
    return result;
}

static void sp_record_lifecycle(SpAudioOutput *output, uint32_t event) {
    if (output->lifecycle_observer != NULL) {
        output->lifecycle_observer(output->observer_context, event);
    }
}

static void sp_mark_renderer_retired(SpAudioOutput *output) {
    output->renderer_retired = 1;
    if (!output->retirement_notified && output->retirement_observer != NULL) {
        output->retirement_notified = 1;
        output->retirement_observer(output->observer_context);
    }
    // Full quiescence and disposal succeeded. Remove renderer callback pointers before returning
    // ownership to Rust; setup/rollback observers remain until the wrapper is released.
    output->renderer = NULL;
    output->render = NULL;
    output->multichannel_render = NULL;
}

static uint32_t sp_native_releasable(const SpAudioOutput *output) {
    return output->renderer_retired && !output->unit_created &&
           output->aggregate_device == kAudioObjectUnknown &&
           !output->sample_rate_listener_attached &&
           !output->frame_count_listener_attached;
}

static void sp_zero_buffers(AudioBufferList *buffers) {
    if (buffers == NULL) {
        return;
    }
    for (uint32_t index = 0; index < buffers->mNumberBuffers; ++index) {
        AudioBuffer *buffer = &buffers->mBuffers[index];
        if (buffer->mData != NULL && buffer->mDataByteSize != 0) {
            memset(buffer->mData, 0, buffer->mDataByteSize);
        }
    }
}

static void sp_telemetry_init(SpAtomicTelemetry *telemetry) {
    atomic_init(&telemetry->active_callbacks, 0);
    atomic_init(&telemetry->writer_active, 0);
    atomic_init(&telemetry->sequence, 0);
    atomic_init(&telemetry->callbacks, 0);
    atomic_init(&telemetry->rendered, 0);
    atomic_init(&telemetry->silenced, 0);
    atomic_init(&telemetry->invalid_frames, 0);
    atomic_init(&telemetry->invalid_buffers, 0);
    atomic_init(&telemetry->invalid_channels, 0);
    atomic_init(&telemetry->invalid_bytes, 0);
    atomic_init(&telemetry->frame_histogram_32, 0);
    atomic_init(&telemetry->frame_histogram_64, 0);
    atomic_init(&telemetry->frame_histogram_128, 0);
    atomic_init(&telemetry->frame_histogram_256, 0);
}

// The AUHAL render callback is the only permitted sequence writer. CoreAudio normally
// serializes callbacks for one AudioUnit; the guard rejects any concurrent or reentrant
// entry without waiting and before it can call Rust. Control-thread code only snapshots.
static int sp_telemetry_begin(SpAtomicTelemetry *telemetry) {
    uint32_t expected = 0;
    if (!atomic_compare_exchange_strong_explicit(
            &telemetry->writer_active, &expected, 1, memory_order_seq_cst,
            memory_order_seq_cst)) {
        return 0;
    }
    // Sequential consistency makes the odd marker precede every counter update globally.
    atomic_fetch_add_explicit(&telemetry->sequence, 1, memory_order_seq_cst);
    return 1;
}

static void sp_telemetry_end(SpAtomicTelemetry *telemetry) {
    atomic_fetch_add_explicit(&telemetry->sequence, 1, memory_order_seq_cst);
    atomic_store_explicit(&telemetry->writer_active, 0, memory_order_seq_cst);
}

static void sp_callback_leave(SpAudioOutput *output) {
    atomic_fetch_sub_explicit(&output->telemetry.active_callbacks, 1,
                              memory_order_seq_cst);
}

static void sp_callback_finish(SpAudioOutput *output) {
    sp_telemetry_end(&output->telemetry);
    sp_callback_leave(output);
}

static OSStatus sp_render_callback(void *reference, AudioUnitRenderActionFlags *flags,
                                   const AudioTimeStamp *timestamp, UInt32 bus,
                                   UInt32 frame_count, AudioBufferList *buffers) {
    (void)flags;
    (void)timestamp;
    (void)bus;

    if (reference == NULL) {
        sp_zero_buffers(buffers);
        return noErr;
    }

    SpAudioOutput *output = (SpAudioOutput *)reference;
    // This is the first access through the callback context. Teardown waits for this count
    // after stop and detach before it attempts uninitialization or disposal.
    atomic_fetch_add_explicit(&output->telemetry.active_callbacks, 1,
                              memory_order_seq_cst);
    sp_zero_buffers(buffers);
    if (!sp_telemetry_begin(&output->telemetry)) {
        sp_callback_leave(output);
        return noErr;
    }
    atomic_fetch_add_explicit(&output->telemetry.callbacks, 1,
                              memory_order_seq_cst);

    if (frame_count != output->frames) {
        atomic_fetch_add_explicit(&output->telemetry.invalid_frames, 1,
                                  memory_order_seq_cst);
        sp_callback_finish(output);
        return noErr;
    }
    if (buffers == NULL || buffers->mNumberBuffers != 1) {
        atomic_fetch_add_explicit(&output->telemetry.invalid_buffers, 1,
                                  memory_order_seq_cst);
        sp_callback_finish(output);
        return noErr;
    }

    AudioBuffer *buffer = &buffers->mBuffers[0];
    if (buffer->mData == NULL) {
        atomic_fetch_add_explicit(&output->telemetry.invalid_buffers, 1,
                                  memory_order_seq_cst);
        sp_callback_finish(output);
        return noErr;
    }
    uint32_t channels = output->multichannel ? output->output_channels : 2;
    if (buffer->mNumberChannels != channels) {
        atomic_fetch_add_explicit(&output->telemetry.invalid_channels, 1,
                                  memory_order_seq_cst);
        sp_callback_finish(output);
        return noErr;
    }

    const uint32_t expected_bytes =
        output->frames * channels * (uint32_t)sizeof(float);
    if (buffer->mDataByteSize != expected_bytes) {
        atomic_fetch_add_explicit(&output->telemetry.invalid_bytes, 1,
                                  memory_order_seq_cst);
        sp_callback_finish(output);
        return noErr;
    }

    if (output->frames == 32) {
        atomic_fetch_add_explicit(&output->telemetry.frame_histogram_32, 1,
                                  memory_order_seq_cst);
    } else if (output->frames == 64) {
        atomic_fetch_add_explicit(&output->telemetry.frame_histogram_64, 1,
                                  memory_order_seq_cst);
    } else if (output->frames == 128) {
        atomic_fetch_add_explicit(&output->telemetry.frame_histogram_128, 1,
                                  memory_order_seq_cst);
    } else {
        atomic_fetch_add_explicit(&output->telemetry.frame_histogram_256, 1,
                                  memory_order_seq_cst);
    }

    if (atomic_load_explicit(&output->device_event, memory_order_acquire) != 0) {
        atomic_fetch_add_explicit(&output->telemetry.silenced, 1, memory_order_seq_cst);
        sp_callback_finish(output);
        return noErr;
    }
    uint32_t disposition;
    if (output->multichannel) {
        if (output->input_channels != 0) {
            AudioBufferList input_buffers = {
                .mNumberBuffers = 1,
                .mBuffers = {{.mNumberChannels = output->input_channels,
                              .mDataByteSize = output->frames * output->input_channels *
                                               (uint32_t)sizeof(float),
                              .mData = output->input_scratch}},
            };
            if (AudioUnitRender(output->unit, flags, timestamp, 1, output->frames,
                                &input_buffers) != noErr) {
                atomic_fetch_add_explicit(&output->telemetry.silenced, 1,
                                          memory_order_seq_cst);
                sp_callback_finish(output);
                return noErr;
            }
        }
        disposition = output->multichannel_render(
            output->renderer, output->input_scratch, output->input_channels,
            (float *)buffer->mData, output->output_channels, output->frames);
    } else {
        disposition = output->render(output->renderer, (float *)buffer->mData, output->frames);
    }
    if (disposition == SP_RENDERED) {
        atomic_fetch_add_explicit(&output->telemetry.rendered, 1,
                                  memory_order_seq_cst);
    } else {
        sp_zero_buffers(buffers);
        atomic_fetch_add_explicit(&output->telemetry.silenced, 1,
                                  memory_order_seq_cst);
    }
    sp_callback_finish(output);
    return noErr;
}

static OSStatus sp_resolve_default_device(AudioDeviceID *device) {
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioHardwarePropertyDefaultOutputDevice,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    UInt32 size = sizeof(*device);
    return AudioObjectGetPropertyData(kAudioObjectSystemObject, &address, 0, NULL,
                                      &size, device);
}

static SpNativeFailure sp_native_ok(void) {
    return (SpNativeFailure){.status = 0, .operation = SP_AUDIO_OPERATION_NONE};
}

static SpNativeFailure sp_native_error(int32_t status, uint32_t operation) {
    return (SpNativeFailure){.status = status, .operation = operation};
}

static SpNativeFailure sp_read_nominal_sample_rate(AudioDeviceID device,
                                                   double *sample_rate) {
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioDevicePropertyNominalSampleRate,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    UInt32 size = sizeof(*sample_rate);
    OSStatus status = AudioObjectGetPropertyData(device, &address, 0, NULL,
                                                 &size, sample_rate);
    if (status != noErr) {
        return sp_native_error(status,
                               SP_AUDIO_OPERATION_READ_NOMINAL_SAMPLE_RATE);
    }
    return sp_native_ok();
}

static SpNativeFailure sp_read_output_channels(AudioDeviceID device,
                                               uint32_t *channels) {
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioDevicePropertyStreamConfiguration,
        .mScope = kAudioObjectPropertyScopeOutput,
        .mElement = kAudioObjectPropertyElementMain,
    };
    UInt32 size = 0;
    OSStatus status =
        AudioObjectGetPropertyDataSize(device, &address, 0, NULL, &size);
    if (status != noErr) {
        return sp_native_error(status,
                               SP_AUDIO_OPERATION_READ_STREAM_CONFIGURATION);
    }
    AudioBufferList *buffers = malloc(size);
    if (buffers == NULL) {
        return sp_native_error(kAudio_MemFullError,
                               SP_AUDIO_OPERATION_READ_STREAM_CONFIGURATION);
    }
    status = AudioObjectGetPropertyData(device, &address, 0, NULL, &size,
                                        buffers);
    if (status == noErr) {
        uint32_t total = 0;
        for (uint32_t index = 0; index < buffers->mNumberBuffers; ++index) {
            total += buffers->mBuffers[index].mNumberChannels;
        }
        *channels = total;
    }
    free(buffers);
    if (status != noErr) {
        return sp_native_error(status,
                               SP_AUDIO_OPERATION_READ_STREAM_CONFIGURATION);
    }
    return sp_native_ok();
}

static SpNativeFailure sp_read_input_channels(AudioDeviceID device,
                                              uint32_t *channels) {
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioDevicePropertyStreamConfiguration,
        .mScope = kAudioDevicePropertyScopeInput,
        .mElement = kAudioObjectPropertyElementMain,
    };
    UInt32 size = 0;
    OSStatus status = AudioObjectGetPropertyDataSize(device, &address, 0, NULL, &size);
    if (status != noErr) return sp_native_error(status, SP_AUDIO_OPERATION_READ_STREAM_CONFIGURATION);
    AudioBufferList *buffers = malloc(size);
    if (buffers == NULL) return sp_native_error(kAudio_MemFullError, SP_AUDIO_OPERATION_READ_STREAM_CONFIGURATION);
    status = AudioObjectGetPropertyData(device, &address, 0, NULL, &size, buffers);
    if (status == noErr) {
        uint32_t total = 0;
        for (uint32_t index = 0; index < buffers->mNumberBuffers; ++index)
            total += buffers->mBuffers[index].mNumberChannels;
        *channels = total;
    }
    free(buffers);
    return status == noErr ? sp_native_ok() : sp_native_error(status, SP_AUDIO_OPERATION_READ_STREAM_CONFIGURATION);
}

static SpNativeFailure sp_read_current_frame_count(AudioDeviceID device,
                                                   uint32_t *frames) {
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioDevicePropertyBufferFrameSize,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    UInt32 size = sizeof(*frames);
    OSStatus status = AudioObjectGetPropertyData(device, &address, 0, NULL,
                                                 &size, frames);
    if (status != noErr) {
        return sp_native_error(status,
                               SP_AUDIO_OPERATION_READ_CURRENT_FRAME_COUNT);
    }
    return sp_native_ok();
}

static SpNativeFailure sp_read_device_format(AudioDeviceID device,
                                             SpDeviceFormatReport *report) {
    SpNativeFailure failure =
        sp_read_nominal_sample_rate(device, &report->sample_rate_hz);
    if (failure.status != 0) {
        return failure;
    }
    failure = sp_read_output_channels(device, &report->channel_count);
    if (failure.status != 0) {
        return failure;
    }
    failure = sp_read_current_frame_count(
        device, &report->current_frames_per_slice);
    if (failure.status != 0) {
        return failure;
    }

    AudioObjectPropertyAddress range_address = {
        .mSelector = kAudioDevicePropertyBufferFrameSizeRange,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    AudioValueRange range;
    UInt32 range_size = sizeof(range);
    OSStatus status = AudioObjectGetPropertyData(device, &range_address, 0, NULL,
                                                 &range_size, &range);
    if (status != noErr) {
        return sp_native_error(status, SP_AUDIO_OPERATION_READ_FRAME_RANGE);
    }
    report->supported_minimum_frames_per_slice = (uint32_t)range.mMinimum;
    report->supported_maximum_frames_per_slice = (uint32_t)range.mMaximum;

    AudioObjectPropertyAddress variable_address = {
        .mSelector = kAudioDevicePropertyUsesVariableBufferFrameSizes,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    report->uses_variable_buffer_frame_sizes = 0;
    report->maximum_callback_frames_per_slice =
        report->current_frames_per_slice;
    if (AudioObjectHasProperty(device, &variable_address)) {
        UInt32 variable_size =
            sizeof(report->maximum_callback_frames_per_slice);
        status = AudioObjectGetPropertyData(
            device, &variable_address, 0, NULL, &variable_size,
            &report->maximum_callback_frames_per_slice);
        if (status != noErr) {
            return sp_native_error(
                status, SP_AUDIO_OPERATION_READ_VARIABLE_FRAME_CAPABILITY);
        }
        report->uses_variable_buffer_frame_sizes = 1;
    }
    return sp_native_ok();
}

static OSStatus sp_supports_sample_rate(AudioDeviceID device,
                                       double sample_rate,
                                       uint32_t *supported) {
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioDevicePropertyAvailableNominalSampleRates,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    UInt32 size = 0;
    OSStatus status =
        AudioObjectGetPropertyDataSize(device, &address, 0, NULL, &size);
    if (status != noErr) {
        return status;
    }
    AudioValueRange *ranges = malloc(size);
    if (ranges == NULL) {
        return kAudio_MemFullError;
    }
    status = AudioObjectGetPropertyData(device, &address, 0, NULL, &size,
                                        ranges);
    *supported = 0;
    if (status == noErr) {
        const uint32_t count = size / (uint32_t)sizeof(*ranges);
        for (uint32_t index = 0; index < count; ++index) {
            if (ranges[index].mMinimum <= sample_rate &&
                sample_rate <= ranges[index].mMaximum) {
                *supported = 1;
                break;
            }
        }
    }
    free(ranges);
    return status;
}

static OSStatus sp_set_sample_rate(AudioDeviceID device, double sample_rate) {
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioDevicePropertyNominalSampleRate,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    return AudioObjectSetPropertyData(device, &address, 0, NULL,
                                      sizeof(sample_rate), &sample_rate);
}

static OSStatus sp_set_frame_count(AudioDeviceID device, uint32_t frames) {
    AudioObjectPropertyAddress address = {
        .mSelector = kAudioDevicePropertyBufferFrameSize,
        .mScope = kAudioObjectPropertyScopeGlobal,
        .mElement = kAudioObjectPropertyElementMain,
    };
    return AudioObjectSetPropertyData(device, &address, 0, NULL,
                                      sizeof(frames), &frames);
}

static void sp_poll_delay(void) {
    const struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
    (void)nanosleep(&delay, NULL);
}

static OSStatus sp_property_listener(AudioObjectID object, UInt32 count,
                                     const AudioObjectPropertyAddress addresses[],
                                     void *context) {
    (void)object;
    (void)count;
    (void)addresses;
    SpPropertyListenerState *listener = context;
    atomic_fetch_add_explicit(&listener->generation, 1, memory_order_release);
    return noErr;
}

static SpNativeFailure sp_add_listener(SpAudioOutput *output,
                                       AudioObjectPropertySelector selector,
                                       SpPropertyListenerState *listener,
                                       uint8_t *attached, uint32_t operation,
                                       uint32_t event) {
    if (output->fake_backend) {
        *attached = 1;
        sp_record_lifecycle(output, event);
        return sp_native_ok();
    }
    AudioObjectPropertyAddress address = {selector,
                                          kAudioObjectPropertyScopeGlobal,
                                          kAudioObjectPropertyElementMain};
    OSStatus status = AudioObjectAddPropertyListener(output->device, &address,
                                                     sp_property_listener,
                                                     listener);
    if (status != noErr) {
        return sp_native_error(status, operation);
    }
    *attached = 1;
    sp_record_lifecycle(output, event);
    return sp_native_ok();
}

static SpNativeFailure sp_remove_listener(SpAudioOutput *output,
                                          AudioObjectPropertySelector selector,
                                          SpPropertyListenerState *listener,
                                          uint8_t *attached, uint32_t operation,
                                          uint32_t event) {
    if (!*attached) {
        return sp_native_ok();
    }
    if (output->fake_backend) {
        *attached = 0;
        sp_record_lifecycle(output, event);
        return sp_native_ok();
    }
    AudioObjectPropertyAddress address = {selector,
                                          kAudioObjectPropertyScopeGlobal,
                                          kAudioObjectPropertyElementMain};
    OSStatus status = AudioObjectRemovePropertyListener(output->device, &address,
                                                        sp_property_listener,
                                                        listener);
    if (status != noErr) {
        return sp_native_error(status, operation);
    }
    *attached = 0;
    sp_record_lifecycle(output, event);
    return sp_native_ok();
}

static SpNativeFailure sp_wait_notification(SpAudioOutput *output,
                                             SpPropertyListenerState *listener,
                                             uint32_t generation,
                                             uint32_t notification_polls,
                                             uint32_t timeout_operation,
                                             uint32_t event) {
    for (uint32_t poll = 0; poll < SP_PROPERTY_POLL_LIMIT; ++poll) {
        if (output->fake_backend && notification_polls != SP_FAKE_NEVER_NOTIFIES &&
            poll >= notification_polls) {
            atomic_fetch_add_explicit(&listener->generation, 1, memory_order_release);
            sp_record_lifecycle(output, event);
            notification_polls = SP_FAKE_NEVER_NOTIFIES;
        }
        if (atomic_load_explicit(&listener->generation, memory_order_acquire) != generation) {
            return sp_native_ok();
        }
        if (!output->fake_backend) sp_poll_delay();
    }
    return sp_native_error(SP_AUDIO_STATUS_PROPERTY_TIMEOUT, timeout_operation);
}

static int sp_report_has_immutable_requirements(SpDeviceFormatReport report,
                                                uint32_t frames, uint32_t channels) {
    // The AUHAL client stream is fixed stereo, so an output with more physical channels is
    // supported when it has channels 1 and 2 available. `sp_configure_output_channel_map`
    // explicitly silences every physical channel after those two.
    return report.channel_count >= channels &&
           !report.uses_variable_buffer_frame_sizes &&
           report.maximum_callback_frames_per_slice ==
               report.current_frames_per_slice &&
           report.supported_minimum_frames_per_slice <= frames &&
           frames <= report.supported_maximum_frames_per_slice;
}

static int sp_report_matches(SpDeviceFormatReport report, uint32_t frames,
                             uint32_t channels) {
    return sp_report_has_immutable_requirements(report, frames, channels) &&
           report.sample_rate_hz == 48000.0 &&
           report.current_frames_per_slice == frames &&
           report.maximum_callback_frames_per_slice == frames;
}

// Maps the fixed two-channel AUHAL client stream to physical output channels 1 and 2. The
// output-unit channel-map array indexes destination physical channels, so every remaining device
// channel receives -1 and stays silent. This runs only during control-thread setup.
static SpNativeFailure sp_configure_output_channel_map(AudioUnit unit,
                                                       uint32_t physical_channels,
                                                       uint32_t client_channels,
                                                       uint32_t offset) {
    if (client_channels == 0 || offset > physical_channels ||
        client_channels > physical_channels - offset ||
        physical_channels > UINT32_MAX / (uint32_t)sizeof(SInt32)) {
        return sp_native_error(SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
                               SP_AUDIO_OPERATION_CONFIGURE_CLIENT_FORMAT);
    }
    const uint32_t bytes = physical_channels * (uint32_t)sizeof(SInt32);
    SInt32 *channel_map = malloc(bytes);
    if (channel_map == NULL) {
        return sp_native_error(kAudio_MemFullError,
                               SP_AUDIO_OPERATION_CONFIGURE_CLIENT_FORMAT);
    }
    for (uint32_t channel = 0; channel < physical_channels; ++channel) {
        channel_map[channel] = (channel >= offset && channel - offset < client_channels)
                                   ? (SInt32)(channel - offset) : -1;
    }
    OSStatus status = AudioUnitSetProperty(
        unit, kAudioOutputUnitProperty_ChannelMap, kAudioUnitScope_Input, 0,
        channel_map, bytes);
    free(channel_map);
    if (status != noErr) {
        return sp_native_error(status, SP_AUDIO_OPERATION_CONFIGURE_CLIENT_FORMAT);
    }
    return sp_native_ok();
}

static int sp_fake_fails(const SpAudioOutput *output, uint32_t operation,
                         SpAudioResult *result) {
    if (output->fake.failure_operation != operation) {
        return 0;
    }
    *result = sp_result_error(output->fake.failure_status, operation,
                              output->fake_report);
    return 1;
}

static int sp_fake_teardown_fails(const SpAudioOutput *output,
                                  uint32_t operation,
                                  SpAudioResult *result) {
    if (output->fake.cleanup_failure_operation == operation) {
        *result = sp_result_error(output->fake.cleanup_failure_status, operation,
                                  output->fake_report);
        return 1;
    }
    return sp_fake_fails(output, operation, result);
}

static SpAudioResult sp_destroy_output(SpAudioOutput *output);
static OSStatus sp_add_device_change_listeners(SpAudioOutput *output);
static OSStatus sp_remove_device_change_listeners(SpAudioOutput *output);

static SpAudioResult sp_finish_failed_create(SpAudioOutput *output,
                                             SpAudioResult failure) {
    SpAudioResult cleanup = sp_destroy_output(output);
    failure.cleanup_status = cleanup.status;
    failure.cleanup_operation = cleanup.operation;
    failure.listener_cleanup_status = cleanup.listener_cleanup_status;
    failure.listener_cleanup_operation = cleanup.listener_cleanup_operation;
    failure.renderer_retired = cleanup.renderer_retired;
    failure.native_releasable = cleanup.native_releasable;
    failure.mutation_flags = output->mutation_flags;
    if (output->fake_backend) {
        if (output->fake.final_state_failure_operation != 0) {
            failure.final_state_status = output->fake.final_state_failure_status;
            failure.final_state_operation = output->fake.final_state_failure_operation;
        } else {
            failure.report = output->fake_report;
            failure.device_state_available = 1;
        }
    } else if (output->device != kAudioObjectUnknown) {
        SpNativeFailure final_state = sp_read_device_format(output->device, &failure.report);
        if (final_state.status != 0) {
            failure.final_state_status = final_state.status;
            failure.final_state_operation = final_state.operation;
        } else {
            failure.device_state_available = 1;
        }
    }
    return failure;
}

static SpAudioResult sp_create_fake_output(SpAudioOutput *output,
                                           uint8_t allow_reconfiguration) {
    SpAudioResult result = sp_result_ok(output->fake_report);
    const uint32_t read_operations[] = {
        SP_AUDIO_OPERATION_RESOLVE_DEFAULT_DEVICE,
        SP_AUDIO_OPERATION_READ_NOMINAL_SAMPLE_RATE,
        SP_AUDIO_OPERATION_READ_STREAM_CONFIGURATION,
        SP_AUDIO_OPERATION_READ_CURRENT_FRAME_COUNT,
        SP_AUDIO_OPERATION_READ_FRAME_RANGE,
        SP_AUDIO_OPERATION_READ_VARIABLE_FRAME_CAPABILITY,
    };
    for (size_t index = 0;
         index < sizeof(read_operations) / sizeof(read_operations[0]);
         ++index) {
        if (sp_fake_fails(output, read_operations[index], &result)) {
            return sp_finish_failed_create(output, result);
        }
    }

    if (!sp_report_has_immutable_requirements(output->fake_report,
                                               output->frames, 2)) {
        result = sp_result_error(SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
                                 SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT,
                                 output->fake_report);
        return sp_finish_failed_create(output, result);
    }
    if (!sp_report_matches(output->fake_report, output->frames, 2) &&
        !allow_reconfiguration) {
        result = sp_result_error(SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
                                 SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT,
                                 output->fake_report);
        return sp_finish_failed_create(output, result);
    }

    if (output->fake_report.sample_rate_hz != 48000.0) {
        if (sp_fake_fails(
                output, SP_AUDIO_OPERATION_READ_SAMPLE_RATE_CAPABILITIES,
                &result)) {
            return sp_finish_failed_create(output, result);
        }
        if (!output->fake.supports_48000) {
            result = sp_result_error(SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
                                     SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT,
                                     output->fake_report);
            return sp_finish_failed_create(output, result);
        }
        SpNativeFailure listener = sp_add_listener(
            output, kAudioDevicePropertyNominalSampleRate,
            &output->sample_rate_listener, &output->sample_rate_listener_attached,
            SP_AUDIO_OPERATION_ADD_SAMPLE_RATE_LISTENER,
            SP_LIFECYCLE_ADD_SAMPLE_RATE_LISTENER);
        if (listener.status != 0) {
            return sp_finish_failed_create(output, sp_result_error(listener.status, listener.operation, output->fake_report));
        }
        if (sp_fake_fails(output, SP_AUDIO_OPERATION_REQUEST_SAMPLE_RATE,
                          &result)) {
            return sp_finish_failed_create(output, result);
        }
        output->mutation_flags |= SP_MUTATION_SAMPLE_RATE_REQUESTED;
        sp_record_lifecycle(output, SP_LIFECYCLE_REQUEST_SAMPLE_RATE);
        SpNativeFailure wait = sp_wait_notification(
            output, &output->sample_rate_listener,
            atomic_load_explicit(&output->sample_rate_listener.generation, memory_order_acquire),
            output->fake.sample_rate_notification_polls,
            SP_AUDIO_OPERATION_WAIT_SAMPLE_RATE, SP_LIFECYCLE_NOTIFY_SAMPLE_RATE);
        if (wait.status != 0) {
            result = sp_result_error(wait.status, wait.operation, output->fake_report);
            return sp_finish_failed_create(output, result);
        }
        output->fake_report.sample_rate_hz = 48000.0;
        if (output->fake.sample_rate_coupled_frame_count != 0) {
            output->fake_report.current_frames_per_slice = output->fake.sample_rate_coupled_frame_count;
            output->fake_report.maximum_callback_frames_per_slice = output->fake.sample_rate_coupled_frame_count;
        }
        output->mutation_flags |= SP_MUTATION_SAMPLE_RATE_ACKNOWLEDGED;
    }

    if (output->fake_report.current_frames_per_slice != output->frames) {
        SpNativeFailure listener = sp_add_listener(
            output, kAudioDevicePropertyBufferFrameSize,
            &output->frame_count_listener, &output->frame_count_listener_attached,
            SP_AUDIO_OPERATION_ADD_FRAME_COUNT_LISTENER,
            SP_LIFECYCLE_ADD_FRAME_COUNT_LISTENER);
        if (listener.status != 0) {
            return sp_finish_failed_create(output, sp_result_error(listener.status, listener.operation, output->fake_report));
        }
        if (sp_fake_fails(output, SP_AUDIO_OPERATION_REQUEST_FRAME_COUNT,
                          &result)) {
            return sp_finish_failed_create(output, result);
        }
        output->mutation_flags |= SP_MUTATION_FRAME_COUNT_REQUESTED;
        sp_record_lifecycle(output, SP_LIFECYCLE_REQUEST_FRAME_COUNT);
        SpNativeFailure wait = sp_wait_notification(
            output, &output->frame_count_listener,
            atomic_load_explicit(&output->frame_count_listener.generation, memory_order_acquire),
            output->fake.frame_count_notification_polls,
            SP_AUDIO_OPERATION_WAIT_FRAME_COUNT, SP_LIFECYCLE_NOTIFY_FRAME_COUNT);
        if (wait.status != 0) {
            result = sp_result_error(wait.status, wait.operation, output->fake_report);
            return sp_finish_failed_create(output, result);
        }
        output->fake_report.current_frames_per_slice = output->frames;
        if (!output->fake_report.uses_variable_buffer_frame_sizes) {
            output->fake_report.maximum_callback_frames_per_slice = output->frames;
        }
        output->mutation_flags |= SP_MUTATION_FRAME_COUNT_ACKNOWLEDGED;
    }

    if (!sp_report_matches(output->fake_report, output->frames, 2)) {
        result = sp_result_error(SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
                                 SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT,
                                 output->fake_report);
        return sp_finish_failed_create(output, result);
    }
    SpNativeFailure remove_sample = sp_remove_listener(
        output, kAudioDevicePropertyNominalSampleRate, &output->sample_rate_listener,
        &output->sample_rate_listener_attached, SP_AUDIO_OPERATION_REMOVE_SAMPLE_RATE_LISTENER,
        SP_LIFECYCLE_REMOVE_SAMPLE_RATE_LISTENER);
    SpNativeFailure remove_frames = sp_remove_listener(
        output, kAudioDevicePropertyBufferFrameSize, &output->frame_count_listener,
        &output->frame_count_listener_attached, SP_AUDIO_OPERATION_REMOVE_FRAME_COUNT_LISTENER,
        SP_LIFECYCLE_REMOVE_FRAME_COUNT_LISTENER);
    if (remove_sample.status != 0 || remove_frames.status != 0) {
        SpNativeFailure failure = remove_sample.status != 0 ? remove_sample : remove_frames;
        return sp_finish_failed_create(output, sp_result_error(failure.status, failure.operation, output->fake_report));
    }

    const uint32_t operations[] = {
        SP_AUDIO_OPERATION_CREATE_UNIT,
        SP_AUDIO_OPERATION_BIND_DEVICE,
        SP_AUDIO_OPERATION_DISABLE_INPUT,
        SP_AUDIO_OPERATION_ENABLE_OUTPUT,
        SP_AUDIO_OPERATION_CONFIGURE_CLIENT_FORMAT,
        SP_AUDIO_OPERATION_SET_MAXIMUM_FRAMES,
        SP_AUDIO_OPERATION_VERIFY_UNIT_MAXIMUM,
        SP_AUDIO_OPERATION_ATTACH_CALLBACK,
        SP_AUDIO_OPERATION_INITIALIZE,
        SP_AUDIO_OPERATION_START,
    };
    for (size_t index = 0;
         index < sizeof(operations) / sizeof(operations[0]); ++index) {
        if (sp_fake_fails(output, operations[index], &result)) {
            return sp_finish_failed_create(output, result);
        }
        if (operations[index] == SP_AUDIO_OPERATION_CREATE_UNIT) {
            output->unit_created = 1;
        } else if (operations[index] ==
                   SP_AUDIO_OPERATION_SET_MAXIMUM_FRAMES) {
            output->fake_report.audio_unit_maximum_frames_per_slice =
                output->frames;
        } else if (operations[index] ==
                   SP_AUDIO_OPERATION_ATTACH_CALLBACK) {
            output->callback_attached = 1;
        } else if (operations[index] == SP_AUDIO_OPERATION_INITIALIZE) {
            output->initialized = 1;
            sp_record_lifecycle(output, SP_LIFECYCLE_INITIALIZE);
        } else if (operations[index] == SP_AUDIO_OPERATION_START) {
            output->started = 1;
            sp_record_lifecycle(output, SP_LIFECYCLE_START);
        }
    }

    return sp_result_ok(output->fake_report);
}

static SpAudioResult sp_create_core_audio_output(
    SpAudioOutput *output, uint8_t allow_reconfiguration) {
    SpDeviceFormatReport report = {0};
    OSStatus status = noErr;
    if (output->device == kAudioObjectUnknown) status = sp_resolve_default_device(&output->device);
    if (status != noErr) {
        return sp_finish_failed_create(
            output, sp_result_error(status,
                                    SP_AUDIO_OPERATION_RESOLVE_DEFAULT_DEVICE,
                                    report));
    }

    SpNativeFailure failure =
        sp_read_device_format(output->device, &report);
    if (failure.status != 0) {
        return sp_finish_failed_create(
            output, sp_result_error(failure.status, failure.operation, report));
    }
    if (output->duplex) {
        uint32_t input_channels = 0;
        failure = sp_read_input_channels(output->device, &input_channels);
        if (failure.status != 0 || input_channels == 0) {
            return sp_finish_failed_create(output, sp_result_error(
                failure.status != 0 ? failure.status : SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
                failure.status != 0 ? failure.operation : SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT, report));
        }
    }
    if (!sp_report_has_immutable_requirements(report, output->frames,
                                              output->multichannel ? output->output_channels + output->output_channel_offset : 2)) {
        return sp_finish_failed_create(
            output, sp_result_error(SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
                                    SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT,
                                    report));
    }
    if (!sp_report_matches(report, output->frames,
                           output->multichannel ? output->output_channels + output->output_channel_offset : 2) &&
        !allow_reconfiguration) {
        return sp_finish_failed_create(
            output, sp_result_error(SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
                                    SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT,
                                    report));
    }

    if (report.sample_rate_hz != 48000.0) {
        uint32_t supported = 0;
        status = sp_supports_sample_rate(output->device, 48000.0,
                                         &supported);
        if (status != noErr) {
            return sp_finish_failed_create(
                output,
                sp_result_error(
                    status,
                    SP_AUDIO_OPERATION_READ_SAMPLE_RATE_CAPABILITIES,
                    report));
        }
        if (!supported) {
            return sp_finish_failed_create(
                output,
                sp_result_error(SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
                                SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT,
                                report));
        }
        failure = sp_add_listener(output, kAudioDevicePropertyNominalSampleRate,
                                  &output->sample_rate_listener,
                                  &output->sample_rate_listener_attached,
                                  SP_AUDIO_OPERATION_ADD_SAMPLE_RATE_LISTENER,
                                  SP_LIFECYCLE_ADD_SAMPLE_RATE_LISTENER);
        if (failure.status != 0) return sp_finish_failed_create(output, sp_result_error(failure.status, failure.operation, report));
        uint32_t generation = atomic_load_explicit(&output->sample_rate_listener.generation, memory_order_acquire);
        status = sp_set_sample_rate(output->device, 48000.0);
        if (status != noErr) {
            return sp_finish_failed_create(
                output,
                sp_result_error(status,
                                SP_AUDIO_OPERATION_REQUEST_SAMPLE_RATE,
                                report));
        }
        output->mutation_flags |= SP_MUTATION_SAMPLE_RATE_REQUESTED;
        failure = sp_wait_notification(output, &output->sample_rate_listener, generation,
                                       SP_FAKE_NEVER_NOTIFIES, SP_AUDIO_OPERATION_WAIT_SAMPLE_RATE,
                                       SP_LIFECYCLE_NOTIFY_SAMPLE_RATE);
        if (failure.status != 0) {
            return sp_finish_failed_create(
                output,
                sp_result_error(failure.status, failure.operation, report));
        }
        output->mutation_flags |= SP_MUTATION_SAMPLE_RATE_ACKNOWLEDGED;
        failure = sp_read_device_format(output->device, &report);
        if (failure.status != 0) return sp_finish_failed_create(output, sp_result_error(failure.status, failure.operation, report));
    }
    if (report.current_frames_per_slice != output->frames) {
        failure = sp_add_listener(output, kAudioDevicePropertyBufferFrameSize,
                                  &output->frame_count_listener,
                                  &output->frame_count_listener_attached,
                                  SP_AUDIO_OPERATION_ADD_FRAME_COUNT_LISTENER,
                                  SP_LIFECYCLE_ADD_FRAME_COUNT_LISTENER);
        if (failure.status != 0) return sp_finish_failed_create(output, sp_result_error(failure.status, failure.operation, report));
        uint32_t generation = atomic_load_explicit(&output->frame_count_listener.generation, memory_order_acquire);
        status = sp_set_frame_count(output->device, output->frames);
        if (status != noErr) {
            return sp_finish_failed_create(
                output,
                sp_result_error(status,
                                SP_AUDIO_OPERATION_REQUEST_FRAME_COUNT,
                                report));
        }
        output->mutation_flags |= SP_MUTATION_FRAME_COUNT_REQUESTED;
        failure = sp_wait_notification(output, &output->frame_count_listener, generation,
                                       SP_FAKE_NEVER_NOTIFIES, SP_AUDIO_OPERATION_WAIT_FRAME_COUNT,
                                       SP_LIFECYCLE_NOTIFY_FRAME_COUNT);
        if (failure.status != 0) {
            return sp_finish_failed_create(
                output,
                sp_result_error(failure.status, failure.operation, report));
        }
        output->mutation_flags |= SP_MUTATION_FRAME_COUNT_ACKNOWLEDGED;
        failure = sp_read_device_format(output->device, &report);
        if (failure.status != 0) return sp_finish_failed_create(output, sp_result_error(failure.status, failure.operation, report));
    }

    failure = sp_remove_listener(output, kAudioDevicePropertyNominalSampleRate,
                                 &output->sample_rate_listener, &output->sample_rate_listener_attached,
                                 SP_AUDIO_OPERATION_REMOVE_SAMPLE_RATE_LISTENER,
                                 SP_LIFECYCLE_REMOVE_SAMPLE_RATE_LISTENER);
    if (failure.status == 0) failure = sp_remove_listener(output, kAudioDevicePropertyBufferFrameSize,
                                 &output->frame_count_listener, &output->frame_count_listener_attached,
                                 SP_AUDIO_OPERATION_REMOVE_FRAME_COUNT_LISTENER,
                                 SP_LIFECYCLE_REMOVE_FRAME_COUNT_LISTENER);
    if (failure.status != 0) return sp_finish_failed_create(output, sp_result_error(failure.status, failure.operation, report));
    failure = sp_read_device_format(output->device, &report);
    if (failure.status != 0) {
        return sp_finish_failed_create(
            output, sp_result_error(failure.status, failure.operation, report));
    }
    if (!sp_report_matches(report, output->frames,
                           output->multichannel ? output->output_channels + output->output_channel_offset : 2)) {
        return sp_finish_failed_create(
            output, sp_result_error(SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
                                    SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT,
                                    report));
    }

    AudioComponentDescription description = {
        .componentType = kAudioUnitType_Output,
        .componentSubType = kAudioUnitSubType_HALOutput,
        .componentManufacturer = kAudioUnitManufacturer_Apple,
        .componentFlags = 0,
        .componentFlagsMask = 0,
    };
    AudioComponent component = AudioComponentFindNext(NULL, &description);
    if (component == NULL) {
        return sp_finish_failed_create(
            output, sp_result_error(SP_AUDIO_STATUS_COMPONENT_NOT_FOUND,
                                    SP_AUDIO_OPERATION_CREATE_UNIT, report));
    }
    status = AudioComponentInstanceNew(component, &output->unit);
    if (status != noErr) {
        return sp_finish_failed_create(
            output, sp_result_error(status, SP_AUDIO_OPERATION_CREATE_UNIT,
                                    report));
    }
    output->unit_created = 1;

    status = AudioUnitSetProperty(output->unit,
                                  kAudioOutputUnitProperty_CurrentDevice,
                                  kAudioUnitScope_Global, 0, &output->device,
                                  sizeof(output->device));
    if (status != noErr) {
        return sp_finish_failed_create(
            output, sp_result_error(status, SP_AUDIO_OPERATION_BIND_DEVICE,
                                    report));
    }

    UInt32 input_enabled = output->duplex ? 1 : 0;
    status = AudioUnitSetProperty(output->unit,
                                  kAudioOutputUnitProperty_EnableIO,
                                  kAudioUnitScope_Input, 1, &input_enabled,
                                  sizeof(input_enabled));
    if (status != noErr) {
        return sp_finish_failed_create(output, sp_result_error(
            status, output->duplex ? SP_AUDIO_OPERATION_ENABLE_OUTPUT : SP_AUDIO_OPERATION_DISABLE_INPUT, report));
    }

    UInt32 enable_output = 1;
    status = AudioUnitSetProperty(output->unit,
                                  kAudioOutputUnitProperty_EnableIO,
                                  kAudioUnitScope_Output, 0, &enable_output,
                                  sizeof(enable_output));
    if (status != noErr) {
        return sp_finish_failed_create(
            output, sp_result_error(status, SP_AUDIO_OPERATION_ENABLE_OUTPUT,
                                    report));
    }

    uint32_t output_channels = output->multichannel ? output->output_channels : 2;
    AudioStreamBasicDescription client_format = {
        .mSampleRate = 48000.0,
        .mFormatID = kAudioFormatLinearPCM,
        .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        .mBytesPerPacket = output_channels * sizeof(float),
        .mFramesPerPacket = 1,
        .mBytesPerFrame = output_channels * sizeof(float),
        .mChannelsPerFrame = output_channels,
        .mBitsPerChannel = 8 * sizeof(float),
        .mReserved = 0,
    };
    status = AudioUnitSetProperty(output->unit,
                                  kAudioUnitProperty_StreamFormat,
                                  kAudioUnitScope_Input, 0, &client_format,
                                  sizeof(client_format));
    if (status != noErr) {
        return sp_finish_failed_create(
            output,
            sp_result_error(status,
                            SP_AUDIO_OPERATION_CONFIGURE_CLIENT_FORMAT,
                            report));
    }
    failure = sp_configure_output_channel_map(output->unit, report.channel_count,
                                              output_channels, output->output_channel_offset);
    if (failure.status != 0) {
        return sp_finish_failed_create(
            output, sp_result_error(failure.status, failure.operation, report));
    }
    if (output->duplex) {
        if (output->multichannel) {
            client_format.mBytesPerPacket = output->input_channels * sizeof(float);
            client_format.mBytesPerFrame = client_format.mBytesPerPacket;
            client_format.mChannelsPerFrame = output->input_channels;
        }
        status = AudioUnitSetProperty(output->unit, kAudioUnitProperty_StreamFormat,
                                      kAudioUnitScope_Output, 1, &client_format,
                                      sizeof(client_format));
        if (status != noErr) return sp_finish_failed_create(output, sp_result_error(
            status, SP_AUDIO_OPERATION_CONFIGURE_CLIENT_FORMAT, report));
        if (output->multichannel) {
            SInt32 map[64];
            for (uint32_t channel = 0; channel < output->input_channels; ++channel) {
                map[channel] = (SInt32)(output->input_channel_offset + channel);
            }
            status = AudioUnitSetProperty(output->unit, kAudioOutputUnitProperty_ChannelMap,
                                          kAudioUnitScope_Output, 1, map,
                                          output->input_channels * sizeof(SInt32));
            if (status != noErr) return sp_finish_failed_create(output, sp_result_error(
                status, SP_AUDIO_OPERATION_CONFIGURE_CLIENT_FORMAT, report));
        }
    }

    UInt32 maximum_frames = report.maximum_callback_frames_per_slice;
    status = AudioUnitSetProperty(output->unit,
                                  kAudioUnitProperty_MaximumFramesPerSlice,
                                  kAudioUnitScope_Global, 0, &maximum_frames,
                                  sizeof(maximum_frames));
    if (status != noErr) {
        return sp_finish_failed_create(
            output,
            sp_result_error(status,
                            SP_AUDIO_OPERATION_SET_MAXIMUM_FRAMES, report));
    }
    UInt32 maximum_size = sizeof(maximum_frames);
    status = AudioUnitGetProperty(output->unit,
                                  kAudioUnitProperty_MaximumFramesPerSlice,
                                  kAudioUnitScope_Global, 0, &maximum_frames,
                                  &maximum_size);
    report.audio_unit_maximum_frames_per_slice = maximum_frames;
    if (status != noErr ||
        maximum_frames < report.maximum_callback_frames_per_slice) {
        if (status == noErr) {
            status = SP_AUDIO_STATUS_UNSUPPORTED_FORMAT;
        }
        return sp_finish_failed_create(
            output,
            sp_result_error(status,
                            SP_AUDIO_OPERATION_VERIFY_UNIT_MAXIMUM, report));
    }

    AURenderCallbackStruct callback = {
        .inputProc = sp_render_callback,
        .inputProcRefCon = output,
    };
    status = AudioUnitSetProperty(output->unit,
                                  kAudioUnitProperty_SetRenderCallback,
                                  kAudioUnitScope_Input, 0, &callback,
                                  sizeof(callback));
    if (status != noErr) {
        return sp_finish_failed_create(
            output, sp_result_error(status,
                                    SP_AUDIO_OPERATION_ATTACH_CALLBACK,
                                    report));
    }
    output->callback_attached = 1;

    status = AudioUnitInitialize(output->unit);
    if (status != noErr) {
        return sp_finish_failed_create(
            output, sp_result_error(status, SP_AUDIO_OPERATION_INITIALIZE,
                                    report));
    }
    output->initialized = 1;

    status = AudioOutputUnitStart(output->unit);
    if (status != noErr) {
        return sp_finish_failed_create(
            output,
            sp_result_error(status, SP_AUDIO_OPERATION_START, report));
    }
    output->started = 1;
    if (output->duplex || output->multichannel) {
        status = sp_add_device_change_listeners(output);
        if (status != noErr) return sp_finish_failed_create(output, sp_result_error(
            status, SP_AUDIO_OPERATION_START, report));
    }
    return sp_result_ok(report);
}

SpAudioResult sp_audio_output_create(
    SpAudioOutput **output_result, uint32_t frames,
    uint8_t allow_reconfiguration, void *renderer, SpRenderCallback render,
    const SpFakeConfig *fake_config, void *observer_context,
    SpLifecycleCallback lifecycle_observer,
    SpRetirementCallback retirement_observer) {
    SpDeviceFormatReport report = {
        .sample_rate_hz = 48000.0,
        .channel_count = 2,
        .current_frames_per_slice = frames,
        .supported_minimum_frames_per_slice = 128,
        .supported_maximum_frames_per_slice = 256,
        .maximum_callback_frames_per_slice = frames,
        .audio_unit_maximum_frames_per_slice = 0,
        .uses_variable_buffer_frame_sizes = 0,
    };
    *output_result = NULL;

    if (frames != 32 && frames != 64 && frames != 128 && frames != 256) {
        SpAudioResult result = sp_result_error(
            SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
            SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT, report);
        result.renderer_retired = 1;
        result.native_releasable = 1;
        return result;
    }
    if (fake_config != NULL && fake_config->enabled &&
        fake_config->fail_allocation) {
        SpAudioResult result = sp_result_error(
            kAudio_MemFullError, SP_AUDIO_OPERATION_ALLOCATE_STATE, report);
        result.renderer_retired = 1;
        result.native_releasable = 1;
        return result;
    }

    SpAudioOutput *output = calloc(1, sizeof(*output));
    if (output == NULL) {
        SpAudioResult result = sp_result_error(
            kAudio_MemFullError, SP_AUDIO_OPERATION_ALLOCATE_STATE, report);
        result.renderer_retired = 1;
        result.native_releasable = 1;
        return result;
    }
    // `calloc` does not formally initialize C11 atomic objects. Initialize every atomic
    // before publishing the native pointer or selecting the real/fake backend.
    sp_telemetry_init(&output->telemetry);
    atomic_init(&output->sample_rate_listener.generation, 0);
    atomic_init(&output->frame_count_listener.generation, 0);
    atomic_init(&output->device_event, 0);
    atomic_init(&output->device_event_pending, 0);
    *output_result = output;

    output->frames = frames;
    output->renderer = renderer;
    output->render = render;
    output->observer_context = observer_context;
    output->lifecycle_observer = lifecycle_observer;
    output->retirement_observer = retirement_observer;
    if (fake_config != NULL && fake_config->enabled) {
        output->fake_backend = 1;
        output->fake = *fake_config;
        output->fake_report.sample_rate_hz = fake_config->sample_rate_hz;
        output->fake_report.channel_count = fake_config->channel_count;
        output->fake_report.current_frames_per_slice =
            fake_config->current_frames_per_slice;
        output->fake_report.supported_minimum_frames_per_slice =
            fake_config->supported_minimum_frames_per_slice;
        output->fake_report.supported_maximum_frames_per_slice =
            fake_config->supported_maximum_frames_per_slice;
        output->fake_report.maximum_callback_frames_per_slice =
            fake_config->maximum_callback_frames_per_slice;
        output->fake_report.uses_variable_buffer_frame_sizes =
            fake_config->uses_variable_buffer_frame_sizes;
        return sp_create_fake_output(output, allow_reconfiguration);
    }
    return sp_create_core_audio_output(output, allow_reconfiguration);
}

static void sp_fake_final_callback(SpAudioOutput *output) {
    float samples[512];
    struct {
        UInt32 mNumberBuffers;
        AudioBuffer mBuffers[1];
    } buffers = {
        .mNumberBuffers = 1,
        .mBuffers = {{
            .mNumberChannels = 2,
            .mDataByteSize =
                output->frames * 2u * (uint32_t)sizeof(float),
            .mData = samples,
        }},
    };
    (void)sp_render_callback(output, NULL, NULL, 0, output->frames,
                             (AudioBufferList *)&buffers);
}

static SpNativeFailure sp_wait_callbacks_quiesced(SpAudioOutput *output) {
    for (uint32_t poll = 0; poll < SP_QUIESCE_POLL_LIMIT; ++poll) {
        const uint32_t active = atomic_load_explicit(
            &output->telemetry.active_callbacks, memory_order_seq_cst);
        const uint32_t fake_pending =
            output->fake_backend &&
            output->fake.quiescence_convergence_polls != 0 &&
            (output->fake.quiescence_convergence_polls ==
                 SP_FAKE_NEVER_NOTIFIES ||
             poll < output->fake.quiescence_convergence_polls);
        if (active == 0 && !fake_pending) {
            return sp_native_ok();
        }
        if (!output->fake_backend) {
            sp_poll_delay();
        }
    }
    return sp_native_error(SP_AUDIO_STATUS_CALLBACK_QUIESCE_TIMEOUT,
                           SP_AUDIO_OPERATION_QUIESCE_CALLBACKS);
}

static SpAudioResult sp_destroy_fake_output(SpAudioOutput *output) {
    SpAudioResult result = sp_result_ok(output->fake_report);
    SpNativeFailure listener = sp_remove_listener(
        output, kAudioDevicePropertyNominalSampleRate, &output->sample_rate_listener,
        &output->sample_rate_listener_attached, SP_AUDIO_OPERATION_REMOVE_SAMPLE_RATE_LISTENER,
        SP_LIFECYCLE_REMOVE_SAMPLE_RATE_LISTENER);
    if (listener.status == 0) listener = sp_remove_listener(
        output, kAudioDevicePropertyBufferFrameSize, &output->frame_count_listener,
        &output->frame_count_listener_attached, SP_AUDIO_OPERATION_REMOVE_FRAME_COUNT_LISTENER,
        SP_LIFECYCLE_REMOVE_FRAME_COUNT_LISTENER);
    if (listener.status != 0) {
        result.listener_cleanup_status = listener.status;
        result.listener_cleanup_operation = listener.operation;
        result.status = listener.status;
        result.operation = listener.operation;
        result.renderer_retired = output->renderer_retired;
        result.native_releasable = sp_native_releasable(output);
        return result;
    }
    if (output->started) {
        if (output->fake.callback_on_stop) {
            output->fake.callback_on_stop = 0;
            sp_fake_final_callback(output);
        }
        if (sp_fake_teardown_fails(output, SP_AUDIO_OPERATION_STOP, &result)) {
            result.renderer_retired = output->renderer_retired;
            result.native_releasable = sp_native_releasable(output);
            return result;
        }
        output->started = 0;
        sp_record_lifecycle(output, SP_LIFECYCLE_STOP);
    }
    if (output->callback_attached) {
        if (sp_fake_teardown_fails(output, SP_AUDIO_OPERATION_DETACH_CALLBACK,
                               &result)) {
            result.renderer_retired = output->renderer_retired;
            result.native_releasable = sp_native_releasable(output);
            return result;
        }
        output->callback_attached = 0;
        sp_record_lifecycle(output, SP_LIFECYCLE_DETACH);
    }

    SpNativeFailure quiescence = sp_wait_callbacks_quiesced(output);
    if (quiescence.status != 0) {
        result = sp_result_error(quiescence.status, quiescence.operation,
                                 output->fake_report);
        result.renderer_retired = 0;
        result.native_releasable = 0;
        return result;
    }

    if (output->initialized) {
        if (sp_fake_teardown_fails(output, SP_AUDIO_OPERATION_UNINITIALIZE,
                               &result)) {
            result.renderer_retired = output->renderer_retired;
            result.native_releasable = sp_native_releasable(output);
            return result;
        }
        output->initialized = 0;
        sp_record_lifecycle(output, SP_LIFECYCLE_UNINITIALIZE);
    }
    if (output->unit_created) {
        if (sp_fake_teardown_fails(output, SP_AUDIO_OPERATION_DISPOSE, &result)) {
            result.renderer_retired = output->renderer_retired;
            result.native_releasable = 0;
            return result;
        }
        output->unit_created = 0;
        sp_record_lifecycle(output, SP_LIFECYCLE_DISPOSE);
    }
    sp_mark_renderer_retired(output);
    result.renderer_retired = 1;
    result.native_releasable = 1;
    return result;
}

static SpAudioResult sp_destroy_core_audio_output(SpAudioOutput *output) {
    OSStatus device_listener_status = sp_remove_device_change_listeners(output);
    if (device_listener_status != noErr) {
        SpAudioResult result = sp_result_error(device_listener_status,
                                               SP_AUDIO_OPERATION_REMOVE_DEVICE_CHANGE_LISTENER,
                                               (SpDeviceFormatReport){0});
        result.renderer_retired = 0;
        result.native_releasable = 0;
        return result;
    }
    SpDeviceFormatReport report = {0};
    if (output->device != kAudioObjectUnknown) {
        (void)sp_read_device_format(output->device, &report);
    }
    SpAudioResult result = sp_result_ok(report);
    SpNativeFailure listener = sp_remove_listener(
        output, kAudioDevicePropertyNominalSampleRate, &output->sample_rate_listener,
        &output->sample_rate_listener_attached, SP_AUDIO_OPERATION_REMOVE_SAMPLE_RATE_LISTENER,
        SP_LIFECYCLE_REMOVE_SAMPLE_RATE_LISTENER);
    if (listener.status == 0) listener = sp_remove_listener(
        output, kAudioDevicePropertyBufferFrameSize, &output->frame_count_listener,
        &output->frame_count_listener_attached, SP_AUDIO_OPERATION_REMOVE_FRAME_COUNT_LISTENER,
        SP_LIFECYCLE_REMOVE_FRAME_COUNT_LISTENER);
    if (listener.status != 0) {
        result.listener_cleanup_status = listener.status;
        result.listener_cleanup_operation = listener.operation;
        result.status = listener.status;
        result.operation = listener.operation;
        result.renderer_retired = output->renderer_retired;
        result.native_releasable = sp_native_releasable(output);
        return result;
    }

    if (output->started) {
        OSStatus status = AudioOutputUnitStop(output->unit);
        if (status != noErr) {
            result =
                sp_result_error(status, SP_AUDIO_OPERATION_STOP, report);
            result.renderer_retired = output->renderer_retired;
            result.native_releasable = sp_native_releasable(output);
            return result;
        }
        output->started = 0;
    }
    if (output->callback_attached) {
        AURenderCallbackStruct callback = {0};
        OSStatus status = AudioUnitSetProperty(
            output->unit, kAudioUnitProperty_SetRenderCallback,
            kAudioUnitScope_Input, 0, &callback, sizeof(callback));
        if (status != noErr) {
            result = sp_result_error(
                status, SP_AUDIO_OPERATION_DETACH_CALLBACK, report);
            result.renderer_retired = output->renderer_retired;
            result.native_releasable = sp_native_releasable(output);
            return result;
        }
        output->callback_attached = 0;
    }

    SpNativeFailure quiescence = sp_wait_callbacks_quiesced(output);
    if (quiescence.status != 0) {
        result = sp_result_error(quiescence.status, quiescence.operation,
                                 report);
        result.renderer_retired = 0;
        result.native_releasable = 0;
        return result;
    }

    if (output->initialized) {
        OSStatus status = AudioUnitUninitialize(output->unit);
        if (status != noErr) {
            result = sp_result_error(
                status, SP_AUDIO_OPERATION_UNINITIALIZE, report);
            result.renderer_retired = output->renderer_retired;
            result.native_releasable = sp_native_releasable(output);
            return result;
        }
        output->initialized = 0;
    }
    if (output->unit_created) {
        OSStatus status = AudioComponentInstanceDispose(output->unit);
        if (status != noErr) {
            result = sp_result_error(status, SP_AUDIO_OPERATION_DISPOSE,
                                     report);
            result.renderer_retired = output->renderer_retired;
            result.native_releasable = 0;
            return result;
        }
        output->unit = NULL;
        output->unit_created = 0;
    }
    if (output->aggregate_device != kAudioObjectUnknown) {
        OSStatus status = AudioHardwareDestroyAggregateDevice(output->aggregate_device);
        if (status != noErr) {
            result = sp_result_error(status, SP_AUDIO_OPERATION_DISPOSE, report);
            result.renderer_retired = 0;
            result.native_releasable = 0;
            return result;
        }
        output->aggregate_device = kAudioObjectUnknown;
    }
    sp_mark_renderer_retired(output);
    result.renderer_retired = 1;
    result.native_releasable = 1;
    return result;
}

static SpAudioResult sp_destroy_output(SpAudioOutput *output) {
    if (output->fake_backend) {
        return sp_destroy_fake_output(output);
    }
    return sp_destroy_core_audio_output(output);
}

SpAudioResult sp_audio_output_destroy(SpAudioOutput *output) {
    return sp_destroy_output(output);
}

void sp_audio_output_release(SpAudioOutput *output) {
    if (output != NULL && sp_native_releasable(output)) {
        free(output);
    }
}

SpCallbackTelemetry sp_audio_output_telemetry(const SpAudioOutput *output) {
    SpCallbackTelemetry snapshot = {0};
    if (output == NULL) {
        return snapshot;
    }

    for (;;) {
        const uint64_t before = atomic_load_explicit(
            &output->telemetry.sequence, memory_order_seq_cst);
        if (before & 1u) {
            continue;
        }
        snapshot.callbacks = atomic_load_explicit(
            &output->telemetry.callbacks, memory_order_seq_cst);
        snapshot.rendered = atomic_load_explicit(
            &output->telemetry.rendered, memory_order_seq_cst);
        snapshot.silenced = atomic_load_explicit(
            &output->telemetry.silenced, memory_order_seq_cst);
        snapshot.invalid_frames = atomic_load_explicit(
            &output->telemetry.invalid_frames, memory_order_seq_cst);
        snapshot.invalid_buffers = atomic_load_explicit(
            &output->telemetry.invalid_buffers, memory_order_seq_cst);
        snapshot.invalid_channels = atomic_load_explicit(
            &output->telemetry.invalid_channels, memory_order_seq_cst);
        snapshot.invalid_bytes = atomic_load_explicit(
            &output->telemetry.invalid_bytes, memory_order_seq_cst);
        snapshot.frame_histogram_32 = atomic_load_explicit(
            &output->telemetry.frame_histogram_32, memory_order_seq_cst);
        snapshot.frame_histogram_64 = atomic_load_explicit(
            &output->telemetry.frame_histogram_64, memory_order_seq_cst);
        snapshot.frame_histogram_128 = atomic_load_explicit(
            &output->telemetry.frame_histogram_128,
            memory_order_seq_cst);
        snapshot.frame_histogram_256 = atomic_load_explicit(
            &output->telemetry.frame_histogram_256,
            memory_order_seq_cst);
        const uint64_t after = atomic_load_explicit(
            &output->telemetry.sequence, memory_order_seq_cst);
        if (before == after && !(after & 1u)) {
            return snapshot;
        }
    }
}

int32_t sp_audio_test_invoke(SpAudioOutput *output, uint32_t frames,
                             uint32_t buffer_count, uint32_t channels,
                             uint32_t byte_size, float *data,
                             uint32_t data_capacity_bytes) {
    if (output == NULL || !output->fake_backend || buffer_count > 2 ||
        output->renderer_retired || output->renderer == NULL ||
        output->render == NULL) {
        return -1;
    }
    if (byte_size > data_capacity_bytes) {
        return -2;
    }

    struct {
        UInt32 mNumberBuffers;
        AudioBuffer mBuffers[2];
    } buffers = {0};
    buffers.mNumberBuffers = buffer_count;
    for (uint32_t index = 0; index < buffer_count; ++index) {
        buffers.mBuffers[index].mNumberChannels = channels;
        buffers.mBuffers[index].mDataByteSize = byte_size;
        buffers.mBuffers[index].mData = data;
    }
    return sp_render_callback(output, NULL, NULL, 0, frames,
                              (AudioBufferList *)&buffers);
}


typedef struct {
    uint32_t device_id;
    uint32_t input_channels;
    uint32_t output_channels;
    uint32_t is_default_input;
    uint32_t is_default_output;
    uint32_t supports_32_frames;
    uint32_t supports_64_frames;
    uint32_t supports_128_frames;
    uint32_t supports_256_frames;
    char name[256];
} SpEnumeratedDevice;

static uint32_t sp_channel_count(AudioDeviceID device, AudioObjectPropertyScope scope) {
    AudioObjectPropertyAddress address = {kAudioDevicePropertyStreamConfiguration, scope,
                                          kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    if (AudioObjectGetPropertyDataSize(device, &address, 0, NULL, &size) != noErr || size == 0) return 0;
    AudioBufferList *list = malloc(size);
    if (list == NULL) return 0;
    uint32_t channels = 0;
    if (AudioObjectGetPropertyData(device, &address, 0, NULL, &size, list) == noErr) {
        for (uint32_t i = 0; i < list->mNumberBuffers; ++i) channels += list->mBuffers[i].mNumberChannels;
    }
    free(list);
    return channels;
}

uint32_t sp_audio_device_count(void) {
    AudioObjectPropertyAddress address = {kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal,
                                          kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    if (AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &address, 0, NULL, &size) != noErr) return 0;
    return size / (uint32_t)sizeof(AudioDeviceID);
}

int32_t sp_audio_device_at(uint32_t index, SpEnumeratedDevice *result) {
    if (result == NULL) return kAudio_ParamError;
    memset(result, 0, sizeof(*result));
    AudioObjectPropertyAddress all = {kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal,
                                      kAudioObjectPropertyElementMain};
    UInt32 size = 0;
    OSStatus status = AudioObjectGetPropertyDataSize(kAudioObjectSystemObject, &all, 0, NULL, &size);
    if (status != noErr) return status;
    uint32_t count = size / (uint32_t)sizeof(AudioDeviceID);
    if (index >= count) return kAudio_ParamError;
    AudioDeviceID *devices = malloc(size);
    if (devices == NULL) return kAudio_MemFullError;
    status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &all, 0, NULL, &size, devices);
    if (status == noErr) {
        AudioDeviceID device = devices[index];
        result->device_id = device;
        result->input_channels = sp_channel_count(device, kAudioDevicePropertyScopeInput);
        result->output_channels = sp_channel_count(device, kAudioDevicePropertyScopeOutput);
        SpDeviceFormatReport report = {0};
        uint32_t supports_48000 = 0;
        if ((result->input_channels > 0 || result->output_channels > 0) &&
            sp_read_device_format(device, &report).status == 0 &&
            sp_supports_sample_rate(device, 48000.0, &supports_48000) == noErr &&
            supports_48000 && !report.uses_variable_buffer_frame_sizes) {
            result->supports_32_frames =
                report.supported_minimum_frames_per_slice <= 32 &&
                32 <= report.supported_maximum_frames_per_slice;
            result->supports_64_frames =
                report.supported_minimum_frames_per_slice <= 64 &&
                64 <= report.supported_maximum_frames_per_slice;
            result->supports_128_frames =
                report.supported_minimum_frames_per_slice <= 128 &&
                128 <= report.supported_maximum_frames_per_slice;
            result->supports_256_frames =
                report.supported_minimum_frames_per_slice <= 256 &&
                256 <= report.supported_maximum_frames_per_slice;
        }
        AudioObjectPropertyAddress name = {kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
                                           kAudioObjectPropertyElementMain};
        CFStringRef text = NULL;
        UInt32 name_size = sizeof(text);
        if (AudioObjectGetPropertyData(device, &name, 0, NULL, &name_size, &text) == noErr && text != NULL) {
            (void)CFStringGetCString(text, result->name, sizeof(result->name), kCFStringEncodingUTF8);
            CFRelease(text);
        }
        AudioDeviceID default_input = kAudioObjectUnknown, default_output = kAudioObjectUnknown;
        AudioObjectPropertyAddress input_default = {kAudioHardwarePropertyDefaultInputDevice, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        AudioObjectPropertyAddress output_default = {kAudioHardwarePropertyDefaultOutputDevice, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
        UInt32 id_size = sizeof(device);
        (void)AudioObjectGetPropertyData(kAudioObjectSystemObject, &input_default, 0, NULL, &id_size, &default_input);
        (void)AudioObjectGetPropertyData(kAudioObjectSystemObject, &output_default, 0, NULL, &id_size, &default_output);
        result->is_default_input = device == default_input;
        result->is_default_output = device == default_output;
    }
    free(devices);
    return status;
}

static OSStatus sp_device_change_listener(AudioObjectID object, UInt32 count,
                                          const AudioObjectPropertyAddress addresses[], void *context) {
    SpAudioOutput *output = context;
    uint32_t event = object == output->input_device &&
                     output->input_device != output->output_device ? 4 : 2;
    for (UInt32 i = 0; i < count; ++i) {
        if (addresses[i].mSelector == kAudioDevicePropertyDeviceIsAlive) event -= 1;
    }
    atomic_store_explicit(&output->device_event, event, memory_order_release);
    atomic_store_explicit(&output->device_event_pending, event, memory_order_release);
    return noErr;
}

static OSStatus sp_add_device_change_listeners(SpAudioOutput *output) {
    AudioObjectPropertyAddress addresses[] = {
        {kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
        {kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
        {kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
        {kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyScopeInput, kAudioObjectPropertyElementMain},
        {kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyScopeOutput, kAudioObjectPropertyElementMain},
    };
    AudioDeviceID devices[] = {output->device, output->output_device, output->input_device};
    for (uint32_t source = 0; source < 3; ++source) {
        if (devices[source] == kAudioObjectUnknown ||
            (source == 1 && devices[source] == devices[0]) ||
            (source == 2 && (devices[source] == devices[0] || devices[source] == devices[1]))) continue;
        for (uint32_t i = 0; i < 5; ++i) {
            OSStatus status = AudioObjectAddPropertyListener(devices[source], &addresses[i],
                                                              sp_device_change_listener, output);
            if (status != noErr) return status;
            output->device_listener_attached[source][i] = 1;
        }
    }
    return noErr;
}

static OSStatus sp_remove_device_change_listeners(SpAudioOutput *output) {
    AudioObjectPropertyAddress addresses[] = {
        {kAudioDevicePropertyDeviceIsAlive, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
        {kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
        {kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain},
        {kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyScopeInput, kAudioObjectPropertyElementMain},
        {kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyScopeOutput, kAudioObjectPropertyElementMain},
    };
    AudioDeviceID devices[] = {output->device, output->output_device, output->input_device};
    OSStatus first_error = noErr;
    for (uint32_t source = 0; source < 3; ++source) {
        for (uint32_t i = 0; i < 5; ++i) {
            if (output->device_listener_attached[source][i]) {
                OSStatus status = AudioObjectRemovePropertyListener(
                    devices[source], &addresses[i], sp_device_change_listener, output);
                if (status == noErr) output->device_listener_attached[source][i] = 0;
                else if (first_error == noErr) first_error = status;
            }
        }
    }
    return first_error;
}
uint32_t sp_audio_output_take_device_event(SpAudioOutput *output) {
    if (output == NULL) return 0;
    return atomic_exchange_explicit(&output->device_event_pending, 0, memory_order_acq_rel);
}

void sp_audio_test_signal_device_event(SpAudioOutput *output, uint32_t event) {
    if (output == NULL || !output->fake_backend || event < 1 || event > 4) return;
    atomic_store_explicit(&output->device_event, event, memory_order_release);
    atomic_store_explicit(&output->device_event_pending, event, memory_order_release);
}

static OSStatus sp_device_uid(AudioDeviceID device, CFStringRef *uid) {
    AudioObjectPropertyAddress address = {kAudioDevicePropertyDeviceUID,
                                          kAudioObjectPropertyScopeGlobal,
                                          kAudioObjectPropertyElementMain};
    UInt32 size = sizeof(*uid);
    *uid = NULL;
    return AudioObjectGetPropertyData(device, &address, 0, NULL, &size, uid);
}

static OSStatus sp_create_private_aggregate(AudioDeviceID input, AudioDeviceID playback,
                                            AudioDeviceID *aggregate) {
    CFStringRef input_uid = NULL, output_uid = NULL;
    OSStatus status = sp_device_uid(input, &input_uid);
    if (status != noErr) return status;
    status = sp_device_uid(playback, &output_uid);
    if (status != noErr) {
        CFRelease(input_uid);
        return status;
    }
    CFUUIDRef uuid = CFUUIDCreate(kCFAllocatorDefault);
    CFStringRef aggregate_uid = uuid != NULL ? CFUUIDCreateString(kCFAllocatorDefault, uuid) : NULL;
    if (uuid != NULL) CFRelease(uuid);
    CFMutableDictionaryRef description = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFMutableDictionaryRef output_entry = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    CFMutableDictionaryRef input_entry = CFDictionaryCreateMutable(
        kCFAllocatorDefault, 0, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    int32_t enabled = 1;
    CFNumberRef one = CFNumberCreate(kCFAllocatorDefault, kCFNumberSInt32Type, &enabled);
    if (aggregate_uid == NULL || description == NULL || output_entry == NULL ||
        input_entry == NULL || one == NULL) {
        status = kAudio_MemFullError;
    } else {
        // The output device leads the aggregate so its physical output channels start at zero.
        CFDictionarySetValue(output_entry, CFSTR(kAudioSubDeviceUIDKey), output_uid);
        CFDictionarySetValue(input_entry, CFSTR(kAudioSubDeviceUIDKey), input_uid);
        CFDictionarySetValue(input_entry, CFSTR(kAudioSubDeviceDriftCompensationKey),
                             one);
        const void *entries[] = {output_entry, input_entry};
        CFArrayRef subdevices = CFArrayCreate(kCFAllocatorDefault, entries, 2,
                                              &kCFTypeArrayCallBacks);
        if (subdevices == NULL) {
            status = kAudio_MemFullError;
        } else {
            CFDictionarySetValue(description, CFSTR(kAudioAggregateDeviceUIDKey), aggregate_uid);
            CFDictionarySetValue(description, CFSTR(kAudioAggregateDeviceNameKey),
                                 CFSTR("Superposition private I/O"));
            CFDictionarySetValue(description, CFSTR(kAudioAggregateDeviceSubDeviceListKey), subdevices);
            CFDictionarySetValue(description, CFSTR(kAudioAggregateDeviceMainSubDeviceKey), output_uid);
            CFDictionarySetValue(description, CFSTR(kAudioAggregateDeviceIsPrivateKey),
                                 one);
            status = AudioHardwareCreateAggregateDevice(description, aggregate);
            CFRelease(subdevices);
        }
    }
    if (input_entry != NULL) CFRelease(input_entry);
    if (output_entry != NULL) CFRelease(output_entry);
    if (description != NULL) CFRelease(description);
    if (one != NULL) CFRelease(one);
    if (aggregate_uid != NULL) CFRelease(aggregate_uid);
    CFRelease(output_uid);
    CFRelease(input_uid);
    return status;
}

SpAudioResult sp_audio_multichannel_create(SpAudioOutput **output_result,
                                            uint32_t input_device, uint32_t playback_device,
                                            uint32_t frames, uint32_t output_channels,
                                            uint8_t allow_reconfiguration, void *renderer,
                                            SpMultichannelRenderCallback render) {
    SpDeviceFormatReport report = {0};
    *output_result = NULL;
    if ((frames != 32 && frames != 64 && frames != 128 && frames != 256) ||
        output_channels == 0 || output_channels > 64 || renderer == NULL || render == NULL ||
        playback_device == kAudioObjectUnknown) {
        SpAudioResult result = sp_result_error(SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
            SP_AUDIO_OPERATION_VERIFY_DEVICE_FORMAT, report);
        result.renderer_retired = 1; result.native_releasable = 1; return result;
    }
    uint32_t available_output = sp_channel_count(playback_device, kAudioDevicePropertyScopeOutput);
    uint32_t available_input = input_device == kAudioObjectUnknown ? 0 :
        sp_channel_count(input_device, kAudioDevicePropertyScopeInput);
    if (available_output < output_channels ||
        (input_device != kAudioObjectUnknown && available_input == 0)) {
        SpAudioResult result = sp_result_error(SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
            SP_AUDIO_OPERATION_READ_STREAM_CONFIGURATION, report);
        result.renderer_retired = 1; result.native_releasable = 1; return result;
    }
    SpAudioOutput *output = calloc(1, sizeof(*output));
    if (output == NULL) {
        SpAudioResult result = sp_result_error(kAudio_MemFullError,
            SP_AUDIO_OPERATION_ALLOCATE_STATE, report);
        result.renderer_retired = 1; result.native_releasable = 1; return result;
    }
    sp_telemetry_init(&output->telemetry);
    atomic_init(&output->sample_rate_listener.generation, 0);
    atomic_init(&output->frame_count_listener.generation, 0);
    atomic_init(&output->device_event, 0);
    atomic_init(&output->device_event_pending, 0);
    output->frames = frames;
    output->renderer = renderer;
    output->multichannel_render = render;
    output->multichannel = 1;
    output->duplex = input_device != kAudioObjectUnknown;
    output->input_device = input_device;
    output->output_device = playback_device;
    output->input_channels = available_input < 64 ? available_input : 64;
    output->output_channels = output_channels;
    output->device = playback_device;
    if (input_device != kAudioObjectUnknown && input_device != playback_device) {
        OSStatus status = sp_create_private_aggregate(input_device, playback_device,
                                                       &output->aggregate_device);
        if (status != noErr) {
            free(output);
            SpAudioResult result = sp_result_error(status, SP_AUDIO_OPERATION_CREATE_UNIT, report);
            result.renderer_retired = 1; result.native_releasable = 1; return result;
        }
        output->device = output->aggregate_device;
        output->input_channel_offset = sp_channel_count(playback_device,
                                                         kAudioDevicePropertyScopeInput);
        uint32_t aggregate_output = 0, aggregate_input = 0;
        for (uint32_t attempt = 0; attempt < SP_PROPERTY_POLL_LIMIT; ++attempt) {
            aggregate_output = sp_channel_count(output->aggregate_device,
                                                 kAudioDevicePropertyScopeOutput);
            aggregate_input = sp_channel_count(output->aggregate_device,
                                                kAudioDevicePropertyScopeInput);
            if (aggregate_output >= output_channels &&
                aggregate_input >= output->input_channel_offset + output->input_channels) break;
            sp_poll_delay();
        }
        if (aggregate_output < output_channels ||
            aggregate_input < output->input_channel_offset + output->input_channels) {
            *output_result = output;
            return sp_finish_failed_create(output, sp_result_error(
                SP_AUDIO_STATUS_UNSUPPORTED_FORMAT,
                SP_AUDIO_OPERATION_READ_STREAM_CONFIGURATION, report));
        }
    }
    *output_result = output;
    return sp_create_core_audio_output(output, allow_reconfiguration);
}
