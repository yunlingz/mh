//! Retained command-line surface and TTY mode selection.

use std::io::IsTerminal;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "mh", version, about = "Minimal native coding harness")]
pub struct Args {
    /// Initialize ~/.mh/config with editable files and authoritative schemas.
    #[arg(
        long,
        conflicts_with_all = ["print", "continue_session", "session", "resume", "model", "prompt"]
    )]
    pub init: bool,

    /// Run without the fullscreen interface.
    #[arg(short = 'p', long)]
    pub print: bool,

    /// Continue the newest non-empty session for this workspace.
    #[arg(short = 'c', long = "continue", conflicts_with_all = ["session", "resume"])]
    pub continue_session: bool,

    /// Resume an exact session UUID from this workspace.
    #[arg(long, value_name = "UUID")]
    pub session: Option<String>,

    /// Pick a workspace session, or resume an exact UUID.
    #[arg(short = 'r', long, num_args = 0..=1, value_name = "UUID", conflicts_with = "session")]
    pub resume: Option<Option<String>>,

    /// Select a configured provider/model alias and its fixed reasoning effort.
    #[arg(long, value_name = "PROVIDER/ALIAS")]
    pub model: Option<String>,

    /// Initial prompt. Multiple shell words are joined with one space.
    #[arg(value_name = "PROMPT")]
    pub prompt: Vec<String>,
}

impl Args {
    pub fn interactive(&self) -> bool {
        !self.print && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }

    pub fn positional_prompt(&self) -> String {
        self.prompt.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::Args;

    #[test]
    fn metadata_and_retained_arguments_parse_without_provider_secrets() {
        let args = Args::try_parse_from([
            "mh",
            "--print",
            "--session",
            "550e8400-e29b-41d4-a716-446655440000",
            "--model",
            "work/model",
            "hello",
            "world",
        ])
        .unwrap();
        assert!(args.print);
        assert_eq!(args.positional_prompt(), "hello world");
        assert!(Args::try_parse_from(["mh", "--continue", "--session", "bad"]).is_err());
        assert!(Args::try_parse_from(["mh", "--api-key", "secret"]).is_err());
        assert!(Args::try_parse_from(["mh", "--thinking", "high"]).is_err());
        assert!(Args::try_parse_from(["mh", "--init"]).unwrap().init);
        assert!(Args::try_parse_from(["mh", "--init", "prompt"]).is_err());
        assert!(Args::try_parse_from(["mh", "--init", "--print"]).is_err());
        assert_eq!(
            Args::try_parse_from(["mh", "--resume"]).unwrap().resume,
            Some(None)
        );
        assert_eq!(
            Args::try_parse_from(["mh", "-r", "550e8400-e29b-41d4-a716-446655440000"])
                .unwrap()
                .resume,
            Some(Some("550e8400-e29b-41d4-a716-446655440000".into()))
        );
        assert!(Args::try_parse_from(["mh", "--continue", "--resume"]).is_err());
        assert!(Args::try_parse_from(["mh", "--init", "--resume"]).is_err());
    }
}
