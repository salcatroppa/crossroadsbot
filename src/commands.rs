use dashmap::DashSet;
use serenity::{
    collector::message_collector::*,
    framework::standard::{
        help_commands,
        macros::{check, group, help},
        Args, CommandOptions, CommandResult, HelpOptions, Reason, CommandGroup
    },
    model::prelude::*,
    prelude::*,
};
use std::{collections::HashSet, error::Error, fmt, sync::Arc, time::Duration};

// --- Defaults ---
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60 * 3);
pub const CHECK_EMOJI: char = '✅';
pub const CROSS_EMOJI: char = '❌';
pub const ENVELOP_EMOJI: char = '✉';

// --- Global Config ---
pub struct ConfigValues {
    pub main_guild_id: GuildId,
    pub admin_role_id: RoleId,
    pub squadmaker_role_id: RoleId,
    pub emoji_guild_id: GuildId,
}

pub struct LogginConfig {
    pub info: Option<ChannelId>,
    pub error: Option<ChannelId>,
}

// --- Global Data ---
pub struct ConversationLock;
impl TypeMapKey for ConversationLock {
    type Value = Arc<DashSet<UserId>>;
}

pub struct ConfigValuesData;
impl TypeMapKey for ConfigValuesData {
    type Value = Arc<ConfigValues>;
}

pub struct LogginConfigData;
impl TypeMapKey for LogginConfigData {
    type Value = Arc<RwLock<LogginConfig>>;
}

// --- Conversation ---
pub struct Conversation<'a> {
    lock: Arc<DashSet<UserId>>,
    pub user: &'a User,
    pub chan: PrivateChannel,
}

#[derive(Debug)]
pub enum ConversationError {
    ConversationLocked,
    NoDmChannel,
}

impl fmt::Display for ConversationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConversationError::ConversationLocked => write!(f, "User already in Conversation"),
            ConversationError::NoDmChannel => write!(f, "Unable to get DM channel for user"),
        }
    }
}

impl Error for ConversationError {}

impl<'a> Conversation<'a> {
    pub async fn start(
        ctx: &'a Context,
        user: &'a User,
    ) -> Result<Conversation<'a>, ConversationError> {
        let lock = {
            let data_read = ctx.data.read().await;
            data_read.get::<ConversationLock>().unwrap().clone()
        };

        if lock.insert(user.id) {
            // Check if we can open a dm channel
            if let Ok(chan) = user.create_dm_channel(ctx).await {
                return Ok(Conversation {
                    lock: lock,
                    user: user,
                    chan: chan,
                });
            } else {
                // no private channel. Unlock again
                lock.remove(&user.id);
                return Err(ConversationError::NoDmChannel);
            }
        }

        Err(ConversationError::ConversationLocked)
    }

    // Consumes the conversation
    pub async fn timeout_msg(self, ctx: &Context) -> serenity::Result<Message> {
        self.chan
            .send_message(&ctx.http, |m| m.content("Conversation timed out"))
            .await
    }

    // Consumes the conversation
    pub async fn abort(
        self,
        ctx: &Context,
        msg: Option<&str>,
    ) -> serenity::Result<Option<Message>> {
        if let Some(msg) = msg {
            let msg = self.chan.say(ctx, msg).await?;
            return Ok(Some(msg));
        }
        Ok(None)
    }

    pub async fn await_reply(&self, ctx: &Context) -> Option<Arc<Message>> {
        self.user
            .await_reply(ctx)
            .channel_id(self.chan.id)
            .timeout(DEFAULT_TIMEOUT)
            .await
    }

    pub async fn await_replies(&self, ctx: &Context) -> MessageCollector {
        self.user
            .await_replies(ctx)
            .channel_id(self.chan.id)
            .timeout(DEFAULT_TIMEOUT)
            .await
    }
}

impl<'a> Drop for Conversation<'a> {
    fn drop(&mut self) {
        self.lock.remove(&self.user.id);
    }
}

// --- Checks ---
#[check]
#[name = "admin_role"]
async fn admin_rol_check(
    ctx: &Context,
    msg: &Message,
    _: &mut Args,
    _: &CommandOptions,
) -> Result<(), Reason> {
    let (g, r) = {
        let config = ctx
            .data
            .read()
            .await
            .get::<ConfigValuesData>()
            .unwrap()
            .clone();
        (config.main_guild_id, config.admin_role_id)
    };

    match msg.author.has_role(ctx, g, r).await {
        Ok(b) => match b {
            true => Ok(()),
            false => Err(Reason::Log(String::from("No permissions"))),
        },
        Err(_) => Err(Reason::Unknown),
    }
}

#[help]
#[individual_command_tip = "Hello!\n\n\
If you want more information about a specific command, just pass the command as argument."]
#[command_not_found_text = "Could not find: `{}`."]
#[max_levenshtein_distance(3)]
#[indention_prefix = "+"]
#[lacking_conditions = "hide"]
#[strikethrough_commands_tip_in_guild = ""]
#[strikethrough_commands_tip_in_dm = ""]
async fn help_cmd(
    context: &Context,
    msg: &Message,
    args: Args,
    help_options: &'static HelpOptions,
    groups: &[&'static CommandGroup],
    owners: HashSet<UserId>,
) -> CommandResult {
    let _ = help_commands::with_embeds(context, msg, args, help_options, groups, owners).await;
    Ok(())
}

// --- Command Setup ---
mod misc;
use misc::*;
#[group]
#[commands(ping, dudu)]
struct Misc;

mod signup;
use signup::*;
#[group]
#[commands(register)]
struct Signup;

mod config;
use config::*;
#[group]
#[only_in(guilds)]
#[commands(
    set_log_info,
    set_log_error,
    training
)]
struct Config;

mod role;
pub use role::ROLE_GROUP as ROLE_GROUP;
