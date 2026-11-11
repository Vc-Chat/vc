//! # vc-gui
//!
//! Discord-like desktop client for the vc XMPP server, written in Rust with
//! GTK 4 and the workspace's `xmpp-client`/`xmpp-core` crates.
//!
//! Threads: GTK owns the main thread (its widgets are not thread-safe) and the
//! XMPP connection runs on a tokio runtime in background threads. They talk
//! through two tokio channels: [`net::Command`] (UI → network) and
//! [`net::Event`] (network → UI). tokio channels can be awaited outside the
//! tokio runtime, so the UI awaits events with `glib::spawn_future_local`,
//! right on the GTK main loop.
//!
//! Screens live in a [`gtk::Stack`]: `login` ([`login::Login`]) and, once
//! connected, `chat` ([`chat::Chat`]).

mod chat;
mod login;
mod net;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::OnceLock;

use gtk::prelude::*;
use gtk::{gdk, glib};
use tokio::sync::mpsc;

const APP_ID: &str = "io.github.vcchat.Vc";

/// Accent color for outlines, hover, selection and buttons, in both themes.
const ACCENT: &str = "#F5789A";

/// Per-theme palette, prepended to the stylesheet as GTK named colors.
/// `bg` is the main background; the other neutrals are shades around it
/// (rail darker than the sidebar, sidebar darker than the chat, as in Discord).
const DARK: &str = "
@define-color bg #2B2D31;
@define-color sidebar #232428;
@define-color rail #1E1F22;
@define-color surface #383A40;
@define-color border #1E1F22;
@define-color text #DBDEE1;
@define-color muted #949BA4;
";
const LIGHT: &str = "
@define-color bg #FFFFFF;
@define-color sidebar #F2F3F5;
@define-color rail #E3E5E8;
@define-color surface #EBEDEF;
@define-color border #E1E2E4;
@define-color text #313338;
@define-color muted #5C5E66;
";

/// Rules shared by both themes; they only reference the named colors above.
const STYLE: &str = include_str!("style.css");

/// Light/dark theme switch. Cloning is cheap: the state is behind an `Rc`.
#[derive(Clone)]
pub struct Theme(Rc<ThemeState>);

struct ThemeState {
    provider: gtk::CssProvider,
    dark: Cell<bool>,
}

impl Theme {
    /// Installs our stylesheet for the whole display, starting in dark mode
    /// (Discord's default).
    /// ponytail: does not follow the OS light/dark setting; GTK only exposes
    /// it from 4.20 (`gtk-interface-color-scheme`), above what CI has.
    fn new() -> Theme {
        let provider = gtk::CssProvider::new();
        let display = gdk::Display::default().expect("no graphical display available");
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        let theme = Theme(Rc::new(ThemeState {
            provider,
            dark: Cell::new(true),
        }));
        theme.apply();
        theme
    }

    pub fn is_dark(&self) -> bool {
        self.0.dark.get()
    }

    pub fn toggle(&self) {
        self.0.dark.set(!self.is_dark());
        self.apply();
    }

    /// Reloads the stylesheet with the current palette. GTK restyles every
    /// widget on the spot, so the switch is instant.
    fn apply(&self) {
        let palette = if self.is_dark() { DARK } else { LIGHT };
        let css = format!("@define-color accent {ACCENT};\n{palette}\n{STYLE}");
        self.0.provider.load_from_string(&css);
        if let Some(settings) = gtk::Settings::default() {
            set_color_scheme(&settings, self.is_dark());
        }
    }
}

/// Flips GTK's built-in theme variant too, for the bits we do not restyle
/// ourselves (popovers, tooltips, text cursors...). GTK 4.20 replaced
/// `gtk-application-prefer-dark-theme` (and warns when it is set) with the
/// `gtk-interface-color-scheme` enum; we build against the 4.12 API, so the
/// new property is looked up by name and the old one is only a fallback.
fn set_color_scheme(settings: &gtk::Settings, dark: bool) {
    const PROPERTY: &str = "gtk-interface-color-scheme";
    let scheme = settings
        .find_property(PROPERTY)
        .and_then(|spec| glib::EnumClass::with_type(spec.value_type()))
        .and_then(|class| class.to_value_by_nick(if dark { "dark" } else { "light" }));
    match scheme {
        Some(value) => settings.set_property_from_value(PROPERTY, &value),
        None => settings.set_gtk_application_prefer_dark_theme(dark),
    }
}

/// The tokio runtime where XMPP connections run, started on first use.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| tokio::runtime::Runtime::new().expect("failed to start tokio"))
}

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_activate(build_window);
    app.run()
}

fn build_window(app: &gtk::Application) {
    let theme = Theme::new();
    let stack = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    let login = login::Login::new();
    stack.add_named(&login.root, Some("login"));

    // Weak reference: the login widgets own this closure, so a strong `Rc`
    // here would form a cycle that is never freed.
    let weak_login = Rc::downgrade(&login);
    login.connect_submit({
        let stack = stack.clone();
        move |credentials| {
            if let Some(login) = weak_login.upgrade() {
                start_session(credentials, login, stack.clone(), theme.clone());
            }
        }
    });

    gtk::ApplicationWindow::builder()
        .application(app)
        .title("vc")
        .default_width(1200)
        .default_height(760)
        .child(&stack)
        .build()
        .present();
}

/// Starts an XMPP session on the tokio runtime and drives the UI from its
/// events until it ends, then returns to the login screen.
fn start_session(
    credentials: net::Credentials,
    login: Rc<login::Login>,
    stack: gtk::Stack,
    theme: Theme,
) {
    let (commands, command_rx) = mpsc::unbounded_channel();
    let (event_tx, mut events) = mpsc::unbounded_channel();
    runtime().spawn(net::run(credentials, command_rx, event_tx));

    // The command sender moves into the chat view once connected; when the
    // view is dropped, the network task sees the channel close and logs out.
    let mut commands = Some(commands);
    glib::spawn_future_local(async move {
        let mut chat: Option<Rc<chat::Chat>> = None;
        while let Some(event) = events.recv().await {
            match event {
                net::Event::Connected(me) => {
                    let Some(commands) = commands.take() else {
                        continue;
                    };
                    let view = chat::Chat::new(me, commands, theme.clone());
                    stack.add_named(&view.root, Some("chat"));
                    stack.set_visible_child_name("chat");
                    login.reset();
                    chat = Some(view);
                }
                net::Event::Message { from, body } => {
                    if let Some(chat) = &chat {
                        chat.incoming(&from, &body);
                    }
                }
                net::Event::Undelivered { to, condition } => {
                    if let Some(chat) = &chat {
                        chat.undelivered(&to, &condition);
                    }
                }
                net::Event::Closed(reason) => {
                    if let Some(chat) = chat.take() {
                        stack.remove(&chat.root);
                    }
                    stack.set_visible_child_name("login");
                    login.fail(&reason);
                }
            }
        }
    });
}
