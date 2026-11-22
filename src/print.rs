//! Non-interactive presenter. Standard output contains only the final answer.

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, RunStatus};

pub async fn run(agent: Arc<Agent>, prompt: String) -> Result<u8, String> {
    let cancel = CancellationToken::new();
    let request = agent.run(prompt, &cancel);
    tokio::pin!(request);
    let outcome = tokio::select! {
        result = &mut request => result?,
        _ = crate::signals::cancellation() => {
            cancel.cancel();
            request.await?
        }
    };
    if let Some(response) = &outcome.response {
        let text = response.text();
        if !text.is_empty() {
            let mut stdout = io::stdout().lock();
            let terminal = stdout.is_terminal();
            if let Err(error) = write_answer(&mut stdout, &text, terminal) {
                if error.kind() == io::ErrorKind::BrokenPipe {
                    return Ok(0);
                }
                return Err("Cannot write the final response".into());
            }
        }
    }
    if outcome.status == RunStatus::Failed {
        let error = outcome
            .response
            .as_ref()
            .and_then(|response| response.error.as_deref())
            .unwrap_or("Provider request failed");
        eprintln!("mh: {}", crate::text::terminal_text(error, false));
    }
    Ok(match outcome.status {
        RunStatus::Completed => 0,
        RunStatus::Failed => 1,
        RunStatus::Truncated => 2,
        RunStatus::Cancelled => 130,
    })
}

fn write_answer(writer: &mut impl Write, text: &str, terminal: bool) -> io::Result<()> {
    // Only terminal presentation is sanitized. Pipes/files retain the model's
    // original answer, including escape/control bytes, as ordinary data.
    let safe;
    let text = if terminal {
        safe = crate::text::terminal_text(text, true);
        safe.as_str()
    } else {
        text
    };
    writer.write_all(text.as_bytes())?;
    writer.write_all(b"\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::write_answer;

    #[test]
    fn terminal_answers_suppress_osc_clipboard_and_controls_without_touching_clipboard() {
        let answer = "before\x1b]52;c;ZmFrZQ==\x07after\nUnicode 日本\x1b]0;fake-title\x1b\\\x1b[2J\u{9d}52;c;ZmFrZQ==\u{9c}";
        let mut terminal = Vec::new();
        write_answer(&mut terminal, answer, true).unwrap();
        assert_eq!(terminal, "beforeafter\nUnicode 日本\n".as_bytes());
        assert!(!terminal.contains(&0x1b));
        // The fixture writes only to a byte sink; no OSC reaches a terminal and
        // no clipboard API/helper is invoked.
        let mut redirected = Vec::new();
        write_answer(&mut redirected, answer, false).unwrap();
        assert_eq!(redirected, format!("{answer}\n").as_bytes());
    }
}
