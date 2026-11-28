//! Run a mono voice CLAP in the same isolated worker as the single-chain application.
use std::path::Path;

use llcable_routing::{Effect, StageReading};
use nf_core::Block;
pub use nf_fx::{EffectInfo, Parameter, inspect as inspect_clap};

use super::{Voice, VoiceModel};

struct Plugin {
    client: nf_fx::FxClient,
    control: nf_fx::FxControl,
}

impl VoiceModel for Plugin {
    fn process(&mut self, input: &Block, output: &mut Block) {
        // The worker has one hop to respond. FxClient supplies the matching delayed dry block on a miss.
        self.client.process(input, output, false);
    }

    fn reading(&self) -> StageReading {
        StageReading { worker_misses: Some(self.control.missed_blocks()), ..Default::default() }
    }
}

/// Construct on a control thread. Only this branch incurs the mono conversion and two 480-frame hops of delay.
pub fn clap_voice(path: &Path, plugin_id: Option<&str>, params: &[(u32, f64)]) -> Result<Effect, String> {
    let client = nf_fx::FxClient::launch(path, plugin_id)?;
    for &(id, value) in params {
        if !client.set_parameter(id, value) {
            return Err(format!("Invalid parameter {id} for {}", client.info().name));
        }
    }
    let control = client.control();
    Ok(Voice::effect(Plugin { client, control }))
}
