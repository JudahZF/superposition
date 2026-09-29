//! Shared audio preparation for the desktop and headless hosts.

use sp_audio_io_macos::{
    ProductControl, ProductRenderer, current_product_parameters, prepare_product_scenes,
    product_midi_mappings,
};
use sp_engine::PreparedGraph;
use sp_midi::{MidiLearnTable, MidiMappingPublisher};

use crate::ProductRuntime;

pub(crate) struct PreparedAudio {
    pub(crate) renderer: ProductRenderer,
    pub(crate) control: ProductControl,
    pub(crate) mapping_publisher: MidiMappingPublisher,
    pub(crate) mappings: MidiLearnTable,
}

pub(crate) fn prepare_audio(
    product: &mut ProductRuntime,
    model: &sp_model::Session,
) -> Result<PreparedAudio, String> {
    let graph = PreparedGraph::compile(model)
        .map_err(|error| format!("Rack graph is not ready: {error}"))?;
    let banks = product.audio_bank_mappings()?;
    let mut renderer = ProductRenderer::with_rack_banks(graph, banks)
        .map_err(|error| format!("Rack dispatcher setup failed: {error}"))?;
    for (index, rack) in model.racks.iter().enumerate() {
        let mixer = renderer.mixer_mut();
        mixer.set_dry_fallback_available(index, product.rack_dry_fallback_available(rack));
        if let Some(latency) = product.rack_latency_samples(index) {
            mixer.set_dry_delay_frames(index, usize::try_from(latency).unwrap_or(usize::MAX));
        }
        mixer.set_rack_gain(index, 10.0_f32.powf(rack.gain_db.get() / 20.0));
        mixer.set_rack_muted(index, rack.muted);
        mixer.set_rack_bypassed(index, rack.bypassed);
    }
    let mappings = product_midi_mappings(model);
    let (control, control_receiver) = ProductControl::new();
    let (mapping_publisher, mapping_receiver) = MidiMappingPublisher::new();
    renderer = renderer.with_live_control(
        control_receiver,
        mapping_receiver,
        mappings,
        prepare_product_scenes(model),
        current_product_parameters(model),
    );
    Ok(PreparedAudio {
        renderer,
        control,
        mapping_publisher,
        mappings,
    })
}
