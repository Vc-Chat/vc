//! Main screen, laid out like Discord's direct messages view:
//!
//! ```text
//! ┌──────┬────────────────────┬──────────────────────────────────────┐
//! │ rail │ [find or start DM] │ @ bob@localhost              header  │
//! │ (vc) │ DIRECT MESSAGES    │                                      │
//! │      │  (b) bob@localhost │  messages of the selected DM         │
//! │      │  (c) carol@…       │  (one page per conversation)         │
//! │      │                    │                                      │
//! │      │ (a) alice  Online ☀│ [ Message @bob ]            composer │
//! └──────┴────────────────────┴──────────────────────────────────────┘
//! ```
//!
//! Each conversation is keyed by the contact's bare JID and owns a sidebar
//! row plus a message list in a [`gtk::Stack`], so incoming messages land in
//! the right place even when that conversation is not on screen.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{glib, pango};
use tokio::sync::mpsc::UnboundedSender;
use xmpp_core::Jid;

use crate::Theme;
use crate::net::Command;

pub struct Chat {
    pub root: gtk::Box,
    me: Jid,
    commands: UnboundedSender<Command>,
    /// DM rows in the sidebar.
    list: gtk::ListBox,
    /// One message page per DM, plus the "empty" page.
    pages: gtk::Stack,
    title: gtk::Label,
    composer: gtk::Entry,
    conversations: RefCell<HashMap<String, Conversation>>,
    /// Bare JID of the conversation on screen.
    current: RefCell<Option<String>>,
}

/// Widgets of one DM. GTK widgets are reference-counted, so cloning is cheap.
#[derive(Clone)]
struct Conversation {
    row: gtk::ListBoxRow,
    messages: gtk::ListBox,
}

impl Chat {
    pub fn new(me: Jid, commands: UnboundedSender<Command>, theme: Theme) -> Rc<Chat> {
        let my_name = me.local.clone().unwrap_or_default();

        // ── Server rail. ponytail: only "home" (direct messages); servers
        // and channels need group chat (XEP-0045), which the server lacks.
        let home = gtk::Label::builder()
            .label("vc")
            .css_classes(["rail-item", "active"])
            .tooltip_text("Mensagens diretas")
            .build();
        let rail = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["rail"])
            .build();
        rail.append(&home);

        // ── Sidebar: DM search/creation, conversation list and user panel.
        let search = gtk::Entry::builder()
            .placeholder_text("Buscar ou iniciar conversa")
            .css_classes(["search"])
            .tooltip_text("Digite um JID (usuario@dominio) e pressione Enter")
            .build();
        let search_box = gtk::Box::builder().css_classes(["sidebar-header"]).build();
        search.set_hexpand(true);
        search_box.append(&search);

        let list = gtk::ListBox::builder()
            .css_classes(["conversations"])
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        let list_scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        let theme_button = gtk::Button::builder()
            .label(theme_icon(&theme))
            .css_classes(["icon"])
            .tooltip_text("Alternar tema claro/escuro")
            .valign(gtk::Align::Center)
            .build();
        let identity = gtk::Box::new(gtk::Orientation::Vertical, 0);
        identity.append(&label(&my_name, "user-name"));
        identity.append(&label("Online", "user-status"));
        identity.set_hexpand(true);
        identity.set_tooltip_text(Some(&me.to_string()));
        let user_panel = gtk::Box::builder()
            .spacing(8)
            .css_classes(["user-panel"])
            .build();
        user_panel.append(&avatar(&my_name, true));
        user_panel.append(&identity);
        user_panel.append(&theme_button);

        // Explicit `hexpand(false)`: in GTK 4 a child's hexpand (the search
        // entry) propagates to its parent, and the sidebar would steal half
        // of the window from the chat area.
        let sidebar = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["sidebar"])
            .hexpand(false)
            .width_request(240)
            .build();
        sidebar.append(&search_box);
        sidebar.append(&label("MENSAGENS DIRETAS", "section"));
        sidebar.append(&list_scroll);
        sidebar.append(&user_panel);

        // ── Chat area: header, message pages and composer.
        let title = label("", "chat-title");
        let header = gtk::Box::builder()
            .spacing(8)
            .css_classes(["chat-header"])
            .build();
        header.append(&label("@", "at"));
        header.append(&title);

        let pages = gtk::Stack::builder().vexpand(true).build();
        let empty = gtk::Label::builder()
            .label("Escolha uma conversa ou comece uma nova pela busca ao lado.")
            .css_classes(["empty"])
            .wrap(true)
            .build();
        pages.add_named(&empty, Some("empty"));

        let composer = gtk::Entry::builder()
            .placeholder_text("Escolha uma conversa")
            .css_classes(["composer"])
            .sensitive(false)
            .hexpand(true)
            .build();
        let composer_box = gtk::Box::builder().css_classes(["composer-box"]).build();
        composer_box.append(&composer);

        let main = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .build();
        main.append(&header);
        main.append(&pages);
        main.append(&composer_box);

        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.append(&rail);
        root.append(&sidebar);
        root.append(&main);

        let chat = Rc::new(Chat {
            root,
            me,
            commands,
            list,
            pages,
            title,
            composer,
            conversations: RefCell::default(),
            current: RefCell::default(),
        });

        // ── Signals. Closures hold weak references: the widgets own the
        // closures, and a strong `Rc<Chat>` would keep everything alive forever.
        let this = Rc::downgrade(&chat);
        search.connect_activate(move |entry| {
            let Some(chat) = this.upgrade() else { return };
            match Jid::parse(entry.text().trim()) {
                Ok(jid) if jid.local.is_some() => {
                    chat.select(&jid.bare().to_string());
                    entry.set_text("");
                    entry.remove_css_class("error");
                }
                _ => entry.add_css_class("error"),
            }
        });
        let this = Rc::downgrade(&chat);
        chat.list.connect_row_selected(move |_, row| {
            if let (Some(chat), Some(row)) = (this.upgrade(), row) {
                chat.show(&row.widget_name());
            }
        });
        let this = Rc::downgrade(&chat);
        chat.composer.connect_activate(move |entry| {
            if let Some(chat) = this.upgrade() {
                chat.send(&entry.text());
                entry.set_text("");
            }
        });
        theme_button.connect_clicked(move |button| {
            theme.toggle();
            button.set_label(theme_icon(&theme));
        });

        chat
    }

    /// A message arrived from `from` (bare JID).
    pub fn incoming(&self, from: &str, body: &str) {
        let conversation = self.conversation(from);
        conversation
            .messages
            .append(&message_row(local_part(from), body));
        if self.current.borrow().as_deref() != Some(from) {
            conversation.row.add_css_class("unread");
        }
    }

    /// The server bounced a message we sent to `to`.
    pub fn undelivered(&self, to: &str, condition: &str) {
        let text = format!("Mensagem não entregue ({condition}): {to} está offline ou não existe.");
        let note = label(&text, "system");
        self.conversation(to).messages.append(&row(&note));
    }

    /// Opens the conversation with `jid`, creating it on first use.
    /// Selecting the row triggers [`Chat::show`].
    fn select(&self, jid: &str) {
        let conversation = self.conversation(jid);
        self.list.select_row(Some(&conversation.row));
    }

    /// Puts the conversation on screen and readies the composer.
    fn show(&self, jid: &str) {
        let Some(conversation) = self.conversations.borrow().get(jid).cloned() else {
            return;
        };
        conversation.row.remove_css_class("unread");
        self.pages.set_visible_child_name(jid);
        self.title.set_text(jid);
        self.composer.set_sensitive(true);
        self.composer
            .set_placeholder_text(Some(&format!("Conversar com @{}", local_part(jid))));
        self.composer.grab_focus();
        *self.current.borrow_mut() = Some(jid.to_owned());
    }

    /// Sends the composer text to the current conversation and shows it
    /// right away, as Discord does.
    fn send(&self, text: &str) {
        let text = text.trim();
        let Some(to) = self.current.borrow().clone() else {
            return;
        };
        if text.is_empty() {
            return;
        }
        let command = Command::Send {
            to: to.clone(),
            body: text.to_owned(),
        };
        // A send error means the connection is gone; a `Closed` event follows.
        if self.commands.send(command).is_ok() {
            let me = self.me.local.as_deref().unwrap_or_default();
            self.conversation(&to)
                .messages
                .append(&message_row(me, text));
        }
    }

    /// The conversation with `jid`, creating its sidebar row and message page
    /// on first use.
    fn conversation(&self, jid: &str) -> Conversation {
        if let Some(conversation) = self.conversations.borrow().get(jid) {
            return conversation.clone();
        }
        let entry = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        entry.append(&avatar(local_part(jid), true));
        let name = label(jid, "dm-name");
        name.set_ellipsize(pango::EllipsizeMode::End);
        entry.append(&name);
        let conversation_row = gtk::ListBoxRow::builder().child(&entry).build();
        // The row's name is how `connect_row_selected` finds the conversation.
        conversation_row.set_widget_name(jid);
        self.list.append(&conversation_row);

        let messages = gtk::ListBox::builder()
            .css_classes(["messages"])
            .selection_mode(gtk::SelectionMode::None)
            .valign(gtk::Align::End)
            .build();
        let scroller = gtk::ScrolledWindow::builder()
            .child(&messages)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        // Stick to the newest message: whenever the content grows, scroll
        // to the bottom.
        // ponytail: also yanks the view down while reading older messages;
        // only scroll if already at the bottom once there is history to read.
        scroller.vadjustment().connect_upper_notify(|adjustment| {
            adjustment.set_value(adjustment.upper() - adjustment.page_size())
        });
        self.pages.add_named(&scroller, Some(jid));

        let conversation = Conversation {
            row: conversation_row,
            messages,
        };
        self.conversations
            .borrow_mut()
            .insert(jid.to_owned(), conversation.clone());
        conversation
    }
}

/// One chat line: avatar, author, time and text.
/// ponytail: no Discord-style grouping of consecutive messages from the same
/// author; add it if long conversations feel too noisy.
fn message_row(author: &str, body: &str) -> gtk::ListBoxRow {
    let time = glib::DateTime::now_local()
        .and_then(|now| now.format("%H:%M"))
        .map(|time| time.to_string())
        .unwrap_or_default();
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    head.append(&label(author, "author"));
    head.append(&label(&time, "timestamp"));

    let text = gtk::Label::builder()
        .label(body)
        .css_classes(["body"])
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .selectable(true)
        .xalign(0.0)
        .build();
    let column = gtk::Box::new(gtk::Orientation::Vertical, 2);
    column.append(&head);
    column.append(&text);

    let line = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    line.append(&avatar(author, false));
    line.append(&column);
    row(&line)
}

/// Non-interactive list row (messages are not selectable as rows).
fn row(child: &impl IsA<gtk::Widget>) -> gtk::ListBoxRow {
    gtk::ListBoxRow::builder()
        .child(child)
        .activatable(false)
        .build()
}

/// Round avatar with the name's first letter (Discord-like placeholder).
fn avatar(name: &str, small: bool) -> gtk::Label {
    let initial: String = name
        .chars()
        .next()
        .into_iter()
        .flat_map(char::to_uppercase)
        .collect();
    let avatar = gtk::Label::builder()
        .label(initial)
        .css_classes(["avatar"])
        .valign(if small {
            gtk::Align::Center
        } else {
            gtk::Align::Start
        })
        .build();
    if small {
        avatar.add_css_class("small");
    }
    avatar
}

fn label(text: &str, class: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .css_classes([class])
        .xalign(0.0)
        .build()
}

/// The button shows the theme it switches *to*.
fn theme_icon(theme: &Theme) -> &'static str {
    if theme.is_dark() { "☀" } else { "☾" }
}

/// `bob` from `bob@localhost`.
fn local_part(jid: &str) -> &str {
    jid.split('@').next().unwrap_or(jid)
}
