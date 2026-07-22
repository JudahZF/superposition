#include "vst3_shim.h"

#include <dlfcn.h>
#include <cstring>
#include <memory>
#include <string>
#include <vector>

#include "pluginterfaces/base/funknown.h"
#include "pluginterfaces/base/ipluginbase.h"
#include "pluginterfaces/base/ustring.h"
#include "pluginterfaces/gui/iplugview.h"
#include "pluginterfaces/vst/ivstaudioprocessor.h"
#include "pluginterfaces/vst/ivstcomponent.h"
#include "pluginterfaces/vst/ivsteditcontroller.h"
#include "pluginterfaces/vst/ivstmessage.h"
#include "public.sdk/source/common/memorystream.h"
#include "public.sdk/source/vst/hosting/hostclasses.h"

using namespace Steinberg;
using namespace Steinberg::Vst;

namespace {
class ComponentHandler final : public IComponentHandler {
 public:
  tresult PLUGIN_API beginEdit(ParamID id) override { gestures.push_back({id, 0, 0.}); return kResultOk; }
  tresult PLUGIN_API performEdit(ParamID id, ParamValue value) override { gestures.push_back({id, 1, value}); return kResultOk; }
  tresult PLUGIN_API endEdit(ParamID id) override { gestures.push_back({id, 2, 0.}); return kResultOk; }
  tresult PLUGIN_API restartComponent(int32 flags) override { restart_flags |= static_cast<uint32>(flags); return kResultOk; }
  tresult PLUGIN_API queryInterface(const TUID, void**) override { return kNoInterface; }
  uint32 PLUGIN_API addRef() override { return 1; }
  uint32 PLUGIN_API release() override { return 1; }
  struct Gesture { ParamID id; int kind; ParamValue value; };
  std::vector<Gesture> gestures;
  uint32 restart_flags {};
};

bool result_ok(tresult result) { return result == kResultOk || result == kResultTrue; }

void copy_text(char* destination, size_t capacity, const char* source) {
  if (!capacity) return;
  std::strncpy(destination, source ? source : "", capacity - 1);
  destination[capacity - 1] = '\0';
}

std::string string128(const TChar* source) {
  std::string result;
  if (!source) return result;
  for (int32 index = 0; index < 127 && source[index]; ++index)
    result.push_back(source[index] <= 0x7f ? static_cast<char>(source[index]) : '?');
  return result;
}

}  // namespace

struct sp_vst3_native {
  void* library {};
  IPluginFactory* factory {};
  IComponent* component {};
  IAudioProcessor* processor {};
  IEditController* controller {};
  IConnectionPoint* component_connection {};
  IConnectionPoint* controller_connection {};
  IPlugView* view {};
  HostApplication host_application;
  ComponentHandler handler;
  std::string error;
  bool component_initialized {};
  bool controller_initialized {};
  bool connected {};
  bool active {};
  bool processing {};
  double sample_rate {};
  int32 maximum_frames {};

  ~sp_vst3_native() {
    if (view) { view->removed(); view->release(); }
    if (processing) processor->setProcessing(false);
    if (active) component->setActive(false);
    if (connected) {
      if (component_connection && controller_connection) {
        component_connection->disconnect(controller_connection);
        controller_connection->disconnect(component_connection);
      }
    }
    if (controller_initialized) controller->terminate();
    if (component_initialized) component->terminate();
    if (controller_connection) controller_connection->release();
    if (component_connection) component_connection->release();
    if (controller) controller->release();
    if (processor) processor->release();
    if (component) component->release();
    if (factory) factory->release();
    if (library) dlclose(library);
  }

  int32_t fail(const char* message) { error = message; return -1; }
  int32_t fail(const std::string& message) { error = message; return -1; }
};

template <typename Fn>
int32_t invoke(sp_vst3_native* host, Fn&& function) {
  if (!host) return -1;
  try { return function(); }
  catch (const std::exception& error) { return host->fail(error.what()); }
  catch (...) { return host->fail("VST3 SDK call raised a non-standard exception"); }
}

bool class_id(const char* text, FUID& id) {
  return text && id.fromString(reinterpret_cast<const char8*>(text));
}

bool query(FUnknown* object, const TUID iid, void** destination) {
  *destination = nullptr;
  return object && result_ok(object->queryInterface(iid, destination)) && *destination;
}

int32_t save_stream(sp_vst3_native* host, bool component, uint8_t** data, int32_t* size) {
  if (!data || !size) return host->fail("state output pointers are required");
  MemoryStream stream;
  const auto result = component ? host->component->getState(&stream) : host->controller->getState(&stream);
  if (!result_ok(result)) return host->fail("VST3 object rejected getState");
  const auto length = stream.getSize();
  if (length > INT32_MAX) return host->fail("VST3 state stream exceeds C ABI capacity");
  auto* bytes = static_cast<uint8_t*>(std::malloc(static_cast<size_t>(length)));
  if (length && !bytes) return host->fail("could not allocate VST3 state copy");
  if (length) std::memcpy(bytes, stream.getData(), static_cast<size_t>(length));
  *data = bytes; *size = static_cast<int32_t>(length);
  return 0;
}

int32_t load_stream(sp_vst3_native* host, const uint8_t* data, int32_t size, int kind) {
  if (size < 0 || (size && !data)) return host->fail("invalid VST3 state input");
  MemoryStream stream(const_cast<uint8_t*>(data), size);
  tresult result = kResultFalse;
  if (kind == 0) result = host->component->setState(&stream);
  else if (kind == 1) result = host->controller->setComponentState(&stream);
  else result = host->controller->setState(&stream);
  return result_ok(result) ? 0 : host->fail("VST3 object rejected state restoration");
}

extern "C" sp_vst3_native* sp_vst3_native_create(const char* module_path, const char* selected_class,
                                                    double sample_rate, int32_t maximum_frames) {
  try {
    if (!module_path || !selected_class || !(sample_rate > 0.) || maximum_frames <= 0) return nullptr;
    auto host = std::make_unique<sp_vst3_native>();
    host->sample_rate = sample_rate; host->maximum_frames = maximum_frames;
    host->library = dlopen(module_path, RTLD_NOW | RTLD_LOCAL);
    if (!host->library) { host->error = dlerror(); return host.release(); }
    auto factory_function = reinterpret_cast<IPluginFactory* (*)()>(dlsym(host->library, "GetPluginFactory"));
    if (!factory_function || !(host->factory = factory_function())) { host->error = "module does not export GetPluginFactory"; return host.release(); }
    FUID id;
    if (!class_id(selected_class, id)) { host->error = "selected VST3 class id is not a 32-digit hexadecimal FUID"; return host.release(); }
    void* instance = nullptr;
    if (!result_ok(host->factory->createInstance(id.toTUID(), IComponent::iid.toTUID(), &instance)) || !instance) { host->error = "factory could not instantiate the selected IComponent class"; return host.release(); }
    host->component = static_cast<IComponent*>(instance);
    if (!query(host->component, IAudioProcessor::iid.toTUID(), reinterpret_cast<void**>(&host->processor))) { host->error = "selected class does not implement IAudioProcessor"; return host.release(); }
    return host.release();
  } catch (...) { return nullptr; }
}
extern "C" void sp_vst3_native_destroy(sp_vst3_native* host) { delete host; }
extern "C" const char* sp_vst3_native_error(const sp_vst3_native* host) { return host ? host->error.c_str() : "native VST3 handle is null"; }
extern "C" int32_t sp_vst3_native_initialize_component(sp_vst3_native* h) { return invoke(h, [&] { if (h->component_initialized) return h->fail("component already initialized"); if (!result_ok(h->component->initialize(&h->host_application))) return h->fail("IComponent::initialize failed"); h->component_initialized = true; return 0; }); }
extern "C" int32_t sp_vst3_native_initialize_controller(sp_vst3_native* h) { return invoke(h, [&] { if (!h->component_initialized || h->controller_initialized) return h->fail("invalid controller initialization order"); TUID id {}; if (!result_ok(h->component->getControllerClassId(id))) return h->fail("IComponent::getControllerClassId failed"); void* raw = nullptr; if (!result_ok(h->factory->createInstance(id, IEditController::iid.toTUID(), &raw)) || !raw) { if (!query(h->component, IEditController::iid.toTUID(), reinterpret_cast<void**>(&h->controller))) return h->fail("component did not provide an IEditController"); } else h->controller = static_cast<IEditController*>(raw); if (!result_ok(h->controller->initialize(&h->host_application))) return h->fail("IEditController::initialize failed"); h->controller->setComponentHandler(&h->handler); h->controller_initialized = true; return 0; }); }
extern "C" int32_t sp_vst3_native_connect(sp_vst3_native* h) { return invoke(h, [&] { if (!h->controller_initialized || h->connected) return h->fail("invalid connection-point order"); query(h->component, IConnectionPoint::iid.toTUID(), reinterpret_cast<void**>(&h->component_connection)); query(h->controller, IConnectionPoint::iid.toTUID(), reinterpret_cast<void**>(&h->controller_connection)); if (h->component_connection && h->controller_connection) { if (!result_ok(h->component_connection->connect(h->controller_connection)) || !result_ok(h->controller_connection->connect(h->component_connection))) return h->fail("IConnectionPoint::connect failed"); } h->connected = true; return 0; }); }
extern "C" int32_t sp_vst3_native_bus_count(sp_vst3_native* h, uint8_t media, uint8_t direction) { return invoke(h, [&] { return h->component_initialized ? h->component->getBusCount(static_cast<MediaType>(media), static_cast<BusDirection>(direction)) : h->fail("component is not initialized"); }); }
extern "C" int32_t sp_vst3_native_bus_info(sp_vst3_native* h, uint8_t media, uint8_t direction, int32_t index, sp_vst3_native_bus* out) { return invoke(h, [&] { if (!out) return h->fail("bus output is null"); BusInfo info {}; if (!result_ok(h->component->getBusInfo(static_cast<MediaType>(media), static_cast<BusDirection>(direction), index, info))) return h->fail("IComponent::getBusInfo failed"); out->media=media; out->direction=direction; out->bus_type=static_cast<uint8_t>(info.busType); out->channels=static_cast<uint8_t>(info.channelCount); return 0; }); }
extern "C" int32_t sp_vst3_native_set_arrangements(sp_vst3_native* h, uint8_t ins, uint8_t outs) { return invoke(h, [&] { SpeakerArrangement input = ins == 2 ? SpeakerArr::kStereo : ins == 1 ? SpeakerArr::kMono : SpeakerArr::kEmpty; SpeakerArrangement output = outs == 2 ? SpeakerArr::kStereo : outs == 1 ? SpeakerArr::kMono : SpeakerArr::kEmpty; if (!result_ok(h->processor->setBusArrangements(ins ? &input : nullptr, ins ? 1 : 0, &output, 1))) return h->fail("IAudioProcessor::setBusArrangements failed"); ProcessSetup setup {kRealtime, kSample32, h->maximum_frames, h->sample_rate}; return result_ok(h->processor->setupProcessing(setup)) ? 0 : h->fail("IAudioProcessor::setupProcessing failed"); }); }
extern "C" int32_t sp_vst3_native_activate_bus(sp_vst3_native* h, uint8_t m, uint8_t d, int32_t i, uint8_t active) { return invoke(h, [&] { return result_ok(h->component->activateBus(static_cast<MediaType>(m), static_cast<BusDirection>(d), i, active)) ? 0 : h->fail("IComponent::activateBus failed"); }); }
extern "C" int32_t sp_vst3_native_start(sp_vst3_native* h) { return invoke(h, [&] { if (!h->active && !result_ok(h->component->setActive(true))) return h->fail("IComponent::setActive(true) failed"); h->active=true; if (!result_ok(h->processor->setProcessing(true))) return h->fail("IAudioProcessor::setProcessing(true) failed"); h->processing=true; return 0; }); }
extern "C" int32_t sp_vst3_native_stop(sp_vst3_native* h) { return invoke(h, [&] { if (h->processing && !result_ok(h->processor->setProcessing(false))) return h->fail("IAudioProcessor::setProcessing(false) failed"); h->processing=false; return 0; }); }
extern "C" int32_t sp_vst3_native_process_f32(sp_vst3_native* h, const float* const* in, int32_t ni, float* const* out, int32_t no, int32_t frames) { return invoke(h, [&] { if (!h->processing || frames <= 0 || frames > h->maximum_frames) return h->fail("invalid VST3 process call"); AudioBusBuffers input {}; input.numChannels=ni; input.silenceFlags=0; input.channelBuffers32=const_cast<Sample32**>(in); AudioBusBuffers output {}; output.numChannels=no; output.silenceFlags=0; output.channelBuffers32=const_cast<Sample32**>(out); ProcessData data {}; data.processMode=kRealtime; data.symbolicSampleSize=kSample32; data.numSamples=frames; data.numInputs=ni ? 1 : 0; data.numOutputs=no ? 1 : 0; data.inputs=ni ? &input : nullptr; data.outputs=no ? &output : nullptr; return result_ok(h->processor->process(data)) ? 0 : h->fail("IAudioProcessor::process failed"); }); }
extern "C" int32_t sp_vst3_native_parameter_count(sp_vst3_native* h) { return invoke(h, [&] { return h->controller ? h->controller->getParameterCount() : h->fail("controller is not initialized"); }); }
extern "C" int32_t sp_vst3_native_parameter_info(sp_vst3_native* h, int32_t i, sp_vst3_native_parameter* out) { return invoke(h, [&] { if (!out) return h->fail("parameter output is null"); ParameterInfo p {}; if (!result_ok(h->controller->getParameterInfo(i,p))) return h->fail("IEditController::getParameterInfo failed"); out->id=p.id; out->step_count=p.stepCount; out->flags=p.flags; out->normalized=h->controller->getParamNormalized(p.id); out->default_normalized=p.defaultNormalizedValue; copy_text(out->title,sizeof out->title,string128(p.title).c_str()); copy_text(out->short_title,sizeof out->short_title,string128(p.shortTitle).c_str()); copy_text(out->units,sizeof out->units,string128(p.units).c_str()); return 0; }); }
extern "C" int32_t sp_vst3_native_set_parameter(sp_vst3_native* h, uint32_t id, double value) { return invoke(h, [&] { return value >= 0. && value <= 1. && result_ok(h->controller->setParamNormalized(id,value)) ? 0 : h->fail("IEditController::setParamNormalized failed"); }); }
extern "C" int32_t sp_vst3_native_get_parameter(sp_vst3_native* h, uint32_t id, double* out) { return invoke(h, [&] { if (!out) return h->fail("parameter output is null"); *out=h->controller->getParamNormalized(id); return 0; }); }
extern "C" int32_t sp_vst3_native_format_parameter(sp_vst3_native* h, uint32_t id, double v, char* out, int32_t cap) { return invoke(h, [&] { if (!out || cap <= 0) return h->fail("format output is invalid"); String128 text {}; if (!result_ok(h->controller->getParamStringByValue(id,v,text))) return h->fail("IEditController::getParamStringByValue failed"); copy_text(out,static_cast<size_t>(cap),string128(text).c_str()); return 0; }); }
extern "C" int32_t sp_vst3_native_begin_edit(sp_vst3_native* h, uint32_t id) { return invoke(h, [&] { return result_ok(h->handler.beginEdit(id)) ? 0 : h->fail("IComponentHandler::beginEdit failed"); }); }
extern "C" int32_t sp_vst3_native_end_edit(sp_vst3_native* h, uint32_t id) { return invoke(h, [&] { return result_ok(h->handler.endEdit(id)) ? 0 : h->fail("IComponentHandler::endEdit failed"); }); }
extern "C" int32_t sp_vst3_native_get_component_state(sp_vst3_native* h,uint8_t**d,int32_t*s){return invoke(h,[&]{return save_stream(h,true,d,s);});}
extern "C" int32_t sp_vst3_native_get_controller_state(sp_vst3_native* h,uint8_t**d,int32_t*s){return invoke(h,[&]{return save_stream(h,false,d,s);});}
extern "C" void sp_vst3_native_free_bytes(uint8_t* data){std::free(data);}
extern "C" int32_t sp_vst3_native_set_component_state(sp_vst3_native*h,const uint8_t*d,int32_t s){return invoke(h,[&]{return load_stream(h,d,s,0);});}
extern "C" int32_t sp_vst3_native_set_component_state_on_controller(sp_vst3_native*h,const uint8_t*d,int32_t s){return invoke(h,[&]{return load_stream(h,d,s,1);});}
extern "C" int32_t sp_vst3_native_set_controller_state(sp_vst3_native*h,const uint8_t*d,int32_t s){return invoke(h,[&]{return load_stream(h,d,s,2);});}
extern "C" uint32_t sp_vst3_native_latency(sp_vst3_native*h){return h&&h->processor?h->processor->getLatencySamples():0;}
extern "C" uint32_t sp_vst3_native_take_restart_flags(sp_vst3_native*h){if(!h)return 0;auto r=h->handler.restart_flags;h->handler.restart_flags=0;return r;}
extern "C" int32_t sp_vst3_native_open_editor(sp_vst3_native*h,void*parent,int32_t*w,int32_t*he){return invoke(h,[&]{if(h->view)return h->fail("VST3 editor is already open"); h->view=h->controller->createView(ViewType::kEditor);if(!h->view)return h->fail("controller has no editor");ViewRect r{};if(!result_ok(h->view->getSize(&r))||!result_ok(h->view->attached(parent,kPlatformTypeNSView)))return h->fail("IPlugView attachment failed");if(w)*w=r.getWidth();if(he)*he=r.getHeight();return 0;});}
extern "C" int32_t sp_vst3_native_close_editor(sp_vst3_native*h){return invoke(h,[&]{if(!h->view)return h->fail("VST3 editor is not open");h->view->removed();h->view->release();h->view=nullptr;return 0;});}
extern "C" int32_t sp_vst3_native_resize_editor(sp_vst3_native*h,int32_t w,int32_t he){return invoke(h,[&]{if(!h->view||w<=0||he<=0)return h->fail("invalid VST3 editor resize");ViewRect r{0,0,w,he};return result_ok(h->view->onSize(&r))?0:h->fail("IPlugView::onSize failed");});}
