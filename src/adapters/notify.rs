//! The completion channel on the host: `FileNotifyProvider`, the per-sandbox
//! directory an agent's hook writes its events into from inside the box, the
//! watcher that observes it, and `DesktopNotifier`, the one sink this build ships.
//!
//! The channel lives with what remembers the sandbox rather than with what a
//! restart makes meaningless: a completion that happened stays a fact, and the
//! events file is also how long ago the box last did something. The watcher's own
//! pid is the other way around, and lives beside the other host-side helpers'.

use std::ffi::OsStr;
use std::fs;
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::Command;

use inotify::{EventMask, Inotify, WatchMask};

use crate::adapters::helper::HostHelper;
use crate::domain::error::HortError;
use crate::domain::model::SandboxName;
use crate::domain::notify::{EVENTS_FILE, watch_and_notify};
use crate::ports::{Notifier, NotifyProvider, NotifySpec, NotifyWatcher};

const SANDBOXES_DIR: &str = "sandboxes";
const CHANNEL_DIR: &str = "notify";
const WATCHER: HostHelper = HostHelper::new("hort-watcher", "watcher.pid");
/// Room for everything the kernel has to say about a channel in one read. The
/// entries of a channel have one short name, so what this bounds is how many
/// completions arrive together before the next read collects the rest.
const EVENT_BUFFER: usize = 4096;

/// Makes a sandbox's channel directory on disk, under the state hort keeps for
/// that sandbox, and runs the host-side watcher of that directory. hort owns the
/// directory, so one that is not there is made rather than reported.
pub struct FileNotifyProvider {
    state_root: PathBuf,
    /// Where the watcher's pid file goes, beside the pid files of the other
    /// host-side helpers of the same sandbox. A pid means nothing after a restart,
    /// and a root that a restart empties is the only place it cannot be read back
    /// as a stranger.
    runtime_root: PathBuf,
}

impl FileNotifyProvider {
    /// Build a provider keeping every channel under `state_root`, beside the
    /// record and the overlay of the sandbox it belongs to, and every watcher's
    /// pid file under `runtime_root`.
    pub fn new(state_root: PathBuf, runtime_root: PathBuf) -> Self {
        Self { state_root, runtime_root }
    }

    /// Where the watcher of `name` is recorded, beside the other host-side
    /// helpers of the same sandbox.
    fn sandbox_dir(&self, name: &SandboxName) -> PathBuf {
        self.runtime_root.join(SANDBOXES_DIR).join(name.as_str())
    }
}

impl NotifyProvider for FileNotifyProvider {
    fn ensure(&self, name: &SandboxName) -> Result<PathBuf, HortError> {
        let channel = self.state_root.join(SANDBOXES_DIR).join(name.as_str()).join(CHANNEL_DIR);
        fs::create_dir_all(&channel).map_err(|error| HortError::StateIo {
            detail: format!("could not create {}: {error}", channel.display()),
        })?;
        Ok(channel)
    }

    fn provision(&self, spec: &NotifySpec) -> Result<(), HortError> {
        // Building a name that already has a watcher running is a resume, and the
        // one it left behind holds this same channel. Started over it, this one
        // takes its place in the only file that records it, and the first would
        // raise a duplicate of every completion for as long as the machine is up
        // with nothing left on it able to stop the process.
        self.teardown(&spec.name)?;

        let sandbox_dir = self.sandbox_dir(&spec.name);
        fs::create_dir_all(&sandbox_dir).map_err(|error| {
            notify_failure(format!("could not create {}: {error}", sandbox_dir.display()))
        })?;

        // Watched from here rather than from inside the helper: the fork is
        // announced before it serves anything, so a channel the kernel refuses to
        // watch would be reported by a process nobody is listening to any more,
        // and `up` would carry on as if a watcher were running. The descriptor is
        // what the helper is left with when it drops everything else this process
        // had open.
        let watcher = InotifyChannelWatcher::new(&spec.events_dir)?;
        let kept = [watcher.as_raw_fd()];
        let notifier = DesktopNotifier::new(spec.sink.clone());
        let message = spec.message.clone();

        WATCHER
            .start(&sandbox_dir, &kept, move || raise_completions(watcher, &notifier, &message))
            .map_err(notify_failure)
    }

    fn teardown(&self, name: &SandboxName) -> Result<(), HortError> {
        WATCHER.stop(&self.sandbox_dir(name)).map_err(notify_failure)
    }
}

/// What the watcher process does for as long as its sandbox has a channel: raise
/// the message it was given for every completion appended to it.
fn raise_completions(mut watcher: InotifyChannelWatcher, notifier: &dyn Notifier, message: &str) {
    let _ = watch_and_notify(&mut watcher, notifier, message);
}

fn notify_failure(detail: impl Into<String>) -> HortError {
    HortError::NotifyProviderFailed { detail: detail.into() }
}

/// Raises a completion on the user's desktop, through the program it was handed.
///
/// The program is the one the host probe resolved, carried here rather than looked
/// up again by name: a notification that ran whatever the search path answered
/// with at the time is a different program from the one the build was allowed on.
pub struct DesktopNotifier {
    program: PathBuf,
}

impl DesktopNotifier {
    pub fn new(program: PathBuf) -> Self {
        Self { program }
    }
}

impl Notifier for DesktopNotifier {
    fn notify(&self, message: &str) -> Result<(), HortError> {
        // One argument and never a line for a shell to read: the message comes
        // from a configuration file, and a shell would take the punctuation in it
        // for instructions of its own.
        let raised = Command::new(&self.program).arg(message).status().map_err(|error| {
            notify_failure(format!("could not run {}: {error}", self.program.display()))
        })?;
        if !raised.success() {
            return Err(notify_failure(format!(
                "{} raised nothing: {raised}",
                self.program.display()
            )));
        }
        Ok(())
    }
}

/// Watches one sandbox's channel directory through the kernel, reporting each
/// completion appended to it.
///
/// The directory and not the file: the events file does not exist when a sandbox
/// is built, because the first completion is what creates it. Entries are matched
/// by name, or anything at all left in the channel would raise a notification.
pub struct InotifyChannelWatcher {
    inotify: Inotify,
    buffer: [u8; EVENT_BUFFER],
    /// Completions the kernel has already reported and this has not answered
    /// with yet. One read hands over everything that happened since the last one,
    /// so a batch holding two completions is two notifications: nothing here
    /// waits out a window to see whether more arrive.
    pending: usize,
    gone: bool,
}

impl InotifyChannelWatcher {
    /// Start watching `channel`, the host side of one sandbox's completion
    /// channel.
    pub fn new(channel: &Path) -> Result<Self, HortError> {
        let inotify = Inotify::init().map_err(|error| {
            notify_failure(format!("could not ask the kernel to watch: {error}"))
        })?;
        inotify.watches().add(channel, WatchMask::MODIFY | WatchMask::DELETE_SELF).map_err(
            |error| notify_failure(format!("could not watch {}: {error}", channel.display())),
        )?;
        Ok(Self { inotify, buffer: [0; EVENT_BUFFER], pending: 0, gone: false })
    }

    /// Everything the kernel has to say about the channel since it was last
    /// asked: how many completions were appended, and whether the channel itself
    /// is gone.
    fn read_channel(&mut self) -> Result<(usize, bool), HortError> {
        let events = self.inotify.read_events_blocking(&mut self.buffer).map_err(|error| {
            notify_failure(format!("could not read what happened to the channel: {error}"))
        })?;
        let mut appends = 0;
        let mut gone = false;
        for event in events {
            // The watch is dropped by the kernel when the directory goes, which is
            // what `down` does to it while this is still holding it.
            if event.mask.intersects(EventMask::DELETE_SELF | EventMask::IGNORED) {
                gone = true;
            } else if event.name == Some(OsStr::new(EVENTS_FILE)) {
                appends += 1;
            }
        }
        Ok((appends, gone))
    }
}

impl AsRawFd for InotifyChannelWatcher {
    fn as_raw_fd(&self) -> RawFd {
        self.inotify.as_raw_fd()
    }
}

impl NotifyWatcher for InotifyChannelWatcher {
    fn wait_for_append(&mut self) -> Result<bool, HortError> {
        loop {
            if self.pending > 0 {
                self.pending -= 1;
                return Ok(true);
            }
            if self.gone {
                return Ok(false);
            }
            (self.pending, self.gone) = self.read_channel()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::os::unix::fs::PermissionsExt;
    use std::thread::sleep;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use crate::domain::notify::EVENTS_FILE;

    /// A stand-in for a desktop notification program: a script that records every
    /// argument it was run with.
    ///
    /// Named nothing like the real program on purpose. A sink that went looking on
    /// the search path instead of running what it was handed would find the host's
    /// own `notify-send`, raise a real notification, and leave this file empty.
    fn recording_program(dir: &Path, recording: &Path) -> PathBuf {
        let program = dir.join("raise-it");
        fs::write(&program, format!("#!/bin/sh\necho \"$@\" >> {}\n", recording.display()))
            .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        program
    }

    /// The world one sandbox's watcher is given: the channel on the host, the
    /// message, and a program that exists and raises nothing, since these tests
    /// never append a completion.
    fn watching(name: &SandboxName, channel: &Path) -> NotifySpec {
        NotifySpec {
            name: name.clone(),
            events_dir: channel.to_path_buf(),
            message: "hort sandbox 'demo' finished".to_string(),
            sink: PathBuf::from("/bin/true"),
        }
    }

    fn recorded_watcher(runtime_root: &Path, name: &SandboxName) -> u32 {
        let pid_file = runtime_root.join(SANDBOXES_DIR).join(name.as_str()).join("watcher.pid");
        fs::read_to_string(&pid_file)
            .expect("a recorded watcher")
            .trim()
            .parse()
            .expect("the recorded watcher is a pid")
    }

    /// Whether the process at `pid` has stopped being a watcher, asked the way the
    /// teardown itself asks it: a pid outlives the process it named, so what
    /// settles it is whether it still calls itself the helper.
    fn stopped_being_the_watcher_within_deadline(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let named = fs::read_to_string(format!("/proc/{pid}/comm"))
                .is_ok_and(|name| name.trim() == "hort-watcher");
            if !named {
                return true;
            }
            sleep(Duration::from_millis(50));
        }
        false
    }

    #[test]
    fn the_watcher_reports_a_completion_appended_to_the_channel() {
        let channel = TempDir::new().unwrap();
        let mut watcher = InotifyChannelWatcher::new(channel.path()).unwrap();

        // Written after the watch is in place, and by a writer that creates the
        // file: nothing has appended a completion yet when a sandbox is built, so
        // a watcher waiting on the file itself waits on something that is not
        // there and never hears the first completion of the box.
        fs::write(channel.path().join(EVENTS_FILE), "{\"event\":\"stop\"}\n").unwrap();

        assert_eq!(watcher.wait_for_append(), Ok(true));
    }

    #[test]
    fn a_file_of_another_name_in_the_channel_is_no_completion() {
        let channel = TempDir::new().unwrap();
        let mut watcher = InotifyChannelWatcher::new(channel.path()).unwrap();

        fs::write(channel.path().join("notes.txt"), "not a completion\n").unwrap();
        fs::remove_dir_all(channel.path()).unwrap();

        // Two things happened to this channel and only the second is news: the
        // channel is gone, which is what `down` does to it. Reported as an append,
        // the stray file would raise a notification for a completion nobody
        // announced, and the box is writable from inside by an agent under no
        // restrictions.
        assert_eq!(watcher.wait_for_append(), Ok(false));
    }

    #[test]
    fn the_desktop_sink_raises_the_message_through_the_program_it_was_given() {
        let sink = TempDir::new().unwrap();
        let raised = sink.path().join("raised");
        let notifier = DesktopNotifier::new(recording_program(sink.path(), &raised));

        notifier.notify("hort sandbox 'demo' finished").unwrap();

        // The message arrives rendered and the sink's whole job is to put it in
        // front of the user. It has to be able to say it could not, which is why
        // the answer is already there when this returns.
        assert!(fs::read_to_string(&raised).unwrap().contains("hort sandbox 'demo' finished"));
    }

    #[test]
    fn provisioning_a_resumed_sandbox_stops_the_watcher_it_left_behind() {
        let state = TempDir::new().unwrap();
        let runtime = TempDir::new().unwrap();
        let provider =
            FileNotifyProvider::new(state.path().to_path_buf(), runtime.path().to_path_buf());
        let name = SandboxName::new("demo").unwrap();
        let channel = provider.ensure(&name).unwrap();
        provider.provision(&watching(&name, &channel)).unwrap();
        let left_behind = recorded_watcher(runtime.path(), &name);

        provider.provision(&watching(&name, &channel)).unwrap();

        // The watcher has no reason to leave when the container it was started
        // beside dies, and a sandbox whose anchor was killed is built again under
        // the same name as a matter of course. A second one started over it takes
        // its place in the only file that records it, so the first raises a
        // duplicate of every completion for as long as the machine is up and
        // nothing on it can ever stop it.
        assert!(stopped_being_the_watcher_within_deadline(left_behind));

        provider.teardown(&name).unwrap();
    }

    #[test]
    fn the_channel_is_made_under_the_state_of_the_sandbox_it_belongs_to() {
        let state = TempDir::new().unwrap();
        let runtime = TempDir::new().unwrap();
        let provider =
            FileNotifyProvider::new(state.path().to_path_buf(), runtime.path().to_path_buf());

        let channel = provider.ensure(&SandboxName::new("demo").unwrap()).unwrap();

        // Both halves of one guarantee, and neither is worth anything alone: the
        // path it answers with is what the container binds, so a path it reports
        // without making takes the whole box down, and a directory it makes
        // somewhere else is a channel the watcher will never find. Nothing above
        // this derives the address a second time, which is why it is the one
        // asserted here.
        assert_eq!(channel, state.path().join("sandboxes").join("demo").join("notify"));
        assert!(channel.is_dir());
    }

    #[test]
    fn a_channel_that_is_already_there_is_not_an_error() {
        let state = TempDir::new().unwrap();
        let runtime = TempDir::new().unwrap();
        let provider =
            FileNotifyProvider::new(state.path().to_path_buf(), runtime.path().to_path_buf());
        let name = SandboxName::new("demo").unwrap();
        provider.ensure(&name).unwrap();

        // `up` is reentrant against a half-built sandbox, so the second run of a
        // build that broke reaches this again, and a box that already has its
        // channel is a box in the state this was asked for.
        assert!(provider.ensure(&name).is_ok());
    }
}
