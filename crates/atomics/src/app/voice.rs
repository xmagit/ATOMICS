use super::*;

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
                if atomics_core::voice::is_silent(&samples) {
                    let hint = if cfg!(target_os = "macos") {
                        "check mute and System Settings › Privacy & Security › Microphone"
                    } else {
                        "check the default input device, mute and microphone access in your desktop audio settings"
                    };
                    return Err(format!("the microphone recorded only silence ({hint})"));
                }
                let (samples, rate) = atomics_core::voice::downsample(&samples, rate, atomics_core::voice::SPEECH_RATE);
                let wav = atomics_core::voice::wav(&samples, rate);
                let key = config.api_key(env_var).map_err(|e| format!("{e:#}"))?;
                atomics_core::voice::transcribe(&config, &key, &wav).map_err(|e| format!("{e:#}"))
            });
            let _ = tx.send(VoiceEvent::Transcript(transcript));
            ctx.request_repaint();
        });
    }

    pub(super) fn send_voice(&mut self, _ctx: &egui::Context, text: &str) {
        if text.is_empty() {
            self.runtime.toast("Voice: heard nothing");
            return;
        }
        let text = text.to_string();
        let quoted = format!("“{}”", truncate(&text, 60));
        match self.runtime.config.voice.target {
            VoiceTarget::Chief => self.post(move |state| {
                let started = state.send_to_chief(&text);
                match started {
                    Ok(()) => state.toast(format!("Sent to the chief: {quoted}")),
                    Err(error) => state.client_events.push(ClientEvent::VoiceFailed { text, error }),
                }
            }),
            VoiceTarget::Active => {
                if let Some(index) = self
                    .runtime
                    .active_index()
                    .filter(|&i| self.runtime.sessions[i].is_agent() && self.runtime.sessions[i].can_receive())
                {
                    self.post_session(index, move |state, index| {
                        if state.deliver(index, &text) != Delivery::Unavailable {
                            state.toast(format!("Sent to {}: {quoted}", state.sessions[index].title));
                        } else {
                            state
                                .client_events
                                .push(ClientEvent::VoiceFailed { text, error: "No active agent to send to".into() });
                        }
                    });
                } else {
                    self.voice_failed(&text, "No active agent to send to");
                }
            }
        }
    }
    pub(super) fn voice_failed(&mut self, text: &str, error: &str) {
        if !self.client.steer_text.trim().is_empty() {
            self.client.steer_text.push('\n');
        }
        self.client.steer_text.push_str(text);
        self.client.focus_steer = true;
        self.runtime.toast(format!("{error}; the transcript is in the steer bar"));
    }
}
