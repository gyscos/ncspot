extern crate clap;
extern crate crossbeam_channel;
#[macro_use]
extern crate cursive;
#[cfg(feature = "share_clipboard")]
extern crate clipboard;
extern crate failure;
extern crate futures;
#[macro_use]
extern crate lazy_static;
extern crate librespot_core;
extern crate librespot_playback;
extern crate librespot_protocol;
extern crate rspotify;
extern crate tokio_core;
extern crate tokio_timer;
extern crate unicode_width;
extern crate webbrowser;

#[macro_use]
extern crate serde;
extern crate serde_json;
extern crate toml;

#[macro_use]
extern crate log;
extern crate chrono;
extern crate fern;

extern crate rand;
extern crate url;

extern crate strum;
extern crate strum_macros;

extern crate regex;

use std::fs;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use clap::{App, Arg};
use cursive::traits::Identifiable;
use std::ffi::CString;

use librespot_core::authentication::Credentials;
use librespot_core::cache::Cache;
use librespot_playback::audio_backend;

mod album;
mod artist;
mod authentication;
mod command;
mod commands;
mod config;
mod episode;
mod events;
mod library;
mod playable;
mod playlist;
mod queue;
mod sharing;
mod show;
mod spotify;
mod spotify_url;
mod spotify_worker;
mod theme;
mod track;
mod traits;
mod ui;
mod utils;

#[cfg(feature = "mpris")]
mod mpris;

use crate::command::{Command, JumpMode};
use crate::commands::CommandManager;
use crate::config::Config;
use crate::events::{Event, EventManager};
use crate::library::Library;
use crate::spotify::PlayerEvent;
use crate::ui::contextmenu::ContextMenu;

fn setup_logging(filename: &str) -> Result<(), fern::InitError> {
    fern::Dispatch::new()
        // Perform allocation-free log formatting
        .format(|out, message, record| {
            out.finish(format_args!(
                "{} [{}] [{}] {}",
                chrono::Local::now().format("[%Y-%m-%d][%H:%M:%S]"),
                record.target(),
                record.level(),
                message
            ))
        })
        // Add blanket level filter -
        .level(log::LevelFilter::Trace)
        // - and per-module overrides
        .level_for("librespot", log::LevelFilter::Debug)
        // Output to stdout, files, and other Dispatch configurations
        .chain(fern::log_file(filename)?)
        // Apply globally
        .apply()?;
    Ok(())
}

fn credentials_prompt(error_message: Option<String>) -> Credentials {
    if let Some(message) = error_message {
        let mut siv = cursive::default();
        let dialog = cursive::views::Dialog::around(cursive::views::TextView::new(format!(
            "Connection error:\n{}",
            message
        )))
        .button("Ok", |s| s.quit());
        siv.add_layer(dialog);
        siv.run();
    }

    authentication::create_credentials().expect("Could not create credentials")
}

type UserData = Arc<UserDataInner>;
struct UserDataInner {
    pub cmd: CommandManager,
}

fn main() {
    let buf = CString::new("").unwrap();
    unsafe { libc::setlocale(libc::LC_ALL, buf.as_ptr()) };

    let backends = {
        let backends: Vec<&str> = audio_backend::BACKENDS.iter().map(|b| b.0).collect();
        format!("Audio backends: {}", backends.join(", "))
    };
    let matches = App::new("ncspot")
        .version(env!("CARGO_PKG_VERSION"))
        .author("Henrik Friedrichsen <henrik@affekt.org> and contributors")
        .about("cross-platform ncurses Spotify client")
        .after_help(&*backends)
        .arg(
            Arg::with_name("debug")
                .short("d")
                .long("debug")
                .value_name("FILE")
                .help("Enable debug logging to the specified file")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("basepath")
                .short("b")
                .long("basepath")
                .value_name("PATH")
                .help("custom basepath to config/cache files")
                .takes_value(true),
        )
        .get_matches();

    if let Some(filename) = matches.value_of("debug") {
        setup_logging(filename).expect("can't setup logging");
    }

    if let Some(basepath) = matches.value_of("basepath") {
        let path = PathBuf::from_str(basepath).expect("invalid path");
        if !path.exists() {
            fs::create_dir_all(&path).expect("could not create basepath directory");
        }
        *config::BASE_PATH.write().unwrap() = Some(path);
    }

    // Things here may cause the process to abort; we must do them before creating curses windows
    // otherwise the error message will not be seen by a user
    let cfg: Arc<crate::config::Config> = Arc::new(Config::new());

    let cache = Cache::new(config::cache_path("librespot"), true);
    let mut credentials = {
        let cached_credentials = cache.credentials();
        match cached_credentials {
            Some(c) => {
                info!("Using cached credentials");
                c
            }
            None => credentials_prompt(None),
        }
    };

    while let Err(error) = spotify::Spotify::test_credentials(credentials.clone()) {
        let error_msg = match error.get_ref() {
            Some(inner) => inner.to_string(),
            None => error.to_string(),
        };
        credentials = credentials_prompt(Some(error_msg));
    }

    let mut cursive = cursive::default().into_runner();
    let theme = cfg.build_theme();
    cursive.set_theme(theme.clone());

    let event_manager = EventManager::new(cursive.cb_sink().clone());

    let spotify = spotify::Spotify::new(event_manager.clone(), credentials, cfg.clone());

    let queue = Arc::new(queue::Queue::new(spotify.clone(), cfg.clone()));

    #[cfg(feature = "mpris")]
    let mpris_manager = Arc::new(mpris::MprisManager::new(
        event_manager.clone(),
        spotify.clone(),
        queue.clone(),
    ));

    let library = Arc::new(Library::new(&event_manager, spotify.clone(), cfg.clone()));

    let mut cmd_manager = CommandManager::new(
        spotify.clone(),
        queue.clone(),
        library.clone(),
        cfg.clone(),
        event_manager.clone(),
    );

    cmd_manager.register_all();
    cmd_manager.register_keybindings(&mut cursive);

    let user_data: UserData = Arc::new(UserDataInner { cmd: cmd_manager });
    cursive.set_user_data(user_data);

    let search = ui::search::SearchView::new(event_manager.clone(), queue.clone(), library.clone());

    let libraryview = ui::library::LibraryView::new(queue.clone(), library.clone());

    let queueview = ui::queue::QueueView::new(queue.clone(), library.clone());

    #[cfg(feature = "cover")]
    let coverview = ui::cover::CoverView::new(queue.clone(), library.clone(), &cfg);

    let status = ui::statusbar::StatusBar::new(
        queue.clone(),
        library,
        cfg.values().use_nerdfont.unwrap_or(false),
    );

    let mut layout = ui::layout::Layout::new(status, &event_manager, theme)
        .screen("search", search.with_name("search"))
        .screen("library", libraryview.with_name("library"))
        .screen("queue", queueview);

    #[cfg(feature = "cover")]
    layout.add_screen("cover", coverview.with_name("cover"));

    // initial screen is library
    layout.set_screen("library");

    cursive.add_global_callback(':', move |s| {
        if s.find_name::<ContextMenu>("contextmenu").is_none() {
            s.call_on_name("main", |v: &mut ui::layout::Layout| {
                v.enable_cmdline();
            });
        }
    });

    cursive.add_global_callback('/', move |s| {
        if s.find_name::<ContextMenu>("contextmenu").is_none() {
            s.call_on_name("main", |v: &mut ui::layout::Layout| {
                v.enable_jump();
            });
        }
    });

    cursive.add_global_callback(cursive::event::Key::Esc, move |s| {
        if s.find_name::<ContextMenu>("contextmenu").is_none() {
            s.call_on_name("main", |v: &mut ui::layout::Layout| {
                v.clear_cmdline();
            });
        }
    });

    layout.cmdline.set_on_edit(move |s, cmd, _| {
        s.call_on_name("main", |v: &mut ui::layout::Layout| {
            if cmd.is_empty() {
                v.clear_cmdline();
            }
        });
    });

    {
        let ev = event_manager.clone();
        layout.cmdline.set_on_submit(move |s, cmd| {
            {
                let mut main = s.find_name::<ui::layout::Layout>("main").unwrap();
                main.clear_cmdline();
            }
            if cmd.strip_prefix('/').is_some() {
                let query = &cmd[1..];
                let command = Command::Jump(JumpMode::Query(query.to_string()));
                if let Some(data) = s.user_data::<UserData>().cloned() {
                    data.cmd.handle(s, command);
                }
            } else {
                let c = &cmd[1..];
                let parsed = command::parse(c);
                if let Some(parsed) = parsed {
                    if let Some(data) = s.user_data::<UserData>().cloned() {
                        data.cmd.handle(s, parsed)
                    }
                }
            }
            ev.trigger();
        });
    }

    cursive.add_fullscreen_layer(layout.with_name("main"));

    // cursive event loop
    while cursive.is_running() {
        cursive.step();
        for event in event_manager.msg_iter() {
            match event {
                Event::Player(state) => {
                    trace!("event received: {:?}", state);
                    spotify.update_status(state.clone());

                    #[cfg(feature = "mpris")]
                    mpris_manager.update();

                    if state == PlayerEvent::FinishedTrack {
                        queue.next(false);
                    }
                }
                Event::SessionDied => spotify.start_worker(None),
            }
        }
    }
}
