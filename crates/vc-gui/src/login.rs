//! Login screen: a centered card, as on Discord, collecting the JID and
//! password.

use std::rc::Rc;

use gtk::prelude::*;
use xmpp_core::Jid;

use crate::net::Credentials;

pub struct Login {
    pub root: gtk::Overlay,
    jid: gtk::Entry,
    password: gtk::PasswordEntry,
    button: gtk::Button,
    error: gtk::Label,
}

impl Login {
    pub fn new() -> Rc<Login> {
        let jid = gtk::Entry::builder()
            .placeholder_text("alice@localhost")
            .build();
        let password = gtk::PasswordEntry::new();
        let error = gtk::Label::builder()
            .css_classes(["error-text"])
            .wrap(true)
            .xalign(0.0)
            .visible(false)
            .build();
        let button = gtk::Button::builder()
            .label("Log In")
            .css_classes(["primary"])
            .margin_top(8)
            .build();

        // Brand mark.
        let logo = gtk::Label::builder()
            .label("vc")
            .css_classes(["logo"])
            .halign(gtk::Align::Center)
            .margin_bottom(8)
            .build();
        let header = gtk::Box::new(gtk::Orientation::Vertical, 8);
        header.set_margin_bottom(4);
        header.append(&logo);
        for (text, class) in [
            ("Welcome back!", "title"),
            ("We're so excited to see you again!", "subtitle"),
        ] {
            let line = label(text, class);
            line.set_xalign(0.5);
            line.set_justify(gtk::Justification::Center);
            header.append(&line);
        }

        let card = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .width_request(440)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(["card"])
            .build();
        card.append(&header);
        card.append(&field("JID (user@domain)", &jid));
        card.append(&field("Password", &password));
        card.append(&error);
        card.append(&button);

        let page = gtk::Box::builder()
            .css_classes(["login-page"])
            .hexpand(true)
            .vexpand(true)
            .build();
        page.append(&card);
        card.set_hexpand(true);

        // The window has no title bar (see chat.rs): the page moves it and
        // carries the window controls.
        let root = gtk::Overlay::builder()
            .child(&gtk::WindowHandle::builder().child(&page).build())
            .build();
        root.add_overlay(
            &gtk::WindowControls::builder()
                .side(gtk::PackType::End)
                .halign(gtk::Align::End)
                .valign(gtk::Align::Start)
                .css_classes(["login-controls"])
                .build(),
        );

        Rc::new(Login {
            root,
            jid,
            password,
            button,
            error,
        })
    }

    /// Calls `on_submit` with the validated form when the user clicks "Log In"
    /// or presses Enter in either field.
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
        self.jid.connect_activate({
            let submit = submit.clone();
            move |_| submit()
        });
        self.password.connect_activate(move |_| submit());
    }

    /// Validates the form; on success disables the button while connecting.
    fn submit(&self, on_submit: &dyn Fn(Credentials)) {
        let jid = match Jid::parse(self.jid.text().trim()) {
            Ok(jid) if jid.local.is_some() => jid,
            _ => return self.fail("Invalid JID: use the user@domain format."),
        };
        let password = self.password.text().to_string();
        if password.is_empty() {
            return self.fail("Enter your password.");
        }
        self.error.set_visible(false);
        self.set_busy(true);
        on_submit(Credentials { jid, password });
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
            .set_label(if busy { "Connecting…" } else { "Log In" });
    }
}

/// Label with a red "required" asterisk above an input.
fn field(title: &str, input: &impl IsA<gtk::Widget>) -> gtk::Box {
    // No wrapping here: a wrapping label in a horizontal box confuses GTK's
    // height-for-width measuring.
    let title = label(title, "field-label");
    title.set_wrap(false);
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    header.append(&title);
    header.append(
        &gtk::Label::builder()
            .label("*")
            .css_classes(["field-label", "required"])
            .build(),
    );
    let field = gtk::Box::new(gtk::Orientation::Vertical, 8);
    field.append(&header);
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
