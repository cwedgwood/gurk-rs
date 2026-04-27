use std::borrow::Cow;
use std::io::Cursor;
use std::path::Path;

use anyhow::Context as _;
use arboard::ImageData;
use chrono::{DateTime, Local, TimeZone};
use crokey::Combiner;
use crossterm::event::{KeyCode, KeyEvent};
use image::codecs::png::PngEncoder;
use image::{ImageBuffer, ImageEncoder, Rgba};
use presage::libsignal_service::sender::AttachmentSpec;
use tracing::{error, info};
use uuid::Uuid;

use crate::command::{
    Command, DirectionVertical, MoveAmountText, MoveAmountVisual, MoveDirection, Widget, WindowMode,
};
use crate::data::{AssociatedValue, BodyRange, ChannelId, Message};
use crate::storage::MessageId;
use crate::util::{ATTACHMENT_REGEX, URL_REGEX};

use super::{App, HandleReactionOptions, open_file, open_url, to_emoji};

impl App {
    async fn on_command(&mut self, command: Command) -> anyhow::Result<()> {
        match command {
            Command::Help => self.toggle_help(),
            Command::MoveText(MoveDirection::Previous, MoveAmountText::Word) => {
                self.get_input().move_back_word()
            }
            Command::MoveText(MoveDirection::Previous, MoveAmountText::Character) => {
                self.get_input().on_left()
            }
            Command::MoveText(MoveDirection::Previous, MoveAmountText::Line) => {
                self.get_input().move_line_up()
            }
            Command::MoveText(MoveDirection::Next, MoveAmountText::Word) => {
                self.get_input().move_forward_word()
            }
            Command::MoveText(MoveDirection::Next, MoveAmountText::Character) => {
                self.get_input().on_right()
            }
            Command::MoveText(MoveDirection::Next, MoveAmountText::Line) => {
                self.get_input().move_line_down()
            }
            Command::SelectMessage(MoveDirection::Previous, MoveAmountVisual::Entry) => {
                self.on_pgup()
            }
            Command::SelectMessage(MoveDirection::Next, MoveAmountVisual::Entry) => self.on_pgdn(),
            Command::KillBackwardLine => self.get_input().on_delete_line(),
            Command::KillWord => self.get_input().on_delete_word(),
            Command::CopyMessage(_) => self.copy_selection(),
            Command::KillLine => self.get_input().on_delete_suffix(),
            Command::SelectChannel(MoveDirection::Previous) => self.select_previous_channel(),
            Command::SelectChannel(MoveDirection::Next) => self.select_next_channel(),
            Command::SelectChannelModal(MoveDirection::Previous) => self.select_channel_prev(),
            Command::SelectChannelModal(MoveDirection::Next) => self.select_channel_next(),
            Command::KillWholeLine => self.get_input().on_delete_line(),
            Command::BeginningOfLine => self.get_input().on_home(),
            Command::EndOfLine => self.get_input().on_end(),
            Command::EditMessage => {
                self.start_editing();
            }
            // Command::ReplyMessage => unimplemented!("{command:?}"),
            Command::DeleteMessage => {
                self.delete_selected_message();
            }
            Command::ToggleChannelModal => {
                if !self.select_channel.is_shown {
                    self.select_channel.reset(&*self.storage);
                }
                self.select_channel.is_shown = !self.select_channel.is_shown;
            }
            Command::ToggleMultiline => {
                self.is_multiline_input = !self.is_multiline_input;
            }
            Command::React(reaction) => {
                // Tab with @partial before cursor: attempt mention completion
                if reaction.is_none() {
                    use crate::config::MentionCompletionStyle;
                    let completed = match self.config.mention_completion {
                        MentionCompletionStyle::None => false,
                        MentionCompletionStyle::Readline => self.try_mention_completion(),
                        MentionCompletionStyle::Cycle => self.try_mention_completion_cycling(),
                        MentionCompletionStyle::Menu => {
                            // Menu mode uses the popup, Tab not needed to trigger
                            false
                        }
                    };
                    if completed {
                        return Ok(());
                    }
                }
                if let Some(idx) = self.channels.state.selected() {
                    self.add_reaction(idx, reaction).await;
                }
            }
            Command::OpenUrl => {
                self.try_open_url();
            }
            Command::OpenFile => {
                self.try_open_file();
            }
            Command::DeleteCharacter(MoveDirection::Previous) => {
                self.get_input().on_backspace();
            }
            Command::DeleteCharacter(MoveDirection::Next) => {
                self.get_input().on_delete();
            }
            Command::Quit => {
                self.should_quit = true;
            }
            Command::Redraw => {
                self.should_clear = true;
            }
            Command::Scroll(Widget::Help, DirectionVertical::Up, MoveAmountVisual::Entry) => {
                if self.help_scroll.0 >= 1 {
                    self.help_scroll.0 -= 1
                }
            }
            Command::Scroll(Widget::Help, DirectionVertical::Down, MoveAmountVisual::Entry) => {
                // TODO: prevent overscrolling
                self.help_scroll.0 += 1
            }
            Command::ToggleMuteChannel => self.toggle_mute_channel(),
            Command::ToggleChannelList => self.toggle_channel_list(),
            Command::OpenEditor => {
                self.open_editor_requested = true;
            }
            Command::NoOp => {}
        }
        Ok(())
    }

    pub async fn on_key(&mut self, key: KeyEvent) -> anyhow::Result<()> {
        if key.code != KeyCode::Tab {
            self.mention_cycle = None;
        }

        // Handle mention popup if active
        if self.mention_popup.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.mention_popup = None;
                    return Ok(());
                }
                KeyCode::Enter | KeyCode::Tab => {
                    self.accept_mention_popup();
                    return Ok(());
                }
                KeyCode::Up => {
                    if let Some(ref mut popup) = self.mention_popup {
                        let i = popup.state.selected().unwrap_or(0);
                        if i > 0 {
                            popup.state.select(Some(i - 1));
                        }
                    }
                    return Ok(());
                }
                KeyCode::Down => {
                    if let Some(ref mut popup) = self.mention_popup {
                        let i = popup.state.selected().unwrap_or(0);
                        if i + 1 < popup.matches.len() {
                            popup.state.select(Some(i + 1));
                        }
                    }
                    return Ok(());
                }
                KeyCode::Char(c) => {
                    self.get_input().put_char(c);
                    self.update_mention_popup();
                    return Ok(());
                }
                KeyCode::Backspace => {
                    self.get_input().on_backspace();
                    self.update_mention_popup();
                    return Ok(());
                }
                _ => {
                    self.mention_popup = None;
                }
            }
        }

        if let Some(cmd) = self.event_to_command(&key) {
            self.on_command(cmd.clone()).await?;
        } else {
            match key.code {
                KeyCode::Char('\r') => self.get_input().put_char('\n'),
                KeyCode::Enter => {
                    if !self.select_channel.is_shown {
                        if self.is_multiline_input {
                            self.get_input().new_line();
                        } else if !self.input.data.is_empty() {
                            if let Some(idx) = self.channels.state.selected() {
                                self.send_input(idx);
                            }
                        } else {
                            // input is empty
                            self.try_open_url_or_file();
                        }
                    } else if self.select_channel.is_shown
                        && let Some(channel_id) = self.select_channel.selected_channel_id().copied()
                    {
                        self.select_channel.is_shown = false;
                        let old = self.channels.selected_item().copied();
                        let (idx, _) = self
                            .channels
                            .items
                            .iter()
                            .enumerate()
                            .find(|(_, id)| **id == channel_id)
                            .context("channel disappeared during channel select popup")?;
                        self.channels.state.select(Some(idx));
                        let new = self.channels.selected_item().copied();
                        self.swap_channel_draft(old, new);
                    }
                }
                KeyCode::Esc if !self.reset_editing() => {
                    self.reset_message_selection();
                }
                KeyCode::Char(c) => {
                    self.get_input().put_char(c);
                    if c == '@'
                        && self.config.mention_completion
                            == crate::config::MentionCompletionStyle::Menu
                    {
                        self.open_mention_popup();
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn try_open_url_or_file(&mut self) -> Option<()> {
        self.try_open_url().or_else(|| self.try_open_file())
    }

    /// Tries to open the first url in the selected message.
    ///
    /// Does nothing if no message is selected and no url is contained in the message.
    fn try_open_url(&mut self) -> Option<()> {
        let message = self.selected_message()?;
        open_url(&message, &URL_REGEX)?;
        self.reset_message_selection();
        Some(())
    }

    /// Tries to open the first file attachment in the selected message.
    ///
    /// Does nothing if no message is selected and the message contains no attachments.
    fn try_open_file(&mut self) -> Option<()> {
        let message = self.selected_message()?;
        open_file(&message)?;
        self.reset_message_selection();
        Some(())
    }

    fn selected_message_id(&self) -> Option<MessageId> {
        let channel_id = self.channels.selected_item()?;
        let messages = self.messages.get(channel_id)?;
        let message_idx = messages.state.selected()?;
        let arrived_at = messages.items[messages
            .items
            .len()
            .checked_sub(message_idx)?
            .checked_sub(1)?];
        Some(MessageId::new(*channel_id, arrived_at))
    }

    pub(super) fn selected_message(&self) -> Option<Cow<'_, Message>> {
        let message_id = self.selected_message_id()?;
        let message = self.storage.message(message_id);
        info!("selected message: {message:?}");
        message
    }

    /// Returns Some(_) reaction if input is a reaction.
    ///
    /// Inner is None, if the reaction should be removed.
    fn take_reaction(&mut self) -> Option<Option<String>> {
        let input_box = self.get_input();
        if input_box.data.is_empty() {
            Some(None)
        } else {
            let emoji = to_emoji(&input_box.data)?.to_string();
            self.take_input();
            Some(Some(emoji))
        }
    }

    pub async fn add_reaction(
        &mut self,
        channel_idx: usize,
        reaction: Option<String>,
    ) -> Option<()> {
        let reaction = reaction.or_else(|| self.take_reaction()?);
        let channel = self.storage.channel(self.channels.items[channel_idx])?;
        let message = self.selected_message()?;
        let remove = reaction.is_none();
        let emoji = reaction.or_else(|| {
            // find emoji which should be removed
            // if no emoji found => there is no reaction from us => nothing to remove
            message.reactions.iter().find_map(|(id, emoji)| {
                if id == &self.signal_manager.user_id() {
                    Some(emoji.clone())
                } else {
                    None
                }
            })
        })?;

        self.signal_manager
            .send_reaction(&channel, &message, emoji.clone(), remove);

        let channel_id = channel.id;
        let arrived_at = message.arrived_at;
        self.handle_reaction(
            channel_id,
            arrived_at,
            self.signal_manager.user_id(),
            emoji,
            HandleReactionOptions::new().remove(remove),
        )
        .await;

        self.reset_unread_messages();
        self.bubble_up_channel(channel_idx);
        self.reset_message_selection();

        Some(())
    }

    fn take_input(&mut self) -> String {
        self.get_input().take()
    }

    pub(super) fn send_input(&mut self, channel_idx: usize) {
        let input = self.take_input();
        let (input, attachments) = Self::extract_attachments(&input, Local::now(), || {
            self.clipboard.as_mut().map(|c| c.get_image())
        });
        let channel_id = self.channels.items[channel_idx];
        let channel = self
            .storage
            .channel(channel_id)
            .expect("non-existent channel");
        let (input, body_ranges) = self.parse_mentions(&channel, input);
        if !body_ranges.is_empty() || input.contains("XYZZY") {
            info!(?input, ?body_ranges, "XYZZY mention parse result");
        }
        let editing = self.editing.take();
        let quote = editing.is_none().then(|| self.selected_message()).flatten();
        let (sent_message, response) = self.signal_manager.send_text(
            &channel,
            input,
            quote.as_deref(),
            editing.map(|id| id.arrived_at),
            attachments,
            body_ranges,
        );

        let message_id = MessageId::new(channel_id, sent_message.arrived_at);
        let tx = self.event_tx.clone();
        tokio::spawn(async move {
            if let Ok(result) = response.await {
                tx.send(crate::event::Event::SentTextResult { message_id, result })
                    .expect("event sender gone");
            } else {
                error!(?message_id, "response for sending message was lost");
            }
        });

        if let Some(id) = editing {
            self.storage
                .store_edited_message(channel_id, id.arrived_at, sent_message);
        } else {
            let sent_message = self.storage.store_message(channel_id, sent_message);
            self.messages
                .get_mut(&channel_id)
                .expect("non-existent channel")
                .items
                .push(sent_message.arrived_at);
        };

        self.reset_message_selection();
        self.reset_unread_messages();
        self.bubble_up_channel(channel_idx);
    }

    pub fn copy_selection(&mut self) {
        if let Some(message) = self.selected_message()
            && let Some(text) = message.message.as_ref()
        {
            let text = text.clone();
            if let Some(clipboard) = self.clipboard.as_mut() {
                if let Err(error) = clipboard.set_text(text) {
                    error!(%error, "failed to copy text to clipboard");
                } else {
                    info!("copied selected text to clipboard");
                }
            }
        }
    }

    /// Returns `true` if editing was reset, otherwise `false`
    fn reset_editing(&mut self) -> bool {
        let is_reset = self.editing.take().is_some();
        if is_reset {
            self.take_input();
        }
        is_reset
    }

    fn start_editing(&mut self) -> Option<()> {
        if !self.input.is_empty() {
            return None;
        }

        let message_id = self.selected_message_id()?;
        let message = self.storage.message(message_id)?;
        if message.from_id != self.user_id {
            return None;
        }

        let target_sent_timestamp = self
            .storage
            .edits(message_id)
            .last()
            .map(|last_edit| last_edit.arrived_at)
            .unwrap_or(message.arrived_at);
        let message_id = MessageId::new(message_id.channel_id, target_sent_timestamp);
        let text = message.message.clone()?;

        self.editing.replace(message_id);
        self.input.data = text;
        self.input.on_end();

        Some(())
    }

    fn delete_selected_message(&mut self) -> Option<()> {
        let message_id = self.selected_message_id()?;
        let message = self.storage.message(message_id)?.into_owned();

        // Only allow deleting own messages
        if message.from_id != self.user_id {
            return None;
        }

        let channel_id = message_id.channel_id;
        let channel = self.storage.channel(channel_id)?.into_owned();

        if message.deleted || channel_id == ChannelId::User(self.user_id) {
            // Tombstone or Note to Self: fully remove, sync via deleteForMe
            self.signal_manager.send_delete_for_me(&channel, &message);
            self.storage.remove_message(message_id);
            self.remove_message_from_view(channel_id, message.arrived_at);
        } else {
            // Regular channel: delete for everyone (tombstone)
            self.signal_manager
                .send_delete(&channel, message.arrived_at);
            self.storage.delete_message(message_id);
        }

        Some(())
    }

    /// Attempt bash-style tab completion for @mentions.
    /// Returns true if completion was attempted (even if no match).
    fn try_mention_completion(&mut self) -> bool {
        // Find the @ before the cursor
        let input = &self.input.data;
        let cursor_byte = self.input.cursor.idx;
        let before_cursor = &input[..cursor_byte];
        let at_pos = match before_cursor.rfind('@') {
            Some(pos) => pos,
            None => return false,
        };

        // Must be at start or after whitespace
        if at_pos > 0 && !before_cursor.as_bytes()[at_pos - 1].is_ascii_whitespace() {
            return false;
        }

        let partial = &before_cursor[at_pos + 1..];
        if partial.is_empty() {
            self.bell();
            return true;
        }

        // Get channel members
        let channel_id = match self.channels.selected_item() {
            Some(id) => *id,
            None => return false,
        };
        let channel = match self.storage.channel(channel_id) {
            Some(c) => c,
            None => return false,
        };
        let members: Vec<Uuid> = match channel.group_data.as_ref() {
            Some(gd) => gd.members.clone(),
            None => {
                if let ChannelId::User(uuid) = channel.id {
                    vec![uuid]
                } else {
                    return false;
                }
            }
        };

        // Find matching names
        let partial_lower = partial.to_lowercase();
        let matches: Vec<(String, Uuid)> = members
            .iter()
            .map(|&uuid| (self.name_by_id_cached(uuid), uuid))
            .filter(|(name, _)| name.to_lowercase().starts_with(&partial_lower))
            .collect();

        if matches.is_empty() {
            return true; // attempted but no match
        }

        if matches.len() == 1 {
            // Unique match: complete and add space
            let name = &matches[0].0;
            let completion = &name[partial.len()..];
            let insert_pos = cursor_byte;
            self.input.data.insert_str(insert_pos, completion);
            self.input.data.insert(insert_pos + completion.len(), ' ');
            for _ in 0..completion.len() + 1 {
                self.input.on_right();
            }
        } else {
            // Multiple matches: complete to longest common prefix
            let first = matches[0].0.to_lowercase();
            let mut prefix_len = first.len();
            for (name, _) in &matches[1..] {
                let name_lower = name.to_lowercase();
                prefix_len = first
                    .chars()
                    .zip(name_lower.chars())
                    .take_while(|(a, b)| a == b)
                    .count();
            }
            if prefix_len > partial.len() {
                // Can extend the partial
                let original_name = &matches[0].0;
                let completion = &original_name[partial.len()..prefix_len];
                let insert_pos = cursor_byte;
                self.input.data.insert_str(insert_pos, completion);
                for _ in 0..completion.len() {
                    self.input.on_right();
                }
            }
            self.bell();
        }
        true
    }

    fn open_mention_popup(&mut self) {
        let at_byte_pos = self.input.cursor.idx - 1; // cursor is after the '@'

        let channel_id = match self.channels.selected_item() {
            Some(id) => *id,
            None => return,
        };
        let channel = match self.storage.channel(channel_id) {
            Some(c) => c,
            None => return,
        };
        let members: Vec<Uuid> = match channel.group_data.as_ref() {
            Some(gd) => gd.members.clone(),
            None => {
                if let ChannelId::User(uuid) = channel.id {
                    vec![uuid]
                } else {
                    return;
                }
            }
        };

        let matches: Vec<(String, Uuid)> = members
            .iter()
            .map(|&uuid| (self.name_by_id_cached(uuid), uuid))
            .collect();

        if matches.is_empty() {
            return;
        }

        let mut state = ratatui::widgets::ListState::default();
        state.select(Some(0));
        self.mention_popup = Some(super::MentionPopupState {
            at_byte_pos,
            matches,
            state,
        });
    }

    fn update_mention_popup(&mut self) {
        let Some(ref popup) = self.mention_popup else {
            return;
        };
        let at_pos = popup.at_byte_pos;
        let cursor_byte = self.input.cursor.idx;

        // Check if cursor is still after the @
        if cursor_byte <= at_pos {
            self.mention_popup = None;
            return;
        }

        let partial = &self.input.data[at_pos + 1..cursor_byte];
        let partial_lower = partial.to_lowercase();

        let channel_id = match self.channels.selected_item() {
            Some(id) => *id,
            None => {
                self.mention_popup = None;
                return;
            }
        };
        let channel = match self.storage.channel(channel_id) {
            Some(c) => c,
            None => {
                self.mention_popup = None;
                return;
            }
        };
        let members: Vec<Uuid> = match channel.group_data.as_ref() {
            Some(gd) => gd.members.clone(),
            None => {
                if let ChannelId::User(uuid) = channel.id {
                    vec![uuid]
                } else {
                    self.mention_popup = None;
                    return;
                }
            }
        };

        let matches: Vec<(String, Uuid)> = members
            .iter()
            .map(|&uuid| (self.name_by_id_cached(uuid), uuid))
            .filter(|(name, _)| {
                partial_lower.is_empty() || name.to_lowercase().starts_with(&partial_lower)
            })
            .collect();

        if matches.is_empty() {
            self.mention_popup = None;
            return;
        }

        let popup = self.mention_popup.as_mut().unwrap();
        popup.matches = matches;
        // Clamp selection
        if let Some(sel) = popup.state.selected() {
            if sel >= popup.matches.len() {
                popup.state.select(Some(popup.matches.len() - 1));
            }
        } else {
            popup.state.select(Some(0));
        }
    }

    fn accept_mention_popup(&mut self) {
        let Some(popup) = self.mention_popup.take() else {
            return;
        };
        let selected = popup.state.selected().unwrap_or(0);
        if selected >= popup.matches.len() {
            return;
        }
        let (name, _uuid) = &popup.matches[selected];
        let cursor_byte = self.input.cursor.idx;

        // Replace @partial with @Name and add space
        let replace_start = popup.at_byte_pos + 1; // after @
        let name_and_space = format!("{name} ");
        self.input
            .data
            .replace_range(replace_start..cursor_byte, &name_and_space);

        // Move cursor to after the space
        self.input.cursor.idx = replace_start;
        self.input.cursor.col = self.input.data[..replace_start].chars().count();
        for _ in 0..name_and_space.chars().count() {
            self.input.on_right();
        }
    }

    /// Cycling tab completion for @mentions.
    /// First Tab inserts first match. Subsequent Tabs cycle through matches.
    fn try_mention_completion_cycling(&mut self) -> bool {
        let cursor_byte = self.input.cursor.idx;
        let before_cursor = self.input.data[..cursor_byte].to_string();

        // Check if we're continuing a cycle
        let cycle_action = self.mention_cycle.as_ref().and_then(|cycle| {
            let expected_end = cycle.at_byte_pos + 1 + cycle.matches[cycle.index].len();
            if cursor_byte == expected_end {
                Some((
                    cycle.matches[cycle.index].len(),
                    (cycle.index + 1) % cycle.matches.len(),
                    cycle.matches[(cycle.index + 1) % cycle.matches.len()].clone(),
                    cycle.at_byte_pos + 1,
                ))
            } else {
                None
            }
        });

        if let Some((old_name_len, new_index, new_name, replace_start)) = cycle_action {
            let replace_end = replace_start + old_name_len;
            let col_at_start = before_cursor[..replace_start].chars().count();
            let new_name_chars = new_name.chars().count();

            self.input
                .data
                .replace_range(replace_start..replace_end, &new_name);

            self.input.cursor.idx = replace_start;
            self.input.cursor.col = col_at_start;
            for _ in 0..new_name_chars {
                self.input.on_right();
            }

            self.mention_cycle.as_mut().unwrap().index = new_index;
            return true;
        }

        // Reset cycle if cursor moved
        if self.mention_cycle.is_some() {
            self.mention_cycle = None;
        }

        // Start a new completion
        let at_pos = match before_cursor.rfind('@') {
            Some(pos) => pos,
            None => return false,
        };
        if at_pos > 0 && !before_cursor.as_bytes()[at_pos - 1].is_ascii_whitespace() {
            return false;
        }

        let partial = &before_cursor[at_pos + 1..];

        let channel_id = match self.channels.selected_item() {
            Some(id) => *id,
            None => return false,
        };
        let channel = match self.storage.channel(channel_id) {
            Some(c) => c,
            None => return false,
        };
        let members: Vec<Uuid> = match channel.group_data.as_ref() {
            Some(gd) => gd.members.clone(),
            None => {
                if let ChannelId::User(uuid) = channel.id {
                    vec![uuid]
                } else {
                    return false;
                }
            }
        };

        let partial_lower = partial.to_lowercase();
        let matches: Vec<String> = members
            .iter()
            .map(|&uuid| self.name_by_id_cached(uuid))
            .filter(|name| name.to_lowercase().starts_with(&partial_lower))
            .collect();

        if matches.is_empty() {
            self.bell();
            return true;
        }

        // Insert first match
        let completion = &matches[0][partial.len()..];
        let comp_len = completion.len();
        if matches.len() == 1 {
            // Unique match: complete and add space
            let to_insert = format!("{completion} ");
            self.input.data.insert_str(cursor_byte, &to_insert);
            for _ in 0..to_insert.chars().count() {
                self.input.on_right();
            }
        } else {
            self.input.data.insert_str(cursor_byte, completion);
            for _ in 0..comp_len {
                self.input.on_right();
            }
        }

        self.mention_cycle = Some(super::MentionCycleState {
            at_byte_pos: at_pos,
            matches,
            index: 0,
        });

        true
    }

    /// Parse @mentions in the input text and replace with placeholder characters.
    /// Returns the modified text and a list of BodyRanges for the mentions.
    fn parse_mentions(
        &self,
        channel: &crate::data::Channel,
        input: String,
    ) -> (String, Vec<BodyRange>) {
        let members = match channel.group_data.as_ref() {
            Some(group_data) => &group_data.members,
            None => {
                // DM: the only mentionable user is the other party
                if let ChannelId::User(uuid) = channel.id {
                    return self.parse_mentions_with_members(&[uuid], input);
                }
                return (input, vec![]);
            }
        };
        self.parse_mentions_with_members(members, input)
    }

    fn parse_mentions_with_members(
        &self,
        members: &[Uuid],
        input: String,
    ) -> (String, Vec<BodyRange>) {
        // Build name → UUID map from group members
        let name_to_uuid: Vec<(String, Uuid)> = members
            .iter()
            .map(|&uuid| (self.name_by_id_cached(uuid).to_lowercase(), uuid))
            .collect();

        let mut result = String::with_capacity(input.len());
        let mut body_ranges = Vec::new();
        let mut chars = input.char_indices().peekable();
        let mut char_pos: u16 = 0;

        while let Some((i, ch)) = chars.next() {
            if ch == '@' {
                // Try to match a member name after @
                let rest = &input[i + 1..];
                let mut matched = None;
                for (name, uuid) in &name_to_uuid {
                    if rest.to_lowercase().starts_with(name.as_str()) {
                        // Check the char after the name is a boundary
                        let after = rest.get(name.len()..name.len() + 1);
                        if after.is_none()
                            || after.is_some_and(|c| {
                                c.starts_with(|c: char| {
                                    c.is_whitespace() || c.is_ascii_punctuation()
                                })
                            })
                        {
                            matched = Some((name.len(), *uuid));
                            break;
                        }
                    }
                }

                if let Some((name_len, uuid)) = matched {
                    let start = char_pos;
                    result.push('\u{FFFC}');
                    char_pos += 1;
                    body_ranges.push(BodyRange {
                        start,
                        end: char_pos,
                        value: AssociatedValue::MentionUuid(uuid),
                    });
                    // Skip past the matched name in the input
                    for _ in 0..name_len {
                        chars.next();
                    }
                } else {
                    result.push(ch);
                    char_pos += 1;
                }
            } else {
                result.push(ch);
                char_pos += 1;
            }
        }

        (result, body_ranges)
    }

    pub fn event_to_command<'r>(&'r self, event: &KeyEvent) -> Option<&'r Command> {
        let mut combiner = Combiner::default();
        let keys_pressed = combiner.transform(*event)?;
        let modes = if self.is_help() {
            vec![WindowMode::Anywhere, WindowMode::Help]
        } else if self.is_select_channel_shown() {
            vec![WindowMode::Anywhere, WindowMode::ChannelModal]
        } else if self.is_multiline_input {
            vec![
                WindowMode::Anywhere,
                WindowMode::Multiline,
                WindowMode::Normal,
            ]
        } else if self.input.is_empty() {
            vec![
                WindowMode::Anywhere,
                WindowMode::MessageSelected,
                WindowMode::Normal,
            ]
        } else {
            vec![WindowMode::Anywhere, WindowMode::Normal]
        };
        for mode in modes {
            if let Some(kb) = self.mode_keybindings.get(&mode)
                && let Some(cmd) = kb.get(&keys_pressed)
            {
                return Some(cmd);
            }
        }
        if self.is_help() {
            // Swallow event
            Some(&Command::NoOp)
        } else {
            None
        }
    }

    pub(super) fn extract_attachments<Tz: TimeZone>(
        input: &str,
        at: DateTime<Tz>,
        mut get_clipboard_img: impl FnMut() -> Option<Result<ImageData<'static>, arboard::Error>>,
    ) -> (String, Vec<(AttachmentSpec, Vec<u8>)>)
    where
        Tz::Offset: std::fmt::Display,
    {
        let mut offset = 0;
        let mut clean_input = String::new();

        let attachments = ATTACHMENT_REGEX.find_iter(input).filter_map(|m| {
            let path_str = m.as_str().strip_prefix("file://")?;

            clean_input.push_str(input[offset..m.start()].trim_end());
            offset = m.end();

            Some(if path_str.starts_with("clip") {
                // clipboard
                let img = get_clipboard_img()?
                    .inspect_err(|error| error!(%error, "failed to get clipboard image"))
                    .ok()?;

                let width: u32 = img.width.try_into().ok()?;
                let height: u32 = img.height.try_into().ok()?;

                let mut bytes = Vec::new();
                let mut cursor = Cursor::new(&mut bytes);
                let encoder = PngEncoder::new(&mut cursor);

                let png: ImageBuffer<Rgba<_>, _> = ImageBuffer::from_raw(width, height, img.bytes)?;
                let data: Vec<_> = png.into_raw().iter().map(|b| b.swap_bytes()).collect();
                encoder
                    .write_image(
                        &data,
                        img.width as _,
                        img.height as _,
                        image::ExtendedColorType::Rgba8,
                    )
                    .inspect_err(|error| error!(%error, "failed to encode image"))
                    .ok()?;

                let file_name = format!("screenshot-{}.png", at.format("%Y-%m-%dT%H:%M:%S%z"));

                let spec = AttachmentSpec {
                    content_type: "image/png".to_owned(),
                    length: bytes.len(),
                    file_name: Some(file_name),
                    width: Some(width),
                    height: Some(height),
                    ..Default::default()
                };
                (spec, bytes)
            } else {
                // path

                // TODO: Show error to user if the file does not exist. This would prevent not
                // sending the attachment in the end.

                let path = Path::new(path_str);
                let bytes = std::fs::read(path).ok()?;
                let content_type = mime_guess::from_path(path)
                    .first()
                    .map(|mime| mime.essence_str().to_string())
                    .unwrap_or_default();
                let file_name = path.file_name().map(|f| f.to_string_lossy().into());
                let spec = AttachmentSpec {
                    content_type,
                    length: bytes.len(),
                    file_name,
                    ..Default::default()
                };

                (spec, bytes)
            })
        });

        let attachments = attachments.collect();
        clean_input.push_str(&input[offset..]);
        let clean_input = clean_input.trim().to_string();

        (clean_input, attachments)
    }
}
