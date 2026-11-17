//! Main screen, after the "Messages" view of the Discord redesign community
//! file in Figma (node 1:4272), with our palette and iMessage-style bubbles:
//!
//! ```text
//! ┌────┬────────────────────────┬──────────────────────────────────────────┐
//! │ 🔍 │ [Messages]   Friends   │ ◫ (b) bob          📞 🎥 🔕 📌 🔍 ◫  – × │
//! │ 💬 │ (⇅ Newest     ) (🔍)(+) ├──────────────────────────────────────────┤
//! │ ── │ [find or start a chat] │            [November 17, 2026]           │
//! │ ⊞  │ ((b) bob             ) │ (hey alice!)                             │
//! │    │ ((c) carol           ) │                          (hi bob, sup?) │
//! │ ☀  │                        │                                          │
//! │(a)•│                        │ (⊕ Message @bob                      ☺) │
//! └────┴────────────────────────┴──────────────────────────────────────────┘
//! ```
//!
//! The window has no title bar: the tab bar and the chat header move it, and
//! the header carries the window controls, as in the design.
//!
//! Icons the app has no feature for yet (calls, pins, servers...) stay in
//! place so the layout matches the design; their tooltips say so.
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

        // ── Rail: search, home (direct messages) and servers on top; theme
        // switch and our avatar at the bottom, where the design has the inbox
        // and the profile. ponytail: no servers; they need group chat
        // (XEP-0045), which the server lacks.
        let find_icon = icon_button("vc-search-symbolic", "Find or start a conversation");
        let home = gtk::Image::builder()
            .icon_name("vc-messages-symbolic")
            .css_classes(["rail-home"])
            .tooltip_text("Direct Messages")
            .build();
        let divider = gtk::Box::builder()
            .css_classes(["rail-divider"])
            .halign(gtk::Align::Center)
            .build();
        let servers = icon_button("vc-grid-symbolic", "Servers are not supported yet");
        let theme_button = icon_button(theme_icon(&theme), "Toggle light/dark theme");
        let profile = gtk::Overlay::builder().child(&avatar(&my_name)).build();
        profile.add_overlay(
            &gtk::Box::builder()
                .css_classes(["status-dot"])
                .halign(gtk::Align::End)
                .valign(gtk::Align::End)
                .build(),
        );
        let profile_ring = gtk::Box::builder()
            .css_classes(["profile"])
            .halign(gtk::Align::Center)
            .tooltip_text(format!("{me}\nOnline"))
            .build();
        profile_ring.append(&profile);
        let rail = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .css_classes(["rail"])
            .build();
        for widget in [
            find_icon.upcast_ref::<gtk::Widget>(),
            home.upcast_ref(),
            divider.upcast_ref(),
            servers.upcast_ref(),
        ] {
            widget.set_halign(gtk::Align::Center);
            rail.append(widget);
        }
        rail.append(&gtk::Box::builder().vexpand(true).build());
        theme_button.set_halign(gtk::Align::Center);
        rail.append(&theme_button);
        rail.append(&profile_ring);

        // ── Sidebar: tab bar, "Newest" pill with search and new-chat
        // buttons, the JID search they reveal, and the conversation list.
        // ponytail: "Friends" is only drawn; it needs a roster, which the
        // server does not keep yet.
        let tabs = gtk::Box::builder()
            .homogeneous(true)
            .spacing(8)
            .css_classes(["tabs"])
            .build();
        tabs.append(&tab("Messages", true, "Direct messages"));
        tabs.append(&tab(
            "Friends",
            false,
            "Friends are not supported yet: the server keeps no roster",
        ));

        let newest = gtk::Box::builder()
            .spacing(6)
            .css_classes(["pill"])
            .hexpand(true)
            .tooltip_text("Newest conversations first")
            .build();
        newest.append(&gtk::Image::from_icon_name("vc-sort-symbolic"));
        newest.append(&label("Newest", "pill-label"));
        let find = round_button("vc-search-symbolic", "Find a conversation");
        let new_chat = round_button("vc-plus-symbolic", "Start a conversation");
        let tools = gtk::Box::builder()
            .spacing(6)
            .css_classes(["sidebar-tools"])
            .build();
        tools.append(&newest);
        tools.append(&find);
        tools.append(&new_chat);

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Type a JID (user@domain)")
            .tooltip_text("Type a JID (user@domain) and press Enter")
            .build();
        let search_reveal = gtk::Revealer::builder().child(&search).build();
        for button in [&find_icon, &find, &new_chat] {
            let (reveal, search) = (search_reveal.clone(), search.clone());
            button.connect_clicked(move |_| {
                reveal.set_reveal_child(true);
                search.grab_focus();
            });
        }
        search.connect_stop_search(|entry| {
            entry.set_text("");
            hide_search(entry);
        });

        let list = gtk::ListBox::builder()
            .css_classes(["conversations"])
            .selection_mode(gtk::SelectionMode::Single)
            .build();
        let list_scroll = gtk::ScrolledWindow::builder()
            .child(&list)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();

        // Explicit `hexpand(false)`: in GTK 4 a child's hexpand (the "Newest"
        // pill) propagates to its parent, and the sidebar would steal half of
        // the window from the chat area.
        let sidebar = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["sidebar"])
            .hexpand(false)
            .width_request(263)
            .build();
        sidebar.append(&gtk::WindowHandle::builder().child(&tabs).build());
        sidebar.append(&tools);
        sidebar.append(&search_reveal);
        sidebar.append(&list_scroll);

        // ── Chat area: header, message pages and composer.
        let toggle_sidebar = icon_button("vc-sidebar-symbolic", "Show or hide the conversations");
        toggle_sidebar.connect_clicked({
            let sidebar = sidebar.clone();
            move |_| sidebar.set_visible(!sidebar.is_visible())
        });
        let peer_avatar = avatar("");
        let title = label("", "chat-title");
        let peer = gtk::Box::builder().spacing(8).visible(false).build();
        peer.append(&peer_avatar);
        peer.append(&title);
        let actions = gtk::Box::builder()
            .spacing(6)
            .hexpand(true)
            .halign(gtk::Align::End)
            .build();
        for (icon, tooltip) in [
            ("vc-call-symbolic", "Calls are not supported yet"),
            ("vc-video-symbolic", "Video calls are not supported yet"),
            (
                "vc-muted-symbolic",
                "Notification settings are not supported yet",
            ),
            (
                "vc-pinned-symbolic",
                "Pinned messages are not supported yet",
            ),
            ("vc-search-symbolic", "Message search is not supported yet"),
        ] {
            actions.append(&icon_button(icon, tooltip));
        }
        let profile_panel = icon_button(
            "vc-sidebar-symbolic",
            "The profile panel is not supported yet",
        );
        profile_panel.add_css_class("flipped");
        actions.append(&profile_panel);
        let header = gtk::Box::builder()
            .spacing(8)
            .css_classes(["chat-header"])
            .build();
        header.append(&toggle_sidebar);
        header.append(&peer);
        header.append(&actions);
        header.append(&gtk::WindowControls::new(gtk::PackType::End));

        let pages = gtk::Stack::builder().vexpand(true).build();
        let empty = gtk::Label::builder()
            .label("Pick a conversation or start a new one with the + button.")
            .css_classes(["empty"])
            .wrap(true)
            .build();
        pages.add_named(&empty, Some("empty"));

        let composer = gtk::Entry::builder()
            .placeholder_text("Pick a conversation")
            .css_classes(["composer"])
            .primary_icon_name("vc-add-symbolic")
            .primary_icon_tooltip_text("Attachments are not supported yet")
            .show_emoji_icon(true)
            .sensitive(false)
            .hexpand(true)
            .build();
        let composer_box = gtk::Box::builder().css_classes(["composer-box"]).build();
        composer_box.append(&composer);

        let main = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .build();
        main.append(&gtk::WindowHandle::builder().child(&header).build());
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
                    hide_search(entry);
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
            button.set_icon_name(theme_icon(&theme));
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
        // Newest first, as the "Newest" pill says.
        self.list.prepend(&conversation_row);

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

/// Hides the JID search revealer around `entry`.
fn hide_search(entry: &gtk::SearchEntry) {
    if let Some(reveal) = entry.parent().and_downcast::<gtk::Revealer>() {
        reveal.set_reveal_child(false);
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

/// Flat icon button; its size comes from the stylesheet (`-gtk-icon-size`).
fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .css_classes(["icon"])
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .build()
}

/// Round outlined icon button, as next to the "Newest" pill.
fn round_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = icon_button(icon, tooltip);
    button.set_css_classes(&["round"]);
    button
}

fn tab(text: &str, active: bool, tooltip: &str) -> gtk::Label {
    let tab = gtk::Label::builder()
        .label(text)
        .css_classes(["tab"])
        .tooltip_text(tooltip)
        .build();
    if active {
        tab.add_css_class("active");
    }
    tab
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
    if theme.is_dark() {
        "weather-clear-symbolic"
    } else {
        "weather-clear-night-symbolic"
    }
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
