extern crate notify;

//use crate::importer::FirefoxImporter;
use entities::models::crawl_queue::{self, remove_by_rule};
use entities::models::indexed_document;
use entities::regex::{regex_for_robots, WildcardType};
use entities::sea_orm::DatabaseConnection;
use libspyglass::search::Searcher;
use libspyglass::state::AppState;
use libspyglass::task::{self, AppShutdown};
use migration::{Migrator, MigratorTrait};
use shared::config::{Config, UserSettings};
use std::io;
use std::time::Duration;
use tokio::signal;
use tokio::sync::{broadcast, mpsc};
use tracing_log::LogTracer;
use tracing_subscriber::{fmt, layer::SubscriberExt, EnvFilter};

mod api;
mod importer;

use crate::api::start_api_ipc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::new();

    let file_appender = tracing_appender::rolling::daily(config.logs_dir(), "server.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    let subscriber = tracing_subscriber::registry()
        .with(
            EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into())
                .add_directive("tantivy=WARN".parse().unwrap()),
        )
        .with(fmt::Layer::new().with_writer(io::stdout))
        .with(fmt::Layer::new().with_ansi(false).with_writer(non_blocking));

    tracing::subscriber::set_global_default(subscriber).expect("Unable to set a global subscriber");
    LogTracer::init()?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("spyglass-backend")
        .build()
        .unwrap();

    // Initialize/Load user preferences
    let state = rt.block_on(AppState::new(&config));

    // Run any migrations
    match rt.block_on(Migrator::up(&state.db, None)) {
        Ok(_) => {}
        Err(e) => {
            // Ruh-oh something went wrong
            log::error!("Unable to migrate database - {:?}", e);
            // Exit from app
            return Ok(());
        }
    }

    // Start IPC server
    let server = start_api_ipc(&state).expect("Unable to start IPC server");
    rt.block_on(start_backend(&state, &config));
    server.close();

    Ok(())
}

async fn start_backend(state: &AppState, config: &Config) {
    // TODO: Implement user-friendly start-up wizard
    // if state.config.user_settings.run_wizard {
    //     // Import data from Firefox
    //     // TODO: Ask user what browser/profiles to import on first startup.
    //     let importer = FirefoxImporter::new(&state.config);
    //     let _ = importer.import(&state).await;
    // }

    clear_settings_blocked(&state.db, &state.index, &config.user_settings).await;
    // Initialize crawl_queue, set all in-flight tasks to queued.
    crawl_queue::reset_processing(&state.db).await;

    // Create channels for scheduler / crawlers
    let (crawl_queue_tx, crawl_queue_rx) = mpsc::channel(
        state
            .user_settings
            .inflight_crawl_limit
            .value()
            .try_into()
            .unwrap(),
    );
    let (shutdown_tx, _) = broadcast::channel::<AppShutdown>(16);

    // Check lenses for updates & add any bootstrapped URLs to crawler.
    let lens_watcher_handle = tokio::spawn(task::lens_watcher(
        state.clone(),
        config.clone(),
        shutdown_tx.subscribe(),
    ));

    // Crawl scheduler
    let manager_handle = tokio::spawn(task::manager_task(
        state.clone(),
        crawl_queue_tx,
        shutdown_tx.subscribe(),
    ));

    // Crawlers
    let worker_handle = tokio::spawn(task::worker_task(
        state.clone(),
        crawl_queue_rx,
        shutdown_tx.subscribe(),
    ));

    // Clean up crew. Commit anything added to the index in the last 10s
    {
        let state = state.clone();
        let _ = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));

            loop {
                interval.tick().await;
                if let Err(err) = state.index.writer.lock().unwrap().commit() {
                    log::error!("{:?}", err);
                }
            }
        });
    }

    // Gracefully handle shutdowns
    match signal::ctrl_c().await {
        Ok(()) => {
            lens_watcher_handle.abort();
            log::warn!("Shutdown request received");
            shutdown_tx.send(AppShutdown::Now).unwrap();
        }
        Err(err) => {
            log::error!("Unable to listen for shutdown signal: {}", err);
            shutdown_tx.send(AppShutdown::Now).unwrap();
        }
    }

    let _ = tokio::join!(manager_handle, worker_handle);
}

pub async fn clear_settings_blocked(
    db: &DatabaseConnection,
    index: &Searcher,
    settings: &UserSettings,
) {
    for domain in settings.block_list.iter() {
        let domain = if domain.starts_with("*.") {
            domain.to_string()
        } else {
            format!("*{domain}")
        };
        if let Some(rule_like) = regex_for_robots(&domain, WildcardType::Database) {
            // Remove matching crawl tasks
            let _ = remove_by_rule(&db, &rule_like).await;
            // Remove matching indexed documents
            match indexed_document::remove_by_rule(&db, &rule_like).await {
                Ok(doc_ids) => {
                    if let Ok(mut writer) = index.writer.lock() {
                        for doc_id in doc_ids {
                            let res = Searcher::delete(&mut writer, &doc_id);
                            if let Err(err) = res {
                                log::error!("Unable to remove docs: {:?}", err);
                            }
                        }
                    }
                }
                Err(e) => log::error!("Unable to remove docs: {:?}", e),
            }
        }
    }
}
