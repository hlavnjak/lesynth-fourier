// Copyright 2025 Jakub Hlavnicka
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

mod constants;
mod engine;
mod gui;
mod params;
mod plugin;
mod voice;

pub use plugin::LeSynth;

#[cfg(all(debug_assertions, feature = "debug-logging"))]
use std::sync::Once;
#[cfg(all(debug_assertions, feature = "debug-logging"))]
static INIT_LOGGER: Once = Once::new();

#[cfg(all(debug_assertions, feature = "debug-logging"))]
pub fn init_logging() {
    INIT_LOGGER.call_once(|| {
        use std::fs::OpenOptions;
        use std::io::Write;
        
        let log_file = std::env::temp_dir().join("lesynth.log");
        
        env_logger::Builder::new()
            .filter_level(log::LevelFilter::Debug)
            .format(|buf, record| {
                use std::io::Write;
                let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
                writeln!(buf, "[{}] [{}] [{}:{}] {}", 
                    timestamp,
                    record.level(),
                    record.file().unwrap_or("unknown"),
                    record.line().unwrap_or(0),
                    record.args()
                )
            })
            .init();
            
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file) 
        {
            let _ = writeln!(file, "\n=== LeSynth Debug Session Started ===");
        }
        
        log::info!("LeSynth logging initialized. Log file: {:?}", log_file);
    });
}

#[cfg(not(all(debug_assertions, feature = "debug-logging")))]
pub fn init_logging() {
    // No-op when not in debug build with debug-logging feature
}

nih_plug::nih_export_vst3!(LeSynth);
nih_plug::nih_export_clap!(LeSynth);
