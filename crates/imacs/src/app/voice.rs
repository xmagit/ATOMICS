use super::*;

impl State {
    /// Push-to-talk: the first press records, the second transcribes and sends.
    pub(super) fn toggle_voice(&mut self) {
        match self.voice {
            Voice::Idle => {
                let cancelled = Arc::new(AtomicBool::new(false));
                self.voice = Voice::Opening(cancelled.clone());
                let (config, tx, ctx) = (self.config.voice.clone(), self.voice_tx.clone(), self.ctx.clone());
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
                self.voice = Voice::Idle;
                self.toast("Voice: recording cancelled");
            }
            Voice::Recording(_) => self.stop_voice(),
            Voice::Transcribing => self.toast("Voice: still transcribing the last recording"),
        }
    }

    /// Stops recording and transcribes on a background thread.
    pub(super) fn stop_voice(&mut self) {
        let Voice::Recording(recorder) = std::mem::replace(&mut self.voice, Voice::Transcribing) else { return };
        let recording = recorder.finish();
        let (config, tx, ctx) = (self.config.voice.clone(), self.voice_tx.clone(), self.ctx.clone());
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

    /// Sends a transcript the way the steer bar does: to the chief (started and shown
    /// if needed) or to the active agent. Text that cannot be sent lands in the steer bar.
    pub(super) fn send_voice(&mut self, ctx: &egui::Context, text: &str) {
        if text.is_empty() {
            self.toast("Voice: heard nothing");
            return;
        }
        let quoted = format!("“{}”", truncate(text, 60));
        let failed = match self.config.voice.target {
            VoiceTarget::Chief => {
                let started = match self.sessions.iter().position(|s| s.chief && s.can_receive()) {
                    Some(index) => {
                        self.deliver(index, text);
                        Ok(())
                    }
                    None => {
                        self.sessions.retain(|s| !s.chief);
                        self.start_chief(ctx, Some(text.to_string())).map(|_| ())
                    }
                };
                match started {
                    Ok(()) => {
                        self.chief_open = true;
                        self.focus_terminal = self.sessions.iter().find(|s| s.chief).map(|s| s.id.clone());
                        self.toast(format!("Sent to the chief: {quoted}"));
                        None
                    }
                    Err(err) => Some(err),
                }
            }
            VoiceTarget::Active => {
                match self.active_index().filter(|&i| self.sessions[i].is_agent() && self.sessions[i].can_receive()) {
                    Some(index) if self.deliver(index, text) != Delivery::Unavailable => {
                        self.toast(format!("Sent to {}: {quoted}", self.sessions[index].title));
                        None
                    }
                    _ => Some("No active agent to send to".to_string()),
                }
            }
        };
        if let Some(err) = failed {
            if !self.steer_text.trim().is_empty() {
                self.steer_text.push('\n');
            }
            self.steer_text.push_str(text);
            self.focus_steer = true;
            self.toast(format!("{err}; the transcript is in the steer bar"));
        }
    }
}
