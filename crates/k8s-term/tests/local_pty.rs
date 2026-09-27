#![cfg(unix)]
//! Tests the local PTY without a GUI.

use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::term::{ClipboardType, Term};
use k8s_term::{SessionEvent, TermSize, TerminalSession};

fn grid_text(session: &TerminalSession) -> String {
    let term = session.term().lock();
    let content = term.renderable_content();
    let mut out = String::new();
    let mut current_line = i32::MIN;
    for indexed in content.display_iter {
        let line = indexed.point.line.0;
        if line != current_line {
            if current_line != i32::MIN {
                out.push('\n');
            }
            current_line = line;
        }
        out.push(indexed.cell.c);
    }
    out
}

async fn wait_for(
    session: &TerminalSession,
    events: &mut tokio::sync::mpsc::Receiver<SessionEvent>,
    needle: &str,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = grid_text(session);
        if text.contains(needle) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {needle:?}: {text:?}"
        );
        if tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .is_ok()
        {
            session.ack_wakeup();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn local_pty_runs_shell_command() {
    let (session, mut events) = TerminalSession::local(
        Some("/bin/sh".to_owned()),
        Some("/tmp".into()),
        TermSize::new(80, 24),
        8,
        18,
        1000,
    )
    .expect("failed to start local pty");

    session.write(b"echo hello-spike\r".to_vec());
    let text = wait_for(&session, &mut events, "hello-spike").await;
    assert!(
        text.contains("hello-spike"),
        "echo output missing: {text:?}"
    );
}

/// Waits for a trimmed line equal to `needle` and returns its row.
async fn wait_for_row(
    session: &TerminalSession,
    events: &mut tokio::sync::mpsc::Receiver<SessionEvent>,
    needle: &str,
) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = grid_text(session);
        if let Some(row) = text.lines().position(|line| line.trim_end() == needle) {
            return row as i32;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for row {needle:?}"
        );
        if tokio::time::timeout(Duration::from_millis(200), events.recv())
            .await
            .is_ok()
        {
            session.ack_wakeup();
        }
    }
}

async fn next_event(events: &mut tokio::sync::mpsc::Receiver<SessionEvent>) -> SessionEvent {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(200), events.recv()).await
        {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for session event"
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn osc52_store_decodes_base64_to_clipboard_event() {
    let (session, mut events) = TerminalSession::local(
        Some("/bin/sh".to_owned()),
        Some("/tmp".into()),
        TermSize::new(80, 24),
        8,
        18,
        1000,
    )
    .expect("failed to start local pty");
    session.write(b"printf '\\033]52;c;aGVsbG8=\\007'\r".to_vec());
    loop {
        if let SessionEvent::ClipboardStore(kind, text) = next_event(&mut events).await {
            assert_eq!(kind, ClipboardType::Clipboard);
            assert_eq!(text, "hello");
            return;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn osc52_load_is_disabled_by_default() {
    let (session, mut events) = TerminalSession::local(
        Some("/bin/sh".to_owned()),
        Some("/tmp".into()),
        TermSize::new(80, 24),
        8,
        18,
        1000,
    )
    .expect("failed to start local pty");
    session.write(b"printf '\\033]52;c;?\\007'\r".to_vec());

    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        if let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(50), events.recv()).await
        {
            assert!(
                !matches!(event, SessionEvent::ClipboardStore(..)),
                "OSC 52 clipboard reads must stay disabled; fix the terminal configuration"
            );
            session.ack_wakeup();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cursor_blinking_change_is_reported() {
    let (session, mut events) = TerminalSession::local(
        Some("/bin/sh".to_owned()),
        Some("/tmp".into()),
        TermSize::new(80, 24),
        8,
        18,
        1000,
    )
    .expect("failed to start local pty");
    session.write(b"printf '\\033[5 q'\r".to_vec());
    loop {
        if matches!(
            next_event(&mut events).await,
            SessionEvent::CursorBlinkingChanged
        ) {
            assert!(session.cursor_blinking());
            break;
        }
    }
    session.write(b"printf '\\033[2 q'\r".to_vec());
    loop {
        if matches!(
            next_event(&mut events).await,
            SessionEvent::CursorBlinkingChanged
        ) {
            assert!(!session.cursor_blinking());
            return;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn block_selection_copies_rectangle() {
    let (session, mut events) = TerminalSession::local(
        Some("/bin/sh".to_owned()),
        Some("/tmp".into()),
        TermSize::new(20, 6),
        8,
        18,
        1000,
    )
    .expect("failed to start local pty");
    session.write(b"printf 'abcdef\\nghijkl\\nmnopqr\\n'\r".to_vec());
    let row = wait_for_row(&session, &mut events, "abcdef").await;

    session.start_block_selection(Point::new(Line(row), Column(1)), Side::Left);
    session.update_selection(Point::new(Line(row + 2), Column(3)), Side::Right);
    assert_eq!(
        session.selection_to_string().as_deref(),
        Some("bcd\nhij\nnop")
    );
}

/// Tests block selection across a soft wrap without a PTY.
#[test]
fn block_selection_handles_soft_wrapped_lines() {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::selection::{Selection, SelectionType};
    use alacritty_terminal::term::Config;

    struct TestSize;
    impl Dimensions for TestSize {
        fn total_lines(&self) -> usize {
            6
        }
        fn screen_lines(&self) -> usize {
            6
        }
        fn columns(&self) -> usize {
            4
        }
    }

    let mut term = Term::new(Config::default(), &TestSize, VoidListener);
    let mut parser = alacritty_terminal::vte::ansi::Processor::<
        alacritty_terminal::vte::ansi::StdSyncHandler,
    >::default();
    parser.advance(&mut term, b"abcdefgh");

    let mut selection = Selection::new(
        SelectionType::Block,
        Point::new(Line(0), Column(0)),
        Side::Left,
    );
    selection.update(Point::new(Line(1), Column(1)), Side::Right);
    term.selection = Some(selection);
    assert_eq!(term.selection_to_string().as_deref(), Some("ab\nef"));
}

#[tokio::test(flavor = "current_thread")]
async fn resize_updates_grid_size() {
    let (session, mut events) = TerminalSession::local(
        Some("/bin/sh".to_owned()),
        Some("/tmp".into()),
        TermSize::new(80, 24),
        8,
        18,
        1000,
    )
    .expect("failed to start local pty");

    session.resize(TermSize::new(120, 40));
    assert_eq!(session.size(), TermSize::new(120, 40));

    session.write(b"echo resized\r".to_vec());
    let text = wait_for(&session, &mut events, "resized").await;
    assert!(
        text.contains("resized"),
        "output after resize missing: {text:?}"
    );
}
