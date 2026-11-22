use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;

use clap::Parser;
use mh::cli::Args;
use mh::runtime::{Harness, Options, SessionSelection};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("mh: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<u8, String> {
    let args = Args::parse();
    if args.init {
        let layout = mh::storage::Layout::user()?;
        let report = mh::setup::initialize(&layout)?;
        println!("Initialized {}", layout.config_dir().display());
        println!(
            "config.json: {}",
            if report.config_created {
                "created"
            } else {
                "preserved"
            }
        );
        println!(
            "theme.json: {}",
            if report.theme_created {
                "created"
            } else {
                "preserved"
            }
        );
        println!("Schema files: refreshed");
        return Ok(0);
    }
    let interactive = args.interactive();
    if args.resume == Some(None) && !interactive {
        return Err("Session picker requires a terminal; use --resume UUID in print mode".into());
    }
    let mut piped = String::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin()
            .take(32 * 1024 * 1024 + 1)
            .read_to_string(&mut piped)
            .map_err(|_| "Cannot read standard input".to_owned())?;
        if piped.len() > 32 * 1024 * 1024 {
            return Err("Standard input exceeds the 32 MiB limit".into());
        }
    }
    let positional = args.positional_prompt();
    let mut prompt = piped;
    if !prompt.is_empty() && !positional.is_empty() && !prompt.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str(&positional);
    if !interactive && prompt.trim().is_empty() {
        return Err("Print mode requires a prompt or piped standard input".into());
    }
    let cwd = std::env::current_dir().map_err(|_| "Cannot access the current directory")?;
    let prepared = Harness::prepare(&cwd, mh::storage::Layout::user()?)?;
    let selection = if let Some(id) = args.session {
        SessionSelection::Exact(id)
    } else if let Some(resume) = args.resume {
        let id = match resume {
            Some(id) => id,
            None => match mh::ui::app::choose_session(&prepared).await? {
                Some(id) => id,
                None => return Ok(0),
            },
        };
        SessionSelection::Exact(id)
    } else if args.continue_session {
        SessionSelection::Continue
    } else {
        SessionSelection::New
    };
    if interactive {
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(256);
        let (tool_tx, tool_rx) = tokio::sync::mpsc::channel(256);
        let harness = Harness::load_prepared(
            prepared,
            Options {
                selection,
                model: args.model,
                interactive: true,
                events: Some(event_tx.clone()),
                tool_events: Some(tool_tx.clone()),
            },
        )
        .await?;
        let exit = mh::ui::app::run(
            harness,
            event_tx,
            tool_tx,
            event_rx,
            tool_rx,
            (!prompt.trim().is_empty()).then_some(prompt),
        )
        .await?;
        if exit.has_conversation {
            writeln!(
                std::io::stdout().lock(),
                "\nResume this session with:\nmh --resume {}",
                exit.session_id
            )
            .map_err(|_| "Cannot write resume hint")?;
        }
        Ok(0)
    } else {
        let harness = Harness::load_prepared(
            prepared,
            Options {
                selection,
                model: args.model,
                interactive: false,
                events: None,
                tool_events: None,
            },
        )
        .await?;
        if harness
            .agent
            .resources()
            .warnings
            .iter()
            .any(|warning| warning == mh::session::MODEL_CHOICE_MIGRATION_NOTICE)
        {
            let _ = writeln!(
                std::io::stderr().lock(),
                "{}",
                mh::session::MODEL_CHOICE_MIGRATION_NOTICE
            );
        }
        mh::print::run(harness.agent, prompt).await
    }
}
