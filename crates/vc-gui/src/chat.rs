//! Main screen, modeled on the "Messages" view of the Discord redesign
//! community file in Figma, with our palette and iMessage-style bubbles:
//!
//! ```text
//! ┌──────┬───────────────────────┬───────────────────────────────────┐
//! │ (vc) │      [Messages]       │ (b) bob                   header  │
//! │      │ (🔍 find or start) (+)├───────────────────────────────────┤
//! │      │ ((b) bob            ) │         [November 17, 2026]       │
//! │      │ ((c) carol          ) │ (hey alice!)                      │
//! │      │                       │                    (hi bob, sup?) │
//! │  ☀   │                       │                                   │
//! │ (a)• │                       │ (Message @bob)          composer  │
//! └──────┴───────────────────────┴───────────────────────────────────┘
//! ```
//!
//! Each conversation is keyed by the contact's bare JID and owns a sidebar
//! row plus a message list in a [`gtk::Stack`], so incoming messages land in
//! the right place even when that conversation is not on screen.

use std::cell::{Cell, RefCell};
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
    commands: UnboundedSender<Command>,
    /// DM rows in the sidebar.
    list: gtk::ListBox,
    /// One message page per DM, plus the "empty" page.
    pages: gtk::Stack,
    /// Header with the avatar and name of the contact on screen.
    peer: gtk::Box,
    peer_avatar: gtk::Label,
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
    /// (year, month, day) of the last message, to date each new day.
    day: Rc<Cell<Option<(i32, i32, i32)>>>,
}

impl Chat {
    pub fn new(me: Jid, commands: UnboundedSender<Command>, theme: Theme) -> Rc<Chat> {
        let my_name = me.local.clone().unwrap_or_default();

        // ── Server rail: home on top, theme switch and our avatar at the
        // bottom. ponytail: only "home" (direct messages); servers and
        // channels need group chat (XEP-0045), which the server lacks.
        let home = gtk::Label::builder()
            .label("vc")
            .css_classes(["rail-item", "active"])
            .tooltip_text("Direct Messages")
            .build();
        let theme_button = gtk::Button::builder()
            .label(theme_icon(&theme))
            .css_classes(["icon"])
            .tooltip_text("Toggle light/dark theme")
            .halign(gtk::Align::Center)
            .build();
        let my_avatar = gtk::Overlay::builder()
            .child(&avatar(&my_name))
            .tooltip_text(format!("{me}\nOnline"))
            .halign(gtk::Align::Center)
            .build();
        my_avatar.add_overlay(
            &gtk::Box::builder()
                .css_classes(["status-dot"])
                .halign(gtk::Align::End)
                .valign(gtk::Align::End)
                .build(),
        );
        let rail = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .css_classes(["rail"])
            .build();
        rail.append(&home);
        rail.append(&gtk::Box::builder().vexpand(true).build());
        rail.append(&theme_button);
        rail.append(&my_avatar);

        // ── Sidebar: tab bar, DM search/creation and the conversation list.
        // ponytail: no "Friends" tab; it needs a roster, which the server
        // does not keep yet.
        let tabs = gtk::Box::builder().css_classes(["tabs"]).build();
        tabs.append(
            &gtk::Label::builder()
                .label("Messages")
                .css_classes(["tab", "active"])
                .hexpand(true)
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .build(),
        );

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Find or start a chat")
            .tooltip_text("Type a JID (user@domain) and press Enter")
            .hexpand(true)
            .build();
        let new_chat = gtk::Button::builder()
            .label("+")
            .css_classes(["round"])
            .tooltip_text("Start a conversation")
            .build();
        new_chat.connect_clicked({
            let search = search.clone();
            move |_| {
                search.grab_focus();
            }
        });
        let tools = gtk::Box::builder()
            .spacing(8)
            .css_classes(["sidebar-tools"])
            .build();
        tools.append(&search);
        tools.append(&new_chat);

        let list = gtk::ListBox::builder()
            .css_classes(["conversations"])
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        let list_scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        // Explicit `hexpand(false)`: in GTK 4 a child's hexpand (the search
        // entry) propagates to its parent, and the sidebar would steal half
        // of the window from the chat area.
        let sidebar = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["sidebar"])
            .hexpand(false)
            .width_request(260)
            .build();
        sidebar.append(&tabs);
        sidebar.append(&tools);
        sidebar.append(&list_scroll);

        // ── Chat area: header, message pages and composer.
        let peer_avatar = avatar("");
        let title = label("", "chat-title");
        let peer = gtk::Box::builder().spacing(10).visible(false).build();
        peer.append(&peer_avatar);
        peer.append(&title);
        let header = gtk::Box::builder().css_classes(["chat-header"]).build();
        header.append(&peer);

        let pages = gtk::Stack::builder().vexpand(true).build();
        let empty = gtk::Label::builder()
            .label("Pick a conversation or start a new one from the search on the left.")
            .css_classes(["empty"])
            .wrap(true)
            .build();
        pages.add_named(&empty, Some("empty"));

        let composer = gtk::Entry::builder()
            .placeholder_text("Pick a conversation")
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
            commands,
            list,
            pages,
            peer,
            peer_avatar,
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
        conversation.push(&bubble(body, false));
        if self.current.borrow().as_deref() != Some(from) {
            conversation.row.add_css_class("unread");
        }
    }

    /// The server bounced a message we sent to `to`.
    pub fn undelivered(&self, to: &str, condition: &str) {
        let text =
            format!("Message not delivered ({condition}): {to} is offline or does not exist.");
        let note = label(&text, "system");
        self.conversation(to).push(&row(&note));
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
        let name = local_part(jid);
        conversation.row.remove_css_class("unread");
        self.pages.set_visible_child_name(jid);
        self.peer_avatar.set_label(&initial(name));
        self.title.set_text(name);
        self.peer.set_tooltip_text(Some(jid));
        self.peer.set_visible(true);
        self.composer.set_sensitive(true);
        self.composer
            .set_placeholder_text(Some(&format!("Message @{name}")));
        self.composer.grab_focus();
        *self.current.borrow_mut() = Some(jid.to_owned());
    }

    /// Sends the composer text to the current conversation and shows it
    /// right away.
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
            self.conversation(&to).push(&bubble(text, true));
        }
    }

    /// The conversation with `jid`, creating its sidebar row and message page
    /// on first use.
    fn conversation(&self, jid: &str) -> Conversation {
        if let Some(conversation) = self.conversations.borrow().get(jid) {
            return conversation.clone();
        }
        let entry = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        entry.append(&avatar(local_part(jid)));
        let name = label(local_part(jid), "dm-name");
        name.set_ellipsize(pango::EllipsizeMode::End);
        entry.append(&name);
        let conversation_row = gtk::ListBoxRow::builder()
            .child(&entry)
            .tooltip_text(jid)
            .build();
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
            day: Rc::default(),
        };
        self.conversations
            .borrow_mut()
            .insert(jid.to_owned(), conversation.clone());
        conversation
    }
}

impl Conversation {
    /// Appends a message, preceded by a date pill when it is the first one
    /// of the day.
    fn push(&self, message: &gtk::ListBoxRow) {
        if let Ok(now) = glib::DateTime::now_local() {
            let day = (now.year(), now.month(), now.day_of_month());
            if self.day.replace(Some(day)) != Some(day) {
                let pill = gtk::Label::builder()
                    .label(day_label(day))
                    .css_classes(["date-pill"])
                    .halign(gtk::Align::Center)
                    .build();
                self.messages.append(&row(&pill));
            }
        }
        self.messages.append(message);
    }
}

/// "November 17, 2026", always in English, whatever the system locale.
fn day_label((year, month, day): (i32, i32, i32)) -> String {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    format!("{} {day}, {year}", MONTHS[(month - 1) as usize])
}

/// One message as an iMessage-style speech bubble: ours on the right in the
/// accent color, theirs on the left in gray. The time shows on hover.
/// ponytail: no tighter spacing for consecutive bubbles from the same side;
/// add it if long conversations feel too loose.
fn bubble(body: &str, mine: bool) -> gtk::ListBoxRow {
    let time = glib::DateTime::now_local()
        .and_then(|now| now.format("%H:%M"))
        .map(|time| time.to_string())
        .unwrap_or_default();
    let text = gtk::Label::builder()
        .label(body)
        .css_classes(["bubble", if mine { "mine" } else { "theirs" }])
        .halign(if mine {
            gtk::Align::End
        } else {
            gtk::Align::Start
        })
        .wrap(true)
        .wrap_mode(pango::WrapMode::WordChar)
        .max_width_chars(50)
        .selectable(true)
        .xalign(0.0)
        .tooltip_text(time)
        .build();
    row(&text)
}

/// Non-interactive list row (messages are not selectable as rows).
fn row(child: &impl IsA<gtk::Widget>) -> gtk::ListBoxRow {
    gtk::ListBoxRow::builder()
        .child(child)
        .activatable(false)
        .build()
}

/// Round avatar with the name's first letter (Discord-like placeholder).
fn avatar(name: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(initial(name))
        .css_classes(["avatar"])
        .valign(gtk::Align::Center)
        .build()
}

/// The name's first letter, uppercased.
fn initial(name: &str) -> String {
    name.chars()
        .next()
        .into_iter()
        .flat_map(char::to_uppercase)
        .collect()
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

#[test]
fn dates_days_in_english() {
    assert_eq!(day_label((2026, 11, 17)), "November 17, 2026");
    assert_eq!(day_label((2024, 1, 2)), "January 2, 2024");
}
