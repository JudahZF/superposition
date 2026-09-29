#include <errno.h>
#include <mach/mach.h>
#include <mach/mach_time.h>
#include <mach/thread_policy.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdlib.h>

typedef struct {
    thread_act_t thread;
    thread_extended_policy_data_t original_extended;
    uint32_t active_frames;
    uint32_t active_sample_rate;
    bool active;
} SpAudioThreadPolicy;

static int sp_mach_error(kern_return_t status) {
    return status == KERN_SUCCESS ? 0 : EIO;
}

int superposition_audio_thread_policy_leave(void *opaque);

int superposition_audio_thread_policy_new(void **out) {
    if (out == NULL) {
        return EINVAL;
    }
    *out = NULL;
    SpAudioThreadPolicy *policy = calloc(1, sizeof(*policy));
    if (policy == NULL) {
        return ENOMEM;
    }
    policy->thread = mach_thread_self();
    if (policy->thread == MACH_PORT_NULL) {
        free(policy);
        return EIO;
    }
    mach_msg_type_number_t count = THREAD_EXTENDED_POLICY_COUNT;
    boolean_t default_policy = FALSE;
    kern_return_t status = thread_policy_get(
        policy->thread, THREAD_EXTENDED_POLICY,
        (thread_policy_t)&policy->original_extended, &count, &default_policy);
    if (status != KERN_SUCCESS || count != THREAD_EXTENDED_POLICY_COUNT) {
        mach_port_deallocate(mach_task_self(), policy->thread);
        free(policy);
        return EIO;
    }
    // A preexisting real-time constraint cannot be restored by THREAD_STANDARD_POLICY.
    thread_time_constraint_policy_data_t existing_constraint;
    count = THREAD_TIME_CONSTRAINT_POLICY_COUNT;
    default_policy = FALSE;
    status = thread_policy_get(policy->thread, THREAD_TIME_CONSTRAINT_POLICY,
                               (thread_policy_t)&existing_constraint, &count,
                               &default_policy);
    if (status != KERN_SUCCESS || count != THREAD_TIME_CONSTRAINT_POLICY_COUNT) {
        mach_port_deallocate(mach_task_self(), policy->thread);
        free(policy);
        return EIO;
    }
    if (!default_policy) {
        mach_port_deallocate(mach_task_self(), policy->thread);
        free(policy);
        return EINVAL;
    }
    *out = policy;
    return 0;
}

int superposition_audio_thread_policy_enter(void *opaque, uint32_t frames,
                                            uint32_t sample_rate) {
    SpAudioThreadPolicy *policy = opaque;
    if (policy == NULL || (frames != 32 && frames != 64 && frames != 128 && frames != 256) ||
        sample_rate != 48000) {
        return EINVAL;
    }
    if (policy->active && policy->active_frames == frames &&
        policy->active_sample_rate == sample_rate) {
        return 0;
    }

    mach_timebase_info_data_t timebase;
    kern_return_t status = mach_timebase_info(&timebase);
    if (status != KERN_SUCCESS || timebase.numer == 0 || timebase.denom == 0) {
        return EIO;
    }
    // Convert the exact frame period to absolute Mach ticks, rounding upward.
    const __uint128_t numerator = (__uint128_t)frames * 1000000000ULL * timebase.denom;
    const __uint128_t denominator = (__uint128_t)sample_rate * timebase.numer;
    const __uint128_t period_ticks = (numerator + denominator - 1) / denominator;
    if (period_ticks < 4 || period_ticks > UINT32_MAX) {
        return ERANGE;
    }
    const uint32_t period = (uint32_t)period_ticks;
    const uint32_t computation = (uint32_t)((period_ticks + 3) / 4);
    const uint32_t constraint = (uint32_t)((period_ticks + 1) / 2);
    if (computation == 0 || computation > constraint || constraint > period) {
        return ERANGE;
    }
    thread_time_constraint_policy_data_t requested = {
        .period = period,
        .computation = computation,
        .constraint = constraint,
        .preemptible = TRUE,
    };
    status = thread_policy_set(policy->thread, THREAD_TIME_CONSTRAINT_POLICY,
                               (thread_policy_t)&requested,
                               THREAD_TIME_CONSTRAINT_POLICY_COUNT);
    if (status != KERN_SUCCESS) {
        return sp_mach_error(status);
    }
    policy->active = true;
    policy->active_frames = frames;
    policy->active_sample_rate = sample_rate;
    thread_time_constraint_policy_data_t effective;
    mach_msg_type_number_t count = THREAD_TIME_CONSTRAINT_POLICY_COUNT;
    boolean_t default_policy = FALSE;
    status = thread_policy_get(policy->thread, THREAD_TIME_CONSTRAINT_POLICY,
                               (thread_policy_t)&effective, &count, &default_policy);
    if (status != KERN_SUCCESS || count != THREAD_TIME_CONSTRAINT_POLICY_COUNT ||
        default_policy) {
        (void)superposition_audio_thread_policy_leave(policy);
        return EIO;
    }
    return 0;
}

int superposition_audio_thread_policy_leave(void *opaque) {
    SpAudioThreadPolicy *policy = opaque;
    if (policy == NULL) {
        return EINVAL;
    }
    if (!policy->active) {
        return 0;
    }
    kern_return_t status = thread_policy_set(policy->thread, THREAD_STANDARD_POLICY,
                                               NULL, THREAD_STANDARD_POLICY_COUNT);
    if (status != KERN_SUCCESS) {
        return sp_mach_error(status);
    }
    policy->active = false;
    policy->active_frames = 0;
    policy->active_sample_rate = 0;
    if (!policy->original_extended.timeshare) {
        status = thread_policy_set(policy->thread, THREAD_EXTENDED_POLICY,
                                   (thread_policy_t)&policy->original_extended,
                                   THREAD_EXTENDED_POLICY_COUNT);
        if (status != KERN_SUCCESS) {
            return sp_mach_error(status);
        }
    }
    return 0;
}

void superposition_audio_thread_policy_destroy(void *opaque) {
    SpAudioThreadPolicy *policy = opaque;
    if (policy == NULL) {
        return;
    }
    (void)superposition_audio_thread_policy_leave(policy);
    mach_port_deallocate(mach_task_self(), policy->thread);
    free(policy);
}
