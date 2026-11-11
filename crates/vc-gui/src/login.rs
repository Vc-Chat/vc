//! Login screen: a centered card, as on Discord, collecting the JID,
//! password and optional server address / development certificate.

use std::path::PathBuf;
use std::rc::Rc;

use gtk::prelude::*;
use xmpp_core::Jid;

use crate::net::Credentials;

pub struct Login {
    pub root: gtk::Box,
    jid: gtk::Entry,
    password: gtk::PasswordEntry,
    server: gtk::Entry,
    ca: gtk::Entry,
    button: gtk::Button,
    error: gtk::Label,
}

impl Login {
    pub fn new() -> Rc<Login> {
        let jid = gtk::Entry::builder()
            .placeholder_text("alice@localhost")
            .build();
        let password = gtk::PasswordEntry::new();
        let server = gtk::Entry::builder()
            .placeholder_text("opcional — ex.: 127.0.0.1:5222")
            .build();
        let ca = gtk::Entry::builder()
            .placeholder_text("opcional — ex.: certs/cert.pem")
            .build();
        let error = gtk::Label::builder()
            .css_classes(["error-text"])
            .wrap(true)
            .xalign(0.0)
            .visible(false)
            .build();
        let button = gtk::Button::builder()
            .label("Entrar")
            .css_classes(["primary"])
            .margin_top(8)
            .build();

        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .width_request(440)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(["card"])
            .build();
        card.append(&label("Bem-vindo de volta!", "title"));
        card.append(&label(
            "Estamos muito animados em te ver novamente!",
            "subtitle",
        ));
        card.append(&field("JID (USUÁRIO@DOMÍNIO)", &jid));
        card.append(&field("SENHA", &password));
        card.append(&field("SERVIDOR", &server));
        card.append(&field("CERTIFICADO CA (PEM)", &ca));
        card.append(&error);
        card.append(&button);

        let root = gtk::Box::builder()
            .css_classes(["login-page"])
            .hexpand(true)
            .vexpand(true)
            .build();
        root.append(&card);
        card.set_hexpand(true);

        Rc::new(Login {
            root,
            jid,
            password,
            server,
            ca,
            button,
            error,
        })
    }

    /// Calls `on_submit` with the validated form when the user clicks "Entrar"
    /// or presses Enter in any field.
    pub fn connect_submit(self: &Rc<Self>, on_submit: impl Fn(Credentials) + 'static) {
        let on_submit = Rc::new(on_submit);
        // Weak references: the widgets own these closures (see build_window).
        let submit = {
            let this = Rc::downgrade(self);
            move || {
                if let Some(this) = this.upgrade() {
                    this.submit(&*on_submit);
                }
            }
        };
        let submit = Rc::new(submit);
        self.button.connect_clicked({
            let submit = submit.clone();
            move |_| submit()
        });
        for entry in [&self.jid, &self.server, &self.ca] {
            let submit = submit.clone();
            entry.connect_activate(move |_| submit());
        }
        self.password.connect_activate(move |_| submit());
    }

    /// Validates the form; on success disables the button while connecting.
    fn submit(&self, on_submit: &dyn Fn(Credentials)) {
        let jid = match Jid::parse(self.jid.text().trim()) {
            Ok(jid) if jid.local.is_some() => jid,
            _ => return self.fail("JID inválido: use o formato usuario@dominio."),
        };
        let password = self.password.text().to_string();
        if password.is_empty() {
            return self.fail("Digite a senha.");
        }
        let optional = |entry: &gtk::Entry| {
            let text = entry.text().trim().to_owned();
            (!text.is_empty()).then_some(text)
        };
        self.error.set_visible(false);
        self.set_busy(true);
        on_submit(Credentials {
            jid,
            password,
            server: optional(&self.server),
            ca: optional(&self.ca).map(PathBuf::from),
        });
    }

    /// Shows an error (validation, connection or disconnection) and
    /// re-enables the form.
    pub fn fail(&self, message: &str) {
        self.error.set_text(message);
        self.error.set_visible(true);
        self.set_busy(false);
    }

    /// Clears the form after a successful login, so it is ready for the next one.
    pub fn reset(&self) {
        self.error.set_visible(false);
        self.password.set_text("");
        self.set_busy(false);
    }

    fn set_busy(&self, busy: bool) {
        self.button.set_sensitive(!busy);
        self.button
            .set_label(if busy { "Conectando…" } else { "Entrar" });
    }
}

/// Small uppercase label above an input, as in Discord's forms.
fn field(title: &str, input: &impl IsA<gtk::Widget>) -> gtk::Box {
    let field = gtk::Box::new(gtk::Orientation::Vertical, 6);
    field.append(&label(title, "field-label"));
    field.append(input);
    field
}

fn label(text: &str, class: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes([class])
        .xalign(0.0)
        .wrap(true)
        .build()
}
