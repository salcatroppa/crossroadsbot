use crate::{data::GLOB_COMMAND_PREFIX, data::*, db, embeds, log::LogResult, utils::*};
use dashmap::DashSet;
use serenity::{
    client::bridge::gateway::ShardMessenger,
    collector::{message_collector::*, reaction_collector::*},
    futures::future,
    model::prelude::*,
    prelude::*,
};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    sync::Arc,
};

type ConvResult = std::result::Result<Conversation, ConversationError>;

pub struct Conversation {
    lock: Arc<DashSet<UserId>>,
    pub user: User,
    pub chan: PrivateChannel,
    pub msg: Message,
}

#[derive(Debug)]
pub enum ConversationError {
    ConversationLocked,
    NoDmChannel,
    DmBlocked,
    TimedOut,
    Canceled,
    Other(String),
}

impl fmt::Display for ConversationError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ConversationError::ConversationLocked => {
                write!(f, "Already in another DM conversation")
            }
            ConversationError::NoDmChannel => write!(f, "Unable to load DM channel"),
            ConversationError::DmBlocked => {
                write!(f, "Unable to send message in DM channel")
            }
            ConversationError::TimedOut => {
                write!(f, "Conversation timed out")
            }
            ConversationError::Canceled => {
                write!(f, "Conversation canceled")
            }
            ConversationError::Other(s) => {
                write!(f, "{}", s)
            }
        }
    }
}

impl ConversationError {
    pub fn is_init_err(&self) -> bool {
        match self {
            ConversationError::DmBlocked
            | ConversationError::NoDmChannel
            | ConversationError::ConversationLocked => true,
            _ => false,
        }
    }
}

impl Error for ConversationError {}

impl Conversation {
    pub async fn start(ctx: &Context, user: &User) -> ConvResult {
        let lock = {
            let data_read = ctx.data.read().await;
            data_read.get::<ConversationLock>().unwrap().clone()
        };

        if !lock.insert(user.id) {
            return Err(ConversationError::ConversationLocked);
        }

        // Check if we can open a dm channel
        let chan = match user.create_dm_channel(ctx).await {
            Ok(c) => c,
            Err(_) => {
                lock.remove(&user.id);
                return Err(ConversationError::NoDmChannel);
            }
        };

        // Send initial message to channel
        let msg = match chan.send_message(ctx, |m| m.content("Loading ...")).await {
            Ok(m) => m,
            Err(_) => {
                lock.remove(&user.id);
                return Err(ConversationError::DmBlocked);
            }
        };

        Ok(Conversation {
            lock,
            user: user.clone(),
            chan,
            msg,
        })
    }

    // Consumes the conversation
    pub async fn timeout_msg(self, ctx: &Context) -> serenity::Result<Message> {
        self.chan
            .send_message(&ctx.http, |m| m.content("Conversation timed out"))
            .await
    }

    // Consumes the conversation
    pub async fn canceled_msg(self, ctx: &Context) -> serenity::Result<Message> {
        self.chan
            .send_message(&ctx.http, |m| m.content("Conversation got canceled"))
            .await
    }

    pub async fn unexpected_error(self, ctx: &Context) -> serenity::Result<Message> {
        self.msg
            .reply(&ctx.http, "Unexpected error, Sorry =(")
            .await
    }

    pub async fn finish_with_msg(
        self,
        ctx: &Context,
        content: impl fmt::Display,
    ) -> serenity::Result<()> {
        self.chan.say(ctx, content).await?;
        return Ok(());
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

    /// Awaits a reaction on the conversation message. Returns the Collector
    /// to further modify it. eg with a filter
    pub fn await_reaction<'a>(
        &self,
        shard_messenger: &'a impl AsRef<ShardMessenger>,
    ) -> CollectReaction<'a> {
        self.msg
            .await_reaction(shard_messenger)
            .author_id(self.user.id)
            .timeout(DEFAULT_TIMEOUT)
    }

    /// Same as await_reaction but returns a Stream
    pub fn await_reactions<'a>(
        &self,
        shard_messenger: &'a impl AsRef<ShardMessenger>,
    ) -> ReactionCollectorBuilder<'a> {
        self.msg
            .await_reactions(shard_messenger)
            .author_id(self.user.id)
            .timeout(DEFAULT_TIMEOUT)
    }
}

impl Drop for Conversation {
    fn drop(&mut self) {
        self.lock.remove(&self.user.id);
    }
}

static NOT_REGISTERED: &str = "User not registered";
static NOT_OPEN: &str = "Training not found or not open";
static NOT_SIGNED_UP: &str = "Not signup found for user";

pub async fn join_training(ctx: &Context, user: &User, training_id: i32) -> LogResult {
    let mut conv = Conversation::start(ctx, user).await?;

    let db_user = match db::User::by_discord_id(ctx, user.id).await {
        Ok(u) => u,
        Err(diesel::NotFound) => {
            let emb = embeds::not_registered_embed();
            conv.msg
                .edit(ctx, |m| {
                    m.content("");
                    m.embed(|e| {
                        e.0 = emb.0;
                        e
                    })
                })
                .await?;
            return Ok(NOT_REGISTERED.into());
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    // Get training with id
    let training = match db::Training::by_id_and_state(training_id, db::TrainingState::Open).await {
        Ok(t) => Arc::new(t),
        Err(diesel::NotFound) => {
            conv.msg
                .edit(ctx, |m| {
                    m.content(format!(
                        "No **open** training found with id {}",
                        training_id
                    ))
                })
                .await?;
            return Ok(NOT_OPEN.into());
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    // verify if tier requirements pass
    match verify_tier(ctx, &training, &conv.user).await {
        Ok((pass, tier)) => {
            if !pass {
                conv.msg
                    .edit(ctx, |m| {
                        m.content("");
                        m.embed(|e| {
                            e.description("Tier requirement not fulfilled");
                            e.field("Missing tier:", tier, false)
                        })
                    })
                    .await?;
                return Ok("Tier requirement not fulfilled".into());
            }
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    // Check if signup already exist
    match db::Signup::by_user_and_training(&db_user, &training).await {
        Ok(_) => {
            conv.msg
                .edit(ctx, |m| {
                    m.content("");
                    m.embed(|e| {
                        e.description("Already signed up for this training");
                        e.field(
                            "You can edit your signup with:",
                            format!("`{}edit {}`", GLOB_COMMAND_PREFIX, training.id),
                            false,
                        );
                        e.field(
                            "You can remove your signup with:",
                            format!("`{}leave {}`", GLOB_COMMAND_PREFIX, training.id),
                            false,
                        )
                    })
                })
                .await?;
            return Ok("Already signed up".into());
        }
        Err(diesel::NotFound) => (), // This is what we want
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    let new_signup = db::NewSignup {
        training_id: training.id,
        user_id: db_user.id,
    };

    // register new signup
    let signup = match new_signup.add().await {
        Ok(s) => s,
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    conv.msg
        .edit(ctx, |m| {
            m.content(format!(
                "You signed up for **{}**. Please select your roles:",
                training.title
            ))
        })
        .await?;

    // training role mapping
    let training_roles = training.clone().get_training_roles().await?;
    // The actual roles. ignoring deactivated ones (or db load errors in general)
    let roles: Vec<db::Role> = future::join_all(training_roles.iter().map(|tr| tr.role()))
        .await
        .into_iter()
        .filter_map(|r| r.ok())
        .collect();

    // Create sets for selected and unselected
    let selected: HashSet<&db::Role> = HashSet::with_capacity(roles.len());
    let mut unselected: HashSet<&db::Role> = HashSet::with_capacity(roles.len());
    for r in &roles {
        unselected.insert(r);
    }

    let selected = match select_roles(ctx, &mut conv, selected, unselected).await {
        Ok((selected, _)) => selected,
        Err(e) => {
            if let Some(e) = e.downcast_ref::<ConversationError>() {
                match e {
                    ConversationError::TimedOut => {
                        conv.timeout_msg(ctx).await?;
                        return Ok("Timed out".into());
                    }
                    ConversationError::Canceled => {
                        conv.canceled_msg(ctx).await?;
                        return Ok("Canceled".into());
                    }
                    _ => (),
                }
            }
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    // Save roles
    conv.msg.edit(ctx, |m| m.content("Saving roles...")).await?;
    let futs = selected.iter().map(|r| {
        let new_signup_role = db::NewSignupRole {
            role_id: r.id,
            signup_id: signup.id,
        };
        new_signup_role.add()
    });
    match future::try_join_all(futs).await {
        Ok(r) => {
            conv.msg
                .edit(ctx, |m| {
                    m.content("");
                    m.embed(|e| {
                        e.description("Successfully signed up");
                        e.field(
                            training.title.clone(),
                            format!("Training id: {}", training.id),
                            true,
                        );
                        e.field(
                            "Roles",
                            format!("{} role(s) added to your sign up", r.len()),
                            true,
                        );
                        e
                    })
                })
                .await?;
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    }
    Ok("Success".into())
}

pub async fn edit_signup(ctx: &Context, user: &User, training_id: i32) -> LogResult {
    let mut conv = Conversation::start(ctx, user).await?;

    let db_user = match db::User::by_discord_id(ctx, user.id).await {
        Ok(u) => u,
        Err(diesel::NotFound) => {
            let emb = embeds::not_registered_embed();
            conv.msg
                .edit(ctx, |m| {
                    m.content("");
                    m.embed(|e| {
                        e.0 = emb.0;
                        e
                    })
                })
                .await?;
            return Ok(NOT_REGISTERED.into());
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    let training = match db::Training::by_id_and_state(training_id, db::TrainingState::Open).await {
        Ok(t) => Arc::new(t),
        Err(diesel::NotFound) => {
            conv.msg
                .reply(
                    ctx,
                    format!("No **open** training with id {} found", training_id),
                )
                .await?;
            return Ok(NOT_OPEN.into());
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    let signup = match db::Signup::by_user_and_training(&db_user, &training.clone()).await {
        Ok(s) => Arc::new(s),
        Err(diesel::NotFound) => {
            conv.msg
                .edit(ctx, |m| {
                    m.content("");
                    m.embed(|e| {
                        e.description(format!("{} No signup found", CROSS_EMOJI));
                        e.field(
                            "You are not signed up for training:",
                            &training.title,
                            false,
                        );
                        e.field(
                            "If you want to join this training use:",
                            format!("`{}join {}`", GLOB_COMMAND_PREFIX, training.id),
                            false,
                        )
                    })
                })
                .await?;
            return Ok(NOT_SIGNED_UP.into());
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    let training_roles = training.clone().get_training_roles().await?;
    let roles = future::try_join_all(training_roles.iter().map(|r| r.role())).await?;

    let mut selected: HashSet<&db::Role> = HashSet::new();
    let mut unselected: HashSet<&db::Role> = HashSet::new();

    match signup.clone().get_roles().await {
        Ok(v) => {
            // this seems rather inefficient. Consider rework
            let set = v.into_iter().map(|(_, r)| r).collect::<HashSet<_>>();
            for r in &roles {
                if set.contains(r) {
                    selected.insert(r);
                } else {
                    unselected.insert(r);
                }
            }
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    let selected = match select_roles(ctx, &mut conv, selected, unselected).await {
        Ok((selected, _)) => selected,
        Err(e) => {
            if let Some(e) = e.downcast_ref::<ConversationError>() {
                match e {
                    ConversationError::TimedOut => {
                        conv.timeout_msg(ctx).await?;
                        return Ok("Timed out".into());
                    }
                    ConversationError::Canceled => {
                        conv.canceled_msg(ctx).await?;
                        return Ok("Canceled".into());
                    }
                    _ => (),
                }
            }
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    if let Err(e) = signup.clone().clear_roles().await {
        conv.unexpected_error(ctx).await?;
        return Err(e.into());
    }

    match future::try_join_all(selected.iter().map(|r| {
        let new_signup_role = db::NewSignupRole {
            role_id: r.id,
            signup_id: signup.id,
        };
        new_signup_role.add()
    }))
    .await
    {
        Ok(_) => {
            conv.msg
                .edit(ctx, |m| {
                    m.content("");
                    m.embed(|e| {
                        e.description(format!("{}", CHECK_EMOJI));
                        e.field("Changed roles for training:", &training.title, false);
                        e.field(
                            "New roles:",
                            selected
                                .iter()
                                .map(|r| r.repr.clone())
                                .collect::<Vec<_>>()
                                .join(", "),
                            false,
                        )
                    })
                })
                .await?;
            return Ok("Success".into());
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    }
}

pub async fn remove_signup(ctx: &Context, user: &User, training_id: i32) -> LogResult {
    let mut conv = Conversation::start(ctx, user).await?;

    let db_user = match db::User::by_discord_id(ctx, user.id).await {
        Ok(u) => u,
        Err(diesel::NotFound) => {
            let emb = embeds::not_registered_embed();
            conv.msg
                .edit(ctx, |m| {
                    m.content("");
                    m.embed(|e| {
                        e.0 = emb.0;
                        e
                    })
                })
                .await?;
            return Ok(NOT_REGISTERED.into());
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    let training = match db::Training::by_id_and_state(training_id, db::TrainingState::Open).await {
        Ok(t) => Arc::new(t),
        Err(diesel::NotFound) => {
            conv.msg
                .reply(
                    ctx,
                    format!("No **open** training with id {} found", training_id),
                )
                .await?;
            return Ok(NOT_OPEN.into());
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    let signup = match db::Signup::by_user_and_training(&db_user, &training.clone()).await {
        Ok(s) => s,
        Err(diesel::NotFound) => {
            conv.msg
                .edit(ctx, |m| {
                    m.content("");
                    m.embed(|e| {
                        e.description(format!("{} No signup found", CROSS_EMOJI));
                        e.field(
                            "You are not signed up for training:",
                            &training.title,
                            false,
                        );
                        e.field(
                            "If you want to join this training use:",
                            format!("`{}join {}`", GLOB_COMMAND_PREFIX, training.id),
                            false,
                        )
                    })
                })
                .await?;
            return Ok(NOT_SIGNED_UP.into());
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    match signup.remove().await {
        Ok(1) => (),
        Ok(a) => {
            conv.unexpected_error(ctx).await?;
            return Err(format!("Unexpected amount of signups removed. Amount: {}", a).into());
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    }

    conv.msg
        .edit(ctx, |m| {
            m.content("");
            m.embed(|e| {
                e.description(format!("{} Signup removed", CHECK_EMOJI));
                e.field("Signup removed for training:", &training.title, false)
            })
        })
        .await?;

    Ok("Success".into())
}

pub async fn list_signup(ctx: &Context, user: &User) -> LogResult {
    let mut conv = Conversation::start(ctx, user).await?;

    let db_user = match db::User::by_discord_id(ctx, user.id).await {
        Ok(u) => u,
        Err(diesel::NotFound) => {
            let emb = embeds::not_registered_embed();
            conv.msg
                .edit(ctx, |m| {
                    m.content("");
                    m.embed(|e| {
                        e.0 = emb.0;
                        e
                    })
                })
                .await?;
            return Ok(NOT_REGISTERED.into());
        }
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    let signups = match db_user.active_signups(ctx).await {
        Ok(v) => v
            .into_iter()
            .map(|(s, t)| (Arc::new(s), Arc::new(t)))
            .collect::<Vec<_>>(),
        Err(e) => {
            conv.unexpected_error(ctx).await?;
            return Err(e.into());
        }
    };

    let mut roles: HashMap<i32, Vec<db::Role>> = HashMap::with_capacity(signups.len());
    for (s, _) in &signups {
        let signup_roles = match s.clone().get_roles().await {
            Ok(v) => v.into_iter().map(|(_, r)| r).collect::<Vec<_>>(),
            Err(e) => {
                conv.unexpected_error(ctx).await?;
                return Err(e.into());
            }
        };
        roles.insert(s.id, signup_roles);
    }

    conv.msg
        .edit(ctx, |m| {
            m.content("");
            m.embed(|e| {
                e.description("All current active signups");
                for (s, t) in signups {
                    e.field(
                        &t.title,
                        format!(
                        "`Date        :` {}\n\
                         `Time (Utc)  :` {}\n\
                         `Training Id :` {}\n\
                         `Roles       :` {}\n",
                            t.date.date(),
                            t.date.time(),
                            t.id,
                            match roles.get(&s.id) {
                                Some(r) => r
                                    .iter()
                                    .map(|r| r.repr.clone())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                None => String::from("Failed to load roles =("),
                            }
                        ),
                        true,
                    );
                }
                e.footer(|f| {
                    f.text(format!(
                        "To edit or remove your sign up reply with:\n\
                        {}edit <training id>\n\
                        {}leave <training id>",
                        GLOB_COMMAND_PREFIX,
                        GLOB_COMMAND_PREFIX
                    ))
                });
                e
            })
        })
        .await?;

    Ok("Success".into())
}
