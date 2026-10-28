use crate::connection::{ConnectionParams, Credentials, VncConnection, VncEvent};
use crate::protocol::text_input::{char_to_keysym, TypedText};
use crate::sessions::{self, Session, SessionIdentity};
use crate::transfer::{RemoteEntry, TransferEvent, TransferParams, TransferSession};
use egui::{Color32, Key, Modifiers, Rect, Sense, TextureHandle, TextureOptions, Vec2};

mod profile_cpu;
pub use profile_cpu::run_cpu_profile;

pub struct VncApp {
    profile: profile_cpu::UiProfile,
    persist_sessions: bool,
    // Toolbar address field ("host" or "host:port")
    address_input: String,

    // Active VNC session
    connection: Option<VncConnection>,
    texture: Option<TextureHandle>,
    fb_width: u32,
    fb_height: u32,
    desktop_name: String,
    status_msg: String,

    // Password dialog — only shown when server explicitly requests auth
    show_password_dialog: bool,
    pending_username: String,
    pending_password: String,
    needs_username: bool,
    needs_password: bool,

    // Sessions popup window
    show_sessions: bool,
    sessions: Vec<Session>,
    session_selected: Option<usize>,
    sf_name: String,
    sf_host: String,
    sf_port: String,
    sf_username: String,
    sf_password: String,
    sf_editing: Option<usize>,

    // Input state
    keyboard: RemoteKeyboard,
    last_mouse_pos: (u16, u16),
    clipboard: Option<arboard::Clipboard>,
    focus_remote_after_paste: bool,

    // One-time window sizing
    first_frame: bool,

    // Active VNC connection details (for the SFTP side-channel + auto-save)
    current_host: String,
    current_port: u16,
    current_username: Option<String>,
    current_password: String,
    current_source_session: Option<SessionIdentity>,

    // SFTP file-transfer panel (MobaXterm-style)
    transfer: Option<TransferSession>,
    show_transfer: bool,
    ssh_user: String,
    ssh_pass: String,
    transfer_status: String,
    remote_path: String,
    remote_entries: Vec<RemoteEntry>,
    remote_selected: Option<usize>,
}

impl VncApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, initial: Option<ConnectionParams>) -> Self {
        Self::from_initial(initial)
    }

    fn from_initial(initial: Option<ConnectionParams>) -> Self {
        Self::with_sessions(initial, sessions::load())
    }

    fn with_sessions(initial: Option<ConnectionParams>, sessions: Vec<Session>) -> Self {
        let mut app = Self {
            profile: Default::default(),
            persist_sessions: true,
            address_input: String::new(),
            connection: None,
            texture: None,
            fb_width: 0,
            fb_height: 0,
            desktop_name: String::new(),
            status_msg: "Not connected".to_string(),
            show_password_dialog: false,
            pending_username: String::new(),
            pending_password: String::new(),
            needs_username: false,
            needs_password: false,
            show_sessions: false,
            sessions,
            session_selected: None,
            sf_name: String::new(),
            sf_host: String::new(),
            sf_port: "5900".to_string(),
            sf_username: String::new(),
            sf_password: String::new(),
            sf_editing: None,
            keyboard: RemoteKeyboard::default(),
            last_mouse_pos: (0, 0),
            clipboard: None,
            focus_remote_after_paste: false,
            first_frame: true,
            current_host: String::new(),
            current_port: 5900,
            current_username: None,
            current_password: String::new(),
            current_source_session: None,
            transfer: None,
            show_transfer: false,
            ssh_user: String::new(),
            ssh_pass: String::new(),
            transfer_status: String::new(),
            remote_path: String::new(),
            remote_entries: Vec::new(),
            remote_selected: None,
        };

        if let Some(params) = initial {
            app.address_input = if params.port == 5900 {
                params.host.clone()
            } else {
                format!("{}:{}", params.host, params.port)
            };
            app.do_connect(params);
        }

        app
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn parse_address(input: &str) -> (String, u16) {
        let s = input.trim();
        if let Some(pos) = s.rfind(':') {
            let port_part = &s[pos + 1..];
            if let Ok(port) = port_part.parse::<u16>() {
                return (s[..pos].to_string(), port);
            }
        }
        (s.to_string(), 5900)
    }

    /// Start a connection immediately — no pre-connect dialog.
    /// The server will tell us which missing credentials it needs.
    fn do_connect(&mut self, params: ConnectionParams) {
        let ConnectionParams {
            host,
            port,
            username,
            password,
            source_session,
        } = params;
        self.status_msg = format!("Connecting to {}:{}…", host, port);
        self.desktop_name = String::new();
        self.show_password_dialog = false;
        self.pending_username.clear();
        self.pending_password.clear();
        self.keyboard = RemoteKeyboard::default();
        self.focus_remote_after_paste = false;
        self.current_host = host.clone();
        self.current_port = port;
        self.current_username = username.clone();
        self.current_password = password.clone().unwrap_or_default();
        self.current_source_session = source_session.clone();
        // Remember the SSH username saved for this host, for the SFTP panel.
        self.ssh_user = self
            .sessions
            .iter()
            .find(|s| s.host == host && !s.ssh_user.is_empty())
            .map(|s| s.ssh_user.clone())
            .unwrap_or_default();
        self.connection = Some(VncConnection::connect(ConnectionParams {
            host,
            port,
            username,
            password,
            source_session,
        }));
    }

    /// Open a saved session in its own OS window by launching a new process.
    /// Passing the session key (not the password) keeps credentials off the
    /// command line — the child loads the password from the saved sessions file.
    /// Launch a new instance (window) of this app with the given args.
    fn spawn_window(args: &[&str]) {
        if let Ok(exe) = std::env::current_exe() {
            let mut cmd = std::process::Command::new(exe);
            cmd.args(args);
            // Don't spawn an extra console window for the child on Windows.
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                const CREATE_NO_WINDOW: u32 = 0x0800_0000;
                cmd.creation_flags(CREATE_NO_WINDOW);
            }
            let _ = cmd.spawn();
        }
    }

    /// Open a saved session in its own window. Passing the session key (not the
    /// password) keeps credentials off the command line — the child loads the
    /// password from the saved sessions file.
    fn open_session_window(session: &Session) {
        if !session.id.is_empty() {
            Self::spawn_window(&["--session-id", &session.id]);
            return;
        }
        let key = if session.name.is_empty() {
            &session.host
        } else {
            &session.name
        };
        Self::spawn_window(&["--session", key]);
    }

    /// Open an ad-hoc host:port connection in its own window. The child loads a
    /// saved password for that host:port if one exists, else prompts.
    fn open_host_window(host: &str, port: u16) {
        Self::spawn_window(&["--host", host, "--port", &port.to_string()]);
    }

    /// Tear down the SFTP side-channel, if any.
    fn close_transfer(&mut self) {
        if let Some(t) = &self.transfer {
            t.disconnect();
        }
        self.transfer = None;
        self.show_transfer = false;
        self.remote_entries.clear();
        self.remote_selected = None;
        self.remote_path.clear();
        self.transfer_status.clear();
    }

    fn disconnect(&mut self, ctx: &egui::Context) {
        if let Some(conn) = &self.connection {
            conn.disconnect();
        }
        self.connection = None;
        self.texture = None;
        self.show_password_dialog = false;
        self.focus_remote_after_paste = false;
        self.status_msg = "Disconnected".to_string();
        self.desktop_name = String::new();
        self.close_transfer();
        ctx.send_viewport_cmd(egui::ViewportCommand::Title("IronVNC".to_string()));
    }

    fn update_status(&mut self) {
        self.status_msg = if self.desktop_name.is_empty() {
            format!("Connected  {}×{}", self.fb_width, self.fb_height)
        } else {
            format!(
                "{}  {}×{}",
                self.desktop_name, self.fb_width, self.fb_height
            )
        };
    }

    fn local_clipboard(&mut self) -> Result<&mut arboard::Clipboard, String> {
        // Retry on an explicit action if a clipboard backend was unavailable.
        if self.clipboard.is_none() {
            self.clipboard = Some(
                arboard::Clipboard::new()
                    .map_err(|error| format!("Cannot access the local clipboard: {error}"))?,
            );
        }
        Ok(self.clipboard.as_mut().unwrap())
    }

    fn clipboard_text(&mut self) -> Result<String, String> {
        let text = self
            .local_clipboard()?
            .get_text()
            .map_err(|error| format!("Cannot read text from the local clipboard: {error}"))?;
        if text.is_empty() {
            return Err("The local clipboard contains no text".to_string());
        }
        Ok(text)
    }

    fn paste_text(&mut self, text: &str) -> Result<(), String> {
        if !self.can_paste() {
            return Err(
                "Wait for the remote desktop and close local dialogs before pasting".into(),
            );
        }
        let conn = self
            .connection
            .as_ref()
            .ok_or_else(|| "Not connected to a VNC server".to_string())?;
        // Validate the entire text before releasing keys or sending any input.
        let text = TypedText::new(text).map_err(|error| error.to_string())?;
        for (down, keysym) in self.keyboard.release_all() {
            conn.send_key(down, keysym);
        }
        conn.type_text(text)?;
        self.focus_remote_after_paste = true;
        self.status_msg = "Sending clipboard text for typing…".to_string();
        Ok(())
    }

    fn can_paste(&self) -> bool {
        self.connection.is_some()
            && self.texture.is_some()
            && !self.show_password_dialog
            && !self.show_sessions
    }

    // ── Event processing ──────────────────────────────────────────────────────

    fn process_events(&mut self, ctx: &egui::Context) {
        // Keep one finite snapshot: its completion credits remain held during
        // collection. Releasing them in try_iter would let a fast producer turn
        // this UI pass (and egui's pending texture deltas) into an unbounded drain.
        let events: Vec<VncEvent> = match &self.connection {
            Some(conn) => conn.event_rx.try_iter().take(conn.event_rx.len()).collect(),
            None => return,
        };

        for event in events {
            match event {
                VncEvent::DesktopSize(w, h) => {
                    let resized = self.fb_width != w || self.fb_height != h;
                    self.fb_width = w;
                    self.fb_height = h;
                    // (Re)allocate the full texture as black; partial updates
                    // paint into it as rectangles arrive.
                    if resized || self.texture.is_none() {
                        let black = egui::ColorImage::new([w as usize, h as usize], Color32::BLACK);
                        self.texture =
                            Some(ctx.load_texture("vnc-fb", black, TextureOptions::LINEAR));
                    }
                    self.update_status();
                }
                VncEvent::DesktopName(name) => {
                    self.desktop_name = name.clone();
                    self.update_status();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title(format!(
                        "{} — IronVNC",
                        name
                    )));
                    // Connection succeeded — persist it (with password) so it
                    // can be reconnected by double-click without re-entering.
                    self.remember_session();
                }
                VncEvent::FramebufferRects(rects) => {
                    self.profile.pixels_start();
                    // Apply each changed rectangle as a partial texture update —
                    // no full-frame re-upload.
                    if let Some(tex) = &mut self.texture {
                        for r in rects {
                            // Guard against a rect that would fall outside the
                            // current texture (e.g. an update racing a resize),
                            // which would otherwise panic in set_partial.
                            if r.w == 0
                                || r.h == 0
                                || r.x + r.w > self.fb_width
                                || r.y + r.h > self.fb_height
                                || (r.w * r.h * 4) as usize != r.rgba.len()
                            {
                                continue;
                            }
                            let conversion_start = self
                                .profile
                                .active
                                .as_ref()
                                .map(|_| std::time::Instant::now());
                            let image = egui::ColorImage::from_rgba_unmultiplied(
                                [r.w as usize, r.h as usize],
                                &r.rgba,
                            );
                            self.profile.add_conversion(conversion_start);
                            let manager_start = self
                                .profile
                                .active
                                .as_ref()
                                .map(|_| std::time::Instant::now());
                            tex.set_partial(
                                [r.x as usize, r.y as usize],
                                image,
                                TextureOptions::LINEAR,
                            );
                            self.profile.add_manager(manager_start);
                        }
                    }
                    ctx.request_repaint();
                }
                VncEvent::ProfileUpdate(profile) => self.profile.start(profile),
                VncEvent::UpdateComplete(permit) => {
                    self.profile.finish();
                    drop(permit);
                }
                VncEvent::ClipboardText(text) => {
                    let result = self.local_clipboard().and_then(|cb| {
                        cb.set_text(text)
                            .map_err(|error| format!("Cannot update the local clipboard: {error}"))
                    });
                    if let Err(error) = result {
                        self.status_msg = format!("Error: {error}");
                    }
                }
                VncEvent::ClipboardStatus { message, is_error } => {
                    self.status_msg = if is_error {
                        format!("Error: {message}")
                    } else {
                        message
                    };
                }
                VncEvent::NeedPassword => {
                    self.status_msg = "Waiting for password…".to_string();
                    self.pending_password.clear();
                    self.needs_username = false;
                    self.needs_password = true;
                    self.show_password_dialog = true;
                }
                VncEvent::NeedCredentials {
                    username,
                    needs_username,
                    needs_password,
                } => {
                    self.status_msg = "Waiting for credentials…".to_string();
                    self.pending_username = username.unwrap_or_default();
                    self.pending_password.clear();
                    self.needs_username = needs_username;
                    self.needs_password = needs_password;
                    self.show_password_dialog = true;
                }
                VncEvent::Error(e) => {
                    self.status_msg = format!("Error: {e}");
                    self.connection = None;
                    self.texture = None;
                    self.show_password_dialog = false;
                    self.focus_remote_after_paste = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title("IronVNC".to_string()));
                }
                VncEvent::Disconnected => {
                    self.status_msg = "Disconnected".to_string();
                    self.connection = None;
                    self.texture = None;
                    self.show_password_dialog = false;
                    self.focus_remote_after_paste = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Title("IronVNC".to_string()));
                }
            }
        }
    }

    // ── SFTP transfer events ──────────────────────────────────────────────────

    fn process_transfer_events(&mut self) {
        let events: Vec<TransferEvent> = match &self.transfer {
            Some(t) => t.event_rx.try_iter().collect(),
            None => return,
        };
        for event in events {
            match event {
                TransferEvent::Connected { home } => {
                    self.remote_path = home;
                    self.transfer_status = "Connected".to_string();
                }
                TransferEvent::DirListing { path, entries } => {
                    self.remote_path = path;
                    self.remote_entries = entries;
                    self.remote_selected = None;
                }
                TransferEvent::Status(s) => self.transfer_status = s,
                TransferEvent::TransferDone { name } => {
                    self.transfer_status = format!("Done: {name}");
                }
                TransferEvent::Error(e) => {
                    self.transfer_status = format!("Error: {e}");
                }
                TransferEvent::Disconnected => {
                    if self.transfer.is_some() {
                        self.transfer_status = "SFTP disconnected".to_string();
                    }
                    self.transfer = None;
                }
            }
        }
    }

    // ── SFTP file browser panel (MobaXterm-style) ─────────────────────────────

    fn render_transfer_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("📁 Remote files");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").on_hover_text("Close panel").clicked() {
                    self.show_transfer = false;
                }
            });
        });
        ui.separator();

        if self.transfer.is_none() {
            self.render_ssh_login(ui);
            return;
        }

        // Path bar + navigation
        ui.horizontal(|ui| {
            if ui
                .small_button("⬆")
                .on_hover_text("Parent directory")
                .clicked()
            {
                if let Some(parent) = parent_remote(&self.remote_path) {
                    if let Some(t) = &self.transfer {
                        t.list_dir(parent);
                    }
                }
            }
            if ui.small_button("⟳").on_hover_text("Refresh").clicked() {
                let p = self.remote_path.clone();
                if let Some(t) = &self.transfer {
                    t.list_dir(p);
                }
            }
            if ui
                .small_button("＋dir")
                .on_hover_text("New folder")
                .clicked()
            {
                let base = self.remote_path.clone();
                let newdir = join_remote(&base, "new_folder");
                if let Some(t) = &self.transfer {
                    t.mkdir(newdir);
                }
            }
        });
        ui.add(
            egui::Label::new(egui::RichText::new(&self.remote_path).small().monospace()).truncate(),
        );
        ui.separator();

        // Entry list
        let mut navigate: Option<String> = None;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(ui.available_height() - 90.0)
            .show(ui, |ui| {
                let entries = self.remote_entries.clone();
                for (i, e) in entries.iter().enumerate() {
                    let icon = if e.is_dir { "📁" } else { "📄" };
                    let label = if e.is_dir {
                        format!("{icon} {}", e.name)
                    } else {
                        format!("{icon} {}  ({})", e.name, human_size(e.size))
                    };
                    let resp = ui.selectable_label(self.remote_selected == Some(i), label);
                    if resp.clicked() {
                        self.remote_selected = Some(i);
                    }
                    if resp.double_clicked() && e.is_dir {
                        navigate = Some(join_remote(&self.remote_path, &e.name));
                    }
                }
            });
        if let Some(path) = navigate {
            if let Some(t) = &self.transfer {
                t.list_dir(path);
            }
        }

        ui.separator();
        // Upload / download controls
        ui.horizontal(|ui| {
            if ui
                .button("⬆ Upload…")
                .on_hover_text("Pick local files to upload here")
                .clicked()
            {
                if let Some(files) = rfd::FileDialog::new().pick_files() {
                    let dir = self.remote_path.clone();
                    if let Some(t) = &self.transfer {
                        for f in files {
                            t.upload(f, dir.clone());
                        }
                    }
                }
            }
            let can_dl = self
                .remote_selected
                .and_then(|i| self.remote_entries.get(i))
                .map(|e| !e.is_dir)
                .unwrap_or(false);
            if ui
                .add_enabled(can_dl, egui::Button::new("⬇ Download…"))
                .clicked()
            {
                if let Some(i) = self.remote_selected {
                    if let Some(e) = self.remote_entries.get(i).cloned() {
                        let default_dir =
                            dirs::download_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
                        if let Some(dir) = rfd::FileDialog::new()
                            .set_directory(&default_dir)
                            .pick_folder()
                        {
                            let remote = join_remote(&self.remote_path, &e.name);
                            if let Some(t) = &self.transfer {
                                t.download(remote, dir);
                            }
                        }
                    }
                }
            }
        });

        ui.add_space(2.0);
        ui.add(
            egui::Label::new(egui::RichText::new(&self.transfer_status).small().weak()).truncate(),
        );
        ui.label(
            egui::RichText::new("Tip: drag files onto the window to upload")
                .small()
                .weak(),
        );
    }

    fn render_ssh_login(&mut self, ui: &mut egui::Ui) {
        ui.add_space(4.0);
        ui.label(format!(
            "Connect to {} over SSH (port 22)",
            self.current_host
        ));
        ui.label(
            egui::RichText::new("Use the machine's OS login — not the VNC password.")
                .small()
                .weak(),
        );
        ui.add_space(6.0);
        egui::Grid::new("ssh_login")
            .num_columns(2)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                ui.label("Username");
                ui.text_edit_singleline(&mut self.ssh_user);
                ui.end_row();
                ui.label("Password");
                ui.add(egui::TextEdit::singleline(&mut self.ssh_pass).password(true));
                ui.end_row();
            });
        ui.add_space(6.0);
        let can = !self.ssh_user.trim().is_empty() && !self.current_host.is_empty();
        if ui
            .add_enabled(can, egui::Button::new("Connect SFTP"))
            .clicked()
        {
            let user = self.ssh_user.trim().to_string();
            self.transfer = Some(TransferSession::connect(TransferParams {
                host: self.current_host.clone(),
                port: 22,
                username: user.clone(),
                password: self.ssh_pass.clone(),
            }));
            self.transfer_status = "Connecting…".to_string();
            self.ssh_pass.clear();
            // Remember the SSH username on the matching saved session.
            let host = self.current_host.clone();
            if let Some(s) = self.sessions.iter_mut().find(|s| s.host == host) {
                if s.ssh_user != user {
                    s.ssh_user = user;
                    let _ = sessions::save(&self.sessions);
                }
            }
        }
        if !self.transfer_status.is_empty() {
            ui.add_space(4.0);
            ui.add(
                egui::Label::new(egui::RichText::new(&self.transfer_status).small().weak())
                    .truncate(),
            );
        }
    }

    // ── Toolbar ───────────────────────────────────────────────────────────────

    fn render_toolbar(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.horizontal(|ui| {
            if self.connection.is_some() {
                if ui.button("Disconnect").clicked() {
                    self.disconnect(ctx);
                }
                // A connection object also exists while authentication is pending.
                // Do not queue text that could be typed later after login.
                let can_paste = self.can_paste();
                if ui
                    .add_enabled(can_paste, egui::Button::new("Paste text →VNC"))
                    .on_hover_text(
                        "Type local clipboard text into the remote app (up to 64 KiB). \
                         Works without clipboard sharing. Line breaks press Enter; tabs press Tab.",
                    )
                    .clicked()
                {
                    let result = self
                        .clipboard_text()
                        .and_then(|text| self.paste_text(&text));
                    if let Err(error) = result {
                        self.status_msg = format!("Error: {error}");
                    }
                }
                if ui
                    .add_enabled(can_paste, egui::Button::new("Sync clipboard"))
                    .on_hover_text(
                        "Offer local clipboard text to a server that supports clipboard sharing. \
                         Then paste in the remote app. Native macOS servers may ignore this; \
                         use Paste text instead.",
                    )
                    .clicked()
                {
                    let result = self.clipboard_text().and_then(|text| {
                        self.connection
                            .as_ref()
                            .ok_or_else(|| "Not connected to a VNC server".to_string())?
                            .send_clipboard(text)
                    });
                    match result {
                        Ok(()) => {
                            self.status_msg = "Requesting clipboard sync…".to_string();
                            self.focus_remote_after_paste = true;
                        }
                        Err(error) => self.status_msg = format!("Error: {error}"),
                    }
                }
                if ui
                    .button("Ctrl+Alt+Del")
                    .on_hover_text("Send Ctrl+Alt+Del (wakes a blank/locked Windows screen)")
                    .clicked()
                {
                    if let Some(conn) = &self.connection {
                        // Press Ctrl, Alt, Delete then release in reverse order.
                        conn.send_key(true, 0xFFE3);
                        conn.send_key(true, 0xFFE9);
                        conn.send_key(true, 0xFFFF);
                        conn.send_key(false, 0xFFFF);
                        conn.send_key(false, 0xFFE9);
                        conn.send_key(false, 0xFFE3);
                    }
                }
                let files_label = if self.show_transfer {
                    "Files ◀"
                } else {
                    "Files ▶"
                };
                if ui
                    .button(files_label)
                    .on_hover_text("SFTP file browser over SSH (drag & drop to upload)")
                    .clicked()
                {
                    self.show_transfer = !self.show_transfer;
                }
                if ui.button("Sessions").clicked() {
                    self.show_sessions = !self.show_sessions;
                }
            } else {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.address_input)
                        .hint_text("host  or  host:port")
                        .desired_width(220.0),
                );

                let want_connect = (resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)))
                    || ui
                        .add_enabled(
                            !self.address_input.trim().is_empty(),
                            egui::Button::new("Connect"),
                        )
                        .clicked();

                if want_connect && !self.address_input.trim().is_empty() {
                    let (host, port) = Self::parse_address(&self.address_input);
                    // Open the connection in its own window (this window stays a
                    // launcher, so multiple sessions run side by side).
                    Self::open_host_window(&host, port);
                    self.address_input.clear();
                }

                if ui.button("Sessions").clicked() {
                    self.show_sessions = !self.show_sessions;
                }
            }
        });
    }

    // ── Authentication dialog (only missing fields requested by the server) ──

    fn render_password_dialog(&mut self, ctx: &egui::Context) {
        let mut wants_ok = false;
        let mut wants_cancel = false;

        egui::Window::new("Credentials Required")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.set_min_width(280.0);
                ui.label("Enter the credentials required by this server.");
                ui.add_space(6.0);

                if self.needs_username {
                    ui.horizontal(|ui| {
                        ui.label("Username:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.pending_username)
                                .desired_width(180.0),
                        );
                    });
                }
                if self.needs_password {
                    ui.horizontal(|ui| {
                        ui.label("Password:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.pending_password)
                                .password(true)
                                .desired_width(180.0),
                        );
                    });
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    wants_ok = ui.button("OK").clicked() || ui.input(|i| i.key_pressed(Key::Enter));
                    wants_cancel = ui.button("Cancel").clicked();
                });
            });

        if wants_ok {
            if self.needs_username {
                self.current_username = Some(self.pending_username.clone());
            }
            if self.needs_password {
                self.current_password = self.pending_password.clone();
            }
            if let Some(conn) = &self.connection {
                conn.provide_credentials(Credentials {
                    username: self.current_username.clone(),
                    password: self.current_password.clone(),
                });
            }
            self.show_password_dialog = false;
            self.status_msg = "Authenticating…".to_string();
        } else if wants_cancel {
            self.disconnect(ctx);
        }
    }

    /// Persist the current connection (host/port/password) so relaunching or
    /// reconnecting doesn't require re-entering it. Adds a new saved session or
    /// updates the password on an existing one. Called once a connection is
    /// established (so we never save a host that failed to connect).
    fn remember_session(&mut self) {
        if !self.persist_sessions {
            return;
        }
        if self.current_host.is_empty() {
            return;
        }
        // Other session windows may have saved since this window was opened.
        self.sessions = sessions::load();
        if sessions::remember_connection(
            &mut self.sessions,
            &self.current_host,
            self.current_port,
            self.current_username.clone(),
            &self.current_password,
            self.current_source_session.as_ref(),
        ) {
            let _ = sessions::save(&self.sessions);
        }
    }

    // ── Sessions popup window ─────────────────────────────────────────────────

    fn render_sessions_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_sessions;
        let mut connect_idx: Option<usize> = None;
        let mut delete_idx: Option<usize> = None;
        let mut select_idx: Option<usize> = None;

        let session_data: Vec<(usize, String, bool)> = self
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.display_label(), self.session_selected == Some(i)))
            .collect();

        egui::Window::new("Saved Sessions")
            .collapsible(false)
            .resizable(true)
            .default_width(340.0)
            .open(&mut open)
            .show(ctx, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(180.0)
                    .show(ui, |ui| {
                        if session_data.is_empty() {
                            ui.label("No sessions yet — fill in the form below.");
                        }
                        for (i, label, selected) in &session_data {
                            let i = *i;
                            ui.horizontal(|ui| {
                                let resp = ui.selectable_label(*selected, label.as_str());
                                if resp.double_clicked() {
                                    connect_idx = Some(i);
                                } else if resp.clicked() {
                                    select_idx = Some(i);
                                }
                                if ui.small_button("▶").on_hover_text("Connect").clicked() {
                                    connect_idx = Some(i);
                                }
                                if ui.small_button("✕").on_hover_text("Delete").clicked() {
                                    delete_idx = Some(i);
                                }
                            });
                        }
                    });

                ui.separator();

                ui.horizontal(|ui| {
                    if ui.button("+ New").clicked() {
                        self.session_selected = None;
                        self.sf_name.clear();
                        self.sf_host.clear();
                        self.sf_port = "5900".to_string();
                        self.sf_username.clear();
                        self.sf_password.clear();
                        self.sf_editing = None;
                    }
                });

                ui.label(if self.sf_editing.is_some() {
                    "Edit session"
                } else {
                    "New session"
                });

                egui::Grid::new("sf")
                    .num_columns(2)
                    .spacing([8.0, 4.0])
                    .show(ui, |ui| {
                        ui.label("Name");
                        ui.text_edit_singleline(&mut self.sf_name);
                        ui.end_row();
                        ui.label("Host");
                        ui.text_edit_singleline(&mut self.sf_host);
                        ui.end_row();
                        ui.label("Port");
                        ui.add(egui::TextEdit::singleline(&mut self.sf_port).desired_width(70.0));
                        ui.end_row();
                        ui.label("VNC username (optional)");
                        ui.text_edit_singleline(&mut self.sf_username);
                        ui.end_row();
                        ui.label("Password");
                        ui.add(egui::TextEdit::singleline(&mut self.sf_password).password(true));
                        ui.end_row();
                    });

                ui.horizontal(|ui| {
                    let can = !self.sf_host.is_empty();
                    if ui.add_enabled(can, egui::Button::new("Save")).clicked() {
                        self.save_session_form();
                    }
                    if ui
                        .add_enabled(can, egui::Button::new("Save & Connect"))
                        .clicked()
                    {
                        self.save_session_form();
                        if let Some(i) = self.session_selected {
                            connect_idx = Some(i);
                        }
                    }
                });
            });

        self.show_sessions = open;

        if let Some(i) = select_idx {
            self.session_selected = Some(i);
            if let Some(s) = self.sessions.get(i) {
                self.sf_name = s.name.clone();
                self.sf_host = s.host.clone();
                self.sf_port = s.port.to_string();
                self.sf_username = s.username.clone().unwrap_or_default();
                self.sf_password = s.password.clone();
                self.sf_editing = Some(i);
            }
        }
        if let Some(i) = delete_idx {
            self.sessions.remove(i);
            if self.session_selected == Some(i) {
                self.session_selected = None;
                self.sf_editing = None;
            }
            let _ = sessions::save(&self.sessions);
        }
        if let Some(i) = connect_idx {
            if let Some(s) = self.sessions.get(i).cloned() {
                // Open each saved session in its own window.
                Self::open_session_window(&s);
            }
        }
    }

    fn save_session_form(&mut self) {
        // Preserve a previously-saved SSH username when editing.
        let ssh_user = self
            .sf_editing
            .and_then(|i| self.sessions.get(i))
            .map(|s| s.ssh_user.clone())
            .unwrap_or_default();
        let session = Session {
            id: self
                .sf_editing
                .and_then(|i| self.sessions.get(i))
                .map(|s| s.id.clone())
                .filter(|id| !id.is_empty())
                .unwrap_or_else(sessions::new_id),
            name: self.sf_name.trim().to_string(),
            host: self.sf_host.trim().to_string(),
            port: self.sf_port.parse().unwrap_or(5900),
            username: (!self.sf_username.is_empty()).then(|| self.sf_username.clone()),
            password: self.sf_password.clone(),
            ssh_user,
        };
        if let Some(i) = self.sf_editing {
            if i < self.sessions.len() {
                self.sessions[i] = session;
            }
        } else {
            self.sessions.push(session);
            let last = self.sessions.len() - 1;
            self.session_selected = Some(last);
            self.sf_editing = Some(last);
        }
        let _ = sessions::save(&self.sessions);
    }

    // ── Start screen ──────────────────────────────────────────────────────────

    fn render_start_screen(&mut self, ui: &mut egui::Ui) {
        let mut connect_idx: Option<usize> = None;

        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.heading("IronVNC");
            ui.add_space(6.0);

            if self.sessions.is_empty() {
                ui.label("Enter a host above and press Connect. Connections are saved here automatically.");
                ui.add_space(16.0);
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(format!(
                            "Sessions file: {}",
                            sessions::display_path()
                        ))
                        .small()
                        .weak(),
                    )
                    .truncate(),
                );
                return;
            }

            ui.label(
                egui::RichText::new("Saved sessions — click to open in a new window")
                    .strong(),
            );
            ui.add_space(10.0);

            let session_data: Vec<(usize, String)> = self
                .sessions
                .iter()
                .enumerate()
                .map(|(i, s)| (i, s.display_label()))
                .collect();

            // Large, single-click session buttons (centered by the parent).
            for (i, label) in session_data {
                let btn = egui::Button::new(
                    egui::RichText::new(format!("🖥  {label}")).size(15.0),
                )
                .min_size(egui::vec2(420.0, 36.0));
                if ui.add(btn).on_hover_text("Open in a new window").clicked() {
                    connect_idx = Some(i);
                }
                ui.add_space(6.0);
            }

            ui.add_space(10.0);
            if ui
                .button("⚙  Manage sessions")
                .on_hover_text("Add, edit, or delete saved sessions")
                .clicked()
            {
                self.show_sessions = true;
            }

            ui.add_space(20.0);
            ui.add(
                egui::Label::new(
                    egui::RichText::new(format!("Sessions file: {}", sessions::display_path()))
                        .small()
                        .weak(),
                )
                .truncate(),
            );
        });

        if let Some(i) = connect_idx {
            if let Some(s) = self.sessions.get(i).cloned() {
                Self::open_session_window(&s);
            }
        }
    }

    // ── Framebuffer ───────────────────────────────────────────────────────────

    fn render_framebuffer(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(texture) = &self.texture else {
            ui.centered_and_justified(|ui| {
                ui.label("Waiting for first frame…");
            });
            return;
        };

        let tex_size = texture.size_vec2();
        let avail = ui.available_size();
        // Scale to fit the window preserving aspect ratio (like RealVNC's
        // "scale to window"), up or down, then center within the available area.
        let scale = (avail.x / tex_size.x).min(avail.y / tex_size.y).max(0.01);
        let display_size = tex_size * scale;

        let (area, response) = ui.allocate_exact_size(avail, Sense::click_and_drag());
        let rect = Rect::from_center_size(area.center(), display_size);

        // Give the remote screen keyboard focus so keystrokes — including Tab,
        // the arrow keys, and Escape — are sent to the server instead of driving
        // egui's own widget navigation. Grab focus on click, and auto-grab when
        // connected and nothing else (e.g. a dialog's text field) owns it.
        let restoring_paste_focus = std::mem::take(&mut self.focus_remote_after_paste);
        if response.clicked() || restoring_paste_focus {
            response.request_focus();
        } else if self.connection.is_some() && ctx.memory(|m| m.focused().is_none()) {
            response.request_focus();
        }
        let fb_focused = response.has_focus() && ctx.input(|i| i.focused);
        if fb_focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    response.id,
                    egui::EventFilter {
                        tab: true,
                        horizontal_arrows: true,
                        vertical_arrows: true,
                        escape: true,
                    },
                )
            });
        }

        ui.painter().image(
            texture.id(),
            rect,
            Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );

        // Distinct cursor while hovering the remote screen, so it's obvious that
        // input is being captured by the VNC session. Also paint a small
        // software cursor marker at the pointer so it's always visible even if
        // the server isn't rendering its own cursor into the framebuffer.
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            if let Some(p) = ui.ctx().input(|i| i.pointer.latest_pos()) {
                if rect.contains(p) {
                    let painter = ui.painter();
                    let c = Color32::from_rgb(0, 200, 255);
                    painter.line_segment(
                        [p - egui::vec2(7.0, 0.0), p + egui::vec2(7.0, 0.0)],
                        egui::Stroke::new(1.5_f32, c),
                    );
                    painter.line_segment(
                        [p - egui::vec2(0.0, 7.0), p + egui::vec2(0.0, 7.0)],
                        egui::Stroke::new(1.5_f32, c),
                    );
                    painter.circle_stroke(p, 3.0, egui::Stroke::new(1.0_f32, c));
                }
            }
        }

        let Some(conn) = &self.connection else { return };

        // ── Mouse ──
        let pointer_pos = response.interact_pointer_pos().or_else(|| {
            ctx.input(|i| i.pointer.latest_pos())
                .filter(|p| rect.contains(*p))
        });

        if let Some(pos) = pointer_pos {
            let local = pos - rect.min;
            let nx = (local.x / scale).clamp(0.0, self.fb_width as f32 - 1.0) as u16;
            let ny = (local.y / scale).clamp(0.0, self.fb_height as f32 - 1.0) as u16;
            self.last_mouse_pos = (nx, ny);

            let mut buttons = 0u8;
            if response.dragged_by(egui::PointerButton::Primary)
                || response.is_pointer_button_down_on()
            {
                buttons |= 0x01;
            }
            if ctx.input(|i| i.pointer.button_down(egui::PointerButton::Middle)) {
                buttons |= 0x02;
            }
            if ctx.input(|i| i.pointer.button_down(egui::PointerButton::Secondary)) {
                buttons |= 0x04;
            }
            conn.send_pointer(buttons, nx, ny);
        }

        // ── Scroll wheel ──
        let (mx, my) = self.last_mouse_pos;
        ctx.input(|i| {
            let scroll = i.raw_scroll_delta;
            if rect.contains(i.pointer.latest_pos().unwrap_or(egui::Pos2::ZERO)) {
                if scroll.y > 0.0 {
                    conn.send_pointer(0x08, mx, my);
                    conn.send_pointer(0x00, mx, my);
                } else if scroll.y < 0.0 {
                    conn.send_pointer(0x10, mx, my);
                    conn.send_pointer(0x00, mx, my);
                }
            }
        });

        // ── Keyboard (only when the remote screen owns focus, so typing in the
        //    Sessions form or other UI fields doesn't leak to the server) ──
        if fb_focused && !restoring_paste_focus {
            let keyboard = &mut self.keyboard;
            let events = ctx.input(|i| keyboard.translate(&i.events, i.modifiers));
            for (down, keysym) in events {
                conn.send_key(down, keysym);
            }
        } else {
            // The toolbar can be activated with Enter/Space. Its activation
            // frame belongs to the local UI, even after restoring remote focus.
            // Release held keys as well as modifiers when remote focus leaves.
            for (down, keysym) in self.keyboard.release_all() {
                conn.send_key(down, keysym);
            }
        }
    }
}

impl eframe::App for VncApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.process_events(ctx);
        self.process_transfer_events();

        // Drag & drop files onto the window → upload to the current remote dir.
        if self.transfer.is_some() {
            let dropped: Vec<std::path::PathBuf> = ctx.input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .filter_map(|f| f.path.clone())
                    .collect()
            });
            if !dropped.is_empty() {
                let dir = self.remote_path.clone();
                if let Some(t) = &self.transfer {
                    for f in dropped {
                        if f.is_file() {
                            t.upload(f, dir.clone());
                        }
                    }
                }
                self.show_transfer = true;
            }
        }

        // One-time: size window to ~80% of monitor, centered
        if self.first_frame {
            self.first_frame = false;
            if let Some(monitor) = ctx.input(|i| i.viewport().monitor_size) {
                let w = (monitor.x * 0.80).clamp(800.0, 1400.0);
                let h = (monitor.y * 0.80).clamp(560.0, 900.0);
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(Vec2::new(w, h)));
                if let Some(center_cmd) = egui::ViewportCommand::center_on_screen(ctx) {
                    ctx.send_viewport_cmd(center_cmd);
                }
            }
        }

        // Floating windows
        if self.show_password_dialog {
            self.render_password_dialog(ctx);
        }
        if self.show_sessions {
            self.render_sessions_window(ctx);
        }

        // Top toolbar
        egui::TopBottomPanel::top("toolbar")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(6.0))
            .show(ctx, |ui| {
                self.render_toolbar(ui, ctx);
            });

        // Status bar — truncates instead of overflowing
        egui::TopBottomPanel::bottom("statusbar")
            .frame(egui::Frame::side_top_panel(&ctx.style()).inner_margin(4.0))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let color = if self.status_msg.starts_with("Error") {
                        Color32::from_rgb(220, 80, 80)
                    } else if self.connection.is_some() {
                        Color32::from_rgb(100, 210, 100)
                    } else {
                        ui.visuals().weak_text_color()
                    };
                    ui.add(
                        egui::Label::new(
                            egui::RichText::new(&self.status_msg).color(color).small(),
                        )
                        .truncate(),
                    );
                });
            });

        // SFTP file-browser side panel (only while a VNC session is active)
        if self.show_transfer && self.connection.is_some() {
            egui::SidePanel::right("sftp_panel")
                .resizable(true)
                .default_width(320.0)
                .min_width(260.0)
                .show(ctx, |ui| {
                    self.render_transfer_panel(ui);
                });
        }

        // Main area
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.connection.is_some() || self.texture.is_some() {
                self.render_framebuffer(ui, ctx);
            } else {
                self.render_start_screen(ui);
            }
        });

        if self.connection.is_some() || self.transfer.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
}

/// Join a remote directory and a name with a single POSIX '/'.
fn join_remote(dir: &str, name: &str) -> String {
    if dir.ends_with('/') {
        format!("{dir}{name}")
    } else {
        format!("{dir}/{name}")
    }
}

/// Parent of a POSIX remote path, or None at the root.
fn parent_remote(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => Some("/".to_string()),
        Some(i) => Some(trimmed[..i].to_string()),
        None => None,
    }
}

/// Human-readable byte size.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

fn key_to_keysym(key: Key) -> u32 {
    match key {
        Key::Enter => 0xff0d,
        Key::Escape => 0xff1b,
        Key::Backspace => 0xff08,
        Key::Tab => 0xff09,
        Key::ArrowLeft => 0xff51,
        Key::ArrowUp => 0xff52,
        Key::ArrowRight => 0xff53,
        Key::ArrowDown => 0xff54,
        Key::Home => 0xff50,
        Key::End => 0xff57,
        Key::PageUp => 0xff55,
        Key::PageDown => 0xff56,
        Key::Delete => 0xffff,
        Key::Insert => 0xff63,
        Key::F1 => 0xffbe,
        Key::F2 => 0xffbf,
        Key::F3 => 0xffc0,
        Key::F4 => 0xffc1,
        Key::F5 => 0xffc2,
        Key::F6 => 0xffc3,
        Key::F7 => 0xffc4,
        Key::F8 => 0xffc5,
        Key::F9 => 0xffc6,
        Key::F10 => 0xffc7,
        Key::F11 => 0xffc8,
        Key::F12 => 0xffc9,
        Key::Space => b' ' as u32,
        Key::Colon => b':' as u32,
        Key::Comma => b',' as u32,
        Key::Backslash => b'\\' as u32,
        Key::Slash => b'/' as u32,
        Key::Pipe => b'|' as u32,
        Key::Questionmark => b'?' as u32,
        Key::Exclamationmark => b'!' as u32,
        Key::OpenBracket => b'[' as u32,
        Key::CloseBracket => b']' as u32,
        Key::OpenCurlyBracket => b'{' as u32,
        Key::CloseCurlyBracket => b'}' as u32,
        Key::Backtick => b'`' as u32,
        Key::Minus => b'-' as u32,
        Key::Period => b'.' as u32,
        Key::Plus => b'+' as u32,
        Key::Equals => b'=' as u32,
        Key::Semicolon => b';' as u32,
        Key::Quote => b'\'' as u32,
        Key::Num0 => b'0' as u32,
        Key::Num1 => b'1' as u32,
        Key::Num2 => b'2' as u32,
        Key::Num3 => b'3' as u32,
        Key::Num4 => b'4' as u32,
        Key::Num5 => b'5' as u32,
        Key::Num6 => b'6' as u32,
        Key::Num7 => b'7' as u32,
        Key::Num8 => b'8' as u32,
        Key::Num9 => b'9' as u32,
        Key::A => b'a' as u32,
        Key::B => b'b' as u32,
        Key::C => b'c' as u32,
        Key::D => b'd' as u32,
        Key::E => b'e' as u32,
        Key::F => b'f' as u32,
        Key::G => b'g' as u32,
        Key::H => b'h' as u32,
        Key::I => b'i' as u32,
        Key::J => b'j' as u32,
        Key::K => b'k' as u32,
        Key::L => b'l' as u32,
        Key::M => b'm' as u32,
        Key::N => b'n' as u32,
        Key::O => b'o' as u32,
        Key::P => b'p' as u32,
        Key::Q => b'q' as u32,
        Key::R => b'r' as u32,
        Key::S => b's' as u32,
        Key::T => b't' as u32,
        Key::U => b'u' as u32,
        Key::V => b'v' as u32,
        Key::W => b'w' as u32,
        Key::X => b'x' as u32,
        Key::Y => b'y' as u32,
        Key::Z => b'z' as u32,
        _ => 0,
    }
}

fn modifier_key_events(previous: Modifiers, current: Modifiers) -> Vec<(bool, u32)> {
    [
        (previous.ctrl, current.ctrl, 0xffe3),
        (previous.shift, current.shift, 0xffe1),
        (previous.alt, current.alt, 0xffe9),
        (previous.mac_cmd, current.mac_cmd, 0xffe7),
    ]
    .into_iter()
    .filter_map(|(was_down, is_down, keysym)| (was_down != is_down).then_some((is_down, keysym)))
    .collect()
}

#[derive(Default)]
struct RemoteKeyboard {
    modifiers: Modifiers,
    /// Remember the sent keysym so releasing modifiers/layout keys first
    /// cannot leave a different keysym held on the remote host.
    held_keys: Vec<(Key, u32)>,
}

impl RemoteKeyboard {
    fn set_modifiers(&mut self, modifiers: Modifiers, output: &mut Vec<(bool, u32)>) {
        output.extend(modifier_key_events(self.modifiers, modifiers));
        self.modifiers = modifiers;
    }

    fn translate(&mut self, events: &[egui::Event], current: Modifiers) -> Vec<(bool, u32)> {
        let mut output = Vec::new();
        for event in events {
            match event {
                egui::Event::Key {
                    key,
                    physical_key,
                    pressed,
                    modifiers,
                    ..
                } => {
                    self.set_modifiers(*modifiers, &mut output);
                    let identity = physical_key.unwrap_or(*key);
                    if *pressed {
                        let keysym = key_to_keysym(*key);
                        let command = modifiers.ctrl || modifiers.mac_cmd || modifiers.command;
                        // Printable keys arrive through Text during ordinary
                        // typing, and through Key only for Ctrl/Cmd shortcuts.
                        if keysym != 0 && (keysym >= 0xff00 || command) {
                            let keysym = if let Some((_, held)) =
                                self.held_keys.iter().find(|(k, _)| *k == identity)
                            {
                                *held
                            } else {
                                self.held_keys.push((identity, keysym));
                                keysym
                            };
                            output.push((true, keysym));
                        }
                    } else if let Some(index) =
                        self.held_keys.iter().position(|(k, _)| *k == identity)
                    {
                        let (_, keysym) = self.held_keys.remove(index);
                        output.push((false, keysym));
                    }
                }
                egui::Event::Text(text) => {
                    for ch in text.chars() {
                        let keysym = char_to_keysym(ch);
                        output.extend([(true, keysym), (false, keysym)]);
                    }
                }
                egui::Event::Copy | egui::Event::Cut | egui::Event::Paste(_) => {
                    let keysym = match event {
                        egui::Event::Copy => b'c',
                        egui::Event::Cut => b'x',
                        _ => b'v',
                    } as u32;
                    // The semantic event has no modifiers. Prefer the live
                    // command chord; dedicated clipboard keys use the native
                    // primary shortcut. Never read/forward Paste's text.
                    let modifiers = if current.ctrl || current.mac_cmd || current.command {
                        current
                    } else if self.modifiers.ctrl
                        || self.modifiers.mac_cmd
                        || self.modifiers.command
                    {
                        self.modifiers
                    } else {
                        Modifiers {
                            ctrl: !cfg!(target_os = "macos"),
                            mac_cmd: cfg!(target_os = "macos"),
                            command: true,
                            ..Modifiers::default()
                        }
                    };
                    self.set_modifiers(modifiers, &mut output);
                    output.extend([(true, keysym), (false, keysym)]);
                    // egui-winit still emits the later Key release. It has no
                    // matching held key here and is deliberately ignored.
                }
                _ => {}
            }
        }
        self.set_modifiers(current, &mut output);
        output
    }

    fn release_all(&mut self) -> Vec<(bool, u32)> {
        let mut output = self
            .held_keys
            .drain(..)
            .map(|(_, keysym)| (false, keysym))
            .collect();
        self.set_modifiers(Modifiers::default(), &mut output);
        output
    }
}

#[cfg(test)]
mod update_tests;

#[cfg(test)]
mod input_tests {
    use super::*;
    use crate::connection::VncCommand;

    fn connected_app(ctx: &egui::Context) -> (VncApp, crossbeam_channel::Receiver<VncCommand>) {
        let mut app = VncApp::from_initial(None);
        let (_, event_rx) = crossbeam_channel::unbounded();
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        app.connection = Some(VncConnection::from_channels(event_rx, cmd_tx));
        app.texture = Some(ctx.load_texture(
            "paste-test",
            egui::ColorImage::new([2, 2], Color32::BLACK),
            TextureOptions::NEAREST,
        ));
        (app, cmd_rx)
    }

    #[test]
    fn toolbar_paste_releases_held_keys_and_modifiers_before_one_text_command() {
        let ctx = egui::Context::default();
        let (mut app, commands) = connected_app(&ctx);
        let held = Modifiers {
            ctrl: true,
            shift: true,
            alt: true,
            mac_cmd: true,
            command: true,
        };
        app.keyboard
            .translate(&[key_event(Key::A, true, held)], held);
        app.paste_text("é中🙂\r\n\t").unwrap();
        for expected in [b'a' as u32, 0xffe3, 0xffe1, 0xffe9, 0xffe7] {
            assert!(matches!(
                commands.try_recv().unwrap(),
                VncCommand::KeyEvent { down: false, keysym } if keysym == expected
            ));
        }
        assert!(matches!(
            commands.try_recv().unwrap(),
            VncCommand::TypeText(_)
        ));
        assert!(commands.is_empty());
        assert!(app.keyboard.release_all().is_empty());
        assert!(app.focus_remote_after_paste);
    }

    #[test]
    fn invalid_toolbar_paste_preserves_keyboard_and_sends_nothing() {
        let ctx = egui::Context::default();
        let (mut app, commands) = connected_app(&ctx);
        app.keyboard
            .translate(&[key_event(Key::A, true, command())], command());
        for invalid in ["", "valid prefix\0", "valid prefix\x1b"] {
            assert!(app.paste_text(invalid).is_err());
            assert!(commands.is_empty());
            assert!(!app.focus_remote_after_paste);
        }
        assert_eq!(
            app.keyboard.release_all(),
            vec![(false, b'a' as u32), (false, 0xffe7)]
        );
    }

    #[test]
    fn paste_waits_for_a_desktop_and_does_not_queue_through_local_dialogs() {
        let ctx = egui::Context::default();
        let (mut app, commands) = connected_app(&ctx);
        app.show_password_dialog = true;
        assert!(app.paste_text("secret").is_err());
        app.show_password_dialog = false;
        app.show_sessions = true;
        assert!(app.paste_text("secret").is_err());
        app.show_sessions = false;
        app.texture = None;
        assert!(app.paste_text("secret").is_err());
        assert!(commands.is_empty());
    }

    #[test]
    fn disconnected_paste_does_not_claim_success_or_restore_focus() {
        let ctx = egui::Context::default();
        let (mut app, commands) = connected_app(&ctx);
        drop(commands);
        let old_status = app.status_msg.clone();
        assert!(app.paste_text("hello").is_err());
        assert_eq!(app.status_msg, old_status);
        assert!(!app.focus_remote_after_paste);
    }

    #[test]
    fn restoring_paste_focus_does_not_forward_toolbar_activation_keys() {
        for activation in [Key::Enter, Key::Space] {
            let ctx = egui::Context::default();
            let (mut app, commands) = connected_app(&ctx);
            app.paste_text("hello").unwrap();
            assert!(matches!(
                commands.try_recv().unwrap(),
                VncCommand::TypeText(_)
            ));
            let input = egui::RawInput {
                events: vec![key_event(activation, true, Modifiers::default())],
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.render_framebuffer(ui, ctx));
            });
            assert!(commands.is_empty(), "toolbar activation leaked to VNC");
            assert!(!app.focus_remote_after_paste);

            let input = egui::RawInput {
                events: vec![egui::Event::Text("z".into())],
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| app.render_framebuffer(ui, ctx));
            });
            for expected_down in [true, false] {
                assert!(matches!(
                    commands.try_recv().unwrap(),
                    VncCommand::KeyEvent { down, keysym: 0x7a } if down == expected_down
                ));
            }
            assert!(commands.is_empty());
        }
    }

    fn key_event(key: Key, pressed: bool, modifiers: Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed,
            repeat: false,
            modifiers,
        }
    }

    fn command() -> Modifiers {
        Modifiers {
            mac_cmd: true,
            command: true,
            ..Modifiers::default()
        }
    }

    #[test]
    fn command_a_sends_letter_and_releases_it_after_command_is_up() {
        let mut keyboard = RemoteKeyboard::default();
        assert_eq!(
            keyboard.translate(&[key_event(Key::A, true, command())], command()),
            vec![(true, 0xffe7), (true, b'a' as u32)]
        );
        assert_eq!(
            keyboard.translate(
                &[key_event(Key::A, false, Modifiers::default())],
                Modifiers::default()
            ),
            vec![(false, 0xffe7), (false, b'a' as u32)]
        );
        assert!(keyboard.release_all().is_empty());
    }

    #[test]
    fn semantic_clipboard_shortcuts_send_only_command_key_taps() {
        for (event, key, letter) in [
            (egui::Event::Copy, Key::C, b'c'),
            (egui::Event::Cut, Key::X, b'x'),
            (
                egui::Event::Paste("private local clipboard 🦀".to_string()),
                Key::V,
                b'v',
            ),
        ] {
            let mut keyboard = RemoteKeyboard::default();
            assert_eq!(
                keyboard.translate(&[event], command()),
                vec![
                    (true, 0xffe7),
                    (true, letter as u32),
                    (false, letter as u32)
                ]
            );
            // winit's release remains after its semantic press substitution.
            assert!(keyboard
                .translate(&[key_event(key, false, command())], command())
                .is_empty());
            assert_eq!(keyboard.release_all(), vec![(false, 0xffe7)]);
        }
    }

    #[test]
    fn control_clipboard_shortcuts_keep_control_semantics() {
        let control = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::default()
        };
        let mut keyboard = RemoteKeyboard::default();
        assert_eq!(
            keyboard.translate(&[egui::Event::Copy], control),
            vec![(true, 0xffe3), (true, b'c' as u32), (false, b'c' as u32)]
        );
        assert_eq!(keyboard.release_all(), vec![(false, 0xffe3)]);
    }

    #[test]
    fn ordinary_key_plus_text_is_sent_once() {
        let mut keyboard = RemoteKeyboard::default();
        let modifiers = Modifiers::default();
        let events = [
            key_event(Key::A, true, modifiers),
            egui::Event::Text("a".to_string()),
            key_event(Key::A, false, modifiers),
        ];
        assert_eq!(
            keyboard.translate(&events, modifiers),
            vec![(true, b'a' as u32), (false, b'a' as u32)]
        );
        assert!(keyboard.release_all().is_empty());
    }

    #[test]
    fn shortcuts_include_digits_and_punctuation_without_typing_them_twice() {
        for (key, letter) in [
            (Key::Num1, b'1'),
            (Key::Minus, b'-'),
            (Key::Quote, b'\''),
            (Key::Plus, b'+'),
            (Key::OpenBracket, b'['),
        ] {
            let mut keyboard = RemoteKeyboard::default();
            assert_eq!(
                keyboard.translate(
                    &[
                        key_event(key, true, command()),
                        key_event(key, false, command())
                    ],
                    command()
                ),
                vec![
                    (true, 0xffe7),
                    (true, letter as u32),
                    (false, letter as u32)
                ]
            );
        }
    }

    #[test]
    fn focus_loss_releases_held_shortcut_before_modifier() {
        let mut keyboard = RemoteKeyboard::default();
        keyboard.translate(&[key_event(Key::A, true, command())], command());
        assert_eq!(
            keyboard.release_all(),
            vec![(false, b'a' as u32), (false, 0xffe7)]
        );
        assert!(keyboard.release_all().is_empty());
    }

    #[test]
    fn punctuation_release_uses_the_original_keysym_after_shift_changes() {
        let mut keyboard = RemoteKeyboard::default();
        let shifted = Modifiers {
            shift: true,
            ..command()
        };
        let press = egui::Event::Key {
            key: Key::Plus,
            physical_key: Some(Key::Equals),
            pressed: true,
            repeat: false,
            modifiers: shifted,
        };
        let release = egui::Event::Key {
            key: Key::Equals,
            physical_key: Some(Key::Equals),
            pressed: false,
            repeat: false,
            modifiers: Modifiers::default(),
        };
        assert_eq!(
            keyboard.translate(&[press], shifted),
            vec![(true, 0xffe1), (true, 0xffe7), (true, b'+' as u32)]
        );
        assert_eq!(
            keyboard.translate(&[release], Modifiers::default()),
            vec![(false, 0xffe1), (false, 0xffe7), (false, b'+' as u32)]
        );
    }

    #[test]
    fn text_uses_latin1_and_unicode_keysyms() {
        assert_eq!(char_to_keysym('A'), 0x41);
        assert_eq!(char_to_keysym('é'), 0xe9);
        assert_eq!(char_to_keysym('中'), 0x0100_4e2d);
        assert_eq!(char_to_keysym('🦀'), 0x0101_f980);
    }

    #[test]
    fn command_maps_to_meta_and_releases_on_focus_loss() {
        let command = Modifiers {
            mac_cmd: true,
            command: true,
            ..Modifiers::default()
        };
        assert_eq!(
            modifier_key_events(Modifiers::default(), command),
            vec![(true, 0xffe7)]
        );
        assert_eq!(modifier_key_events(command, command), vec![]);
        assert_eq!(
            modifier_key_events(command, Modifiers::default()),
            vec![(false, 0xffe7)]
        );
    }

    #[test]
    fn control_command_alias_does_not_send_meta() {
        let control = Modifiers {
            ctrl: true,
            command: true,
            ..Modifiers::default()
        };
        assert_eq!(
            modifier_key_events(Modifiers::default(), control),
            vec![(true, 0xffe3)]
        );
    }
}
