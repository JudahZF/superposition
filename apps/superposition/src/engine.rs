//! Shared audio preparation for the desktop and headless hosts.

use sp_audio_io::{
    LaneWorker, ProductControl, ProductRenderer, TopologyLane, current_product_parameters,
    prepare_product_scenes, product_midi_mappings,
};
use sp_engine::{PreparedGraph, RackSettings};
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
    let sample_rate_hz = model
        .audio_settings
        .as_ref()
        .map_or(sp_audio_io::PRODUCT_SAMPLE_RATE_HZ, |settings| {
            settings.sample_rate_hz
        });
    // Workers set plug-ins up for one rate; racks started at another rate restart first.
    product.apply_sample_rate(sample_rate_hz)?;
    let banks = product.audio_bank_mappings()?;
    let pipelined = product.pipelined_rack_lanes()?;
    let mut renderer = ProductRenderer::with_rack_banks(graph, banks)
        .map_err(|error| format!("Rack dispatcher setup failed: {error}"))?
        .with_sample_rate(sample_rate_hz)
        .map_err(|error| error.to_string())?;
    for (index, rack) in model.racks.iter().enumerate() {
        let mixer = renderer.mixer_mut();
        mixer.set_dry_fallback_available(index, product.rack_dry_fallback_available(rack));
        if let Some(latency) = product.rack_latency_samples(index) {
            mixer.set_dry_delay_frames(index, usize::try_from(latency).unwrap_or(usize::MAX));
        }
        mixer.set_rack_gain(index, rack_gain(rack));
        mixer.set_rack_muted(index, rack.muted);
        mixer.set_rack_bypassed(index, rack.bypassed);
    }
    let mappings = product_midi_mappings(model);
    let (mut control, control_receiver) = ProductControl::new();
    if !pipelined.is_empty() {
        add_pipelined_racks(product, model, &mut control, pipelined)?;
    }
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

/// Queues a layout that keeps every same-block rack and adds the pipelined (Windows) racks,
/// which a renderer's initial banks cannot be. The first callback applies it; each pipelined
/// rack plays dry until its worker delivers three valid blocks, then fades in.
fn add_pipelined_racks(
    product: &ProductRuntime,
    model: &sp_model::Session,
    control: &mut ProductControl,
    pipelined: Vec<(usize, sp_audio_io::PreparedRackLane)>,
) -> Result<(), String> {
    let graph = PreparedGraph::compile(model)
        .map_err(|error| format!("Rack graph is not ready: {error}"))?;
    let mut pipelined: Vec<_> = pipelined.into_iter().map(Some).collect();
    let lanes = model
        .racks
        .iter()
        .enumerate()
        .map(|(position, rack)| {
            let lane = pipelined
                .iter_mut()
                .find(|lane| lane.as_ref().is_some_and(|(index, _)| *index == position))
                .and_then(Option::take);
            TopologyLane {
                from: Some(position),
                worker: lane.map_or(LaneWorker::Keep, |(_, lane)| LaneWorker::Replace(lane)),
                settings: RackSettings {
                    gain: rack_gain(rack),
                    muted: rack.muted,
                    bypassed: rack.bypassed,
                    latency_frames: product
                        .rack_latency_samples(position)
                        .map_or(0, |latency| usize::try_from(latency).unwrap_or(usize::MAX)),
                },
                dry_fallback: product.rack_dry_fallback_available(rack),
            }
        })
        .collect();
    control
        .publish_topology(model, graph, lanes)
        .map_err(|_| "the Windows racks could not join audio".to_owned())
}

fn rack_gain(rack: &sp_model::Rack) -> f32 {
    10.0_f32.powf(rack.gain_db.get() / 20.0)
}
