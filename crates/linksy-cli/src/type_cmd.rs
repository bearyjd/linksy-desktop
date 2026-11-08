//! `linksy type`: type (or paste) text on a phone through a typing session.

use std::time::Duration;

use linksy_ipc::linksy_core::ControlMsg;
use linksy_ipc::{Client, ClientError};

/// How long each part of a run may take.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// All the writing together.
    pub write: Duration,
    /// Watching for the phone ending the session, once everything is written.
    pub watch: Duration,
    /// Waiting for the service to say it has passed everything on.
    pub drain: Duration,
}

pub const TIMING: Timing = Timing { write: Duration::from_secs(30), watch: linksy_ipc::typing::INSTANT_END, drain: Duration::from_secs(5) };

pub async fn run(c: &Client, to: &str, text: &[String]) -> Result<(), String> {
    use linksy_ipc::typing::{Problem, TargetError, check_target, paste_messages, read_bounded, start_problem};
    let err = |e: ClientError| e.to_string();
    let text = if text.is_empty() { read_bounded(std::io::stdin())? } else { text.join(" ") };
    let (messages, cut) = paste_messages(&text);
    if messages.is_empty() {
        return Err("Nothing to type.".into());
    }
    let not_enabled = format!("{to} didn't take it. On {to}, open Linksy and turn on Linksy Keyboard, then try again and tap Allow. Older versions of the Linksy app can't take typing.");
    // Exactly one device, found the way the service finds it, and only a phone that
    // says it takes typing is asked
    let known = c.devices().await.map_err(err)?;
    let id = check_target(&known, to).map_err(|e| match e {
        TargetError::Resolve(e) => e,
        TargetError::NotTyping => not_enabled.clone(),
    })?;
    println!("Asking {to}: tap Allow on it…");
    let info = match c.start_session(id.to_string(), linksy_ipc::linksy_core::SessionKind::Input).await {
        Ok(info) => info,
        Err(e) => {
            return Err(match start_problem(&e.to_string()) {
                Problem::Unsupported => format!("{to} can't take typing yet. Update Linksy on it."),
                Problem::NotAllowed | Problem::NoAnswer | Problem::EndedAtOnce => not_enabled,
                Problem::Other(e) => e,
            });
        }
    };
    // The session is ended on every way out from here
    let result = match info.socket.as_deref() {
        Some(socket) => type_into(to, socket, &messages, &not_enabled, TIMING).await,
        None => Err("no stream to type into".to_string()),
    };
    let _ = c.end_session(info.session).await;
    result?;
    println!("Typed on {to}.{}", if cut { " That was a lot: only the start was sent." } else { "" });
    Ok(())
}

/// Writes `messages` into a typing session's stream, then watches as long as the window
/// does for the phone ending the session: one that does so at once never took the text.
async fn type_into(to: &str, socket: &std::path::Path, messages: &[ControlMsg], not_enabled: &str, timing: Timing) -> Result<(), String> {
    use linksy_ipc::typing::{INSTANT_END, session_outcome};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let _ = timing;
    let mut stream = tokio::net::UnixStream::connect(socket).await.map_err(|e| e.to_string())?;
    let started = std::time::Instant::now();
    for m in messages {
        if let Err(e) = stream.write_all(&linksy_ipc::linksy_core::protocol::control_frame(m)).await {
            return Err(match session_outcome(Some(started.elapsed())) {
                Err(_) => not_enabled.to_string(),
                Ok(()) => format!("{to} stopped taking typing: {e}"),
            });
        }
    }
    let mut buf = [0u8; 64];
    let ended = loop {
        let Some(left) = INSTANT_END.checked_sub(started.elapsed()) else { break None };
        match tokio::time::timeout(left, stream.read(&mut buf)).await {
            Err(_) => break None,
            Ok(Ok(n)) if n > 0 => {}
            Ok(_) => break Some(started.elapsed()),
        }
    };
    session_outcome(ended).map_err(|_| not_enabled.to_string())?;
    // Closing our end tells the service everything is sent; it closes its own once it has
    // passed it on (or we give up waiting)
    stream.shutdown().await.map_err(|e| e.to_string())?;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(1), async { while matches!(stream.read(&mut buf).await, Ok(n) if n > 0) {} }).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::AsyncReadExt;

    const SHORT: Timing = Timing { write: Duration::from_millis(300), watch: Duration::from_millis(300), drain: Duration::from_millis(300) };

    /// A socket the "service" listens on; `then` is what it does with the first connection and
    /// the bytes it received so far. Returns the run's result and what the service got.
    async fn typing_run<F, Fut>(name: &str, msgs: Vec<ControlMsg>, timing: Timing, then: F) -> (Result<(), String>, Vec<u8>)
    where
        F: FnOnce(tokio::net::UnixStream, Arc<Mutex<Vec<u8>>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let path = std::env::temp_dir().join(format!("linksy-cli-test-{}-{name}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let got = Arc::new(Mutex::new(Vec::new()));
        let sink = got.clone();
        let service = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            then(stream, sink).await;
        });
        let result = tokio::time::timeout(Duration::from_secs(8), type_into("Pixel", &path, &msgs, "not enabled", timing)).await.expect("the run hung");
        service.abort();
        let _ = std::fs::remove_file(&path);
        let bytes = got.lock().unwrap().clone();
        (result, bytes)
    }

    /// Reads everything into `sink` until the client closes its end.
    async fn read_all(stream: &mut tokio::net::UnixStream, sink: &Arc<Mutex<Vec<u8>>>) {
        let mut buf = [0u8; 4096];
        while let Ok(n) = stream.read(&mut buf).await {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&buf[..n]);
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn hi() -> Vec<ControlMsg> {
        linksy_ipc::typing::text_messages("hi")
    }

    /// Enough to fill the socket buffers, so a peer that doesn't read is noticed.
    fn big() -> Vec<ControlMsg> {
        linksy_ipc::typing::text_messages(&"x".repeat(2_000_000))
    }

    #[tokio::test]
    async fn the_text_arrives_and_the_run_succeeds() {
        let (result, got) = typing_run("arrives", hi(), SHORT, |mut s, sink| async move { read_all(&mut s, &sink).await }).await;
        assert_eq!(result, Ok(()));
        assert!(contains(&got, b"hi"), "the phone received {:?}", String::from_utf8_lossy(&got));
    }

    #[tokio::test]
    async fn a_phone_that_closes_before_the_text_is_written_is_reported_not_typed_on() {
        let (result, _) = typing_run("closes", big(), SHORT, |s, _| async move { drop(s) }).await;
        assert_eq!(result, Err("not enabled".to_string()));
    }

    #[tokio::test]
    async fn a_phone_that_takes_the_text_and_closes_is_not_a_failure() {
        let (result, got) = typing_run("took", hi(), Timing { watch: Duration::from_secs(2), ..SHORT }, |mut s, sink| async move {
            let mut buf = [0u8; 256];
            while !contains(&sink.lock().unwrap(), b"hi") {
                match s.read(&mut buf).await {
                    Ok(n) if n > 0 => sink.lock().unwrap().extend_from_slice(&buf[..n]),
                    _ => break,
                }
            }
            drop(s);
        })
        .await;
        assert_eq!(result, Ok(()), "closing after the text was written would make people retype it");
        assert!(contains(&got, b"hi"));
    }

    #[tokio::test]
    async fn a_phone_that_stops_reading_times_the_write_out() {
        let (result, _) = typing_run("stalls", big(), SHORT, |s, _| async move {
            let _held = s;
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
        .await;
        let e = result.unwrap_err();
        assert!(e.contains("timed out") || e.contains("Timed out"), "{e}");
    }

    #[tokio::test]
    async fn a_service_that_does_not_finish_passing_it_on_is_an_error() {
        let (result, _) = typing_run("drain", hi(), SHORT, |mut s, sink| async move {
            read_all(&mut s, &sink).await;
            tokio::time::sleep(Duration::from_secs(30)).await; // never closes its end
        })
        .await;
        assert!(result.is_err(), "success would be printed for text that may still be waiting");
    }
}
