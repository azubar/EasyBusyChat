#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameError {
    TooLarge,
    InvalidUtf8,
}

pub struct NdjsonDecoder {
    buffer: Vec<u8>,
    max_frame_bytes: usize,
}

pub fn command_log_summary(action: &str, payload: &serde_json::Value) -> String {
    if matches!(
        action,
        "setConfig" | "executeScript" | "ensureContact" | "openChatByIdentity"
    ) {
        return format!("action={action} payload=redacted");
    }
    let payload_bytes = serde_json::to_vec(payload).map_or(0, |bytes| bytes.len());
    format!("action={action} payload_bytes={payload_bytes}")
}

impl NdjsonDecoder {
    pub fn new(max_frame_bytes: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_frame_bytes: max_frame_bytes.max(1),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, FrameError> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();

        while let Some(newline) = self.buffer.iter().position(|byte| *byte == b'\n') {
            if newline > self.max_frame_bytes {
                self.buffer.clear();
                return Err(FrameError::TooLarge);
            }

            let mut frame = self.buffer.drain(..=newline).collect::<Vec<_>>();
            frame.pop();
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            if frame.is_empty() {
                continue;
            }
            let text = String::from_utf8(frame).map_err(|_| FrameError::InvalidUtf8)?;
            frames.push(text);
        }

        if self.buffer.len() > self.max_frame_bytes {
            self.buffer.clear();
            return Err(FrameError::TooLarge);
        }
        Ok(frames)
    }

    pub fn pending_bytes(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const FRONTEND: &str = include_str!("../../ui/frontend/index.html");

    #[test]
    fn decoder_preserves_utf8_split_between_reads() {
        let mut decoder = NdjsonDecoder::new(64);

        assert!(decoder.push(&[0xD0]).unwrap().is_empty());
        let frames = decoder.push(&[0x96, b'\n']).unwrap();

        assert_eq!(frames, vec!["Ж"]);
    }

    #[test]
    fn decoder_returns_every_complete_frame_and_keeps_tail() {
        let mut decoder = NdjsonDecoder::new(64);

        assert_eq!(decoder.push(b"one\ntwo\npar").unwrap(), ["one", "two"]);
        assert_eq!(decoder.push(b"tial\n").unwrap(), ["partial"]);
    }

    #[test]
    fn decoder_rejects_oversized_unterminated_frame() {
        let mut decoder = NdjsonDecoder::new(4);

        assert_eq!(decoder.push(b"12345").unwrap_err(), FrameError::TooLarge);
    }

    #[test]
    fn decoder_rejects_invalid_utf8_only_after_complete_frame() {
        let mut decoder = NdjsonDecoder::new(64);

        assert!(decoder.push(&[0xFF]).unwrap().is_empty());
        assert_eq!(decoder.push(b"\n").unwrap_err(), FrameError::InvalidUtf8);
    }

    #[test]
    fn command_summary_never_contains_set_config_secrets() {
        let payload = serde_json::json!({
            "companyId": "company",
            "accessToken": "top-secret-token",
        });

        let summary = command_log_summary("setConfig", &payload);

        assert!(summary.contains("setConfig"));
        assert!(summary.contains("redacted"));
        assert!(!summary.contains("top-secret-token"));
    }

    #[test]
    fn identity_command_summary_never_contains_phone_or_token() {
        let payload = serde_json::json!({
            "provider": "telegram",
            "identity": { "type": "phone", "value": "+380501234567" },
            "accessToken": "top-secret-token",
        });
        let summary = command_log_summary("openChatByIdentity", &payload);
        assert!(summary.contains("redacted"));
        assert!(!summary.contains("+380501234567"));
        assert!(!summary.contains("top-secret-token"));
    }

    #[test]
    fn frontend_readiness_contract_is_ordered_and_correlated() {
        let on_config_start = FRONTEND.find("window.onConfigReady").unwrap();
        let on_config_end = FRONTEND[on_config_start..]
            .find("window.hostNavigating")
            .unwrap()
            + on_config_start;
        let on_config = &FRONTEND[on_config_start..on_config_end];
        assert!(on_config.contains("postHostEvent('webviewReady'"));
        assert!(!on_config.contains("postHostEvent('browserReady'"));
        assert!(!on_config.contains("postHostEvent('reactReady'"));

        let render = FRONTEND
            .find("window.renderChatsApp('chat_container'")
            .unwrap();
        let react_ready = FRONTEND.find("postHostEvent('reactReady'").unwrap();
        assert!(react_ready > render);
        assert!(FRONTEND.contains("disableBrowserNotifications: chatSettings.muted"));

        let open_chat_start = FRONTEND.find("onOpenChat: async function").unwrap();
        let open_chat_end = FRONTEND[open_chat_start..]
            .find("function showMessage")
            .unwrap()
            + open_chat_start;
        let open_chat = &FRONTEND[open_chat_start..open_chat_end];
        assert!(open_chat.contains("postHostEvent('chatOpened'"));
        assert!(open_chat.contains("provider:"));
        assert!(open_chat.contains("accountId:"));
        assert!(open_chat.contains("chatId:"));
    }
}
pub mod auth_profile;
