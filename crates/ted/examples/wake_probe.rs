//! Probes the reader-thread bridge that SPEC §7 names as `ted`'s main
//! cross-platform risk, in isolation from the editor and the frame loop.
//!
//! The question this answers is M0 acceptance (c): does a message sent from a
//! plain `std::thread` wake the platform's foreground run loop promptly, with
//! no timer involved? The failure mode is not a hang — it is a TUI that only
//! redraws when some *other* timer happens to fire, which looks like sluggish
//! input rather than a broken bridge. So this probe deliberately schedules no
//! timer of its own: nothing but the channel can wake the loop.
//!
//! Run with `cargo run -p ted --example wake_probe`. Exits non-zero on failure.

use std::cell::RefCell;
use std::process::ExitCode;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use gpui::Application;
use ted::platform::TerminalPlatform;

const MESSAGES: usize = 5;
const SEND_INTERVAL: Duration = Duration::from_millis(100);
/// Generous enough that a loaded machine won't produce a false failure, but far
/// below any plausible fallback tick, so a loop that is only being woken by
/// something else cannot pass.
const MAX_WAKE_LATENCY: Duration = Duration::from_millis(50);
const WATCHDOG: Duration = Duration::from_secs(15);

fn main() -> ExitCode {
    // A blocked run loop cannot report its own failure, so the deadline is
    // enforced from outside it.
    thread::spawn(|| {
        thread::sleep(WATCHDOG);
        eprintln!("wake_probe: no result within {WATCHDOG:?}; the run loop never woke");
        std::process::exit(2);
    });

    let platform = Rc::new(TerminalPlatform::new(80, 24));
    let latencies: Rc<RefCell<Vec<Duration>>> = Rc::new(RefCell::new(Vec::new()));

    Application::with_platform(platform).run({
        let latencies = latencies.clone();
        move |cx| {
            let (sender, mut receiver) = futures::channel::mpsc::unbounded::<Instant>();

            cx.spawn({
                async move |cx| {
                    while let Some(sent_at) = receiver.next().await {
                        latencies.borrow_mut().push(sent_at.elapsed());
                        if latencies.borrow().len() >= MESSAGES {
                            break;
                        }
                    }
                    cx.update(|cx| cx.quit());
                }
            })
            .detach();

            // A plain OS thread, exactly like the crossterm reader in §7.
            thread::spawn(move || {
                for _ in 0..MESSAGES {
                    thread::sleep(SEND_INTERVAL);
                    if sender.unbounded_send(Instant::now()).is_err() {
                        return;
                    }
                }
            });
        }
    });

    let latencies = latencies.borrow();
    if latencies.len() < MESSAGES {
        eprintln!(
            "wake_probe: FAILED — only {} of {MESSAGES} messages woke the run loop",
            latencies.len()
        );
        return ExitCode::FAILURE;
    }

    let worst = latencies.iter().max().copied().unwrap_or_default();
    for (index, latency) in latencies.iter().enumerate() {
        println!("wake_probe: message {index} woke the loop after {latency:?}");
    }

    if worst > MAX_WAKE_LATENCY {
        eprintln!("wake_probe: FAILED — worst wake latency {worst:?} exceeds {MAX_WAKE_LATENCY:?}");
        return ExitCode::FAILURE;
    }

    println!("wake_probe: PASSED — {MESSAGES} wakes, worst latency {worst:?}, no timer involved");
    ExitCode::SUCCESS
}
