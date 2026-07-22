#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct sp_vst3_native sp_vst3_native;

typedef struct {
  uint8_t media;
  uint8_t direction;
  uint8_t bus_type;
  uint8_t channels;
} sp_vst3_native_bus;

typedef struct {
  uint32_t id;
  int32_t step_count;
  uint32_t flags;
  double normalized;
  double default_normalized;
  char title[128];
  char short_title[128];
  char units[64];
} sp_vst3_native_parameter;

// All functions return zero on success.  A failure's stable text is available
// through sp_vst3_native_error and remains valid until the next shim call.
sp_vst3_native* sp_vst3_native_create(const char* module_path, const char* class_id,
                                      double sample_rate, int32_t maximum_frames);
void sp_vst3_native_destroy(sp_vst3_native* host);
const char* sp_vst3_native_error(const sp_vst3_native* host);
int32_t sp_vst3_native_initialize_component(sp_vst3_native* host);
int32_t sp_vst3_native_initialize_controller(sp_vst3_native* host);
int32_t sp_vst3_native_connect(sp_vst3_native* host);
int32_t sp_vst3_native_bus_count(sp_vst3_native* host, uint8_t media, uint8_t direction);
int32_t sp_vst3_native_bus_info(sp_vst3_native* host, uint8_t media, uint8_t direction,
                                 int32_t index, sp_vst3_native_bus* out);
int32_t sp_vst3_native_set_arrangements(sp_vst3_native* host, uint8_t input_channels,
                                        uint8_t output_channels);
int32_t sp_vst3_native_activate_bus(sp_vst3_native* host, uint8_t media, uint8_t direction,
                                    int32_t index, uint8_t active);
int32_t sp_vst3_native_start(sp_vst3_native* host);
int32_t sp_vst3_native_stop(sp_vst3_native* host);
int32_t sp_vst3_native_process_f32(sp_vst3_native* host, const float* const* input,
                                   int32_t input_channels, float* const* output,
                                   int32_t output_channels, int32_t frames);
int32_t sp_vst3_native_parameter_count(sp_vst3_native* host);
int32_t sp_vst3_native_parameter_info(sp_vst3_native* host, int32_t index,
                                      sp_vst3_native_parameter* out);
int32_t sp_vst3_native_set_parameter(sp_vst3_native* host, uint32_t id, double normalized);
int32_t sp_vst3_native_get_parameter(sp_vst3_native* host, uint32_t id, double* out);
int32_t sp_vst3_native_format_parameter(sp_vst3_native* host, uint32_t id, double normalized,
                                        char* out, int32_t capacity);
int32_t sp_vst3_native_begin_edit(sp_vst3_native* host, uint32_t id);
int32_t sp_vst3_native_end_edit(sp_vst3_native* host, uint32_t id);
int32_t sp_vst3_native_get_component_state(sp_vst3_native* host, uint8_t** data, int32_t* size);
int32_t sp_vst3_native_get_controller_state(sp_vst3_native* host, uint8_t** data, int32_t* size);
void sp_vst3_native_free_bytes(uint8_t* data);
int32_t sp_vst3_native_set_component_state(sp_vst3_native* host, const uint8_t* data, int32_t size);
int32_t sp_vst3_native_set_component_state_on_controller(sp_vst3_native* host, const uint8_t* data, int32_t size);
int32_t sp_vst3_native_set_controller_state(sp_vst3_native* host, const uint8_t* data, int32_t size);
uint32_t sp_vst3_native_latency(sp_vst3_native* host);
uint32_t sp_vst3_native_take_restart_flags(sp_vst3_native* host);
int32_t sp_vst3_native_open_editor(sp_vst3_native* host, void* ns_view, int32_t* width, int32_t* height);
int32_t sp_vst3_native_close_editor(sp_vst3_native* host);
int32_t sp_vst3_native_resize_editor(sp_vst3_native* host, int32_t width, int32_t height);

#ifdef __cplusplus
}
#endif
