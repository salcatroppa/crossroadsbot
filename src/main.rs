use crossroadsbot::{commands, data::*, db, signup_board::*, utils::DIZZY_EMOJI};
use dashmap::DashSet;
use diesel::pg::PgConnection;
use diesel::prelude::*;
use dotenv::dotenv;
use serenity::{
    async_trait,
    client::{Client, EventHandler},
    framework::standard::{macros::hook, DispatchError, StandardFramework},
    model::prelude::*,
    prelude::*,
};
use std::{env, str::FromStr, sync::Arc};
use tracing::{error, info};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

#[macro_use]
extern crate diesel_migrations;
use diesel_migrations::embed_migrations;
embed_migrations!("migrations/");

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        info!("Connected as {}", ready.user.name);
        info!("Refreshing config values");

        let log_channel = db::Config::load(&ctx, String::from(INFO_LOG_NAME))
            .await
            .ok();

        let data_read = ctx.data.read().await;
        let mut log_write = data_read.get::<LogConfigData>().unwrap().write().await;

        match log_channel {
            None => info!("Log channel not found in db. skipped"),
            Some(info) => match ChannelId::from_str(&info.value) {
                Err(e) => error!("Failed to parse log channel id: {}", e),
                Ok(id) => log_write.log = Some(id),
            },
        }

        let signup_board_category = db::Config::load(&ctx, String::from(SIGNUP_BOARD_NAME))
            .await
            .ok();

        if signup_board_category.is_none() {
            info!("Signup board category not found in db");
        } else {
            info!("Resetting signup board");
            ctx.data
                .read()
                .await
                .get::<SignupBoardData>()
                .unwrap()
                .clone()
                .reset(&ctx)
                .await
                .ok();
        }
    }

    async fn resume(&self, _: Context, _: ResumedEvent) {
        info!("Resumed");
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        // we only care about message interactions for now
        let interaction = match interaction.message_component() {
            Some(i) => i,
            None => return,
        };

        let board = {
            ctx.data
                .read()
                .await
                .get::<SignupBoardData>()
                .unwrap()
                .clone()
        };

        board.interaction(&ctx, &interaction).await;
    }
}

#[hook]
async fn dispatch_error_hook(ctx: &Context, msg: &Message, error: DispatchError) {
    match error {
        DispatchError::NotEnoughArguments { min, given } => {
            let s = format!("Need {} arguments, but only got {}.", min, given);
            msg.reply(ctx, &s).await.ok();
        }
        DispatchError::TooManyArguments { max, given } => {
            let s = format!("Max arguments allowed is {}, but got {}.", max, given);
            msg.reply(ctx, &s).await.ok();
        }
        DispatchError::CheckFailed(..) => {
            let s = format!("No permissions to use this command");
            msg.reply(ctx, &s).await.ok();
        }
        _ => {
            msg.react(ctx, DIZZY_EMOJI).await.ok();
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    // Load .env into ENV
    dotenv().ok();

    // Set up logging
    let subscriber = FmtSubscriber::builder()
        .with_env_filter(EnvFilter::from_default_env())
        .finish();

    tracing::subscriber::set_global_default(subscriber).expect("Failed to start the logger");

    // Run migrations on the database
    {
        let database_url = env::var("DATABASE_URL").expect("DATABASE_URL not set");
        let conn = PgConnection::establish(&database_url)
            .expect(&format!("Error connecting to {}", database_url));
        embedded_migrations::run(&conn).expect("Failed to run migrations");
    }

    let token = env::var("DISCORD_TOKEN").expect("discord token not set");
    let app_id = env::var("APPLICATION_ID")
        .expect("application id not set")
        .parse::<u64>()
        .expect("Failed to parse application id");

    let main_guild_id = GuildId::from(
        env::var("MAIN_GUILD_ID")
            .expect("MAIN_GUILD_ID not set")
            .parse::<u64>()
            .expect("Failed to parse manager guild id"),
    );

    let emoji_guild_id = GuildId::from(
        env::var("EMOJI_GUILD_ID")
            .expect("EMOJI_GUILD_ID not set")
            .parse::<u64>()
            .expect("Failed to parse emoji guild id"),
    );

    let admin_role_id = RoleId::from(
        env::var("ADMIN_ROLE_ID")
            .expect("ADMIN_ROLE_ID not set")
            .parse::<u64>()
            .expect("Failed to parse admin role id"),
    );

    let squadmaker_role_id = RoleId::from(
        env::var("SQUADMAKER_ROLE_ID")
            .expect("SQUADMAKER_ROLE_ID not set")
            .parse::<u64>()
            .expect("Failed to parse squadmaker role id"),
    );

    let framework = StandardFramework::new()
        .configure(|c| {
            c.prefix(GLOB_COMMAND_PREFIX);
            c.no_dm_prefix(true)
        })
        .on_dispatch_error(dispatch_error_hook)
        .help(&commands::HELP_CMD)
        .group(&commands::SIGNUP_GROUP)
        .group(&commands::TRAINING_GROUP)
        .group(&commands::ROLE_GROUP)
        .group(&commands::TIER_GROUP)
        .group(&commands::CONFIG_GROUP)
        .group(&commands::MISC_GROUP);

    let mut client = Client::builder(token)
        .application_id(app_id)
        .framework(framework)
        .event_handler(Handler)
        .await
        .expect("Error creating client");

    {
        let mut data = client.data.write().await;
        data.insert::<ConversationLock>(Arc::new(DashSet::new()));
        data.insert::<ConfigValuesData>(Arc::new(ConfigValues {
            main_guild_id,
            admin_role_id,
            squadmaker_role_id,
            emoji_guild_id,
        }));
        data.insert::<LogConfigData>(Arc::new(RwLock::new(LogConfig { log: None })));
        data.insert::<SignupBoardData>(Arc::new(SignupBoard::new()));
        data.insert::<DBPoolData>(Arc::new(db::DBPool::new()));
    }

    let shard_manager = client.shard_manager.clone();

    tokio::spawn(async move {
        tokio::signal::ctrl_c()
            .await
            .expect("Could not register ctrl+c handler");
        shard_manager.lock().await.shutdown_all().await;
    });

    if let Err(why) = client.start().await {
        println!("An error occurred while running the client: {:?}", why);
    }
}
