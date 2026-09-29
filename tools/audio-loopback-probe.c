// Short, headless stereo probe for a selected CoreAudio loopback device.
// Build: clang -std=c11 -O2 -Wall -Wextra -Werror tools/audio-loopback-probe.c \
//   -framework CoreAudio -framework AudioUnit -framework AudioToolbox \
//   -framework CoreFoundation -o /tmp/audio-loopback-probe
#define _POSIX_C_SOURCE 200809L

#include <AudioToolbox/AudioToolbox.h>
#include <AudioUnit/AudioUnit.h>
#include <CoreAudio/CoreAudio.h>
#include <CoreFoundation/CoreFoundation.h>
#include <math.h>
#include <stdatomic.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

enum { RATE = 48000, DEFAULT_SECONDS = 3, MAX_SECONDS = 30, CHANNELS = 2 };

typedef struct {
    AudioUnit unit;
    float *source;
    float *capture;
    uint32_t output_frame;
    atomic_uint active_callbacks;
    atomic_uint captured_frames;
    atomic_int render_error;
    atomic_uint oversized_callbacks;
    uint32_t max_frames;
    uint32_t total_frames;
    float *input_scratch;
} Probe;

typedef struct {
    double rms[CHANNELS];
    float peak[CHANNELS];
} Levels;

static void usage(const char *program) {
    fprintf(stderr,
            "Usage: %s --list\n"
            "       %s (--device-id ID | --device-name NAME) --frames 32|64|128|256 --prefix PATH\n"
            "          [--source-file PATH] [--duration-seconds 1..30] [--gain-db -96..0]\n"
            "          [--send-channels 1,2] [--capture-channels 3,4]\n"
            "Writes PATH.input.f32le and PATH.output.f32le (interleaved stereo, 48 kHz).\n"
            "Without --source-file, plays the original 3-second coded burst probe.\n"
            "File playback defaults to a peak no higher than 0.25 unless --gain-db is given.\n"
            "The selected BlackHole device must be at 48 kHz; its frame size is set if needed.\n",
            program, program);
}

static bool parse_channel_pair(const char *text, uint32_t pair[CHANNELS]) {
    char *end = NULL;
    unsigned long first = strtoul(text, &end, 10);
    if (end == text || *end != ',' || first == 0 || first > UINT32_MAX) return false;
    const char *second_text = end + 1;
    unsigned long second = strtoul(second_text, &end, 10);
    if (end == second_text || *end || second == 0 || second > UINT32_MAX ||
        first == second) return false;
    pair[0] = (uint32_t)first;
    pair[1] = (uint32_t)second;
    return true;
}

static bool get_devices(AudioDeviceID **devices, UInt32 *count) {
    AudioObjectPropertyAddress property = {
        kAudioHardwarePropertyDevices, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain};
    UInt32 bytes = 0;
    OSStatus status = AudioObjectGetPropertyDataSize(kAudioObjectSystemObject,
                                                      &property, 0, NULL, &bytes);
    if (status != noErr || bytes == 0 || bytes % sizeof(AudioDeviceID) != 0) {
        fprintf(stderr, "Cannot enumerate audio devices (OSStatus %d).\n", (int)status);
        return false;
    }
    AudioDeviceID *found = malloc(bytes);
    if (!found) return false;
    status = AudioObjectGetPropertyData(kAudioObjectSystemObject, &property, 0,
                                         NULL, &bytes, found);
    if (status != noErr) {
        fprintf(stderr, "Cannot read audio devices (OSStatus %d).\n", (int)status);
        free(found);
        return false;
    }
    *devices = found;
    *count = bytes / sizeof(AudioDeviceID);
    return true;
}

static bool device_name(AudioDeviceID device, char *name, size_t capacity) {
    AudioObjectPropertyAddress property = {
        kAudioObjectPropertyName, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain};
    CFStringRef string = NULL;
    UInt32 bytes = sizeof(string);
    if (AudioObjectGetPropertyData(device, &property, 0, NULL, &bytes, &string) != noErr ||
        string == NULL) return false;
    bool okay = CFStringGetCString(string, name, (CFIndex)capacity,
                                   kCFStringEncodingUTF8);
    CFRelease(string);
    return okay;
}

static bool read_property(AudioDeviceID device, AudioObjectPropertySelector selector,
                          void *value, UInt32 bytes) {
    AudioObjectPropertyAddress property = {
        selector, kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyElementMain};
    return AudioObjectGetPropertyData(device, &property, 0, NULL, &bytes, value) == noErr;
}

static double elapsed_seconds(struct timespec since);

static bool ensure_frame_size(AudioDeviceID device, UInt32 requested) {
    UInt32 current = 0;
    if (!read_property(device, kAudioDevicePropertyBufferFrameSize,
                       &current, sizeof(current))) {
        fprintf(stderr, "Cannot read BlackHole buffer frame size.\n");
        return false;
    }
    if (current == requested) return true;
    AudioObjectPropertyAddress property = {
        kAudioDevicePropertyBufferFrameSize, kAudioObjectPropertyScopeGlobal,
        kAudioObjectPropertyElementMain};
    OSStatus status = AudioObjectSetPropertyData(device, &property, 0, NULL,
                                                  sizeof(requested), &requested);
    if (status != noErr) {
        fprintf(stderr, "Cannot set BlackHole device %u from %u to %u frames "
                        "(OSStatus %d).\n",
                (unsigned)device, current, requested, (int)status);
        return false;
    }
    struct timespec start;
    clock_gettime(CLOCK_MONOTONIC, &start);
    do {
        if (read_property(device, kAudioDevicePropertyBufferFrameSize,
                          &current, sizeof(current)) && current == requested) {
            printf("BlackHole device %u now reports %u frames/callback.\n",
                   (unsigned)device, current);
            return true;
        }
        struct timespec pause = {.tv_sec = 0, .tv_nsec = 10000000};
        nanosleep(&pause, NULL);
    } while (elapsed_seconds(start) < 2.0);
    fprintf(stderr, "BlackHole device %u did not report requested %u frames "
                    "within 2 seconds (last read %u).\n",
            (unsigned)device, requested, current);
    return false;
}

static uint32_t channel_count(AudioDeviceID device, AudioObjectPropertyScope scope) {
    AudioObjectPropertyAddress property = {
        kAudioDevicePropertyStreamConfiguration, scope, kAudioObjectPropertyElementMain};
    UInt32 bytes = 0;
    if (AudioObjectGetPropertyDataSize(device, &property, 0, NULL, &bytes) != noErr ||
        bytes == 0) return 0;
    AudioBufferList *buffers = malloc(bytes);
    if (!buffers) return 0;
    uint32_t count = 0;
    if (AudioObjectGetPropertyData(device, &property, 0, NULL, &bytes, buffers) == noErr) {
        for (UInt32 index = 0; index < buffers->mNumberBuffers; ++index)
            count += buffers->mBuffers[index].mNumberChannels;
    }
    free(buffers);
    return count;
}

static OSStatus callback(void *context, AudioUnitRenderActionFlags *flags,
                         const AudioTimeStamp *time, UInt32 bus, UInt32 frames,
                         AudioBufferList *output) {
    (void)bus;
    Probe *probe = context;
    atomic_fetch_add(&probe->active_callbacks, 1);
    OSStatus result = noErr;
    if (output == NULL || output->mNumberBuffers != 1 ||
        output->mBuffers[0].mData == NULL ||
        output->mBuffers[0].mDataByteSize < frames * CHANNELS * sizeof(float)) {
        atomic_store(&probe->render_error, kAudio_ParamError);
        result = kAudio_ParamError;
        goto done;
    }

    float *samples = output->mBuffers[0].mData;
    memset(samples, 0, frames * CHANNELS * sizeof(float));
    if (frames > probe->max_frames) {
        atomic_fetch_add(&probe->oversized_callbacks, 1);
        goto done;
    }

    uint32_t available = probe->output_frame < probe->total_frames
                             ? probe->total_frames - probe->output_frame : 0;
    uint32_t source_frames = frames < available ? frames : available;
    if (source_frames)
        memcpy(samples, probe->source + probe->output_frame * CHANNELS,
               source_frames * CHANNELS * sizeof(float));
    probe->output_frame += source_frames;

    AudioBufferList input = {
        .mNumberBuffers = 1,
        .mBuffers = {{.mNumberChannels = CHANNELS,
                      .mDataByteSize = frames * CHANNELS * sizeof(float),
                      .mData = probe->input_scratch}},
    };
    OSStatus status = AudioUnitRender(probe->unit, flags, time, 1, frames, &input);
    if (status != noErr) {
        atomic_store(&probe->render_error, status);
        goto done;
    }
    uint32_t captured = atomic_load(&probe->captured_frames);
    uint32_t capacity = captured < probe->total_frames
                            ? probe->total_frames - captured : 0;
    uint32_t copy_frames = frames < capacity ? frames : capacity;
    if (copy_frames) {
        memcpy(probe->capture + captured * CHANNELS, probe->input_scratch,
               copy_frames * CHANNELS * sizeof(float));
        atomic_store(&probe->captured_frames, captured + copy_frames);
    }
done:
    atomic_fetch_sub(&probe->active_callbacks, 1);
    return result;
}

static void make_stimulus(float *source) {
    // Two short, deterministic noise bursts, then more than two seconds of silence.
    // The separate left/right bursts make channel swaps and delayed returns visible.
    const uint32_t starts[CHANNELS] = {RATE / 4, RATE / 2};
    const uint32_t burst_frames = 480;
    uint32_t state = 0x3e6f57a1u;
    for (uint32_t channel = 0; channel < CHANNELS; ++channel) {
        for (uint32_t i = 0; i < burst_frames; ++i) {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            double window = 0.5 - 0.5 * cos(2.0 * M_PI * i / (burst_frames - 1));
            source[(starts[channel] + i) * CHANNELS + channel] =
                (state & 1u ? 0.025f : -0.025f) * (float)window;
        }
    }
}

static bool decode_source(const char *path, float *samples, uint32_t frames,
                          bool explicit_gain, double gain_db) {
    CFURLRef url = CFURLCreateFromFileSystemRepresentation(
        kCFAllocatorDefault, (const UInt8 *)path, (CFIndex)strlen(path), false);
    if (!url) { fprintf(stderr, "Cannot make URL for source file.\n"); return false; }
    ExtAudioFileRef file = NULL;
    OSStatus status = ExtAudioFileOpenURL(url, &file);
    CFRelease(url);
    if (status != noErr) {
        fprintf(stderr, "Cannot open source file %s (OSStatus %d).\n", path, (int)status);
        return false;
    }
    AudioStreamBasicDescription format = {
        .mSampleRate = RATE,
        .mFormatID = kAudioFormatLinearPCM,
        .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        .mBytesPerPacket = CHANNELS * sizeof(float),
        .mFramesPerPacket = 1,
        .mBytesPerFrame = CHANNELS * sizeof(float),
        .mChannelsPerFrame = CHANNELS,
        .mBitsPerChannel = 8 * sizeof(float),
    };
    status = ExtAudioFileSetProperty(file, kExtAudioFileProperty_ClientDataFormat,
                                     sizeof(format), &format);
    if (status != noErr) {
        fprintf(stderr, "Cannot decode source as 48 kHz stereo float (OSStatus %d).\n",
                (int)status);
        ExtAudioFileDispose(file);
        return false;
    }
    uint32_t decoded = 0;
    while (decoded < frames) {
        UInt32 chunk = frames - decoded;
        if (chunk > 4096) chunk = 4096;
        AudioBufferList buffer = {
            .mNumberBuffers = 1,
            .mBuffers = {{.mNumberChannels = CHANNELS,
                          .mDataByteSize = chunk * CHANNELS * sizeof(float),
                          .mData = samples + decoded * CHANNELS}},
        };
        status = ExtAudioFileRead(file, &chunk, &buffer);
        if (status != noErr) {
            fprintf(stderr, "Source decode failed (OSStatus %d).\n", (int)status);
            ExtAudioFileDispose(file);
            return false;
        }
        if (!chunk) break;
        decoded += chunk;
    }
    status = ExtAudioFileDispose(file);
    if (status != noErr) {
        fprintf(stderr, "Source decoder disposal failed (OSStatus %d).\n", (int)status);
        return false;
    }
    if (!decoded) { fprintf(stderr, "Source file contains no audio.\n"); return false; }
    float peak = 0;
    for (uint32_t i = 0; i < decoded * CHANNELS; ++i) {
        if (!isfinite(samples[i])) { fprintf(stderr, "Source contains non-finite audio.\n"); return false; }
        float value = fabsf(samples[i]);
        if (value > peak) peak = value;
    }
    double gain = explicit_gain ? pow(10.0, gain_db / 20.0)
                                : (peak > 0.25f ? 0.25 / peak : 1.0);
    for (uint32_t i = 0; i < decoded * CHANNELS; ++i)
        samples[i] = (float)(samples[i] * gain);
    printf("Decoded %u source frames from %s; source peak %.6f, gain %.2f dB, "
           "playback peak %.6f.\n", decoded, path, peak, 20.0 * log10(gain),
           peak * gain);
    if (decoded < frames)
        printf("Source ended early; padding %u frames with silence.\n", frames - decoded);
    return true;
}

static Levels levels(const float *samples, uint32_t first, uint32_t end) {
    Levels result = {0};
    for (uint32_t channel = 0; channel < CHANNELS; ++channel) {
        double sum = 0;
        for (uint32_t frame = first; frame < end; ++frame) {
            float sample = samples[frame * CHANNELS + channel];
            float absolute = fabsf(sample);
            if (absolute > result.peak[channel]) result.peak[channel] = absolute;
            sum += (double)sample * sample;
        }
        if (end > first) result.rms[channel] = sqrt(sum / (end - first));
    }
    return result;
}

static bool write_samples(const char *prefix, const char *suffix,
                          const float *samples, uint32_t frames) {
    size_t bytes = strlen(prefix) + strlen(suffix) + 1;
    char *path = malloc(bytes);
    if (!path) return false;
    snprintf(path, bytes, "%s%s", prefix, suffix);
    FILE *file = fopen(path, "wb");
    if (!file) {
        perror(path);
        free(path);
        return false;
    }
    bool okay = fwrite(samples, sizeof(float) * CHANNELS, frames, file) == frames;
    if (fclose(file) != 0) okay = false;
    if (okay) printf("Wrote %s (%u stereo frames)\n", path, frames);
    else fprintf(stderr, "Could not finish writing %s.\n", path);
    free(path);
    return okay;
}

static bool configure_unit(Probe *probe, AudioDeviceID device, uint32_t physical_outputs,
                           const uint32_t send[CHANNELS],
                           const uint32_t capture[CHANNELS], uint32_t frames) {
    AudioComponentDescription description = {
        .componentType = kAudioUnitType_Output,
        .componentSubType = kAudioUnitSubType_HALOutput,
        .componentManufacturer = kAudioUnitManufacturer_Apple,
    };
    AudioComponent component = AudioComponentFindNext(NULL, &description);
    if (!component) { fprintf(stderr, "AUHAL is unavailable.\n"); return false; }
    OSStatus status = AudioComponentInstanceNew(component, &probe->unit);
    if (status != noErr) goto fail;

#define SET_PROPERTY(property, scope, bus, value) do { \
    status = AudioUnitSetProperty(probe->unit, property, scope, bus, \
                                  &(value), sizeof(value)); \
    if (status != noErr) goto fail; \
} while (0)
    UInt32 enabled = 1;
    SET_PROPERTY(kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Input, 1, enabled);
    SET_PROPERTY(kAudioOutputUnitProperty_EnableIO, kAudioUnitScope_Output, 0, enabled);
    SET_PROPERTY(kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0, device);

    AudioStreamBasicDescription format = {
        .mSampleRate = RATE,
        .mFormatID = kAudioFormatLinearPCM,
        .mFormatFlags = kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
        .mBytesPerPacket = CHANNELS * sizeof(float),
        .mFramesPerPacket = 1,
        .mBytesPerFrame = CHANNELS * sizeof(float),
        .mChannelsPerFrame = CHANNELS,
        .mBitsPerChannel = 8 * sizeof(float),
    };
    SET_PROPERTY(kAudioUnitProperty_StreamFormat, kAudioUnitScope_Input, 0, format);
    SET_PROPERTY(kAudioUnitProperty_StreamFormat, kAudioUnitScope_Output, 1, format);

    SInt32 *output_map = malloc(physical_outputs * sizeof(SInt32));
    if (!output_map) { status = kAudio_MemFullError; goto fail; }
    for (uint32_t i = 0; i < physical_outputs; ++i)
        output_map[i] = i == send[0] - 1 ? 0 : (i == send[1] - 1 ? 1 : -1);
    status = AudioUnitSetProperty(probe->unit, kAudioOutputUnitProperty_ChannelMap,
                                  kAudioUnitScope_Input, 0, output_map,
                                  physical_outputs * sizeof(SInt32));
    free(output_map);
    if (status != noErr) goto fail;
    SInt32 input_map[CHANNELS] = {(SInt32)(capture[0] - 1),
                                  (SInt32)(capture[1] - 1)};
    status = AudioUnitSetProperty(probe->unit, kAudioOutputUnitProperty_ChannelMap,
                                  kAudioUnitScope_Output, 1, input_map,
                                  sizeof(input_map));
    if (status != noErr) goto fail;

    SET_PROPERTY(kAudioUnitProperty_MaximumFramesPerSlice,
                 kAudioUnitScope_Global, 0, frames);
    AURenderCallbackStruct render = {.inputProc = callback, .inputProcRefCon = probe};
    SET_PROPERTY(kAudioUnitProperty_SetRenderCallback, kAudioUnitScope_Input, 0, render);
#undef SET_PROPERTY
    status = AudioUnitInitialize(probe->unit);
    if (status != noErr) goto fail;
    return true;

fail:
    fprintf(stderr, "AUHAL setup failed (OSStatus %d).\n", (int)status);
    return false;
}

static double elapsed_seconds(struct timespec since) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return (now.tv_sec - since.tv_sec) + (now.tv_nsec - since.tv_nsec) / 1e9;
}

int main(int argc, char **argv) {
    const char *selected_name = NULL;
    const char *prefix = NULL;
    const char *source_file = NULL;
    AudioDeviceID selected_id = kAudioObjectUnknown;
    uint32_t frames = 0;
    uint32_t duration = DEFAULT_SECONDS;
    uint32_t send_channels[CHANNELS] = {1, 2};
    uint32_t capture_channels[CHANNELS] = {1, 2};
    bool explicit_gain = false;
    double gain_db = 0;
    bool list = false;
    for (int i = 1; i < argc; ++i) {
        if (strcmp(argv[i], "--list") == 0) list = true;
        else if (strcmp(argv[i], "--device-name") == 0 && i + 1 < argc)
            selected_name = argv[++i];
        else if (strcmp(argv[i], "--device-id") == 0 && i + 1 < argc) {
            char *end = NULL;
            unsigned long value = strtoul(argv[++i], &end, 10);
            if (!argv[i][0] || *end || value == 0 || value > UINT32_MAX) {
                usage(argv[0]); return 2;
            }
            selected_id = (AudioDeviceID)value;
        } else if (strcmp(argv[i], "--frames") == 0 && i + 1 < argc) {
            char *end = NULL;
            unsigned long value = strtoul(argv[++i], &end, 10);
            if (*end || (value != 32 && value != 64 && value != 128 && value != 256)) {
                usage(argv[0]); return 2;
            }
            frames = (uint32_t)value;
        } else if (strcmp(argv[i], "--prefix") == 0 && i + 1 < argc)
            prefix = argv[++i];
        else if (strcmp(argv[i], "--source-file") == 0 && i + 1 < argc)
            source_file = argv[++i];
        else if (strcmp(argv[i], "--duration-seconds") == 0 && i + 1 < argc) {
            char *end = NULL;
            unsigned long value = strtoul(argv[++i], &end, 10);
            if (!argv[i][0] || *end || value < 1 || value > MAX_SECONDS) {
                usage(argv[0]); return 2;
            }
            duration = (uint32_t)value;
        } else if (strcmp(argv[i], "--send-channels") == 0 && i + 1 < argc) {
            if (!parse_channel_pair(argv[++i], send_channels)) { usage(argv[0]); return 2; }
        } else if (strcmp(argv[i], "--capture-channels") == 0 && i + 1 < argc) {
            if (!parse_channel_pair(argv[++i], capture_channels)) { usage(argv[0]); return 2; }
        } else if (strcmp(argv[i], "--gain-db") == 0 && i + 1 < argc) {
            char *end = NULL;
            gain_db = strtod(argv[++i], &end);
            if (!argv[i][0] || *end || !isfinite(gain_db) ||
                gain_db < -96.0 || gain_db > 0.0) { usage(argv[0]); return 2; }
            explicit_gain = true;
        }
        else { usage(argv[0]); return 2; }
    }
    if (!list && ((!selected_name && selected_id == kAudioObjectUnknown) ||
                  (selected_name && selected_id != kAudioObjectUnknown) ||
                  !prefix || !*prefix || !frames ||
                  (source_file && !*source_file) || (explicit_gain && !source_file) ||
                  (!source_file && duration != DEFAULT_SECONDS))) {
        usage(argv[0]); return 2;
    }

    AudioDeviceID *devices = NULL;
    UInt32 count = 0;
    if (!get_devices(&devices, &count)) return 1;
    AudioDeviceID device = kAudioObjectUnknown;
    char resolved_name[256] = "";
    unsigned matches = 0;
    for (UInt32 i = 0; i < count; ++i) {
        char name[256] = "(unnamed)";
        (void)device_name(devices[i], name, sizeof(name));
        Float64 rate = 0;
        UInt32 frame_size = 0;
        (void)read_property(devices[i], kAudioDevicePropertyNominalSampleRate,
                            &rate, sizeof(rate));
        (void)read_property(devices[i], kAudioDevicePropertyBufferFrameSize,
                            &frame_size, sizeof(frame_size));
        uint32_t inputs = channel_count(devices[i], kAudioDevicePropertyScopeInput);
        uint32_t outputs = channel_count(devices[i], kAudioDevicePropertyScopeOutput);
        if (list)
            printf("%u  %s  %.0f Hz  %u frames  in:%u out:%u\n",
                   (unsigned)devices[i], name, rate, frame_size, inputs, outputs);
        if ((!selected_name && devices[i] == selected_id) ||
            (selected_name && strcmp(name, selected_name) == 0)) {
            device = devices[i];
            snprintf(resolved_name, sizeof(resolved_name), "%s", name);
            ++matches;
        }
    }
    free(devices);
    if (list) return 0;
    if (matches != 1) {
        fprintf(stderr, "Device selector matched %u devices; use --list and an exact ID.\n", matches);
        return 1;
    }
    if (strstr(resolved_name, "BlackHole") == NULL) {
        fprintf(stderr, "Device %u (%s) is not a BlackHole loopback device.\n",
                (unsigned)device, resolved_name);
        return 1;
    }
    Float64 rate = 0;
    UInt32 frame_size = 0;
    UInt32 alive = 0;
    uint32_t inputs = channel_count(device, kAudioDevicePropertyScopeInput);
    uint32_t outputs = channel_count(device, kAudioDevicePropertyScopeOutput);
    if (!read_property(device, kAudioDevicePropertyNominalSampleRate, &rate, sizeof(rate)) ||
        !read_property(device, kAudioDevicePropertyBufferFrameSize,
                       &frame_size, sizeof(frame_size)) ||
        !read_property(device, kAudioDevicePropertyDeviceIsAlive, &alive, sizeof(alive)) ||
        !alive || rate != RATE || inputs < capture_channels[0] ||
        inputs < capture_channels[1] || outputs < send_channels[0] ||
        outputs < send_channels[1]) {
        fprintf(stderr, "Device %u must be alive, duplex stereo, and at 48000 Hz "
                        "(found %.0f Hz, %u frames, in:%u out:%u; "
                        "requested send:%u,%u capture:%u,%u).\n",
                (unsigned)device, rate, frame_size, inputs, outputs,
                send_channels[0], send_channels[1],
                capture_channels[0], capture_channels[1]);
        return 1;
    }

    uint32_t total_frames = RATE * duration;
    Probe probe = {.max_frames = frames, .total_frames = total_frames};
    probe.source = calloc(total_frames * CHANNELS, sizeof(float));
    probe.capture = calloc(total_frames * CHANNELS, sizeof(float));
    probe.input_scratch = calloc(frames * CHANNELS, sizeof(float));
    if (!probe.source || !probe.capture || !probe.input_scratch) {
        fprintf(stderr, "Cannot allocate probe buffers.\n");
        free(probe.source); free(probe.capture); free(probe.input_scratch);
        return 1;
    }
    bool source_okay = source_file
        ? decode_source(source_file, probe.source, total_frames, explicit_gain, gain_db)
        : (make_stimulus(probe.source), true);
    if (!source_okay) {
        free(probe.source); free(probe.capture); free(probe.input_scratch);
        return 1;
    }
    if (!ensure_frame_size(device, frames) ||
        !read_property(device, kAudioDevicePropertyNominalSampleRate, &rate, sizeof(rate)) ||
        rate != RATE) {
        fprintf(stderr, "BlackHole device format verification failed.\n");
        free(probe.source); free(probe.capture); free(probe.input_scratch);
        return 1;
    }
    printf("Routing send %u,%u -> capture %u,%u.\n",
           send_channels[0], send_channels[1], capture_channels[0], capture_channels[1]);
    bool initialized = configure_unit(&probe, device, outputs, send_channels,
                                      capture_channels, frames);
    bool started = false;
    bool cleanup_okay = true;
    bool safe_to_free = true;
    if (initialized) {
        OSStatus status = AudioOutputUnitStart(probe.unit);
        if (status != noErr)
            fprintf(stderr, "Could not start AUHAL (OSStatus %d).\n", (int)status);
        else started = true;
    }
    if (started) {
        struct timespec start;
        clock_gettime(CLOCK_MONOTONIC, &start);
        while (atomic_load(&probe.captured_frames) < total_frames &&
               atomic_load(&probe.render_error) == 0 &&
               elapsed_seconds(start) < duration + 2.0) {
            struct timespec pause = {.tv_sec = 0, .tv_nsec = 10000000};
            nanosleep(&pause, NULL);
        }
        OSStatus status = AudioOutputUnitStop(probe.unit);
        if (status != noErr) {
            fprintf(stderr, "Could not stop AUHAL (OSStatus %d).\n", (int)status);
            cleanup_okay = false;
        }
        struct timespec quiesce;
        clock_gettime(CLOCK_MONOTONIC, &quiesce);
        while (atomic_load(&probe.active_callbacks) != 0 &&
               elapsed_seconds(quiesce) < 1.0) {
            struct timespec pause = {.tv_sec = 0, .tv_nsec = 1000000};
            nanosleep(&pause, NULL);
        }
        if (atomic_load(&probe.active_callbacks) != 0) {
            fprintf(stderr, "AUHAL callback did not quiesce.\n");
            cleanup_okay = false;
        }
    }
    if (probe.unit) {
        if (initialized) {
            OSStatus status = AudioUnitUninitialize(probe.unit);
            if (status != noErr) {
                fprintf(stderr, "AUHAL uninitialize failed (OSStatus %d).\n", (int)status);
                cleanup_okay = false;
            }
        }
        OSStatus status = AudioComponentInstanceDispose(probe.unit);
        if (status != noErr) {
            fprintf(stderr, "AUHAL dispose failed (OSStatus %d).\n", (int)status);
            cleanup_okay = false;
            safe_to_free = false;
        }
    }

    if (atomic_load(&probe.active_callbacks) != 0) safe_to_free = false;
    if (!safe_to_free) {
        fprintf(stderr, "AUHAL teardown is incomplete; exiting before callback storage expires.\n");
        fflush(stderr);
        _Exit(1);
    }
    uint32_t captured = atomic_load(&probe.captured_frames);
    uint32_t transmitted = probe.output_frame;
    bool okay = started && cleanup_okay && captured == total_frames &&
                transmitted == total_frames &&
                atomic_load(&probe.render_error) == 0 &&
                atomic_load(&probe.oversized_callbacks) == 0;
    if (started) {
        printf("Device %u, 48000 Hz, %u frames/callback; transmitted %u, captured %u frames.\n",
               (unsigned)device, frames, transmitted, captured);
        printf("Render error: %d; oversized callbacks: %u.\n",
               atomic_load(&probe.render_error), atomic_load(&probe.oversized_callbacks));
        if (source_file) {
            Levels source_levels = levels(probe.source, 0, transmitted);
            Levels captured_levels = levels(probe.capture, 0, captured);
            printf("Source L/R RMS %.6f/%.6f, peak %.6f/%.6f.\n",
                   source_levels.rms[0], source_levels.rms[1],
                   source_levels.peak[0], source_levels.peak[1]);
            printf("Capture L/R RMS %.6f/%.6f, peak %.6f/%.6f.\n",
                   captured_levels.rms[0], captured_levels.rms[1],
                   captured_levels.peak[0], captured_levels.peak[1]);
        } else {
            Levels before = levels(probe.capture, 0, captured < RATE / 5 ? captured : RATE / 5);
            Levels response = levels(probe.capture, captured < RATE / 5 ? captured : RATE / 5,
                                     captured < RATE * 3 / 4 ? captured : RATE * 3 / 4);
            Levels tail = levels(probe.capture, captured < RATE * 3 / 4 ? captured : RATE * 3 / 4,
                                 captured);
            printf("Capture levels (L/R RMS, peak):\n");
            printf("  pre-burst 0-.2s:     %.6f/%.6f, %.6f/%.6f\n",
                   before.rms[0], before.rms[1], before.peak[0], before.peak[1]);
            printf("  bursts .2-.75s:      %.6f/%.6f, %.6f/%.6f\n",
                   response.rms[0], response.rms[1], response.peak[0], response.peak[1]);
            printf("  silence .75-3s:      %.6f/%.6f, %.6f/%.6f\n",
                   tail.rms[0], tail.rms[1], tail.peak[0], tail.peak[1]);
            printf("Stimulus: 10 ms coded bursts at .25s left and .50s right, peak 0.025; "
                   "silence otherwise.\n");
        }
        if (!write_samples(prefix, ".input.f32le", probe.capture, captured)) okay = false;
        if (!write_samples(prefix, ".output.f32le", probe.source, transmitted)) okay = false;
    }
    free(probe.source);
    free(probe.capture);
    free(probe.input_scratch);
    return okay ? 0 : 1;
}
