//! `DesktopNotifier` (`Notifier`) via `notify-send` (the MVP's only sink), plus
//! the host-side `Watcher` (`NotifyWatcher` read-port) via raw `inotify` watching
//! the notify dir. The notifier receives an already-rendered message.
//!
//! See backlog A-06, A-07.

// TODO(A-06/A-07): the desktop notifier and the inotify watcher.
