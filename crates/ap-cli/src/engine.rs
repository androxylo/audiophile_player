//! Full-memory headless playback pipeline coordinator.
//!
//! Currently runs in idle skeleton mode (no audio I/O yet) to validate startup,
//! steady idle wait, and graceful shutdown lifecycle before Milestone 1 DSP & WDM-KS wiring.

use std::io::{self, Write};

use ap_core::config::{ChannelDelayConfig, EngineConfig, OutputTransport};

use crate::signals::ShutdownSignal;

/// Headless playback engine lifecycle container.
#[derive(Debug)]
pub struct PlaybackEngine {
    config: EngineConfig,
    running: bool,
}

impl PlaybackEngine {
    pub fn new_default() -> Self {
        Self {
            config: EngineConfig {
                dac_name_filter: "Gungnir 2".to_string(),
                transport: OutputTransport::WdmKs,
                rew_convolution_wav: None,
                channel_delay: ChannelDelayConfig::default(),
                http_bind_addr: "127.0.0.1:8080".to_string(),
            },
            running: false,
        }
    }

    /// Start the headless engine (currently idle placeholder; prints startup confirmation).
    pub fn start(&mut self) {
        self.running = true;
        println!(
            "audiophile-player v{} started (transport: {:?}, target DAC: '{}', idle mode — waiting for interrupt signal).",
            env!("CARGO_PKG_VERSION"),
            self.config.transport,
            self.config.dac_name_filter
        );
        let _ = io::stdout().flush();
    }

    /// Gracefully stop the headless engine and release resources cleanly.
    pub fn stop(&mut self, signal: ShutdownSignal) {
        if !self.running {
            return;
        }
        println!("audiophile-player received {signal}; shutting down gracefully...");
        let _ = io::stdout().flush();

        // Future Milestone 1 teardown: stop HTTP control server and release exclusive DAC pin.
        self.running = false;

        println!("audiophile-player stopped cleanly.");
        let _ = io::stdout().flush();
    }
}
