//! Playing a voice message in the conversation view: its `.ogg` downloads
//! from Discord's CDN, decodes Opus to 48 kHz stereo through the same
//! `soundboard::decode` that plays soundboard sounds, and hands the samples
//! to `audio_io::Output` on a blocking thread. Dropping the playback stops it.
//!
//! Playback is "fake": the whole clip is decoded before it plays, since voice
//! messages are short (seconds, not minutes). No seeking, no waveform-based
//! progress — the UI shows the rendered waveform from Discord's `waveform`
//! field and the `duration_secs` duration.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{
    audio_io::{self, Output},
    chat::{VoiceMessage, VoiceMessageCommand},
    soundboard,
};

const MAX_VOICE_MESSAGE_BYTES: usize = 10 * 1024 * 1024;

pub struct VoicePlayback {
    pub state: VoiceMessage,
    stop: Arc<AtomicBool>,
    playing: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}


impl VoicePlayback {
    pub fn start(
        state: VoiceMessage,
        discord: yapfast_discord::service::Discord,
        url: String,
        duration_ms: Option<u64>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let playing = Arc::new(AtomicBool::new(true));
        let (stop_clone, playing_clone) = (stop.clone(), playing.clone());
        let thread = std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(_) => return,
            };
            let data = match runtime.block_on(async {
                discord.download_now(&url, MAX_VOICE_MESSAGE_BYTES).await
            }) {
                Ok(data) => data,
                Err(_) => return,
            };
            if stop_clone.load(Ordering::Relaxed) {
                return;
            }
            let samples = match soundboard::decode(&data) {
                Ok(samples) => samples,
                Err(_) => return,
            };
            let Ok(mut output) =
                Output::<i16>::open_or_default(None, "yapfast-voice-message", audio_io::RATE, 2, None)
            else {
                return;
            };
            let chunk = audio_io::RATE as usize * 2;
            for frame in samples.chunks(chunk) {
                if stop_clone.load(Ordering::Relaxed) || !playing_clone.load(Ordering::Relaxed) {
                    break;
                }
                if output.write(frame).is_err() {
                    break;
                }
            }
            let _ = output.drain();
        });
        let mut playback = Self {
            state,
            stop,
            playing,
            thread: Some(thread),
        };
        playback.state.loading = false;
        playback.state.playing = true;
        playback
    }

    pub fn command(&mut self, command: VoiceMessageCommand) {
        match command {
            VoiceMessageCommand::Play => {
                self.state.playing = true;
                self.state.ended = false;
                self.playing.store(true, Ordering::Relaxed);
            }
            VoiceMessageCommand::Pause => {
                self.state.playing = false;
                self.playing.store(false, Ordering::Relaxed);
            }
            VoiceMessageCommand::Stop => {
                // Dropping stops it; the caller (`Chat::stop_voice_message`)
                // takes the playback out.
            }
        }
    }

    pub fn finished(&self) -> bool {
        self.thread
            .as_ref()
            .is_some_and(|thread| thread.is_finished())
    }
}

impl Drop for VoicePlayback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            // The thread checks `stop` between chunks, so this join is quick.
            let _ = thread.join();
        }
    }
}
