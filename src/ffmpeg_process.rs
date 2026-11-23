//! Launch FFmpeg in the background without flashing a Command Prompt window.
//!
//! FFmpeg is a console-subsystem program. When NeuroSnip (a Windows GUI app)
//! starts it, Windows otherwise creates a visible console window. Apply the
//! flag to *every* FFmpeg invocation: capture, video preview, seeking,
//! metadata probing, frame extraction and edited-video export.

use std::{ffi::OsStr, process::Command};

pub(crate) fn hidden_ffmpeg<S: AsRef<OsStr>>(executable: S) -> Command {
    let mut command = Command::new(executable);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW prevents a console for console-subsystem children.
        // Unlike DETACHED_PROCESS, it keeps standard input/output/error pipes
        // usable, which recording stop and playback decoding require.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}
