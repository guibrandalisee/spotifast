//! One ordered writer for UI settings and session snapshots. Pending saves are
//! coalesced per file; shutdown drains the latest snapshots before returning.
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use crate::paths::AppDirs;
use crate::settings::{SessionState, Settings};

#[derive(Default)]
struct Pending {
    settings: Option<Settings>,
    session: Option<SessionState>,
}

impl Pending {
    fn is_empty(&self) -> bool {
        self.settings.is_none() && self.session.is_none()
    }
}

#[derive(Default)]
struct State {
    pending: Pending,
    writing: bool,
    stopped: bool,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

pub(crate) struct Writer {
    dirs: AppDirs,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl Writer {
    pub fn new(dirs: AppDirs) -> Self {
        Self {
            dirs,
            shared: Arc::new(Shared::default()),
            thread: None,
        }
    }

    fn start(&mut self) {
        if self.thread.is_none() {
            let dirs = self.dirs.clone();
            self.start_with(move |pending| {
                if let Some(settings) = pending.settings {
                    settings.save(&dirs.settings_file());
                }
                if let Some(session) = pending.session {
                    session.save(&dirs.session_file());
                }
            });
        }
    }

    fn start_with(&mut self, write: impl FnMut(Pending) + Send + 'static) {
        let shared = self.shared.clone();
        self.thread = Some(
            std::thread::Builder::new()
                .name("state-writer".into())
                .spawn(move || run(shared, write))
                .expect("Unable to start state writer"),
        );
    }

    pub fn settings(&mut self, settings: Settings) {
        self.start();
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending.settings = Some(settings);
        self.shared.changed.notify_one();
    }

    pub fn session(&mut self, session: SessionState) {
        self.start();
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.pending.session = Some(session);
        self.shared.changed.notify_one();
    }

    /// Only used at final shutdown and in tests that read the saved files.
    pub fn flush(&self) {
        let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
        while state.writing || !state.pending.is_empty() {
            state = self
                .shared
                .changed
                .wait(state)
                .unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().unwrap_or_else(|e| e.into_inner());
            state.stopped = true;
            self.shared.changed.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(shared: Arc<Shared>, mut write: impl FnMut(Pending)) {
    loop {
        let pending = {
            let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.pending.is_empty() && !state.stopped {
                state = shared
                    .changed
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
            if state.pending.is_empty() && state.stopped {
                break;
            }
            state.writing = true;
            std::mem::take(&mut state.pending)
        };
        write(pending);
        let mut state = shared.state.lock().unwrap_or_else(|e| e.into_inner());
        state.writing = false;
        shared.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn dirs() -> AppDirs {
        let root = std::env::temp_dir().join(format!(
            "spotifast-writer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        AppDirs {
            config: root.join("config"),
            state: root.join("state"),
            cache: root.join("cache"),
        }
    }

    #[test]
    fn slow_write_does_not_block_submission_and_latest_snapshots_win() {
        let (started, waiting) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let saved = Arc::new(Mutex::new(Vec::new()));
        let results = saved.clone();
        let mut first = true;
        let mut writer = Writer::new(dirs());
        writer.start_with(move |pending| {
            if first {
                first = false;
                started.send(()).unwrap();
                blocked.recv_timeout(Duration::from_secs(5)).unwrap();
            }
            results.lock().unwrap().push(pending);
        });
        writer.session(SessionState {
            last_position_ms: 1,
            ..Default::default()
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        for position in 2..=100 {
            writer.session(SessionState {
                last_position_ms: position,
                ..Default::default()
            });
            writer.settings(Settings {
                zoom: position as f32,
                ..Default::default()
            });
        }
        release.send(()).unwrap();
        writer.flush();
        let saved = saved.lock().unwrap();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[1].session.as_ref().unwrap().last_position_ms, 100);
        assert_eq!(saved[1].settings.as_ref().unwrap().zoom, 100.0);
    }

    #[test]
    fn dropping_writer_drains_both_files_and_keeps_their_formats() {
        let dirs = dirs();
        let settings = Settings {
            zoom: 1.5,
            ..Default::default()
        };
        let session = SessionState {
            last_position_ms: 12345,
            ..Default::default()
        };
        {
            let mut writer = Writer::new(dirs.clone());
            writer.settings(settings.clone());
            writer.session(session.clone());
        }
        assert!(Settings::load(&dirs.settings_file()) == settings);
        assert_eq!(SessionState::load(&dirs.session_file()), session);
        assert!(!dirs.settings_file().with_extension("json.tmp").exists());
        assert!(!dirs.session_file().with_extension("json.tmp").exists());
        std::fs::remove_dir_all(dirs.config.parent().unwrap()).unwrap();
    }
}
