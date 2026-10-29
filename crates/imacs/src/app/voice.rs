use super::*;

impl State {}

impl Desktop<'_> {
    pub(super) fn toggle_voice(&mut self) {
        match self.client.voice {
            Voice::Idle => {
                let cancelled = Arc::new(AtomicBool::new(false));
                self.client.voice = Voice::Opening(cancelled.clone());
                let (config, tx, ctx) =
                    (self.runtime.config.voice.clone(), self.client.voice_tx.clone(), self.runtime.ctx.clone());
                std::thread::spawn(move || {
                    let recording = Recorder::start().and_then(|recorder| {
                        config.api_key(env_var).map_err(|err| format!("{err:#}"))?;
                        Ok(recorder)
                    });
                    if !cancelled.load(Ordering::Relaxed) {
                        let _ = tx.send(VoiceEvent::Opened(cancelled, recording));
                        ctx.request_repaint();
                    }
                });
            }
            Voice::Opening(ref cancelled) => {
                cancelled.store(true, Ordering::Relaxed);
                self.client.voice = Voice::Idle;
                self.runtime.toast("Voice: recording cancelled");
            }
            Voice::Recording(_) => self.stop_voice(),
            Voice::Transcribing => self.runtime.toast("Voice: still transcribing the last recording"),
        }
    }

    pub(super) fn stop_voice(&mut self) {
        let Voice::Recording(recorder) = std::mem::replace(&mut self.client.voice, Voice::Transcribing) else { return };
        let recording = recorder.finish();
        let (config, tx, ctx) =
            (self.runtime.config.voice.clone(), self.client.voice_tx.clone(), self.runtime.ctx.clone());
        std::thread::spawn(move || {
            let transcript = recording.and_then(|(samples, rate)| {
                if samples.len() < rate as usize / 5 {
                    return Err("nothing recorded".to_string());
                }
                if imacs_core::voice::is_silent(&samples) {
                    return Err("the microphone recorded only silence (muted, or no microphone access in System \
                                Settings › Privacy & Security?)"
                        .to_string());
                }
                let (samples, rate) = imacs_core::voice::downsample(&samples, rate, imacs_core::voice::SPEECH_RATE);
                let wav = imacs_core::voice::wav(&samples, rate);
                let key = config.api_key(env_var).map_err(|e| format!("{e:#}"))?;
                imacs_core::voice::transcribe(&config, &key, &wav).map_err(|e| format!("{e:#}"))
            });
            let _ = tx.send(VoiceEvent::Transcript(transcript));
            ctx.request_repaint();
        });
    }

    pub(super) fn send_voice(&mut self, ctx: &egui::Context, text: &str) {
        if text.is_empty() {
            self.runtime.toast("Voice: heard nothing");
            return;
        }
        let quoted = format!("“{}”", truncate(text, 60));
        let failed = match self.runtime.config.voice.target {
            VoiceTarget::Chief => {
                let started = match self.runtime.sessions.iter().position(|s| s.chief && s.can_receive()) {
                    Some(index) => {
                        self.runtime.deliver(index, text);
                        Ok(())
                    }
                    None => {
                        self.runtime.sessions.retain(|s| !s.chief);
                        self.runtime.start_chief(ctx, Some(text.to_string())).map(|_| ())
                    }
                };
                match started {
                    Ok(()) => {
                        self.runtime.chief_open = true;
                        self.runtime.focus_terminal =
                            self.runtime.sessions.iter().find(|s| s.chief).map(|s| s.id.clone());
                        self.runtime.toast(format!("Sent to the chief: {quoted}"));
                        None
                    }
                    Err(err) => Some(err),
                }
            }
            VoiceTarget::Active => {
                match self
                    .runtime
                    .active_index()
                    .filter(|&i| self.runtime.sessions[i].is_agent() && self.runtime.sessions[i].can_receive())
                {
                    Some(index) if self.runtime.deliver(index, text) != Delivery::Unavailable => {
                        self.runtime.toast(format!("Sent to {}: {quoted}", self.runtime.sessions[index].title));
                        None
                    }
                    _ => Some("No active agent to send to".to_string()),
                }
            }
        };
        if let Some(err) = failed {
            if !self.client.steer_text.trim().is_empty() {
                self.client.steer_text.push('\n');
            }
            self.client.steer_text.push_str(text);
            self.client.focus_steer = true;
            self.runtime.toast(format!("{err}; the transcript is in the steer bar"));
        }
    }
}
