use crate::protocol::client::VncClient;
use crate::protocol::text_input::TypedText;
use crate::sessions::SessionIdentity;
use crossbeam_channel::{Receiver, Sender};
use std::thread;

#[derive(Clone, Debug)]
pub struct ConnectionParams {
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// The saved session selected by the user, independent of its endpoint.
    pub source_session: Option<SessionIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub username: Option<String>,
    pub password: String,
}

/// A single changed rectangle: tightly-packed `w*h*4` RGBA bytes at (x, y).
pub struct FbRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub rgba: Vec<u8>,
}

/// Messages sent from the VNC background thread to the UI thread.
pub enum VncEvent {
    DesktopSize(u32, u32),
    /// Only the rectangles that changed this update — applied as partial
    /// texture updates so we never re-upload the whole framebuffer.
    FramebufferRects(Vec<FbRect>),
    DesktopName(String),
    /// Server sent clipboard text.
    ClipboardText(String),
    /// Nonfatal feedback for a user-requested clipboard operation.
    ClipboardStatus {
        message: String,
        is_error: bool,
    },
    /// Server requires VNC authentication — UI should prompt for password.
    NeedPassword,
    /// Ask only for missing fields after the server selects its auth subtype.
    NeedCredentials {
        username: Option<String>,
        needs_username: bool,
        needs_password: bool,
    },
    Error(String),
    Disconnected,
}

/// Commands sent from the UI thread to the VNC background thread.
pub enum VncCommand {
    KeyEvent {
        down: bool,
        keysym: u32,
    },
    PointerEvent {
        buttons: u8,
        x: u16,
        y: u16,
    },
    /// Send clipboard text to the server.
    SetClipboard(String),
    /// Type prevalidated text using ordinary RFB key taps, without clipboard
    /// protocol support on the server.
    TypeText(TypedText),
    /// Response to NeedPassword: provide the password to the waiting auth step.
    ProvidePassword(String),
    ProvideCredentials(Credentials),
    Disconnect,
}

pub struct VncConnection {
    pub event_rx: Receiver<VncEvent>,
    pub cmd_tx: Sender<VncCommand>,
}

impl VncConnection {
    pub fn connect(params: ConnectionParams) -> Self {
        let (event_tx, event_rx) = crossbeam_channel::unbounded::<VncEvent>();
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<VncCommand>();

        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");

            rt.block_on(async {
                match VncClient::connect(&params, event_tx.clone(), cmd_rx).await {
                    Ok(()) => {
                        let _ = event_tx.send(VncEvent::Disconnected);
                    }
                    Err(e) => {
                        let _ = event_tx.send(VncEvent::Error(e.to_string()));
                    }
                }
            });
        });

        Self { event_rx, cmd_tx }
    }

    pub fn send_key(&self, down: bool, keysym: u32) {
        let _ = self.cmd_tx.send(VncCommand::KeyEvent { down, keysym });
    }

    pub fn send_pointer(&self, buttons: u8, x: u16, y: u16) {
        let _ = self.cmd_tx.send(VncCommand::PointerEvent { buttons, x, y });
    }

    pub fn send_clipboard(&self, text: String) -> Result<(), String> {
        self.cmd_tx
            .send(VncCommand::SetClipboard(text))
            .map_err(|_| "VNC connection is no longer available".to_string())
    }

    pub fn type_text(&self, text: TypedText) -> Result<(), String> {
        self.cmd_tx
            .send(VncCommand::TypeText(text))
            .map_err(|_| "VNC connection is no longer available".to_string())
    }

    pub fn provide_credentials(&self, credentials: Credentials) {
        let _ = self
            .cmd_tx
            .send(VncCommand::ProvideCredentials(credentials));
    }

    pub fn disconnect(&self) {
        let _ = self.cmd_tx.send(VncCommand::Disconnect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnected_clipboard_actions_report_errors_without_payloads() {
        let (_event_tx, event_rx) = crossbeam_channel::unbounded();
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        drop(cmd_rx);
        let connection = VncConnection { event_rx, cmd_tx };
        let message = "private clipboard value";
        let expected = "VNC connection is no longer available";
        assert_eq!(
            connection.send_clipboard(message.into()).unwrap_err(),
            expected
        );
        assert_eq!(
            connection
                .type_text(TypedText::new(message).unwrap())
                .unwrap_err(),
            expected
        );
    }
}
