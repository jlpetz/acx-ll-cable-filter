use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::{Arc, OnceLock};

use nf_core::config::EffectConfig;
use nf_core::controls::ChainControls;
use nf_core::stage::{Activation, AudioPorts, MONO_IN_OUT, ProcessIo, Stage, StageDescriptor, StageError, features, text};

/// The loaded plugin and its current deadline counters, for the settings window and CLI.
#[derive(Clone, Debug)]
pub struct EffectInfo {
    pub path: PathBuf,
    pub plugin_id: String,
    pub name: String,
    pub params: Vec<nf_fx::Parameter>,
    pub missed_blocks: u64,
    pub processed_blocks: u64,
    pub failed: bool,
    pub worker_blocks: u8,
}

/// Owned by the control side; the audio thread only needs the client and the bypass flags.
pub(crate) struct LoadedEffect {
    path: PathBuf,
    info: nf_fx::EffectInfo,
    control: nf_fx::FxControl,
    bypass: Arc<AtomicBool>,
    worker_blocks: Arc<AtomicU8>,
}

type OpenedEffect = Option<(LoadedEffect, Box<ExternalStage>)>;

impl LoadedEffect {
    pub(crate) fn open(config: &EffectConfig, controls: Arc<ChainControls>) -> Result<OpenedEffect, String> {
        Self::open_for(config, controls, None)
    }

    fn open_for(
        config: &EffectConfig, controls: Arc<ChainControls>, offline_failure: Option<Arc<OnceLock<String>>>,
    ) -> Result<OpenedEffect, String> {
        let Some(path) = &config.path else { return Ok(None) };
        if usize::from(config.worker_blocks) > nf_fx::MAX_DEPTH {
            return Err("External effect worker_blocks must be 0 (automatic) or between 1 and 4.".into());
        }
        let client = nf_fx::FxClient::launch(path, config.plugin_id.as_deref())?;
        let info = client.info().clone();
        let descriptor = StageDescriptor {
            id: info.id.clone().into(),
            name: info.name.clone().into(),
            vendor: text(""),
            url: text(""),
            version: text(""),
            description: text("External CLAP effect"),
            features: Cow::Owned(vec![features::AUDIO_EFFECT]),
        };
        let bypass = Arc::new(AtomicBool::new(config.bypass));
        let worker_blocks = Arc::new(AtomicU8::new(1));
        let loaded =
            Self { path: path.clone(), info, control: client.control(), bypass: bypass.clone(), worker_blocks: worker_blocks.clone() };
        loaded.apply(config);
        Ok(Some((loaded, Box::new(ExternalStage { descriptor, client, controls, bypass, offline_failure, worker_blocks }))))
    }

    pub(crate) fn pin_id(&self, cfg: &mut EffectConfig) {
        if cfg.path.as_ref() == Some(&self.path) && cfg.plugin_id.is_none() {
            cfg.plugin_id = Some(self.info.id.clone());
        }
    }

    pub(crate) fn apply(&self, cfg: &EffectConfig) {
        if cfg.path.as_ref() != Some(&self.path) || cfg.plugin_id.as_deref().is_some_and(|id| id != self.info.id) {
            return;
        }
        for param in &self.info.params {
            self.control.set_parameter(param.id, cfg.params.get(&param.id).copied().unwrap_or(param.default));
        }
        self.bypass.store(cfg.bypass, Relaxed);
    }

    pub(crate) fn snapshot(&self) -> EffectInfo {
        EffectInfo {
            path: self.path.clone(),
            plugin_id: self.info.id.clone(),
            name: self.info.name.clone(),
            params: self.info.params.clone(),
            missed_blocks: self.control.missed_blocks(),
            processed_blocks: self.control.processed_blocks(),
            failed: self.control.failed(),
            worker_blocks: self.worker_blocks.load(Relaxed),
        }
    }
}

pub(crate) struct ExternalStage {
    descriptor: StageDescriptor,
    client: nf_fx::FxClient,
    controls: Arc<ChainControls>,
    bypass: Arc<AtomicBool>,
    worker_blocks: Arc<AtomicU8>,
    /// Offline renders can wait for the helper and can return a processing error to the WAV command.
    offline_failure: Option<Arc<OnceLock<String>>>,
}

impl ExternalStage {
    pub(crate) fn set_depth(&mut self, blocks: u8) -> Result<(), StageError> {
        self.client.set_depth(usize::from(blocks)).map_err(StageError)?;
        self.worker_blocks.store(blocks, Relaxed);
        Ok(())
    }
}

impl Stage for ExternalStage {
    fn descriptor(&self) -> &StageDescriptor {
        &self.descriptor
    }

    fn audio_ports(&self) -> &AudioPorts {
        &MONO_IN_OUT
    }

    fn activate(&mut self, activation: &Activation) -> Result<(), StageError> {
        activation.require_host(&self.descriptor.name)
    }

    fn latency(&self) -> u32 {
        (self.client.depth() * nf_core::BLOCK) as u32
    }

    fn process(&mut self, io: ProcessIo<'_>) {
        let bypass = self.bypass.load(Relaxed) || self.controls.passthrough.load(Relaxed);
        if let Some(failure) = &self.offline_failure {
            if failure.get().is_some() {
                io.output.fill(0.0);
            } else if let Err(e) = self.client.process_offline(io.inputs[0], io.output, bypass) {
                let _ = failure.set(e);
            }
        } else {
            self.client.process(io.inputs[0], io.output, bypass);
        }
    }

    fn reset(&mut self) {
        self.client.reset();
    }
}

/// Run the configured external effect for an offline WAV render. The caller checks the returned error slot before saving the file.
pub fn attach_offline_effect(
    chain: &mut nf_core::chain::Chain, config: &EffectConfig, controls: Arc<ChainControls>,
) -> Result<Arc<OnceLock<String>>, String> {
    let failure = Arc::new(OnceLock::new());
    if let Some((_, mut stage)) = LoadedEffect::open_for(config, controls, Some(failure.clone()))? {
        stage.set_depth(config.worker_blocks.max(1)).map_err(|e| e.0)?;
        chain.attach_effect(stage).map_err(|e| e.0)?;
    }
    Ok(failure)
}
