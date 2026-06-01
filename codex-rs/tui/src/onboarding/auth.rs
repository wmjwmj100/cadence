#![allow(clippy::unwrap_used)]

use anyhow::Context;
use anyhow::anyhow;
use codex_core::AuthManager;
use codex_core::auth::AuthCredentialsStoreMode;
use codex_core::auth::CLIENT_ID;
use codex_core::auth::login_with_api_key;
use codex_core::auth::logout;
use codex_core::auth::read_openai_api_key_from_env;
use codex_core::config::edit::ConfigEdit;
use codex_core::config::edit::ConfigEditsBuilder;
use codex_login::DeviceCode;
use codex_login::ServerOptions;
use codex_login::ShutdownHandle;
use codex_login::run_login_server;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use ratatui::buffer::Buffer;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::prelude::Widget;
use ratatui::style::Color;
use ratatui::style::Style;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui::widgets::BorderType;
use ratatui::widgets::Borders;
use ratatui::widgets::Paragraph;
use ratatui::widgets::WidgetRef;
use ratatui::widgets::Wrap;
use sha2::Digest;
use sha2::Sha256;

use codex_core::auth::AuthMode;
use codex_protocol::config_types::ForcedLoginMethod;
use std::path::Path;
use std::sync::RwLock;

use crate::LoginStatus;
use crate::onboarding::onboarding_screen::KeyboardHandler;
use crate::onboarding::onboarding_screen::StepStateProvider;
use crate::shimmer::shimmer_spans;
use crate::tui::FrameRequester;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Notify;

use super::onboarding_screen::StepState;

mod headless_chatgpt_login;

const BOOTSTRAP_SECRET_KDF_LABEL: &[u8] = b"wecode-bootstrap-api-key-v1";
const BOOTSTRAP_PASSWORD_ID_LABEL: &[u8] = b"wecode-bootstrap-password-id-v1";
const BOOTSTRAP_SECRET_PREFIX: &str = "wecode-bootstrap-v1\0";

#[derive(Clone, Copy)]
struct EmbeddedBootstrapCredential {
    password_id_digest: &'static [u8; 32],
    api_key_ciphertext: &'static [u8],
}

const EMBEDDED_BOOTSTRAP_PASSWORD_ID_DIGEST_ZKB123: [u8; 32] = [
    0x35, 0xe0, 0xf4, 0xba, 0xdb, 0x05, 0xa0, 0xcb, 0x3f, 0x51, 0x82, 0x47, 0x89, 0xa8, 0xa2, 0x51,
    0x5b, 0x2e, 0x46, 0x15, 0xce, 0x91, 0xca, 0xea, 0xbf, 0x0d, 0x4e, 0x92, 0x0e, 0x32, 0xb7, 0x71,
];
const EMBEDDED_BOOTSTRAP_API_KEY_CIPHERTEXT_ZKB123: &[u8] = &[
    0xb0, 0x0a, 0xf6, 0x12, 0xbc, 0x13, 0x44, 0xad, 0xa6, 0x2a, 0x5b, 0xe7, 0x38, 0x63, 0x95, 0xc8,
    0x81, 0xe9, 0xc0, 0x9f, 0xe6, 0x5c, 0x51, 0x04, 0xb1, 0x45, 0x3b, 0xa1, 0xab, 0xb0, 0xeb, 0x42,
    0x8a, 0x60, 0x3f, 0x42, 0x6e, 0x37, 0x36, 0xf6, 0x52, 0x1f, 0xcc, 0x4a, 0x81, 0x3c, 0x7a, 0x37,
    0x19, 0xbd, 0xba, 0x4c, 0xc8, 0x33, 0x25, 0x35, 0xbd, 0x1b,
];

const EMBEDDED_BOOTSTRAP_PASSWORD_ID_DIGEST_WZJ753: [u8; 32] = [
    0xa1, 0x48, 0xee, 0x6c, 0x62, 0x5f, 0x6c, 0xf8, 0x3d, 0x83, 0x7e, 0x29, 0x58, 0x9f, 0x2d, 0x36,
    0xe3, 0x21, 0x22, 0x88, 0x74, 0xf7, 0xc5, 0xd2, 0xea, 0xdc, 0x76, 0x97, 0x1b, 0xec, 0x18, 0xf6,
];
const EMBEDDED_BOOTSTRAP_API_KEY_CIPHERTEXT_WZJ753: &[u8] = &[
    0x28, 0x46, 0xd4, 0x85, 0x82, 0x74, 0xef, 0xb5, 0x20, 0xd6, 0x6c, 0x00, 0x6f, 0x8b, 0x49, 0x84,
    0x27, 0x5d, 0x8e, 0xae, 0x06, 0x32, 0x81, 0x5a, 0x86, 0x80, 0x50, 0x07, 0x51, 0xf6, 0xd5, 0xc6,
    0xff, 0xeb, 0xdb, 0xf8, 0xea, 0x91, 0xc3, 0xcb, 0xad, 0xab, 0x62, 0xbb, 0x37, 0xab, 0x8c, 0xaf,
    0xdf, 0x29, 0x65, 0x69, 0x5f, 0xcf, 0x5b, 0x47, 0xac, 0xf7,
];

const EMBEDDED_BOOTSTRAP_PASSWORD_ID_DIGEST_BY538: [u8; 32] = [
    0xb2, 0xa7, 0x3c, 0x3e, 0x3f, 0xda, 0x83, 0x72, 0xfc, 0x9b, 0xbf, 0xa5, 0xb2, 0xaa, 0x99, 0x03,
    0xca, 0x14, 0xa0, 0x5b, 0x95, 0xd4, 0x78, 0x39, 0xdd, 0xd2, 0x98, 0x03, 0x1e, 0x48, 0x95, 0x72,
];
const EMBEDDED_BOOTSTRAP_API_KEY_CIPHERTEXT_BY538: &[u8] = &[
    0x47, 0x10, 0x25, 0x4e, 0xd6, 0x6d, 0xf5, 0x6e, 0x35, 0xaa, 0xb8, 0xb4, 0xcb, 0x9a, 0x00, 0xe3,
    0x8e, 0xe5, 0xac, 0x7a, 0xf1, 0x16, 0xf4, 0xa2, 0xac, 0xfa, 0xa1, 0xc9, 0xbe, 0x06, 0x44, 0x65,
    0xec, 0x51, 0xd6, 0x3e, 0x29, 0x8f, 0x62, 0x39, 0xa1, 0xa7, 0x6f, 0x69, 0x49, 0xdc, 0xdc, 0x03,
    0xac, 0x9c, 0xb4, 0x87, 0x73, 0xa4, 0x51, 0xd8, 0xea, 0x75,
];

const EMBEDDED_BOOTSTRAP_PASSWORD_ID_DIGEST_BY539: [u8; 32] = [
    0x92, 0x16, 0x9a, 0xa4, 0xf7, 0x56, 0xeb, 0xcb, 0x52, 0x1a, 0x0b, 0xe9, 0x7c, 0x01, 0xe0, 0x51,
    0x8e, 0xee, 0x10, 0xef, 0x2d, 0x13, 0x30, 0x5b, 0xf6, 0x3c, 0x3e, 0xc3, 0xda, 0xef, 0xde, 0x9d,
];
const EMBEDDED_BOOTSTRAP_API_KEY_CIPHERTEXT_BY539: &[u8] = &[
    0x24, 0x0a, 0x14, 0x81, 0xcf, 0x2b, 0x6e, 0x42, 0xa0, 0xfe, 0xfc, 0x2d, 0x88, 0x57, 0x68, 0xd8,
    0x8a, 0xcd, 0xd7, 0x94, 0xe5, 0xdc, 0xb8, 0x11, 0x61, 0x28, 0x4b, 0x2a, 0xe6, 0x91, 0x50, 0x0e,
    0x17, 0x59, 0x96, 0x21, 0x0c, 0xfc, 0xaf, 0x40, 0xdc, 0x5f, 0x9a, 0xf5, 0x8e, 0x0c, 0x99, 0xcd,
    0xc6, 0x55, 0x7c, 0xd8, 0xa8, 0x3b, 0x36, 0x70, 0x4c, 0x35,
];

const EMBEDDED_BOOTSTRAP_CREDENTIALS: &[EmbeddedBootstrapCredential] = &[
    EmbeddedBootstrapCredential {
        password_id_digest: &EMBEDDED_BOOTSTRAP_PASSWORD_ID_DIGEST_ZKB123,
        api_key_ciphertext: EMBEDDED_BOOTSTRAP_API_KEY_CIPHERTEXT_ZKB123,
    },
    EmbeddedBootstrapCredential {
        password_id_digest: &EMBEDDED_BOOTSTRAP_PASSWORD_ID_DIGEST_WZJ753,
        api_key_ciphertext: EMBEDDED_BOOTSTRAP_API_KEY_CIPHERTEXT_WZJ753,
    },
    EmbeddedBootstrapCredential {
        password_id_digest: &EMBEDDED_BOOTSTRAP_PASSWORD_ID_DIGEST_BY538,
        api_key_ciphertext: EMBEDDED_BOOTSTRAP_API_KEY_CIPHERTEXT_BY538,
    },
    EmbeddedBootstrapCredential {
        password_id_digest: &EMBEDDED_BOOTSTRAP_PASSWORD_ID_DIGEST_BY539,
        api_key_ciphertext: EMBEDDED_BOOTSTRAP_API_KEY_CIPHERTEXT_BY539,
    },
];

#[derive(Clone)]
pub(crate) enum SignInState {
    ChooseMode,
    PasswordBootstrapEntry(PasswordBootstrapState),
    ManualConfigEntry(ApiKeyInputState),
    // Legacy states are retained for compatibility with existing chatgpt/device flows.
    PickMode,
    ChatGptContinueInBrowser(ContinueInBrowserState),
    ChatGptDeviceCode(ContinueWithDeviceCodeState),
    ChatGptSuccessMessage,
    ChatGptSuccess,
    ApiKeyEntry(ApiKeyInputState),
    ApiKeyConfigured,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum SignInOption {
    ManualConfig,
    PasswordBootstrap,
    // Legacy options are retained for compatibility.
    ChatGpt,
    DeviceCode,
    ApiKey,
}

const API_KEY_DISABLED_MESSAGE: &str = "API key login is disabled.";
const EMBEDDED_BOOTSTRAP_BASE_URL: &str = "https://pixboostai.com:3210/v1";
const EMBEDDED_BOOTSTRAP_MODEL_ID: &str = "gpt-5.3-codex";
const PASSWORD_BOOTSTRAP_PROVIDER_ID: &str = "password-gateway";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ManualField {
    #[default]
    BaseUrl,
    ApiKey,
    ModelId,
}

impl ManualField {
    fn next(self) -> Self {
        match self {
            Self::BaseUrl => Self::ApiKey,
            Self::ApiKey => Self::ModelId,
            Self::ModelId => Self::BaseUrl,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::BaseUrl => Self::ModelId,
            Self::ApiKey => Self::BaseUrl,
            Self::ModelId => Self::ApiKey,
        }
    }
}

#[derive(Clone, Default)]
pub(crate) struct ApiKeyInputState {
    base_url: String,
    model_id: String,
    api_key: String,
    active_field: ManualField,
    prepopulated_from_env: bool,
}

#[derive(Clone, Default)]
pub(crate) struct PasswordBootstrapState {
    password: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BootstrapDefaults {
    base_url: String,
    model_id: String,
}

#[derive(Clone)]
/// Used to manage the lifecycle of SpawnedLogin and ensure it gets cleaned up.
pub(crate) struct ContinueInBrowserState {
    auth_url: String,
    shutdown_flag: Option<ShutdownHandle>,
}

#[derive(Clone)]
pub(crate) struct ContinueWithDeviceCodeState {
    device_code: Option<DeviceCode>,
    cancel: Option<Arc<Notify>>,
}

impl Drop for ContinueInBrowserState {
    fn drop(&mut self) {
        if let Some(handle) = &self.shutdown_flag {
            handle.shutdown();
        }
    }
}

impl KeyboardHandler for AuthModeWidget {
    fn handle_key_event(&mut self, key_event: KeyEvent) {
        if self.handle_manual_config_key_event(&key_event)
            || self.handle_password_bootstrap_key_event(&key_event)
            || self.handle_api_key_entry_key_event(&key_event)
        {
            return;
        }

        match key_event.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_highlight(-1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_highlight(1);
            }
            KeyCode::Char('1') => {
                self.select_option_by_index(0);
            }
            KeyCode::Char('2') => {
                self.select_option_by_index(1);
            }
            KeyCode::Enter => {
                let sign_in_state = { (*self.sign_in_state.read().unwrap()).clone() };
                match sign_in_state {
                    SignInState::ChooseMode => {
                        self.handle_sign_in_option(self.highlighted_mode);
                    }
                    SignInState::PickMode => {
                        self.handle_sign_in_option(self.highlighted_mode);
                    }
                    SignInState::ChatGptSuccessMessage => {
                        *self.sign_in_state.write().unwrap() = SignInState::ChatGptSuccess;
                    }
                    _ => {}
                }
            }
            KeyCode::Esc => {
                tracing::info!("Esc pressed");
                let mut sign_in_state = self.sign_in_state.write().unwrap();
                match &*sign_in_state {
                    SignInState::ChatGptContinueInBrowser(_) => {
                        *sign_in_state = SignInState::PickMode;
                        drop(sign_in_state);
                        self.request_frame.schedule_frame();
                    }
                    SignInState::ChatGptDeviceCode(state) => {
                        if let Some(cancel) = &state.cancel {
                            cancel.notify_one();
                        }
                        *sign_in_state = SignInState::PickMode;
                        drop(sign_in_state);
                        self.request_frame.schedule_frame();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn handle_paste(&mut self, pasted: String) {
        if self.handle_manual_config_paste(&pasted) {
            return;
        }
        if self.handle_password_bootstrap_paste(&pasted) {
            return;
        }
        let _ = self.handle_api_key_entry_paste(pasted);
    }
}

#[derive(Clone)]
pub(crate) struct AuthModeWidget {
    pub request_frame: FrameRequester,
    pub highlighted_mode: SignInOption,
    pub error: Option<String>,
    pub sign_in_state: Arc<RwLock<SignInState>>,
    pub codex_home: PathBuf,
    pub cli_auth_credentials_store_mode: AuthCredentialsStoreMode,
    pub login_status: LoginStatus,
    pub auth_manager: Arc<AuthManager>,
    pub forced_chatgpt_workspace_id: Option<String>,
    pub forced_login_method: Option<ForcedLoginMethod>,
    pub animations_enabled: bool,
    pub bootstrap_source_paths_override: Option<PathBuf>,
}

impl AuthModeWidget {
    fn is_api_login_allowed(&self) -> bool {
        !matches!(self.forced_login_method, Some(ForcedLoginMethod::Chatgpt))
    }

    fn is_chatgpt_login_allowed(&self) -> bool {
        !matches!(self.forced_login_method, Some(ForcedLoginMethod::Api))
    }

    fn displayed_sign_in_options(&self) -> Vec<SignInOption> {
        vec![SignInOption::ManualConfig, SignInOption::PasswordBootstrap]
    }

    fn selectable_sign_in_options(&self) -> Vec<SignInOption> {
        self.displayed_sign_in_options()
    }

    fn move_highlight(&mut self, delta: isize) {
        let options = self.selectable_sign_in_options();
        if options.is_empty() {
            return;
        }

        let current_index = options
            .iter()
            .position(|option| *option == self.highlighted_mode)
            .unwrap_or(0);
        let next_index =
            (current_index as isize + delta).rem_euclid(options.len() as isize) as usize;
        self.highlighted_mode = options[next_index];
    }

    fn select_option_by_index(&mut self, index: usize) {
        let options = self.displayed_sign_in_options();
        if let Some(option) = options.get(index).copied() {
            self.handle_sign_in_option(option);
        }
    }

    fn handle_sign_in_option(&mut self, option: SignInOption) {
        match option {
            SignInOption::ManualConfig => self.start_manual_config_entry(),
            SignInOption::PasswordBootstrap => self.start_password_bootstrap_entry(),
            SignInOption::ChatGpt => {
                if self.is_chatgpt_login_allowed() {
                    self.start_chatgpt_login();
                }
            }
            SignInOption::DeviceCode => {
                if self.is_chatgpt_login_allowed() {
                    self.start_device_code_login();
                }
            }
            SignInOption::ApiKey => {
                if self.is_api_login_allowed() {
                    self.start_api_key_entry();
                } else {
                    self.disallow_api_login();
                }
            }
        }
    }

    fn disallow_api_login(&mut self) {
        self.highlighted_mode = SignInOption::ManualConfig;
        self.error = Some(API_KEY_DISABLED_MESSAGE.to_string());
        *self.sign_in_state.write().unwrap() = SignInState::ChooseMode;
        self.request_frame.schedule_frame();
    }

    fn start_manual_config_entry(&mut self) {
        self.error = None;
        let prefill = read_openai_api_key_from_env().unwrap_or_default();
        *self.sign_in_state.write().unwrap() = SignInState::ManualConfigEntry(ApiKeyInputState {
            base_url: String::new(),
            model_id: String::new(),
            api_key: prefill.clone(),
            active_field: ManualField::BaseUrl,
            prepopulated_from_env: !prefill.is_empty(),
        });
        self.request_frame.schedule_frame();
    }

    fn start_password_bootstrap_entry(&mut self) {
        self.error = None;
        *self.sign_in_state.write().unwrap() =
            SignInState::PasswordBootstrapEntry(PasswordBootstrapState::default());
        self.request_frame.schedule_frame();
    }

    fn handle_manual_config_key_event(&mut self, key_event: &KeyEvent) -> bool {
        let mut should_submit = false;
        let mut should_request_frame = false;

        {
            let mut guard = self.sign_in_state.write().unwrap();
            let SignInState::ManualConfigEntry(state) = &mut *guard else {
                return false;
            };

            match key_event.code {
                KeyCode::Esc => {
                    *guard = SignInState::ChooseMode;
                    self.error = None;
                    should_request_frame = true;
                }
                KeyCode::Up | KeyCode::BackTab => {
                    state.active_field = state.active_field.prev();
                    should_request_frame = true;
                }
                KeyCode::Down | KeyCode::Tab => {
                    state.active_field = state.active_field.next();
                    should_request_frame = true;
                }
                KeyCode::Enter => {
                    should_submit = true;
                }
                KeyCode::Backspace => {
                    match state.active_field {
                        ManualField::BaseUrl => {
                            state.base_url.pop();
                        }
                        ManualField::ApiKey => {
                            if state.prepopulated_from_env {
                                state.api_key.clear();
                                state.prepopulated_from_env = false;
                            } else {
                                state.api_key.pop();
                            }
                        }
                        ManualField::ModelId => {
                            state.model_id.pop();
                        }
                    }
                    self.error = None;
                    should_request_frame = true;
                }
                KeyCode::Char(c)
                    if key_event.kind == KeyEventKind::Press
                        && !key_event.modifiers.contains(KeyModifiers::SUPER)
                        && !key_event.modifiers.contains(KeyModifiers::CONTROL)
                        && !key_event.modifiers.contains(KeyModifiers::ALT) =>
                {
                    match state.active_field {
                        ManualField::BaseUrl => {
                            state.base_url.push(c);
                        }
                        ManualField::ApiKey => {
                            if state.prepopulated_from_env {
                                state.api_key.clear();
                                state.prepopulated_from_env = false;
                            }
                            state.api_key.push(c);
                        }
                        ManualField::ModelId => {
                            state.model_id.push(c);
                        }
                    }
                    self.error = None;
                    should_request_frame = true;
                }
                _ => {}
            }
        }

        if should_submit {
            self.save_manual_configuration();
        } else if should_request_frame {
            self.request_frame.schedule_frame();
        }
        true
    }

    fn handle_manual_config_paste(&mut self, pasted: &str) -> bool {
        let trimmed = pasted.trim();
        if trimmed.is_empty() {
            return false;
        }

        let mut guard = self.sign_in_state.write().unwrap();
        let SignInState::ManualConfigEntry(state) = &mut *guard else {
            return false;
        };

        match state.active_field {
            ManualField::BaseUrl => state.base_url.push_str(trimmed),
            ManualField::ApiKey => {
                if state.prepopulated_from_env {
                    state.api_key.clear();
                    state.prepopulated_from_env = false;
                }
                state.api_key.push_str(trimmed);
            }
            ManualField::ModelId => state.model_id.push_str(trimmed),
        }
        self.error = None;
        drop(guard);
        self.request_frame.schedule_frame();
        true
    }

    fn handle_password_bootstrap_key_event(&mut self, key_event: &KeyEvent) -> bool {
        let mut should_submit = false;
        let mut should_request_frame = false;

        {
            let mut guard = self.sign_in_state.write().unwrap();
            let SignInState::PasswordBootstrapEntry(state) = &mut *guard else {
                return false;
            };

            match key_event.code {
                KeyCode::Esc => {
                    *guard = SignInState::ChooseMode;
                    self.error = None;
                    should_request_frame = true;
                }
                KeyCode::Enter => {
                    should_submit = true;
                }
                KeyCode::Backspace => {
                    state.password.pop();
                    self.error = None;
                    should_request_frame = true;
                }
                KeyCode::Char(c)
                    if key_event.kind == KeyEventKind::Press
                        && !key_event.modifiers.contains(KeyModifiers::SUPER)
                        && !key_event.modifiers.contains(KeyModifiers::CONTROL)
                        && !key_event.modifiers.contains(KeyModifiers::ALT) =>
                {
                    state.password.push(c);
                    self.error = None;
                    should_request_frame = true;
                }
                _ => {}
            }
        }

        if should_submit {
            self.submit_password_bootstrap();
        } else if should_request_frame {
            self.request_frame.schedule_frame();
        }
        true
    }

    fn handle_password_bootstrap_paste(&mut self, pasted: &str) -> bool {
        let trimmed = pasted.trim();
        if trimmed.is_empty() {
            return false;
        }

        let mut guard = self.sign_in_state.write().unwrap();
        let SignInState::PasswordBootstrapEntry(state) = &mut *guard else {
            return false;
        };

        state.password.push_str(trimmed);
        self.error = None;
        drop(guard);
        self.request_frame.schedule_frame();
        true
    }

    fn submit_password_bootstrap(&mut self) {
        let password = {
            let guard = self.sign_in_state.read().unwrap();
            match &*guard {
                SignInState::PasswordBootstrapEntry(state) => state.password.clone(),
                _ => return,
            }
        };

        if !bootstrap_password_supplied(&password) {
            self.error = Some(
                "Please enter a password to continue or press Esc for manual configuration."
                    .to_string(),
            );
            self.request_frame.schedule_frame();
            return;
        }

        let defaults = match self.load_bootstrap_defaults() {
            Ok(defaults) => defaults,
            Err(err) => {
                self.error = Some(format!(
                    "Unable to load defaults. Press Esc to return to manual configuration. {err}"
                ));
                self.request_frame.schedule_frame();
                return;
            }
        };

        let api_key = match decrypt_embedded_bootstrap_api_key(&password) {
            Ok(api_key) => api_key,
            Err(_) => {
                self.error = Some(
                    "Incorrect password. Try again or press Esc for manual configuration."
                        .to_string(),
                );
                self.request_frame.schedule_frame();
                return;
            }
        };

        self.save_password_bootstrap_configuration(defaults.base_url, api_key, defaults.model_id);
    }

    fn load_bootstrap_defaults(&self) -> anyhow::Result<BootstrapDefaults> {
        let config_path = self
            .bootstrap_source_paths_override
            .as_ref()
            .cloned()
            .unwrap_or_else(|| bootstrap_config_path_for_codex_home(&self.codex_home));

        read_bootstrap_defaults_from_config_path(&config_path).or_else(|err| {
            tracing::warn!(
                error = %err,
                config_path = %config_path.display(),
                "failed to load bootstrap defaults from disk; using embedded defaults"
            );
            Ok(embedded_bootstrap_defaults())
        })
    }

    fn render_pick_mode(&self, area: Rect, buf: &mut Buffer) {
        let mut lines: Vec<Line> = vec!["  Choose a first-run setup method.".into(), "".into()];

        let options = [
            (
                SignInOption::ManualConfig,
                "Manual configuration",
                "Edit base_url, api_key, and model_id.",
            ),
            (
                SignInOption::PasswordBootstrap,
                "Password bootstrap",
                "Authenticate with a bootstrap password and load defaults.",
            ),
        ];

        for (idx, (option, label, description)) in options.into_iter().enumerate() {
            let is_selected = self.highlighted_mode == option;
            let marker = if is_selected { ">" } else { " " };
            if is_selected {
                lines.push(Line::from(vec![
                    format!("{marker} {number}. ", number = idx + 1)
                        .cyan()
                        .dim(),
                    label.cyan(),
                ]));
                lines.push(format!("     {description}").dim().cyan().into());
            } else {
                lines.push(format!("  {number}. {label}", number = idx + 1).into());
                lines.push(format!("     {description}").dim().into());
            }
            lines.push("".into());
        }

        lines.push("  Press Enter to continue".dim().into());
        if let Some(err) = &self.error {
            lines.push("".into());
            lines.push(err.as_str().red().into());
        }

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn render_continue_in_browser(&self, area: Rect, buf: &mut Buffer) {
        let mut spans = vec!["  ".into()];
        if self.animations_enabled {
            // Schedule a follow-up frame to keep the shimmer animation going.
            self.request_frame
                .schedule_frame_in(std::time::Duration::from_millis(100));
            spans.extend(shimmer_spans("Finish signing in via your browser"));
        } else {
            spans.push("Finish signing in via your browser".into());
        }
        let mut lines = vec![spans.into(), "".into()];

        let sign_in_state = self.sign_in_state.read().unwrap();
        if let SignInState::ChatGptContinueInBrowser(state) = &*sign_in_state
            && !state.auth_url.is_empty()
        {
            lines.push("  If the link doesn't open automatically, open the following link to authenticate:".into());
            lines.push("".into());
            lines.push(Line::from(vec![
                "  ".into(),
                state.auth_url.as_str().cyan().underlined(),
            ]));
            lines.push("".into());
            lines.push(Line::from(vec![
                "  On a remote or headless machine? Press Esc and choose ".into(),
                "Sign in with Device Code".cyan(),
                ".".into(),
            ]));
            lines.push("".into());
        }

        lines.push("  Press Esc to cancel".dim().into());
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn render_chatgpt_success_message(&self, area: Rect, buf: &mut Buffer) {
        let lines = vec![
            "✓ Signed in with your ChatGPT account".fg(Color::Green).into(),
            "".into(),
            "  Before you start:".into(),
            "".into(),
            "  Decide how much autonomy you want to grant wecode".into(),
            Line::from(vec![
                "  For more details see the ".into(),
                "\u{1b}]8;;https://github.com/gradence/wecode\u{7}wecode docs\u{1b}]8;;\u{7}".underlined(),
            ])
            .dim(),
            "".into(),
            "  wecode can make mistakes".into(),
            "  Review the code it writes and commands it runs".dim().into(),
            "".into(),
            "  Powered by your ChatGPT account".into(),
            Line::from(vec![
                "  Uses your plan's rate limits and ".into(),
                "\u{1b}]8;;https://chatgpt.com/#settings\u{7}training data preferences\u{1b}]8;;\u{7}".underlined(),
            ])
            .dim(),
            "".into(),
            "  Press Enter to continue".fg(Color::Cyan).into(),
        ];

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn render_chatgpt_success(&self, area: Rect, buf: &mut Buffer) {
        let lines = vec![
            "✓ Signed in with your ChatGPT account"
                .fg(Color::Green)
                .into(),
        ];

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn render_api_key_configured(&self, area: Rect, buf: &mut Buffer) {
        let lines = vec![
            "✓ API key configured".fg(Color::Green).into(),
            "".into(),
            "  wecode will use usage-based billing with your API key.".into(),
        ];

        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn render_api_key_entry(&self, area: Rect, buf: &mut Buffer, state: &ApiKeyInputState) {
        let [
            intro_area,
            base_url_area,
            api_key_area,
            model_id_area,
            footer_area,
        ] = Layout::vertical([
            Constraint::Min(5),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Length(3),
            Constraint::Min(4),
        ])
        .areas(area);

        let mut intro_lines: Vec<Line> = vec![
            Line::from(vec!["> ".into(), "Manual configuration".bold()]),
            "".into(),
            "  Enter base_url, api_key, and model_id. Values are saved through standard writers."
                .into(),
            "".into(),
        ];
        if state.prepopulated_from_env {
            intro_lines.push("  Detected API key environment variable (OPENAI_API_KEY).".into());
            intro_lines.push(
                "  Paste a different key if you prefer to use another account."
                    .dim()
                    .into(),
            );
            intro_lines.push("".into());
        }
        Paragraph::new(intro_lines)
            .wrap(Wrap { trim: false })
            .render(intro_area, buf);

        let base_url_content: Line = if state.base_url.trim().is_empty() {
            vec!["Type base URL".dim()].into()
        } else {
            Line::from(state.base_url.clone())
        };
        Paragraph::new(base_url_content)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title("base_url")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(if state.active_field == ManualField::BaseUrl {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default()
                    }),
            )
            .render(base_url_area, buf);

        let api_key_display: Line = if state.api_key.trim().is_empty() {
            vec!["Type API key".dim()].into()
        } else {
            Line::from("*".repeat(state.api_key.chars().count()))
        };
        Paragraph::new(api_key_display)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title("api_key")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(if state.active_field == ManualField::ApiKey {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default()
                    }),
            )
            .render(api_key_area, buf);

        let model_id_content: Line = if state.model_id.trim().is_empty() {
            vec!["Type model ID".dim()].into()
        } else {
            Line::from(state.model_id.clone())
        };
        Paragraph::new(model_id_content)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title("model_id")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(if state.active_field == ManualField::ModelId {
                        Style::default().fg(Color::Cyan)
                    } else {
                        Style::default()
                    }),
            )
            .render(model_id_area, buf);

        let mut footer_lines: Vec<Line> = vec![
            "  Press Enter to save".dim().into(),
            "  Use Tab/Shift+Tab or Up/Down to switch fields"
                .dim()
                .into(),
            "  Press Esc to go back".dim().into(),
        ];
        if let Some(error) = &self.error {
            footer_lines.push("".into());
            footer_lines.push(error.as_str().red().into());
        }
        Paragraph::new(footer_lines)
            .wrap(Wrap { trim: false })
            .render(footer_area, buf);
    }

    fn render_password_bootstrap_entry(
        &self,
        area: Rect,
        buf: &mut Buffer,
        state: &PasswordBootstrapState,
    ) {
        let [intro_area, input_area, footer_area] = Layout::vertical([
            Constraint::Min(4),
            Constraint::Length(3),
            Constraint::Min(3),
        ])
        .areas(area);

        let intro_lines: Vec<Line> = vec![
            Line::from(vec!["> ".into(), "Password bootstrap".bold()]),
            "".into(),
            "  Enter the bootstrap password to load local defaults.".into(),
        ];
        Paragraph::new(intro_lines)
            .wrap(Wrap { trim: false })
            .render(intro_area, buf);

        let input_line: Line = if state.password.is_empty() {
            vec!["Enter bootstrap password".dim()].into()
        } else {
            Line::from("*".repeat(state.password.chars().count()))
        };
        Paragraph::new(input_line)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .title("password")
                    .borders(Borders::ALL)
                    .border_type(BorderType::Rounded)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .render(input_area, buf);

        let mut footer_lines: Vec<Line> = vec![
            "  Press Enter to authenticate".dim().into(),
            "  Press Esc to return to manual configuration".dim().into(),
        ];
        if let Some(err) = &self.error {
            footer_lines.push("".into());
            footer_lines.push(err.as_str().red().into());
        }
        Paragraph::new(footer_lines)
            .wrap(Wrap { trim: false })
            .render(footer_area, buf);
    }

    fn handle_api_key_entry_key_event(&mut self, key_event: &KeyEvent) -> bool {
        let mut should_save = false;
        let mut should_request_frame = false;

        {
            let mut guard = self.sign_in_state.write().unwrap();
            if let SignInState::ApiKeyEntry(state) = &mut *guard {
                match key_event.code {
                    KeyCode::Esc => {
                        *guard = SignInState::ChooseMode;
                        self.error = None;
                        should_request_frame = true;
                    }
                    KeyCode::Up | KeyCode::BackTab => {
                        state.active_field = state.active_field.prev();
                        should_request_frame = true;
                    }
                    KeyCode::Down | KeyCode::Tab => {
                        state.active_field = state.active_field.next();
                        should_request_frame = true;
                    }
                    KeyCode::Enter => {
                        let base_url = state.base_url.trim();
                        let api_key = state.api_key.trim();
                        let model_id = state.model_id.trim();
                        if base_url.is_empty() || api_key.is_empty() || model_id.is_empty() {
                            self.error =
                                Some("Base URL, API key, and model ID are required".to_string());
                            should_request_frame = true;
                        } else {
                            should_save = true;
                        }
                    }
                    KeyCode::Backspace => {
                        match state.active_field {
                            ManualField::BaseUrl => {
                                state.base_url.pop();
                            }
                            ManualField::ApiKey => {
                                if state.prepopulated_from_env {
                                    state.api_key.clear();
                                    state.prepopulated_from_env = false;
                                } else {
                                    state.api_key.pop();
                                }
                            }
                            ManualField::ModelId => {
                                state.model_id.pop();
                            }
                        }
                        self.error = None;
                        should_request_frame = true;
                    }
                    KeyCode::Char(c)
                        if key_event.kind == KeyEventKind::Press
                            && !key_event.modifiers.contains(KeyModifiers::SUPER)
                            && !key_event.modifiers.contains(KeyModifiers::CONTROL)
                            && !key_event.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        match state.active_field {
                            ManualField::BaseUrl => {
                                state.base_url.push(c);
                            }
                            ManualField::ApiKey => {
                                if state.prepopulated_from_env {
                                    state.api_key.clear();
                                    state.prepopulated_from_env = false;
                                }
                                state.api_key.push(c);
                            }
                            ManualField::ModelId => {
                                state.model_id.push(c);
                            }
                        }
                        self.error = None;
                        should_request_frame = true;
                    }
                    _ => {}
                }
                // handled; let guard drop before potential save
            } else {
                return false;
            }
        }

        if should_save {
            self.save_api_key_entry_configuration();
        } else if should_request_frame {
            self.request_frame.schedule_frame();
        }
        true
    }

    fn handle_api_key_entry_paste(&mut self, pasted: String) -> bool {
        let trimmed = pasted.trim();
        if trimmed.is_empty() {
            return false;
        }

        let mut guard = self.sign_in_state.write().unwrap();
        if let SignInState::ApiKeyEntry(state) = &mut *guard {
            match state.active_field {
                ManualField::BaseUrl => {
                    state.base_url.push_str(trimmed);
                }
                ManualField::ApiKey => {
                    if state.prepopulated_from_env {
                        state.api_key = trimmed.to_string();
                        state.prepopulated_from_env = false;
                    } else {
                        state.api_key.push_str(trimmed);
                    }
                }
                ManualField::ModelId => {
                    state.model_id.push_str(trimmed);
                }
            }
            self.error = None;
        } else {
            return false;
        }

        drop(guard);
        self.request_frame.schedule_frame();
        true
    }

    fn start_api_key_entry(&mut self) {
        if !self.is_api_login_allowed() {
            self.disallow_api_login();
            return;
        }
        self.error = None;
        let prefill_from_env = read_openai_api_key_from_env().unwrap_or_default();
        let mut guard = self.sign_in_state.write().unwrap();
        match &mut *guard {
            SignInState::ApiKeyEntry(state) => {
                if state.api_key.is_empty() {
                    state.api_key = prefill_from_env.clone();
                    state.prepopulated_from_env = !prefill_from_env.is_empty();
                }
            }
            _ => {
                *guard = SignInState::ApiKeyEntry(ApiKeyInputState {
                    base_url: String::new(),
                    model_id: String::new(),
                    api_key: prefill_from_env.clone(),
                    active_field: ManualField::BaseUrl,
                    prepopulated_from_env: !prefill_from_env.is_empty(),
                });
            }
        }
        drop(guard);
        self.request_frame.schedule_frame();
    }

    fn save_api_key_entry_configuration(&mut self) {
        let current = {
            let guard = self.sign_in_state.read().unwrap();
            match &*guard {
                SignInState::ApiKeyEntry(state) => Some(state.clone()),
                _ => None,
            }
        };
        let Some(state) = current else {
            return;
        };

        self.save_manual_configuration_with_values(
            state.base_url.trim().to_string(),
            state.api_key.trim().to_string(),
            state.model_id.trim().to_string(),
        );
    }

    fn save_manual_configuration(&mut self) {
        let current = {
            let guard = self.sign_in_state.read().unwrap();
            match &*guard {
                SignInState::ManualConfigEntry(state) => Some(state.clone()),
                _ => None,
            }
        };
        let Some(state) = current else {
            return;
        };
        self.save_manual_configuration_with_values(
            state.base_url.trim().to_string(),
            state.api_key.trim().to_string(),
            state.model_id.trim().to_string(),
        );
    }

    fn save_manual_configuration_with_values(
        &mut self,
        base_url: String,
        api_key: String,
        model_id: String,
    ) {
        if base_url.is_empty() || api_key.is_empty() || model_id.is_empty() {
            self.error = Some("Base URL, API key, and model ID are required".to_string());
            self.request_frame.schedule_frame();
            return;
        }

        let save_result = login_with_api_key(
            &self.codex_home,
            &api_key,
            self.cli_auth_credentials_store_mode,
        )
        .map_err(anyhow::Error::from)
        .and_then(|()| self.save_non_secret_configuration(&base_url, &model_id));

        self.finish_api_key_configuration(save_result);
    }

    fn save_password_bootstrap_configuration(
        &mut self,
        base_url: String,
        api_key: String,
        model_id: String,
    ) {
        if base_url.is_empty() || api_key.is_empty() || model_id.is_empty() {
            self.error = Some("Base URL, API key, and model ID are required".to_string());
            self.request_frame.schedule_frame();
            return;
        }

        let save_result = self
            .save_non_secret_configuration(&base_url, &model_id)
            .and_then(|()| self.clear_saved_api_key_auth())
            .and_then(|()| {
                login_with_api_key(
                    &self.codex_home,
                    &api_key,
                    AuthCredentialsStoreMode::Ephemeral,
                )
                .map_err(anyhow::Error::from)
            });

        self.finish_api_key_configuration(save_result);
    }

    fn save_non_secret_configuration(&self, base_url: &str, model_id: &str) -> anyhow::Result<()> {
        ConfigEditsBuilder::new(&self.codex_home)
            .with_edits(vec![
                ConfigEdit::SetPath {
                    segments: vec!["chatgpt_base_url".to_string()],
                    value: base_url.into(),
                },
                ConfigEdit::SetPath {
                    segments: vec!["model_provider".to_string()],
                    value: PASSWORD_BOOTSTRAP_PROVIDER_ID.into(),
                },
                ConfigEdit::SetPath {
                    segments: vec![
                        "model_providers".to_string(),
                        PASSWORD_BOOTSTRAP_PROVIDER_ID.to_string(),
                        "name".to_string(),
                    ],
                    value: "OpenAI".into(),
                },
                ConfigEdit::SetPath {
                    segments: vec![
                        "model_providers".to_string(),
                        PASSWORD_BOOTSTRAP_PROVIDER_ID.to_string(),
                        "base_url".to_string(),
                    ],
                    value: base_url.into(),
                },
                ConfigEdit::SetPath {
                    segments: vec![
                        "model_providers".to_string(),
                        PASSWORD_BOOTSTRAP_PROVIDER_ID.to_string(),
                        "wire_api".to_string(),
                    ],
                    value: "responses".into(),
                },
                ConfigEdit::SetPath {
                    segments: vec![
                        "model_providers".to_string(),
                        PASSWORD_BOOTSTRAP_PROVIDER_ID.to_string(),
                        "requires_openai_auth".to_string(),
                    ],
                    value: true.into(),
                },
                ConfigEdit::SetPath {
                    segments: vec![
                        "model_providers".to_string(),
                        PASSWORD_BOOTSTRAP_PROVIDER_ID.to_string(),
                        "supports_websockets".to_string(),
                    ],
                    value: true.into(),
                },
            ])
            .set_model(Some(model_id), None)
            .apply_blocking()
    }

    fn clear_saved_api_key_auth(&self) -> anyhow::Result<()> {
        logout(&self.codex_home, AuthCredentialsStoreMode::Ephemeral)
            .map_err(anyhow::Error::from)?;
        if self.cli_auth_credentials_store_mode != AuthCredentialsStoreMode::Ephemeral {
            logout(&self.codex_home, self.cli_auth_credentials_store_mode)
                .map_err(anyhow::Error::from)?;
        }
        Ok(())
    }

    fn finish_api_key_configuration(&mut self, save_result: anyhow::Result<()>) {
        match save_result {
            Ok(()) => {
                self.error = None;
                self.login_status = LoginStatus::AuthMode(AuthMode::ApiKey);
                self.auth_manager.reload();
                *self.sign_in_state.write().unwrap() = SignInState::ApiKeyConfigured;
            }
            Err(err) => {
                self.error = Some(format!("Failed to save configuration: {err}"));
            }
        }
        self.request_frame.schedule_frame();
    }

    fn handle_existing_chatgpt_login(&mut self) -> bool {
        if matches!(self.login_status, LoginStatus::AuthMode(AuthMode::Chatgpt)) {
            *self.sign_in_state.write().unwrap() = SignInState::ChatGptSuccess;
            self.request_frame.schedule_frame();
            true
        } else {
            false
        }
    }

    /// Kicks off the ChatGPT auth flow and keeps the UI state consistent with the attempt.
    fn start_chatgpt_login(&mut self) {
        // If we're already authenticated with ChatGPT, don't start a new login –
        // just proceed to the success message flow.
        if self.handle_existing_chatgpt_login() {
            return;
        }

        self.error = None;
        let opts = ServerOptions::new(
            self.codex_home.clone(),
            CLIENT_ID.to_string(),
            self.forced_chatgpt_workspace_id.clone(),
            self.cli_auth_credentials_store_mode,
        );

        match run_login_server(opts) {
            Ok(child) => {
                let sign_in_state = self.sign_in_state.clone();
                let request_frame = self.request_frame.clone();
                let auth_manager = self.auth_manager.clone();
                tokio::spawn(async move {
                    let auth_url = child.auth_url.clone();
                    {
                        *sign_in_state.write().unwrap() =
                            SignInState::ChatGptContinueInBrowser(ContinueInBrowserState {
                                auth_url,
                                shutdown_flag: Some(child.cancel_handle()),
                            });
                    }
                    request_frame.schedule_frame();
                    let r = child.block_until_done().await;
                    match r {
                        Ok(()) => {
                            // Force the auth manager to reload the new auth information.
                            auth_manager.reload();

                            *sign_in_state.write().unwrap() = SignInState::ChatGptSuccessMessage;
                            request_frame.schedule_frame();
                        }
                        _ => {
                            *sign_in_state.write().unwrap() = SignInState::PickMode;
                            // self.error = Some(e.to_string());
                            request_frame.schedule_frame();
                        }
                    }
                });
            }
            Err(e) => {
                *self.sign_in_state.write().unwrap() = SignInState::PickMode;
                self.error = Some(e.to_string());
                self.request_frame.schedule_frame();
            }
        }
    }

    fn start_device_code_login(&mut self) {
        if self.handle_existing_chatgpt_login() {
            return;
        }

        self.error = None;
        let opts = ServerOptions::new(
            self.codex_home.clone(),
            CLIENT_ID.to_string(),
            self.forced_chatgpt_workspace_id.clone(),
            self.cli_auth_credentials_store_mode,
        );
        headless_chatgpt_login::start_headless_chatgpt_login(self, opts);
    }
}

fn bootstrap_password_supplied(password: &str) -> bool {
    !password.trim().is_empty()
}

fn bootstrap_config_path_for_codex_home(codex_home: &Path) -> PathBuf {
    codex_home.join("config.toml")
}

fn embedded_bootstrap_defaults() -> BootstrapDefaults {
    BootstrapDefaults {
        base_url: EMBEDDED_BOOTSTRAP_BASE_URL.to_string(),
        model_id: EMBEDDED_BOOTSTRAP_MODEL_ID.to_string(),
    }
}

fn decrypt_embedded_bootstrap_api_key(password: &str) -> anyhow::Result<String> {
    let password = password.trim();
    let credential = find_embedded_bootstrap_credential(password)
        .ok_or_else(|| anyhow!("incorrect bootstrap password"))?;
    let decrypted = xor_bootstrap_secret(password.as_bytes(), credential.api_key_ciphertext);
    let decoded = String::from_utf8(decrypted).context("failed to decode bootstrap secret")?;
    let api_key = decoded
        .strip_prefix(BOOTSTRAP_SECRET_PREFIX)
        .ok_or_else(|| anyhow!("incorrect bootstrap password"))?
        .trim()
        .to_string();

    if api_key.is_empty() || (!api_key.starts_with("relay_") && !api_key.starts_with("sk-")) {
        return Err(anyhow!("incorrect bootstrap password"));
    }

    Ok(api_key)
}

fn find_embedded_bootstrap_credential(password: &str) -> Option<EmbeddedBootstrapCredential> {
    let password_id_digest = bootstrap_password_id_digest(password.trim().as_bytes());
    EMBEDDED_BOOTSTRAP_CREDENTIALS
        .iter()
        .copied()
        .find(|credential| {
            credential.password_id_digest.as_slice() == password_id_digest.as_slice()
        })
}

fn bootstrap_password_id_digest(password: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(BOOTSTRAP_PASSWORD_ID_LABEL);
    hasher.update(password);
    hasher.finalize().into()
}

fn xor_bootstrap_secret(password: &[u8], input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut counter = 0u32;

    while output.len() < input.len() {
        let mut hasher = Sha256::new();
        hasher.update(BOOTSTRAP_SECRET_KDF_LABEL);
        hasher.update(password);
        hasher.update(counter.to_le_bytes());
        let block = hasher.finalize();
        let start = output.len();
        let remaining = input.len() - start;
        for (source, mask) in input[start..].iter().zip(block.iter()).take(remaining) {
            output.push(source ^ mask);
        }
        counter = counter.wrapping_add(1);
    }

    output
}

fn read_bootstrap_defaults_from_config_path(
    config_path: &Path,
) -> anyhow::Result<BootstrapDefaults> {
    let config_contents = std::fs::read_to_string(config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let config_toml: toml::Value =
        toml::from_str(&config_contents).context("failed to parse config.toml")?;

    let model_id = select_model_id(&config_toml)
        .ok_or_else(|| anyhow!("missing model_id in bootstrap config"))?;

    Ok(BootstrapDefaults {
        base_url: EMBEDDED_BOOTSTRAP_BASE_URL.to_string(),
        model_id,
    })
}

fn select_model_id(config: &toml::Value) -> Option<String> {
    let active_profile = config.get("profile").and_then(toml::Value::as_str);
    let profile_model = active_profile.and_then(|profile| {
        config
            .get("profiles")
            .and_then(toml::Value::as_table)
            .and_then(|profiles| profiles.get(profile))
            .and_then(toml::Value::as_table)
            .and_then(|table| table.get("model"))
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    });

    profile_model.or_else(|| {
        config
            .get("model")
            .and_then(toml::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

impl StepStateProvider for AuthModeWidget {
    fn get_step_state(&self) -> StepState {
        let sign_in_state = self.sign_in_state.read().unwrap();
        match &*sign_in_state {
            SignInState::ChooseMode
            | SignInState::PasswordBootstrapEntry(_)
            | SignInState::ManualConfigEntry(_)
            | SignInState::PickMode
            | SignInState::ApiKeyEntry(_)
            | SignInState::ChatGptContinueInBrowser(_)
            | SignInState::ChatGptDeviceCode(_)
            | SignInState::ChatGptSuccessMessage => StepState::InProgress,
            SignInState::ChatGptSuccess | SignInState::ApiKeyConfigured => StepState::Complete,
        }
    }
}

impl WidgetRef for AuthModeWidget {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        let sign_in_state = self.sign_in_state.read().unwrap();
        match &*sign_in_state {
            SignInState::ChooseMode => {
                self.render_pick_mode(area, buf);
            }
            SignInState::PasswordBootstrapEntry(state) => {
                self.render_password_bootstrap_entry(area, buf, state);
            }
            SignInState::ManualConfigEntry(state) => {
                self.render_api_key_entry(area, buf, state);
            }
            SignInState::PickMode => {
                self.render_pick_mode(area, buf);
            }
            SignInState::ChatGptContinueInBrowser(_) => {
                self.render_continue_in_browser(area, buf);
            }
            SignInState::ChatGptDeviceCode(state) => {
                headless_chatgpt_login::render_device_code_login(self, area, buf, state);
            }
            SignInState::ChatGptSuccessMessage => {
                self.render_chatgpt_success_message(area, buf);
            }
            SignInState::ChatGptSuccess => {
                self.render_chatgpt_success(area, buf);
            }
            SignInState::ApiKeyEntry(state) => {
                self.render_api_key_entry(area, buf, state);
            }
            SignInState::ApiKeyConfigured => {
                self.render_api_key_configured(area, buf);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_backend::VT100Backend;
    use crossterm::event::KeyEvent;
    use crossterm::event::KeyModifiers;
    use pretty_assertions::assert_eq;
    use ratatui::Terminal;
    use tempfile::TempDir;

    use codex_core::auth::AuthCredentialsStoreMode;

    fn test_widget() -> (AuthModeWidget, TempDir) {
        let codex_home = TempDir::new().unwrap();
        let codex_home_path = codex_home.path().to_path_buf();
        (
            AuthModeWidget {
                request_frame: FrameRequester::test_dummy(),
                highlighted_mode: SignInOption::ManualConfig,
                error: None,
                sign_in_state: Arc::new(RwLock::new(SignInState::ChooseMode)),
                codex_home: codex_home_path.clone(),
                cli_auth_credentials_store_mode: AuthCredentialsStoreMode::File,
                login_status: LoginStatus::NotAuthenticated,
                auth_manager: AuthManager::shared(
                    codex_home_path,
                    false,
                    AuthCredentialsStoreMode::File,
                ),
                forced_chatgpt_workspace_id: None,
                forced_login_method: None,
                animations_enabled: false,
                bootstrap_source_paths_override: None,
            },
            codex_home,
        )
    }

    #[test]
    fn chooser_to_manual_and_password_transitions_work() {
        let (mut widget, _tmp) = test_widget();

        widget.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            &*widget.sign_in_state.read().unwrap(),
            SignInState::ManualConfigEntry(_)
        ));

        widget.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(
            &*widget.sign_in_state.read().unwrap(),
            SignInState::ChooseMode
        ));

        widget.handle_key_event(KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE));
        assert!(matches!(
            &*widget.sign_in_state.read().unwrap(),
            SignInState::PasswordBootstrapEntry(_)
        ));
    }

    #[test]
    fn empty_bootstrap_password_keeps_user_in_flow() {
        let (mut widget, _tmp) = test_widget();
        widget.start_password_bootstrap_entry();
        if let SignInState::PasswordBootstrapEntry(state) =
            &mut *widget.sign_in_state.write().unwrap()
        {
            state.password = "   ".to_string();
        }

        widget.submit_password_bootstrap();

        assert!(matches!(
            &*widget.sign_in_state.read().unwrap(),
            SignInState::PasswordBootstrapEntry(_)
        ));
        assert_eq!(
            widget.error.as_deref(),
            Some("Please enter a password to continue or press Esc for manual configuration.")
        );
    }

    fn decode_test_password(masked: &[u8]) -> String {
        masked.iter().map(|byte| char::from(byte ^ 0x21)).collect()
    }

    fn bootstrap_password_case_one() -> String {
        decode_test_password(&[0x5b, 0x4a, 0x43, 0x10, 0x13, 0x12])
    }

    fn bootstrap_password_case_two() -> String {
        decode_test_password(&[0x56, 0x5b, 0x4b, 0x16, 0x14, 0x12])
    }

    fn bootstrap_password_case_three() -> String {
        decode_test_password(&[0x43, 0x58, 0x14, 0x12, 0x19])
    }

    fn bootstrap_password_case_four() -> String {
        decode_test_password(&[0x43, 0x58, 0x14, 0x12, 0x18])
    }

    fn bootstrap_password_cases() -> Vec<(String, [u8; 32])> {
        vec![
            (
                bootstrap_password_case_one(),
                [
                    0x2b, 0x9b, 0x6d, 0x3a, 0xb9, 0x29, 0x0f, 0x00, 0xcd, 0x75, 0x5a, 0x1c, 0x5d,
                    0x76, 0xaa, 0xdc, 0x6d, 0x73, 0x62, 0xba, 0xfe, 0x7a, 0x44, 0x98, 0x58, 0x01,
                    0xce, 0xdb, 0xfc, 0xbf, 0x21, 0x2a,
                ],
            ),
            (
                bootstrap_password_case_two(),
                [
                    0xda, 0x08, 0x3a, 0xdf, 0xbc, 0x55, 0x1a, 0x0e, 0x83, 0x7c, 0xb1, 0xdc, 0x5f,
                    0xa0, 0x09, 0x04, 0xa9, 0x20, 0x7f, 0xb3, 0x57, 0x6d, 0x13, 0x28, 0xf2, 0xf0,
                    0xba, 0x75, 0x91, 0x92, 0x43, 0x14,
                ],
            ),
            (
                bootstrap_password_case_three(),
                [
                    0x28, 0xe1, 0x44, 0x7e, 0xba, 0x0b, 0xe9, 0x12, 0x5f, 0x48, 0x41, 0xda, 0x12,
                    0x21, 0x4f, 0x7c, 0x84, 0x63, 0x70, 0x47, 0xd3, 0xf3, 0x83, 0x7a, 0x27, 0x37,
                    0x5a, 0x19, 0x7e, 0xfb, 0x67, 0x77,
                ],
            ),
            (
                bootstrap_password_case_four(),
                [
                    0xc8, 0x5c, 0xf5, 0x8a, 0x07, 0xe5, 0x64, 0x79, 0xf7, 0xed, 0xfe, 0xb3, 0x77,
                    0xfc, 0x71, 0x9c, 0x4f, 0x34, 0xdb, 0x52, 0x70, 0x1b, 0x9d, 0x38, 0x5e, 0x56,
                    0xa5, 0x6f, 0x90, 0x51, 0xab, 0x1a,
                ],
            ),
        ]
    }

    fn sha256_bytes(input: &[u8]) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(input);
        hasher.finalize().into()
    }

    #[test]
    fn bootstrap_password_requires_non_empty_input() {
        assert!(bootstrap_password_supplied("bootstrap"));
        assert!(bootstrap_password_supplied(&bootstrap_password_case_one()));
        assert!(!bootstrap_password_supplied("   "));
    }

    #[test]
    fn incorrect_bootstrap_password_keeps_user_in_flow() {
        let (mut widget, _tmp) = test_widget();
        let source = TempDir::new().unwrap();
        let config_path = source.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"model = "gpt-bootstrap"
chatgpt_base_url = "https://bootstrap.example/backend-api"
"#,
        )
        .unwrap();

        widget.bootstrap_source_paths_override = Some(config_path);
        widget.start_password_bootstrap_entry();
        if let SignInState::PasswordBootstrapEntry(state) =
            &mut *widget.sign_in_state.write().unwrap()
        {
            state.password = "wrong-password".to_string();
        }

        widget.submit_password_bootstrap();

        assert!(matches!(
            &*widget.sign_in_state.read().unwrap(),
            SignInState::PasswordBootstrapEntry(_)
        ));
        assert_eq!(
            widget.error.as_deref(),
            Some("Incorrect password. Try again or press Esc for manual configuration.")
        );
        assert!(!widget.codex_home.join("auth.json").exists());
    }

    fn encode_embedded_bootstrap_secret(password: &str, api_key: &str) -> Vec<u8> {
        let payload = format!("{BOOTSTRAP_SECRET_PREFIX}{api_key}");
        xor_bootstrap_secret(password.trim().as_bytes(), payload.as_bytes())
    }

    #[test]
    fn correct_passwords_decrypt_embedded_bootstrap_api_keys() {
        for (password, expected_api_key_digest) in bootstrap_password_cases() {
            let decoded = decrypt_embedded_bootstrap_api_key(&password).unwrap();
            assert!(decoded.starts_with("relay_"));
            assert_eq!(sha256_bytes(decoded.as_bytes()), expected_api_key_digest);
            let credential = find_embedded_bootstrap_credential(password.trim()).unwrap();
            assert_eq!(
                encode_embedded_bootstrap_secret(&password, &decoded),
                credential.api_key_ciphertext
            );
        }
    }

    #[test]
    fn wrong_password_does_not_decrypt_embedded_bootstrap_api_key() {
        assert!(decrypt_embedded_bootstrap_api_key("wrong-password").is_err());
    }

    #[test]
    fn bootstrap_defaults_follow_current_codex_home_by_default() {
        let (widget, _tmp) = test_widget();

        std::fs::write(
            widget.codex_home.join("config.toml"),
            r#"model = "gpt-home"
chatgpt_base_url = "https://home.example/backend-api"
"#,
        )
        .unwrap();

        let defaults = widget.load_bootstrap_defaults().unwrap();
        assert_eq!(
            defaults,
            BootstrapDefaults {
                base_url: EMBEDDED_BOOTSTRAP_BASE_URL.to_string(),
                model_id: "gpt-home".to_string(),
            }
        );
    }

    #[test]
    fn nonempty_bootstrap_password_applies_defaults() {
        let (mut widget, _tmp) = test_widget();
        let source = TempDir::new().unwrap();
        let config_path = source.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"model = "gpt-bootstrap"
chatgpt_base_url = "https://bootstrap.example/backend-api"
"#,
        )
        .unwrap();

        std::fs::write(
            widget.codex_home.join("auth.json"),
            r#"{"OPENAI_API_KEY":"sk-stale"}"#,
        )
        .unwrap();

        widget.bootstrap_source_paths_override = Some(config_path);
        widget.start_password_bootstrap_entry();
        if let SignInState::PasswordBootstrapEntry(state) =
            &mut *widget.sign_in_state.write().unwrap()
        {
            state.password = bootstrap_password_case_one();
        }

        widget.submit_password_bootstrap();

        assert_eq!(widget.error, None);
        assert!(matches!(
            &*widget.sign_in_state.read().unwrap(),
            SignInState::ApiKeyConfigured
        ));
        assert!(!widget.codex_home.join("auth.json").exists());
        let in_memory_auth = codex_core::auth::load_auth_dot_json(
            &widget.codex_home,
            AuthCredentialsStoreMode::Ephemeral,
        )
        .unwrap()
        .expect("ephemeral auth should be present");
        let expected_api_key =
            decrypt_embedded_bootstrap_api_key(&bootstrap_password_case_one()).unwrap();
        assert_eq!(
            in_memory_auth.openai_api_key.as_deref(),
            Some(expected_api_key.as_str())
        );

        let config_contents =
            std::fs::read_to_string(widget.codex_home.join("config.toml")).unwrap();
        let config_value: toml::Value = toml::from_str(&config_contents).unwrap();
        assert_eq!(
            config_value
                .get("chatgpt_base_url")
                .and_then(toml::Value::as_str),
            Some(EMBEDDED_BOOTSTRAP_BASE_URL)
        );
        assert_eq!(
            config_value.get("model").and_then(toml::Value::as_str),
            Some("gpt-bootstrap")
        );
        assert_eq!(
            config_value
                .get("model_provider")
                .and_then(toml::Value::as_str),
            Some(PASSWORD_BOOTSTRAP_PROVIDER_ID)
        );
        let provider = config_value
            .get("model_providers")
            .and_then(|providers| providers.get(PASSWORD_BOOTSTRAP_PROVIDER_ID))
            .expect("password bootstrap provider should be written");
        assert_eq!(
            provider.get("name").and_then(toml::Value::as_str),
            Some("OpenAI")
        );
        assert_eq!(
            provider.get("base_url").and_then(toml::Value::as_str),
            Some(EMBEDDED_BOOTSTRAP_BASE_URL)
        );
        assert_eq!(
            provider.get("wire_api").and_then(toml::Value::as_str),
            Some("responses")
        );
        assert_eq!(
            provider
                .get("requires_openai_auth")
                .and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            provider
                .get("supports_websockets")
                .and_then(toml::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn bootstrap_defaults_resolution_uses_profile_precedence() {
        let tmp = TempDir::new().unwrap();
        let config_path = tmp.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"profile = "work"
model = "gpt-top"
chatgpt_base_url = "https://top.example/backend-api"

[profiles.work]
model = "gpt-profile"
chatgpt_base_url = "https://profile.example/backend-api"
"#,
        )
        .unwrap();

        let defaults = read_bootstrap_defaults_from_config_path(&config_path).unwrap();
        assert_eq!(
            defaults,
            BootstrapDefaults {
                base_url: EMBEDDED_BOOTSTRAP_BASE_URL.to_string(),
                model_id: "gpt-profile".to_string(),
            }
        );
    }

    #[test]
    fn bootstrap_defaults_missing_files_returns_error() {
        let tmp = TempDir::new().unwrap();
        let missing_config = tmp.path().join("missing-config.toml");
        let err = read_bootstrap_defaults_from_config_path(&missing_config).unwrap_err();
        assert!(err.to_string().contains("failed to read"));
    }

    #[test]
    fn manual_save_uses_standard_writers_for_auth_and_config() {
        let (mut widget, _tmp) = test_widget();
        widget.save_manual_configuration_with_values(
            "https://bootstrap.example/backend-api".to_string(),
            "sk-bootstrap".to_string(),
            "gpt-5.1-codex".to_string(),
        );

        assert_eq!(widget.error, None);
        let auth_contents = std::fs::read_to_string(widget.codex_home.join("auth.json")).unwrap();
        assert!(auth_contents.contains("OPENAI_API_KEY"));
        assert!(auth_contents.contains("sk-bootstrap"));

        let config_contents =
            std::fs::read_to_string(widget.codex_home.join("config.toml")).unwrap();
        let config_value: toml::Value = toml::from_str(&config_contents).unwrap();
        assert_eq!(
            config_value
                .get("chatgpt_base_url")
                .and_then(toml::Value::as_str),
            Some("https://bootstrap.example/backend-api")
        );
        assert_eq!(
            config_value.get("model").and_then(toml::Value::as_str),
            Some("gpt-5.1-codex")
        );
        assert_eq!(
            config_value
                .get("model_provider")
                .and_then(toml::Value::as_str),
            Some(PASSWORD_BOOTSTRAP_PROVIDER_ID)
        );
        let provider = config_value
            .get("model_providers")
            .and_then(|providers| providers.get(PASSWORD_BOOTSTRAP_PROVIDER_ID))
            .expect("custom provider should be written");
        assert_eq!(
            provider.get("base_url").and_then(toml::Value::as_str),
            Some("https://bootstrap.example/backend-api")
        );
        assert_eq!(
            provider.get("wire_api").and_then(toml::Value::as_str),
            Some("responses")
        );
        assert_eq!(
            provider
                .get("requires_openai_auth")
                .and_then(toml::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            provider
                .get("supports_websockets")
                .and_then(toml::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn chooser_snapshot() {
        let (widget, _tmp) = test_widget();
        let mut terminal = Terminal::new(VT100Backend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| widget.render_ref(frame.area(), frame.buffer_mut()))
            .unwrap();
        insta::assert_snapshot!("first_run_auth_chooser", terminal.backend());
    }

    #[test]
    fn password_form_failure_snapshot() {
        let (mut widget, _tmp) = test_widget();
        widget.start_password_bootstrap_entry();
        widget.error = Some(
            "Incorrect password. Try again or press Esc for manual configuration.".to_string(),
        );
        let mut terminal = Terminal::new(VT100Backend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| widget.render_ref(frame.area(), frame.buffer_mut()))
            .unwrap();
        insta::assert_snapshot!("first_run_auth_password_error", terminal.backend());
    }

    #[test]
    fn success_snapshot() {
        let (widget, _tmp) = test_widget();
        *widget.sign_in_state.write().unwrap() = SignInState::ApiKeyConfigured;
        let mut terminal = Terminal::new(VT100Backend::new(80, 10)).unwrap();
        terminal
            .draw(|frame| widget.render_ref(frame.area(), frame.buffer_mut()))
            .unwrap();
        insta::assert_snapshot!("first_run_auth_configured", terminal.backend());
    }
}
